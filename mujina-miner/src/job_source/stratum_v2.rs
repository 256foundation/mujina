//! Stratum V2 Extended Channel job source.
//!
//! Bridges [`StratumV2Client`] events into the scheduler's job-source
//! abstraction.  Converts `NewExtendedMiningJob`/`SetNewPrevHash` pairs into
//! [`JobTemplate`]s and manages the connection lifecycle with exponential
//! back-off.
//!
//! # Share Submission
//!
//! Shares received via [`SourceCommand::SubmitShare`] are forwarded to the pool
//! as `SubmitSharesExtended` messages.  Each share gets a monotonically
//! increasing sequence number within the connection.  Pending submits are
//! tracked in a `VecDeque`; they are drained on `SubmitSharesSuccess` and
//! removed individually on `SubmitSharesError`.
//!
//! ## Stale-Share Detection
//!
//! Job state is managed by an [`ExtendedChannel`] from `channels_sv2`.  Active
//! and past jobs (valid within the current chain tip) are tracked by the
//! channel; stale jobs (superseded by a prior chain tip) are rejected.
//! Shares referencing an unknown or stale job are dropped without forwarding.
//! The version field in each `SubmitSharesExtended` is reconstructed by
//! zeroing the GP bits from the job's base version and OR-ing in the
//! hardware-rolled GP bits from the share.

use std::collections::VecDeque;
use std::fmt;
use std::time::Duration;

use anyhow::{Context as _, Result};
use bitcoin::block::Version;
use bitcoin::hash_types::{BlockHash, TxMerkleNode};
use bitcoin::hashes::Hash as _;
use bitcoin::pow::CompactTarget;
use stratum_apps::stratum_core::binary_sv2::B032;
use stratum_apps::stratum_core::channels_sv2::chain_tip::ChainTip;
use stratum_apps::stratum_core::channels_sv2::client::error::ExtendedChannelError;
use stratum_apps::stratum_core::channels_sv2::client::extended::{ExtendedChannel, ExtendedJob};
use stratum_apps::stratum_core::channels_sv2::client::share_accounting::{
    ShareValidationError, ShareValidationResult,
};
use stratum_apps::stratum_core::channels_sv2::extranonce_manager::ExtranoncePrefix;
use stratum_apps::stratum_core::mining_sv2::SubmitSharesExtended;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::stratum_v2::{
    ClientCommand, ClientEvent, ClientOutcome, PoolConfig, StratumV2Client, target_from_le_bytes,
};
use crate::tracing::prelude::*;

use super::connection::{ConnectOutcome, ExponentialBackoff, backoff_wait};
use super::{
    Extranonce2Range, GeneralPurposeBits, JobTemplate, MerkleRootKind, MerkleRootTemplate, Share,
    SourceCommand, SourceEvent, VersionTemplate,
};

/// Minimum reconnect back-off delay.
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
/// Maximum reconnect back-off delay.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// Connections alive for at least this long reset the back-off on disconnect.
const STABLE_CONNECTION_THRESHOLD: Duration = Duration::from_secs(60);

/// Per-connection channel state.
///
/// Populated on `OpenExtendedMiningChannelSuccess` and replaced on each
/// reconnect.
struct SessionState {
    channel_id: u32,
    group_channel_id: u32,
    /// The number of bytes in the extranonce (prefix + rollable).
    extranonce_size: u8,
    /// Rollable portion of the extranonce (capped by the u64 in `Extranonce2::value`).
    extranonce_rollable_size: u8,
    /// Monotonically increasing sequence number for `SubmitSharesExtended`.
    next_seq: u32,
    /// Sequence numbers of submitted-but-unacknowledged shares, in submission
    /// order.  Drained front-to-front on `SubmitSharesSuccess`, removed by
    /// value on `SubmitSharesError`.
    pending_submits: VecDeque<u32>,
    /// Extended channel state: job lifecycle, target, extranonce prefix, and
    /// share accounting.
    channel: ExtendedChannel<'static>,
}

impl SessionState {
    /// Return true if `channel_id` targets this session.
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

    /// Drain all pending submit entries acknowledged by `last_seq`.
    ///
    /// `front` is considered ≤ `last_seq` when the forward distance from
    /// `front` to `last_seq` in the circular u32 space is ≤ 2^31 − 1.
    /// This is the "less than or equal to" relation from RFC 1982 § 3.2
    /// (<https://www.rfc-editor.org/rfc/rfc1982#section-3.2>), applied with
    /// SERIAL_BITS = 32 so the threshold is `u32::MAX / 2` (= 2^31 − 1).
    ///
    /// The expression `last_seq.wrapping_sub(front) <= u32::MAX / 2` avoids
    /// the half-window ambiguity of the `wrapping_sub as i32 <= 0` pattern,
    /// which misclassifies a `front` exactly 2^31 steps ahead of `last_seq`
    /// as already acknowledged (because `2^31 as i32 == i32::MIN < 0`).
    fn acknowledge_up_to(&mut self, last_seq: u32) {
        while let Some(&front) = self.pending_submits.front() {
            if last_seq.wrapping_sub(front) <= u32::MAX / 2 {
                self.pending_submits.pop_front();
            } else {
                break;
            }
        }
    }

