//! The scheduler module manages the distribution of mining jobs to hash boards
//! and ASIC chips.
//!
//! This is a work-in-progress. It's currently the main and initial place where
//! functionality is added, after which the functionality is refactored out to
//! where it belongs.

use std::collections::HashMap;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::board::{Board, BoardEvent};
use crate::job_generator::{verify_nonce, JobGenerator};
use crate::tracing::prelude::*;

// TODO: Future enhancements for frequency ramping:
// - Make ramp parameters configurable (step size, delay, target)
// - Monitor chip temperature/errors during ramp
// - Coordinate with board-level voltage regulators
// - Implement adaptive ramping based on chip response
// - Add rollback on errors during ramp

/// Run the scheduler task, receiving boards from the board manager.
pub async fn task(running: CancellationToken, mut board_rx: mpsc::Receiver<Box<dyn Board + Send>>) {
    trace!("Scheduler task started.");

    // Wait for the first board from the board manager
    let mut board = match board_rx.recv().await {
        Some(board) => {
            info!(
                "Received board from board manager: {}",
                board.board_info().model
            );
            info!("Board has {} chip(s)", board.chip_count());
            board
        }
        None => {
            error!("Board channel closed before receiving any boards");
            return;
        }
    };

    // Get the event receiver from the board
    let mut event_rx = match board.take_event_receiver() {
        Some(rx) => rx,
        None => {
            error!("Board was not initialized properly - no event receiver available");
            return;
        }
    };

    // Create job generator for testing (using difficulty 1 for easy verification)
    let difficulty = 1;
    let mut job_generator = JobGenerator::new(difficulty);
    info!("Created job generator with difficulty {}", difficulty);

    // Track active jobs for nonce verification
    let mut active_jobs: HashMap<u64, crate::asic::MiningJob> = HashMap::new();

    // Track mining statistics
    let mut stats = MiningStats {
        difficulty: difficulty as f64,
        ..Default::default()
    };

    // Send initial job to start mining
    let initial_job = job_generator.next_job();
    let job_id = initial_job.job_id;
    active_jobs.insert(job_id, initial_job.clone());

    if let Err(e) = board.send_job(&initial_job).await {
        error!("Failed to send initial job: {e}");
        return;
    }
    info!("Sent initial mining job {} to chips", job_id);

    // Main scheduler loop
    info!("Starting mining scheduler");

    while !running.is_cancelled() {
        tokio::select! {
            // Handle board events
            Some(event) = event_rx.recv() => {
                match event {
                    BoardEvent::NonceFound(nonce_result) => {
                        info!("Nonce found! Job {} nonce {:#x}", nonce_result.job_id, nonce_result.nonce);

                        stats.nonces_found += 1;

                        // Verify the nonce
                        if let Some(job) = active_jobs.get(&nonce_result.job_id) {
                            match verify_nonce(job, nonce_result.nonce, nonce_result.version) {
                                Ok((block_hash, valid)) => {
                                    if valid {
                                        stats.valid_nonces += 1;
                                        info!("✓ Valid nonce! Block hash: {:x}", block_hash);
                                        info!("  Job ID: {}, Nonce: {:#010x}", nonce_result.job_id, nonce_result.nonce);
                                        // TODO: Submit to pool when connected
                                    } else {
                                        stats.invalid_nonces += 1;
                                        warn!("✗ Invalid nonce - hash doesn't meet target");
                                        warn!("  Job ID: {}, Nonce: {:#010x}", nonce_result.job_id, nonce_result.nonce);
                                        warn!("  Hash: {:x}", block_hash);
                                    }
                                }
                                Err(e) => {
                                    error!("Failed to verify nonce: {}", e);
                                }
                            }
                        } else {
                            warn!("Received nonce for unknown job ID: {}", nonce_result.job_id);
                        }
                    }
                    BoardEvent::JobComplete { job_id, reason } => {
                        info!("Job {} completed: {:?}", job_id, reason);
                        stats.jobs_completed += 1;

                        // Remove completed job from tracking
                        active_jobs.remove(&job_id);

                        // Send a new job to keep the chips busy
                        let new_job = job_generator.next_job();
                        let new_job_id = new_job.job_id;
                        active_jobs.insert(new_job_id, new_job.clone());

                        if let Err(e) = board.send_job(&new_job).await {
                            error!("Failed to send new job: {e}");
                        } else {
                            debug!("Sent new job {} to chips", new_job_id);
                        }
                    }
                    BoardEvent::ChipError { chip_address, error } => {
                        error!("Chip {} error: {}", chip_address, error);
                    }
                    BoardEvent::ChipStatusUpdate { chip_address, temperature_c, frequency_mhz } => {
                        trace!("Chip {} status - temp: {:?}°C, freq: {:?}MHz",
                               chip_address, temperature_c, frequency_mhz);
                    }
                    BoardEvent::BoardFault { component, fault, recoverable } => {
                        error!("Board fault in {}: {}", component, fault);

                        if !recoverable {
                            error!("Non-recoverable board fault detected - shutting down board");
                            // Cancel all active jobs
                            active_jobs.clear();
                            // Signal shutdown
                            running.cancel();
                        } else {
                            warn!("Recoverable fault - attempting to continue operation");
                        }
                    }
                }
            }

            // Periodic status check
            _ = tokio::time::sleep(tokio::time::Duration::from_secs(30)) => {
                trace!("Scheduler heartbeat - mining active");
                stats.log_summary();
            }

            // Shutdown
            _ = running.cancelled() => {
                info!("Scheduler shutdown requested");
                break;
            }
        }
    }

    // Log final statistics
    info!("Mining session complete");
    stats.log_summary();

    // Graceful shutdown sequence
    info!("Starting graceful hardware shutdown...");

    // Stop sending new jobs by canceling any pending job
    if let Some(job_id) = active_jobs.keys().next().copied() {
        if let Err(e) = board.cancel_job(job_id).await {
            warn!("Failed to cancel active job during shutdown: {}", e);
        }
    }

    // Gracefully shutdown the board
    info!("Shutting down board");
    if let Err(e) = board.shutdown().await {
        error!("Failed to shutdown board properly: {}", e);
    }

    info!("Board shutdown complete");
    trace!("Scheduler task stopped.");
}

