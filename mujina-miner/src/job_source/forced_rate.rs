//! Forced share rate source wrapper for testing.
//!
//! Wraps another job source and overrides share_target to achieve a target
//! share rate. Useful for testing CPU mining with real pools that cannot
//! express sufficiently low difficulty.
//!
//! # Usage
//!
//! Set `MUJINA_POOL_FORCED_RATE=18` to enable the wrapper targeting 18
//! shares per minute (~3.33 seconds between shares). The wrapper intercepts
//! job templates from the inner source and replaces their share_target with
//! one computed to achieve the target rate at the current hashrate.
//!
//! Shares are forwarded to the inner source regardless of whether they meet
//! the pool's actual difficulty. The pool may accept (if configured with low
//! mindiff) or reject them---either is valid for testing.

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace, warn};

use super::{JobTemplate, SourceCommand, SourceEvent};
use crate::types::{target_for_share_rate, Difficulty, HashRate, ShareRate};

/// Configuration for forced share rate wrapper.
pub struct ForcedRateConfig {
    /// Target share rate (shares per minute)
    pub target_rate: ShareRate,
}

impl ForcedRateConfig {
    /// Parse from environment variables.
    ///
    /// Returns `Some` if `MUJINA_POOL_FORCED_RATE` is set. The value specifies
    /// the target rate in shares per minute; defaults to 18 if unparseable.
    pub fn from_env() -> Option<Self> {
        let val = std::env::var("MUJINA_POOL_FORCED_RATE").ok()?;
        let shares_per_min: f64 = match val.parse::<f64>() {
            Ok(v) if v.is_finite() && v > 0.0 => v,
            Ok(v) => {
                warn!(
                    value = v,
                    "MUJINA_POOL_FORCED_RATE must be finite and positive, using default 18"
                );
                18.0
            }
            Err(_) => {
                warn!(
                    value = %val,
                    "Invalid MUJINA_POOL_FORCED_RATE, using default 18"
                );
                18.0
            }
        };
        Some(Self {
            target_rate: ShareRate::per_minute(shares_per_min),
        })
    }
}

/// Wrapper that overrides share_target to force a specific share rate.
///
/// Sits between the scheduler and an inner source (typically StratumV1Source),
/// intercepting job templates to replace their share_target with one computed
/// to achieve a target share rate at the current hashrate.
pub struct ForcedRateSource {
    /// Cached target share rate (computed once from config)
    target_rate: ShareRate,

    /// Upstream event receiver (from inner source)
    inner_event_rx: mpsc::Receiver<SourceEvent>,

    /// Downstream event sender (to scheduler)
    outer_event_tx: mpsc::Sender<SourceEvent>,

    /// Upstream command sender (to inner source)
    inner_command_tx: mpsc::Sender<SourceCommand>,

    /// Downstream command receiver (from scheduler)
    outer_command_rx: mpsc::Receiver<SourceCommand>,

    /// Current hashrate estimate from scheduler
    hashrate: HashRate,

    /// Shutdown token
    shutdown: CancellationToken,
}

