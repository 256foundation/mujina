//! Encrypted Stratum V2 Extended Channel client.
//!
//! The client performs the full SV2 connection sequence:
//! DNS resolve -> TCP connect -> Noise NX handshake -> SetupConnection ->
//! OpenExtendedMiningChannel -> main select! event loop.
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

use bitcoin::block::Version;
use bitcoin::pow::Target;
use stratum_apps::key_utils::Secp256k1PublicKey;
use stratum_apps::network_helpers::Error as NetworkError;
use stratum_apps::network_helpers::connect_with_noise;
use stratum_apps::network_helpers::noise_stream::{NoiseTcpReadHalf, NoiseTcpWriteHalf};
use stratum_apps::network_helpers::resolve_host;
use stratum_apps::stratum_core::binary_sv2::{B032Owned, Str0255Owned, U256Owned};
use stratum_apps::stratum_core::channels_sv2::extranonce_manager::ExtranoncePrefix;
use stratum_apps::stratum_core::codec_sv2::{MessageFrame, SerializedFrame};
use stratum_apps::stratum_core::common_messages_sv2::{
    Protocol, ReconnectOwned, SetupConnectionOwned,
};
use stratum_apps::stratum_core::mining_sv2::{
    CloseChannelOwned, NewExtendedMiningJobOwned, OpenExtendedMiningChannelOwned,
    OpenExtendedMiningChannelSuccessOwned, SetExtranoncePrefixOwned, SetNewPrevHashOwned,
    SetTargetOwned, SubmitSharesErrorOwned, SubmitSharesExtendedOwned, SubmitSharesSuccess,
    UpdateChannelOwned,
};
use stratum_apps::stratum_core::parsers_sv2::{
    AnyMessage, AnyMessageOwned, CommonMessagesOwned, MiningOwned,
};
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
    pub const MIN_EXTRANONCE_SIZE: u16 = 8;

    /// Maximum total extranonce size (in bytes) accepted from the pool.
    ///
    /// `SubmitSharesExtended.extranonce` is typed `B032` (max 32 bytes), so
    /// any pool-assigned `extranonce_size > 32` would cause every share to
    /// fail `B032` encoding.  Reject the channel at open time instead.
    pub const MAX_EXTRANONCE_SIZE: u16 = 32;

    /// `SetupConnection.flags` bit announcing that the miner rolls the
    /// version field, so the pool must only send jobs that allow it.
    pub const REQUIRES_VERSION_ROLLING: u32 = 1 << 2;

    /// `SetupConnection.Success.flags` bit by which the pool refuses any
    /// change to the version field.
    pub const REQUIRES_FIXED_VERSION: u32 = 1 << 0;
}

/// Stratum V2 pool connection configuration.
///
/// Text fields are encoded as SV2 `STR0_255` where the messages carrying
/// them are built; [`PoolConfig::validate`] checks up front that each one
/// fits.
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Pool host name or IP address.
    pub host: String,
    /// Pool TCP port.
    pub port: u16,
    /// Public key that authenticates the pool in the Noise_NX handshake.
    pub authority_key: Secp256k1PublicKey,
    /// Identity the channel is opened for, sent in
    /// `OpenExtendedMiningChannel`.
    pub user_identity: String,
    /// Device vendor, sent in `SetupConnection`.
    pub vendor: String,
    /// Device hardware version, sent in `SetupConnection`.
    pub hardware_version: String,
    /// Device firmware, sent in `SetupConnection`.
    pub firmware: String,
    /// Device identifier, sent in `SetupConnection`.
    pub device_id: String,
    /// Hash rate the channel is opened with.
    pub nominal_hash_rate: HashRate,
}

