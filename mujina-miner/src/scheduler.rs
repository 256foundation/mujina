//! The scheduler module manages the distribution of mining jobs to hash boards
//! and ASIC chips.
//!
//! # Share Filtering (Three-Layer Architecture)
//!
//! Share filtering happens at three independent levels:
//!
//! **Layer 1 - Chip TicketMask (hardware pre-filter):**
//! - Configured by thread during initialization
//! - Chip only reports nonces meeting this threshold
//! - Set for frequent health signals (~1/sec at current hashrate)
//!
//! **Layer 2 - HashTask.share_target (thread-to-scheduler filter):**
//! - Configured by scheduler when assigning work
//! - Thread validates and sends shares meeting this via task's share channel
//! - Controls message volume to scheduler
//! - Allows per-thread difficulty adjustment
//!
//! **Layer 3 - JobTemplate.share_target (scheduler-to-source filter):**
//! - Set by pool via Stratum mining.set_difficulty
//! - Scheduler validates before forwarding to source
//! - Only pool-worthy shares submitted
//!
//! The scheduler receives shares meeting HashTask.share_target, uses them for
//! statistics and monitoring, then filters again before pool submission. This
//! provides accurate per-thread metrics while controlling network traffic.
//!
//! This is a work-in-progress. It's currently the main and initial place where
//! functionality is added, after which the functionality is refactored out to
//! where it belongs.

use slotmap::SlotMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{StreamExt, StreamMap};
use tokio_util::sync::CancellationToken;

use crate::asic::hash_thread::{HashTask, HashThread, HashThreadEvent, Share};
use crate::job_source::{
    JobTemplate, MerkleRootKind, Share as SourceShare, SourceCommand, SourceEvent,
};
use crate::tracing::prelude::*;
use crate::types::{expected_time_to_share_from_target, HashRate};

/// Unique identifier for a job source, assigned by the scheduler.
pub type SourceId = slotmap::DefaultKey;

/// Unique identifier for a hash thread, assigned by the scheduler.
pub type ThreadId = slotmap::DefaultKey;

/// Unique identifier for a task, assigned by the scheduler.
pub type TaskId = slotmap::DefaultKey;

/// Scheduler-side bookkeeping for an active task.
///
/// Each HashTask sent to a thread has a corresponding TaskEntry in the
/// scheduler. When a share arrives on the task's channel, this provides
/// routing: which source to submit to and the job template for validation.
struct TaskEntry {
    /// Source that provided this job
    source_id: SourceId,

    /// Job template (shared with the HashTask sent to thread)
    template: Arc<JobTemplate>,

    /// Thread this task was assigned to
    thread_id: ThreadId,
}

/// Registration message for adding a job source to the scheduler.
///
/// The daemon creates sources and sends this message to register them.
/// The scheduler inserts the source into its SlotMap and begins listening
/// for events.
pub struct SourceRegistration {
    /// Source name for logging
    pub name: String,

    /// Event receiver for this source (UpdateJob, ReplaceJob, ClearJobs)
    pub event_rx: mpsc::Receiver<SourceEvent>,

    /// Command sender for this source (SubmitShare, etc.)
    pub command_tx: mpsc::Sender<SourceCommand>,
}

/// Internal scheduler tracking for a registered source.
struct SourceEntry {
    /// Source name for logging
    name: String,

    /// Command channel for sending to this source
    command_tx: mpsc::Sender<SourceCommand>,
}

/// Calculates aggregate hashrate from all registered threads.
fn total_hashrate_estimate<T>(threads: &SlotMap<ThreadId, T>) -> HashRate
where
    T: std::ops::Deref<Target = dyn HashThread>,
{
    let total: u64 = threads
        .values()
        .map(|t| t.capabilities().hashrate_estimate.0)
        .sum();
    HashRate(total)
}

/// Broadcasts hashrate update to all registered sources.
///
/// Currently sends the same aggregate hashrate to all sources. In the future,
/// this will send per-source allocations based on scheduling policy (e.g., when
/// splitting hashrate across multiple pools).
async fn broadcast_hashrate(sources: &SlotMap<SourceId, SourceEntry>, hashrate: HashRate) {
    for source in sources.values() {
        let _ = source
            .command_tx
            .send(SourceCommand::UpdateHashRate(hashrate))
            .await;
    }
}

