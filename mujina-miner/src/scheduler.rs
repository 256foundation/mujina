//! The scheduler module manages the distribution of mining jobs to hash boards
//! and ASIC chips.
//!
//! # Share Filtering
//!
//! The scheduler receives ALL shares from HashThreads and performs final
//! filtering before forwarding to JobSources:
//!
//! - HashThreads forward all chip shares (hash already computed)
//! - Scheduler filters by job target (only pool-worthy shares submitted)
//! - Scheduler uses all shares for per-thread hashrate measurement
//! - Scheduler tracks chip health across all threads
//!
//! This centralized filtering provides accurate monitoring while keeping
//! thread implementations simple.
//!
//! This is a work-in-progress. It's currently the main and initial place where
//! functionality is added, after which the functionality is refactored out to
//! where it belongs.

use std::collections::HashMap;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::hash_thread::{HashThread, HashThreadEvent};
use crate::job_generator::JobGenerator;
use crate::tracing::prelude::*;

// TODO: Future enhancements for frequency ramping:
// - Make ramp parameters configurable (step size, delay, target)
// - Monitor chip temperature/errors during ramp
// - Coordinate with board-level voltage regulators
// - Implement adaptive ramping based on chip response
// - Add rollback on errors during ramp

/// Run the scheduler task, receiving hash threads from the backplane.
pub async fn task(
    running: CancellationToken,
    mut thread_rx: mpsc::Receiver<Vec<Box<dyn HashThread>>>,
) {
    // Wait for the first set of hash threads from the backplane
    let threads = match thread_rx.recv().await {
        Some(threads) => {
            info!("Received {} hash thread(s) from backplane", threads.len());
            threads
        }
        None => return,
    };

    // Store threads and get their event receivers
    // For now, we only support one thread (Bitaxe Gamma has 1 chip)
    let mut thread = threads
        .into_iter()
        .next()
        .expect("Should have at least one thread");

    info!("Received hash thread from backplane");

    // Get the event receiver from the thread
    let mut event_rx = match thread.take_event_receiver() {
        Some(rx) => rx,
        None => {
            error!("Thread was not initialized properly - no event receiver available");
            return;
        }
    };

    // Create job generator for testing (using difficulty 1 for easy verification)
    let difficulty = 1;
    let _job_generator = JobGenerator::new(difficulty);
    info!("Created job generator with difficulty {}", difficulty);

    // Track active jobs for nonce verification
    let _active_jobs: HashMap<u64, crate::asic::MiningJob> = HashMap::new();

    // Track mining statistics
    let mut stats = MiningStats {
        difficulty: difficulty as f64,
        ..Default::default()
    };

    // TODO: Assign initial work to thread via thread.update_work()
    // For now, thread starts idle - work assignment will be implemented later
    info!("Thread ready (idle, awaiting work assignment implementation)");

    // Main scheduler loop
    info!("Starting mining scheduler");

    while !running.is_cancelled() {
        tokio::select! {
            // Handle hash thread events
            Some(event) = event_rx.recv() => {
                match event {
                    HashThreadEvent::ShareFound(share) => {
                        info!("Share found! Job {} nonce {:#x}", share.job_id, share.nonce);
                        stats.nonces_found += 1;
                        stats.valid_nonces += 1;
                        // TODO: Verify share and submit to pool
                    }

                    HashThreadEvent::WorkExhausted { en2_searched } => {
                        info!("Thread exhausted work (searched {} EN2 values)", en2_searched);
                        stats.jobs_completed += 1;

                        // TODO: Assign new work via thread.update_work()
                        // For now, we don't have work assignment implemented
                        warn!("Work exhausted but new work assignment not yet implemented");
                    }

                    HashThreadEvent::WorkDepletionWarning { estimated_remaining_ms } => {
                        debug!("Work depletion warning: ~{}ms remaining", estimated_remaining_ms);
                        // TODO: Prepare next work assignment
                    }

                    HashThreadEvent::StatusUpdate(status) => {
                        trace!("Thread status: hashrate={:.2} GH/s, active={}",
                               status.hashrate / 1_000_000_000.0, status.is_active);
                    }

                    HashThreadEvent::GoingOffline => {
                        warn!("Hash thread going offline");
                        // Thread is shutting down (board removed, fault, etc.)
                        // TODO: Handle thread removal, reassign work to other threads
                        running.cancel();  // For now, just shut down
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

    info!("Scheduler shutdown complete");
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