impl PoolConfig {
    /// Checks that every text field fits the SV2 `STR0_255` type.
    ///
    /// # Errors
    ///
    /// Returns [`StratumV2Error::Protocol`] naming the first field longer
    /// than 255 bytes.
    pub fn validate(&self) -> StratumV2Result<()> {
        for (field, value) in [
            ("host", &self.host),
            ("user identity", &self.user_identity),
            ("vendor", &self.vendor),
            ("hardware version", &self.hardware_version),
            ("firmware", &self.firmware),
            ("device id", &self.device_id),
        ] {
            Str0255Owned::try_from(value.as_bytes()).map_err(|_| {
                StratumV2Error::Protocol(format!("{field} is longer than 255 bytes"))
            })?;
        }
        Ok(())
    }

    /// Returns a copy of this configuration that connects to `host:port`.
    ///
    /// Every other field is kept, the authority public key included, so a
    /// pool can move the miner between its own servers but never to a server
    /// outside its authority.
    pub(crate) fn with_endpoint(&self, host: String, port: u16) -> Self {
        Self {
            host,
            port,
            ..self.clone()
        }
    }
}

/// Commands sent to the SV2 client from the consumer.
#[derive(Debug, Clone)]
pub enum ClientCommand {
    /// Send a `SubmitSharesExtended` message to the pool.
    SubmitShare(ShareSubmission),
    /// Send an `UpdateChannel` message to the pool.
    UpdateChannel {
        /// Channel the update applies to.
        channel_id: u32,
        /// Hash rate the pool should size the channel's target for.
        nominal_hash_rate: HashRate,
    },
}

/// A share to submit to the pool.
///
/// The client numbers it within the connection and encodes it as
/// `SubmitSharesExtended`.
#[derive(Debug, Clone)]
pub struct ShareSubmission {
    /// Channel the share was mined on.
    pub channel_id: u32,
    /// Job the share was mined for.
    pub job_id: u32,
    /// Nonce found by the hardware.
    pub nonce: u32,
    /// Block header timestamp.
    pub ntime: u32,
    /// Block header version, including any rolled bits.
    pub version: Version,
    /// The miner's extranonce, excluding the pool-assigned prefix.
    pub extranonce: Vec<u8>,
}

/// Events emitted by the SV2 client to the consumer.
///
/// The client checks wire values and converts them to domain types before
/// emitting them.  Jobs and prev hashes stay wire messages because the
/// consumer passes them whole to `channels_sv2`'s `ExtendedChannel`.
#[derive(Debug)]
pub enum ClientEvent {
    /// Pool accepted SetupConnection.
    SetupConnectionSuccess {
        /// Protocol version the pool negotiated.
        used_version: u16,
        /// Capability flags the pool returned.
        flags: u32,
    },
    /// Pool accepted OpenExtendedMiningChannel.
    OpenExtendedMiningChannelSuccess {
        /// Channel the pool opened.
        channel_id: u32,
        /// Group the channel belongs to.
        group_channel_id: u32,
        /// Initial share target.
        target: Target,
        /// Pool-assigned bytes that precede the miner's extranonce.
        extranonce_prefix: ExtranoncePrefix,
        /// Bytes the miner rolls after the prefix, within
        /// [`constants::MIN_EXTRANONCE_SIZE`] and
        /// [`constants::MAX_EXTRANONCE_SIZE`].
        extranonce_size: u16,
    },
    /// New mining job from the pool.
    NewExtendedMiningJob(NewExtendedMiningJobOwned),
    /// New previous block hash (invalidates un-activated future jobs).
    SetNewPrevHash(SetNewPrevHashOwned),
    /// Pool updated the share difficulty target.
    SetTarget {
        /// Channel or group the target applies to.
        channel_id: u32,
        /// New maximum share target.
        target: Target,
    },
    /// Pool replaced the channel's extranonce prefix.
    SetExtranoncePrefix {
        /// Channel the prefix applies to.
        channel_id: u32,
        /// New prefix for jobs that follow.
        extranonce_prefix: ExtranoncePrefix,
    },
    /// Pool accepted previously submitted shares.
    SubmitSharesSuccess {
        /// Channel or group the acknowledgement applies to.
        channel_id: u32,
        /// Highest sequence number the acknowledgement covers.
        last_sequence_number: u32,
        /// Count of submits accepted in this batch.
        new_submits_accepted_count: u32,
    },
    /// Pool rejected a previously submitted share.
    SubmitSharesError {
        /// Channel or group the rejection applies to.
        channel_id: u32,
        /// Sequence number of the rejected share.
        sequence_number: u32,
        /// Error code the pool gave.
        reason: String,
    },
    /// Pool closed the Extended Channel.
    CloseChannel {
        /// Channel the pool closed.
        channel_id: u32,
        /// Reason code the pool gave.
        reason: String,
    },
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
    Message(AnyMessageOwned),
    Error(StratumV2Error),
    Done,
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
        const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
        const NOISE_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