/// Threshold for warning about high share difficulty.
///
/// If expected time to find a share exceeds this, warn the operator that the
/// pool difficulty may be misconfigured for this hashrate.
const HIGH_DIFFICULTY_THRESHOLD: Duration = Duration::from_secs(300); // 5 minutes

/// Warn if job difficulty is unreasonably high for our hashrate.
///
/// Returns `true` if warning was triggered, so caller can track and avoid
/// repeated warnings.
fn warn_if_difficulty_too_high(job: &JobTemplate, hashrate: HashRate, source_name: &str) -> bool {
    if hashrate.0 == 0 {
        return false; // Can't calculate without hashrate
    }

    let time_to_share = expected_time_to_share_from_target(job.share_target, hashrate);

    if time_to_share > HIGH_DIFFICULTY_THRESHOLD {
        warn!(
            source = %source_name,
            job_id = %job.id,
            hashrate = %hashrate.to_human_readable(),
            expected_share_interval = %format_duration(time_to_share.as_secs()),
            "Share difficulty too high for hashrate (expected > 5 min between shares)"
        );
        true
    } else {
        false
    }
}

// TODO: Future enhancements for frequency ramping:
// - Make ramp parameters configurable (step size, delay, target)
// - Monitor chip temperature/errors during ramp
// - Coordinate with board-level voltage regulators
// - Implement adaptive ramping based on chip response
// - Add rollback on errors during ramp

