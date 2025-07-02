//! The scheduler module manages the distribution of mining jobs to hash boards
//! and ASIC chips.
//!
//! This is a work-in-progress. It's currently the main and initial place where
//! functionality is added, after which the functionality is refactored out to
//! where it belongs.

use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::board::{Board, BoardEvent};
use crate::job_generator::{verify_nonce, JobGenerator};
use crate::tracing::prelude::*;

/// Initial mining frequency in MHz (start conservative)
const INITIAL_FREQUENCY_MHZ: f32 = 200.0;
/// Target mining frequency in MHz (can be ramped up to)
const TARGET_FREQUENCY_MHZ: f32 = 500.0;
/// Frequency ramp step size in MHz
const FREQUENCY_STEP_MHZ: f32 = 25.0;
/// Delay between frequency steps to allow chip stabilization
const FREQUENCY_STEP_DELAY_MS: u64 = 500;

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

    // Configure chips for mining
    // TODO: This needs to be moved into board-specific initialization
    // if let Err(e) = configure_chips_for_mining(&mut board).await {
    //     error!("Failed to configure chips: {e}");
    //     return;
    // }

    // Create job generator for testing (using difficulty 1 for easy verification)
    let difficulty = 1.0;
    let mut job_generator = JobGenerator::new(difficulty);
    info!("Created job generator with difficulty {}", difficulty);

    // Track active jobs for nonce verification
    let mut active_jobs: HashMap<u64, crate::asic::MiningJob> = HashMap::new();

    // Track mining statistics
    let mut stats = MiningStats {
        difficulty,
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
                            match verify_nonce(job, nonce_result.nonce) {
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

    // Stop sending new jobs by canceling any pending job on current chip
    if let Some(job_id) = active_jobs.keys().next().copied() {
        if let Err(e) = board.cancel_job(job_id).await {
            warn!("Failed to cancel active job during shutdown: {}", e);
        }
    }

    // Send chain inactive command to stop hashing
    // TODO: This needs to be moved into board-specific shutdown
    // if let Err(e) = shutdown_chips(&mut board).await {
    //     error!("Failed to properly shutdown chips: {}", e);
    // }

    // Give chips time to stop hashing
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Hold chips in reset to ensure they stay in a safe state
    info!("Holding chips in reset");
    if let Err(e) = board.hold_in_reset().await {
        error!("Failed to hold chips in reset during shutdown: {}", e);
    }

    info!("Hardware shutdown complete");
    trace!("Scheduler task stopped.");
}

// // TODO: Move board-specific configuration into board implementations
// // /// Configure discovered chips for mining operation.
// // ///
// // /// This includes:
// // /// - Setting initial PLL frequency (with ramping)
// // /// - Enabling version rolling
// // /// - Configuring other chip-specific settings
// // async fn configure_chips_for_mining(board: &mut BitaxeBoard) -> Result<(), BoardError> {
//     info!("Configuring chips for mining...");
//
//     // Get chip info to determine chip type
//     let chip_infos = board.chip_infos();
//     if chip_infos.is_empty() {
//         return Err(BoardError::InitializationFailed("No chips discovered".to_string()));
//     }
//
//     // Check what type of chips we have
//     let chip_type = ChipType::from(chip_infos[0].chip_id);
//     info!("Detected chip type: {:?}", chip_type);
//
//     // Create protocol handler
//     let protocol = BM13xxProtocol::new();
//
//     // Get initialization commands for single chip (Bitaxe has one chip)
//     let init_freq = Frequency::from_mhz(INITIAL_FREQUENCY_MHZ)
//         .map_err(|e| BoardError::InitializationFailed(format!("Invalid frequency: {}", e)))?;
//
//     let init_commands = protocol.single_chip_init(init_freq);
//
//     // Send initialization commands
//     info!("Sending {} initialization commands", init_commands.len());
//     board.send_config_commands(init_commands).await?;
//
//     // Wait for chip to stabilize at initial frequency
//     tokio::time::sleep(Duration::from_millis(FREQUENCY_STEP_DELAY_MS)).await;
//
//     // Perform frequency ramping if needed
//     if TARGET_FREQUENCY_MHZ > INITIAL_FREQUENCY_MHZ {
//         info!("Starting frequency ramp from {} MHz to {} MHz",
//               INITIAL_FREQUENCY_MHZ, TARGET_FREQUENCY_MHZ);
//
//         let mut current_freq = INITIAL_FREQUENCY_MHZ;
//         while current_freq < TARGET_FREQUENCY_MHZ {
//             current_freq = (current_freq + FREQUENCY_STEP_MHZ).min(TARGET_FREQUENCY_MHZ);
//
//             let freq = Frequency::from_mhz(current_freq)
//                 .map_err(|e| BoardError::InitializationFailed(format!("Invalid frequency: {}", e)))?;
//
//             // Generate PLL commands for new frequency
//             let pll_commands = protocol.frequency_ramp(
//                 Frequency::from_mhz(current_freq - FREQUENCY_STEP_MHZ).unwrap(),
//                 freq,
//                 1  // Single step since we're doing it manually
//             );
//
//             info!("Setting frequency to {} MHz", current_freq);
//             board.send_config_commands(pll_commands).await?;
//
//             // Wait for chip to stabilize
//             tokio::time::sleep(Duration::from_millis(FREQUENCY_STEP_DELAY_MS)).await;
//         }
//
//         info!("Frequency ramp complete");
//     }
//
//     info!("Chip configuration complete");
//     Ok(())
// }
//
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

        // Calculate hashrate based on nonce finding rate (Poisson process)
        // At difficulty D, probability of finding a valid nonce is 1/(D × 2^32)
        // So if we find N nonces in time T, estimated hashes = N × D × 2^32
        // This accounts for early job termination when nonces are found

        let hashrate_total = if elapsed > 0.0 && self.valid_nonces > 0 {
            // Use valid nonces as indicator of work done
            // Each valid nonce represents ~(D × 2^32) hashes on average
            let hashes_per_nonce = self.difficulty * (u32::MAX as f64 + 1.0);
            let estimated_hashes = self.valid_nonces as f64 * hashes_per_nonce;
            estimated_hashes / elapsed
        } else if elapsed > 0.0 {
            // No nonces found yet - use job completion rate as fallback
            // This is less accurate but gives some indication
            let estimated_hashes = self.jobs_completed as f64 * (u32::MAX as f64 + 1.0) * 0.5; // Assume 50% searched on average
            estimated_hashes / elapsed
        } else {
            0.0
        };

        info!("Mining statistics:");
        info!("  Uptime: {:.0}s", elapsed);
        info!("  Difficulty: {}", self.difficulty);
        if self.valid_nonces > 0 {
            info!(
                "  Hashrate: {:.2} MH/s (estimated from {} valid nonces at difficulty {})",
                hashrate_total / 1_000_000.0,
                self.valid_nonces,
                self.difficulty
            );

            // Statistical confidence note
            if self.valid_nonces < 10 {
                info!("  Note: Hashrate estimate has high variance with <10 nonces");
            }
        } else {
            info!(
                "  Hashrate: ~{:.2} MH/s (estimated, no valid nonces yet)",
                hashrate_total / 1_000_000.0
            );
        }

        // Theoretical hashrate for BM1370 at target frequency
        // BM1370 has 1280 cores, each doing 1 hash per clock cycle
        let theoretical_hashrate = TARGET_FREQUENCY_MHZ as f64 * 1280.0; // MH/s
        info!(
            "  Theoretical: {:.2} MH/s at {} MHz",
            theoretical_hashrate, TARGET_FREQUENCY_MHZ
        );

        if hashrate_total > 0.0 && self.valid_nonces > 0 {
            let efficiency = (hashrate_total / 1_000_000.0) / theoretical_hashrate * 100.0;
            info!("  Efficiency: {:.1}%", efficiency);
        }
        info!("  Total nonces found: {}", self.nonces_found);
        info!(
            "  Valid nonces: {} ({:.2}%)",
            self.valid_nonces,
            if self.nonces_found > 0 {
                self.valid_nonces as f64 / self.nonces_found as f64 * 100.0
            } else {
                0.0
            }
        );
        info!("  Invalid nonces: {}", self.invalid_nonces);
        info!("  Jobs completed: {}", self.jobs_completed);

        // Poisson process analysis
        if elapsed > 0.0 && theoretical_hashrate > 0.0 {
            // At difficulty D, expected rate = hashrate / (D × 2^32)
            let expected_rate =
                (theoretical_hashrate * 1_000_000.0) / (self.difficulty * (u32::MAX as f64 + 1.0)); // nonces per second
            let expected_nonces = expected_rate * elapsed;
            info!(
                "  Expected ~{:.1} valid nonces in {:.0}s, found {}",
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

// /// Shutdown chips by sending chain inactive command
// async fn shutdown_chips(board: &mut BitaxeBoard) -> Result<(), BoardError> {
//     info!("Sending chain inactive command to stop hashing");
//
//     // Chain inactive command stops all chips from hashing
//     let command = Command::ChainInactive;
//
//     board.send_config_command(command).await?;
//
//     info!("Chips commanded to stop hashing");
//     Ok(())
// }
