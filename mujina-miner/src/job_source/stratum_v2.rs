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
use bitcoin::pow::{CompactTarget, Target};
use stratum_apps::stratum_core::binary_sv2::{B032, U256};
use stratum_apps::stratum_core::channels_sv2::chain_tip::ChainTip;
use stratum_apps::stratum_core::channels_sv2::client::error::ExtendedChannelError;
use stratum_apps::stratum_core::channels_sv2::client::extended::{ExtendedChannel, ExtendedJob};
use stratum_apps::stratum_core::channels_sv2::client::share_accounting::{
    ShareValidationError, ShareValidationResult,
};
use stratum_apps::stratum_core::channels_sv2::extranonce_manager::ExtranoncePrefix;
use stratum_apps::stratum_core::mining_sv2::{SubmitSharesExtended, UpdateChannel};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::stratum_v2::{
    ClientCommand, ClientEvent, ClientOutcome, PoolConfig, StratumV2Client, target_from_le_bytes,
};
use crate::tracing::prelude::*;
use crate::types::Difficulty;

use super::connection::{ConnectOutcome, ExponentialBackoff, backoff_wait};
use super::{
    Extranonce2Range, GeneralPurposeBits, JobTemplate, MerkleRootKind, MerkleRootTemplate, Share,
    SourceCommand, SourceEvent, VersionTemplate,
};

/// How long the pool may go without sending work before the connection is
/// recycled.
///
/// A half-open TCP connection — the NAT or firewall drops state without
/// sending a FIN — produces no reader error and no EOF, so without a deadline
/// both the source and the client wait forever and the reconnect back-off is
/// never reached.  The V1 source bounds the same gap with the same limit.
const MAX_JOB_GAP: Duration = Duration::from_secs(2 * 60);

/// Why [`StratumV2Source::serve_events`] stopped serving a connection.
enum ServeOutcome {
    /// The source is shutting down.
    Shutdown,
    /// The client task ended; its return value says why.
    ClientExited,
    /// The session cannot continue and the connection must be rebuilt.
    SessionFailed(anyhow::Error),
    /// The pool stopped sending work within [`MAX_JOB_GAP`].
    JobGap,
}

/// Per-connection channel state.
///
/// Populated on `OpenExtendedMiningChannelSuccess` and replaced on each
/// reconnect.
struct SessionState {
    channel_id: u32,
    group_channel_id: u32,
    /// The miner's own extranonce allocation, in bytes.
    ///
    /// This is the pool's `extranonce_size` and excludes the pool-assigned
    /// `extranonce_prefix`.
    extranonce_size: u8,
    /// The portion of that allocation the miner can count through, capped at
    /// 8 by the `u64` in `Extranonce2::value`.
    extranonce_counter_size: u8,
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

    /// Drains all pending submit entries acknowledged by `last_seq`.
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

    /// Removes the entry for `seq` from the pending queue.
    ///
    /// Called when the pool rejects that share.
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
    /// Endpoint a pool's `Reconnect` named for the next connection attempt.
    reconnect_to: Option<(String, u16)>,
}

impl fmt::Display for StratumV2Source {
    /// Formats the source as `host:port`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.config.host(), self.config.port)
    }
}

