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
use crate::types::{
    expected_time_to_share_from_target, target_for_share_rate, Difficulty, HashRate, ShareRate,
    Target,
};
use crate::u256::U256;

/// Unique identifier for a job source, assigned by the scheduler.
type SourceId = slotmap::DefaultKey;

/// Unique identifier for a hash thread, assigned by the scheduler.
type ThreadId = slotmap::DefaultKey;

/// Unique identifier for a task, assigned by the scheduler.
type TaskId = slotmap::DefaultKey;

// StreamMap type aliases for cleaner function signatures.
// These are kept as locals in run() rather than struct fields to avoid
// borrow conflicts with tokio::select!.
type SourceEventStream = StreamMap<SourceId, ReceiverStream<SourceEvent>>;
type ThreadEventStream = StreamMap<ThreadId, ReceiverStream<HashThreadEvent>>;
type ShareStream = StreamMap<TaskId, ReceiverStream<Share>>;

/// Scheduler-side bookkeeping for an active task.
///
/// Each HashTask sent to a thread has a corresponding TaskEntry in the
/// scheduler. When a share arrives on the task's channel, this provides
/// routing: which source to submit to and the job template for validation.
#[derive(Debug)]
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

    /// Maximum average share submission rate for this source.
    pub max_share_rate: Option<ShareRate>,
}

/// Internal scheduler tracking for a registered source.
#[derive(Debug)]
struct SourceEntry {
    /// Source name for logging
    name: String,

    /// Command channel for sending to this source
    command_tx: mpsc::Sender<SourceCommand>,

    /// Last job received from this source (for assigning to newly-arriving threads)
    last_job: Option<Arc<JobTemplate>>,

    /// Maximum average share submission rate for this source.
    max_share_rate: Option<ShareRate>,
}

/// Whether to update alongside existing work or replace it.
#[derive(Debug)]
enum AssignMode {
    /// Add new task alongside existing (UpdateJob behavior)
    Update,
    /// Invalidate old tasks, replace current work (ReplaceJob behavior)
    Replace,
}

/// Core scheduler state.
///
/// StreamMaps are kept separate (in `run()`) to avoid borrow conflicts with
/// `tokio::select!`. This struct holds the business state that methods operate
/// on.
struct Scheduler {
    /// Source storage and command channels
    sources: SlotMap<SourceId, SourceEntry>,

    /// Thread storage
    threads: SlotMap<ThreadId, Box<dyn HashThread>>,

    /// Task bookkeeping (maps tasks to sources/threads)
    tasks: SlotMap<TaskId, TaskEntry>,

    /// Mining statistics
    stats: MiningStats,

    /// Track sources warned about high difficulty (reset on hashrate change)
    difficulty_warned_sources: HashSet<SourceId>,

    /// Track thread count for disconnect detection
    last_thread_count: usize,
}

impl Scheduler {
    fn new() -> Self {
        Self {
            sources: SlotMap::new(),
            threads: SlotMap::new(),
            tasks: SlotMap::new(),
            stats: MiningStats::default(),
            difficulty_warned_sources: HashSet::new(),
            last_thread_count: 0,
        }
    }

    /// Calculates aggregate hashrate estimate from all registered threads.
    ///
    /// Uses static estimates from capabilities. Useful for initial hashrate
    /// before measurements are available.
    fn estimated_hashrate(&self) -> HashRate {
        let total: u64 = self
            .threads
            .values()
            .map(|t| t.capabilities().hashrate_estimate.0)
            .sum();
        HashRate(total)
    }

    /// Calculates aggregate measured hashrate from all registered threads.
    ///
    /// Falls back to estimated hashrate if no measurements available yet.
    fn measured_hashrate(&self) -> HashRate {
        let total: u64 = self.threads.values().map(|t| t.status().hashrate.0).sum();
        let measured = HashRate(total);

        // Fall back to estimate if no measurements yet
        if measured.is_zero() {
            self.estimated_hashrate()
        } else {
            measured
        }
    }