/// Mining statistics tracker
struct MiningStats {
    nonces_found: u64,
    valid_nonces: u64,
    invalid_nonces: u64,
    jobs_completed: u64,
    start_time: std::time::Instant,
    last_log_time: std::time::Instant,
    difficulty: f64,
}

impl Default for MiningStats {
    fn default() -> Self {
        let now = std::time::Instant::now();
        Self {
            nonces_found: 0,
            valid_nonces: 0,
            invalid_nonces: 0,
            jobs_completed: 0,
            start_time: now,
            last_log_time: now,
            difficulty: 1.0,
        }
    }
}

impl MiningStats {
    fn log_summary(&mut self) {
        let elapsed = self.start_time.elapsed().as_secs_f64();
        let _interval = self.last_log_time.elapsed().as_secs_f64();
        self.last_log_time = std::time::Instant::now();

        info!("Mining statistics:");
        info!("  Uptime: {:.0}s", elapsed);
        info!("  Difficulty: {}", self.difficulty);

        // Theoretical hashrate based on chip specifications
        // BM1370 has 1280 hash engines, each doing 1 hash per clock cycle
        const TARGET_FREQUENCY_MHZ: f32 = 500.0;
        const BM1370_HASH_ENGINES: f64 = 1280.0;
        let theoretical_hashrate_mhs = TARGET_FREQUENCY_MHZ as f64 * BM1370_HASH_ENGINES;
        info!(
            "  Hashrate (theoretical): {:.2} MH/s at {} MHz",
            theoretical_hashrate_mhs, TARGET_FREQUENCY_MHZ
        );

        // Measured hashrate from nonce finding rate
        // At difficulty D, finding a valid nonce requires searching ~(D × 2^32) hashes on average
        // This is a Poisson process, so variance is high with small sample sizes
        if elapsed > 0.0 && self.valid_nonces > 0 {
            let hashes_per_nonce = self.difficulty * (u32::MAX as f64 + 1.0);
            let estimated_total_hashes = self.valid_nonces as f64 * hashes_per_nonce;
            let measured_hashrate_mhs = (estimated_total_hashes / elapsed) / 1_000_000.0;

            info!(
                "  Hashrate (measured): {:.2} MH/s from {} valid nonces",
                measured_hashrate_mhs, self.valid_nonces
            );

            if self.valid_nonces < 10 {
                info!("    Note: High variance with <10 nonces, use theoretical hashrate instead");
            }

            let efficiency = (measured_hashrate_mhs / theoretical_hashrate_mhs) * 100.0;
            info!("  Efficiency: {:.1}%", efficiency);
        }

        info!("  Nonces found: {}", self.nonces_found);
        info!(
            "  Valid: {} ({:.2}%), Invalid: {}",
            self.valid_nonces,
            if self.nonces_found > 0 {
                self.valid_nonces as f64 / self.nonces_found as f64 * 100.0
            } else {
                0.0
            },
            self.invalid_nonces
        );
        info!("  Jobs completed: {}", self.jobs_completed);

        // Poisson process analysis: expected nonce rate at this hashrate and difficulty
        if elapsed > 0.0 && theoretical_hashrate_mhs > 0.0 {
            // At difficulty D, expected rate = hashrate / (D × 2^32)
            let expected_rate = (theoretical_hashrate_mhs * 1_000_000.0)
                / (self.difficulty * (u32::MAX as f64 + 1.0)); // nonces per second
            let expected_nonces = expected_rate * elapsed;
            info!(
                "  Expected ~{:.1} valid nonces in {:.0}s at theoretical hashrate, found {}",
                expected_nonces, elapsed, self.valid_nonces
            );

            // For a Poisson process, variance equals mean
            if expected_nonces > 1.0 {
                let std_dev = expected_nonces.sqrt();
                let lower = (expected_nonces - 2.0 * std_dev).max(0.0);
                let upper = expected_nonces + 2.0 * std_dev;
                info!(
                    "  95% confidence interval: {:.1} - {:.1} nonces",
                    lower, upper
                );
            }
        }
    }
}
