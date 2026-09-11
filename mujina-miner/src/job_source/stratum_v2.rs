//! Stratum V2 Extended Channel job source.
//!
//! Bridges [`StratumV2Client`] events into the scheduler's job-source
//! abstraction.  Converts `NewExtendedMiningJob`/`SetNewPrevHash` pairs into
//! [`JobTemplate`]s and manages the connection lifecycle with exponential
//! back-off.
//!
//! # Share Submission
//!
//! Shares received via [`SourceCommand::SubmitShare`] go to the client, which
//! numbers them within the connection and sends them to the pool as
//! `SubmitSharesExtended` messages.  Shares are not checked locally: the
//! scheduler drops late shares from work a `ReplaceJob` retired, and the pool
//! judges the rest.
//!
//! # Job State
//!
//! Job state is managed by an [`ExtendedChannel`] from `channels_sv2`.  Active
//! and past jobs (valid within the current chain tip) are tracked by the
//! channel, and future jobs wait there until a `SetNewPrevHash` activates one.

use std::fmt;

use anyhow::{Context as _, Result, bail};
use bitcoin::block::Version;
use bitcoin::hash_types::{BlockHash, TxMerkleNode};
use bitcoin::hashes::Hash as _;
use bitcoin::pow::CompactTarget;
use stratum_apps::key_utils::Secp256k1PublicKey;
use stratum_apps::stratum_core::channels_sv2::chain_tip::ChainTip;
use stratum_apps::stratum_core::channels_sv2::client::error::ExtendedChannelError;
use stratum_apps::stratum_core::channels_sv2::client::extended::{ExtendedChannel, ExtendedJob};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio_util::sync::CancellationToken;

use crate::stratum_v2::{
    ClientCommand, ClientEvent, ClientOutcome, PoolConfig, ShareSubmission, StratumV2Client,
};
use crate::tracing::prelude::*;
use crate::types::{Difficulty, HashRate};

use super::reconnect::{ConnectOutcome, Reconnect, wait_for_hash_rate};
use super::{
    ExtranonceRange, GeneralPurposeBits, JobTemplate, MerkleRootKind, MerkleRootTemplate, Share,
    SourceCommand, SourceEvent, VersionTemplate,
};

/// Stratum V2 Extended Channel job source.
///
/// Manages the connection lifecycle (DNS -> TCP -> Noise NX -> session
/// negotiation) with exponential back-off and translates pool events into
/// [`SourceEvent`]s for the scheduler.
pub struct StratumV2Source {
    config: SourceConfig,
    /// Latest hash rate the scheduler reported, announced to the pool.
    nominal_hash_rate: HashRate,
    event_tx: mpsc::Sender<SourceEvent>,
    command_rx: mpsc::Receiver<SourceCommand>,
    shutdown: CancellationToken,
    session: Option<SessionState>,
    /// Endpoint a pool's `Reconnect` named for the next connection attempt.
    reconnect_to: Option<(String, u16)>,
}

/// Settings a [`StratumV2Source`] is created with.
#[derive(Debug, Clone)]
pub struct SourceConfig {
    /// Pool host name or IP address.
    pub host: String,
    /// Pool TCP port.
    pub port: u16,
    /// Public key that authenticates the pool in the Noise_NX handshake.
    pub authority_key: Secp256k1PublicKey,
    /// Identity the channel is opened for, typically the worker name.
    pub user: String,
    /// Device vendor reported to the pool.
    pub vendor: String,
    /// Device hardware version reported to the pool.
    pub hardware_version: String,
    /// Device firmware reported to the pool.
    pub firmware: String,
    /// Device identifier reported to the pool.
    pub device_id: String,
}

/// Why [`StratumV2Source::serve_events`] stopped serving a connection.
enum ServeOutcome {
    /// The source is shutting down.
    Shutdown,
    /// The client task ended; its return value says why.
    ClientExited,
    /// The session cannot continue and the connection must be rebuilt.
    SessionFailed(anyhow::Error),
}

/// Per-connection channel state.
///
/// Populated on `OpenExtendedMiningChannelSuccess` and replaced on each
/// reconnect.
struct SessionState {
    channel_id: u32,
    group_channel_id: u32,
    /// The extranonces the miner rolls, filling the pool's `extranonce_size`.
    ///
    /// The slot excludes the pool-assigned `extranonce_prefix`.
    extranonce_range: ExtranonceRange,
    /// Extended channel state: job lifecycle, target, and extranonce prefix.
    channel: ExtendedChannel,
}

impl fmt::Display for StratumV2Source {
    /// Formats the source as `host:port`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.config.host, self.config.port)
    }
}

