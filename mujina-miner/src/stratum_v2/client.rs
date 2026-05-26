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
use stratum_apps::stratum_core::common_messages_sv2::{
    ChannelEndpointChanged, Protocol, Reconnect, SetupConnection,
};
use stratum_apps::stratum_core::mining_sv2::{
    CloseChannel, NewExtendedMiningJob, OpenExtendedMiningChannel,
    OpenExtendedMiningChannelSuccess, SetNewPrevHash, SetTarget, SubmitSharesError,
    SubmitSharesExtended, SubmitSharesSuccess,
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

    /// The pool host as text, for DNS resolution and logging. `Str0255`'s own
    /// formatting renders the raw wire bytes, so callers that need the
    /// hostname string go through here.
    pub(crate) fn host(&self) -> String {
        self.host.as_utf8_or_hex()
    }
}

/// Commands sent to the SV2 client from the consumer.
#[derive(Debug, Clone)]
pub enum ClientCommand {
    SubmitShare(SubmitSharesExtended<'static>),
}

/// Events emitted by the SV2 client to the consumer.
#[derive(Debug, Clone)]
pub enum ClientEvent {
    /// Pool accepted SetupConnection.
    SetupConnectionSuccess { used_version: u16, flags: u32 },
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
    /// Pool requested a reconnect.
    Reconnect(Reconnect<'static>),
    /// Pool reassigned the channel to a different endpoint.
    ChannelEndpointChanged(ChannelEndpointChanged),
    /// Pool closed the Extended Channel.
    CloseChannel(CloseChannel<'static>),
}

/// Outcome returned by [`StratumV2Client::run`] on a clean exit.
///
/// Lets the caller distinguish a user-requested shutdown from a connection
/// close without relying on a side-channel flag or error-string inspection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientOutcome {
    /// Cancellation token was fired; miner requested a clean stop.
    Shutdown,
    /// TCP connection closed (pool side or network); caller should reconnect.
    ConnectionClosed,
    /// Pool sent `Reconnect`, `CloseChannel`, or `ChannelEndpointChanged`;
    /// caller should reconnect (possibly to a new host).
    PoolRequestedReconnect,
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

/// Converts a `binary_sv2` specific error type to a our error type
/// so call sites can use `?` on the crate's own `Str0255::try_from`.
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

    /// Connect to the pool and run the full protocol lifecycle.
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
        .map_err(|_| StratumV2Error::Protocol("Noise handshake timed out".to_string()))?
        .map_err(|e| StratumV2Error::Protocol(format!("Noise handshake failed: {e}")))?;

        info!(host = %self.config.host(), "Noise NX handshake completed");

        // Split + spawn reader task
        let (read_half, write_half) = noise_stream.into_split();

        let (reader_tx, mut reader_rx) = mpsc::channel::<ReaderMessage>(16);
        let reader_shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            reader_task(read_half, reader_tx, reader_shutdown).await;
        });

        // Negotiate session, returning the write half for the event loop.
        let write_half = match self.negotiate_session(&mut reader_rx, write_half).await {
            Ok(wh) => wh,
            Err(StratumV2Error::ReconnectDuringSetup) => {
                return Ok(ClientOutcome::PoolRequestedReconnect);
            }
            Err(e) => return Err(e),
        };

        info!(host = %self.config.host(), user = %self.config.user_identity.as_utf8_or_hex(), "SV2 connection established");

