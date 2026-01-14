//! CPU mining loop.
//!
//! Runs in a dedicated `std::thread` to avoid blocking the Tokio runtime.
//! Implements duty cycling for CPU utilization control.
//!
//! # Performance
//!
//! This implementation prioritizes readability over performance. It achieves
//! ~5 MH/s per thread, which is sufficient for testing and development but
//! leaves significant performance on the table. Potential optimizations:
//!
//! - **Midstate caching**: Precompute SHA-256 state after the first 64 bytes
//!   of the header (which don't change between nonces), saving ~50% of hash
//!   work. This alone can double throughput.
//!
//! - **SIMD multi-buffer hashing**: Process 4-8 nonces in parallel using
//!   AVX2/AVX-512 intrinsics. This abandons SHA-NI (which is single-stream)
//!   in favor of wider SIMD registers, potentially achieving 2-4x throughput.
//!
//! Production CPU miners like cpuminer-opt use these techniques to achieve
//! 10-20+ MH/s per thread on modern hardware.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, TryRecvError},
    Arc, RwLock,
};
use std::time::{Duration, Instant};

use bitcoin::block::Header as BlockHeader;

use crate::{
    asic::hash_thread::{HashTask, HashThreadError, HashThreadStatus, Share},
    job_source::MerkleRootKind,
    tracing::prelude::*,
    types::HashRate,
    u256::U256,
};

/// Commands sent to the mining thread.
#[derive(Debug)]
pub enum MinerCommand {
    /// Update task (old shares still valid).
    UpdateTask {
        task: HashTask,
        response_tx: tokio::sync::oneshot::Sender<Result<Option<HashTask>, HashThreadError>>,
    },

    /// Replace task (old shares invalid).
    ReplaceTask {
        task: HashTask,
        response_tx: tokio::sync::oneshot::Sender<Result<Option<HashTask>, HashThreadError>>,
    },

    /// Go idle (stop hashing).
    GoIdle {
        response_tx: tokio::sync::oneshot::Sender<Result<Option<HashTask>, HashThreadError>>,
    },

    /// Shutdown the thread.
    Shutdown,
}

