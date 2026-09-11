//! Encrypted Stratum V2 Extended Channel client.
//!
//! The client performs the full SV2 connection sequence:
//! DNS resolve → TCP connect → Noise NX handshake → SetupConnection →
//! OpenExtendedMiningChannel → main select! event loop.
//!
//! Read/write halves are decoupled via a spawned reader task because
//! `NoiseTcpReadHalf::read_frame()` is not cancellation-safe.
//!
//! # SV2 Spec References
//!
//! - [Common Protocol > `SetupConnection`][sv2-setup]
//! - [Mining Protocol > `OpenExtendedMiningChannel`][sv2-open-channel]
//! - [Protocol Security > URL Scheme and Pool Authority Key][sv2-url]
//!
//! [sv2-setup]: https://github.com/stratum-mining/sv2-spec/blob/main/03-Protocol-Overview.md#361-setupconnection-client---server
//! [sv2-open-channel]: https://github.com/stratum-mining/sv2-spec/blob/main/05-Mining-Protocol.md#534-openextendedminingchannel-client---server
//! [sv2-url]: https://github.com/stratum-mining/sv2-spec/blob/main/04-Protocol-Security.md#47-url-scheme-and-pool-authority-key

use std::ops::ControlFlow;
use std::time::Duration;

use bitcoin::pow::Target;
use stratum_apps::key_utils::Secp256k1PublicKey;
use stratum_apps::network_helpers::Error as NetworkError;
use stratum_apps::network_helpers::connect_with_noise;
use stratum_apps::network_helpers::noise_stream::{NoiseTcpReadHalf, NoiseTcpWriteHalf};
use stratum_apps::network_helpers::resolve_host;
use stratum_apps::stratum_core::binary_sv2::Str0255;
use stratum_apps::stratum_core::codec_sv2::StandardEitherFrame;
use stratum_apps::stratum_core::common_messages_sv2::{Protocol, Reconnect, SetupConnection};
use stratum_apps::stratum_core::mining_sv2::{
    CloseChannel, NewExtendedMiningJob, OpenExtendedMiningChannel,
    OpenExtendedMiningChannelSuccess, SetNewPrevHash, SetTarget, SubmitSharesError,
    SubmitSharesExtended, SubmitSharesSuccess, UpdateChannel,
};
use stratum_apps::stratum_core::parsers_sv2::{AnyMessage, CommonMessages, Mining};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::tracing::prelude::*;
use crate::types::HashRate;

use super::error::{StratumV2Error, StratumV2Result};

/// Protocol constants for the Stratum V2 Extended Channel client.
pub mod constants {
    /// Minimum total extranonce size (in bytes) requested from the pool.
    ///
    /// The pool splits this into a fixed prefix and a miner-controlled
    /// set of bytes.  Hardware rolls the 32-bit nonce at full speed; when
    /// exhausted it wraps the rollable bytes for a fresh coinbase. A small
    /// rollable bytes space wraps fast, causing share collisions at high
    /// hashrates. If the pool assigns fewer bytes than this minimum, the
    /// channel is rejected.
    pub const MIN_EXTRANONCE_SIZE: usize = 8;

    /// Maximum total extranonce size (in bytes) accepted from the pool.
    ///
    /// `SubmitSharesExtended.extranonce` is typed `B032` (max 32 bytes), so
    /// any pool-assigned `extranonce_size > 32` would cause every share to
    /// fail `B032` encoding.  Reject the channel at open time instead.
    pub const MAX_EXTRANONCE_SIZE: usize = 32;

    /// `SetupConnection.flags` bit announcing that the miner rolls the
    /// version field, so the pool must only send jobs that allow it.
    pub const REQUIRES_VERSION_ROLLING: u32 = 1 << 2;

    /// `SetupConnection.Success.flags` bit by which the pool refuses any
    /// change to the version field.
    pub const REQUIRES_FIXED_VERSION: u32 = 1 << 0;
}