    /// Remove the entry for `seq` from the pending queue (pool rejected it).
    fn discard_pending(&mut self, seq: u32) {
        self.pending_submits.retain(|&s| s != seq);
    }
}

/// Stratum V2 Extended Channel job source.
///
/// Manages the connection lifecycle (DNS → TCP → Noise NX → session
/// negotiation) with exponential back-off and translates pool events into
/// [`SourceEvent`]s for the scheduler.
pub struct StratumV2Source {
    config: PoolConfig,
    event_tx: mpsc::Sender<SourceEvent>,
    command_rx: mpsc::Receiver<SourceCommand>,
    shutdown: CancellationToken,
    session: Option<SessionState>,
}

impl fmt::Display for StratumV2Source {
    /// Human-readable pool name (`host:port`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.config.host(), self.config.port)
    }
}

impl StratumV2Source {
    /// Create a new Stratum V2 job source.
    pub fn new(
        config: PoolConfig,
        command_rx: mpsc::Receiver<SourceCommand>,
        event_tx: mpsc::Sender<SourceEvent>,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            config,
            event_tx,
            command_rx,
            shutdown,
            session: None,
        }
    }

    /// Run the source (main entry point).
    ///
    /// Drains incoming commands until a positive hashrate is reported, then
    /// enters the connect loop: attempt connection, serve events, and reconnect
    /// with exponential back-off on disconnect.
    pub async fn run(mut self) -> Result<()> {
        info!(
            host = %self.config.host(),
            port = self.config.port,
            "Waiting for hash threads before connecting"
        );

        // Wait for positive hashrate before opening a connection.
        loop {
            tokio::select! {
                Some(cmd) = self.command_rx.recv() => {
                    match cmd {
                        SourceCommand::UpdateHashRate(rate) => {
                            if !rate.is_zero() {
                                break;
                            }
                        }
                        // No connection yet; drop shares silently.
                        SourceCommand::SubmitShare(_) => {}
                    }
                }
                _ = self.shutdown.cancelled() => return Ok(()),
            }
        }

        // Connect with automatic reconnection.
        let mut backoff = ExponentialBackoff::new(INITIAL_BACKOFF, MAX_BACKOFF);

        loop {
            self.session = None;

            info!(host = %self.config.host(), port = self.config.port, "Connecting to pool");

            let connected_at = tokio::time::Instant::now();
            match self.connect_and_run().await {
                ConnectOutcome::Shutdown => return Ok(()),
                ConnectOutcome::Fatal(e) => {
                    error!(error = %e, "Fatal pool error, not reconnecting");
                    return Err(e);
                }
                ConnectOutcome::Disconnected => {
                    // Invalidate stale work.
                    if let Err(e) = self.event_tx.send(SourceEvent::ClearJobs).await {
                        warn!(error = %e, "Failed to send ClearJobs");
                    }
                    if connected_at.elapsed() >= STABLE_CONNECTION_THRESHOLD {
                        backoff.reset();
                    }
                    let delay = backoff.next_delay();
                    info!(
                        host = %self.config.host(),
                        delay_secs = delay.as_secs_f64(),
                        "Reconnecting after back-off"
                    );
                    if self.backoff_wait(delay).await {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Run a single connection attempt through its full lifecycle.
    ///
    /// Spawns the [`StratumV2Client`] task, runs the event loop until the
    /// client exits (or shutdown is requested), and returns the outcome.
    async fn connect_and_run(&mut self) -> ConnectOutcome {
        let (client_event_tx, mut client_event_rx) = mpsc::channel::<ClientEvent>(100);
        let (client_command_tx, client_command_rx) = mpsc::channel::<ClientCommand>(100);

        let client = StratumV2Client::new(
            self.config.clone(),
            client_event_tx,
            client_command_rx,
            self.shutdown.clone(),
        );

        let client_handle = tokio::spawn(async move { client.run().await });

        loop {
            tokio::select! {
                event_opt = client_event_rx.recv() => {
                    match event_opt {
                        Some(event) => {
                            if let Err(e) = self.handle_client_event(event).await {
                                warn!(error = %e, "Error handling client event");
                            }
                        }
                        None => {
                            // Client task exited; determine outcome below.
                            break;
                        }
                    }
                }

                Some(cmd) = self.command_rx.recv() => {
                    match cmd {
                        SourceCommand::SubmitShare(share) => {
                            self.handle_share_submission(share, &client_command_tx).await;
                        }
                        SourceCommand::UpdateHashRate(_) => {}
                    }
                }

                _ = self.shutdown.cancelled() => {
                    return ConnectOutcome::Shutdown;
                }
            }
        }

        match client_handle.await {
            Ok(Ok(ClientOutcome::Shutdown)) => ConnectOutcome::Shutdown,
            Ok(Ok(ClientOutcome::ConnectionClosed | ClientOutcome::PoolRequestedReconnect)) => {
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

    /// Dispatch a single event from the [`StratumV2Client`].
    async fn handle_client_event(&mut self, event: ClientEvent) -> Result<()> {
        match event {
            ClientEvent::SetupConnectionSuccess { used_version, .. } => {
                debug!(version = used_version, "SetupConnection accepted");
            }

            ClientEvent::OpenExtendedMiningChannelSuccess(msg) => {
                let extranonce_prefix_bytes = msg.extranonce_prefix.inner_as_ref().to_vec();
                let extranonce_prefix =
                    ExtranoncePrefix::from_wire(extranonce_prefix_bytes.clone()).map_err(|e| {
                        anyhow::anyhow!(
                            "OpenExtendedMiningChannelSuccess: invalid extranonce prefix: {e}"
                        )
                    })?;

                let extranonce_rollable_size =
                    Self::derive_extranonce_rollable_size(msg.extranonce_size)
                        .context("OpenExtendedMiningChannelSuccess: invalid extranonce params")?;
                let extranonce_size = (msg.extranonce_size as usize).min(u8::MAX as usize) as u8;
                let initial_target = target_from_le_bytes(msg.target.inner_as_ref())
                    .context("OpenExtendedMiningChannelSuccess: invalid target")?;

                info!(
                    host = %self.config.host(),
                    channel_id = msg.channel_id,
                    extranonce_prefix = %hex::encode(&extranonce_prefix_bytes),
                    extranonce_rollable_size,
                    extranonce_size,
                    "Extended channel opened"
                );

                // version_rolling_allowed is set per-job by the pool on NewExtendedMiningJob.
                // The channel-level flag here is used by validate_share() to enforce BIP320
                // compliance. Set true because Mujina performs version rolling in software.
                let channel = ExtendedChannel::new(
                    msg.channel_id,
                    self.config.user_identity.as_utf8_or_hex(),
                    extranonce_prefix,
                    initial_target,
                    self.config.nominal_hash_rate.0 as f32,
                    true,
                    msg.extranonce_size,
                );

                self.session = Some(SessionState {
                    channel_id: msg.channel_id,
                    group_channel_id: msg.group_channel_id,
                    extranonce_rollable_size,
                    extranonce_size,
                    next_seq: 0,
                    pending_submits: VecDeque::new(),
                    channel,
                });
            }

            ClientEvent::SetTarget(msg) => {
                let target = target_from_le_bytes(msg.maximum_target.inner_as_ref())
                    .context("SetTarget: invalid target")?;
                if let Some(session) = &mut self.session {
                    if !session.accepts_channel_id(msg.channel_id) {
                        warn!(
                            expected = session.channel_id,
                            group = session.group_channel_id,
                            got = msg.channel_id,
                            "SetTarget channel_id mismatch; ignoring"
                        );
                        return Ok(());
                    }
                    debug!(channel_id = msg.channel_id, %target, "SetTarget");
                    session.channel.set_target(target);
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
                    // Re-borrow session immutably — the mutable op is done.
                    let session = self.session.as_ref().unwrap();
                    let Some(chain_tip) = session.channel.get_chain_tip() else {
                        warn!(
                            job_id,
                            "Non-future job arrived before SetNewPrevHash; dropping"
                        );
                        return Ok(());
                    };
                    let active = session
                        .channel
                        .get_active_job()
                        .expect("active job must be set after non-future activation");
                    let template = Self::build_job_template(
                        active,
                        chain_tip,
                        session.extranonce_rollable_size,
                        session.extranonce_size,
                    )?;
                    debug!(job_id, "Emitting ReplaceJob");
                    self.event_tx
                        .send(SourceEvent::ReplaceJob(template))
                        .await?;
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

                let prev_hash_hex = hex::encode(msg.prev_hash.inner_as_ref());
                let job_id = msg.job_id;
                debug!(
                    job_id,
                    prev_hash = %prev_hash_hex,
                    nbits = format!("{:#010x}", msg.nbits),
                    "SetNewPrevHash"
                );

                match session.channel.on_set_new_prev_hash(msg) {
                    Ok(()) => {
                        // Re-borrow session immutably — the mutable op is done.
                        let session = self.session.as_ref().unwrap();
                        let chain_tip = session.channel.get_chain_tip().unwrap();
                        let active = session
                            .channel
                            .get_active_job()
                            .expect("active job must be set after on_set_new_prev_hash");
                        let template = Self::build_job_template(
                            active,
                            chain_tip,
                            session.extranonce_rollable_size,
                            session.extranonce_size,
                        )?;
                        debug!(job_id, "Emitting ReplaceJob (future → active)");
                        self.event_tx
                            .send(SourceEvent::ReplaceJob(template))
                            .await?;
                    }
                    Err(ExtendedChannelError::JobIdNotFound) => {
                        // No future job was buffered for this prevhash — clear work
                        // and wait for the next NewExtendedMiningJob.
                        debug!(
                            job_id,
                            "SetNewPrevHash: no buffered future job; clearing work"
                        );
                        if let Err(e) = self.event_tx.send(SourceEvent::ClearJobs).await {
                            warn!(error = %e, "Failed to send ClearJobs on SetNewPrevHash");
                        }
                    }
                    Err(e) => return Err(anyhow::anyhow!("SetNewPrevHash: {e:?}")),
                }
            }

            // Pool-initiated reconnect signals.  The client task will exit with
            // PoolRequestedReconnect after emitting these events; no local flag needed.
            ClientEvent::Reconnect(msg) => {
                info!(
                    host = %self.config.host(),
                    new_host = msg.new_host.as_utf8_or_hex(),
                    new_port = msg.new_port,
                    "Pool requested reconnect"
                );
            }
            ClientEvent::ChannelEndpointChanged(msg) => {
                info!(
                    channel_id = msg.channel_id,
                    "Channel endpoint changed; reconnecting"
                );
            }
            ClientEvent::CloseChannel(msg) => {
                info!(
                    channel_id = msg.channel_id,
                    reason = msg.reason_code.as_utf8_or_hex(),
                    "Channel closed by pool; reconnecting"
                );
            }

            ClientEvent::SubmitSharesSuccess(msg) => {
                if let Some(session) = &mut self.session {
                    if !session.accepts_channel_id(msg.channel_id) {
                        warn!(
                            expected = session.channel_id,
                            group = session.group_channel_id,
                            got = msg.channel_id,
                            "SubmitSharesSuccess channel_id mismatch; ignoring"
                        );
                        return Ok(());
                    }
                    debug!(
                        channel_id = msg.channel_id,
                        last_seq = msg.last_sequence_number,
                        accepted = msg.new_submits_accepted_count,
                        "Pool accepted shares"
                    );
                    session.acknowledge_up_to(msg.last_sequence_number);
                    session.channel.on_share_acknowledgement(
                        msg.new_submits_accepted_count,
                        msg.new_shares_sum as f64,
                    );
                    if msg.new_submits_accepted_count > 0 {
                        self.event_tx
                            .send(SourceEvent::SharesAccepted(msg.new_submits_accepted_count))
                            .await?;
                    }
                }
            }

            ClientEvent::SubmitSharesError(msg) => {
                if let Some(session) = &mut self.session {
                    if !session.accepts_channel_id(msg.channel_id) {
                        warn!(
                            expected = session.channel_id,
                            group = session.group_channel_id,
                            got = msg.channel_id,
                            "SubmitSharesError channel_id mismatch; ignoring"
                        );
                        return Ok(());
                    }
                    warn!(
                        channel_id = msg.channel_id,
                        seq = msg.sequence_number,
                        reason = msg.error_code.as_utf8_or_hex(),
                        "Share rejected by pool"
                    );
                    session.discard_pending(msg.sequence_number);
                    session
                        .channel
                        .on_share_rejection(msg.error_code.as_utf8_or_hex());
                    self.event_tx.send(SourceEvent::SharesRejected).await?;
                }
            }
        }

        Ok(())
    }

    /// Build and forward a share to the pool.
    ///
    /// Drops the share (with a trace/warn log) if:
    /// - There is no active session (not yet connected or reconnecting).
    /// - The job ID is not a valid `u32` (wrong protocol).
    /// - Extranonce2 is missing from the share.
    /// - Extranonce2 bytes cannot be encoded as B032 (> 32 bytes; should not
    ///   happen because `client.rs` rejects `extranonce_size > MAX_EXTRANONCE_SIZE (32)`
    ///   at channel open).
    /// - Local share validation fails (stale job, below target, duplicate, or
    ///   version rolling violation); see [`ExtendedChannel::validate_share`].
    async fn handle_share_submission(
        &mut self,
        share: Share,
        client_cmd_tx: &mpsc::Sender<ClientCommand>,
    ) {
        let Some(session) = &mut self.session else {
            trace!(job_id = %share.job_id, "Share dropped: no active session");
            return;
        };

        let job_id: u32 = match share.job_id.parse() {
            Ok(id) => id,
            Err(_) => {
                warn!(
                    job_id = %share.job_id,
                    "SV2 share has non-u32 job_id (wrong protocol?); dropping"
                );
                return;
            }
        };

        let Some(en2) = share.extranonce2 else {
            warn!(job_id, "SV2 share missing extranonce2; dropping");
            return;
        };

        let mut en2_bytes: Vec<u8> = en2.into();
        // Zero-pad to the pool's full extranonce2 allocation when it exceeds
        // the 8-byte counter width. This is due to a `u64` in `Extranonce2::value`
        // limiting how many bytes extranonce could take of the coinbase script sig.
        en2_bytes.resize(session.extranonce_size as usize, 0);
        // Mining Protocol > SubmitSharesExtended:
        // https://github.com/stratum-mining/sv2-spec/blob/main/05-Mining-Protocol.md#5312-submitsharesextended-client---server
        // extranonce size MUST equal the negotiated extranonce_size; full coinbase:
        // coinbase_tx_prefix + extranonce_prefix + extranonce + coinbase_tx_suffix.
        let extranonce = match B032::try_from(en2_bytes) {
            Ok(b) => b.into_static(),
            Err(e) => {
                warn!(job_id, error = ?e, "Failed to encode extranonce as B032; dropping");
                return;
            }
        };

        let seq = session.next_seq;
        session.next_seq = session.next_seq.wrapping_add(1);
        session.pending_submits.push_back(seq);

        // VersionTemplate enforces that base_version has bits 13–28 clear; hardware
        // only modifies those bits, so share.version is already the exact version
        // used in the block header.
        let sv2_share = SubmitSharesExtended {
            channel_id: session.channel_id,
            sequence_number: seq,
            job_id,
            nonce: share.nonce,
            ntime: share.time,
            version: share.version.to_consensus() as u32,
            extranonce,
        };

        match session.channel.validate_share(sv2_share.clone()) {
            Ok(ShareValidationResult::BlockFound(_)) => {
                info!(job_id, seq, "Block candidate found");
            }
            Ok(ShareValidationResult::Valid(_)) => {}
            // Below-target shares still reach the pool — hardware/software filters at
            // target before this point, so this is rare in practice. Pool feedback on
            // these (e.g. difficulty-too-low after a SetTarget race) is more useful than
            // a silent local drop.
            Err(ShareValidationError::DoesNotMeetTarget) => {}
            Err(e) => {
                debug!(job_id, seq, error = ?e, "Share failed local validation; dropping");
                session.pending_submits.pop_back();
                return;
            }
        }

        trace!(
            job_id,
            seq,
            nonce = format!("{:#010x}", share.nonce),
            "Submitting share to pool"
        );

        if client_cmd_tx
            .send(ClientCommand::SubmitShare(sv2_share))
            .await
            .is_err()
        {
            warn!(seq, "Client disconnected before share could be forwarded");
        }
    }

    /// Convert a [`NewExtendedMiningJob`] into a [`JobTemplate`].
    ///
    /// The SV2 field mapping is:
    ///
    /// | SV2 field              | `MerkleRootTemplate` field |
    /// |------------------------|---------------------------|
    /// | `coinbase_tx_prefix`   | `coinbase1`               |
    /// | `extranonce_prefix`    | `extranonce1`             |
    /// | `coinbase_tx_suffix`   | `coinbase2`               |
    /// | `merkle_path` entries  | `merkle_branches`         |
    fn build_job_template(
        job: &ExtendedJob<'static>,
        chain_tip: &ChainTip,
        extranonce_rollable_size: u8,
        extranonce_size: u8,
    ) -> Result<JobTemplate> {
        let (msg, extranonce_prefix, target) = job;

        let extranonce2_range = Extranonce2Range::new(extranonce_rollable_size)
            .map_err(|e| anyhow::anyhow!("extranonce2 range: {e}"))?;

        // Version template.
        //
        // GP bits (13–28) are defined by BIP320 (https://github.com/bitcoin/bips/blob/master/bip-0320.mediawiki)
        // for general-purpose version rolling.
        //
        // When version_rolling_allowed=true, the pool sets them to zero and
        // the miner fills them — maps to GeneralPurposeBits::full().
        //
        // When version_rolling_allowed=false, the version is fixed.
        // VersionTemplate requires bits 13–28 clear in the base, so strip
        // them.  Compliant SV2 pools set them to zero in this case; if not,
        // we warn and strip.
        let raw_version = msg.version;
        let gp_bits_mask = if msg.version_rolling_allowed {
            GeneralPurposeBits::full()
        } else {
            if raw_version & 0x1fffe000 != 0 {
                warn!(
                    job_id = msg.job_id,
                    version = format!("{:#010x}", raw_version),
                    "Non-rolling SV2 job has GP bits set; stripping them"
                );
            }
            GeneralPurposeBits::none()
        };
        let base_version = Version::from_consensus((raw_version & !0x1fffe000) as i32);
        let version_template = VersionTemplate::new(base_version, gp_bits_mask)
            .map_err(|e| anyhow::anyhow!("VersionTemplate: {e}"))?;

        // Merkle path: Seq0255<U256> → Vec<TxMerkleNode>.
        //
        // U256<'decoder> is a reference type (&[u8]), so iterating over
        // inner_as_ref() yields &&[u8]; deref once to get &[u8].
        let merkle_branches: Vec<TxMerkleNode> = msg
            .merkle_path
            .inner_as_ref()
            .iter()
            .map(|u256| {
                let bytes: [u8; 32] = (*u256)
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("merkle branch is not 32 bytes"))?;
                Ok(TxMerkleNode::from_byte_array(bytes))
            })
            .collect::<Result<Vec<_>>>()?;

        let prev_hash_bytes: [u8; 32] = chain_tip
            .prev_hash()
            .inner_as_ref()
            .try_into()
            .map_err(|_| anyhow::anyhow!("chain tip prev_hash is not 32 bytes"))?;

        Ok(JobTemplate {
            id: msg.job_id.to_string(),
            prev_blockhash: BlockHash::from_byte_array(prev_hash_bytes),
            version: version_template,
            bits: CompactTarget::from_consensus(chain_tip.nbits()),
            share_target: *target,
            // For immediate (non-future) jobs, min_ntime is set in the job message.
            // For future jobs activated by SetNewPrevHash, min_ntime is None in the
            // job and falls back to the value from the chain tip.
            time: msg
                .min_ntime
                .clone()
                .into_inner()
                .unwrap_or_else(|| chain_tip.min_ntime()),
            merkle_root: MerkleRootKind::Computed(MerkleRootTemplate {
                coinbase1: msg.coinbase_tx_prefix.inner_as_ref().to_vec(),
                extranonce1: extranonce_prefix.clone(),
                extranonce2_range,
                extranonce2_size: extranonce_size,
                coinbase2: msg.coinbase_tx_suffix.inner_as_ref().to_vec(),
                merkle_branches,
            }),
        })
    }

    /// Derive the rollable portion size from the pool's `extranonce_size`.
    ///
    /// `extranonce_size` is the miner's total extranonce allocation per the SV2
    /// spec (`OpenExtendedMiningChannelSuccess.extranonce_size`). It does NOT
    /// include the pool's fixed `extranonce_prefix`.
    ///
    /// Because [`Extranonce2`] stores the counter as a `u64`, the rollable size
    /// is capped at 8. When the pool allocates more than 8 bytes the miner wraps (restart from zero)
    /// the first 8 and zero-pads the remainder in the coinbase and in
    /// `SubmitSharesExtended.extranonce`.
    fn derive_extranonce_rollable_size(extranonce_size: u16) -> Result<u8> {
        if extranonce_size == 0 {
            return Err(anyhow::anyhow!(
                "extranonce_size 0 out of range (must be ≥ 1 byte)"
            ));
        }
        Ok((extranonce_size as usize).min(8) as u8)
    }

    /// Drain commands during back-off sleep.
    ///
    /// Returns `true` if shutdown was requested during the wait.
    async fn backoff_wait(&mut self, delay: Duration) -> bool {
        backoff_wait(delay, &mut self.command_rx, &self.shutdown).await
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::super::Extranonce2;
    use super::*;
    use bitcoin::pow::Target;

    // ---- derive_extranonce_rollable_size ----

    #[test]
    fn extranonce_rollable_size_typical() {
        // Pool's extranonce_size = 4 bytes → counter width 4.
        assert_eq!(
            StratumV2Source::derive_extranonce_rollable_size(4).unwrap(),
            4
        );
    }

    #[test]
    fn extranonce_rollable_size_valid_boundary() {
        // Pool's extranonce_size = 8 bytes → counter width 8 (maximum).
        assert_eq!(
            StratumV2Source::derive_extranonce_rollable_size(8).unwrap(),
            8
        );

        // Pool's extranonce_size = 1 byte → counter width 1 (minimum).
        assert_eq!(
            StratumV2Source::derive_extranonce_rollable_size(1).unwrap(),
            1
        );
    }

    #[test]
    fn extranonce_rollable_size_zero_rejected() {
        let err = StratumV2Source::derive_extranonce_rollable_size(0).unwrap_err();
        assert!(err.to_string().contains("out of range"));
    }

    #[test]
    fn extranonce_rollable_size_capped_when_pool_allocates_more_than_8() {
        // Pool's extranonce_size = 16 bytes; counter is capped at 8, remainder zero-padded.
        assert_eq!(
            StratumV2Source::derive_extranonce_rollable_size(16).unwrap(),
            8
        );
    }

    // ---- target_from_le_bytes ----

    #[test]
    fn target_from_le_bytes_roundtrip() {
        let target = Target::MAX;
        let le_bytes = target.to_le_bytes();
        let recovered = target_from_le_bytes(&le_bytes).unwrap();
        assert_eq!(recovered, target);
    }

    #[test]
    fn target_from_le_bytes_wrong_length() {
        let err = target_from_le_bytes(&[0u8; 16]).unwrap_err();
        assert!(err.to_string().contains("32-byte"));
    }

    // ---- ExponentialBackoff ----

    #[test]
    fn backoff_doubles_each_step() {
        let mut b = ExponentialBackoff::new(Duration::from_secs(1), Duration::from_secs(60));
        let d1 = b.next_delay();
        let d2 = b.next_delay();
        let d3 = b.next_delay();

        // Nominal: 1 s, 2 s, 4 s. Jitter in [0.5, 1.0].
        assert!(d1 >= Duration::from_millis(500) && d1 < Duration::from_secs(1));
        assert!(d2 >= Duration::from_secs(1) && d2 < Duration::from_secs(2));
        assert!(d3 >= Duration::from_secs(2) && d3 < Duration::from_secs(4));
    }

    #[test]
    fn backoff_caps_at_max() {
        let mut b = ExponentialBackoff::new(Duration::from_secs(32), Duration::from_secs(60));
        b.next_delay(); // 32 s nominal
        let d = b.next_delay(); // capped at 60 s → jittered to [30, 60)
        assert!(d >= Duration::from_secs(30) && d < Duration::from_secs(60));
    }

    #[test]
    fn backoff_reset_restores_initial() {
        let mut b = ExponentialBackoff::new(Duration::from_secs(1), Duration::from_secs(60));
        b.next_delay();
        b.next_delay();
        b.reset();
        let d = b.next_delay();
        assert!(d >= Duration::from_millis(500) && d < Duration::from_secs(1));
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
        );
        SessionState {
            channel_id: 1,
            group_channel_id: 0,
            extranonce_rollable_size: 4,
            extranonce_size: 4,
            next_seq: 0,
            pending_submits: VecDeque::new(),
            channel,
        }
    }

    /// Contract: SubmitSharesSuccess removes all pending entries up to and
    /// including last_sequence_number; later entries are untouched.
    #[test]
    fn success_drains_acknowledged_sequences() {
        let mut session = make_session();
        session.pending_submits.extend([0, 1, 2, 3, 4]);

        session.acknowledge_up_to(2);

        assert_eq!(session.pending_submits, VecDeque::from([3, 4]));
    }

    /// Contract: acknowledge_up_to is correct when next_seq has wrapped
    /// around u32::MAX (entries near MAX must compare as before entries
    /// near 0 when last_seq is near 0).
    #[test]
    fn success_handles_sequence_wraparound() {
        let mut session = make_session();
        session
            .pending_submits
            .extend([u32::MAX - 1, u32::MAX, 0, 1]);

        session.acknowledge_up_to(0);

        assert_eq!(session.pending_submits, VecDeque::from([1]));
    }

    /// Contract: an entry exactly 2^31 steps ahead of last_seq is NOT
    /// acknowledged — it lies on the ambiguous boundary of the RFC 1982
    /// half-window and must be treated as a future (un-acked) entry.
    #[test]
    fn success_does_not_drain_half_window_boundary() {
        let mut session = make_session();
        // 2^31 = u32::MAX / 2 + 1 steps ahead of last_seq = 0.
        let half_window_plus_one = (u32::MAX / 2).wrapping_add(1);
        session.pending_submits.extend([0, 1, half_window_plus_one]);

        session.acknowledge_up_to(0);

        // Only seq 0 (== last_seq) is drained; 1 and the boundary entry stay.
        assert_eq!(
            session.pending_submits,
            VecDeque::from([1, half_window_plus_one])
        );
    }

    /// Contract: SubmitSharesError removes only the rejected entry;
    /// all other pending entries are untouched.
    #[test]
    fn error_removes_rejected_sequence() {
        let mut session = make_session();
        session.pending_submits.extend([0, 1, 2, 3]);

        session.discard_pending(2);

        assert_eq!(session.pending_submits, VecDeque::from([0, 1, 3]));
    }

    #[test]
    fn extranonce2_encoding_little_endian() {
        let en2 = Extranonce2::new(0x0102_0304, 4).unwrap();
        let bytes: Vec<u8> = en2.into();
        assert_eq!(bytes, vec![0x04, 0x03, 0x02, 0x01]);
    }

    #[test]
    fn extranonce2_encoding_single_byte() {
        let en2 = Extranonce2::new(0xab, 1).unwrap();
        let bytes: Vec<u8> = en2.into();
        assert_eq!(bytes, vec![0xab]);
    }

    // ---- build_job_template ----

    use bitcoin::{hash_types::BlockHash, pow::CompactTarget};
    use stratum_apps::stratum_core::{
        binary_sv2::{B064K, Seq0255, Sv2Option, U256},
        mining_sv2::{NewExtendedMiningJob, SetNewPrevHash as SetNewPrevHashMp},
    };

    /// Construct a `NewExtendedMiningJob<'static>` with the given parameters.
    fn make_job(
        job_id: u32,
        version: u32,
        version_rolling_allowed: bool,
        ntime: Option<u32>, // None → future job
        coinbase_prefix: Vec<u8>,
        coinbase_suffix: Vec<u8>,
        merkle_hashes: Vec<[u8; 32]>,
    ) -> NewExtendedMiningJob<'static> {
        let coinbase_tx_prefix = B064K::try_from(coinbase_prefix)
            .expect("valid coinbase prefix")
            .into_static();
        let coinbase_tx_suffix = B064K::try_from(coinbase_suffix)
            .expect("valid coinbase suffix")
            .into_static();
        let merkle_path_items: Vec<U256<'static>> = merkle_hashes
            .into_iter()
            .map(|h| {
                U256::try_from(h.to_vec())
                    .expect("valid 32-byte hash")
                    .into_static()
            })
            .collect();
        let merkle_path: Seq0255<'static, U256<'static>> = Seq0255::new(merkle_path_items)
            .expect("≤255 items")
            .into_static();
        NewExtendedMiningJob {
            channel_id: 1,
            job_id,
            min_ntime: Sv2Option::new(ntime),
            version,
            version_rolling_allowed,
            merkle_path,
            coinbase_tx_prefix,
            coinbase_tx_suffix,
        }
    }

    /// Construct a [`ChainTip`] with a zeroed prev_hash and the given
    /// `min_ntime` / `nbits` values.
    fn make_chain_tip(min_ntime: u32, nbits: u32) -> ChainTip {
        ChainTip::from(SetNewPrevHashMp {
            channel_id: 0,
            job_id: 0,
            prev_hash: [0u8; 32].into(),
            nbits,
            min_ntime,
        })
    }

    /// Contract: coinbase_tx_prefix → coinbase1, extranonce_prefix →
    /// extranonce1, coinbase_tx_suffix → coinbase2, merkle_path →
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
            &(job, vec![0xde, 0xad], Target::MAX),
            &chain_tip,
            4,
            4,
        )
        .unwrap();

        assert_eq!(tmpl.id, "42");
        assert_eq!(tmpl.prev_blockhash, BlockHash::all_zeros());
        assert_eq!(tmpl.bits, CompactTarget::from_consensus(0x1d00_ffff));
        assert_eq!(tmpl.share_target, Target::MAX);

        let MerkleRootKind::Computed(mrt) = tmpl.merkle_root else {
            panic!("expected MerkleRootKind::Computed");
        };
        assert_eq!(mrt.coinbase1, prefix);
        assert_eq!(mrt.extranonce1, vec![0xde, 0xad]);
        assert_eq!(mrt.coinbase2, suffix);
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
            &(immediate, vec![0xde, 0xad], Target::MAX),
            &chain_tip,
            4,
            4,
        )
        .unwrap();
        assert_eq!(
            tmpl.time, job_ntime,
            "immediate job must use its own min_ntime"
        );

        // Future job: chain tip ntime is the fallback.
        let future = make_job(2, 0x2000_0000, false, None, vec![], vec![], vec![]);
        let tmpl = StratumV2Source::build_job_template(
            &(future, vec![0xde, 0xad], Target::MAX),
            &chain_tip,
            4,
            4,
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
            &(fixed, vec![0xde, 0xad], Target::MAX),
            &chain_tip,
            4,
            4,
        )
        .unwrap();
        assert_eq!(
            tmpl.version.gp_bits_mask(),
            GeneralPurposeBits::none(),
            "fixed-version job must have no GP bits"
        );

        let rolling = make_job(2, 0x2000_0000, true, Some(0), vec![], vec![], vec![]);
        let tmpl = StratumV2Source::build_job_template(
            &(rolling, vec![0xde, 0xad], Target::MAX),
            &chain_tip,
            4,
            4,
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
        use bitcoin::hash_types::TxMerkleNode;
        use bitcoin::hashes::Hash as _;

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
            &(job, vec![0xde, 0xad], Target::MAX),
            &chain_tip,
            4,
            4,
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

    // ---- extranonce_size propagation ----

    /// Contract: extranonce_size is forwarded from the session into the
    /// MerkleRootTemplate so compute_merkle_root can zero-pad en2 correctly
    /// when the pool's extranonce_size exceeds 8 bytes.
    #[test]
    fn build_job_template_extranonce_size_propagated() {
        let chain_tip = make_chain_tip(0, 0x1d00_ffff);
        let job = make_job(1, 0x2000_0000, false, Some(0), vec![], vec![], vec![]);
        let tmpl = StratumV2Source::build_job_template(
            &(job, vec![0xde, 0xad], Target::MAX),
            &chain_tip,
            8,
            16,
        )
        .unwrap();

        let MerkleRootKind::Computed(mrt) = tmpl.merkle_root else {
            panic!("expected MerkleRootKind::Computed");
        };
        assert_eq!(
            mrt.extranonce2_size, 16,
            "extranonce_size must match the pool extranonce_size"
        );
        assert_eq!(
            mrt.extranonce2_range.size, 8,
            "counter width must remain capped at 8"
        );
    }

    /// Contract: when extranonce_size == extranonce_rollable_size (the typical case),
    /// extranonce2_size equals extranonce2_range.size in the resulting template.
    #[test]
    fn build_job_template_extranonce_size_equals_rollable_when_not_padded() {
        let chain_tip = make_chain_tip(0, 0x1d00_ffff);
        let job = make_job(1, 0x2000_0000, false, Some(0), vec![], vec![], vec![]);
        let tmpl = StratumV2Source::build_job_template(
            &(job, vec![0xde, 0xad], Target::MAX),
            &chain_tip,
            4,
            4,
        )
        .unwrap();

        let MerkleRootKind::Computed(mrt) = tmpl.merkle_root else {
            panic!("expected MerkleRootKind::Computed");
        };
        assert_eq!(
            mrt.extranonce2_size, mrt.extranonce2_range.size,
            "extranonce_size and rollable size must match when the pool does not over-allocate"
        );
    }
}