        let addrs = resolve_host(&self.config.host, self.config.port)
            .await
            .map_err(|e| StratumV2Error::DnsResolutionFailed {
                host: self.config.host.clone(),
                error: e.to_string(),
            })?;

        debug!(host = %self.config.host, ?addrs, "Resolved pool addresses");

        // TCP connect, trying each resolved address in turn
        let endpoint = format!("{}:{}", self.config.host, self.config.port);
        let stream = tokio::time::timeout(
            TCP_CONNECT_TIMEOUT,
            tokio::net::TcpStream::connect(addrs.as_slice()),
        )
        .await
        .map_err(|_| StratumV2Error::ConnectionFailed {
            endpoint: endpoint.clone(),
            source: std::io::Error::new(std::io::ErrorKind::TimedOut, "TCP connect timeout"),
        })?
        .map_err(|source| StratumV2Error::ConnectionFailed { endpoint, source })?;

        debug!(host = %self.config.host, "TCP connection established");

        // Noise NX handshake (via stratum-apps)
        let noise_stream = tokio::time::timeout(
            NOISE_HANDSHAKE_TIMEOUT,
            connect_with_noise(stream, Some(self.config.authority_key)),
        )
        .await
        .map_err(|_| StratumV2Error::Timeout("the Noise handshake".to_string()))?
        .map_err(|e| StratumV2Error::Handshake(e.to_string()))?;

        info!(host = %self.config.host, "Noise NX handshake completed");

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

