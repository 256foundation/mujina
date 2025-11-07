//! Dummy job source for testing and development.
//!
//! This source generates JobTemplates based on real block data from block 881,423.
//! It uses authentic coinbase transaction parts and merkle branches, with extranonce2
//! initialized to the actual winning value. This gives mining hardware a high
//! probability of finding the real block hash quickly, making it an excellent test
//! of the complete mining stack.

use anyhow::Result;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use super::test_blocks::block_881423;
use super::{
    Extranonce2Range, JobTemplate, MerkleRootKind, MerkleRootTemplate, SourceCommand, SourceEvent,
    SourceHandle, VersionTemplate,
};

/// Dummy job source that generates work from test block data.
///
/// Emits JobTemplates on a fixed interval using authentic data from block 881,423.
/// The job is configured with the actual winning extranonce2 and version, giving
/// hardware an excellent chance of finding the real block hash.
pub struct DummySource {
    handle: SourceHandle,

    /// Where to send events (scheduler, test harness, etc.)
    event_tx: mpsc::Sender<(SourceHandle, SourceEvent)>,

    /// Where to receive commands
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
        handle: SourceHandle,
        event_tx: mpsc::Sender<(SourceHandle, SourceEvent)>,
        command_rx: mpsc::Receiver<SourceCommand>,
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

        let job_template = JobTemplate {
            id: "dummy-0".into(),
            prev_blockhash: *block_881423::PREV_BLOCKHASH,

            // Start with the exact winning version, allow rolling top 10 bits
            // (BM1370 capability per register 0xA4 documentation)
            version: VersionTemplate {
                version: *block_881423::VERSION,
                mask: Some(0x3ff0_0000), // Top 10 bits rollable
            },

            bits: *block_881423::BITS,
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
            handle,
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
    /// coordinator. Runs until the shutdown token is cancelled.
    pub async fn run(mut self) -> Result<()> {
        info!("Dummy source {} starting", self.handle.name());

        loop {
            tokio::select! {
                // ACTIVELY emit job events on timer
                _ = tokio::time::sleep(self.interval) => {
                    debug!("Dummy: emitting job {}", self.job_template.id);
                    self.event_tx.send((
                        self.handle.clone(),
                        SourceEvent::UpdateJob(self.job_template.clone())
                    )).await?;
                }

                // REACTIVELY handle commands
                Some(cmd) = self.command_rx.recv() => {
                    match cmd {
                        SourceCommand::SubmitShare(share) => {
                            debug!(
                                "Dummy: received share nonce=0x{:08x} for job {}",
                                share.nonce,
                                share.job_id
                            );
                        }
                    }
                }

                // Cooperative shutdown
                _ = self.shutdown.cancelled() => {
                    info!("Dummy source {} shutting down", self.handle.name());
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
        // Time starts paused - auto-advances when runtime is idle

        // Create message channels
        let (event_tx, mut event_rx) = mpsc::channel(10);
        let (command_tx, command_rx) = mpsc::channel(10);

        // Create shutdown token
        let shutdown = CancellationToken::new();

        // Create handle (Arc pointer provides unique identity)
        let handle = SourceHandle::new("test-dummy".into(), command_tx);

        // Create and spawn dummy source with realistic interval
        // With paused time, this completes instantly
        let dummy = DummySource::new(
            handle.clone(),
            event_tx,
            command_rx,
            shutdown.clone(),
            Duration::from_secs(30), // Realistic interval, but time is paused
        )
        .expect("failed to create dummy source");
        tokio::spawn(dummy.run());

        // Receive job event - time auto-advances to when timer fires
        let (recv_handle, event) = event_rx.recv().await.expect("channel closed");

        assert_eq!(recv_handle, handle);

        match event {
            SourceEvent::UpdateJob(job) => {
                assert_eq!(job.id, "dummy-0");
                assert_eq!(job.prev_blockhash, *block_881423::PREV_BLOCKHASH);
                assert_eq!(job.bits, *block_881423::BITS);
                assert_eq!(job.time, block_881423::TIME);

                // Verify it's using computed merkle root
                assert!(matches!(job.merkle_root, MerkleRootKind::Computed(_)));
            }
            _ => panic!("Expected NewJob event"),
        }

        // Send share command back with the actual winning nonce
        let share = Share {
            job_id: "dummy-0".into(),
            nonce: block_881423::NONCE,
            time: block_881423::TIME,
            version: *block_881423::VERSION,
            extranonce2: None,
        };
        handle
            .submit_share(share)
            .await
            .expect("failed to submit share");

        // Clean shutdown
        shutdown.cancel();
    }

    #[test]
    fn test_dummy_job_produces_valid_block_hash() {
        // Create a dummy source to get the job template
        let (event_tx, _event_rx) = mpsc::channel(10);
        let (command_tx, command_rx) = mpsc::channel(10);
        let shutdown = CancellationToken::new();
        let handle = SourceHandle::new("test-mining".into(), command_tx);

        let dummy = DummySource::new(
            handle,
            event_tx,
            command_rx,
            shutdown,
            Duration::from_secs(30),
        )
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