/// Run the CPU mining loop.
///
/// This function runs in a dedicated `std::thread`. It receives commands
/// via an `mpsc` channel and hashes using the current task.
///
/// # Arguments
///
/// * `thread_name` - Name for logging
/// * `cmd_rx` - Channel for receiving commands
/// * `status` - Shared status for queries
/// * `duty_percent` - Target CPU duty cycle (1-100)
/// * `shutdown` - Atomic flag for graceful shutdown
pub fn run_mining_loop(
    thread_name: String,
    cmd_rx: mpsc::Receiver<MinerCommand>,
    status: Arc<RwLock<HashThreadStatus>>,
    duty_percent: u8,
    shutdown: Arc<AtomicBool>,
) {
    // Calculate duty cycle timing
    let cycle_ms = 1000u64;
    let work_ms = (cycle_ms as f64 * duty_percent as f64 / 100.0) as u64;

    let mut current_task: Option<HashTask> = None;
    let mut cached_merkle_root: Option<bitcoin::TxMerkleNode> = None;
    let mut nonce: u32 = 0;
    let mut last_ntime_tick = Instant::now();
    let mut shares_found: u64 = 0;
    let mut hashes_computed: u64 = 0;
    let mut last_hashrate_update = Instant::now();

    loop {
        // Check shutdown flag
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        // Process any pending commands
        loop {
            match cmd_rx.try_recv() {
                Ok(cmd) => match cmd {
                    MinerCommand::UpdateTask { task, response_tx } => {
                        cached_merkle_root = compute_merkle_root(&task);
                        let old = current_task.replace(task);
                        nonce = 0;
                        update_status(&status, true, shares_found);
                        let _ = response_tx.send(Ok(old));
                    }
                    MinerCommand::ReplaceTask { task, response_tx } => {
                        cached_merkle_root = compute_merkle_root(&task);
                        let old = current_task.replace(task);
                        nonce = 0;
                        update_status(&status, true, shares_found);
                        let _ = response_tx.send(Ok(old));
                    }
                    MinerCommand::GoIdle { response_tx } => {
                        cached_merkle_root = None;
                        let old = current_task.take();
                        update_status(&status, false, shares_found);
                        let _ = response_tx.send(Ok(old));
                    }
                    MinerCommand::Shutdown => {
                        return;
                    }
                },
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        // Mine if we have a task
        if current_task.is_some() {
            let cycle_start = Instant::now();

            // Hash for work_ms milliseconds, checking time only every 10000 hashes
            loop {
                // Check elapsed time and other conditions every 10000 hashes
                if hashes_computed.is_multiple_of(10000) {
                    if cycle_start.elapsed() >= Duration::from_millis(work_ms) {
                        break;
                    }
                    if shutdown.load(Ordering::Relaxed) {
                        return;
                    }
                }

                // Check for high-priority commands periodically (every 1000 hashes)
                if hashes_computed.is_multiple_of(1000) {
                    if let Ok(cmd) = cmd_rx.try_recv() {
                        match cmd {
                            MinerCommand::ReplaceTask { task, response_tx } => {
                                cached_merkle_root = compute_merkle_root(&task);
                                let old = current_task.replace(task);
                                nonce = 0;
                                update_status(&status, true, shares_found);
                                let _ = response_tx.send(Ok(old));
                                // Continue with new task in next iteration
                                break;
                            }
                            MinerCommand::UpdateTask { task, response_tx } => {
                                cached_merkle_root = compute_merkle_root(&task);
                                let old = current_task.replace(task);
                                nonce = 0;
                                update_status(&status, true, shares_found);
                                let _ = response_tx.send(Ok(old));
                                break;
                            }
                            MinerCommand::GoIdle { response_tx } => {
                                cached_merkle_root = None;
                                let old = current_task.take();
                                update_status(&status, false, shares_found);
                                let _ = response_tx.send(Ok(old));
                                break;
                            }
                            MinerCommand::Shutdown => return,
                        }
                    }
                }

                // Try this nonce
                if let (Some(ref task), Some(merkle_root)) = (&current_task, cached_merkle_root) {
                    if let Some(share) = try_nonce(task, merkle_root, nonce) {
                        shares_found += 1;
                        debug!(
                            thread = %thread_name,
                            nonce = %format!("{:#010x}", share.nonce),
                            "Share found"
                        );
                        // Send share via blocking send (we're in std::thread)
                        let _ = task.share_tx.blocking_send(share);
                    }
                }

                nonce = nonce.wrapping_add(1);
                hashes_computed += 1;
            }

            // Sleep for remainder of cycle (utilization control)
            let elapsed = cycle_start.elapsed().as_millis() as u64;
            let sleep_ms = cycle_ms.saturating_sub(elapsed);
            if sleep_ms > 0 {
                std::thread::sleep(Duration::from_millis(sleep_ms));
            }

            // Roll ntime every second
            if last_ntime_tick.elapsed() >= Duration::from_secs(1) {
                if let Some(ref mut task) = current_task {
                    task.ntime += 1;
                }
                last_ntime_tick = Instant::now();
            }

            // Update hashrate periodically
            if last_hashrate_update.elapsed() >= Duration::from_secs(5) {
                let elapsed_secs = last_hashrate_update.elapsed().as_secs_f64();
                let hashes_per_sec = (hashes_computed as f64 / elapsed_secs) as u64;
                let hashrate = HashRate(hashes_per_sec);

                trace!(
                    thread = %thread_name,
                    hashrate = %hashrate,
                    shares = shares_found,
                    "CPU miner stats"
                );

                {
                    let mut s = status.write().unwrap();
                    s.hashrate = hashrate;
                    s.chip_shares_found = shares_found;
                }

                hashes_computed = 0;
                last_hashrate_update = Instant::now();
            }
        } else {
            // Idle - sleep briefly to avoid busy-waiting
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Compute merkle root for a task (called once when task is assigned).
fn compute_merkle_root(task: &HashTask) -> Option<bitcoin::TxMerkleNode> {
    let template = task.template.as_ref();
    match &template.merkle_root {
        MerkleRootKind::Fixed(mr) => Some(*mr),
        MerkleRootKind::Computed(_) => {
            let en2 = task.en2.as_ref()?;
            template.compute_merkle_root(en2).ok()
        }
    }
}

/// Try a single nonce and return a share if it meets the task's share target.
fn try_nonce(task: &HashTask, merkle_root: bitcoin::TxMerkleNode, nonce: u32) -> Option<Share> {
    let template = task.template.as_ref();

    // Build block header and compute double-SHA256
    let header = BlockHeader {
        version: template.version.base(),
        prev_blockhash: template.prev_blockhash,
        merkle_root,
        time: task.ntime,
        bits: template.bits,
        nonce,
    };
    let hash = header.block_hash();

    if task.share_target.is_met_by(hash) {
        Some(Share {
            nonce,
            hash,
            version: template.version.base(),
            ntime: task.ntime,
            extranonce2: task.en2,
            expected_hashes: U256::from(task.share_target.to_work()),
        })
    } else {
        None
    }
}

/// Update shared status.
fn update_status(status: &Arc<RwLock<HashThreadStatus>>, is_active: bool, shares_found: u64) {
    let mut s = status.write().unwrap();
    s.is_active = is_active;
    s.chip_shares_found = shares_found;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job_source::{GeneralPurposeBits, JobTemplate, MerkleRootKind, VersionTemplate};
    use bitcoin::hashes::Hash;
    use bitcoin::pow::Target;
    use std::sync::Arc;
    use tokio::sync::mpsc as tokio_mpsc;

    /// Create a test task with a very easy target (high difficulty threshold).
    fn make_test_task() -> HashTask {
        // Use a very easy target so we find shares quickly
        // Target with leading zeros removed = easy to meet
        let easy_target = Target::from_be_bytes([
            0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff,
        ]);

        let template = Arc::new(JobTemplate {
            id: "test-job".into(),
            prev_blockhash: bitcoin::BlockHash::all_zeros(),
            version: VersionTemplate::new(
                bitcoin::block::Version::from_consensus(0x20000000),
                GeneralPurposeBits::none(),
            )
            .unwrap(),
            bits: bitcoin::pow::CompactTarget::from_consensus(0x1d00ffff),
            share_target: easy_target,
            time: 1234567890,
            merkle_root: MerkleRootKind::Fixed(bitcoin::TxMerkleNode::all_zeros()),
        });

        let (share_tx, _share_rx) = tokio_mpsc::channel(100);

        HashTask {
            template,
            en2_range: None,
            en2: None,
            share_target: easy_target,
            ntime: 1234567890,
            share_tx,
        }
    }

    #[test]
    fn test_try_nonce_finds_easy_shares() {
        let task = make_test_task();
        let merkle_root = compute_merkle_root(&task).unwrap();

        // With such an easy target, we should find a share within a few attempts
        let mut found = false;
        for nonce in 0..1000 {
            if try_nonce(&task, merkle_root, nonce).is_some() {
                found = true;
                break;
            }
        }

        assert!(found, "Should find a share with easy target");
    }

    #[test]
    fn test_try_nonce_returns_correct_share_fields() {
        let task = make_test_task();
        let merkle_root = compute_merkle_root(&task).unwrap();

        // Find a valid share
        let share = (0..10000)
            .find_map(|nonce| try_nonce(&task, merkle_root, nonce))
            .expect("Should find a share");

        // Verify share fields match task
        assert_eq!(share.ntime, task.ntime);
        assert_eq!(share.version, task.template.version.base());
        assert!(task.share_target.is_met_by(share.hash));
    }

    #[test]
    fn test_try_nonce_with_computed_merkle_root() {
        use crate::job_source::{
            test_blocks::block_881423, Extranonce2, Extranonce2Range, MerkleRootTemplate,
        };

        // Use block 881423 test data (same as dummy source)
        let extranonce2_bytes = block_881423::extranonce2_bytes();
        let extranonce2_actual = u32::from_le_bytes(extranonce2_bytes.try_into().unwrap());

        let extranonce2_range = Extranonce2Range::new_range(
            extranonce2_actual as u64,
            extranonce2_actual as u64 + 100,
            4,
        )
        .unwrap();

        let en2 = Extranonce2::new(extranonce2_actual as u64, 4).unwrap();

        // Use an easy target for quick testing
        let easy_target = Target::from_be_bytes([
            0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff,
        ]);

        let v = block_881423::VERSION.to_consensus() as u32;
        let base_cleaned = (v & !0x1fff_e000) as i32;
        let version = VersionTemplate::new(
            bitcoin::block::Version::from_consensus(base_cleaned),
            GeneralPurposeBits::full(),
        )
        .unwrap();

        let template = Arc::new(JobTemplate {
            id: "test-computed".into(),
            prev_blockhash: *block_881423::PREV_BLOCKHASH,
            version,
            bits: *block_881423::BITS,
            share_target: easy_target,
            time: block_881423::TIME,
            merkle_root: MerkleRootKind::Computed(MerkleRootTemplate {
                coinbase1: block_881423::coinbase1_bytes().to_vec(),
                extranonce1: block_881423::extranonce1_bytes().to_vec(),
                extranonce2_range,
                coinbase2: block_881423::coinbase2_bytes().to_vec(),
                merkle_branches: block_881423::MERKLE_BRANCHES.clone(),
            }),
        });

        let (share_tx, _share_rx) = tokio_mpsc::channel(100);

        let task = HashTask {
            template,
            en2_range: None,
            en2: Some(en2),
            share_target: easy_target,
            ntime: block_881423::TIME,
            share_tx,
        };

        // With computed merkle root and easy target, we should find shares
        let merkle_root = compute_merkle_root(&task).unwrap();
        let mut found = false;
        for nonce in 0..10000 {
            if try_nonce(&task, merkle_root, nonce).is_some() {
                found = true;
                break;
            }
        }

        assert!(found, "Should find a share with computed merkle root");
    }
}