        info!(host = %self.config.host, user = %self.config.user_identity, "SV2 connection established");

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
        mut write_half: NoiseTcpWriteHalf,
    ) -> StratumV2Result<NoiseTcpWriteHalf> {
        // A pool may go quiet after the handshake; without a budget on these
        // reads the setup exchange waits forever and the reconnect back-off
        // in the source is never reached.
        const SETUP_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

        let setup = SetupConnectionOwned {
            protocol: Protocol::MiningProtocol,
            min_version: 2,
            max_version: 2,
            flags: constants::REQUIRES_VERSION_ROLLING,
            endpoint_host: self.config.host.as_bytes().try_into()?,
            endpoint_port: self.config.port,
            vendor: self.config.vendor.as_bytes().try_into()?,
            hardware_version: self.config.hardware_version.as_bytes().try_into()?,
            firmware: self.config.firmware.as_bytes().try_into()?,
            device_id: self.config.device_id.as_bytes().try_into()?,
        };

        debug!(host = %self.config.host, "Sending SetupConnection");
        send_message(
            &mut write_half,
            AnyMessageOwned::Common(CommonMessagesOwned::SetupConnection(setup)),
        )
        .await?;

        let setup_response = tokio::time::timeout(SETUP_RESPONSE_TIMEOUT, recv_one(reader_rx))
            .await
            .map_err(|_| StratumV2Error::Timeout("SetupConnection.Success".to_string()))??;

        match setup_response {
            AnyMessageOwned::Common(CommonMessagesOwned::SetupConnectionSuccess(msg)) => {
                debug!(host = %self.config.host, version = msg.used_version, "SetupConnection accepted");
                if msg.used_version != 2 {
                    return Err(StratumV2Error::Protocol(format!(
                        "negotiated SV2 version {} (expected 2)",
                        msg.used_version
                    )));
                }
                if msg.flags & constants::REQUIRES_FIXED_VERSION != 0 {
                    warn!(
                        host = %self.config.host,
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
            AnyMessageOwned::Common(CommonMessagesOwned::SetupConnectionError(msg)) => {
                let reason = msg.error_code.as_utf8_or_hex();
                warn!(host = %self.config.host, %reason, "SetupConnection rejected");
                return Err(StratumV2Error::SetupRejected(reason));
            }
            unexpected => {
                return Err(StratumV2Error::Protocol(format!(
                    "expected SetupConnectionSuccess, got {unexpected:?}"
                )));
            }
        }

        let max_target = U256Owned::from(Target::MAX.to_le_bytes());

        let open = OpenExtendedMiningChannelOwned {
            request_id: 1,
            user_identity: self.config.user_identity.as_bytes().try_into()?,
            nominal_hash_rate: self.config.nominal_hash_rate.0 as f32,
            max_target,
            min_extranonce_size: constants::MIN_EXTRANONCE_SIZE,
        };

        debug!(host = %self.config.host, "Sending OpenExtendedMiningChannel");
        send_message(
            &mut write_half,
            AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannel(open)),
        )
        .await?;

        let open_response = tokio::time::timeout(SETUP_RESPONSE_TIMEOUT, recv_one(reader_rx))
            .await
            .map_err(|_| {
                StratumV2Error::Timeout("OpenExtendedMiningChannel.Success".to_string())
            })??;

        match open_response {
            AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannelSuccess(msg)) => {
                // extranonce_size is the miner's own allocation and does not
                // include extranonce_prefix, which the upstream server
                // allocates and sends alongside it.
                if !(constants::MIN_EXTRANONCE_SIZE..=constants::MAX_EXTRANONCE_SIZE)
                    .contains(&msg.extranonce_size)
                {
                    return Err(StratumV2Error::ExtranonceSizeMismatch);
                }
                if msg.request_id != 1 {
                    return Err(StratumV2Error::Protocol(format!(
                        "unexpected request_id {}",
                        msg.request_id
                    )));
                }

                let event = ClientEvent::try_from(msg)?;
                debug!(host = %self.config.host, ?event, "OpenExtendedMiningChannel accepted");
                self.emit(event).await?;
            }
            AnyMessageOwned::Mining(MiningOwned::OpenMiningChannelError(msg)) => {
                let reason = msg.error_code.as_utf8_or_hex();
                warn!(host = %self.config.host, %reason, "OpenExtendedMiningChannel rejected");
                return Err(StratumV2Error::OpenChannelRejected(reason));
            }
            AnyMessageOwned::Common(CommonMessagesOwned::Reconnect(msg)) => {
                let (host, port) = reconnect_endpoint(&self.config, &msg);
                info!(
                    host = %self.config.host,
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
        mut write_half: NoiseTcpWriteHalf,
    ) -> StratumV2Result<ClientOutcome> {
        let mut next_seq: u32 = 0;

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
                            error!(host = %self.config.host, error = %e, "Reader task error");
                            return Err(e);
                        }
                        Some(ReaderMessage::Done) | None => {
                            info!(host = %self.config.host, "Reader task finished");
                            return Ok(ClientOutcome::ConnectionClosed);
                        }
                    }
                }

                command = self.command_rx.recv() => {
                    match command {
                        Some(ClientCommand::SubmitShare(share)) => {
                            let job_id = share.job_id;
                            let submit = match submit_message(share, next_seq) {
                                Ok(submit) => submit,
                                Err(e) => {
                                    warn!(
                                        job_id,
                                        error = %e,
                                        "Failed to encode extranonce as B032; dropping"
                                    );
                                    continue;
                                }
                            };
                            trace!(
                                job_id,
                                seq = next_seq,
                                nonce = format!("{:#010x}", submit.nonce),
                                "Submitting share to pool"
                            );
                            next_seq = next_seq.wrapping_add(1);
                            send_message(
                                &mut write_half,
                                AnyMessageOwned::Mining(MiningOwned::SubmitSharesExtended(submit)),
                            ).await?;
                        }
                        Some(ClientCommand::UpdateChannel { channel_id, nominal_hash_rate }) => {
                            debug!(channel_id, %nominal_hash_rate, "Updating channel");
                            let update = UpdateChannelOwned {
                                channel_id,
                                nominal_hash_rate: nominal_hash_rate.0 as f32,
                                maximum_target: U256Owned::from(Target::MAX.to_le_bytes()),
                            };
                            send_message(
                                &mut write_half,
                                AnyMessageOwned::Mining(MiningOwned::UpdateChannel(update)),
                            ).await?;
                        }
                        None => {
                            info!(host = %self.config.host, "Command channel closed; stopping");
                            return Ok(ClientOutcome::Shutdown);
                        }
                    }
                }

                _ = self.shutdown.cancelled() => {
                    info!(host = %self.config.host, "Shutting down");
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
        msg: AnyMessageOwned,
    ) -> StratumV2Result<ControlFlow<ClientOutcome>> {
        match msg {
            AnyMessageOwned::Mining(MiningOwned::NewExtendedMiningJob(job)) => {
                trace!(
                    job_id = job.job_id,
                    future = job.is_future(),
                    "NewExtendedMiningJob"
                );
                self.emit(ClientEvent::NewExtendedMiningJob(job)).await?;
                Ok(ControlFlow::Continue(()))
            }
            AnyMessageOwned::Mining(MiningOwned::SetNewPrevHash(prev)) => {
                trace!(prev_hash = %hex::encode(prev.prev_hash.as_bytes()), "SetNewPrevHash");
                self.emit(ClientEvent::SetNewPrevHash(prev)).await?;
                Ok(ControlFlow::Continue(()))
            }
            AnyMessageOwned::Mining(MiningOwned::SetTarget(target)) => {
                debug!(channel_id = target.channel_id, "SetTarget");
                self.emit(ClientEvent::from(target)).await?;
                Ok(ControlFlow::Continue(()))
            }
            AnyMessageOwned::Mining(MiningOwned::SetExtranoncePrefix(msg)) => {
                debug!(channel_id = msg.channel_id, "SetExtranoncePrefix");
                self.emit(ClientEvent::try_from(msg)?).await?;
                Ok(ControlFlow::Continue(()))
            }
            AnyMessageOwned::Mining(MiningOwned::SubmitSharesSuccess(success)) => {
                trace!(channel_id = success.channel_id, "SubmitShares.Success");
                self.emit(ClientEvent::from(success)).await?;
                Ok(ControlFlow::Continue(()))
            }
            AnyMessageOwned::Mining(MiningOwned::SubmitSharesError(error)) => {
                trace!(
                    channel_id = error.channel_id,
                    seq = error.sequence_number,
                    reason = error.error_code.as_utf8_or_hex(),
                    "SubmitShares.Error"
                );
                self.emit(ClientEvent::from(error)).await?;
                Ok(ControlFlow::Continue(()))
            }
            AnyMessageOwned::Common(CommonMessagesOwned::Reconnect(msg)) => {
                let (host, port) = reconnect_endpoint(&self.config, &msg);
                info!(
                    host = %self.config.host,
                    new_host = %host,
                    new_port = port,
                    "Reconnect"
                );
                Ok(ControlFlow::Break(ClientOutcome::Reconnect { host, port }))
            }
            // The message resets the extension state of a channel whose
            // endpoint moved.  This client negotiates no extensions, so there
            // is nothing to reset and the channel carries on.
            AnyMessageOwned::Common(CommonMessagesOwned::ChannelEndpointChanged(msg)) => {
                info!(channel_id = msg.channel_id, "ChannelEndpointChanged");
                Ok(ControlFlow::Continue(()))
            }
            // Group membership is not tracked beyond the group_channel_id the
            // channel opened with, so a move to another group is only logged.
            AnyMessageOwned::Mining(MiningOwned::SetGroupChannel(msg)) => {
                info!(
                    group_channel_id = msg.group_channel_id,
                    channel_ids = ?msg.channel_ids,
                    "SetGroupChannel"
                );
                Ok(ControlFlow::Continue(()))
            }
            AnyMessageOwned::Mining(MiningOwned::CloseChannel(msg)) => {
                info!(channel_id = msg.channel_id, "CloseChannel");
                self.emit(ClientEvent::from(msg)).await?;
                Ok(ControlFlow::Break(ClientOutcome::ChannelClosed))
            }
            AnyMessageOwned::Mining(MiningOwned::UpdateChannelError(error)) => {
                warn!(
                    channel_id = error.channel_id,
                    reason = error.error_code.as_utf8_or_hex(),
                    "UpdateChannel.Error"
                );
                Ok(ControlFlow::Continue(()))
            }
            unexpected => {
                warn!(host = %self.config.host, ?unexpected, "Ignoring unexpected message");
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

impl TryFrom<OpenExtendedMiningChannelSuccessOwned> for ClientEvent {
    type Error = StratumV2Error;

    fn try_from(msg: OpenExtendedMiningChannelSuccessOwned) -> StratumV2Result<Self> {
        Ok(ClientEvent::OpenExtendedMiningChannelSuccess {
            channel_id: msg.channel_id,
            group_channel_id: msg.group_channel_id,
            target: Target::from_le_bytes(msg.target.into_array()),
            extranonce_prefix: ExtranoncePrefix::from_wire(msg.extranonce_prefix.into_bytes())?,
            extranonce_size: msg.extranonce_size,
        })
    }
}

impl From<SetTargetOwned> for ClientEvent {
    fn from(msg: SetTargetOwned) -> Self {
        ClientEvent::SetTarget {
            channel_id: msg.channel_id,
            target: Target::from_le_bytes(msg.maximum_target.into_array()),
        }
    }
}

impl TryFrom<SetExtranoncePrefixOwned> for ClientEvent {
    type Error = StratumV2Error;

    fn try_from(msg: SetExtranoncePrefixOwned) -> StratumV2Result<Self> {
        Ok(ClientEvent::SetExtranoncePrefix {
            channel_id: msg.channel_id,
            extranonce_prefix: ExtranoncePrefix::from_wire(msg.extranonce_prefix.into_bytes())?,
        })
    }
}

impl From<SubmitSharesSuccess> for ClientEvent {
    fn from(msg: SubmitSharesSuccess) -> Self {
        ClientEvent::SubmitSharesSuccess {
            channel_id: msg.channel_id,
            last_sequence_number: msg.last_sequence_number,
            new_submits_accepted_count: msg.new_submits_accepted_count,
        }
    }
}

impl From<SubmitSharesErrorOwned> for ClientEvent {
    fn from(msg: SubmitSharesErrorOwned) -> Self {
        ClientEvent::SubmitSharesError {
            channel_id: msg.channel_id,
            sequence_number: msg.sequence_number,
            reason: msg.error_code.as_utf8_or_hex(),
        }
    }
}

impl From<CloseChannelOwned> for ClientEvent {
    fn from(msg: CloseChannelOwned) -> Self {
        ClientEvent::CloseChannel {
            channel_id: msg.channel_id,
            reason: msg.reason_code.as_utf8_or_hex(),
        }
    }
}

async fn reader_task(
    mut read_half: NoiseTcpReadHalf,
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
                            warn!(error = %e, "Read error from pool");
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
fn extract_any_message(mut frame: SerializedFrame) -> StratumV2Result<Option<AnyMessageOwned>> {
    let header = frame.header();
    let extension_type = header.ext_type_without_channel_msg();
    if extension_type != 0 {
        debug!(
            extension_type = format!("{extension_type:#06x}"),
            msg_type = header.msg_type(),
            "Discarding message of an unknown extension"
        );
        return Ok(None);
    }
    AnyMessage::try_from((header, frame.payload()))
        .map(|m| Some(m.into_owned()))
        .map_err(|e| StratumV2Error::Protocol(format!("parse error: {e}")))
}

/// Resolves a `Reconnect` into the endpoint the next connection targets.
///
/// An empty `new_host` or a zero `new_port` stands for the endpoint in use.
fn reconnect_endpoint(config: &PoolConfig, msg: &ReconnectOwned) -> (String, u16) {
    let host = if msg.new_host.is_empty() {
        config.host.clone()
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

/// Encodes a share as `SubmitSharesExtended` with the given sequence number.
///
/// The extranonce must match the channel's negotiated `extranonce_size`; the
/// pool rebuilds the coinbase as `coinbase_tx_prefix + extranonce_prefix +
/// extranonce + coinbase_tx_suffix` (see [Mining Protocol >
/// SubmitSharesExtended][sv2-submit]).
///
/// # Errors
///
/// Returns [`StratumV2Error::Protocol`] if the extranonce is longer than
/// `B032` allows.  [`constants::MAX_EXTRANONCE_SIZE`] rejects such a
/// channel at open time, so this should not happen.
///
/// [sv2-submit]: https://github.com/stratum-mining/sv2-spec/blob/main/05-Mining-Protocol.md#5312-submitsharesextended-client---server
fn submit_message(
    share: ShareSubmission,
    sequence_number: u32,
) -> StratumV2Result<SubmitSharesExtendedOwned> {
    Ok(SubmitSharesExtendedOwned {
        channel_id: share.channel_id,
        sequence_number,
        job_id: share.job_id,
        nonce: share.nonce,
        ntime: share.ntime,
        version: share.version.to_consensus() as u32,
        extranonce: B032Owned::try_from(share.extranonce)?,
    })
}

async fn send_message(
    write_half: &mut NoiseTcpWriteHalf,
    msg: AnyMessageOwned,
) -> StratumV2Result<()> {
    // A pool that stops reading its socket would otherwise block the write,
    // and with it the client's event loop, forever.  A frame cut short by
    // the timeout is harmless, since the error ends the connection.
    const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

    let frame = MessageFrame::try_from(msg)
        .map_err(|e| StratumV2Error::Protocol(format!("frame encode failed: {e}")))?;
    tokio::time::timeout(WRITE_TIMEOUT, write_half.write_frame(frame))
        .await
        .map_err(|_| StratumV2Error::Timeout("the pool to read a frame".to_string()))?
        .map_err(|e| StratumV2Error::Protocol(format!("write_frame failed: {e}")))
}

async fn recv_one(rx: &mut mpsc::Receiver<ReaderMessage>) -> StratumV2Result<AnyMessageOwned> {
    match rx.recv().await {
        Some(ReaderMessage::Message(msg)) => Ok(msg),
        Some(ReaderMessage::Error(e)) => Err(e),
        Some(ReaderMessage::Done) | None => Err(StratumV2Error::Protocol(
            "reader terminated during negotiation".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config() -> PoolConfig {
        PoolConfig {
            host: "pool.example.com".to_string(),
            port: 3333,
            authority_key: "9bDuixKmZqAJnrmP746n8zU1wyAQRrus7th9dxnkPg6RzQvCnan"
                .parse()
                .unwrap(),
            user_identity: "worker".to_string(),
            vendor: "mujina".to_string(),
            hardware_version: "unknown".to_string(),
            firmware: "mujina-miner/test".to_string(),
            device_id: String::new(),
            nominal_hash_rate: HashRate::from_terahashes(1.0),
        }
    }

    fn make_reconnect(new_host: &str, new_port: u16) -> ReconnectOwned {
        ReconnectOwned {
            new_host: Str0255Owned::try_from(new_host.as_bytes()).unwrap(),
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

        let moved = config.with_endpoint("backup.example.com".to_string(), 4444);

        assert_eq!(moved.host, "backup.example.com");
        assert_eq!(moved.port, 4444);
        assert_eq!(
            format!("{:?}", moved.authority_key),
            format!("{:?}", config.authority_key)
        );
    }

    /// Contract: a text field too long for an SV2 `STR0_255` fails
    /// validation, naming the field.
    #[test]
    fn validate_rejects_an_oversized_field() {
        let config = PoolConfig {
            user_identity: "w".repeat(256),
            ..make_config()
        };

        let err = config.validate().unwrap_err();

        assert!(err.to_string().contains("user identity"), "got {err}");
    }

    /// Contract: an accepted channel open reaches the consumer as domain
    /// values, with the little-endian wire target decoded.
    #[test]
    fn channel_open_converts_to_domain_values() {
        let msg = OpenExtendedMiningChannelSuccessOwned {
            request_id: 1,
            channel_id: 3,
            target: U256Owned::from(Target::MAX.to_le_bytes()),
            extranonce_size: 8,
            extranonce_prefix: vec![0xde, 0xad].try_into().unwrap(),
            group_channel_id: 5,
        };

        let event = ClientEvent::try_from(msg).unwrap();

        let ClientEvent::OpenExtendedMiningChannelSuccess {
            channel_id,
            group_channel_id,
            target,
            extranonce_prefix,
            extranonce_size,
        } = event
        else {
            panic!("expected OpenExtendedMiningChannelSuccess, got {event:?}");
        };
        assert_eq!(channel_id, 3);
        assert_eq!(group_channel_id, 5);
        assert_eq!(target, Target::MAX);
        assert_eq!(extranonce_prefix.as_bytes(), [0xde, 0xad]);
        assert_eq!(extranonce_size, 8);
    }

    /// Contract: a share goes to the pool with every field it was submitted
    /// with and the sequence number the client gave it.
    #[test]
    fn share_encodes_as_submit_shares_extended() {
        let share = ShareSubmission {
            channel_id: 3,
            job_id: 7,
            nonce: 0xdead_beef,
            ntime: 1_700_000_000,
            version: Version::from_consensus(0x2000_e000),
            extranonce: vec![0x01, 0x02, 0x03, 0x04],
        };

        let submit = submit_message(share, 42).unwrap();

        assert_eq!(submit.channel_id, 3);
        assert_eq!(submit.sequence_number, 42);
        assert_eq!(submit.job_id, 7);
        assert_eq!(submit.nonce, 0xdead_beef);
        assert_eq!(submit.ntime, 1_700_000_000);
        assert_eq!(submit.version, 0x2000_e000);
        assert_eq!(submit.extranonce.as_ref(), [0x01, 0x02, 0x03, 0x04]);
    }

    /// Contract: a SetTarget reaches the consumer with its little-endian
    /// wire target decoded.
    #[test]
    fn set_target_converts_to_domain_values() {
        let msg = SetTargetOwned {
            channel_id: 3,
            maximum_target: U256Owned::from(Target::MAX.to_le_bytes()),
        };

        let event = ClientEvent::from(msg);

        let ClientEvent::SetTarget { channel_id, target } = event else {
            panic!("expected SetTarget, got {event:?}");
        };
        assert_eq!(channel_id, 3);
        assert_eq!(target, Target::MAX);
    }
}
