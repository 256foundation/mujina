//! Stratum v1 job source implementation.
//!
//! This module integrates the Stratum v1 client into mujina-miner's job source
//! abstraction. It handles the conversion between Stratum protocol messages and
//! the internal JobTemplate/Share types used by the scheduler.

use std::future;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio::time::{self, Instant};
use tokio_util::sync::CancellationToken;

use crate::stratum_v1::{
    ClientCommand, ClientEvent, Connector, JobNotification, PoolConfig, StratumV1Client,
};
use crate::tracing::prelude::*;
use crate::types::{Difficulty, HashRate, ShareRate};

use super::connection::{ConnectOutcome, ExponentialBackoff};
use super::{
    Extranonce2Range, GeneralPurposeBits, JobTemplate, MerkleRootKind, MerkleRootTemplate, Share,
    SourceCommand, SourceEvent, VersionTemplate,
};

/// Target share rate for suggest_difficulty: 20 shares/min (one every 3 sec).
const SUGGESTED_SHARE_RATE: ShareRate = ShareRate::from_interval(Duration::from_secs(3));

/// Downward deadband. Re-suggest when the new difficulty falls to or below the
/// last suggestion divided by this. Small, so we react quickly to a drop:
/// below the floor the pool can't lower difficulty on its own, and a drop in
/// board hashrate is a real change, not luck. A starting value, tune with use.
const DOWN_FACTOR: f64 = 1.25;

/// Upward deadband. Re-suggest when the new difficulty rises to or above the
/// last suggestion times this. Larger than the downward factor because the
/// pool raises difficulty on its own, but below 2.0 so adding a second
/// identical board still registers. A starting value, tune with use.
const UP_FACTOR: f64 = 1.5;

/// Minimum time between suggest_difficulty messages. A material change during
/// the cooldown is held and flushed once when it expires, so a burst of changes
/// costs at most one message per interval. A starting value, tune with use.
const SUGGEST_MIN_INTERVAL: Duration = Duration::from_secs(5);

/// Minimum connection duration before backoff resets on disconnect.
///
/// If a connection was alive for at least this long, the next reconnect
/// starts at the initial backoff (1 s). Shorter connections leave the
/// backoff untouched, preventing tight reconnection loops against a
/// flapping pool that accepts and immediately drops.
const STABLE_CONNECTION_THRESHOLD: Duration = Duration::from_secs(60);

/// Stratum v1 job source.
///
/// Wraps a StratumV1Client and bridges between the Stratum protocol and
/// mujina-miner's job source abstraction. Converts incoming mining.notify
/// messages to JobTemplates and outgoing Share submissions to Stratum format.
pub struct StratumV1Source {
    /// Pool configuration
    config: PoolConfig,

    /// Where to send events to scheduler
    event_tx: mpsc::Sender<SourceEvent>,

    /// Where to receive commands from scheduler
    command_rx: mpsc::Receiver<SourceCommand>,

    /// Shutdown signal
    shutdown: CancellationToken,

    /// Protocol state from subscription
    state: Option<ProtocolState>,

    /// Track if first accepted share has been logged
    first_share_logged: bool,

    /// Expected hashrate (an estimate, not a measurement)
    expected_hashrate: HashRate,

    /// Last difficulty we suggested to the pool (for material-change detection)
    last_suggested_difficulty: Option<f64>,

    /// When the suggest_difficulty cooldown expires, `None` when not throttling.
    cooldown_until: Option<Instant>,

    /// Factory for creating transport connections.
    connector: Box<dyn Connector>,
}

/// Protocol state after successful subscription.
#[derive(Debug, Clone)]
struct ProtocolState {
    /// Extranonce1 from mining.subscribe
    extranonce1: Vec<u8>,

    /// Extranonce2 size in bytes
    extranonce2_size: usize,

    /// Current share difficulty (from mining.set_difficulty)
    share_difficulty: Option<Difficulty>,

    /// Authorized version mask (from mining.configure or mining.set_version_mask)
    version_mask: Option<u32>,
}

impl StratumV1Source {
    /// Create a new Stratum v1 source.
    pub fn new(
        config: PoolConfig,
        command_rx: mpsc::Receiver<SourceCommand>,
        event_tx: mpsc::Sender<SourceEvent>,
        shutdown: CancellationToken,
        connector: Box<dyn Connector>,
    ) -> Self {
        Self {
            config,
            event_tx,
            command_rx,
            shutdown,
            state: None,
            first_share_logged: false,
            expected_hashrate: HashRate::default(),
            last_suggested_difficulty: None,
            cooldown_until: None,
            connector,
        }
    }

    /// Human-readable name derived from pool URL (e.g., "solo.ckpool.org:3333").
    pub fn name(&self) -> String {
        self.config
            .url
            .strip_prefix("stratum+tcp://")
            .or_else(|| self.config.url.strip_prefix("stratum://"))
            .or_else(|| self.config.url.strip_prefix("tcp://"))
            .unwrap_or(&self.config.url)
            .to_string()
    }

    /// Convert Stratum JobNotification to JobTemplate.
    fn job_to_template(&self, job: JobNotification) -> Result<JobTemplate> {
        let state = self
            .state
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No protocol state (not subscribed)"))?;

        // Create extranonce2 range (full range for the given size)
        let extranonce2_range = Extranonce2Range::new(state.extranonce2_size as u8)?;

        // Convert version to VersionTemplate
        // Use authorized mask from pool (or none if pool didn't authorize version rolling)
        let gp_bits_mask = state
            .version_mask
            .map(|mask| GeneralPurposeBits::from(&mask.to_be_bytes()))
            .unwrap_or_else(GeneralPurposeBits::none);

        let version_template = VersionTemplate::new(job.version, gp_bits_mask)?;

        // Use pool's share difficulty directly (scheduler handles rate limiting)
        let share_difficulty = state.share_difficulty.unwrap_or(Difficulty::from(1));
        let share_target = share_difficulty.to_target();

        Ok(JobTemplate {
            id: job.job_id,
            prev_blockhash: job.prev_hash,
            version: version_template,
            bits: job.nbits,
            share_target,
            time: job.ntime,
            merkle_root: MerkleRootKind::Computed(MerkleRootTemplate {
                coinbase1: job.coinbase1,
                extranonce1: state.extranonce1.clone(),
                extranonce2_range,
                coinbase2: job.coinbase2,
                merkle_branches: job.merkle_branches,
            }),
        })
    }

