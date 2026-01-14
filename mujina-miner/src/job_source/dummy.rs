//! Dummy job source for testing and development.
//!
//! This source generates JobTemplates based on real block data from block 881,423.
//! It uses authentic coinbase transaction parts and merkle branches, with extranonce2
//! initialized to the actual winning value. This gives mining hardware a high
//! probability of finding the real block hash quickly, making it an excellent test
//! of the complete mining stack.

use anyhow::Result;
use bitcoin::block::Version;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::types::{target_for_share_rate, HashRate, ShareRate};

use super::test_blocks::block_881423;
use super::{
    Extranonce2Range, GeneralPurposeBits, JobTemplate, MerkleRootKind, MerkleRootTemplate,
    SourceCommand, SourceEvent, VersionTemplate,
};

/// Dummy job source that generates work from test block data.
///
/// Emits JobTemplates on a fixed interval using authentic data from block 881,423.
/// The job is configured with the actual winning extranonce2 and version, giving
/// hardware an excellent chance of finding the real block hash.
pub struct DummySource {
    /// Where to send events to scheduler
    event_tx: mpsc::Sender<SourceEvent>,

    /// Where to receive commands from scheduler
    command_rx: mpsc::Receiver<SourceCommand>,

    /// Cooperative cancellation for graceful shutdown
    shutdown: CancellationToken,

    /// The job template to emit
    job_template: JobTemplate,

    /// How often to emit jobs
    interval: Duration,
}

impl DummySource {
    /// Create a new dummy source.
    ///
    /// Builds a JobTemplate from block 881,423 with authentic coinbase parts,
    /// merkle branches, and the winning extranonce2/version values. This creates
    /// a job that hardware can solve almost immediately, validating the entire
    /// mining stack.
    pub fn new(
        command_rx: mpsc::Receiver<SourceCommand>,
        event_tx: mpsc::Sender<SourceEvent>,
        shutdown: CancellationToken,
        interval: Duration,
    ) -> Result<Self> {
        // Extract the actual extranonce2 from the winning block
        let extranonce2_bytes = block_881423::extranonce2_bytes();
        let extranonce2_actual = u32::from_le_bytes(extranonce2_bytes.try_into().expect("4 bytes"));

        // Create extranonce2 range around the winning value
        // This gives hardware high probability of hitting the real block hash
        let extranonce2_range = Extranonce2Range::new_range(
            extranonce2_actual as u64,
            extranonce2_actual as u64 + 100, // Small range for quick testing
            4,                               // 4 bytes
        )?;

        // Get merkle branches as typed TxMerkleNode
        let merkle_branches = block_881423::MERKLE_BRANCHES.clone();

        // Use block_881423 version with GP bits (13-28) masked away
        let v = block_881423::VERSION.to_consensus() as u32;
        let base_cleaned = (v & !0x1fff_e000) as i32;
        let version = VersionTemplate::new(
            Version::from_consensus(base_cleaned),
            GeneralPurposeBits::full(),
        )
        .expect("Masked version has no GP bits set");

        let job_template = JobTemplate {
            id: "dummy-0".into(),
            prev_blockhash: *block_881423::PREV_BLOCKHASH,
            version,
            bits: *block_881423::BITS,

            // Share difficulty: ~1 share per 10 seconds at 1 TH/s
            share_target: target_for_share_rate(
                ShareRate::per_minute(6.0),
                HashRate::from_terahashes(1.0),
            ),

            time: block_881423::TIME,

            // Use computed merkle root with authentic coinbase parts
            merkle_root: MerkleRootKind::Computed(MerkleRootTemplate {
                coinbase1: block_881423::coinbase1_bytes().to_vec(),
                extranonce1: block_881423::extranonce1_bytes().to_vec(),
                extranonce2_range,
                coinbase2: block_881423::coinbase2_bytes().to_vec(),
                merkle_branches,
            }),
        };

        Ok(Self {
            event_tx,
            command_rx,
            shutdown,
            job_template,
            interval,
        })
    }