    /// Compute the share_target for a HashTask.
    ///
    /// Applies the source's rate limit (if any) to avoid flooding. Returns
    /// the harder of the source's target or the rate-limited target.
    fn compute_share_target(
        max_share_rate: Option<ShareRate>,
        hashrate: HashRate,
        source_target: Target,
    ) -> Target {
        let Some(max_rate) = max_share_rate else {
            return source_target;
        };

        if hashrate.is_zero() {
            return source_target;
        }

        let rate_limit_target = target_for_share_rate(max_rate, hashrate);

        // Return the harder target (smaller value = higher difficulty)
        std::cmp::min(source_target, rate_limit_target)
    }

    /// Collects hashrate command senders from all sources.
    ///
    /// Used with `broadcast_hashrate()` to avoid capturing `&self` across
    /// await points (Scheduler contains Box<dyn HashThread> which isn't Sync).
    fn hashrate_senders(&self) -> Vec<mpsc::Sender<SourceCommand>> {
        self.sources
            .values()
            .map(|s| s.command_tx.clone())
            .collect()
    }

    /// Remove tasks matching a predicate, closing their share channels.
    fn remove_tasks_where(
        &mut self,
        share_channels: &mut ShareStream,
        predicate: impl Fn(&TaskEntry) -> bool,
    ) {
        let task_ids: Vec<TaskId> = self
            .tasks
            .iter()
            .filter(|(_, entry)| predicate(entry))
            .map(|(id, _)| id)
            .collect();

        for task_id in task_ids {
            self.tasks.remove(task_id);
            share_channels.remove(&task_id);
        }
    }

    /// Handle registration of a new job source.
    async fn handle_source_registration(
        &mut self,
        registration: SourceRegistration,
        source_events: &mut SourceEventStream,
    ) {
        let source_id = self.sources.insert(SourceEntry {
            name: registration.name.clone(),
            command_tx: registration.command_tx,
            last_job: None,
            max_share_rate: registration.max_share_rate,
        });
        source_events.insert(source_id, ReceiverStream::new(registration.event_rx));
        debug!(source_id = ?source_id, name = %registration.name, "Source registered");

        // Send current hashrate estimate to the new source
        let hashrate = self.measured_hashrate();
        let _ = self.sources[source_id]
            .command_tx
            .send(SourceCommand::UpdateHashRate(hashrate))
            .await;
    }

    /// Assign or replace work on all threads from a job template.
    async fn assign_job_to_threads(
        &mut self,
        mode: AssignMode,
        source_id: SourceId,
        job_template: JobTemplate,
        share_channels: &mut ShareStream,
    ) {
        let source_name = self
            .sources
            .get(source_id)
            .map(|s| s.name.clone())
            .unwrap_or_else(|| "unknown".to_string());

        // Extract EN2 range (only supported for computed merkle roots)
        let full_en2_range = match &job_template.merkle_root {
            MerkleRootKind::Computed(template) => template.extranonce2_range.clone(),
            MerkleRootKind::Fixed(_) => {
                error!(job_id = %job_template.id, "Header-only jobs not supported");
                return;
            }
        };

        let template = Arc::new(job_template);

        // Cache job for newly-arriving threads
        if let Some(source) = self.sources.get_mut(source_id) {
            source.last_job = Some(template.clone());
        }

        // Skip assignment if no threads registered yet
        if self.threads.is_empty() {
            debug!(source = %source_name, "No threads yet, job cached for later");
            return;
        }

        // Check if difficulty is reasonable for our hashrate (once per source)
        if !self.difficulty_warned_sources.contains(&source_id) {
            let hashrate = self.measured_hashrate();
            if warn_if_difficulty_too_high(&template, hashrate, &source_name) {
                self.difficulty_warned_sources.insert(source_id);
            }
        }

        // If replacing, invalidate old tasks for this source first
        if matches!(mode, AssignMode::Replace) {
            self.remove_tasks_where(share_channels, |e| e.source_id == source_id);
        }

        // Split EN2 range among all threads
        let en2_slices = full_en2_range
            .split(self.threads.len())
            .expect("Failed to split EN2 range among threads");

        // Compute share_target with rate limiting applied
        let max_share_rate = self.sources.get(source_id).and_then(|s| s.max_share_rate);
        let hashrate = self.measured_hashrate();
        let share_target =
            Self::compute_share_target(max_share_rate, hashrate, template.share_target);

        // Assign work to all threads
        for ((thread_id, thread), en2_range) in self.threads.iter_mut().zip(en2_slices) {
            let starting_en2 = en2_range.iter().next();

            // Create share channel for this task
            let (share_tx, share_rx) = mpsc::channel(32);

            let hash_task = HashTask {
                template: template.clone(),
                en2_range: Some(en2_range),
                en2: starting_en2,
                share_target,
                ntime: template.time,
                share_tx,
            };

            let result = match mode {
                AssignMode::Update => thread.update_task(hash_task).await,
                AssignMode::Replace => thread.replace_task(hash_task).await,
            };

            if let Err(e) = result {
                error!(thread = %thread.name(), error = %e, "Failed to assign task");
            } else {
                let task_id = self.tasks.insert(TaskEntry {
                    source_id,
                    template: template.clone(),
                    thread_id,
                });
                share_channels.insert(task_id, ReceiverStream::new(share_rx));
            }
        }
    }