    /// Handle a client event.
    async fn handle_client_event(&mut self, event: ClientEvent) -> Result<()> {
        match event {
            ClientEvent::VersionRollingConfigured { authorized_mask } => {
                if let Some(mask) = authorized_mask {
                    debug!(
                        mask = format!("{:#x}", mask),
                        "Version rolling authorized by pool"
                    );
                } else {
                    debug!("Pool doesn't support version rolling");
                }

                // Store the mask (or lack thereof)
                if let Some(state) = &mut self.state {
                    state.version_mask = authorized_mask;
                } else {
                    // Configure happens before subscribe, so state might not exist yet
                    // Create temporary state that will be updated by Subscribed event
                    self.state = Some(ProtocolState {
                        extranonce1: Vec::new(),
                        extranonce2_size: 0,
                        share_difficulty: None,
                        version_mask: authorized_mask,
                    });
                }
            }

            ClientEvent::Subscribed {
                extranonce1,
                extranonce2_size,
            } => {
                info!(
                    pool = %self.config.url,
                    user = %self.config.username,
                    "Subscribed."
                );

                // Update or create protocol state
                // Preserve version_mask if already set by VersionRollingConfigured
                if let Some(state) = &mut self.state {
                    state.extranonce1 = extranonce1;
                    state.extranonce2_size = extranonce2_size;
                } else {
                    self.state = Some(ProtocolState {
                        extranonce1,
                        extranonce2_size,
                        share_difficulty: None,
                        version_mask: None,
                    });
                }
            }

            ClientEvent::NewJob(job) => {
                debug!(job_id = %job.job_id, clean_jobs = job.clean_jobs, "Received job from pool");

                let clean_jobs = job.clean_jobs;
                let template = self.job_to_template(job)?;
                let event = if clean_jobs {
                    SourceEvent::ReplaceJob(template)
                } else {
                    SourceEvent::UpdateJob(template)
                };

                self.event_tx.send(event).await?;
            }

            ClientEvent::DifficultyChanged(diff) => {
                let difficulty = Difficulty::from_f64(diff);
                debug!(difficulty = %difficulty, "Pool difficulty changed");
                if let Some(state) = &mut self.state {
                    state.share_difficulty = Some(difficulty);
                }
            }

            ClientEvent::VersionMaskSet(mask) => {
                info!(mask = format!("{:#010x}", mask), "Version mask set");
                if let Some(state) = &mut self.state {
                    state.version_mask = Some(mask);
                }
            }

            ClientEvent::ShareAccepted { job_id, nonce } => {
                if !self.first_share_logged {
                    self.first_share_logged = true;
                    info!(
                        pool = %self.config.url,
                        user = %self.config.username,
                        nonce = format!("{:#x}", nonce),
                        job_id = %job_id,
                        "First share accepted."
                    );
                } else {
                    debug!(
                        pool = %self.config.url,
                        user = %self.config.username,
                        nonce = format!("{:#x}", nonce),
                        job_id = %job_id,
                        "Share accepted."
                    );
                }
            }

            ClientEvent::ShareRejected { job_id, reason } => {
                warn!(job_id = %job_id, reason = %reason, "Share rejected by pool");
            }

            ClientEvent::Disconnected => {
                warn!("Disconnected from pool");
                // ClearJobs is sent by the reconnection loop after
                // connect_and_run() returns, covering both this path
                // and I/O errors where StratumV1Client exits without
                // sending Disconnected.
            }

            ClientEvent::Error(err) => {
                warn!(error = %err, "Pool error");
            }
        }

        Ok(())
    }

    /// Convert Share to SubmitParams.
    fn share_to_submit_params(&self, share: Share) -> Result<crate::stratum_v1::SubmitParams> {
        let state = self
            .state
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No protocol state (not subscribed)"))?;

        // Extract extranonce2 from share (if present)
        let extranonce2 = share
            .extranonce2
            .map(Vec::from)
            .unwrap_or_else(|| vec![0; state.extranonce2_size]);

        // Extract version bits if version rolling was authorized
        // Always include version_bits parameter when pool authorized rolling,
        // even if the value is 0x00000000 (pool requires the field)
        let version_bits = state.version_mask.map(|mask| {
            let rolled = share.version.to_consensus() as u32;
            rolled & mask
        });

        Ok(crate::stratum_v1::SubmitParams {
            username: self.config.username.clone(),
            job_id: share.job_id,
            extranonce2,
            ntime: share.time,
            nonce: share.nonce,
            version_bits,
        })
    }

    /// Compute the suggested difficulty for the given hashrate.
    ///
    /// Returns `None` for zero hashrate (nothing to suggest yet). The value is
    /// fractional below 1, so a very slow worker (such as the CPU miner) still
    /// gets a target it can meet; the client encodes whole difficulties as
    /// integers on the wire.
    fn compute_suggested_difficulty(hashrate: HashRate) -> Option<f64> {
        if hashrate.is_zero() {
            return None;
        }
        let target = SUGGESTED_SHARE_RATE.to_target(hashrate);
        Some(Difficulty::from_target(target).as_f64())
    }

    /// Whether `new_diff` differs enough from the last suggestion to re-suggest.
    ///
    /// The deadband is asymmetric, measured from the last suggestion (which the
    /// pool holds as a difficulty floor) using the down and up factors. Any
    /// value is material when there is no prior suggestion.
    ///
    /// The baseline is deliberately our last suggestion, not the pool's live
    /// set difficulty. Measuring against the live value would fight the pool's
    /// vardiff and oscillate, re-suggesting against a change the pool just made
    /// above the floor on its own.
    fn is_material_change(new_diff: f64, last_suggested: Option<f64>) -> bool {
        let Some(floor) = last_suggested else {
            return true;
        };
        new_diff <= floor / DOWN_FACTOR || new_diff >= floor * UP_FACTOR
    }

    /// Suggest difficulty for the latest hashrate, subject to the deadband and
    /// the cooldown.
    ///
    /// A material change during the cooldown is held; `flush_suggest` sends it
    /// once the cooldown expires, so a burst collapses to one message per
    /// interval.
    async fn maybe_suggest_difficulty(&mut self, client_command_tx: &mpsc::Sender<ClientCommand>) {
        let Some(new_diff) = Self::compute_suggested_difficulty(self.expected_hashrate) else {
            return;
        };
        if !Self::is_material_change(new_diff, self.last_suggested_difficulty) {
            return;
        }
        if self.in_cooldown(Instant::now()) {
            return;
        }
        self.send_suggest(new_diff, client_command_tx).await;
    }

    /// Re-test the latest hashrate when the cooldown expires, sending a still-
    /// material suggestion or else ending the throttle.
    async fn flush_suggest(&mut self, client_command_tx: &mpsc::Sender<ClientCommand>) {
        let pending = Self::compute_suggested_difficulty(self.expected_hashrate)
            .filter(|diff| Self::is_material_change(*diff, self.last_suggested_difficulty));
        match pending {
            Some(diff) => self.send_suggest(diff, client_command_tx).await,
            None => self.cooldown_until = None,
        }
    }

    /// Send a `SuggestDifficulty`, record it as the new floor, and arm the
    /// cooldown.
    async fn send_suggest(
        &mut self,
        new_diff: f64,
        client_command_tx: &mpsc::Sender<ClientCommand>,
    ) {
        debug!(
            difficulty = new_diff,
            hashrate = %self.expected_hashrate,
            "Suggesting difficulty to pool"
        );
        self.last_suggested_difficulty = Some(new_diff);
        self.cooldown_until = Some(Instant::now() + SUGGEST_MIN_INTERVAL);

        if let Err(e) = client_command_tx
            .send(ClientCommand::SuggestDifficulty(new_diff))
            .await
        {
            warn!(error = %e, "Failed to send suggest_difficulty to client");
        }
    }

    /// Whether the suggest cooldown is still active at `now`.
    fn in_cooldown(&self, now: Instant) -> bool {
        self.cooldown_until.is_some_and(|until| now < until)
    }