/// Stratum V2 pool connection configuration.
#[derive(Debug, Clone)]
pub struct PoolConfig {
    pub(crate) host: Str0255<'static>,
    pub(crate) port: u16,
    pub(crate) authority_pubkey: Secp256k1PublicKey,
    pub(crate) user_identity: Str0255<'static>,
    pub(crate) vendor: Str0255<'static>,
    pub(crate) hardware_version: Str0255<'static>,
    pub(crate) firmware: Str0255<'static>,
    pub(crate) device_id: Str0255<'static>,
    pub(crate) nominal_hash_rate: HashRate,
}

impl PoolConfig {
    /// Builds a pool configuration, encoding the text fields for the wire.
    ///
    /// # Errors
    ///
    /// Returns [`StratumV2Error::Protocol`] if any text field exceeds the
    /// 255-byte limit of the SV2 `STR0_255` type.
    #[expect(
        clippy::too_many_arguments,
        reason = "SV2 SetupConnection + OpenExtendedMiningChannel \
                  field set; grouping into a sub-struct would not reduce the \
                  caller's burden"
    )]
    pub fn new(
        host: String,
        port: u16,
        authority_pubkey: Secp256k1PublicKey,
        user_identity: String,
        vendor: String,
        hardware_version: String,
        firmware: String,
        device_id: String,
        nominal_hash_rate: HashRate,
    ) -> StratumV2Result<Self> {
        Ok(Self {
            host: Str0255::try_from(host).map_err(protocol_error)?,
            port,
            authority_pubkey,
            user_identity: Str0255::try_from(user_identity).map_err(protocol_error)?,
            vendor: Str0255::try_from(vendor).map_err(protocol_error)?,
            hardware_version: Str0255::try_from(hardware_version).map_err(protocol_error)?,
            firmware: Str0255::try_from(firmware).map_err(protocol_error)?,
            device_id: Str0255::try_from(device_id).map_err(protocol_error)?,
            nominal_hash_rate,
        })
    }

    /// Returns the pool host as text, for DNS resolution and logging.
    ///
    /// `Str0255`'s own formatting renders the raw wire bytes, so callers that
    /// need the hostname string go through here.
    pub(crate) fn host(&self) -> String {
        self.host.as_utf8_or_hex()
    }

    /// Returns a copy of this configuration that connects to `host:port`.
    ///
    /// Every other field is kept, the authority public key included, so a
    /// pool can move the miner between its own servers but never to a server
    /// outside its authority.
    ///
    /// # Errors
    ///
    /// Returns [`StratumV2Error::Protocol`] if `host` exceeds the 255-byte
    /// limit of the SV2 `STR0_255` type.
    pub(crate) fn with_endpoint(&self, host: String, port: u16) -> StratumV2Result<Self> {
        Ok(Self {
            host: Str0255::try_from(host).map_err(protocol_error)?,
            port,
            ..self.clone()
        })
    }
}