/// Run the scheduler task, receiving hash threads and job sources.
pub async fn task(
    running: CancellationToken,
    mut thread_rx: mpsc::Receiver<Vec<Box<dyn HashThread>>>,
    mut source_reg_rx: mpsc::Receiver<SourceRegistration>,
) {
    // Source storage and event multiplexing
    let mut sources: SlotMap<SourceId, SourceEntry> = SlotMap::new();
    let mut source_events: StreamMap<SourceId, ReceiverStream<SourceEvent>> = StreamMap::new();

    // Thread storage and event multiplexing
    let mut threads: SlotMap<ThreadId, Box<dyn HashThread>> = SlotMap::new();
    let mut thread_events: StreamMap<ThreadId, ReceiverStream<HashThreadEvent>> = StreamMap::new();

    // Task bookkeeping: each task gets an entry and a share channel
    let mut tasks: SlotMap<TaskId, TaskEntry> = SlotMap::new();
    let mut share_channels: StreamMap<TaskId, ReceiverStream<Share>> = StreamMap::new();

    // Wait for the first set of hash threads from the backplane
    let initial_threads = match thread_rx.recv().await {
        Some(threads) => threads,
        None => return,
    };

    if initial_threads.is_empty() {
        error!("No hash threads received from backplane");
        return;
    }

    debug!(
        "Received {} hash thread(s) from backplane",
        initial_threads.len()
    );

    // Insert threads into SlotMap and StreamMap
    for mut thread in initial_threads {
        let event_rx = thread
            .take_event_receiver()
            .expect("Thread missing event receiver");

        let thread_id = threads.insert(thread);
        thread_events.insert(thread_id, ReceiverStream::new(event_rx));
        debug!(thread_id = ?thread_id, "Thread registered");
    }

    // Track mining statistics
    let mut stats = MiningStats::default();

    // Create interval for periodic status logging
    let mut status_interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
    status_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut first_tick = true;

    debug!("Scheduler ready (awaiting job sources)");

    // Track thread count to detect disconnections
    let mut last_thread_count = threads.len();

    // Track sources we've warned about high difficulty (reset on hashrate change)
    let mut difficulty_warned_sources: HashSet<SourceId> = HashSet::new();

    // Main scheduler loop

    while !running.is_cancelled() {
        tokio::select! {
            // Source registration
            Some(registration) = source_reg_rx.recv() => {
                let source_id = sources.insert(SourceEntry {
                    name: registration.name.clone(),
                    command_tx: registration.command_tx,
                });
                source_events.insert(source_id, ReceiverStream::new(registration.event_rx));
                debug!(source_id = ?source_id, name = %registration.name, "Source registered");

                // Send current hashrate estimate to the new source
                let hashrate = total_hashrate_estimate(&threads);
                if let Some(source) = sources.get(source_id) {
                    let _ = source.command_tx.send(SourceCommand::UpdateHashRate(hashrate)).await;
                }
            }

            // Source events
            Some((source_id, event)) = source_events.next() => {
                let source = sources.get(source_id)
                    .expect("StreamMap returned invalid source_id");

                // TODO: Factor out common job assignment logic (EN2 extraction, ActiveJob
                // creation, range splitting, work assignment) into helper function
                match event {
                    SourceEvent::UpdateJob(job_template) => {
                        debug!(
                            source = %source.name,
                            job_id = %job_template.id,
                            "UpdateJob received"
                        );

                        // Check if difficulty is reasonable for our hashrate (once per source)
                        if !difficulty_warned_sources.contains(&source_id) {
                            let hashrate = total_hashrate_estimate(&threads);
                            if warn_if_difficulty_too_high(&job_template, hashrate, &source.name) {
                                difficulty_warned_sources.insert(source_id);
                            }
                        }

                        // Extract EN2 range (only supported for computed merkle roots)
                        let full_en2_range = match &job_template.merkle_root {
                            MerkleRootKind::Computed(template) => template.extranonce2_range.clone(),
                            MerkleRootKind::Fixed(_) => {
                                error!(job_id = %job_template.id, "Header-only jobs not supported");
                                continue;
                            }
                        };

                        let template = Arc::new(job_template);

                        // Split EN2 range among all threads
                        let en2_slices = full_en2_range.split(threads.len())
                            .expect("Failed to split EN2 range among threads");

                        // Assign work to all threads (old tasks remain valid)
                        for ((thread_id, thread), en2_range) in threads.iter_mut().zip(en2_slices) {
                            let starting_en2 = en2_range.iter().next();

                            // Create share channel for this task
                            let (share_tx, share_rx) = mpsc::channel(32);

                            let hash_task = HashTask {
                                template: template.clone(),
                                en2_range: Some(en2_range),
                                en2: starting_en2,
                                share_target: template.share_target,
                                ntime: template.time,
                                share_tx,
                            };

                            if let Err(e) = thread.update_work(hash_task).await {
                                error!(thread_id = ?thread_id, error = %e, "Failed to assign work");
                            } else {
                                // Register task in scheduler (old tasks stay active)
                                let task_id = tasks.insert(TaskEntry {
                                    source_id,
                                    template: template.clone(),
                                    thread_id,
                                });
                                share_channels.insert(task_id, ReceiverStream::new(share_rx));
                            }
                        }
                    }

                    SourceEvent::ReplaceJob(job_template) => {
                        debug!(
                            source = %source.name,
                            job_id = %job_template.id,
                            "ReplaceJob received"
                        );

                        // Check if difficulty is reasonable for our hashrate (once per source)
                        if !difficulty_warned_sources.contains(&source_id) {
                            let hashrate = total_hashrate_estimate(&threads);
                            if warn_if_difficulty_too_high(&job_template, hashrate, &source.name) {
                                difficulty_warned_sources.insert(source_id);
                            }
                        }

                        // Extract EN2 range (only supported for computed merkle roots)
                        let full_en2_range = match &job_template.merkle_root {
                            MerkleRootKind::Computed(template) => template.extranonce2_range.clone(),
                            MerkleRootKind::Fixed(_) => {
                                error!(job_id = %job_template.id, "Header-only jobs not supported");
                                continue;
                            }
                        };

                        // Invalidate old tasks for this source (close channels, stale shares fail)
                        let old_task_ids: Vec<TaskId> = tasks
                            .iter()
                            .filter(|(_, entry)| entry.source_id == source_id)
                            .map(|(id, _)| id)
                            .collect();
                        for task_id in old_task_ids {
                            tasks.remove(task_id);
                            share_channels.remove(&task_id);
                        }

                        let template = Arc::new(job_template);

                        // Split EN2 range among all threads
                        let en2_slices = full_en2_range.split(threads.len())
                            .expect("Failed to split EN2 range among threads");

                        // Replace work on all threads (old shares invalid)
                        for ((thread_id, thread), en2_range) in threads.iter_mut().zip(en2_slices) {
                            let starting_en2 = en2_range.iter().next();

                            // Create share channel for this task
                            let (share_tx, share_rx) = mpsc::channel(32);

                            let hash_task = HashTask {
                                template: template.clone(),
                                en2_range: Some(en2_range),
                                en2: starting_en2,
                                share_target: template.share_target,
                                ntime: template.time,
                                share_tx,
                            };

                            if let Err(e) = thread.replace_work(hash_task).await {
                                error!(thread_id = ?thread_id, error = %e, "Failed to replace work");
                            } else {
                                // Register new task
                                let task_id = tasks.insert(TaskEntry {
                                    source_id,
                                    template: template.clone(),
                                    thread_id,
                                });
                                share_channels.insert(task_id, ReceiverStream::new(share_rx));
                            }
                        }
                    }

                    SourceEvent::ClearJobs => {
                        debug!(source = %source.name, "ClearJobs received");

                        // Remove tasks for this source (channels close, stale shares fail)
                        // Don't idle threads---they continue with current work until
                        // it's exhausted or new work arrives from another source
                        let old_task_ids: Vec<TaskId> = tasks
                            .iter()
                            .filter(|(_, entry)| entry.source_id == source_id)
                            .map(|(id, _)| id)
                            .collect();
                        for task_id in old_task_ids {
                            tasks.remove(task_id);
                            share_channels.remove(&task_id);
                        }
                    }
                }
            }

            // Share channels (from tasks)
            Some((task_id, share)) = share_channels.next() => {
                // Look up task context for routing
                let Some(task_entry) = tasks.get(task_id) else {
                    // Task was removed (ReplaceJob/ClearJobs) but share arrived
                    // before channel closed. This is normal; just drop the share.
                    trace!(task_id = ?task_id, "Share for removed task (dropped)");
                    continue;
                };

                debug!(
                    task_id = ?task_id,
                    job_id = %task_entry.template.id,
                    nonce = format!("{:#x}", share.nonce),
                    hash = %share.hash,
                    "Share found"
                );

                // Track hashes for hashrate measurement
                // Use threshold difficulty, not achieved difficulty (see MiningStats doc)
                let hashes = (share.threshold_difficulty * (u32::MAX as f64 + 1.0)) as u128;
                stats.total_hashes += hashes;

                // Check if share meets source threshold
                if task_entry.template.share_target.is_met_by(share.hash) {
                    stats.shares_submitted += 1;

                    // Submit share to originating source
                    if let Some(source) = sources.get(task_entry.source_id) {
                        let source_share = SourceShare::from((share, task_entry.template.id.clone()));

                        if let Err(e) = source.command_tx.send(SourceCommand::SubmitShare(source_share)).await {
                            error!(
                                source_id = ?task_entry.source_id,
                                error = %e,
                                "Failed to submit share to source"
                            );
                        } else {
                            debug!(source = %source.name, "Share submitted to source");
                        }
                    } else {
                        error!(source_id = ?task_entry.source_id, "Share for unknown source");
                    }
                } else {
                    trace!(
                        task_id = ?task_id,
                        nonce = format!("{:#x}", share.nonce),
                        "Share below source threshold (not submitted)"
                    );
                }
            }

            // Thread events
            Some((thread_id, event)) = thread_events.next() => {
                match event {
                    HashThreadEvent::WorkExhausted { en2_searched } => {
                        info!(thread_id = ?thread_id, en2_searched, "Work exhausted");
                        // TODO: Assign new work to this thread
                    }

                    HashThreadEvent::WorkDepletionWarning { estimated_remaining_ms } => {
                        debug!(thread_id = ?thread_id, remaining_ms = estimated_remaining_ms, "Work depletion warning");
                        // TODO: Prepare next work assignment
                    }

                    HashThreadEvent::StatusUpdate(status) => {
                        trace!(
                            thread_id = ?thread_id,
                            hashrate = %status.hashrate.to_human_readable(),
                            active = status.is_active,
                            "Thread status"
                        );
                    }
                }
            }

            // New thread batches from backplane
            Some(new_threads) = thread_rx.recv() => {
                if new_threads.is_empty() {
                    continue;
                }

                debug!("Received {} new hash thread(s)", new_threads.len());

                for mut thread in new_threads {
                    let event_rx = thread
                        .take_event_receiver()
                        .expect("Thread missing event receiver");

                    let thread_id = threads.insert(thread);
                    thread_events.insert(thread_id, ReceiverStream::new(event_rx));
                    debug!(thread_id = ?thread_id, "Thread registered");
                }

                // Broadcast updated hashrate to all sources
                let hashrate = total_hashrate_estimate(&threads);
                broadcast_hashrate(&sources, hashrate).await;

                // Reset difficulty warnings since hashrate changed
                difficulty_warned_sources.clear();

                last_thread_count = thread_events.len();
            }

            // Periodic status check
            _ = status_interval.tick() => {
                if first_tick {
                    first_tick = false;
                } else {
                    stats.log_summary();
                }
            }

            // Shutdown
            _ = running.cancelled() => {
                debug!("Scheduler shutdown requested");
                break;
            }
        }

        // Detect thread disconnections (StreamMap silently removes ended streams)
        // Check thread_events since that's where disconnections are detected.
        let current_count = thread_events.len();
        if current_count != last_thread_count {
            debug!(
                previous = last_thread_count,
                current = current_count,
                "Thread count changed"
            );

            // Remove threads that no longer have active event streams
            let active_thread_ids: HashSet<_> = thread_events.keys().collect();
            threads.retain(|id, _| active_thread_ids.contains(&id));

            // Remove tasks for disconnected threads
            let stale_task_ids: Vec<TaskId> = tasks
                .iter()
                .filter(|(_, entry)| !active_thread_ids.contains(&entry.thread_id))
                .map(|(id, _)| id)
                .collect();
            for task_id in stale_task_ids {
                tasks.remove(task_id);
                share_channels.remove(&task_id);
            }

            last_thread_count = current_count;

            // Broadcast updated hashrate to all sources
            let hashrate = total_hashrate_estimate(&threads);
            broadcast_hashrate(&sources, hashrate).await;

            // Reset difficulty warnings since hashrate changed
            difficulty_warned_sources.clear();
        }
    }

    // Log final statistics
    stats.log_summary();

    debug!("Scheduler shutdown complete");
}