impl StratumV2Source {
    /// Creates a new Stratum V2 job source.
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
            reconnect_to: None,
        }
    }

    /// Runs the source.
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
                                self.config.nominal_hash_rate = rate;
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

        // Connect with automatic reconnection.  A connection that stays up
        // for STABLE_CONNECTION_THRESHOLD resets the back-off on disconnect.
        const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
        const MAX_BACKOFF: Duration = Duration::from_secs(60);
        const STABLE_CONNECTION_THRESHOLD: Duration = Duration::from_secs(60);

        let mut backoff = ExponentialBackoff::new(INITIAL_BACKOFF, MAX_BACKOFF);

        loop {
            self.session = None;

            // A Reconnect names the server for the next attempt only.  Later
            // attempts go back to the configured pool, which can redirect
            // again, so a server that stops answering cannot strand the miner.
            let config = match self.reconnect_to.take() {
                Some((host, port)) => match self.config.with_endpoint(host, port) {
                    Ok(config) => config,
                    Err(e) => {
                        warn!(error = %e, "Unusable reconnect endpoint; using the configured pool");
                        self.config.clone()
                    }
                },
                None => self.config.clone(),
            };

            info!(host = %config.host(), port = config.port, "Connecting to pool");

            let connected_at = tokio::time::Instant::now();
            match self.connect_and_run(config).await {
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

    /// Runs a single connection attempt through its full lifecycle.
    ///
    /// Spawns the [`StratumV2Client`] task against `config`, runs the event
    /// loop until the client exits (or shutdown is requested), and returns the
    /// outcome.
    async fn connect_and_run(&mut self, config: PoolConfig) -> ConnectOutcome {
        let (client_event_tx, mut client_event_rx) = mpsc::channel::<ClientEvent>(100);
        let (client_command_tx, client_command_rx) = mpsc::channel::<ClientCommand>(100);

        // A per-attempt token, so a session-fatal event can stop this client
        // without cancelling the source's own shutdown token.
        let attempt_shutdown = self.shutdown.child_token();

        let client = StratumV2Client::new(
            config,
            client_event_tx,
            client_command_rx,
            attempt_shutdown.clone(),
        );

        let client_handle = tokio::spawn(async move { client.run().await });

        let serve = self
            .serve_events(&mut client_event_rx, &client_command_tx)
            .await;

        match serve {
            ServeOutcome::Shutdown => return ConnectOutcome::Shutdown,
            ServeOutcome::SessionFailed(e) => {
                // The session is unusable: either channel setup failed, or the
                // channel state has drifted from the pool's.  Tear the
                // connection down so the back-off loop rebuilds it, rather
                // than idling on a socket that will never produce work.
                error!(error = %e, "Session error; reconnecting");
                attempt_shutdown.cancel();
                let _ = client_handle.await;
                return ConnectOutcome::Disconnected;
            }
            ServeOutcome::JobGap => {
                warn!(
                    gap_secs = MAX_JOB_GAP.as_secs(),
                    "No job from pool within the gap limit; recycling the connection"
                );
                attempt_shutdown.cancel();
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
        let mut gap_start = tokio::time::Instant::now();

        loop {
            tokio::select! {
                event_opt = client_event_rx.recv() => {
                    match event_opt {
                        Some(event) => {
                            // Only work-bearing messages count as liveness; a
                            // pool that keeps the socket warm without ever
                            // sending a job is the case MAX_JOB_GAP guards.
                            if matches!(
                                event,
                                ClientEvent::NewExtendedMiningJob(_)
                                    | ClientEvent::SetNewPrevHash(_)
                            ) {
                                gap_start = tokio::time::Instant::now();
                            }
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
                            self.handle_share_submission(share, client_command_tx).await;
                        }
                        SourceCommand::UpdateHashRate(rate) => {
                            self.config.nominal_hash_rate = rate;
                            self.report_hash_rate(client_command_tx).await;
                        }
                    }
                }

                _ = tokio::time::sleep_until(gap_start + MAX_JOB_GAP) => {
                    return ServeOutcome::JobGap;
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

            ClientEvent::OpenExtendedMiningChannelSuccess(msg) => {
                let extranonce_prefix_bytes = msg.extranonce_prefix.inner_as_ref().to_vec();
                let extranonce_prefix =
                    ExtranoncePrefix::from_wire(extranonce_prefix_bytes.clone()).map_err(|e| {
                        anyhow::anyhow!(
                            "OpenExtendedMiningChannelSuccess: invalid extranonce prefix: {e}"
                        )
                    })?;

                let extranonce_counter_size =
                    Self::derive_extranonce_counter_size(msg.extranonce_size)
                        .context("OpenExtendedMiningChannelSuccess: invalid extranonce params")?;
                let extranonce_size = (msg.extranonce_size as usize).min(u8::MAX as usize) as u8;
                let initial_target = target_from_le_bytes(msg.target.inner_as_ref())
                    .context("OpenExtendedMiningChannelSuccess: invalid target")?;

                info!(
                    host = %self.config.host(),
                    channel_id = msg.channel_id,
                    extranonce_prefix = %hex::encode(&extranonce_prefix_bytes),
                    extranonce_counter_size,
                    extranonce_size,
                    "Extended channel opened"
                );

                // version_rolling_allowed is set per-job by the pool on NewExtendedMiningJob.
                // The channel-level flag here is used by validate_share() to enforce BIP320
                // compliance. Set true because Mujina performs version rolling in software.
                //
                // The last argument is the pool's full extranonce_size, not
                // extranonce_counter_size: the channel checks a submitted
                // share's extranonce length against it, and shares carry the
                // whole allocation zero-padded past the counter width.
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
                    extranonce_counter_size,
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
                    debug!(
                        channel_id = msg.channel_id,
                        difficulty = %Difficulty::from_target(target),
                        "SetTarget"
                    );
                    session.channel.set_target(target);
                }
            }

            ClientEvent::SetExtranoncePrefix(msg) => {
                let Some(session) = &mut self.session else {
                    warn!("SetExtranoncePrefix arrived before channel opened; dropping");
                    return Ok(());
                };
                // The prefix belongs to one channel, never to a group, so a
                // message addressed anywhere else is not meant for this one.
                if msg.channel_id != session.channel_id {
                    warn!(
                        expected = session.channel_id,
                        got = msg.channel_id,
                        "SetExtranoncePrefix channel_id mismatch; ignoring"
                    );
                    return Ok(());
                }
                let extranonce_prefix_bytes = msg.extranonce_prefix.inner_as_ref().to_vec();
                let extranonce_prefix =
                    ExtranoncePrefix::from_wire(extranonce_prefix_bytes.clone()).map_err(|e| {
                        anyhow::anyhow!("SetExtranoncePrefix: invalid extranonce prefix: {e}")
                    })?;
                session
                    .channel
                    .set_extranonce_prefix(extranonce_prefix)
                    .map_err(|e| anyhow::anyhow!("SetExtranoncePrefix: {e:?}"))?;
                info!(
                    channel_id = msg.channel_id,
                    extranonce_prefix = %hex::encode(&extranonce_prefix_bytes),
                    "Extranonce prefix replaced"
                );
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
                // channels_sv2 reads coinbase_tx_prefix[4] and [5] to spot the
                // BIP141 marker without checking the length first, so a
                // shorter prefix panics inside the dependency, and that panic
                // reaches the daemon as a fatal outcome.  Reject the job here
                // instead and let the session be rebuilt.
                const MIN_COINBASE_PREFIX_LEN: usize = 6;
                let coinbase_prefix_len = job.coinbase_tx_prefix.inner_as_ref().len();
                if coinbase_prefix_len < MIN_COINBASE_PREFIX_LEN {
                    return Err(anyhow::anyhow!(
                        "NewExtendedMiningJob {job_id}: coinbase_tx_prefix is \
                         {coinbase_prefix_len} bytes, need at least \
                         {MIN_COINBASE_PREFIX_LEN}"
                    ));
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
                    let template = Self::build_job_template(
                        active,
                        chain_tip,
                        session.extranonce_counter_size,
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
                        let template = Self::build_job_template(
                            active,
                            chain_tip,
                            session.extranonce_counter_size,
                            session.extranonce_size,
                        )?;
                        debug!(job_id, "Emitting ReplaceJob (future → active)");
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
                        return Err(anyhow::anyhow!(
                            "SetNewPrevHash {job_id}: no buffered future job for this \
                             prev hash, leaving the channel chain tip stale"
                        ));
                    }
                    Err(e) => return Err(anyhow::anyhow!("SetNewPrevHash: {e:?}")),
                }
            }

            // The client task exits with ChannelClosed after emitting this
            // event; no local flag needed.
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

    /// Builds a share and forwards it to the pool.
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

    /// Converts a [`NewExtendedMiningJob`] into a [`JobTemplate`].
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
        extranonce_counter_size: u8,
        extranonce_size: u8,
    ) -> Result<JobTemplate> {
        let (msg, extranonce_prefix, target) = job;

        let extranonce2_range = Extranonce2Range::new(extranonce_counter_size)
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

    /// Derives the extranonce counter width from the pool's `extranonce_size`.
    ///
    /// `extranonce_size` is the miner's total extranonce allocation per the SV2
    /// spec (`OpenExtendedMiningChannelSuccess.extranonce_size`). It does NOT
    /// include the pool's fixed `extranonce_prefix`.
    ///
    /// Because [`Extranonce2`] stores the counter as a `u64`, the counter width
    /// is capped at 8. When the pool allocates more than 8 bytes the miner wraps (restart from zero)
    /// the first 8 and zero-pads the remainder in the coinbase and in
    /// `SubmitSharesExtended.extranonce`.
    fn derive_extranonce_counter_size(extranonce_size: u16) -> Result<u8> {
        if extranonce_size == 0 {
            return Err(anyhow::anyhow!(
                "extranonce_size 0 out of range (must be ≥ 1 byte)"
            ));
        }
        Ok((extranonce_size as usize).min(8) as u8)
    }

    /// Tells the pool the channel's current nominal hash rate.
    ///
    /// Pools size the channel's target from this figure, so a value left at
    /// the rate observed when the channel opened costs share quality for the
    /// life of the connection.  Does nothing before a channel exists; the
    /// opening `OpenExtendedMiningChannel` carries the rate instead.
    async fn report_hash_rate(&self, client_command_tx: &mpsc::Sender<ClientCommand>) {
        let Some(session) = &self.session else {
            return;
        };

        let update = UpdateChannel {
            channel_id: session.channel_id,
            nominal_hash_rate: self.config.nominal_hash_rate.0 as f32,
            maximum_target: U256::from(Target::MAX.to_le_bytes()),
        };

        if let Err(e) = client_command_tx
            .send(ClientCommand::UpdateChannel(update.into_static()))
            .await
        {
            warn!(error = %e, "Failed to queue UpdateChannel");
        }
    }

    /// Drains commands during the back-off sleep.
    ///
    /// Returns `true` if shutdown was requested during the wait.
    async fn backoff_wait(&mut self, delay: Duration) -> bool {
        let config = &mut self.config;
        backoff_wait(delay, &mut self.command_rx, &self.shutdown, |cmd| {
            if let SourceCommand::UpdateHashRate(rate) = cmd {
                config.nominal_hash_rate = rate;
            }
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use stratum_apps::stratum_core::{
        binary_sv2::{B064K, Seq0255, Sv2Option},
        mining_sv2::{
            NewExtendedMiningJob, OpenExtendedMiningChannelSuccess, SetExtranoncePrefix,
            SetNewPrevHash as SetNewPrevHashMp,
        },
    };

    use crate::types::HashRate;

    use super::super::Extranonce2;
    use super::*;

    // ---- derive_extranonce_counter_size ----

    #[test]
    fn extranonce_counter_size_typical() {
        // Pool's extranonce_size = 4 bytes → counter width 4.
        assert_eq!(
            StratumV2Source::derive_extranonce_counter_size(4).unwrap(),
            4
        );
    }

    #[test]
    fn extranonce_counter_size_valid_boundary() {
        // Pool's extranonce_size = 8 bytes → counter width 8 (maximum).
        assert_eq!(
            StratumV2Source::derive_extranonce_counter_size(8).unwrap(),
            8
        );

        // Pool's extranonce_size = 1 byte → counter width 1 (minimum).
        assert_eq!(
            StratumV2Source::derive_extranonce_counter_size(1).unwrap(),
            1
        );
    }

    #[test]
    fn extranonce_counter_size_zero_rejected() {
        let err = StratumV2Source::derive_extranonce_counter_size(0).unwrap_err();
        assert!(err.to_string().contains("out of range"));
    }

    #[test]
    fn extranonce_counter_size_capped_when_pool_allocates_more_than_8() {
        // Pool's extranonce_size = 16 bytes; counter is capped at 8, remainder zero-padded.
        assert_eq!(
            StratumV2Source::derive_extranonce_counter_size(16).unwrap(),
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

    /// Builds a source wired to loose channels, with no client task behind
    /// them, so `serve_events` can be driven directly.
    fn make_source() -> (
        StratumV2Source,
        mpsc::Sender<SourceCommand>,
        mpsc::Receiver<SourceEvent>,
        CancellationToken,
    ) {
        let config = PoolConfig::new(
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
        .unwrap();

        let (command_tx, command_rx) = mpsc::channel(10);
        let (event_tx, event_rx) = mpsc::channel(10);
        let shutdown = CancellationToken::new();
        let source = StratumV2Source::new(config, command_rx, event_tx, shutdown.clone());
        (source, command_tx, event_rx, shutdown)
    }

    /// Contract: a pool that holds the socket open but sends no work for
    /// MAX_JOB_GAP is treated as dead, so the reconnect back-off is reachable.
    #[tokio::test(start_paused = true)]
    async fn silent_pool_trips_the_job_gap() {
        let (mut source, _command_tx, _event_rx, _shutdown) = make_source();
        // Held open, so the loop never sees the client exit.
        let (_client_event_tx, mut client_event_rx) = mpsc::channel::<ClientEvent>(10);
        let (client_command_tx, _client_command_rx) = mpsc::channel::<ClientCommand>(10);

        tokio::time::advance(MAX_JOB_GAP + Duration::from_secs(1)).await;
        let outcome = source
            .serve_events(&mut client_event_rx, &client_command_tx)
            .await;

        assert!(matches!(outcome, ServeOutcome::JobGap));
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
            .send(ClientEvent::OpenExtendedMiningChannelSuccess(
                channel_open_with_extranonce_size(0),
            ))
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

    /// Contract: a cancelled shutdown token stops the loop with Shutdown, in
    /// preference to the job-gap deadline.
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

    /// Builds an `OpenExtendedMiningChannelSuccess` carrying the given
    /// extranonce allocation.
    fn channel_open_with_extranonce_size(
        extranonce_size: u16,
    ) -> OpenExtendedMiningChannelSuccess<'static> {
        OpenExtendedMiningChannelSuccess {
            request_id: 1,
            channel_id: 1,
            target: U256::from(Target::MAX.to_le_bytes()),
            extranonce_size,
            extranonce_prefix: B032::try_from(vec![0xde, 0xad]).unwrap().into_static(),
            group_channel_id: 0,
        }
    }

    /// Contract: a job whose coinbase prefix is too short to carry a BIP141
    /// marker is rejected at this boundary.  channels_sv2 indexes bytes 4 and
    /// 5 of that prefix unchecked, so letting it through panics the client
    /// task and takes the daemon down with it.
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
            err.to_string().contains("coinbase_tx_prefix"),
            "error should name the offending field, got: {err}"
        );
    }

    /// Contract: a SetExtranoncePrefix applies to jobs received after it, so
    /// their templates carry the new prefix into the coinbase.
    #[tokio::test]
    async fn set_extranonce_prefix_applies_to_later_jobs() {
        let (mut source, _command_tx, mut event_rx, _shutdown) = make_source();

        // Give the channel a chain tip, so a non-future job is emitted as
        // soon as it arrives.
        let mut session = make_session();
        session
            .channel
            .on_new_extended_mining_job(make_job(
                7,
                0x2000_0000,
                false,
                None, // future
                vec![0x01; 8],
                vec![0x02; 8],
                vec![],
            ))
            .expect("future job accepted");
        session
            .channel
            .on_set_new_prev_hash(SetNewPrevHashMp {
                channel_id: 1,
                job_id: 7,
                prev_hash: U256::from([0u8; 32]),
                min_ntime: 0,
                nbits: 0x1d00_ffff,
            })
            .expect("prev hash activates the future job");
        source.session = Some(session);

        source
            .handle_client_event(ClientEvent::SetExtranoncePrefix(SetExtranoncePrefix {
                channel_id: 1,
                extranonce_prefix: B032::try_from(vec![0xbe, 0xef]).unwrap().into_static(),
            }))
            .await
            .expect("SetExtranoncePrefix is handled");
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
            .expect("job is handled");

        let SourceEvent::ReplaceJob(template) = event_rx.try_recv().expect("a job was emitted")
        else {
            panic!("expected ReplaceJob");
        };
        let MerkleRootKind::Computed(mrt) = template.merkle_root else {
            panic!("expected MerkleRootKind::Computed");
        };
        assert_eq!(
            mrt.extranonce1,
            vec![0xbe, 0xef],
            "a job received after SetExtranoncePrefix must use the new prefix"
        );
    }

    /// Contract: a SetExtranoncePrefix addressed to the group channel leaves
    /// the channel's prefix unchanged, since the prefix belongs to a single
    /// channel.
    #[tokio::test]
    async fn set_extranonce_prefix_for_group_channel_is_ignored() {
        let (mut source, _command_tx, _event_rx, _shutdown) = make_source();
        source.session = Some(SessionState {
            group_channel_id: 9,
            ..make_session()
        });

        source
            .handle_client_event(ClientEvent::SetExtranoncePrefix(SetExtranoncePrefix {
                channel_id: 9,
                extranonce_prefix: B032::try_from(vec![0xbe, 0xef]).unwrap().into_static(),
            }))
            .await
            .expect("SetExtranoncePrefix is handled");

        let session = source.session.as_ref().expect("session is kept");
        assert_eq!(
            session.channel.get_extranonce_prefix(),
            [0xde, 0xad].as_slice(),
            "a group-addressed SetExtranoncePrefix must not change the prefix"
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
        );
        SessionState {
            channel_id: 1,
            group_channel_id: 0,
            extranonce_counter_size: 4,
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

    /// Constructs a `NewExtendedMiningJob<'static>` with the given parameters.
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

    /// Constructs a [`ChainTip`] with a zeroed prev_hash.
    ///
    /// `min_ntime` and `nbits` are taken from the arguments.
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

    /// Contract: when extranonce_size == extranonce_counter_size (the typical case),
    /// extranonce2_size equals extranonce2_range.size in the resulting template.
    #[test]
    fn build_job_template_extranonce_size_equals_counter_when_not_padded() {
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
            "extranonce_size and counter width must match when the pool does not over-allocate"
        );
    }
}