        // Main event loop
        self.run_event_loop(reader_rx, write_half).await
    }

    /// Negotiate SetupConnection + OpenExtendedMiningChannel.
    ///
    /// Returns the write half on success. If the pool sends a Reconnect
    /// during setup, emits the event and returns `Err(ReconnectDuringSetup)`
    /// so the caller can tear down and reconnect.
    async fn negotiate_session(
        &self,
        reader_rx: &mut mpsc::Receiver<ReaderMessage>,
        mut write_half: NoiseTcpWriteHalf<AnyMessage<'static>>,
    ) -> StratumV2Result<NoiseTcpWriteHalf<AnyMessage<'static>>> {
        let setup = SetupConnection {
            protocol: Protocol::MiningProtocol,
            min_version: 2,
            max_version: 2,
            flags: 0,
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

        match recv_one(reader_rx).await? {
            AnyMessage::Common(CommonMessages::SetupConnectionSuccess(msg)) => {
                debug!(host = %self.config.host(), version = msg.used_version, "SetupConnection accepted");
                if msg.used_version != 2 {
                    return Err(StratumV2Error::Protocol(format!(
                        "negotiated SV2 version {} (expected 2)",
                        msg.used_version
                    )));
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

        loop {
            match recv_one(reader_rx).await? {
                AnyMessage::Mining(Mining::OpenExtendedMiningChannelSuccess(msg)) => {
                    let target = target_from_le_bytes(msg.target.inner_as_ref())?;
                    debug!(
                        host = %self.config.host(),
                        channel_id = msg.channel_id,
                        extranonce_prefix = %hex::encode(msg.extranonce_prefix.inner_as_ref()),
                        %target,
                        "OpenExtendedMiningChannel accepted"
                    );

                    // extranonce_size covers the prefix and the rollable bytes areas
                    // of the Extended Extranonce; extranonce_prefix (already allocated by the
                    // upstream server) is separate.
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
                    break;
                }
                AnyMessage::Mining(Mining::OpenMiningChannelError(msg)) => {
                    let reason = msg.error_code.as_utf8_or_hex();
                    warn!(host = %self.config.host(), %reason, "OpenExtendedMiningChannel rejected");
                    return Err(StratumV2Error::OpenChannelRejected(reason));
                }
                AnyMessage::Common(CommonMessages::Reconnect(msg)) => {
                    info!(host = %self.config.host(), "Pool requested reconnect during setup");
                    self.emit(ClientEvent::Reconnect(msg)).await?;
                    return Err(StratumV2Error::ReconnectDuringSetup);
                }
                // Pools may pipeline SetTarget, SetNewPrevHash, or NewExtendedMiningJob
                // before the channel-open response arrives. Skip them here; the event
                // loop processes them once the channel is established.
                AnyMessage::Mining(
                    Mining::SetTarget(_)
                    | Mining::SetNewPrevHash(_)
                    | Mining::NewExtendedMiningJob(_),
                ) => {
                    debug!(host = %self.config.host(), "Skipping pipelined mining message during channel open");
                }
                unexpected => {
                    return Err(StratumV2Error::Protocol(format!(
                        "expected OpenExtendedMiningChannelSuccess, got {unexpected:?}"
                    )));
                }
            }
        }

        Ok(write_half)
    }

    /// Main event loop: dispatch commands and route inbound messages.
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
                            if self.handle_message(msg).await?.is_break() {
                                info!(host = %self.config.host(), "Pool requested disconnect");
                                return Ok(ClientOutcome::PoolRequestedReconnect);
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

    /// Dispatch a received message.
    ///
    /// Returns `Ok(ControlFlow::Break(()))` when the pool requests a disconnect
    /// (Reconnect, ChannelEndpointChanged, CloseChannel), `Ok(ControlFlow::Continue(()))`
    /// otherwise.
    async fn handle_message(
        &mut self,
        msg: AnyMessage<'static>,
    ) -> StratumV2Result<ControlFlow<()>> {
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
                info!(host = %self.config.host(), "Reconnect");
                self.emit(ClientEvent::Reconnect(msg)).await?;
                Ok(ControlFlow::Break(()))
            }
            AnyMessage::Common(CommonMessages::ChannelEndpointChanged(msg)) => {
                info!(host = %self.config.host(), "ChannelEndpointChanged");
                self.emit(ClientEvent::ChannelEndpointChanged(msg)).await?;
                Ok(ControlFlow::Break(()))
            }
            AnyMessage::Mining(Mining::CloseChannel(msg)) => {
                info!(channel_id = msg.channel_id, "CloseChannel");
                self.emit(ClientEvent::CloseChannel(msg)).await?;
                Ok(ControlFlow::Break(()))
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
                            Ok(m) => m,
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

fn extract_any_message(
    frame: StandardEitherFrame<AnyMessage<'static>>,
) -> StratumV2Result<AnyMessage<'static>> {
    match frame {
        StandardEitherFrame::Sv2(mut sv2_frame) => {
            let header = sv2_frame
                .get_header()
                .ok_or_else(|| StratumV2Error::Protocol("frame without header".to_string()))?;
            AnyMessage::try_from((header, sv2_frame.payload()))
                .map(|m| m.into_static())
                .map_err(|e| StratumV2Error::Protocol(format!("parse error: {e}")))
        }
        StandardEitherFrame::HandShake(_) => Err(StratumV2Error::Protocol(
            "unexpected handshake frame after Noise handshake".to_string(),
        )),
    }
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

pub(crate) fn target_from_le_bytes(bytes: &[u8]) -> StratumV2Result<Target> {
    let arr: [u8; 32] = bytes.try_into().map_err(|_| {
        StratumV2Error::Protocol(format!(
            "expected 32-byte target, got {} bytes",
            bytes.len()
        ))
    })?;
    Ok(Target::from_le_bytes(arr))
}