/// Commands sent to the SV2 client from the consumer.
#[derive(Debug, Clone)]
pub enum ClientCommand {
    /// Send a `SubmitSharesExtended` message to the pool.
    SubmitShare(SubmitSharesExtended<'static>),
    /// Send an `UpdateChannel` message to the pool.
    UpdateChannel(UpdateChannel<'static>),
}

/// Events emitted by the SV2 client to the consumer.
#[derive(Debug, Clone)]
pub enum ClientEvent {
    /// Pool accepted SetupConnection.
    SetupConnectionSuccess {
        /// Protocol version the pool negotiated.
        used_version: u16,
        /// Capability flags the pool returned.
        flags: u32,
    },
    /// Pool accepted OpenExtendedMiningChannel.
    OpenExtendedMiningChannelSuccess(OpenExtendedMiningChannelSuccess<'static>),
    /// New mining job from the pool.
    NewExtendedMiningJob(NewExtendedMiningJob<'static>),
    /// New previous block hash (invalidates un-activated future jobs).
    SetNewPrevHash(SetNewPrevHash<'static>),
    /// Pool updated the share difficulty target.
    SetTarget(SetTarget<'static>),
    /// Pool accepted a previously submitted share.
    SubmitSharesSuccess(SubmitSharesSuccess),
    /// Pool rejected a previously submitted share.
    SubmitSharesError(SubmitSharesError<'static>),
    /// Pool closed the Extended Channel.
    CloseChannel(CloseChannel<'static>),
}

/// Outcome returned by [`StratumV2Client::run`] on a clean exit.
///
/// Lets the caller distinguish a user-requested shutdown from a connection
/// close without relying on a side-channel flag or error-string inspection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientOutcome {
    /// Cancellation token was fired; miner requested a clean stop.
    Shutdown,
    /// TCP connection closed (pool side or network); caller should reconnect.
    ConnectionClosed,
    /// Pool sent `CloseChannel`; caller should reconnect to reopen it.
    ChannelClosed,
    /// Pool sent `Reconnect`; caller should reconnect to `host:port`.
    ///
    /// Both fields are always filled in: an empty `new_host` or a zero
    /// `new_port` in the message stands for the endpoint in use.
    Reconnect {
        /// Host the next connection should target.
        host: String,
        /// Port the next connection should target.
        port: u16,
    },
}

/// Internal channel message from the reader task to the event loop.
///
/// `NoiseTcpReadHalf::read_frame()` is not cancellation-safe: if dropped
/// mid-read, the internal codec state is left inconsistent. The fix is to
/// isolate it in a dedicated spawned task so it is never in a `select!`
/// branch that can be cancelled between bytes. This is the same pattern used
/// by `stratum-apps` itself in `Connection::spawn_reader`
/// (`network_helpers/noise_connection.rs`).
enum ReaderMessage {
    Message(AnyMessage<'static>),
    Error(StratumV2Error),
    Done,
}

/// Converts a `binary_sv2` error into this module's error type.
///
/// Lets call sites use `?` on the crate's own `Str0255::try_from`.
fn protocol_error(e: stratum_apps::stratum_core::binary_sv2::Error) -> StratumV2Error {
    StratumV2Error::Protocol(format!("invalid SV2 value: {e:?}"))
}

/// Encrypted Stratum V2 client using Extended Channels.
pub struct StratumV2Client {
    config: PoolConfig,
    event_tx: mpsc::Sender<ClientEvent>,
    command_rx: mpsc::Receiver<ClientCommand>,
    shutdown: CancellationToken,
}

impl StratumV2Client {
    /// Creates a client bound to the given event and command channels.
    ///
    /// The client does no I/O until [`StratumV2Client::run`] is called.
    pub fn new(
        config: PoolConfig,
        event_tx: mpsc::Sender<ClientEvent>,
        command_rx: mpsc::Receiver<ClientCommand>,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            config,
            event_tx,
            command_rx,
            shutdown,
        }
    }

    /// Connects to the pool and runs the full protocol lifecycle.
    pub async fn run(mut self) -> StratumV2Result<ClientOutcome> {
        const TCP_CONNECT_TIMEOUT_SECS: u64 = 5;
        const NOISE_HANDSHAKE_TIMEOUT_SECS: u64 = 5;

        let addr = resolve_host(&self.config.host(), self.config.port)
            .await
            .map_err(|e| StratumV2Error::DnsResolutionFailed {
                host: self.config.host(),
                error: e.to_string(),
            })?;

        debug!(host = %self.config.host(), %addr, "Resolved pool address");

        // TCP connect
        let stream = tokio::time::timeout(
            Duration::from_secs(TCP_CONNECT_TIMEOUT_SECS),
            tokio::net::TcpStream::connect(addr),
        )
        .await
        .map_err(|_| StratumV2Error::ConnectionFailed {
            addr,
            source: std::io::Error::new(std::io::ErrorKind::TimedOut, "TCP connect timeout"),
        })?
        .map_err(|source| StratumV2Error::ConnectionFailed { addr, source })?;

        debug!(host = %self.config.host(), "TCP connection established");

        // Noise NX handshake (via stratum-apps)
        let noise_stream = tokio::time::timeout(
            Duration::from_secs(NOISE_HANDSHAKE_TIMEOUT_SECS),
            connect_with_noise::<AnyMessage<'static>>(stream, Some(self.config.authority_pubkey)),
        )
        .await
        .map_err(|_| StratumV2Error::Timeout("the Noise handshake".to_string()))?
        .map_err(|e| StratumV2Error::Handshake(e.to_string()))?;

        info!(host = %self.config.host(), "Noise NX handshake completed");

        // Split + spawn reader task
        let (read_half, write_half) = noise_stream.into_split();

        let (reader_tx, mut reader_rx) = mpsc::channel::<ReaderMessage>(16);
        // The reader owns the socket's read half.  Cancelling its token on
        // every return from run stops it there, so it never stays parked in
        // read_frame holding the socket open until the pool closes it.
        let reader_shutdown = self.shutdown.child_token();
        let _reader_guard = reader_shutdown.clone().drop_guard();
        tokio::spawn(async move {
            reader_task(read_half, reader_tx, reader_shutdown).await;
        });

        // Negotiate session, returning the write half for the event loop.
        let write_half = match self.negotiate_session(&mut reader_rx, write_half).await {
            Ok(write_half) => write_half,
            Err(StratumV2Error::ReconnectDuringSetup { host, port }) => {
                return Ok(ClientOutcome::Reconnect { host, port });
            }
            Err(e) => return Err(e),
        };

        info!(host = %self.config.host(), user = %self.config.user_identity.as_utf8_or_hex(), "SV2 connection established");

        // Main event loop
        self.run_event_loop(reader_rx, write_half).await
    }

    /// Negotiates SetupConnection and OpenExtendedMiningChannel.
    ///
    /// Returns the write half for the event loop once the channel is open.
    /// Any message other than the expected response ends the attempt with a
    /// protocol error.
    ///
    /// If the pool sends a Reconnect during setup, returns
    /// `Err(ReconnectDuringSetup)` carrying the endpoint it names, so the
    /// caller can tear down and reconnect there.
    async fn negotiate_session(
        &self,
        reader_rx: &mut mpsc::Receiver<ReaderMessage>,
        mut write_half: NoiseTcpWriteHalf<AnyMessage<'static>>,
    ) -> StratumV2Result<NoiseTcpWriteHalf<AnyMessage<'static>>> {
        // A pool may go quiet after the handshake; without a budget on these
        // reads the setup exchange waits forever and the reconnect back-off
        // in the source is never reached.
        const SETUP_RESPONSE_TIMEOUT_SECS: u64 = 10;

        let setup = SetupConnection {
            protocol: Protocol::MiningProtocol,
            min_version: 2,
            max_version: 2,
            flags: constants::REQUIRES_VERSION_ROLLING,
            endpoint_host: self.config.host.clone(),
            endpoint_port: self.config.port,
            vendor: self.config.vendor.clone(),
            hardware_version: self.config.hardware_version.clone(),
            firmware: self.config.firmware.clone(),
            device_id: self.config.device_id.clone(),
        };

        debug!(host = %self.config.host(), "Sending SetupConnection");
        send_message(
            &mut write_half,
            AnyMessage::Common(CommonMessages::SetupConnection(setup.into_static())),
        )
        .await?;

        let setup_response = tokio::time::timeout(
            Duration::from_secs(SETUP_RESPONSE_TIMEOUT_SECS),
            recv_one(reader_rx),
        )
        .await
        .map_err(|_| StratumV2Error::Timeout("SetupConnection.Success".to_string()))??;

        match setup_response {
            AnyMessage::Common(CommonMessages::SetupConnectionSuccess(msg)) => {
                debug!(host = %self.config.host(), version = msg.used_version, "SetupConnection accepted");
                if msg.used_version != 2 {
                    return Err(StratumV2Error::Protocol(format!(
                        "negotiated SV2 version {} (expected 2)",
                        msg.used_version
                    )));
                }
                if msg.flags & constants::REQUIRES_FIXED_VERSION != 0 {
                    warn!(
                        host = %self.config.host(),
                        flags = format!("{:#010x}", msg.flags),
                        "Pool requires a fixed version field"
                    );
                    return Err(StratumV2Error::FixedVersionRequired);
                }
                self.emit(ClientEvent::SetupConnectionSuccess {
                    used_version: msg.used_version,
                    flags: msg.flags,
                })
                .await?;
            }
            AnyMessage::Common(CommonMessages::SetupConnectionError(msg)) => {
                let reason = msg.error_code.as_utf8_or_hex();
                warn!(host = %self.config.host(), %reason, "SetupConnection rejected");
                return Err(StratumV2Error::SetupRejected(reason));
            }
            unexpected => {
                return Err(StratumV2Error::Protocol(format!(
                    "expected SetupConnectionSuccess, got {unexpected:?}"
                )));
            }
        }

        let max_target_bytes = Target::MAX.to_le_bytes();
        let max_target = stratum_apps::stratum_core::binary_sv2::U256::from(max_target_bytes);

        let open = OpenExtendedMiningChannel {
            request_id: 1,
            user_identity: self.config.user_identity.clone(),
            nominal_hash_rate: self.config.nominal_hash_rate.0 as f32,
            max_target,
            min_extranonce_size: constants::MIN_EXTRANONCE_SIZE as u16,
        };

        debug!(host = %self.config.host(), "Sending OpenExtendedMiningChannel");
        send_message(
            &mut write_half,
            AnyMessage::Mining(Mining::OpenExtendedMiningChannel(open.into_static())),
        )
        .await?;

        let open_response = tokio::time::timeout(
            Duration::from_secs(SETUP_RESPONSE_TIMEOUT_SECS),
            recv_one(reader_rx),
        )
        .await
        .map_err(|_| StratumV2Error::Timeout("OpenExtendedMiningChannel.Success".to_string()))??;

        match open_response {
            AnyMessage::Mining(Mining::OpenExtendedMiningChannelSuccess(msg)) => {
                let target = target_from_le_bytes(msg.target.inner_as_ref())?;
                debug!(
                    host = %self.config.host(),
                    channel_id = msg.channel_id,
                    extranonce_prefix = %hex::encode(msg.extranonce_prefix.inner_as_ref()),
                    %target,
                    "OpenExtendedMiningChannel accepted"
                );

                // extranonce_size is the miner's own allocation and does not
                // include extranonce_prefix, which the upstream server
                // allocates and sends alongside it.
                if (msg.extranonce_size as usize) < constants::MIN_EXTRANONCE_SIZE
                    || (msg.extranonce_size as usize) > constants::MAX_EXTRANONCE_SIZE
                {
                    return Err(StratumV2Error::ExtranonceSizeMismatch);
                }
                if msg.request_id != 1 {
                    return Err(StratumV2Error::Protocol(format!(
                        "unexpected request_id {}",
                        msg.request_id
                    )));
                }

                self.emit(ClientEvent::OpenExtendedMiningChannelSuccess(msg))
                    .await?;
            }
            AnyMessage::Mining(Mining::OpenMiningChannelError(msg)) => {
                let reason = msg.error_code.as_utf8_or_hex();
                warn!(host = %self.config.host(), %reason, "OpenExtendedMiningChannel rejected");
                return Err(StratumV2Error::OpenChannelRejected(reason));
            }
            AnyMessage::Common(CommonMessages::Reconnect(msg)) => {
                let (host, port) = reconnect_endpoint(&self.config, &msg);
                info!(
                    host = %self.config.host(),
                    new_host = %host,
                    new_port = port,
                    "Pool requested reconnect during setup"
                );
                return Err(StratumV2Error::ReconnectDuringSetup { host, port });
            }
            unexpected => {
                return Err(StratumV2Error::Protocol(format!(
                    "expected OpenExtendedMiningChannelSuccess, got {unexpected:?}"
                )));
            }
        }

        Ok(write_half)
    }

    /// Dispatches outgoing commands and routes inbound messages.
    ///
    /// This is the client's main loop, entered once the channel is open.
    async fn run_event_loop(
        &mut self,
        mut reader_rx: mpsc::Receiver<ReaderMessage>,
        mut write_half: NoiseTcpWriteHalf<AnyMessage<'static>>,
    ) -> StratumV2Result<ClientOutcome> {
        loop {
            tokio::select! {
                reader_msg = reader_rx.recv() => {
                    match reader_msg {
                        Some(ReaderMessage::Message(msg)) => {
                            if let ControlFlow::Break(outcome) = self.handle_message(msg).await? {
                                return Ok(outcome);
                            }
                        }
                        Some(ReaderMessage::Error(e)) => {
                            error!(host = %self.config.host(), err = %e, "Reader task error");
                            return Err(e);
                        }
                        Some(ReaderMessage::Done) | None => {
                            info!(host = %self.config.host(), "Reader task finished");
                            return Ok(ClientOutcome::ConnectionClosed);
                        }
                    }
                }

                command = self.command_rx.recv() => {
                    match command {
                        Some(ClientCommand::SubmitShare(share)) => {
                            trace!(job_id = share.job_id, "Submitting share");
                            send_message(
                                &mut write_half,
                                AnyMessage::Mining(Mining::SubmitSharesExtended(share)),
                            ).await?;
                        }
                        Some(ClientCommand::UpdateChannel(update)) => {
                            debug!(
                                channel_id = update.channel_id,
                                nominal_hash_rate = update.nominal_hash_rate,
                                "Updating channel"
                            );
                            send_message(
                                &mut write_half,
                                AnyMessage::Mining(Mining::UpdateChannel(update)),
                            ).await?;
                        }
                        None => {
                            info!(host = %self.config.host(), "Command channel closed; stopping");
                            return Ok(ClientOutcome::Shutdown);
                        }
                    }
                }

                _ = self.shutdown.cancelled() => {
                    info!(host = %self.config.host(), "Shutting down");
                    return Ok(ClientOutcome::Shutdown);
                }
            }
        }
    }

    /// Dispatches a received message.
    ///
    /// Returns `Ok(ControlFlow::Break(outcome))` when the pool ends the
    /// connection (Reconnect, CloseChannel), `Ok(ControlFlow::Continue(()))`
    /// otherwise.
    async fn handle_message(
        &mut self,
        msg: AnyMessage<'static>,
    ) -> StratumV2Result<ControlFlow<ClientOutcome>> {
        match msg {
            AnyMessage::Mining(Mining::NewExtendedMiningJob(job)) => {
                trace!(
                    job_id = job.job_id,
                    future = job.is_future(),
                    "NewExtendedMiningJob"
                );
                self.emit(ClientEvent::NewExtendedMiningJob(job)).await?;
                Ok(ControlFlow::Continue(()))
            }
            AnyMessage::Mining(Mining::SetNewPrevHash(prev)) => {
                debug!(prev_hash = %hex::encode(prev.prev_hash.inner_as_ref()), "SetNewPrevHash");
                self.emit(ClientEvent::SetNewPrevHash(prev)).await?;
                Ok(ControlFlow::Continue(()))
            }
            AnyMessage::Mining(Mining::SetTarget(target)) => {
                debug!(channel_id = target.channel_id, "SetTarget");
                self.emit(ClientEvent::SetTarget(target)).await?;
                Ok(ControlFlow::Continue(()))
            }
            AnyMessage::Mining(Mining::SubmitSharesSuccess(success)) => {
                trace!(channel_id = success.channel_id, "SubmitShares.Success");
                self.emit(ClientEvent::SubmitSharesSuccess(success)).await?;
                Ok(ControlFlow::Continue(()))
            }
            AnyMessage::Mining(Mining::SubmitSharesError(error)) => {
                warn!(
                    channel_id = error.channel_id,
                    seq = error.sequence_number,
                    reason = error.error_code.as_utf8_or_hex(),
                    "SubmitShares.Error"
                );
                self.emit(ClientEvent::SubmitSharesError(error)).await?;
                Ok(ControlFlow::Continue(()))
            }
            AnyMessage::Common(CommonMessages::Reconnect(msg)) => {
                let (host, port) = reconnect_endpoint(&self.config, &msg);
                info!(
                    host = %self.config.host(),
                    new_host = %host,
                    new_port = port,
                    "Reconnect"
                );
                Ok(ControlFlow::Break(ClientOutcome::Reconnect { host, port }))
            }
            // The message resets the extension state of a channel whose
            // endpoint moved.  This client negotiates no extensions, so there
            // is nothing to reset and the channel carries on.
            AnyMessage::Common(CommonMessages::ChannelEndpointChanged(msg)) => {
                info!(channel_id = msg.channel_id, "ChannelEndpointChanged");
                Ok(ControlFlow::Continue(()))
            }
            // Group membership is not tracked beyond the group_channel_id the
            // channel opened with, so a move to another group is only logged.
            AnyMessage::Mining(Mining::SetGroupChannel(msg)) => {
                info!(
                    group_channel_id = msg.group_channel_id,
                    channel_ids = ?msg.channel_ids,
                    "SetGroupChannel"
                );
                Ok(ControlFlow::Continue(()))
            }
            AnyMessage::Mining(Mining::CloseChannel(msg)) => {
                info!(channel_id = msg.channel_id, "CloseChannel");
                self.emit(ClientEvent::CloseChannel(msg)).await?;
                Ok(ControlFlow::Break(ClientOutcome::ChannelClosed))
            }
            AnyMessage::Mining(Mining::UpdateChannelError(error)) => {
                warn!(
                    channel_id = error.channel_id,
                    reason = error.error_code.as_utf8_or_hex(),
                    "UpdateChannel.Error"
                );
                Ok(ControlFlow::Continue(()))
            }
            unexpected => {
                warn!(host = %self.config.host(), ?unexpected, "Ignoring unexpected message");
                Ok(ControlFlow::Continue(()))
            }
        }
    }

    async fn emit(&self, event: ClientEvent) -> StratumV2Result<()> {
        self.event_tx
            .send(event)
            .await
            .map_err(|_| StratumV2Error::Protocol("event receiver dropped".to_string()))
    }
}

async fn reader_task(
    mut read_half: NoiseTcpReadHalf<AnyMessage<'static>>,
    tx: mpsc::Sender<ReaderMessage>,
    shutdown: CancellationToken,
) {
    loop {
        tokio::select! {
            result = read_half.read_frame() => {
                match result {
                    Ok(frame) => {
                        let msg = match extract_any_message(frame) {
                            Ok(Some(m)) => m,
                            Ok(None) => continue,
                            Err(e) => {
                                let _ = tx.send(ReaderMessage::Error(e)).await;
                                return;
                            }
                        };
                        if tx.send(ReaderMessage::Message(msg)).await.is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        if matches!(e, NetworkError::SocketClosed) {
                            debug!("Pool closed connection");
                            let _ = tx.send(ReaderMessage::Done).await;
                        } else {
                            warn!(err = %e, "Read error from pool");
                            let _ = tx.send(ReaderMessage::Error(
                                StratumV2Error::Protocol(format!("read error: {e}"))
                            )).await;
                        }
                        return;
                    }
                }
            }
            _ = shutdown.cancelled() => {
                let _ = tx.send(ReaderMessage::Done).await;
                return;
            }
        }
    }
}

/// Decodes a frame into a message.
///
/// Returns `Ok(None)` for a message of a protocol extension.  This client
/// negotiates no extensions, so any non-zero `extension_type` is one it
/// cannot interpret, and the spec requires such messages to be discarded
/// rather than treated as errors.
fn extract_any_message(
    frame: StandardEitherFrame<AnyMessage<'static>>,
) -> StratumV2Result<Option<AnyMessage<'static>>> {
    match frame {
        StandardEitherFrame::Sv2(mut sv2_frame) => {
            let header = sv2_frame
                .get_header()
                .ok_or_else(|| StratumV2Error::Protocol("frame without header".to_string()))?;
            let extension_type = header.ext_type_without_channel_msg();
            if extension_type != 0 {
                debug!(
                    extension_type = format!("{extension_type:#06x}"),
                    msg_type = header.msg_type(),
                    "Discarding message of an unknown extension"
                );
                return Ok(None);
            }
            AnyMessage::try_from((header, sv2_frame.payload()))
                .map(|m| Some(m.into_static()))
                .map_err(|e| StratumV2Error::Protocol(format!("parse error: {e}")))
        }
        StandardEitherFrame::HandShake(_) => Err(StratumV2Error::Protocol(
            "unexpected handshake frame after Noise handshake".to_string(),
        )),
    }
}

/// Resolves a `Reconnect` into the endpoint the next connection targets.
///
/// An empty `new_host` or a zero `new_port` stands for the endpoint in use.
fn reconnect_endpoint(config: &PoolConfig, msg: &Reconnect<'_>) -> (String, u16) {
    let host = if msg.new_host.inner_as_ref().is_empty() {
        config.host()
    } else {
        msg.new_host.as_utf8_or_hex()
    };
    let port = if msg.new_port == 0 {
        config.port
    } else {
        msg.new_port
    };
    (host, port)
}

async fn send_message(
    write_half: &mut NoiseTcpWriteHalf<AnyMessage<'static>>,
    msg: AnyMessage<'static>,
) -> StratumV2Result<()> {
    let sv2_frame =
        msg.try_into()
            .map_err(|e: stratum_apps::stratum_core::parsers_sv2::ParserError| {
                StratumV2Error::Protocol(format!("frame encode failed: {e}"))
            })?;
    write_half
        .write_frame(StandardEitherFrame::Sv2(sv2_frame))
        .await
        .map_err(|e| StratumV2Error::Protocol(format!("write_frame failed: {e}")))
}

async fn recv_one(rx: &mut mpsc::Receiver<ReaderMessage>) -> StratumV2Result<AnyMessage<'static>> {
    match rx.recv().await {
        Some(ReaderMessage::Message(msg)) => Ok(msg),
        Some(ReaderMessage::Error(e)) => Err(e),
        Some(ReaderMessage::Done) | None => Err(StratumV2Error::Protocol(
            "reader terminated during negotiation".to_string(),
        )),
    }
}

/// Decodes a 32-byte little-endian SV2 target into a [`Target`].
///
/// # Errors
///
/// Returns [`StratumV2Error::Protocol`] if `bytes` is not exactly 32 bytes.
pub(crate) fn target_from_le_bytes(bytes: &[u8]) -> StratumV2Result<Target> {
    let arr: [u8; 32] = bytes.try_into().map_err(|_| {
        StratumV2Error::Protocol(format!(
            "expected 32-byte target, got {} bytes",
            bytes.len()
        ))
    })?;
    Ok(Target::from_le_bytes(arr))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config() -> PoolConfig {
        PoolConfig::new(
            "pool.example.com".to_string(),
            3333,
            "9bDuixKmZqAJnrmP746n8zU1wyAQRrus7th9dxnkPg6RzQvCnan"
                .parse()
                .unwrap(),
            "worker".to_string(),
            "mujina".to_string(),
            "unknown".to_string(),
            "mujina-miner/test".to_string(),
            String::new(),
            HashRate::from_terahashes(1.0),
        )
        .unwrap()
    }

    fn make_reconnect(new_host: &str, new_port: u16) -> Reconnect<'static> {
        Reconnect {
            new_host: Str0255::try_from(new_host.to_string()).unwrap(),
            new_port,
        }
    }

    /// Contract: a Reconnect naming a host and port sends the next connection
    /// there.
    #[test]
    fn reconnect_names_the_next_endpoint() {
        let endpoint =
            reconnect_endpoint(&make_config(), &make_reconnect("backup.example.com", 4444));

        assert_eq!(endpoint, ("backup.example.com".to_string(), 4444));
    }

    /// Contract: an empty host or a zero port in a Reconnect keeps that part of
    /// the endpoint in use.
    #[test]
    fn reconnect_blank_fields_keep_the_current_endpoint() {
        let config = make_config();

        assert_eq!(
            reconnect_endpoint(&config, &make_reconnect("", 0)),
            ("pool.example.com".to_string(), 3333)
        );
        assert_eq!(
            reconnect_endpoint(&config, &make_reconnect("", 4444)),
            ("pool.example.com".to_string(), 4444)
        );
        assert_eq!(
            reconnect_endpoint(&config, &make_reconnect("backup.example.com", 0)),
            ("backup.example.com".to_string(), 3333)
        );
    }

    /// Contract: moving a configuration to another endpoint keeps the
    /// authority key, so a Reconnect cannot send the miner to a server
    /// outside the pool's authority.
    #[test]
    fn with_endpoint_keeps_the_authority_key() {
        let config = make_config();

        let moved = config
            .with_endpoint("backup.example.com".to_string(), 4444)
            .unwrap();

        assert_eq!(moved.host(), "backup.example.com");
        assert_eq!(moved.port, 4444);
        assert_eq!(
            format!("{:?}", moved.authority_pubkey),
            format!("{:?}", config.authority_pubkey)
        );
    }
}