    /// Run the source (main event loop).
    ///
    /// Defers pool connection until the scheduler reports a positive hashrate
    /// via `UpdateHashRate`---no point fetching jobs if there are no hash
    /// threads to work them. A secondary benefit: `suggest_difficulty` always
    /// has a meaningful value at connect time. Reconnects automatically with
    /// exponential backoff when the connection drops.
    pub async fn run(mut self) -> Result<()> {
        info!(pool = %self.config.url, "Waiting for hash threads before connecting");

        // Phase 1: wait until hash threads are available before connecting.
        // Drain commands; only UpdateHashRate matters here.
        loop {
            tokio::select! {
                Some(cmd) = self.command_rx.recv() => {
                    match cmd {
                        SourceCommand::UpdateHashRate(rate) => {
                            self.expected_hashrate = rate;
                            if !rate.is_zero() {
                                break;
                            }
                        }
                        SourceCommand::SubmitShare(_) => {
                            // No connection yet, drop silently.
                        }
                    }
                }
                _ = self.shutdown.cancelled() => {
                    return Ok(());
                }
            }
        }

        // Phase 2: connect with automatic reconnection.
        let mut backoff = ExponentialBackoff::new(Duration::from_secs(1), Duration::from_secs(60));

        loop {
            // Reset per-connection state so a fresh handshake starts clean.
            self.state = None;
            self.first_share_logged = false;

            info!(pool = %self.config.url, "Connecting to pool");

            let connected_at = Instant::now();
            match self.connect_and_run().await {
                ConnectOutcome::Shutdown => return Ok(()),
                ConnectOutcome::Fatal(e) => {
                    error!(error = %e, "Fatal pool error, not reconnecting");
                    return Err(e);
                }
                ConnectOutcome::Disconnected => {
                    // Invalidate stale work from the dead connection.
                    if let Err(e) = self.event_tx.send(SourceEvent::ClearJobs).await {
                        warn!(error = %e, "Failed to send ClearJobs");
                    }
                    if connected_at.elapsed() >= STABLE_CONNECTION_THRESHOLD {
                        backoff.reset();
                    }
                    let delay = backoff.next_delay();
                    info!(
                        pool = %self.config.url,
                        delay_secs = delay.as_secs_f64(),
                        "Reconnecting after backoff"
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
    /// Creates channels, spawns the client task, runs the event loop
    /// until disconnect or shutdown, and returns the outcome.
    async fn connect_and_run(&mut self) -> ConnectOutcome {
        let (client_event_tx, mut client_event_rx) = mpsc::channel(100);
        let (client_command_tx, client_command_rx) = mpsc::channel(100);

        // Compute initial difficulty so the client can send it inline
        // during the handshake, before the first job arrives.
        let initial_difficulty = Self::compute_suggested_difficulty(self.expected_hashrate);
        self.last_suggested_difficulty = initial_difficulty;
        self.cooldown_until = None;

        let client = StratumV1Client::with_commands(
            self.config.clone(),
            client_event_tx,
            client_command_rx,
            self.shutdown.clone(),
            initial_difficulty,
        );

        let transport = tokio::select! {
            result = self.connector.connect() => {
                match result {
                    Ok(t) => t,
                    Err(e) => {
                        if e.is_fatal() {
                            return ConnectOutcome::Fatal(e.into());
                        }
                        warn!(error = %e, "Failed to connect");
                        return ConnectOutcome::Disconnected;
                    }
                }
            }
            _ = self.shutdown.cancelled() => {
                return ConnectOutcome::Shutdown;
            }
        };

        let client_handle = tokio::spawn(async move { client.run_with_transport(transport).await });

        // Main event loop
        loop {
            // Copied out so the cooldown branch captures the value, not `self`.
            let cooldown_until = self.cooldown_until;
            tokio::select! {
                event_opt = client_event_rx.recv() => {
                    match event_opt {
                        Some(event) => {
                            if let Err(e) = self.handle_client_event(event).await {
                                warn!(error = %e, "Error handling client event");
                            }
                        }
                        None => {
                            // Client task exited; check why below.
                            break;
                        }
                    }
                }

                Some(cmd) = self.command_rx.recv() => {
                    match cmd {
                        SourceCommand::SubmitShare(share) => {
                            debug!(
                                pool = %self.name(),
                                job_id = %share.job_id,
                                nonce = format!("{:#x}", share.nonce),
                                "Submitting share"
                            );

                            match self.share_to_submit_params(share) {
                                Ok(submit_params) => {
                                    if let Err(e) = client_command_tx.send(
                                        ClientCommand::SubmitShare(submit_params)
                                    ).await {
                                        warn!(error = %e, "Failed to send share to client");
                                    }
                                }
                                Err(e) => {
                                    warn!(error = %e, "Failed to convert share");
                                }
                            }
                        }

                        SourceCommand::UpdateHashRate(rate) => {
                            self.expected_hashrate = rate;
                            self.maybe_suggest_difficulty(&client_command_tx).await;
                        }
                    }
                }

                // Cooldown expiry: flush a suggestion held during the cooldown.
                _ = async {
                    match cooldown_until {
                        Some(until) => time::sleep_until(until).await,
                        None => future::pending().await,
                    }
                }, if cooldown_until.is_some() => {
                    self.flush_suggest(&client_command_tx).await;
                }

                _ = self.shutdown.cancelled() => {
                    return ConnectOutcome::Shutdown;
                }
            }
        }

        // Client task exited -- determine outcome from its return value.
        match client_handle.await {
            Ok(Ok(())) => ConnectOutcome::Shutdown,
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

    /// Wait for the given duration, draining commands in the meantime.
    ///
    /// Returns `true` if shutdown was requested during the wait.
    async fn backoff_wait(&mut self, delay: Duration) -> bool {
        let sleep = tokio::time::sleep(delay);
        tokio::pin!(sleep);

        loop {
            tokio::select! {
                _ = &mut sleep => {
                    return false;
                }
                Some(cmd) = self.command_rx.recv() => {
                    match cmd {
                        SourceCommand::UpdateHashRate(rate) => {
                            self.expected_hashrate = rate;
                        }
                        SourceCommand::SubmitShare(_) => {
                            // No connection, drop silently.
                        }
                    }
                }
                _ = self.shutdown.cancelled() => {
                    return true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asic::bm13xx::test_data::esp_miner_job::{
        POOL_SHARE_DIFFICULTY_INT, STRATUM_EXTRANONCE1, STRATUM_EXTRANONCE2_SIZE, VERSION_MASK,
        notify, submit,
    };
    use crate::asic::bm13xx::test_data::stratum_json;
    use crate::job_source::Extranonce2;
    use crate::stratum_v1::{
        JobNotification, JsonRpcMessage, MockConnector, MockTransport, MockTransportHandle,
        StratumResult, Transport,
    };
    use bitcoin::block::Version;
    use serde_json::json;

    /// Connector that panics if called. For tests that never reach connect().
    struct NeverConnector;

    #[async_trait::async_trait]
    impl Connector for NeverConnector {
        async fn connect(&mut self) -> StratumResult<Box<dyn Transport>> {
            unreachable!("NeverConnector::connect() should not be called");
        }
    }

    /// Helper to convert extranonce2 bytes to Extranonce2 type.
    fn extranonce2_from_bytes(bytes: &[u8]) -> Extranonce2 {
        let size = bytes.len() as u8;
        // Pad to 8 bytes for u64 conversion (little-endian)
        let mut padded = [0u8; 8];
        padded[..bytes.len()].copy_from_slice(bytes);
        let value = u64::from_le_bytes(padded);
        Extranonce2::new(value, size).unwrap()
    }

    /// Helper to create a StratumV1Source with protocol state for testing.
    fn source_with_state(
        extranonce1: Vec<u8>,
        extranonce2_size: usize,
        share_difficulty: Option<u64>,
        version_mask: Option<u32>,
    ) -> StratumV1Source {
        let (event_tx, _event_rx) = mpsc::channel(10);
        let (_command_tx, command_rx) = mpsc::channel(10);
        let shutdown = CancellationToken::new();

        let config = PoolConfig {
            url: "stratum+tcp://test:3333".to_string(),
            username: "testworker".to_string(),
            password: "x".to_string(),
            user_agent: "test".to_string(),
            ..Default::default()
        };

        let mut source = StratumV1Source::new(
            config,
            command_rx,
            event_tx,
            shutdown,
            Box::new(NeverConnector),
        );

        source.state = Some(ProtocolState {
            extranonce1,
            extranonce2_size,
            share_difficulty: share_difficulty.map(Difficulty::from),
            version_mask,
        });

        source
    }

    /// Test job_to_template with real capture data from esp-miner.
    ///
    /// Uses the Bitaxe Gamma capture that produced an accepted share at
    /// difficulty 29588. Validates that Stratum JobNotification converts
    /// correctly to internal JobTemplate.
    #[test]
    fn test_job_to_template_with_capture_data() {
        // Create source with protocol state matching the capture
        let extranonce1 = hex::decode(STRATUM_EXTRANONCE1).unwrap();
        let source = source_with_state(
            extranonce1.clone(),
            STRATUM_EXTRANONCE2_SIZE,
            Some(POOL_SHARE_DIFFICULTY_INT),
            Some(VERSION_MASK),
        );

        // Parse the actual wire message
        let json: serde_json::Value = serde_json::from_str(stratum_json::MINING_NOTIFY)
            .expect("Failed to parse MINING_NOTIFY JSON");
        let params = json["params"].as_array().expect("params not an array");

        let job =
            JobNotification::from_stratum_params(params).expect("Failed to parse capture data");

        // Convert to JobTemplate
        let template = source.job_to_template(job).expect("job_to_template failed");

        // Validate job ID preserved
        assert_eq!(template.id, notify::JOB_ID_STRING);

        // Validate prev_blockhash matches wire capture
        assert_eq!(
            template.prev_blockhash,
            *notify::PREV_BLOCKHASH,
            "prev_blockhash mismatch"
        );

        // Validate version template has correct base and mask
        assert_eq!(
            template.version.base(),
            *notify::VERSION,
            "Base version mismatch"
        );

        // Version mask should allow GP bits (bits 13-28)
        let gp_mask = template.version.gp_bits_mask();
        assert_eq!(
            gp_mask.as_bytes(),
            &[0xff, 0xff],
            "GP bits mask should be full (0xffff) for mask 0x1fffe000"
        );

        // Validate bits (network difficulty)
        assert_eq!(template.bits, *notify::NBITS, "nbits mismatch");

        // Validate timestamp
        assert_eq!(template.time, *notify::NTIME, "ntime mismatch");

        // Validate merkle root template
        match &template.merkle_root {
            MerkleRootKind::Computed(mrt) => {
                assert_eq!(
                    mrt.coinbase1,
                    hex::decode(notify::COINBASE1).unwrap(),
                    "coinbase1 mismatch"
                );
                assert_eq!(mrt.extranonce1, extranonce1, "extranonce1 mismatch");
                assert_eq!(
                    mrt.coinbase2,
                    hex::decode(notify::COINBASE2).unwrap(),
                    "coinbase2 mismatch"
                );
                assert_eq!(
                    mrt.merkle_branches.len(),
                    12,
                    "Wrong number of merkle branches"
                );
            }
            MerkleRootKind::Fixed(_) => panic!("Expected Computed merkle root"),
        }

        // Validate share target was computed from difficulty
        // At difficulty 8192, the target should allow shares easier than network difficulty
        let share_difficulty_float = template.share_target.difficulty_float();
        assert!(
            (share_difficulty_float - POOL_SHARE_DIFFICULTY_INT as f64).abs() < 1.0,
            "Share target difficulty mismatch: got {}, expected {}",
            share_difficulty_float,
            POOL_SHARE_DIFFICULTY_INT
        );
    }

    /// Test job_to_template without version rolling support.
    ///
    /// When pool doesn't authorize version rolling, the version_mask should
    /// be None and the GP bits mask should be zero (no rolling allowed).
    #[test]
    fn test_job_to_template_without_version_rolling() {
        let extranonce1 = hex::decode(STRATUM_EXTRANONCE1).unwrap();
        let source = source_with_state(
            extranonce1,
            STRATUM_EXTRANONCE2_SIZE,
            Some(POOL_SHARE_DIFFICULTY_INT),
            None, // No version rolling
        );

        let params = json!([
            "jobid",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "aa",
            "bb",
            [],
            "20000000",
            "1d00ffff",
            "5a5a5a5a",
            false
        ]);

        let job = JobNotification::from_stratum_params(params.as_array().unwrap()).unwrap();
        let template = source.job_to_template(job).unwrap();

        // GP bits mask should be zero when no version rolling
        let gp_mask = template.version.gp_bits_mask();
        assert_eq!(
            gp_mask.as_bytes(),
            &[0x00, 0x00],
            "GP bits mask should be zero without version rolling"
        );
    }

    /// Test job_to_template uses default difficulty when not set.
    #[test]
    fn test_job_to_template_default_difficulty() {
        let extranonce1 = hex::decode(STRATUM_EXTRANONCE1).unwrap();
        let source = source_with_state(
            extranonce1,
            STRATUM_EXTRANONCE2_SIZE,
            None, // No difficulty set yet
            Some(VERSION_MASK),
        );

        let params = json!([
            "jobid",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "aa",
            "bb",
            [],
            "20000000",
            "1d00ffff",
            "5a5a5a5a",
            false
        ]);

        let job = JobNotification::from_stratum_params(params.as_array().unwrap()).unwrap();
        let template = source.job_to_template(job).unwrap();

        // Should default to difficulty 1 (Target::MAX)
        let share_difficulty_float = template.share_target.difficulty_float();
        assert!(
            share_difficulty_float < 2.0,
            "Default difficulty should be ~1, got {}",
            share_difficulty_float
        );
    }

    /// Test share_to_submit_params with real capture data.
    ///
    /// Converts the share found by the Bitaxe Gamma back to Stratum format
    /// and validates against the actual mining.submit from the capture.
    #[test]
    fn test_share_to_submit_params_with_capture_data() {
        let extranonce1 = hex::decode(STRATUM_EXTRANONCE1).unwrap();
        let source = source_with_state(
            extranonce1,
            STRATUM_EXTRANONCE2_SIZE,
            Some(POOL_SHARE_DIFFICULTY_INT),
            Some(VERSION_MASK),
        );

        // Build Share from capture data
        // The version in Share should be the FULL version (base | rolled bits)
        let full_version = Version::from_consensus(*submit::VERSION as i32 | 0x20000000);

        let share = Share {
            job_id: submit::JOB_ID_STRING.to_string(),
            nonce: *submit::NONCE,
            time: *submit::NTIME,
            version: full_version,
            extranonce2: Some(extranonce2_from_bytes(&*submit::EXTRANONCE2)),
        };

        // Convert to SubmitParams
        let params = source
            .share_to_submit_params(share)
            .expect("share_to_submit_params failed");

        // Validate username
        assert_eq!(params.username, "testworker");

        // Validate job ID
        assert_eq!(params.job_id, submit::JOB_ID_STRING);

        // Validate extranonce2
        assert_eq!(
            params.extranonce2,
            submit::EXTRANONCE2.to_vec(),
            "extranonce2 mismatch"
        );

        // Validate ntime
        assert_eq!(params.ntime, *submit::NTIME, "ntime mismatch");

        // Validate nonce
        assert_eq!(params.nonce, *submit::NONCE, "nonce mismatch");

        // Validate version_bits (should be rolled bits only, masked)
        // The capture shows version_bits = 0x00b44000
        assert_eq!(
            params.version_bits,
            Some(*submit::VERSION),
            "version_bits mismatch"
        );
    }

    /// Test share_to_submit_params without version rolling.
    ///
    /// When pool didn't authorize version rolling, version_bits should be None.
    #[test]
    fn test_share_to_submit_params_without_version_rolling() {
        let extranonce1 = hex::decode(STRATUM_EXTRANONCE1).unwrap();
        let source = source_with_state(
            extranonce1,
            STRATUM_EXTRANONCE2_SIZE,
            Some(POOL_SHARE_DIFFICULTY_INT),
            None, // No version rolling
        );

        let share = Share {
            job_id: "testjob".to_string(),
            nonce: 0x12345678,
            time: 0x65432100,
            version: Version::from_consensus(0x20000000),
            extranonce2: Some(extranonce2_from_bytes(&[0xde, 0xad, 0xbe, 0xef])),
        };

        let params = source.share_to_submit_params(share).unwrap();

        // version_bits should be None when no version rolling authorized
        assert_eq!(
            params.version_bits, None,
            "version_bits should be None without version rolling"
        );
    }

    /// Test share_to_submit_params uses default extranonce2 when not provided.
    #[test]
    fn test_share_to_submit_params_default_extranonce2() {
        let extranonce1 = hex::decode(STRATUM_EXTRANONCE1).unwrap();
        let source = source_with_state(
            extranonce1,
            STRATUM_EXTRANONCE2_SIZE,
            Some(POOL_SHARE_DIFFICULTY_INT),
            None,
        );

        let share = Share {
            job_id: "testjob".to_string(),
            nonce: 0x12345678,
            time: 0x65432100,
            version: Version::from_consensus(0x20000000),
            extranonce2: None, // Not provided
        };

        let params = source.share_to_submit_params(share).unwrap();

        // Should default to zeros with correct size
        assert_eq!(
            params.extranonce2,
            vec![0u8; STRATUM_EXTRANONCE2_SIZE],
            "Default extranonce2 should be zeros"
        );
    }

    /// Test SubmitParams serialization matches expected wire format.
    ///
    /// Validates the complete round-trip: Share → SubmitParams → JSON matches
    /// the actual mining.submit from the capture.
    #[test]
    fn test_submit_params_wire_format_matches_capture() {
        let extranonce1 = hex::decode(STRATUM_EXTRANONCE1).unwrap();
        let source = source_with_state(
            extranonce1,
            STRATUM_EXTRANONCE2_SIZE,
            Some(POOL_SHARE_DIFFICULTY_INT),
            Some(VERSION_MASK),
        );

        // Build Share from capture
        let full_version = Version::from_consensus(*submit::VERSION as i32 | 0x20000000);

        let share = Share {
            job_id: submit::JOB_ID_STRING.to_string(),
            nonce: *submit::NONCE,
            time: *submit::NTIME,
            version: full_version,
            extranonce2: Some(extranonce2_from_bytes(&*submit::EXTRANONCE2)),
        };

        // Convert to SubmitParams and then to JSON
        let params = source.share_to_submit_params(share).unwrap();
        let json = params.to_stratum_json();

        // Validate each field matches capture
        // params[0] = username (we use "testworker")
        // params[1] = job_id
        assert_eq!(json[1].as_str().unwrap(), submit::JOB_ID_STRING);

        // params[2] = extranonce2 (hex)
        assert_eq!(json[2].as_str().unwrap(), submit::EXTRANONCE2_STRING);

        // params[3] = ntime (hex)
        assert_eq!(json[3].as_str().unwrap(), submit::NTIME_STRING);

        // params[4] = nonce (hex)
        assert_eq!(json[4].as_str().unwrap(), submit::NONCE_STRING);

        // params[5] = version_bits (hex)
        assert_eq!(json[5].as_str().unwrap(), submit::VERSION_STRING);
    }

    /// Test merkle root computation through job template.
    ///
    /// Validates that the JobTemplate can compute the correct merkle root
    /// using the extranonce2 from the capture.
    #[test]
    fn test_job_template_merkle_root_computation() {
        let extranonce1 = hex::decode(STRATUM_EXTRANONCE1).unwrap();
        let source = source_with_state(
            extranonce1,
            STRATUM_EXTRANONCE2_SIZE,
            Some(POOL_SHARE_DIFFICULTY_INT),
            Some(VERSION_MASK),
        );

        // Parse the actual wire message
        let json: serde_json::Value = serde_json::from_str(stratum_json::MINING_NOTIFY)
            .expect("Failed to parse MINING_NOTIFY JSON");
        let params = json["params"].as_array().expect("params not an array");

        let job = JobNotification::from_stratum_params(params).unwrap();
        let template = source.job_to_template(job).unwrap();

        // Compute merkle root using the extranonce2 from the capture
        let en2 = extranonce2_from_bytes(&*submit::EXTRANONCE2);
        let merkle_root = template
            .compute_merkle_root(&en2)
            .expect("Merkle root computation failed");

        // This should match the merkle root in the wire capture
        assert_eq!(
            merkle_root,
            *notify::MERKLE_ROOT,
            "Computed merkle root doesn't match capture"
        );
    }

    #[test]
    fn test_compute_suggested_difficulty_zero_hashrate() {
        assert_eq!(
            StratumV1Source::compute_suggested_difficulty(HashRate::default()),
            None,
        );
    }

    #[test]
    fn test_compute_suggested_difficulty_bitaxe_gamma() {
        // ~1 TH/s at 20 shares/min (3-sec interval) should yield ~698
        let diff =
            StratumV1Source::compute_suggested_difficulty(HashRate::from_terahashes(1.0)).unwrap();
        assert!(
            (600.0..800.0).contains(&diff),
            "Bitaxe Gamma difficulty {diff} not in expected range 600..800"
        );
    }

    #[test]
    fn test_compute_suggested_difficulty_fractional_for_slow_worker() {
        // A very slow worker (e.g. the CPU miner) needs a fractional target it
        // can actually meet, not one rounded up to 1.
        let diff =
            StratumV1Source::compute_suggested_difficulty(HashRate::from_megahashes(1.0)).unwrap();
        assert!(
            diff > 0.0 && diff < 1.0,
            "slow-worker difficulty {diff} should be a fraction below 1"
        );
    }

    #[tokio::test]
    async fn test_maybe_suggest_difficulty_first_call_always_sends() {
        let (event_tx, _event_rx) = mpsc::channel(10);
        let (_command_tx, command_rx) = mpsc::channel(10);
        let shutdown = CancellationToken::new();
        let config = PoolConfig {
            url: "stratum+tcp://test:3333".to_string(),
            ..Default::default()
        };

        let mut source = StratumV1Source::new(
            config,
            command_rx,
            event_tx,
            shutdown,
            Box::new(NeverConnector),
        );
        source.expected_hashrate = HashRate::from_terahashes(1.0);

        let (client_tx, mut client_rx) = mpsc::channel(10);
        source.maybe_suggest_difficulty(&client_tx).await;

        let cmd = client_rx.try_recv().expect("should have sent command");
        match cmd {
            ClientCommand::SuggestDifficulty(d) => {
                assert!(d > 0.0, "difficulty should be positive");
            }
            other => panic!("expected SuggestDifficulty, got {other:?}"),
        }
        assert!(source.last_suggested_difficulty.is_some());
    }

    #[test]
    fn material_change_with_no_prior_suggestion() {
        assert!(StratumV1Source::is_material_change(100.0, None));
    }

    #[test]
    fn material_change_deadband_around_floor() {
        // Floor of 100: down fires at <= 80 (100 / 1.25), up at >= 150
        // (100 * 1.5); the band between is suppressed.
        let floor = Some(100.0);
        assert!(StratumV1Source::is_material_change(80.0, floor)); // down threshold
        assert!(!StratumV1Source::is_material_change(81.0, floor)); // just inside down
        assert!(!StratumV1Source::is_material_change(100.0, floor)); // unchanged
        assert!(!StratumV1Source::is_material_change(149.0, floor)); // just inside up
        assert!(StratumV1Source::is_material_change(150.0, floor)); // up threshold
        assert!(StratumV1Source::is_material_change(300.0, floor)); // well above
    }

    #[test]
    fn material_change_is_asymmetric() {
        // A 1.4x rise is suppressed (below the 1.5x up threshold) while a
        // matching 1.4x fall fires (past the 1.25x down threshold).
        let floor = Some(100.0);
        assert!(!StratumV1Source::is_material_change(140.0, floor));
        assert!(StratumV1Source::is_material_change(71.0, floor));
    }

    /// Build a source for testing the suggest policy in isolation. The other
    /// ends of its channels are dropped; the policy methods don't use them.
    fn throttle_test_source() -> StratumV1Source {
        let (event_tx, _event_rx) = mpsc::channel(10);
        let (_command_tx, command_rx) = mpsc::channel(10);
        let config = PoolConfig {
            url: "stratum+tcp://test:3333".to_string(),
            ..Default::default()
        };
        StratumV1Source::new(
            config,
            command_rx,
            event_tx,
            CancellationToken::new(),
            Box::new(NeverConnector),
        )
    }

    fn drain(rx: &mut mpsc::Receiver<ClientCommand>) -> usize {
        let mut count = 0;
        while rx.try_recv().is_ok() {
            count += 1;
        }
        count
    }

    #[tokio::test(start_paused = true)]
    async fn throttle_emits_first_change_immediately() {
        let mut source = throttle_test_source();
        let (client_tx, mut client_rx) = mpsc::channel(10);

        source.expected_hashrate = HashRate::from_terahashes(1.0);
        source.maybe_suggest_difficulty(&client_tx).await;
        assert_eq!(drain(&mut client_rx), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn throttle_holds_during_cooldown_then_flushes_once() {
        let mut source = throttle_test_source();
        let (client_tx, mut client_rx) = mpsc::channel(10);

        source.expected_hashrate = HashRate::from_terahashes(1.0);
        source.maybe_suggest_difficulty(&client_tx).await;
        assert_eq!(drain(&mut client_rx), 1);

        // A material change during the cooldown is held.
        source.expected_hashrate = HashRate::from_terahashes(4.0);
        source.maybe_suggest_difficulty(&client_tx).await;
        assert_eq!(drain(&mut client_rx), 0);

        // When the cooldown expires it flushes exactly once.
        time::advance(SUGGEST_MIN_INTERVAL).await;
        source.flush_suggest(&client_tx).await;
        assert_eq!(drain(&mut client_rx), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn throttle_subthreshold_oscillation_goes_quiet() {
        let mut source = throttle_test_source();
        let (client_tx, mut client_rx) = mpsc::channel(10);

        // Floor at 1 TH/s.
        source.expected_hashrate = HashRate::from_terahashes(1.0);
        source.maybe_suggest_difficulty(&client_tx).await;
        assert_eq!(drain(&mut client_rx), 1);

        // A material drop, held during the cooldown, flushes as one down-step.
        source.expected_hashrate = HashRate::from_terahashes(0.5);
        source.maybe_suggest_difficulty(&client_tx).await;
        time::advance(SUGGEST_MIN_INTERVAL).await;
        source.flush_suggest(&client_tx).await;
        assert_eq!(drain(&mut client_rx), 1);

        // Small oscillation around the new floor stays inside the deadband, so
        // the next flush sends nothing.
        for th in [0.52, 0.48, 0.55, 0.5] {
            source.expected_hashrate = HashRate::from_terahashes(th);
            source.maybe_suggest_difficulty(&client_tx).await;
        }
        time::advance(SUGGEST_MIN_INTERVAL).await;
        source.flush_suggest(&client_tx).await;
        assert_eq!(drain(&mut client_rx), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn throttle_sustained_flapping_bounded_per_interval() {
        let mut source = throttle_test_source();
        let (client_tx, mut client_rx) = mpsc::channel(10);

        source.expected_hashrate = HashRate::from_terahashes(1.0);
        source.maybe_suggest_difficulty(&client_tx).await;
        assert_eq!(drain(&mut client_rx), 1);

        // Each interval flaps wildly but settles on a value that is material
        // versus the prior suggestion; the flush emits at most once per
        // interval.
        for target in [5.0, 1.0, 5.0] {
            for th in [2.0, 8.0, 0.5] {
                source.expected_hashrate = HashRate::from_terahashes(th);
                source.maybe_suggest_difficulty(&client_tx).await;
            }
            source.expected_hashrate = HashRate::from_terahashes(target);
            source.maybe_suggest_difficulty(&client_tx).await;
            assert_eq!(drain(&mut client_rx), 0, "flaps during cooldown are held");

            time::advance(SUGGEST_MIN_INTERVAL).await;
            source.flush_suggest(&client_tx).await;
            assert_eq!(drain(&mut client_rx), 1, "one emit per interval");
        }
    }

    #[tokio::test]
    async fn test_maybe_suggest_difficulty_suppresses_small_changes() {
        let (event_tx, _event_rx) = mpsc::channel(10);
        let (_command_tx, command_rx) = mpsc::channel(10);
        let shutdown = CancellationToken::new();
        let config = PoolConfig {
            url: "stratum+tcp://test:3333".to_string(),
            ..Default::default()
        };

        let mut source = StratumV1Source::new(
            config,
            command_rx,
            event_tx,
            shutdown,
            Box::new(NeverConnector),
        );
        source.expected_hashrate = HashRate::from_terahashes(1.0);

        let (client_tx, mut client_rx) = mpsc::channel(10);

        // First call sends
        source.maybe_suggest_difficulty(&client_tx).await;
        let _ = client_rx.try_recv().unwrap();

        // Same hashrate again -- no material change, should not send
        source.maybe_suggest_difficulty(&client_tx).await;
        assert!(
            client_rx.try_recv().is_err(),
            "should not re-suggest for same hashrate"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_maybe_suggest_difficulty_sends_on_material_change() {
        let (event_tx, _event_rx) = mpsc::channel(10);
        let (_command_tx, command_rx) = mpsc::channel(10);
        let shutdown = CancellationToken::new();
        let config = PoolConfig {
            url: "stratum+tcp://test:3333".to_string(),
            ..Default::default()
        };

        let mut source = StratumV1Source::new(
            config,
            command_rx,
            event_tx,
            shutdown,
            Box::new(NeverConnector),
        );
        source.expected_hashrate = HashRate::from_terahashes(1.0);

        let (client_tx, mut client_rx) = mpsc::channel(10);

        // First call
        source.maybe_suggest_difficulty(&client_tx).await;
        let _ = client_rx.try_recv().unwrap();

        // Past the cooldown, a material rise re-suggests.
        time::advance(SUGGEST_MIN_INTERVAL).await;
        source.expected_hashrate = HashRate::from_terahashes(2.5);
        source.maybe_suggest_difficulty(&client_tx).await;
        assert!(
            client_rx.try_recv().is_ok(),
            "should re-suggest after a material hashrate change"
        );
    }

    #[tokio::test]
    async fn test_maybe_suggest_difficulty_skips_zero_hashrate() {
        let (event_tx, _event_rx) = mpsc::channel(10);
        let (_command_tx, command_rx) = mpsc::channel(10);
        let shutdown = CancellationToken::new();
        let config = PoolConfig {
            url: "stratum+tcp://test:3333".to_string(),
            ..Default::default()
        };

        let mut source = StratumV1Source::new(
            config,
            command_rx,
            event_tx,
            shutdown,
            Box::new(NeverConnector),
        );
        // hashrate is zero by default

        let (client_tx, mut client_rx) = mpsc::channel(10);
        source.maybe_suggest_difficulty(&client_tx).await;
        assert!(
            client_rx.try_recv().is_err(),
            "should not suggest with zero hashrate"
        );
    }

    #[test]
    fn backoff_doubles_each_step() {
        let mut backoff = ExponentialBackoff::new(Duration::from_secs(1), Duration::from_secs(60));

        let d1 = backoff.next_delay();
        let d2 = backoff.next_delay();
        let d3 = backoff.next_delay();

        // Nominal sequence is 1s, 2s, 4s. With jitter in [0.5, 1.0],
        // each delay is at least half the nominal value.
        assert!(d1 >= Duration::from_millis(500), "d1={d1:?}");
        assert!(d1 < Duration::from_secs(1), "d1={d1:?}");

        assert!(d2 >= Duration::from_secs(1), "d2={d2:?}");
        assert!(d2 < Duration::from_secs(2), "d2={d2:?}");

        assert!(d3 >= Duration::from_secs(2), "d3={d3:?}");
        assert!(d3 < Duration::from_secs(4), "d3={d3:?}");
    }

    #[test]
    fn backoff_caps_at_max() {
        let mut backoff = ExponentialBackoff::new(Duration::from_secs(32), Duration::from_secs(60));

        let _d1 = backoff.next_delay(); // consumes 32s nominal
        let d2 = backoff.next_delay(); // nominal capped at 60s

        assert!(d2 >= Duration::from_secs(30), "d2={d2:?}");
        assert!(d2 < Duration::from_secs(60), "d2={d2:?}");

        // Further calls stay at max
        let d3 = backoff.next_delay();
        assert!(d3 >= Duration::from_secs(30), "d3={d3:?}");
        assert!(d3 < Duration::from_secs(60), "d3={d3:?}");
    }

    #[test]
    fn backoff_reset_restores_initial() {
        let mut backoff = ExponentialBackoff::new(Duration::from_secs(1), Duration::from_secs(60));

        // Advance past initial
        let _ = backoff.next_delay();
        let _ = backoff.next_delay();
        let _ = backoff.next_delay();

        backoff.reset();

        let d = backoff.next_delay();
        assert!(d >= Duration::from_millis(500), "d={d:?}");
        assert!(d < Duration::from_secs(1), "d={d:?}");
    }

    // ---- reconnection integration tests ----

    /// Respond to mining.configure and mining.subscribe with success.
    ///
    /// Shared prefix for tests that need to diverge at the authorize
    /// or suggest_difficulty step.
    async fn do_configure_and_subscribe(handle: &mut MockTransportHandle) {
        // mining.configure
        let msg = handle.recv().await;
        assert_eq!(msg.method(), Some("mining.configure"));
        handle.send(JsonRpcMessage::Response {
            id: msg.id().unwrap(),
            result: Some(json!({
                "version-rolling": true,
                "version-rolling.mask": "1fffe000"
            })),
            error: None,
        });

        // mining.subscribe
        let msg = handle.recv().await;
        assert_eq!(msg.method(), Some("mining.subscribe"));
        handle.send(JsonRpcMessage::Response {
            id: msg.id().unwrap(),
            result: Some(json!([[], "aabb", 4])),
            error: None,
        });
    }

    /// Complete the full Stratum handshake from the test (pool) side.
    ///
    /// Responds to mining.configure, mining.subscribe, mining.authorize,
    /// and mining.suggest_difficulty. After this returns, the client is
    /// in its main event loop, ready for notifications.
    async fn do_handshake(handle: &mut MockTransportHandle) {
        do_configure_and_subscribe(handle).await;

        // mining.authorize
        let msg = handle.recv().await;
        assert_eq!(msg.method(), Some("mining.authorize"));
        handle.send(JsonRpcMessage::Response {
            id: msg.id().unwrap(),
            result: Some(json!(true)),
            error: None,
        });

        // mining.suggest_difficulty
        let msg = handle.recv().await;
        assert_eq!(msg.method(), Some("mining.suggest_difficulty"));
        handle.send(JsonRpcMessage::Response {
            id: msg.id().unwrap(),
            result: Some(json!(true)),
            error: None,
        });
    }

    /// Build a minimal mining.notify notification.
    fn job_notification(job_id: &str) -> JsonRpcMessage {
        JsonRpcMessage::notification(
            "mining.notify",
            json!([
                job_id,
                "0000000000000000000000000000000000000000000000000000000000000000",
                "aa",
                "bb",
                [],
                "20000000",
                "1d00ffff",
                "5a5a5a5a",
                true
            ]),
        )
    }

    /// Create a StratumV1Source wired to a mock transport channel.
    ///
    /// Returns (source, event_rx, command_tx, mock_tx, shutdown).
    fn source_with_mock_transports() -> (
        StratumV1Source,
        mpsc::Receiver<SourceEvent>,
        mpsc::Sender<SourceCommand>,
        mpsc::Sender<MockTransport>,
        CancellationToken,
    ) {
        let (event_tx, event_rx) = mpsc::channel(100);
        let (command_tx, command_rx) = mpsc::channel(100);
        let shutdown = CancellationToken::new();
        let (mock_tx, mock_rx) = mpsc::channel(10);

        let config = PoolConfig {
            url: "stratum+tcp://test:3333".to_string(),
            username: "testworker".to_string(),
            password: "x".to_string(),
            user_agent: "test".to_string(),
            ..Default::default()
        };

        let source = StratumV1Source::new(
            config,
            command_rx,
            event_tx,
            shutdown.clone(),
            Box::new(MockConnector::new(mock_rx)),
        );

        (source, event_rx, command_tx, mock_tx, shutdown)
    }

    #[tokio::test(start_paused = true)]
    async fn reconnects_after_disconnect() {
        let (source, mut event_rx, command_tx, mock_tx, shutdown) = source_with_mock_transports();

        let (transport1, mut handle1) = MockTransport::pair();
        let (transport2, mut handle2) = MockTransport::pair();
        mock_tx.send(transport1).await.unwrap();
        mock_tx.send(transport2).await.unwrap();

        let source_handle = tokio::spawn(source.run());

        // Trigger Phase 1 -> Phase 2 with a positive hashrate.
        command_tx
            .send(SourceCommand::UpdateHashRate(HashRate::from_gigahashes(
                500.0,
            )))
            .await
            .unwrap();

        // First connection: handshake, receive one job, then disconnect.
        do_handshake(&mut handle1).await;
        handle1.send(job_notification("job-1"));

        let event = event_rx.recv().await.unwrap();
        assert!(
            matches!(event, SourceEvent::ReplaceJob(ref t) if t.id == "job-1"),
            "expected ReplaceJob(job-1), got {event:?}",
        );

        // Drop the handle to simulate pool going away.
        drop(handle1);

        let event = event_rx.recv().await.unwrap();
        assert!(
            matches!(event, SourceEvent::ClearJobs),
            "expected ClearJobs after disconnect, got {event:?}",
        );

        // Advance past the backoff (max initial is 1s).
        time::advance(Duration::from_secs(2)).await;

        // Second connection: handshake, receive a job.
        do_handshake(&mut handle2).await;
        handle2.send(job_notification("job-2"));

        let event = event_rx.recv().await.unwrap();
        assert!(
            matches!(event, SourceEvent::ReplaceJob(ref t) if t.id == "job-2"),
            "expected ReplaceJob(job-2), got {event:?}",
        );

        shutdown.cancel();
        source_handle.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn backoff_escalates_across_disconnects() {
        let (source, mut event_rx, command_tx, mock_tx, shutdown) = source_with_mock_transports();

        // Four transports that fail immediately (handle dropped).
        for _ in 0..4 {
            let (transport, handle) = MockTransport::pair();
            drop(handle);
            mock_tx.send(transport).await.unwrap();
        }

        let source_handle = tokio::spawn(source.run());

        command_tx
            .send(SourceCommand::UpdateHashRate(HashRate::from_gigahashes(
                500.0,
            )))
            .await
            .unwrap();

        // 1st disconnect: nominal 1s, jittered to [0.5s, 1.0s).
        let event = event_rx.recv().await.unwrap();
        assert!(matches!(event, SourceEvent::ClearJobs));

        // Advancing 0.4s is below the minimum (0.5s); no reconnect yet.
        time::advance(Duration::from_millis(400)).await;
        tokio::task::yield_now().await;
        assert!(
            event_rx.try_recv().is_err(),
            "reconnected too soon after 1st disconnect",
        );

        // Advance past the maximum (1.0s total) to trigger reconnect.
        time::advance(Duration::from_millis(600)).await;

        // 2nd disconnect: nominal 2s, jittered to [1.0s, 2.0s).
        let event = event_rx.recv().await.unwrap();
        assert!(matches!(event, SourceEvent::ClearJobs));

        // Advancing 0.9s is below the minimum (1.0s); no reconnect yet.
        time::advance(Duration::from_millis(900)).await;
        tokio::task::yield_now().await;
        assert!(
            event_rx.try_recv().is_err(),
            "reconnected too soon after 2nd disconnect",
        );

        // Advance past the maximum (2.0s total).
        time::advance(Duration::from_millis(1100)).await;

        // 3rd disconnect: nominal 4s, jittered to [2.0s, 4.0s).
        let event = event_rx.recv().await.unwrap();
        assert!(matches!(event, SourceEvent::ClearJobs));

        // Advancing 1.9s is below the minimum (2.0s); proves escalation.
        time::advance(Duration::from_millis(1900)).await;
        tokio::task::yield_now().await;
        assert!(
            event_rx.try_recv().is_err(),
            "reconnected too soon after 3rd disconnect",
        );

        shutdown.cancel();
        source_handle.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn fatal_error_stops_retrying() {
        let (source, _event_rx, command_tx, mock_tx, _shutdown) = source_with_mock_transports();

        let (transport, mut handle) = MockTransport::pair();
        mock_tx.send(transport).await.unwrap();

        let source_handle = tokio::spawn(source.run());

        command_tx
            .send(SourceCommand::UpdateHashRate(HashRate::from_gigahashes(
                500.0,
            )))
            .await
            .unwrap();

        do_configure_and_subscribe(&mut handle).await;

        // mining.authorize -- reject
        let msg = handle.recv().await;
        handle.send(JsonRpcMessage::Response {
            id: msg.id().unwrap(),
            result: Some(json!(false)),
            error: None,
        });

        // Source should return Err (fatal, no reconnect).
        let result = source_handle.await.unwrap();
        assert!(result.is_err(), "expected fatal error, got Ok");
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_during_backoff() {
        let (source, mut event_rx, command_tx, mock_tx, shutdown) = source_with_mock_transports();

        // Transport whose handle is already gone -- client will see
        // immediate disconnect when it tries to write.
        let (transport, handle) = MockTransport::pair();
        drop(handle);
        mock_tx.send(transport).await.unwrap();

        let source_handle = tokio::spawn(source.run());

        command_tx
            .send(SourceCommand::UpdateHashRate(HashRate::from_gigahashes(
                500.0,
            )))
            .await
            .unwrap();

        // Wait for ClearJobs, which proves the source processed the
        // disconnect and entered the backoff wait.
        let event = event_rx.recv().await.unwrap();
        assert!(
            matches!(event, SourceEvent::ClearJobs),
            "expected ClearJobs, got {event:?}",
        );

        // Cancel shutdown while still in the backoff wait.
        shutdown.cancel();

        let result = source_handle.await.unwrap();
        assert!(result.is_ok(), "expected clean shutdown, got {result:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn hashrate_updated_during_backoff() {
        let (source, mut event_rx, command_tx, mock_tx, shutdown) = source_with_mock_transports();

        // First transport -- handle dropped for immediate disconnect.
        let (transport1, handle1) = MockTransport::pair();
        drop(handle1);

        let (transport2, mut handle2) = MockTransport::pair();
        mock_tx.send(transport1).await.unwrap();
        mock_tx.send(transport2).await.unwrap();

        let source_handle = tokio::spawn(source.run());

        // Initial low hashrate to enter Phase 2.
        command_tx
            .send(SourceCommand::UpdateHashRate(HashRate::from_gigahashes(
                100.0,
            )))
            .await
            .unwrap();

        // Wait for ClearJobs, which proves the source processed the
        // disconnect and entered the backoff wait.
        let event = event_rx.recv().await.unwrap();
        assert!(
            matches!(event, SourceEvent::ClearJobs),
            "expected ClearJobs, got {event:?}",
        );

        // Update hashrate during backoff -- 10x higher.
        command_tx
            .send(SourceCommand::UpdateHashRate(HashRate::from_gigahashes(
                1000.0,
            )))
            .await
            .unwrap();

        // Advance past backoff.
        time::advance(Duration::from_secs(2)).await;

        // Handshake through authorize, then inspect suggest_difficulty.
        do_configure_and_subscribe(&mut handle2).await;

        let msg = handle2.recv().await;
        assert_eq!(msg.method(), Some("mining.authorize"));
        handle2.send(JsonRpcMessage::Response {
            id: msg.id().unwrap(),
            result: Some(json!(true)),
            error: None,
        });

        let msg = handle2.recv().await;
        assert_eq!(msg.method(), Some("mining.suggest_difficulty"));
        let JsonRpcMessage::Request { params, .. } = &msg else {
            panic!("expected Request");
        };
        let difficulty = params[0].as_u64().unwrap();
        // 1000 GH/s at 20 shares/min yields ~698. The original 100
        // GH/s would give ~70. Assert above 300 to confirm the
        // updated hashrate was used.
        assert!(
            difficulty > 300,
            "expected difficulty > 300 (from 1000 GH/s), got {difficulty}"
        );
        handle2.send(JsonRpcMessage::Response {
            id: msg.id().unwrap(),
            result: Some(json!(true)),
            error: None,
        });

        shutdown.cancel();
        source_handle.await.unwrap().unwrap();
    }
}