    /// Run the dummy source (active loop).
    ///
    /// Emits JobTemplates on a timer and handles share submissions from the
    /// scheduler. Runs until the shutdown token is cancelled.
    pub async fn run(mut self) -> Result<()> {
        // Send initial job immediately
        debug!(job_id = %self.job_template.id, "Emitting initial job");
        self.event_tx
            .send(SourceEvent::UpdateJob(self.job_template.clone()))
            .await?;

        loop {
            tokio::select! {
                _ = tokio::time::sleep(self.interval) => {
                    debug!(job_id = %self.job_template.id, "Emitting job");
                    self.event_tx.send(SourceEvent::UpdateJob(self.job_template.clone())).await?;
                }

                Some(cmd) = self.command_rx.recv() => {
                    match cmd {
                        SourceCommand::SubmitShare(share) => {
                            debug!(
                                job_id = %share.job_id,
                                nonce = format!("{:#x}", share.nonce),
                                "Share received"
                            );
                        }
                        SourceCommand::UpdateHashRate(_) => {
                            // Ignored in dummy source
                        }
                    }
                }

                _ = self.shutdown.cancelled() => {
                    info!("Dummy source shutting down");
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
    use crate::job_source::Share;

    #[tokio::test(start_paused = true)]
    async fn test_dummy_source_communication() {
        let (event_tx, mut event_rx) = mpsc::channel(10);
        let (command_tx, command_rx) = mpsc::channel(10);
        let shutdown = CancellationToken::new();

        let dummy = DummySource::new(
            command_rx,
            event_tx,
            shutdown.clone(),
            Duration::from_secs(30),
        )
        .expect("failed to create dummy source");
        tokio::spawn(dummy.run());

        let event = event_rx.recv().await.expect("channel closed");

        match event {
            SourceEvent::UpdateJob(job) => {
                assert_eq!(job.id, "dummy-0");
                assert_eq!(job.prev_blockhash, *block_881423::PREV_BLOCKHASH);
                assert_eq!(job.bits, *block_881423::BITS);
                assert_eq!(job.time, block_881423::TIME);
                assert!(matches!(job.merkle_root, MerkleRootKind::Computed(_)));
            }
            _ => panic!("Expected UpdateJob event"),
        }

        let share = Share {
            job_id: "dummy-0".into(),
            nonce: block_881423::NONCE,
            time: block_881423::TIME,
            version: *block_881423::VERSION,
            extranonce2: None,
        };
        command_tx
            .send(SourceCommand::SubmitShare(share))
            .await
            .expect("failed to send");

        shutdown.cancel();
    }

    #[test]
    fn test_dummy_job_produces_valid_block_hash() {
        let (event_tx, _event_rx) = mpsc::channel(10);
        let (_command_tx, command_rx) = mpsc::channel(10);
        let shutdown = CancellationToken::new();

        let dummy = DummySource::new(command_rx, event_tx, shutdown, Duration::from_secs(30))
            .expect("failed to create dummy source");

        let job = dummy.job_template;

        // Extract the merkle root template
        let merkle_template = match job.merkle_root {
            MerkleRootKind::Computed(template) => template,
            _ => panic!("Expected computed merkle root"),
        };

        // Compute merkle root with the golden extranonce2
        let computed_merkle_root = merkle_template
            .compute_merkle_root(&block_881423::EXTRANONCE2)
            .expect("valid merkle root computation");

        // Build block header with golden values
        use bitcoin::block::Header as BlockHeader;
        let header = BlockHeader {
            version: *block_881423::VERSION,
            prev_blockhash: job.prev_blockhash,
            merkle_root: computed_merkle_root,
            time: job.time,
            bits: job.bits,
            nonce: block_881423::NONCE,
        };

        // Compute block hash
        let computed_hash = header.block_hash();

        // Verify it matches the known block hash
        assert_eq!(
            computed_hash,
            *block_881423::BLOCK_HASH,
            "Computed block hash doesn't match expected"
        );
    }
}