impl StratumV2Source {
    /// Creates a new Stratum V2 job source.
    pub fn new(
        config: SourceConfig,
        command_rx: mpsc::Receiver<SourceCommand>,
        event_tx: mpsc::Sender<SourceEvent>,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            config,
            nominal_hash_rate: HashRate::default(),
            event_tx,
            command_rx,
            shutdown,
            session: None,
            reconnect_to: None,
        }
    }

    /// Runs the source.
    ///
    /// Drains incoming commands until a positive hashrate is reported, then
    /// enters the connect loop: attempt connection, serve events, and reconnect
    /// with exponential back-off on disconnect.
    ///
    /// # Errors
    ///
    /// Returns an error at once if a configured text field does not fit the
    /// SV2 wire format, and later on a pool error that reconnecting cannot
    /// fix.
    pub async fn run(mut self) -> Result<()> {
        self.client_config()
            .validate()
            .context("Invalid Stratum V2 source config")?;

        info!(
            host = %self.config.host,
            port = self.config.port,
            "Waiting for hash threads before connecting"
        );

        let Some(rate) = wait_for_hash_rate(&mut self.command_rx, &self.shutdown).await else {
            return Ok(());
        };
        self.nominal_hash_rate = rate;

        let mut reconnect = Reconnect::new();

        loop {
            self.session = None;

            // A Reconnect names the server for the next attempt only.  Later
            // attempts go back to the configured pool, which can redirect
            // again, so a server that stops answering cannot strand the miner.
            let config = match self.reconnect_to.take() {
                Some((host, port)) => self.client_config().with_endpoint(host, port),
                None => self.client_config(),
            };

            info!(host = %config.host, port = config.port, "Connecting to pool");

            reconnect.attempt();
            match self.connect_and_run(config).await {
                ConnectOutcome::Shutdown => return Ok(()),
                ConnectOutcome::Fatal(e) => {
                    error!(error = %e, "Fatal pool error, not reconnecting");
                    return Err(e);
                }
                ConnectOutcome::Disconnected => {
                    if reconnect
                        .wait_after_disconnect(
                            &self.event_tx,
                            &mut self.command_rx,
                            &self.shutdown,
                            &mut self.nominal_hash_rate,
                        )
                        .await
                    {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Runs a single connection attempt through its full lifecycle.
    ///
    /// Spawns the [`StratumV2Client`] task against `config`, runs the event
    /// loop until the client exits (or shutdown is requested), and returns the
    /// outcome.
    async fn connect_and_run(&mut self, config: PoolConfig) -> ConnectOutcome {
        let (client_event_tx, mut client_event_rx) = mpsc::channel::<ClientEvent>(100);
        let (client_command_tx, client_command_rx) = mpsc::channel::<ClientCommand>(100);

        let client = StratumV2Client::new(
            config,
            client_event_tx,
            client_command_rx,
            self.shutdown.child_token(),
        );

        let client_handle = tokio::spawn(async move { client.run().await });

        let serve = self
            .serve_events(&mut client_event_rx, &client_command_tx)
            .await;

        match serve {
            ServeOutcome::Shutdown => return ConnectOutcome::Shutdown,
            ServeOutcome::SessionFailed(e) => {
                error!(error = %e, "Session error; reconnecting");
                client_handle.abort();
                let _ = client_handle.await;
                return ConnectOutcome::Disconnected;
            }
            // The client ended on its own; its return value says why.
            ServeOutcome::ClientExited => {}
        }

        match client_handle.await {
            Ok(Ok(ClientOutcome::Shutdown)) => ConnectOutcome::Shutdown,
            Ok(Ok(ClientOutcome::ConnectionClosed | ClientOutcome::ChannelClosed)) => {
                ConnectOutcome::Disconnected
            }
            Ok(Ok(ClientOutcome::Reconnect { host, port })) => {
                self.reconnect_to = Some((host, port));
                ConnectOutcome::Disconnected
            }
            Ok(Err(e)) => {
                if e.is_fatal() {
                    ConnectOutcome::Fatal(e.into())
                } else {
                    warn!(error = %e, "Disconnected from pool");
                    ConnectOutcome::Disconnected
                }
            }
            Err(join_err) => {
                ConnectOutcome::Fatal(anyhow::anyhow!("Client task panicked: {}", join_err))
            }
        }
    }

    /// Serves one connection's events until it ends.
    ///
    /// Split out from [`StratumV2Source::connect_and_run`] so the loop can be
    /// driven directly in tests, without a pool or a client task.
    async fn serve_events(
        &mut self,
        client_event_rx: &mut mpsc::Receiver<ClientEvent>,
        client_command_tx: &mpsc::Sender<ClientCommand>,
    ) -> ServeOutcome {
        loop {
            tokio::select! {
                event_opt = client_event_rx.recv() => {
                    match event_opt {
                        Some(event) => {
                            if let Err(e) = self.handle_client_event(event).await {
                                return ServeOutcome::SessionFailed(e);
                            }
                        }
                        None => return ServeOutcome::ClientExited,
                    }
                }

                Some(cmd) = self.command_rx.recv() => {
                    match cmd {
                        SourceCommand::SubmitShare(share) => {
                            self.handle_share_submission(share, client_command_tx);
                        }
                        SourceCommand::UpdateHashRate(rate) => {
                            self.nominal_hash_rate = rate;
                            self.report_hash_rate(client_command_tx);
                        }
                    }
                }

                _ = self.shutdown.cancelled() => return ServeOutcome::Shutdown,
            }
        }
    }

    /// Dispatches a single event from the [`StratumV2Client`].
    async fn handle_client_event(&mut self, event: ClientEvent) -> Result<()> {
        match event {
            ClientEvent::SetupConnectionSuccess { used_version, .. } => {
                debug!(version = used_version, "SetupConnection accepted");
            }

            ClientEvent::OpenExtendedMiningChannelSuccess {
                channel_id,
                group_channel_id,
                target,
                extranonce_prefix,
                extranonce_size,
            } => {
                let extranonce_range = ExtranonceRange::full(extranonce_size.into())
                    .context("OpenExtendedMiningChannelSuccess: invalid extranonce size")?;

                info!(
                    host = %self.config.host,
                    channel_id,
                    extranonce_prefix = %hex::encode(extranonce_prefix.as_bytes()),
                    extranonce_size,
                    "Extended channel opened"
                );

                // version_rolling_allowed is set per-job by the pool on NewExtendedMiningJob.
                // The channel-level flag here is used by validate_share() to enforce BIP320
                // compliance. Set true because Mujina performs version rolling in software.
                let channel = ExtendedChannel::new(
                    channel_id,
                    self.config.user.clone(),
                    extranonce_prefix,
                    target,
                    self.nominal_hash_rate.0 as f32,
                    true,
                    extranonce_size,
                    None,
                )
                .map_err(|e| anyhow::anyhow!("OpenExtendedMiningChannelSuccess: {e:?}"))?;

                self.session = Some(SessionState {
                    channel_id,
                    group_channel_id,
                    extranonce_range,
                    channel,
                });
            }

            ClientEvent::SetTarget { channel_id, target } => {
                if let Some(session) = &mut self.session {
                    if !session.accepts_channel_id(channel_id) {
                        warn!(
                            expected = session.channel_id,
                            group = session.group_channel_id,
                            got = channel_id,
                            "SetTarget channel_id mismatch; ignoring"
                        );
                        return Ok(());
                    }
                    debug!(
                        channel_id,
                        difficulty = %Difficulty::from_target(target),
                        "SetTarget"
                    );
                    session
                        .channel
                        .set_target(target)
                        .map_err(|e| anyhow::anyhow!("SetTarget: {e:?}"))?;
                }
            }

            ClientEvent::NewExtendedMiningJob(job) => {
                let job_id = job.job_id;
                let is_future = job.is_future();
                let Some(session) = &mut self.session else {
                    warn!(job_id, "Job arrived before channel opened; dropping");
                    return Ok(());
                };
                if !session.accepts_channel_id(job.channel_id) {
                    warn!(
                        expected = session.channel_id,
                        group = session.group_channel_id,
                        got = job.channel_id,
                        job_id,
                        "NewExtendedMiningJob channel_id mismatch; ignoring"
                    );
                    return Ok(());
                }
                if is_future {
                    debug!(job_id, "Buffering future job");
                }
                session
                    .channel
                    .on_new_extended_mining_job(job)
                    .map_err(|e| anyhow::anyhow!("NewExtendedMiningJob: {e:?}"))?;
                if !is_future {
                    let Some(chain_tip) = session.channel.get_chain_tip() else {
                        warn!(
                            job_id,
                            "Non-future job arrived before SetNewPrevHash; dropping"
                        );
                        return Ok(());
                    };
                    let active = session.channel.get_active_job().ok_or_else(|| {
                        anyhow::anyhow!(
                            "NewExtendedMiningJob {job_id}: channel reports no active job \
                             after activating a non-future job"
                        )
                    })?;
                    let template =
                        Self::build_job_template(active, chain_tip, session.extranonce_range)?;
                    debug!(job_id, "Emitting UpdateJob");
                    self.event_tx.send(SourceEvent::UpdateJob(template)).await?;
                }
            }

            ClientEvent::SetNewPrevHash(msg) => {
                let Some(session) = &mut self.session else {
                    warn!("SetNewPrevHash arrived before channel opened; dropping");
                    return Ok(());
                };

                if !session.accepts_channel_id(msg.channel_id) {
                    warn!(
                        expected = session.channel_id,
                        group = session.group_channel_id,
                        got = msg.channel_id,
                        "SetNewPrevHash channel_id mismatch; ignoring"
                    );
                    return Ok(());
                }

                let prev_hash_hex = hex::encode(msg.prev_hash.as_bytes());
                let job_id = msg.job_id;
                debug!(
                    job_id,
                    prev_hash = %prev_hash_hex,
                    nbits = format!("{:#010x}", msg.nbits),
                    "SetNewPrevHash"
                );

                match session.channel.on_set_new_prev_hash(msg) {
                    Ok(()) => {
                        let chain_tip = session.channel.get_chain_tip().ok_or_else(|| {
                            anyhow::anyhow!(
                                "SetNewPrevHash {job_id}: channel reports no chain tip after \
                                 accepting the new prev hash"
                            )
                        })?;
                        let active = session.channel.get_active_job().ok_or_else(|| {
                            anyhow::anyhow!(
                                "SetNewPrevHash {job_id}: channel reports no active job after \
                                 accepting the new prev hash"
                            )
                        })?;
                        let template =
                            Self::build_job_template(active, chain_tip, session.extranonce_range)?;
                        debug!(job_id, "Emitting ReplaceJob (future -> active)");
                        self.event_tx
                            .send(SourceEvent::ReplaceJob(template))
                            .await?;
                    }
                    Err(ExtendedChannelError::JobIdNotFound) => {
                        // The channel rejects the prev hash without recording it,
                        // so its chain tip still describes the previous block.
                        // Every non-future job that followed would be built on
                        // that stale tip and rejected by the pool, with nothing
                        // in the logs to say why.  Fail the session instead and
                        // rebuild the channel from the pool's current state.
                        bail!(
                            "SetNewPrevHash {job_id}: no buffered future job for this \
                             prev hash, leaving the channel chain tip stale"
                        );
                    }
                    Err(e) => bail!("SetNewPrevHash: {e:?}"),
                }
            }

            // The client task exits with ChannelClosed after emitting this
            // event; no local flag needed.
            ClientEvent::CloseChannel { channel_id, reason } => {
                info!(channel_id, reason, "Channel closed by pool; reconnecting");
            }

            ClientEvent::SubmitSharesSuccess {
                channel_id,
                last_sequence_number,
                new_submits_accepted_count,
            } => {
                if let Some(session) = &mut self.session {
                    if !session.accepts_channel_id(channel_id) {
                        warn!(
                            expected = session.channel_id,
                            group = session.group_channel_id,
                            got = channel_id,
                            "SubmitSharesSuccess channel_id mismatch; ignoring"
                        );
                        return Ok(());
                    }
                    debug!(
                        channel_id,
                        last_seq = last_sequence_number,
                        accepted = new_submits_accepted_count,
                        "Pool accepted shares"
                    );
                }
            }

            ClientEvent::SubmitSharesError {
                channel_id,
                sequence_number,
                reason,
            } => {
                if let Some(session) = &mut self.session {
                    if !session.accepts_channel_id(channel_id) {
                        warn!(
                            expected = session.channel_id,
                            group = session.group_channel_id,
                            got = channel_id,
                            "SubmitSharesError channel_id mismatch; ignoring"
                        );
                        return Ok(());
                    }
                    warn!(
                        channel_id,
                        seq = sequence_number,
                        reason,
                        "Share rejected by pool"
                    );
                }
            }
        }

        Ok(())
    }

    /// Forwards a share to the client for submission to the pool.
    ///
    /// Drops the share (with a trace or warn log) if:
    /// - There is no active session (not yet connected or reconnecting).
    /// - The job ID is not a valid `u32` (wrong protocol).
    /// - The extranonce is missing from the share.
    /// - The client has fallen behind.  The source never waits on the
    ///   client, so a stuck connection cannot stop it reading the
    ///   scheduler's commands.
    fn handle_share_submission(&self, share: Share, client_cmd_tx: &mpsc::Sender<ClientCommand>) {
        let Some(session) = &self.session else {
            trace!(job_id = %share.job_id, "Share dropped: no active session");
            return;
        };

        let Ok(job_id) = share.job_id.parse::<u32>() else {
            warn!(
                job_id = %share.job_id,
                "SV2 share has non-u32 job_id (wrong protocol?); dropping"
            );
            return;
        };

        let Some(extranonce) = share.extranonce else {
            warn!(job_id, "SV2 share missing extranonce; dropping");
            return;
        };

        // VersionTemplate enforces that base_version has bits 13-28 clear; hardware
        // only modifies those bits, so share.version is already the exact version
        // used in the block header.
        let submission = ShareSubmission {
            channel_id: session.channel_id,
            job_id,
            nonce: share.nonce,
            ntime: share.time,
            version: share.version,
            extranonce: extranonce.into(),
        };

        match client_cmd_tx.try_send(ClientCommand::SubmitShare(submission)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                warn!(job_id, "Client is behind; dropping share");
            }
            Err(TrySendError::Closed(_)) => {
                warn!(
                    job_id,
                    "Client disconnected before share could be forwarded"
                );
            }
        }
    }

    /// Converts a [`NewExtendedMiningJob`] into a [`JobTemplate`].
    ///
    /// The SV2 field mapping is:
    ///
    /// | SV2 field              | `MerkleRootTemplate` field |
    /// |------------------------|----------------------------|
    /// | `coinbase_tx_prefix`   | `coinbase_prefix`          |
    /// | `extranonce_prefix`    | `extranonce_prefix`        |
    /// | `coinbase_tx_suffix`   | `coinbase_suffix`          |
    /// | `merkle_path` entries  | `merkle_branches`          |
    fn build_job_template(
        job: &ExtendedJob,
        chain_tip: &ChainTip,
        extranonce_range: ExtranonceRange,
    ) -> Result<JobTemplate> {
        let ExtendedJob {
            job_message: msg,
            extranonce_prefix,
            target,
        } = job;

        // Version template.
        //
        // GP bits (13-28) are defined by BIP320 (https://github.com/bitcoin/bips/blob/master/bip-0320.mediawiki)
        // for general-purpose version rolling.
        //
        // When version_rolling_allowed=true, the pool sets them to zero and
        // the miner fills them, which maps to GeneralPurposeBits::full().
        //
        // When version_rolling_allowed=false, the version is fixed.
        // VersionTemplate requires bits 13-28 clear in the base, so strip
        // them.  Compliant SV2 pools set them to zero in this case; if not,
        // we warn and strip.
        // BIP320's general-purpose bits 13-28 of the version field.
        const GP_BITS_MASK: u32 = 0x1fffe000;

        let raw_version = msg.version;
        let gp_bits_mask = if msg.version_rolling_allowed {
            GeneralPurposeBits::full()
        } else {
            if raw_version & GP_BITS_MASK != 0 {
                warn!(
                    job_id = msg.job_id,
                    version = format!("{:#010x}", raw_version),
                    "Non-rolling SV2 job has GP bits set; stripping them"
                );
            }
            GeneralPurposeBits::none()
        };
        let base_version = Version::from_consensus((raw_version & !GP_BITS_MASK) as i32);
        let version_template = VersionTemplate::new(base_version, gp_bits_mask)
            .map_err(|e| anyhow::anyhow!("VersionTemplate: {e}"))?;

        let merkle_branches: Vec<TxMerkleNode> = msg
            .merkle_path
            .iter()
            .map(|node| TxMerkleNode::from_byte_array(node.to_array()))
            .collect();

        Ok(JobTemplate {
            id: msg.job_id.to_string(),
            prev_blockhash: BlockHash::from_byte_array(chain_tip.prev_hash().into_array()),
            version: version_template,
            bits: CompactTarget::from_consensus(chain_tip.nbits()),
            share_target: *target,
            // For immediate (non-future) jobs, min_ntime is set in the job message.
            // For future jobs activated by SetNewPrevHash, min_ntime is None in the
            // job and falls back to the value from the chain tip.
            time: msg
                .min_ntime
                .as_ref()
                .copied()
                .unwrap_or_else(|| chain_tip.min_ntime()),
            merkle_root: MerkleRootKind::Computed(MerkleRootTemplate {
                coinbase_prefix: msg.coinbase_tx_prefix.to_owned_bytes(),
                extranonce_prefix: extranonce_prefix.clone(),
                extranonce_range,
                coinbase_suffix: msg.coinbase_tx_suffix.to_owned_bytes(),
                merkle_branches,
            }),
        })
    }

    /// Tells the pool the channel's current nominal hash rate.
    ///
    /// Pools size the channel's target from this figure, so a value left at
    /// the rate observed when the channel opened costs share quality for the
    /// life of the connection.  Does nothing before a channel exists; the
    /// opening `OpenExtendedMiningChannel` carries the rate instead.
    ///
    /// Like shares, the update is dropped rather than waited on when the
    /// client has fallen behind; the source keeps the rate, so the next
    /// update or channel open carries it.
    fn report_hash_rate(&self, client_command_tx: &mpsc::Sender<ClientCommand>) {
        let Some(session) = &self.session else {
            return;
        };

        let update = ClientCommand::UpdateChannel {
            channel_id: session.channel_id,
            nominal_hash_rate: self.nominal_hash_rate,
        };

        if let Err(e) = client_command_tx.try_send(update) {
            warn!(error = %e, "Failed to queue UpdateChannel");
        }
    }

    /// Builds the client's configuration for the configured pool, carrying
    /// the current hash rate.
    fn client_config(&self) -> PoolConfig {
        PoolConfig {
            host: self.config.host.clone(),
            port: self.config.port,
            authority_key: self.config.authority_key,
            user_identity: self.config.user.clone(),
            vendor: self.config.vendor.clone(),
            hardware_version: self.config.hardware_version.clone(),
            firmware: self.config.firmware.clone(),
            device_id: self.config.device_id.clone(),
            nominal_hash_rate: self.nominal_hash_rate,
        }
    }
}

impl SessionState {
    /// Returns `true` if `channel_id` targets this session.
    ///
    /// The `channel_id` field in [Mining Protocol > `NewExtendedMiningJob`][sv2-job] and
    /// [Mining Protocol > `SetNewPrevHash`][sv2-prevhash] may address either the
    /// individual channel or the group channel. Accept both; reject only messages
    /// clearly meant for a different session.
    ///
    /// [sv2-job]: https://github.com/stratum-mining/sv2-spec/blob/main/05-Mining-Protocol.md#5316-newextendedminingjob-server---client
    /// [sv2-prevhash]: https://github.com/stratum-mining/sv2-spec/blob/main/05-Mining-Protocol.md#5317-setnewprevhash-server---client-broadcast
    fn accepts_channel_id(&self, channel_id: u32) -> bool {
        channel_id == self.channel_id || channel_id == self.group_channel_id
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use bitcoin::pow::Target;
    use stratum_apps::stratum_core::{
        binary_sv2::{B064KOwned, Seq0255Owned, Sv2OptionOwned, U256Owned},
        channels_sv2::extranonce_manager::ExtranoncePrefix,
        mining_sv2::{NewExtendedMiningJobOwned, SetNewPrevHashOwned},
    };

    use super::*;

    /// Builds a source wired to loose channels, with no client task behind
    /// them, so `serve_events` can be driven directly.
    fn make_source() -> (
        StratumV2Source,
        mpsc::Sender<SourceCommand>,
        mpsc::Receiver<SourceEvent>,
        CancellationToken,
    ) {
        let config = SourceConfig {
            host: "pool.example.com".to_string(),
            port: 3333,
            authority_key: "9bDuixKmZqAJnrmP746n8zU1wyAQRrus7th9dxnkPg6RzQvCnan"
                .parse()
                .unwrap(),
            user: "worker".to_string(),
            vendor: "mujina".to_string(),
            hardware_version: "unknown".to_string(),
            firmware: "mujina-miner/test".to_string(),
            device_id: String::new(),
        };

        let (command_tx, command_rx) = mpsc::channel(10);
        let (event_tx, event_rx) = mpsc::channel(10);
        let shutdown = CancellationToken::new();
        let mut source = StratumV2Source::new(config, command_rx, event_tx, shutdown.clone());
        source.nominal_hash_rate = HashRate::from_terahashes(1.0);
        (source, command_tx, event_rx, shutdown)
    }

    /// Contract: a configured value the SV2 wire format cannot carry stops
    /// the source when it starts, before it waits for a hash rate.
    #[tokio::test]
    async fn oversized_config_fails_at_startup() {
        let (mut source, _command_tx, _event_rx, _shutdown) = make_source();
        source.config.user = "w".repeat(256);

        let err = source.run().await.unwrap_err();

        assert!(format!("{err:#}").contains("user identity"), "got {err:#}");
    }

    /// Contract: an event that fails the session ends the loop with
    /// SessionFailed rather than being logged and ignored, so the caller
    /// rebuilds the channel instead of idling on a live socket.
    #[tokio::test(start_paused = true)]
    async fn session_error_ends_the_loop() {
        let (mut source, _command_tx, _event_rx, _shutdown) = make_source();
        let (client_event_tx, mut client_event_rx) = mpsc::channel::<ClientEvent>(10);
        let (client_command_tx, _client_command_rx) = mpsc::channel::<ClientCommand>(10);

        // A zero extranonce allocation cannot be used, so channel setup
        // fails.  Swallowing that error would leave the source connected
        // but permanently idle.
        client_event_tx
            .send(channel_open_with_extranonce_size(0))
            .await
            .unwrap();

        let outcome = source
            .serve_events(&mut client_event_rx, &client_command_tx)
            .await;

        assert!(
            matches!(outcome, ServeOutcome::SessionFailed(_)),
            "expected SessionFailed, got a different outcome"
        );
    }

    /// Contract: when the client task drops its event sender, the loop reports
    /// ClientExited so the caller can read the task's own result.
    #[tokio::test(start_paused = true)]
    async fn client_exit_ends_the_loop() {
        let (mut source, _command_tx, _event_rx, _shutdown) = make_source();
        let (client_event_tx, mut client_event_rx) = mpsc::channel::<ClientEvent>(10);
        let (client_command_tx, _client_command_rx) = mpsc::channel::<ClientCommand>(10);

        drop(client_event_tx);

        let outcome = source
            .serve_events(&mut client_event_rx, &client_command_tx)
            .await;

        assert!(matches!(outcome, ServeOutcome::ClientExited));
    }

    /// Contract: a cancelled shutdown token stops the loop with Shutdown.
    #[tokio::test(start_paused = true)]
    async fn shutdown_ends_the_loop() {
        let (mut source, _command_tx, _event_rx, shutdown) = make_source();
        let (_client_event_tx, mut client_event_rx) = mpsc::channel::<ClientEvent>(10);
        let (client_command_tx, _client_command_rx) = mpsc::channel::<ClientCommand>(10);

        shutdown.cancel();

        let outcome = source
            .serve_events(&mut client_event_rx, &client_command_tx)
            .await;

        assert!(matches!(outcome, ServeOutcome::Shutdown));
    }

    /// Builds an `OpenExtendedMiningChannelSuccess` event carrying the given
    /// extranonce allocation.
    fn channel_open_with_extranonce_size(extranonce_size: u16) -> ClientEvent {
        ClientEvent::OpenExtendedMiningChannelSuccess {
            channel_id: 1,
            group_channel_id: 0,
            target: Target::MAX,
            extranonce_prefix: ExtranoncePrefix::from_wire(vec![0xde, 0xad]).unwrap(),
            extranonce_size,
        }
    }

    /// Contract: a job whose coinbase prefix is too short to carry a BIP141
    /// marker fails the session instead of panicking the source task.
    #[tokio::test]
    async fn short_coinbase_prefix_is_rejected_not_panicked() {
        let (mut source, _command_tx, _event_rx, _shutdown) = make_source();
        source.session = Some(make_session());

        let err = source
            .handle_client_event(ClientEvent::NewExtendedMiningJob(make_job(
                1,
                0x2000_0000,
                false,
                None,
                vec![0x01; 4], // one byte short of the marker offset
                vec![],
                vec![],
            )))
            .await
            .expect_err("a short coinbase prefix must fail the session");

        assert!(
            err.to_string().contains("NewExtendedMiningJob"),
            "error should name the rejected message, got: {err}"
        );
    }

    /// Contract: a SetNewPrevHash moves work to a new block, so the job it
    /// activates replaces earlier work; a non-future job builds on the same
    /// block, so it leaves late shares from earlier jobs valid.
    #[tokio::test]
    async fn job_lifecycle_replaces_on_prev_hash_and_updates_on_same_block() {
        let (mut source, _command_tx, mut event_rx, _shutdown) = make_source();
        source.session = Some(make_session());

        source
            .handle_client_event(ClientEvent::NewExtendedMiningJob(make_job(
                7,
                0x2000_0000,
                false,
                None, // future
                vec![0x01; 8],
                vec![0x02; 8],
                vec![],
            )))
            .await
            .expect("future job is handled");
        assert!(
            event_rx.try_recv().is_err(),
            "a future job waits for its SetNewPrevHash"
        );

        source
            .handle_client_event(ClientEvent::SetNewPrevHash(SetNewPrevHashOwned {
                channel_id: 1,
                job_id: 7,
                prev_hash: U256Owned::from([0u8; 32]),
                min_ntime: 0,
                nbits: 0x1d00_ffff,
            }))
            .await
            .expect("prev hash is handled");
        let event = event_rx.try_recv().expect("a job was emitted");
        assert!(
            matches!(event, SourceEvent::ReplaceJob(ref t) if t.id == "7"),
            "expected ReplaceJob(7), got {event:?}"
        );

        source
            .handle_client_event(ClientEvent::NewExtendedMiningJob(make_job(
                8,
                0x2000_0000,
                false,
                Some(0),
                vec![0x01; 8],
                vec![0x02; 8],
                vec![],
            )))
            .await
            .expect("non-future job is handled");
        let event = event_rx.try_recv().expect("a job was emitted");
        assert!(
            matches!(event, SourceEvent::UpdateJob(ref t) if t.id == "8"),
            "expected UpdateJob(8), got {event:?}"
        );
    }

    fn make_session() -> SessionState {
        let extranonce_prefix = ExtranoncePrefix::from_wire(vec![0xde, 0xad]).unwrap();
        let channel = ExtendedChannel::new(
            1,
            "test".to_string(),
            extranonce_prefix,
            Target::MAX,
            1.0,
            false,
            4,
            None,
        )
        .expect("valid channel parameters");
        SessionState {
            channel_id: 1,
            group_channel_id: 0,
            extranonce_range: ExtranonceRange::full(4).unwrap(),
            channel,
        }
    }

    // ---- build_job_template ----

    /// Constructs a `NewExtendedMiningJobOwned` with the given parameters.
    fn make_job(
        job_id: u32,
        version: u32,
        version_rolling_allowed: bool,
        ntime: Option<u32>, // None -> future job
        coinbase_prefix: Vec<u8>,
        coinbase_suffix: Vec<u8>,
        merkle_hashes: Vec<[u8; 32]>,
    ) -> NewExtendedMiningJobOwned {
        let coinbase_tx_prefix =
            B064KOwned::try_from(coinbase_prefix).expect("valid coinbase prefix");
        let coinbase_tx_suffix =
            B064KOwned::try_from(coinbase_suffix).expect("valid coinbase suffix");
        let merkle_path =
            Seq0255Owned::new(merkle_hashes.into_iter().map(U256Owned::from).collect())
                .expect("at most 255 items");
        NewExtendedMiningJobOwned {
            channel_id: 1,
            job_id,
            min_ntime: Sv2OptionOwned::new(ntime),
            version,
            version_rolling_allowed,
            merkle_path,
            coinbase_tx_prefix,
            coinbase_tx_suffix,
        }
    }

    /// Constructs a [`ChainTip`] with a zeroed prev_hash.
    ///
    /// `min_ntime` and `nbits` are taken from the arguments.
    fn make_chain_tip(min_ntime: u32, nbits: u32) -> ChainTip {
        ChainTip::new([0u8; 32].into(), nbits, min_ntime)
    }

    /// Wraps `job` as the channel tracks it, with extranonce prefix
    /// `de ad` and the easiest share target.
    fn make_extended_job(job: NewExtendedMiningJobOwned) -> ExtendedJob {
        ExtendedJob {
            job_message: job,
            extranonce_prefix: vec![0xde, 0xad],
            target: Target::MAX,
        }
    }

    /// Contract: coinbase_tx_prefix -> coinbase_prefix, extranonce_prefix ->
    /// extranonce_prefix, coinbase_tx_suffix -> coinbase_suffix, merkle_path ->
    /// merkle_branches.  job_id, prev_hash, nbits, and share_target are also
    /// forwarded without modification.
    #[test]
    fn build_job_template_maps_all_fields() {
        let prefix = vec![0xAA, 0xBB, 0xCC];
        let suffix = vec![0xDD, 0xEE, 0xFF];
        let job = make_job(
            42,
            0x2000_0000,
            false,
            Some(0x0102_0304), // immediate job
            prefix.clone(),
            suffix.clone(),
            vec![],
        );
        let chain_tip = make_chain_tip(0x0102_0304, 0x1d00_ffff);
        let tmpl = StratumV2Source::build_job_template(
            &make_extended_job(job),
            &chain_tip,
            ExtranonceRange::full(4).unwrap(),
        )
        .unwrap();

        assert_eq!(tmpl.id, "42");
        assert_eq!(tmpl.prev_blockhash, BlockHash::all_zeros());
        assert_eq!(tmpl.bits, CompactTarget::from_consensus(0x1d00_ffff));
        assert_eq!(tmpl.share_target, Target::MAX);

        let MerkleRootKind::Computed(mrt) = tmpl.merkle_root else {
            panic!("expected MerkleRootKind::Computed");
        };
        assert_eq!(mrt.coinbase_prefix, prefix);
        assert_eq!(mrt.extranonce_prefix, vec![0xde, 0xad]);
        assert_eq!(mrt.coinbase_suffix, suffix);
        assert!(mrt.merkle_branches.is_empty());
    }

    /// Contract: for an immediate job, `template.time` comes from the job's
    /// own `min_ntime`.  For a future job, it falls back to
    /// `chain_tip.min_ntime()`.
    #[test]
    fn build_job_template_ntime_from_job_or_prevhash() {
        // Immediate job: job's ntime wins.
        let job_ntime = 0xAABB_CCDD;
        let prevhash_ntime = 0x1111_2222;
        let immediate = make_job(
            1,
            0x2000_0000,
            false,
            Some(job_ntime),
            vec![],
            vec![],
            vec![],
        );
        let chain_tip = make_chain_tip(prevhash_ntime, 0x1d00_ffff);
        let tmpl = StratumV2Source::build_job_template(
            &make_extended_job(immediate),
            &chain_tip,
            ExtranonceRange::full(4).unwrap(),
        )
        .unwrap();
        assert_eq!(
            tmpl.time, job_ntime,
            "immediate job must use its own min_ntime"
        );

        // Future job: chain tip ntime is the fallback.
        let future = make_job(2, 0x2000_0000, false, None, vec![], vec![], vec![]);
        let tmpl = StratumV2Source::build_job_template(
            &make_extended_job(future),
            &chain_tip,
            ExtranonceRange::full(4).unwrap(),
        )
        .unwrap();
        assert_eq!(
            tmpl.time, prevhash_ntime,
            "future job must fall back to chain_tip.min_ntime()"
        );
    }

    /// Contract: when `version_rolling_allowed = false`, the GP-bits mask in
    /// the resulting [`VersionTemplate`] is `GeneralPurposeBits::none()`.
    /// When `version_rolling_allowed = true`, the mask is
    /// `GeneralPurposeBits::full()`.
    #[test]
    fn build_job_template_version_rolling_flag() {
        let chain_tip = make_chain_tip(0, 0x1d00_ffff);

        let fixed = make_job(1, 0x2000_0000, false, Some(0), vec![], vec![], vec![]);
        let tmpl = StratumV2Source::build_job_template(
            &make_extended_job(fixed),
            &chain_tip,
            ExtranonceRange::full(4).unwrap(),
        )
        .unwrap();
        assert_eq!(
            tmpl.version.gp_bits_mask(),
            GeneralPurposeBits::none(),
            "fixed-version job must have no GP bits"
        );

        let rolling = make_job(2, 0x2000_0000, true, Some(0), vec![], vec![], vec![]);
        let tmpl = StratumV2Source::build_job_template(
            &make_extended_job(rolling),
            &chain_tip,
            ExtranonceRange::full(4).unwrap(),
        )
        .unwrap();
        assert_eq!(
            tmpl.version.gp_bits_mask(),
            GeneralPurposeBits::full(),
            "version-rolling job must expose all GP bits"
        );
    }

    /// Contract: each 32-byte entry in `merkle_path` becomes one
    /// [`TxMerkleNode`] in `merkle_branches`, preserving order.
    #[test]
    fn build_job_template_merkle_path_forwarded() {
        let hash_a = [0xABu8; 32];
        let hash_b = [0x12u8; 32];
        let job = make_job(
            1,
            0x2000_0000,
            false,
            Some(0),
            vec![],
            vec![],
            vec![hash_a, hash_b],
        );
        let chain_tip = make_chain_tip(0, 0x1d00_ffff);
        let tmpl = StratumV2Source::build_job_template(
            &make_extended_job(job),
            &chain_tip,
            ExtranonceRange::full(4).unwrap(),
        )
        .unwrap();
        let MerkleRootKind::Computed(mrt) = tmpl.merkle_root else {
            panic!("expected MerkleRootKind::Computed");
        };
        assert_eq!(mrt.merkle_branches.len(), 2);
        assert_eq!(
            mrt.merkle_branches[0],
            TxMerkleNode::from_byte_array(hash_a)
        );
        assert_eq!(
            mrt.merkle_branches[1],
            TxMerkleNode::from_byte_array(hash_b)
        );
    }

    // ---- accepts_channel_id ----

    /// Contract: a session accepts messages addressed to its individual
    /// channel_id, its group_channel_id, and rejects anything else.
    #[test]
    fn session_accepts_individual_and_group_channel_id() {
        let session = SessionState {
            channel_id: 2,
            group_channel_id: 1,
            ..make_session()
        };

        assert!(
            session.accepts_channel_id(2),
            "must accept individual channel_id"
        );
        assert!(
            session.accepts_channel_id(1),
            "must accept group_channel_id"
        );
        assert!(!session.accepts_channel_id(3), "must reject unrelated id");
        assert!(!session.accepts_channel_id(0), "must reject zero id");
    }

    // ---- extranonce slot width ----

    /// Contract: the template's extranonce range fills the pool's whole
    /// extranonce_size, including allocations wider than 8 bytes.
    #[test]
    fn build_job_template_range_fills_the_pool_allocation() {
        let chain_tip = make_chain_tip(0, 0x1d00_ffff);
        let job = make_job(1, 0x2000_0000, false, Some(0), vec![], vec![], vec![]);
        let tmpl = StratumV2Source::build_job_template(
            &make_extended_job(job),
            &chain_tip,
            ExtranonceRange::full(16).unwrap(),
        )
        .unwrap();

        let MerkleRootKind::Computed(mrt) = tmpl.merkle_root else {
            panic!("expected MerkleRootKind::Computed");
        };
        assert_eq!(mrt.extranonce_range.width(), 16);
    }
}