/// Format seconds as human-readable duration.
///
/// Scales format based on duration to keep output compact:
/// - Under 1 minute: "45s"
/// - Under 1 hour: "12m 30s"
/// - Under 1 day: "12h 38m"
/// - 1 day or more: "1d 12h"
fn format_duration(secs: u64) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;

    if secs >= DAY {
        let days = secs / DAY;
        let hours = (secs % DAY) / HOUR;
        format!("{}d {}h", days, hours)
    } else if secs >= HOUR {
        let hours = secs / HOUR;
        let mins = (secs % HOUR) / MINUTE;
        format!("{}h {}m", hours, mins)
    } else if secs >= MINUTE {
        let mins = secs / MINUTE;
        let s = secs % MINUTE;
        format!("{}m {}s", mins, s)
    } else {
        format!("{}s", secs)
    }
}

/// Mining statistics tracker
///
/// # Hashrate Calculation Methodology
///
/// We calculate hashrate using **threshold difficulty**, not achieved difficulty.
///
/// ## Statistical Model
///
/// - Chip hashes at constant rate (what we want to measure)
/// - Shares meeting threshold D_t arrive as Poisson process
/// - Achieved difficulty follows exponential distribution (memoryless)
/// - Each share represents expected work: D_t * 2^32 hashes
///
/// ## Comparison to Pool Statistics
///
/// Mining pools (like hydrapool) use achieved difficulty because they don't
/// control miner thresholds. We control our threshold, so we can use it
/// directly for more stable estimates.
///
/// Using achieved difficulty introduces high variance from outliers. One lucky
/// difficulty-10M share would dominate the average, incorrectly inflating
/// hashrate estimates. Threshold-based calculation is variance-minimizing.
struct MiningStats {
    start_time: std::time::Instant,
    /// Total hashes performed (accumulated across all shares).
    ///
    /// Uses u128 for overflow safety. At 1 TH/s, u64 would overflow in 5 hours
    /// but u128 won't overflow for 10 quadrillion years.
    total_hashes: u128,
    shares_submitted: u64,
}

impl Default for MiningStats {
    fn default() -> Self {
        let now = std::time::Instant::now();
        Self {
            start_time: now,
            total_hashes: 0,
            shares_submitted: 0,
        }
    }
}

impl MiningStats {
    fn log_summary(&self) {
        let elapsed = self.start_time.elapsed();

        let hashrate_str = if self.total_hashes > 0 && elapsed.as_secs() > 0 {
            let rate = HashRate((self.total_hashes as f64 / elapsed.as_secs_f64()) as u64);
            rate.to_human_readable()
        } else {
            "--".to_string()
        };

        info!(
            uptime = %format_duration(elapsed.as_secs()),
            hashrate = %hashrate_str,
            shares = self.shares_submitted,
            "Mining status."
        );
    }
}