    /// Handle ClearJobs event from a source.
    fn handle_clear_jobs(&mut self, source_id: SourceId, share_channels: &mut ShareStream) {
        let source_name = self
            .sources
            .get(source_id)
            .map(|s| s.name.as_str())
            .unwrap_or("unknown");
        debug!(source = %source_name, "ClearJobs received");

        // Clear cached job so newly-arriving threads don't get stale work
        if let Some(source) = self.sources.get_mut(source_id) {
            source.last_job = None;
        }

        // Remove tasks for this source (channels close, stale shares fail)
        self.remove_tasks_where(share_channels, |e| e.source_id == source_id);
    }

    /// Handle a share arriving from a task's channel.
    async fn handle_share(&mut self, task_id: TaskId, share: Share) {
        // Look up task context for routing
        let Some(task_entry) = self.tasks.get(task_id) else {
            // Task was removed (ReplaceJob/ClearJobs) but share arrived
            // before channel closed. This is normal; just drop the share.
            trace!(task_id = ?task_id, "Share for removed task (dropped)");
            return;
        };

        // Extract fields for logging (share may be consumed on submission)
        let nonce = share.nonce;
        let hash = share.hash;
        let share_difficulty = Difficulty::from_hash(&hash);
        let threshold = Difficulty::from_target(task_entry.template.share_target);

        debug!(
            source = %self.sources.get(task_entry.source_id).map(|s| s.name.as_str()).unwrap_or("unknown"),
            job_id = %task_entry.template.id,
            nonce = format!("{:#x}", nonce),
            hash = %hash,
            share_difficulty = %share_difficulty,
            threshold = %threshold,
            "Share found"
        );

        // Track hashes for hashrate measurement (see MiningStats doc)
        self.stats.total_hashes += share.expected_hashes;

        // Check if share meets source threshold
        if task_entry.template.share_target.is_met_by(hash) {
            self.stats.shares_submitted += 1;

            // Submit share to originating source
            if let Some(source) = self.sources.get(task_entry.source_id) {
                let source_share = SourceShare::from((share, task_entry.template.id.clone()));

                if let Err(e) = source
                    .command_tx
                    .send(SourceCommand::SubmitShare(source_share))
                    .await
                {
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
                source = %self.sources.get(task_entry.source_id).map(|s| s.name.as_str()).unwrap_or("unknown"),
                job_id = %task_entry.template.id,
                nonce = format!("{:#x}", nonce),
                share_difficulty = %share_difficulty,
                threshold = %threshold,
                "Share below source threshold (not submitted)"
            );
        }
    }

    /// Handle an event from a hash thread.
    fn handle_thread_event(&mut self, thread_id: ThreadId, event: HashThreadEvent) {
        let thread_name = self
            .threads
            .get(thread_id)
            .map(|t| t.name())
            .unwrap_or("unknown");

        match event {
            HashThreadEvent::WorkExhausted { en2_searched } => {
                info!(thread = %thread_name, en2_searched, "Work exhausted");
                // TODO: Assign new work to this thread
            }

            HashThreadEvent::WorkDepletionWarning {
                estimated_remaining_ms,
            } => {
                debug!(thread = %thread_name, remaining_ms = estimated_remaining_ms, "Work depletion warning");
                // TODO: Prepare next work assignment
            }

            HashThreadEvent::StatusUpdate(status) => {
                trace!(
                    thread = %thread_name,
                    hashrate = %status.hashrate.to_human_readable(),
                    active = status.is_active,
                    "Thread status"
                );
            }
        }
    }

    /// Handle a new thread arriving from the backplane.
    async fn handle_new_thread(
        &mut self,
        mut thread: Box<dyn HashThread>,
        thread_events: &mut ThreadEventStream,
        share_channels: &mut ShareStream,
    ) {
        let event_rx = thread
            .take_event_receiver()
            .expect("Thread missing event receiver");

        let thread_name = thread.name().to_string();
        let thread_id = self.threads.insert(thread);
        thread_events.insert(thread_id, ReceiverStream::new(event_rx));
        debug!(thread = %thread_name, "Thread registered");

        // Broadcast updated hashrate to all sources
        let hashrate = self.measured_hashrate();
        let senders = self.hashrate_senders();
        broadcast_hashrate(senders, hashrate).await;

        // Reset difficulty warnings since hashrate changed
        self.difficulty_warned_sources.clear();

        self.last_thread_count = thread_events.len();

        // Compute hashrate once for all sources
        let hashrate = self.measured_hashrate();

        // Assign cached jobs from all sources to the new thread
        for (source_id, source) in self.sources.iter() {
            let Some(template) = &source.last_job else {
                continue;
            };

            // Extract full EN2 range (new thread overlaps with others)
            let full_en2_range = match &template.merkle_root {
                MerkleRootKind::Computed(t) => t.extranonce2_range.clone(),
                MerkleRootKind::Fixed(_) => continue,
            };

            // Compute share_target with rate limiting applied
            let share_target =
                Self::compute_share_target(source.max_share_rate, hashrate, template.share_target);

            let (share_tx, share_rx) = mpsc::channel(32);
            let hash_task = HashTask {
                template: template.clone(),
                en2_range: Some(full_en2_range.clone()),
                en2: full_en2_range.iter().next(),
                share_target,
                ntime: template.time,
                share_tx,
            };

            let thread = self
                .threads
                .get_mut(thread_id)
                .expect("Just inserted thread");
            if let Err(e) = thread.update_task(hash_task).await {
                error!(thread = %thread.name(), error = %e, "Failed to assign cached job");
            } else {
                let task_id = self.tasks.insert(TaskEntry {
                    source_id,
                    template: template.clone(),
                    thread_id,
                });
                share_channels.insert(task_id, ReceiverStream::new(share_rx));
                debug!(
                    thread = %thread.name(),
                    source = %source.name,
                    job_id = %template.id,
                    "Assigned cached job to new thread"
                );
            }
        }
    }

    /// Detect and handle thread disconnections.
    async fn handle_thread_disconnections(
        &mut self,
        thread_events: &ThreadEventStream,
        share_channels: &mut ShareStream,
    ) {
        let current_count = thread_events.len();
        if current_count == self.last_thread_count {
            return;
        }

        debug!(
            previous = self.last_thread_count,
            current = current_count,
            "Thread count changed"
        );

        // Remove threads that no longer have active event streams
        let active_thread_ids: HashSet<_> = thread_events.keys().collect();
        self.threads.retain(|id, _| active_thread_ids.contains(&id));

        // Remove tasks for disconnected threads
        self.remove_tasks_where(share_channels, |e| {
            !active_thread_ids.contains(&e.thread_id)
        });

        self.last_thread_count = current_count;

        // Broadcast updated hashrate to all sources
        let hashrate = self.measured_hashrate();
        let senders = self.hashrate_senders();
        broadcast_hashrate(senders, hashrate).await;

        // Reset difficulty warnings since hashrate changed
        self.difficulty_warned_sources.clear();
    }

    /// Main scheduler loop.
    async fn run(
        &mut self,
        running: CancellationToken,
        mut thread_rx: mpsc::Receiver<Box<dyn HashThread>>,
        mut source_reg_rx: mpsc::Receiver<SourceRegistration>,
    ) {
        // StreamMaps as locals (not in self) to avoid borrow conflicts in select!
        let mut source_events: SourceEventStream = StreamMap::new();
        let mut thread_events: ThreadEventStream = StreamMap::new();
        let mut share_channels: ShareStream = StreamMap::new();

        // Create interval for periodic status logging
        let mut status_interval = tokio::time::interval(Duration::from_secs(30));
        status_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut first_status_tick = true;

        // Create interval for periodic hashrate broadcasts to sources
        let mut hashrate_interval = tokio::time::interval(Duration::from_secs(10));
        hashrate_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut first_hashrate_tick = true;

        while !running.is_cancelled() {
            tokio::select! {
                // Source registration
                Some(registration) = source_reg_rx.recv() => {
                    self.handle_source_registration(registration, &mut source_events).await;
                }

                // Source events
                Some((source_id, event)) = source_events.next() => {
                    let source_name = self.sources.get(source_id)
                        .map(|s| s.name.as_str())
                        .unwrap_or("unknown");

                    match event {
                        SourceEvent::UpdateJob(job_template) => {
                            debug!(
                                source = %source_name,
                                job_id = %job_template.id,
                                "UpdateJob received"
                            );
                            self.assign_job_to_threads(
                                AssignMode::Update,
                                source_id,
                                job_template,
                                &mut share_channels,
                            ).await;
                        }

                        SourceEvent::ReplaceJob(job_template) => {
                            debug!(
                                source = %source_name,
                                job_id = %job_template.id,
                                "ReplaceJob received"
                            );
                            self.assign_job_to_threads(
                                AssignMode::Replace,
                                source_id,
                                job_template,
                                &mut share_channels,
                            ).await;
                        }

                        SourceEvent::ClearJobs => {
                            self.handle_clear_jobs(source_id, &mut share_channels);
                        }
                    }
                }

                // Share channels (from tasks)
                Some((task_id, share)) = share_channels.next() => {
                    self.handle_share(task_id, share).await;
                }

                // Thread events
                Some((thread_id, event)) = thread_events.next() => {
                    self.handle_thread_event(thread_id, event);
                }

                // New thread from backplane
                Some(thread) = thread_rx.recv() => {
                    self.handle_new_thread(thread, &mut thread_events, &mut share_channels).await;
                }

                // Periodic status logging
                _ = status_interval.tick() => {
                    if first_status_tick {
                        first_status_tick = false;
                    } else {
                        self.stats.log_summary();
                    }
                }

                // Periodic hashrate broadcast to sources
                _ = hashrate_interval.tick() => {
                    if first_hashrate_tick {
                        first_hashrate_tick = false;
                    } else {
                        let hashrate = self.measured_hashrate();
                        let senders = self.hashrate_senders();
                        broadcast_hashrate(senders, hashrate).await;
                    }
                }

                // Shutdown
                _ = running.cancelled() => {
                    debug!("Scheduler shutdown requested");
                    break;
                }
            }

            // Detect thread disconnections (StreamMap silently removes ended streams)
            self.handle_thread_disconnections(&thread_events, &mut share_channels)
                .await;
        }

        // Log final statistics
        self.stats.log_summary();

        debug!("Scheduler shutdown complete");
    }
}

/// Broadcasts hashrate update to all registered sources.
///
/// Takes pre-collected senders to avoid capturing Scheduler across await
/// points (it contains Box<dyn HashThread> which isn't Sync).
async fn broadcast_hashrate(senders: Vec<mpsc::Sender<SourceCommand>>, hashrate: HashRate) {
    for sender in senders {
        let _ = sender.send(SourceCommand::UpdateHashRate(hashrate)).await;
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
    if hashrate.is_zero() {
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

/// Run the scheduler task, receiving hash threads and job sources.
pub async fn task(
    running: CancellationToken,
    thread_rx: mpsc::Receiver<Box<dyn HashThread>>,
    source_reg_rx: mpsc::Receiver<SourceRegistration>,
) {
    let mut scheduler = Scheduler::new();
    scheduler.run(running, thread_rx, source_reg_rx).await;
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
#[derive(Debug)]
struct MiningStats {
    start_time: std::time::Instant,
    /// Total hashes performed (accumulated across all shares).
    ///
    /// Uses U256 for overflow safety and to match Share::expected_hashes.
    total_hashes: U256,
    shares_submitted: u64,
}

impl Default for MiningStats {
    fn default() -> Self {
        Self {
            start_time: std::time::Instant::now(),
            total_hashes: U256::ZERO,
            shares_submitted: 0,
        }
    }
}

impl MiningStats {
    fn log_summary(&self) {
        let elapsed = self.start_time.elapsed();

        let hashrate_str = if self.total_hashes != U256::ZERO && elapsed.as_secs() > 0 {
            let rate = HashRate((self.total_hashes / elapsed.as_secs()).saturating_to_u64());
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