impl ForcedRateSource {
    /// Create a new forced rate wrapper.
    ///
    /// Channel wiring:
    /// - `inner_event_rx`: receives events from the wrapped source
    /// - `outer_event_tx`: sends modified events to the scheduler
    /// - `inner_command_tx`: forwards commands to the wrapped source
    /// - `outer_command_rx`: receives commands from the scheduler
    pub fn new(
        config: ForcedRateConfig,
        inner_event_rx: mpsc::Receiver<SourceEvent>,
        outer_event_tx: mpsc::Sender<SourceEvent>,
        inner_command_tx: mpsc::Sender<SourceCommand>,
        outer_command_rx: mpsc::Receiver<SourceCommand>,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            target_rate: config.target_rate,
            inner_event_rx,
            outer_event_tx,
            inner_command_tx,
            outer_command_rx,
            hashrate: HashRate::default(),
            shutdown,
        }
    }

    /// Modify job template to use our computed share_target.
    fn modify_job(&self, mut job: JobTemplate) -> JobTemplate {
        if self.hashrate.is_zero() {
            // No hashrate yet; pass through unmodified
            return job;
        }

        let target = target_for_share_rate(self.target_rate, self.hashrate);

        trace!(
            job_id = %job.id,
            hashrate = %self.hashrate,
            target_rate = %self.target_rate,
            difficulty = %Difficulty::from_target(target),
            "Forcing share target"
        );

        job.share_target = target;
        job
    }

    /// Run the wrapper, forwarding events and commands between scheduler and
    /// inner source.
    pub async fn run(mut self) -> anyhow::Result<()> {
        loop {
            tokio::select! {
                // Events from inner source -> modify and forward to scheduler
                event = self.inner_event_rx.recv() => {
                    let Some(event) = event else {
                        debug!("Inner source closed, shutting down wrapper");
                        break;
                    };
                    let modified = match event {
                        SourceEvent::UpdateJob(job) => {
                            SourceEvent::UpdateJob(self.modify_job(job))
                        }
                        SourceEvent::ReplaceJob(job) => {
                            SourceEvent::ReplaceJob(self.modify_job(job))
                        }
                        SourceEvent::ClearJobs => SourceEvent::ClearJobs,
                    };
                    self.outer_event_tx.send(modified).await?;
                }

                // Commands from scheduler -> update state and forward to inner
                cmd = self.outer_command_rx.recv() => {
                    let Some(cmd) = cmd else {
                        debug!("Scheduler closed command channel, shutting down wrapper");
                        break;
                    };
                    if let SourceCommand::UpdateHashRate(hr) = &cmd {
                        trace!(hashrate = %hr, "Hashrate updated");
                        self.hashrate = *hr;
                    }
                    self.inner_command_tx.send(cmd).await?;
                }

                _ = self.shutdown.cancelled() => {
                    debug!("Forced rate wrapper shutting down");
                    break;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job_source::{GeneralPurposeBits, MerkleRootKind, VersionTemplate};
    use crate::types::target_for_share_rate;
    use bitcoin::block::Version;
    use bitcoin::hashes::Hash;
    use bitcoin::pow::{CompactTarget, Target};
    use bitcoin::BlockHash;

    /// Test harness holding all channel endpoints for a ForcedRateSource.
    struct TestHarness {
        wrapper: ForcedRateSource,
        /// Send events as if from inner source
        inner_event_tx: mpsc::Sender<SourceEvent>,
        /// Receive events forwarded to scheduler
        outer_event_rx: mpsc::Receiver<SourceEvent>,
        /// Receive commands forwarded to inner source
        inner_cmd_rx: mpsc::Receiver<SourceCommand>,
        /// Send commands as if from scheduler
        outer_cmd_tx: mpsc::Sender<SourceCommand>,
        shutdown: CancellationToken,
    }

    impl TestHarness {
        fn new(target_rate: ShareRate) -> Self {
            let (inner_event_tx, inner_event_rx) = mpsc::channel(10);
            let (outer_event_tx, outer_event_rx) = mpsc::channel(10);
            let (inner_cmd_tx, inner_cmd_rx) = mpsc::channel(10);
            let (outer_cmd_tx, outer_cmd_rx) = mpsc::channel(10);
            let shutdown = CancellationToken::new();

            let config = ForcedRateConfig { target_rate };
            let wrapper = ForcedRateSource::new(
                config,
                inner_event_rx,
                outer_event_tx,
                inner_cmd_tx,
                outer_cmd_rx,
                shutdown.clone(),
            );

            Self {
                wrapper,
                inner_event_tx,
                outer_event_rx,
                inner_cmd_rx,
                outer_cmd_tx,
                shutdown,
            }
        }
    }

    fn make_test_job(id: &str, share_target: Target) -> JobTemplate {
        JobTemplate {
            id: id.to_string(),
            prev_blockhash: BlockHash::all_zeros(),
            version: VersionTemplate::new(
                Version::from_consensus(0x20000000),
                GeneralPurposeBits::none(),
            )
            .unwrap(),
            bits: CompactTarget::from_consensus(0x1d00ffff),
            share_target,
            time: 0,
            merkle_root: MerkleRootKind::Fixed(bitcoin::TxMerkleNode::all_zeros()),
        }
    }

    #[tokio::test]
    async fn test_modify_job_computes_correct_target() {
        let target_rate = ShareRate::per_minute(6.0); // 10 seconds between shares
        let mut harness = TestHarness::new(target_rate);

        // Set hashrate so target computation works
        let hashrate = HashRate::from_megahashes(100.0);
        harness.wrapper.hashrate = hashrate;

        // Create job with arbitrary target (will be replaced)
        let job = make_test_job("test-1", Target::MAX);

        // Modify the job
        let modified = harness.wrapper.modify_job(job);

        // Verify the target matches what target_for_share_rate computes
        let expected_target = target_for_share_rate(target_rate, hashrate);
        assert_eq!(modified.share_target, expected_target);
    }

    #[tokio::test]
    async fn test_zero_hashrate_passes_job_unmodified() {
        let harness = TestHarness::new(ShareRate::per_minute(6.0));

        // Hashrate is zero by default
        assert!(harness.wrapper.hashrate.is_zero());

        let original_target = Target::MAX;
        let job = make_test_job("test-2", original_target);

        let modified = harness.wrapper.modify_job(job);

        // Should be unchanged
        assert_eq!(modified.share_target, original_target);
    }

    #[tokio::test]
    async fn test_hashrate_update_forwarded() {
        let harness = TestHarness::new(ShareRate::per_minute(6.0));
        let TestHarness {
            wrapper,
            mut inner_cmd_rx,
            outer_cmd_tx,
            shutdown,
            ..
        } = harness;

        let handle = tokio::spawn(wrapper.run());

        // Send hashrate update
        let hashrate = HashRate::from_gigahashes(1.0);
        outer_cmd_tx
            .send(SourceCommand::UpdateHashRate(hashrate))
            .await
            .unwrap();

        // Should be forwarded to inner source
        let received = inner_cmd_rx.recv().await.unwrap();
        assert!(matches!(
            received,
            SourceCommand::UpdateHashRate(hr) if hr == hashrate
        ));

        shutdown.cancel();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_job_events_forwarded_with_modified_target() {
        let target_rate = ShareRate::per_minute(12.0); // 5 seconds between shares
        let harness = TestHarness::new(target_rate);
        let TestHarness {
            wrapper,
            inner_event_tx,
            mut outer_event_rx,
            mut inner_cmd_rx,
            outer_cmd_tx,
            shutdown,
        } = harness;

        let handle = tokio::spawn(wrapper.run());

        // Send hashrate update
        let hashrate = HashRate::from_megahashes(50.0);
        outer_cmd_tx
            .send(SourceCommand::UpdateHashRate(hashrate))
            .await
            .unwrap();

        // Wait for command to be forwarded (proves it was processed)
        let _ = inner_cmd_rx.recv().await.unwrap();

        // Now send a job from inner source
        let job = make_test_job("job-1", Target::MAX);
        inner_event_tx
            .send(SourceEvent::ReplaceJob(job))
            .await
            .unwrap();

        // Receive modified job and verify target
        let event = outer_event_rx.recv().await.unwrap();
        let expected_target = target_for_share_rate(target_rate, hashrate);

        match event {
            SourceEvent::ReplaceJob(job) => {
                assert_eq!(job.id, "job-1");
                assert_eq!(job.share_target, expected_target);
            }
            _ => panic!("Expected ReplaceJob"),
        }

        shutdown.cancel();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_inner_source_closure_shuts_down_wrapper() {
        let harness = TestHarness::new(ShareRate::per_minute(6.0));
        let TestHarness {
            wrapper,
            inner_event_tx,
            ..
        } = harness;

        let handle = tokio::spawn(wrapper.run());

        // Drop the inner event sender to close the channel
        drop(inner_event_tx);

        // Wrapper should exit gracefully
        let result = handle.await.unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_clear_jobs_forwarded_unchanged() {
        let harness = TestHarness::new(ShareRate::per_minute(6.0));
        let TestHarness {
            wrapper,
            inner_event_tx,
            mut outer_event_rx,
            shutdown,
            ..
        } = harness;

        let handle = tokio::spawn(wrapper.run());

        // Send ClearJobs
        inner_event_tx.send(SourceEvent::ClearJobs).await.unwrap();

        // Should be forwarded unchanged
        let event = outer_event_rx.recv().await.unwrap();
        assert!(matches!(event, SourceEvent::ClearJobs));

        shutdown.cancel();
        handle.await.unwrap().unwrap();
    }
}
