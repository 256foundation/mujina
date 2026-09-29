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
//! **Layer 2 - HashTask.share_target (scheduler target, per-thread):**
//! - Computed per thread from that thread's hashrate
//! - Clamps source difficulty between a measurement floor (1
//!   share/sec) and a flood ceiling (10 shares/sec)
//! - Feeds per-thread hashrate estimators with frequent samples
//! - Decoupled from pool difficulty so measurement works even
//!   when pool difficulty is very high
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
use tokio::sync::{mpsc, watch};

use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{StreamExt, StreamMap};
use tokio_util::sync::CancellationToken;

use crate::api::commands::SchedulerCommand;
use crate::api_client::types::{MinerTelemetry, SourceTelemetry};
use crate::asic::hash_thread::{HashTask, HashThread, HashThreadEvent, Share};
use crate::job_source::{
    JobTemplate, MerkleRootKind, Share as SourceShare, SourceCommand, SourceEvent,
};
use crate::tracing::prelude::*;
use crate::types::{
    AlarmStatus, DebouncedAlarm, Difficulty, HashRate, HashrateEstimator, ShareRate, Target,
    expected_time_to_share_from_target,
};

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

/// Window duration for per-thread hashrate estimation.
const HASHRATE_WINDOW: Duration = Duration::from_secs(5 * 60);

/// Fallback timeout for the startup gate, armed when the enumeration-complete
/// signal arrives. Keeps a board that never reports its hashrate from blocking
/// the first source broadcast forever.
const STARTUP_GATE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a released thread has to return its first share before the
/// staggered start stops releasing (see [`Stagger`]).
const STAGGER_SHARE_TIMEOUT: Duration = Duration::from_secs(15);

/// START CHAINS ONE AT A TIME, EACH ONCE THE LAST IS PROVEN WORKING.
///
/// Opt-in, with `MUJINA_STAGGER_START_S=<spacing in seconds>`. Chains that
/// share one supply and start hashing together present it with one load step
/// the size of all of them; started in turn, each step is one chain's, and a
/// step can be attributed to the chain that made it.
///
/// At the first assignment after start or resume the extranonce2 range is
/// split across every eligible thread exactly as without this, but only the
/// first thread is given work. Each next thread is released once the one
/// before it has returned a share and at least `spacing` has passed since
/// that one was released. A released thread that returns no share within
/// [`STAGGER_SHARE_TIMEOUT`] holds the rest idle: a chain that cannot show it
/// is working is no reason to start another. A job that arrives mid-release
/// goes to the released threads only; the others get the current job, in
/// their own slice, when they are released.
///
/// With one thread there is nothing to stagger. Stops are unaffected.
struct Stagger {
    spacing: Duration,
    share_timeout: Duration,
    episode: Option<ReleaseEpisode>,
}

/// STOP CHAINS ONE AT A TIME, WHEN A PAUSE ASKS FOR IT.
///
/// Opt-in, with `MUJINA_STAGGER_STOP_S=<spacing in seconds>`. A pause idles
/// the first thread at once and the rest one per `spacing`, so a supply shared
/// by several chains sees each chain's load come off as its own step. Only a
/// deliberate pause is staggered: a safety stop or a shutdown is never delayed.
/// A pause replies when the first thread is idle; the rest follow on the
/// scheduler's clock rather than holding the loop.
struct StopStagger {
    spacing: Duration,
    queue: std::collections::VecDeque<ThreadId>,
    last_at: tokio::time::Instant,
}

impl StopStagger {
    fn from_env() -> Option<Self> {
        let raw = std::env::var("MUJINA_STAGGER_STOP_S").ok()?;
        match raw.trim().parse::<u64>() {
            Ok(secs) if secs > 0 => Some(Self {
                spacing: Duration::from_secs(secs),
                queue: std::collections::VecDeque::new(),
                last_at: tokio::time::Instant::now(),
            }),
            _ => {
                warn!(value = %raw, "MUJINA_STAGGER_STOP_S is not a positive whole number of seconds; a pause stops every chain at once");
                None
            }
        }
    }
}

/// One start: every thread in it, in release order, and how far it has got.
struct ReleaseEpisode {
    /// The split is across all of these, so a thread released late gets the
    /// slice it would have had at the start.
    order: Vec<ThreadId>,
    /// `order[..released]` have work.
    released: usize,
    last_release_at: tokio::time::Instant,
    share_since_release: bool,
    held: bool,
    /// The order length the released threads' current slices were cut for.
    /// A release after the order changed re-cuts all of them, or a late
    /// thread's slice would overlap an earlier one's.
    split_len: usize,
}

impl Stagger {
    fn from_env() -> Option<Self> {
        let raw = std::env::var("MUJINA_STAGGER_START_S").ok()?;
        match raw.trim().parse::<u64>() {
            Ok(secs) if secs > 0 => Some(Self {
                spacing: Duration::from_secs(secs),
                share_timeout: STAGGER_SHARE_TIMEOUT,
                episode: None,
            }),
            _ => {
                warn!(value = %raw, "MUJINA_STAGGER_START_S is not a positive whole number of seconds; chains start together");
                None
            }
        }
    }
}

impl ReleaseEpisode {
    /// Keep the order to the threads still eligible, first-come for newcomers,
    /// and keep the released prefix counting only threads still present.
    fn reconcile(&mut self, eligible: &[ThreadId]) {
        let released: Vec<ThreadId> = self.order[..self.released].to_vec();
        self.order.retain(|t| eligible.contains(t));
        for t in eligible {
            if !self.order.contains(t) {
                self.order.push(*t);
            }
        }
        self.released = self.order.iter().filter(|t| released.contains(t)).count();
    }
}

/// Per-thread measurement floor: minimum share rate for hashrate
/// estimation (1 share/sec).
///
/// When pool difficulty is high relative to a thread's hashrate,
/// shares arrive too infrequently for the estimator to settle. The
/// scheduler overrides with an easier target so each thread produces
/// at least this many samples per second.
const MEASUREMENT_SHARE_RATE: ShareRate = ShareRate::from_interval(Duration::from_secs(1));

/// Per-thread flood ceiling: maximum share rate to bound scheduler
/// processing and network traffic to the source (10 shares/sec).
///
/// This is deliberately much higher than a typical pool's target
/// share rate (~0.3/sec for ckpool). Capping closer to the pool's
/// target would mask the natural share flood that vardiff algorithms
/// use to raise difficulty---the pool would see a well-behaved rate
/// and never adjust. At 10/sec the pool sees a ~33x overshoot,
/// giving vardiff a clear signal to converge quickly.
const FLOOD_CAP_RATE: ShareRate = ShareRate::from_interval(Duration::from_millis(100));

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

    /// Connection URL for this source (e.g. "stratum+tcp://pool:3333").
    pub url: Option<String>,

    /// Event receiver for this source (UpdateJob, ReplaceJob, ClearJobs)
    pub event_rx: mpsc::Receiver<SourceEvent>,

    /// Command sender for this source (SubmitShare, etc.)
    pub command_tx: mpsc::Sender<SourceCommand>,
}

/// Item the backplane sends to the scheduler on the thread-registration channel.
pub enum ThreadRegistration {
    /// A new hash thread to schedule.
    Thread(Box<dyn HashThread>),

    /// Initial enumeration across all transports is complete.
    ///
    /// Sent once, after every starting thread, so the scheduler knows its
    /// initial board set is fully registered.
    InitialEnumerationComplete,
}

/// Internal scheduler tracking for a registered source.
#[derive(Debug)]
struct SourceEntry {
    /// Source name for logging
    name: String,

    /// Connection URL for this source.
    url: Option<String>,

    /// Command channel for sending to this source
    command_tx: mpsc::Sender<SourceCommand>,

    /// Last job received from this source (for assigning to newly-arriving threads)
    last_job: Option<Arc<JobTemplate>>,

    /// Debounced alarm for high-difficulty warnings.
    difficulty_alarm: DebouncedAlarm,
}

/// Whether to update alongside existing work or replace it.
#[derive(Debug, Clone)]
enum AssignMode {
    /// Add new task alongside existing (UpdateJob behavior)
    Update,
    /// Invalidate old tasks, replace current work (ReplaceJob behavior)
    Replace,
}

/// Scheduler-side bookkeeping for a hash thread.
struct ThreadEntry {
    thread: Box<dyn HashThread>,
    hashrate: HashrateEstimator,

    /// Hashrate the thread declared via `ExpectedHashRate`, `None` until its
    /// first report.
    expected: Option<HashRate>,
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
    threads: SlotMap<ThreadId, ThreadEntry>,

    /// Task bookkeeping (maps tasks to sources/threads)
    tasks: SlotMap<TaskId, TaskEntry>,

    /// Mining statistics
    stats: MiningStats,

    /// Track thread count for disconnect detection
    last_thread_count: usize,

    /// Holds the first source broadcast until startup enumeration completes
    startup_gate: StartupGate,

    /// Mining paused
    paused: bool,

    /// Staggered start, when configured.
    stagger: Option<Stagger>,

    /// Staggered stop on pause, when configured.
    stop_stagger: Option<StopStagger>,
}

impl Scheduler {
    fn new(start_paused: bool) -> Self {
        Self {
            sources: SlotMap::new(),
            threads: SlotMap::new(),
            tasks: SlotMap::new(),
            stats: MiningStats::default(),
            last_thread_count: 0,
            startup_gate: StartupGate::new(),
            paused: start_paused,
            stagger: None,
            stop_stagger: None,
        }
    }

    /// Aggregate measured hashrate from per-thread estimators.
    ///
    /// Returns the truth: zero if no shares have been recorded yet.
    fn measured_hashrate(&mut self) -> HashRate {
        self.threads
            .values_mut()
            .map(|entry| entry.hashrate.hashrate())
            .sum()
    }

    /// Aggregate of the hashrates threads declared they expect to deliver.
    ///
    /// Feed-forward, summed across threads that have reported; a thread that
    /// has not yet reported contributes nothing. Drives source difficulty and
    /// the difficulty-too-high warning, where a zero at startup is unhelpful.
    fn expected_hashrate(&self) -> HashRate {
        self.threads
            .values()
            .filter_map(|entry| entry.expected)
            .sum()
    }

    /// Expected hashrate allocated to one source.
    ///
    /// Degenerate today: the source gets the full aggregate. The signature
    /// takes a source so a proportional split across sources is a localized
    /// change here.
    fn allocated_hashrate(&self, _source_id: SourceId) -> HashRate {
        self.expected_hashrate()
    }

    /// Threads eligible for work: those that have reported an expected hashrate.
    fn eligible_thread_ids(&self) -> impl Iterator<Item = ThreadId> + '_ {
        self.threads
            .iter()
            .filter(|(_, entry)| entry.expected.is_some())
            .map(|(id, _)| id)
    }

    /// Build a [`MinerTelemetry`] snapshot from current scheduler state.
    ///
    /// The scheduler contributes aggregate stats and source info. Board
    /// and thread details come from the backplane, not the scheduler, so
    /// `boards` is left empty here.
    fn compute_miner_telemetry(&mut self) -> MinerTelemetry {
        MinerTelemetry {
            uptime_secs: self.stats.start_time.elapsed().as_secs(),
            hashrate: u64::from(self.measured_hashrate()),
            shares_submitted: self.stats.shares_submitted,
            paused: self.paused,
            boards: vec![],
            sources: self
                .sources
                .values()
                .map(|s| SourceTelemetry {
                    name: s.name.clone(),
                    url: s.url.clone(),
                    difficulty: s.last_job.as_ref().map(|j| {
                        let d = Difficulty::from_target(j.share_target).as_f64();
                        if d >= 10.0 { d.round() } else { d }
                    }),
                })
                .collect(),
        }
    }

    /// Compute the per-thread scheduler target for HashTask.
    ///
    /// Clamps the source's pool difficulty between a measurement floor
    /// (1 share/sec) and a flood ceiling (10 shares/sec). When pool
    /// difficulty falls outside this range, the scheduler target
    /// overrides it; when inside, the source target passes through.
    fn compute_scheduler_target(hashrate: HashRate, source_target: Target) -> Target {
        if hashrate.is_zero() {
            return source_target;
        }

        let measurement_target = MEASUREMENT_SHARE_RATE.to_target(hashrate);
        let flood_cap_target = FLOOD_CAP_RATE.to_target(hashrate);

        source_target.clamp(measurement_target, flood_cap_target)
    }

    /// Pairs every source's command sender with its current hashrate allocation.
    ///
    /// Collected up front so the caller can send without holding `&self`
    /// across await points (Scheduler contains Box<dyn HashThread>, not Sync).
    fn collect_hashrate_updates(&self) -> Vec<(mpsc::Sender<SourceCommand>, HashRate)> {
        self.sources
            .iter()
            .map(|(id, s)| (s.command_tx.clone(), self.allocated_hashrate(id)))
            .collect()
    }

    /// React to a change in aggregate hashrate: reset the high-difficulty
    /// warning debounce and resend every source its current allocation.
    async fn broadcast_hashrate_change(&mut self) {
        for source in self.sources.values_mut() {
            source.difficulty_alarm.reset();
        }
        let updates = self.collect_hashrate_updates();
        send_hashrate_updates(updates).await;
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
            url: registration.url,
            command_tx: registration.command_tx,
            last_job: None,
            difficulty_alarm: DebouncedAlarm::new(HIGH_DIFFICULTY_DEBOUNCE),
        });
        source_events.insert(source_id, ReceiverStream::new(registration.event_rx));
        debug!(source_id = ?source_id, name = %registration.name, "Source registered");

        // Hashrate is not yet split across sources: each is told the full
        // aggregate, which over-suggests difficulty to every pool but the one
        // that should get the whole hashrate.
        if self.sources.len() > 1 {
            warn!(
                sources = self.sources.len(),
                "Multiple sources active, but hashrate is not split across them"
            );
        }

        // A new source changes how the total is divided, so re-send every
        // source its allocation. While the startup gate holds, skip it; the
        // broadcast when the gate opens covers every source.
        if !self.startup_gate.is_holding() {
            self.broadcast_hashrate_change().await;
        }
    }

    /// Assign or replace work on all threads from a job template.
    async fn assign_job_to_threads(
        &mut self,
        mode: AssignMode,
        source_id: SourceId,
        job_template: JobTemplate,
        share_channels: &mut ShareStream,
    ) {
        // Paused means no work reaches the hardware. Checked here rather than
        // at the source, so a job arriving while paused is dropped instead of
        // queued: on resume the scheduler assigns the cached job, which is the
        // current one, not a stale one.
        if self.paused {
            debug!("Mining paused; job not assigned");
            return;
        }

        let source_name = self
            .sources
            .get(source_id)
            .map(|s| s.name.clone())
            .unwrap_or_else(|| "unknown".to_string());

        // Only computed merkle roots carry an EN2 range to split.
        if let MerkleRootKind::Fixed(_) = &job_template.merkle_root {
            error!(job_id = %job_template.id, "Header-only jobs not supported");
            return;
        }

        let template = Arc::new(job_template);

        // Reset debounce when difficulty changes so the alarm doesn't
        // fire during the transient after a pool adjustment.
        if let Some(source) = self.sources.get_mut(source_id) {
            let prev_target = source.last_job.as_ref().map(|j| j.share_target);
            if prev_target != Some(template.share_target) {
                source.difficulty_alarm.reset();
            }
            source.last_job = Some(template.clone());
        }

        // Skip assignment if no threads registered yet
        if self.threads.is_empty() {
            debug!(source = %source_name, "No threads yet, job cached for later");
            return;
        }

        // Debounced difficulty warning
        let hashrate = self.expected_hashrate();
        if let Some(source) = self.sources.get_mut(source_id) {
            let too_high = is_difficulty_too_high(&template, hashrate);
            match source.difficulty_alarm.check(too_high) {
                AlarmStatus::Triggered => {
                    let difficulty = Difficulty::from_target(template.share_target);
                    warn!(
                        source = %source_name,
                        job_id = %template.id,
                        difficulty = %difficulty,
                        hashrate = %hashrate.to_human_readable(),
                        expected_share_interval =
                            %format_duration(expected_time_to_share_from_target(
                                template.share_target, hashrate).as_secs()),
                        "Share difficulty too high for hashrate \
                         (expected > 5 min between shares)"
                    );
                }
                AlarmStatus::Resolved => {
                    info!(
                        source = %source_name,
                        "Share difficulty now acceptable for hashrate"
                    );
                }
                _ => {}
            }
        }

        // If replacing, invalidate old tasks for this source first
        if matches!(mode, AssignMode::Replace) {
            self.remove_tasks_where(share_channels, |e| e.source_id == source_id);
        }

        self.assign_template(mode, source_id, template, share_channels)
            .await;
    }

    /// Hand one job to the threads that should have it, each in its own slice
    /// of the extranonce2 range.
    ///
    /// The split is across every eligible thread. Without a staggered start
    /// every one of them gets its slice now; with one, only the released ones
    /// do (see [`Stagger`]).
    ///
    /// TODO: A thread that becomes eligible later is handed the full EN2 range
    /// on its first report, overlapping these slices until the next job
    /// re-splits.
    async fn assign_template(
        &mut self,
        mode: AssignMode,
        source_id: SourceId,
        template: Arc<JobTemplate>,
        share_channels: &mut ShareStream,
    ) {
        let full_en2_range = match &template.merkle_root {
            MerkleRootKind::Computed(t) => t.extranonce2_range.clone(),
            MerkleRootKind::Fixed(_) => return,
        };
        let eligible: Vec<ThreadId> = self.eligible_thread_ids().collect();
        if eligible.is_empty() {
            debug!(job_id = %template.id, "No eligible threads yet, job cached for later");
            return;
        }
        let (order, give) = match self.stagger.as_mut() {
            None => (eligible.clone(), eligible.len()),
            Some(stagger) => {
                let episode = stagger.episode.get_or_insert_with(|| ReleaseEpisode {
                    order: Vec::new(),
                    released: 0,
                    last_release_at: tokio::time::Instant::now(),
                    share_since_release: false,
                    held: false,
                    split_len: 0,
                });
                episode.reconcile(&eligible);
                // A held start releases nothing, on this job or any later one.
                if episode.released == 0 && !episode.held {
                    episode.released = 1;
                    episode.last_release_at = tokio::time::Instant::now();
                    info!(
                        thread = %self.threads.get(episode.order[0]).map(|e| e.thread.name()).unwrap_or("?"),
                        chains = episode.order.len(),
                        spacing_s = stagger.spacing.as_secs(),
                        "Staggered start: releasing the first chain; the next waits for its share"
                    );
                }
                episode.split_len = episode.order.len();
                (episode.order.clone(), episode.released)
            }
        };
        let en2_slices = full_en2_range
            .split(order.len())
            .expect("Failed to split EN2 range among threads");
        for (thread_id, en2_range) in order.into_iter().zip(en2_slices).take(give) {
            self.assign_slice(
                mode.clone(),
                source_id,
                &template,
                thread_id,
                en2_range,
                share_channels,
            )
            .await;
        }
    }

    /// Give one thread one job, in one slice, and record the task.
    async fn assign_slice(
        &mut self,
        mode: AssignMode,
        source_id: SourceId,
        template: &Arc<JobTemplate>,
        thread_id: ThreadId,
        en2_range: crate::job_source::Extranonce2Range,
        share_channels: &mut ShareStream,
    ) {
        let starting_en2 = en2_range.iter().next();
        let Some(entry) = self.threads.get_mut(thread_id) else {
            return;
        };
        let hashrate = entry
            .hashrate
            .settled_hashrate()
            .or(entry.expected)
            .unwrap_or_default();
        let share_target = Self::compute_scheduler_target(hashrate, template.share_target);
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
            AssignMode::Update => entry.thread.update_task(hash_task).await,
            AssignMode::Replace => entry.thread.replace_task(hash_task).await,
        };
        if let Err(e) = result {
            error!(thread = %entry.thread.name(), error = %e, "Failed to assign task");
        } else {
            let task_id = self.tasks.insert(TaskEntry {
                source_id,
                template: template.clone(),
                thread_id,
            });
            share_channels.insert(task_id, ReceiverStream::new(share_rx));
        }
    }

    /// A share came back from `thread_id`. If it is the thread released last,
    /// the next one may follow once the spacing has passed.
    fn note_release_share(&mut self, thread_id: ThreadId) {
        if let Some(episode) = self.stagger.as_mut().and_then(|s| s.episode.as_mut())
            && episode.released > 0
            && episode.order.get(episode.released - 1) == Some(&thread_id)
            && !episode.share_since_release
        {
            episode.share_since_release = true;
            // Said once per release, so a run's log shows the share each next
            // release waited for rather than implying it.
            let waited =
                tokio::time::Instant::now().saturating_duration_since(episode.last_release_at);
            let more = episode.order.len() - episode.released;
            info!(
                thread = %self.threads.get(thread_id).map(|e| e.thread.name()).unwrap_or("?"),
                after_s = waited.as_secs_f32(),
                chains_waiting = more,
                "Staggered start: the chain released last has returned its first share"
            );
        }
    }

    /// Release the next chain if it is due, or hold the rest if the last one
    /// never proved it was working. Called on a timer.
    async fn advance_release(
        &mut self,
        now: tokio::time::Instant,
        share_channels: &mut ShareStream,
    ) {
        if self.paused {
            return;
        }
        let Some(stagger) = self.stagger.as_mut() else {
            return;
        };
        let (spacing, share_timeout) = (stagger.spacing, stagger.share_timeout);
        let Some(episode) = stagger.episode.as_mut() else {
            return;
        };
        if episode.held || episode.released >= episode.order.len() {
            return;
        }
        // THE CHAIN RELEASED FIRST HAS GONE before returning a share (its
        // thread disconnected). Nothing has shown it is working, which is the
        // hold's own condition. This used to index order[released - 1] with
        // released 0 and abort the daemon (the pre-mining review).
        if episode.released == 0 {
            episode.held = true;
            error!(
                held = episode.order.len(),
                "STAGGERED START HELD: the chain released first went away before returning a share; the remaining chains stay idle"
            );
            return;
        }
        let since = now.saturating_duration_since(episode.last_release_at);
        if !episode.share_since_release {
            if since >= share_timeout {
                episode.held = true;
                let waiting = episode.order[episode.released - 1];
                let held = episode.order.len() - episode.released;
                error!(
                    thread = %self.threads.get(waiting).map(|e| e.thread.name()).unwrap_or("?"),
                    waited_s = since.as_secs(),
                    held,
                    "STAGGERED START HELD: the last chain released has returned no share; the remaining chains stay idle"
                );
            }
            return;
        }
        if since < spacing {
            return;
        }
        // No job to give (a source dropped mid-start): wait for one rather
        // than release a chain with nothing to do and then blame it for
        // returning no share.
        let jobs: Vec<(SourceId, Arc<JobTemplate>)> = self
            .sources
            .iter()
            .filter_map(|(id, source)| source.last_job.clone().map(|job| (id, job)))
            .collect();
        if jobs.is_empty() {
            return;
        }
        let Some(episode) = self.stagger.as_mut().and_then(|s| s.episode.as_mut()) else {
            return;
        };
        let index = episode.released;
        let next = episode.order[index];
        let chains = episode.order.len();
        let resplit = episode.split_len != chains;
        let already: Vec<ThreadId> = episode.order[..index].to_vec();
        episode.released += 1;
        episode.share_since_release = false;
        episode.last_release_at = now;
        episode.split_len = chains;
        info!(
            thread = %self.threads.get(next).map(|e| e.thread.name()).unwrap_or("?"),
            chain = index + 1,
            chains,
            resplit,
            "Staggered start: releasing the next chain"
        );
        for (source_id, template) in jobs {
            let MerkleRootKind::Computed(t) = &template.merkle_root else {
                continue;
            };
            let Some(slices) = t.extranonce2_range.split(chains) else {
                continue;
            };
            // THE ORDER CHANGED SINCE THE RELEASED CHAINS WERE CUT THEIR
            // SLICES (a thread joined late, or one went away): re-cut theirs
            // too, so the new chain's slice cannot lie inside one of them.
            if resplit {
                self.remove_tasks_where(share_channels, |e| {
                    e.source_id == source_id && already.contains(&e.thread_id)
                });
                for (i, thread_id) in already.iter().enumerate() {
                    self.assign_slice(
                        AssignMode::Replace,
                        source_id,
                        &template,
                        *thread_id,
                        slices[i].clone(),
                        share_channels,
                    )
                    .await;
                }
            }
            self.assign_slice(
                AssignMode::Update,
                source_id,
                &template,
                next,
                slices[index].clone(),
                share_channels,
            )
            .await;
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

        // Feed share work to per-thread hashrate estimator
        let share_thread = task_entry.thread_id;
        if let Some(entry) = self.threads.get_mut(share_thread) {
            entry.hashrate.record(share.expected_work);
        }
        self.note_release_share(share_thread);
        let Some(task_entry) = self.tasks.get(task_id) else {
            return;
        };

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
    async fn handle_thread_event(
        &mut self,
        thread_id: ThreadId,
        event: HashThreadEvent,
        share_channels: &mut ShareStream,
    ) {
        let thread_name = self
            .threads
            .get(thread_id)
            .map(|entry| entry.thread.name())
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

            HashThreadEvent::TelemetryUpdate(_) => {
                trace!(thread = %thread_name, "Thread telemetry update");
            }

            HashThreadEvent::ExpectedHashRate(rate) => {
                let Some(entry) = self.threads.get_mut(thread_id) else {
                    return;
                };
                let first_report = entry.expected.is_none();
                entry.expected = Some(rate);
                let name = entry.thread.name().to_string();
                trace!(
                    thread = %name,
                    expected = %rate.to_human_readable(),
                    "Thread declared expected hashrate"
                );

                // First report is the thread's connect: hand it any cached jobs.
                if first_report {
                    self.assign_cached_jobs_to_thread(thread_id, &name, share_channels)
                        .await;
                }

                // Count the first report toward the startup gate, then
                // broadcast only once the gate is open. While it holds, the
                // report is recorded but the broadcast is suppressed; the
                // report that opens the gate sends the first one.
                if first_report && self.startup_gate.is_holding() {
                    self.startup_gate.record_reported();
                }
                if !self.startup_gate.is_holding() {
                    self.broadcast_hashrate_change().await;
                }
            }
        }
    }

    /// Handle a new thread arriving from the backplane.
    ///
    /// Registers and configures the thread only. Work assignment and the source
    /// broadcast happen when the thread makes its first ExpectedHashRate report,
    /// not on arrival.
    async fn handle_new_thread(
        &mut self,
        mut thread: Box<dyn HashThread>,
        thread_events: &mut ThreadEventStream,
    ) {
        let event_rx = thread
            .take_event_receiver()
            .expect("Thread missing event receiver");

        let thread_name = thread.name().to_string();
        let thread_id = self.threads.insert(ThreadEntry {
            thread,
            hashrate: HashrateEstimator::new(HASHRATE_WINDOW),
            expected: None,
        });
        self.startup_gate.record_registered();
        thread_events.insert(thread_id, ReceiverStream::new(event_rx));
        debug!(thread = %thread_name, "Thread registered");

        // Configure the thread; it replies with ExpectedHashRate on its event
        // channel, handled in handle_thread_event.
        let entry = self
            .threads
            .get_mut(thread_id)
            .expect("Just inserted thread");
        if let Err(e) = entry.thread.configure().await {
            error!(thread = %thread_name, error = %e, "Failed to configure thread");
        }

        self.last_thread_count = thread_events.len();
    }

    /// Assign each source's cached job to a newly-eligible thread.
    ///
    /// Called from the thread's first ExpectedHashRate report. The thread takes
    /// the full EN2 range for each source, overlapping the other threads until
    /// the next job resplits the range.
    async fn assign_cached_jobs_to_thread(
        &mut self,
        thread_id: ThreadId,
        thread_name: &str,
        share_channels: &mut ShareStream,
    ) {
        // A board that attaches while paused stays idle. This is what makes
        // observer mode safe on a machine whose boards share one PSU rail: an
        // enumerated board draws no hashing current until mining is resumed.
        if self.paused {
            debug!(thread = %thread_name, "Mining paused; thread left idle");
            return;
        }
        // STAGGERED: a thread that reports mid-start joins the queue rather
        // than taking the whole range at once; with no start under way, the
        // cached jobs begin one, with this thread first.
        if self.stagger.is_some() {
            let joined = self
                .stagger
                .as_mut()
                .and_then(|s| s.episode.as_mut())
                .map(|episode| {
                    if !episode.order.contains(&thread_id) {
                        episode.order.push(thread_id);
                    }
                })
                .is_some();
            if !joined {
                let jobs: Vec<(SourceId, Arc<JobTemplate>)> = self
                    .sources
                    .iter()
                    .filter_map(|(id, source)| source.last_job.clone().map(|job| (id, job)))
                    .collect();
                for (source_id, template) in jobs {
                    self.assign_template(AssignMode::Update, source_id, template, share_channels)
                        .await;
                }
            }
            return;
        }

        let thread_hashrate = {
            let entry = self
                .threads
                .get_mut(thread_id)
                .expect("thread present for cached-job assignment");
            entry
                .hashrate
                .settled_hashrate()
                .or(entry.expected)
                .unwrap_or_default()
        };

        for (source_id, source) in self.sources.iter() {
            let Some(template) = &source.last_job else {
                continue;
            };

            // Extract full EN2 range (new thread overlaps with others)
            let full_en2_range = match &template.merkle_root {
                MerkleRootKind::Computed(t) => t.extranonce2_range.clone(),
                MerkleRootKind::Fixed(_) => continue,
            };

            let share_target =
                Self::compute_scheduler_target(thread_hashrate, template.share_target);

            let (share_tx, share_rx) = mpsc::channel(32);
            let hash_task = HashTask {
                template: template.clone(),
                en2_range: Some(full_en2_range.clone()),
                en2: full_en2_range.iter().next(),
                share_target,
                ntime: template.time,
                share_tx,
            };

            let entry = self
                .threads
                .get_mut(thread_id)
                .expect("Just inserted thread");
            if let Err(e) = entry.thread.update_task(hash_task).await {
                error!(thread = %thread_name, error = %e, "Failed to assign cached job");
            } else {
                let task_id = self.tasks.insert(TaskEntry {
                    source_id,
                    template: template.clone(),
                    thread_id,
                });
                share_channels.insert(task_id, ReceiverStream::new(share_rx));
                debug!(
                    thread = %thread_name,
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
        if let Some(episode) = self.stagger.as_mut().and_then(|s| s.episode.as_mut()) {
            let live: Vec<ThreadId> = episode
                .order
                .iter()
                .copied()
                .filter(|t| active_thread_ids.contains(t))
                .collect();
            episode.reconcile(&live);
        }

        // Remove tasks for disconnected threads
        self.remove_tasks_where(share_channels, |e| {
            !active_thread_ids.contains(&e.thread_id)
        });

        self.last_thread_count = current_count;

        self.broadcast_hashrate_change().await;
    }

    /// Handle an API command, sending the result back on the reply channel.
    ///
    /// Publishes an updated state snapshot before replying so the API
    /// handler's subsequent `borrow()` sees the new value.
    async fn handle_api_command(
        &mut self,
        cmd: SchedulerCommand,
        miner_telemetry_tx: &watch::Sender<MinerTelemetry>,
        share_channels: &mut ShareStream,
    ) {
        match cmd {
            SchedulerCommand::PauseMining { reply } => {
                // A pause during a staggered stop is an escalation: the chains
                // still loaded stop now rather than on a restarted clock.
                let pending: Vec<ThreadId> = self
                    .stop_stagger
                    .as_mut()
                    .map(|stop| stop.queue.drain(..).collect())
                    .unwrap_or_default();
                let second = self.paused && !pending.is_empty();
                for thread_id in pending {
                    self.idle_thread(thread_id, share_channels).await;
                }
                if second {
                    info!("Mining paused again mid-stop: every remaining chain idled at once");
                    let _ = miner_telemetry_tx.send(self.compute_miner_telemetry());
                    let _ = reply.send(Ok(()));
                    return;
                }
                self.paused = true;
                if let Some(stagger) = self.stagger.as_mut() {
                    stagger.episode = None;
                }
                if self.stop_stagger.is_some() && self.threads.len() > 1 {
                    let mut order: Vec<ThreadId> = self.threads.keys().collect();
                    let first = order.remove(0);
                    self.idle_thread(first, share_channels).await;
                    let spacing = {
                        let stop = self.stop_stagger.as_mut().expect("checked above");
                        stop.queue = order.into();
                        stop.last_at = tokio::time::Instant::now();
                        stop.spacing
                    };
                    info!(
                        remaining = self.threads.len() - 1,
                        spacing_s = spacing.as_secs(),
                        "Mining paused: first chain idled, the rest follow one per spacing"
                    );
                } else {
                    self.idle_all_threads(share_channels).await;
                    info!("Mining paused: every thread idled, no further work assigned");
                }
                let _ = miner_telemetry_tx.send(self.compute_miner_telemetry());
                let _ = reply.send(Ok(()));
            }
            SchedulerCommand::ResumeMining { reply } => {
                // A resume mid-stop finishes the stop first, so no chain is
                // still running its old task when the cached job is re-split.
                let pending: Vec<ThreadId> = self
                    .stop_stagger
                    .as_mut()
                    .map(|stop| stop.queue.drain(..).collect())
                    .unwrap_or_default();
                for thread_id in pending {
                    self.idle_thread(thread_id, share_channels).await;
                }
                self.paused = false;
                // SPLIT, as a new job is. Handing each thread the cached job
                // through the new-thread path gave every thread the WHOLE
                // extranonce2 range, so after a resume every chain hashed the
                // same space until the next job re-split it.
                let jobs: Vec<(SourceId, Arc<JobTemplate>)> = self
                    .sources
                    .iter()
                    .filter_map(|(id, source)| source.last_job.clone().map(|job| (id, job)))
                    .collect();
                for (source_id, template) in jobs {
                    self.assign_template(AssignMode::Update, source_id, template, share_channels)
                        .await;
                }
                info!(threads = self.threads.len(), "Mining resumed");
                let _ = miner_telemetry_tx.send(self.compute_miner_telemetry());
                let _ = reply.send(Ok(()));
            }
        }
    }

    /// Idle one thread and drop its tasks.
    async fn idle_thread(&mut self, thread_id: ThreadId, share_channels: &mut ShareStream) {
        if let Some(entry) = self.threads.get_mut(thread_id)
            && let Err(e) = entry.thread.go_idle().await
        {
            error!(thread = %entry.thread.name(), error = %e, "Failed to idle thread");
        }
        self.remove_tasks_where(share_channels, |e| e.thread_id == thread_id);
    }

    /// Idle the next chain of a staggered stop when it is due.
    async fn advance_stop(&mut self, now: tokio::time::Instant, share_channels: &mut ShareStream) {
        let next = match self.stop_stagger.as_mut() {
            Some(stop)
                if !stop.queue.is_empty()
                    && now.saturating_duration_since(stop.last_at) >= stop.spacing =>
            {
                stop.last_at = now;
                stop.queue.pop_front()
            }
            _ => None,
        };
        if let Some(thread_id) = next {
            let name = self
                .threads
                .get(thread_id)
                .map(|e| e.thread.name().to_string());
            self.idle_thread(thread_id, share_channels).await;
            info!(thread = %name.as_deref().unwrap_or("?"), "Staggered stop: chain idled");
        }
    }

    /// Stop every thread and drop the tasks they were running.
    ///
    /// `go_idle` is the thread's own stop: it stops dispatching and returns
    /// whatever task it held. Their tasks and share channels go with them, so
    /// a later resume assigns the current job rather than reviving a stale one.
    ///
    /// **This is a load step, and on some machines that matters.** Dispatched
    /// work is what draws current; on a series-stacked hashboard the per-ASIC
    /// voltage follows each ASIC's draw relative to its neighbours, so taking
    /// all work away at once moves operating points, not just hashrate. That
    /// is why such machines keep a floor of work under the ASICs rather than
    /// letting the pipeline run dry. Pausing a powered, loaded machine is a
    /// deliberate act with a power consequence; starting paused (observer
    /// mode) has none, because no work was ever dispatched.
    async fn idle_all_threads(&mut self, share_channels: &mut ShareStream) {
        let thread_ids: Vec<ThreadId> = self.threads.keys().collect();
        for thread_id in thread_ids {
            let Some(entry) = self.threads.get_mut(thread_id) else {
                continue;
            };
            if let Err(e) = entry.thread.go_idle().await {
                error!(thread = %entry.thread.name(), error = %e, "Failed to idle thread");
            }
        }
        self.remove_tasks_where(share_channels, |_| true);
    }

    /// Main scheduler loop.
    async fn run(
        &mut self,
        running: CancellationToken,
        mut thread_rx: mpsc::Receiver<ThreadRegistration>,
        mut source_reg_rx: mpsc::Receiver<SourceRegistration>,
        miner_telemetry_tx: watch::Sender<MinerTelemetry>,
        mut cmd_rx: mpsc::Receiver<SchedulerCommand>,
    ) {
        // StreamMaps as locals (not in self) to avoid borrow conflicts in select!
        let mut source_events: SourceEventStream = StreamMap::new();
        let mut thread_events: ThreadEventStream = StreamMap::new();
        let mut share_channels: ShareStream = StreamMap::new();

        // Create interval for periodic status logging
        let mut status_interval = tokio::time::interval(Duration::from_secs(30));
        status_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut first_status_tick = true;

        // Create interval for periodic API telemetry publishing
        let mut telemetry_interval = tokio::time::interval(Duration::from_secs(10));
        telemetry_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // The staggered start's clock: releases are checked once a second.
        let mut release_interval = tokio::time::interval(Duration::from_secs(1));
        release_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Deadline for the startup-gate fallback, set when the enumeration-
        // complete signal arrives without immediately opening the gate.
        let mut gate_deadline: Option<tokio::time::Instant> = None;

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
                    self.handle_thread_event(thread_id, event, &mut share_channels).await;
                }

                // Thread registration from backplane
                Some(registration) = thread_rx.recv() => {
                    match registration {
                        ThreadRegistration::Thread(thread) => {
                            self.handle_new_thread(thread, &mut thread_events).await;
                        }
                        ThreadRegistration::InitialEnumerationComplete => {
                            self.startup_gate.record_enumeration_complete();
                            if self.startup_gate.is_holding() {
                                gate_deadline =
                                    Some(tokio::time::Instant::now() + STARTUP_GATE_TIMEOUT);
                            } else {
                                self.broadcast_hashrate_change().await;
                            }
                        }
                    }
                }

                // Startup-gate fallback: open after the timeout even if some
                // thread never reported.
                _ = async {
                    match gate_deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                }, if self.startup_gate.is_holding() => {
                    self.startup_gate.record_timeout();
                    debug!("Startup gate opened by fallback timeout; not every thread reported");
                    self.broadcast_hashrate_change().await;
                    gate_deadline = None;
                }

                // Periodic status logging
                _ = status_interval.tick() => {
                    if first_status_tick {
                        first_status_tick = false;
                    } else {
                        let hashrate = self.measured_hashrate();
                        self.stats.log_summary(hashrate);
                    }
                }

                // API commands
                Some(cmd) = cmd_rx.recv() => {
                    self.handle_api_command(cmd, &miner_telemetry_tx, &mut share_channels).await;
                }

                // Staggered start: release the next chain when it is due.
                _ = release_interval.tick(), if self.stagger.is_some() || self.stop_stagger.is_some() => {
                    let now = tokio::time::Instant::now();
                    self.advance_release(now, &mut share_channels).await;
                    self.advance_stop(now, &mut share_channels).await;
                }

                // Periodic state publishing
                _ = telemetry_interval.tick() => {
                    let _ = miner_telemetry_tx.send(self.compute_miner_telemetry());
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
        let hashrate = self.measured_hashrate();
        self.stats.log_summary(hashrate);

        debug!("Scheduler shutdown complete");
    }
}

/// Sends each source its hashrate allocation.
///
/// Takes pre-collected (sender, hashrate) pairs to avoid capturing Scheduler
/// across await points (it contains Box<dyn HashThread> which isn't Sync).
async fn send_hashrate_updates(updates: Vec<(mpsc::Sender<SourceCommand>, HashRate)>) {
    for (sender, hashrate) in updates {
        let _ = sender.send(SourceCommand::UpdateHashRate(hashrate)).await;
    }
}

/// Threshold for warning about high share difficulty.
///
/// If expected time to find a share exceeds this, warn the operator that the
/// pool difficulty may be misconfigured for this hashrate.
const HIGH_DIFFICULTY_THRESHOLD: Duration = Duration::from_secs(300); // 5 minutes

/// How long difficulty must remain too high before warning.
///
/// Absorbs transients like pool connections starting with a default
/// difficulty before `suggest_difficulty` takes effect, and brief
/// hashrate changes from board hotplug.
const HIGH_DIFFICULTY_DEBOUNCE: Duration = Duration::from_secs(30);

/// Check whether job difficulty is unreasonably high for our hashrate.
fn is_difficulty_too_high(job: &JobTemplate, hashrate: HashRate) -> bool {
    if hashrate.is_zero() {
        return false;
    }

    let time_to_share = expected_time_to_share_from_target(job.share_target, hashrate);
    time_to_share > HIGH_DIFFICULTY_THRESHOLD
}

/// Run the scheduler task, receiving hash threads and job sources.
pub async fn task(
    running: CancellationToken,
    thread_rx: mpsc::Receiver<ThreadRegistration>,
    source_reg_rx: mpsc::Receiver<SourceRegistration>,
    miner_telemetry_tx: watch::Sender<MinerTelemetry>,
    cmd_rx: mpsc::Receiver<SchedulerCommand>,
    start_paused: bool,
) {
    let mut scheduler = Scheduler::new(start_paused);
    scheduler.stagger = Stagger::from_env();
    scheduler.stop_stagger = StopStagger::from_env();
    if let Some(stop) = &scheduler.stop_stagger {
        info!(
            spacing_s = stop.spacing.as_secs(),
            "Staggered stop configured: a pause idles chains one at a time"
        );
    }
    if let Some(stagger) = &scheduler.stagger {
        info!(
            spacing_s = stagger.spacing.as_secs(),
            "Staggered start configured: chains start one at a time"
        );
    }
    scheduler
        .run(
            running,
            thread_rx,
            source_reg_rx,
            miner_telemetry_tx,
            cmd_rx,
        )
        .await;
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

/// One-shot gate that holds the first source broadcast until startup
/// enumeration is provably complete.
///
/// Held until the enumeration-complete signal has been seen (so every starting
/// thread is registered) and every registered thread has reported its expected
/// hashrate, or until a fallback timeout forces it. Once open it stays open.
#[derive(Debug)]
struct StartupGate {
    registered: usize,
    reported: usize,
    enumeration_complete: bool,
    open: bool,
}

impl StartupGate {
    fn new() -> Self {
        Self {
            registered: 0,
            reported: 0,
            enumeration_complete: false,
            open: false,
        }
    }

    fn is_holding(&self) -> bool {
        !self.open
    }

    /// Count a newly-registered thread.
    fn record_registered(&mut self) {
        if !self.open {
            self.registered += 1;
        }
    }

    /// Count a thread's first report.
    fn record_reported(&mut self) {
        if !self.open {
            self.reported += 1;
            self.try_open();
        }
    }

    /// Record the enumeration-complete signal.
    fn record_enumeration_complete(&mut self) {
        if !self.open {
            self.enumeration_complete = true;
            self.try_open();
        }
    }

    /// Force the gate open via fallback timeout.
    fn record_timeout(&mut self) {
        self.open = true;
    }

    fn try_open(&mut self) {
        if self.enumeration_complete && self.reported >= self.registered {
            self.open = true;
        }
    }
}

/// Mining statistics tracker.
#[derive(Debug)]
struct MiningStats {
    start_time: std::time::Instant,
    shares_submitted: u64,
}

impl Default for MiningStats {
    fn default() -> Self {
        Self {
            start_time: std::time::Instant::now(),
            shares_submitted: 0,
        }
    }
}

impl MiningStats {
    fn log_summary(&self, hashrate: HashRate) {
        let elapsed = self.start_time.elapsed();

        let hashrate_str = if hashrate.is_zero() {
            "--".to_string()
        } else {
            hashrate.to_human_readable()
        };

        info!(
            uptime = %format_duration(elapsed.as_secs()),
            hashrate = %hashrate_str,
            shares = self.shares_submitted,
            "Mining status."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asic::hash_thread::{HashThreadCapabilities, HashThreadStatus};
    use crate::job_source::{MerkleRootKind, VersionTemplate};
    use crate::types::Difficulty;
    use bitcoin::hashes::Hash;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::oneshot;

    /// A hash thread that records what the scheduler asked it to do.
    ///
    /// The scheduler had no behavioural tests because exercising it needs a
    /// thread; this is the smallest one that works, and the counters are what
    /// the pause tests assert on.
    #[derive(Default)]
    struct ThreadCalls {
        updates: AtomicUsize,
        replaces: AtomicUsize,
        idles: AtomicUsize,
        /// The extranonce2 range of every task handed over, in order.
        ranges: std::sync::Mutex<Vec<Option<crate::job_source::Extranonce2Range>>>,
    }

    struct RecordingThread {
        name: String,
        capabilities: HashThreadCapabilities,
        calls: Arc<ThreadCalls>,
    }

    #[async_trait::async_trait]
    impl HashThread for RecordingThread {
        fn name(&self) -> &str {
            &self.name
        }
        fn capabilities(&self) -> &HashThreadCapabilities {
            &self.capabilities
        }
        async fn configure(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn update_task(&mut self, task: HashTask) -> anyhow::Result<Option<HashTask>> {
            self.calls.updates.fetch_add(1, Ordering::SeqCst);
            self.calls
                .ranges
                .lock()
                .unwrap()
                .push(task.en2_range.clone());
            Ok(None)
        }
        async fn replace_task(&mut self, task: HashTask) -> anyhow::Result<Option<HashTask>> {
            self.calls.replaces.fetch_add(1, Ordering::SeqCst);
            self.calls
                .ranges
                .lock()
                .unwrap()
                .push(task.en2_range.clone());
            Ok(None)
        }
        async fn go_idle(&mut self) -> anyhow::Result<Option<HashTask>> {
            self.calls.idles.fetch_add(1, Ordering::SeqCst);
            Ok(None)
        }
        fn take_event_receiver(&mut self) -> Option<mpsc::Receiver<HashThreadEvent>> {
            None
        }
        fn status(&self) -> HashThreadStatus {
            HashThreadStatus::default()
        }
    }

    fn test_template() -> Arc<JobTemplate> {
        Arc::new(JobTemplate {
            id: "job-1".into(),
            prev_blockhash: bitcoin::BlockHash::all_zeros(),
            version: VersionTemplate::new(
                bitcoin::block::Version::from_consensus(0x2000_0000),
                crate::job_source::GeneralPurposeBits::full(),
            )
            .unwrap(),
            bits: bitcoin::pow::CompactTarget::from_consensus(0x1d00_ffff),
            share_target: Difficulty::from(1024).to_target(),
            time: 1_700_000_000,
            // Computed, not Fixed: the cached-job path skips a fixed merkle
            // root, so a Fixed fixture would pass the pause tests for the
            // wrong reason.
            merkle_root: MerkleRootKind::Computed(crate::job_source::MerkleRootTemplate {
                coinbase1: vec![0u8; 4],
                extranonce1: vec![0u8; 4],
                extranonce2_range: crate::job_source::Extranonce2Range::new(4).unwrap(),
                coinbase2: vec![0u8; 4],
                merkle_branches: Vec::new(),
            }),
        })
    }

    /// A scheduler holding one source with a cached job and one thread that
    /// has reported an expected hashrate, so it is eligible for work.
    fn scheduler_with_source_and_thread(
        paused: bool,
    ) -> (Scheduler, SourceId, ThreadId, Arc<ThreadCalls>) {
        let mut scheduler = Scheduler::new(paused);
        let (command_tx, _command_rx) = mpsc::channel(4);
        let source_id = scheduler.sources.insert(SourceEntry {
            name: "test".into(),
            url: None,
            command_tx,
            last_job: Some(test_template()),
            difficulty_alarm: DebouncedAlarm::new(Duration::from_secs(60)),
        });
        let calls = Arc::new(ThreadCalls::default());
        let thread_id = scheduler.threads.insert(ThreadEntry {
            thread: Box::new(RecordingThread {
                name: "test-thread".into(),
                capabilities: HashThreadCapabilities::default(),
                calls: calls.clone(),
            }),
            hashrate: HashrateEstimator::new(Duration::from_secs(60)),
            expected: Some(HashRate::from_terahashes(1.0)),
        });
        (scheduler, source_id, thread_id, calls)
    }

    /// A scheduler with one source (and its cached job) and `n` eligible
    /// threads, staggered or not.
    fn scheduler_with_threads(
        n: usize,
        paused: bool,
        stagger: Option<Duration>,
    ) -> (Scheduler, SourceId, Vec<(ThreadId, Arc<ThreadCalls>)>) {
        let (mut scheduler, source_id, first, calls) = scheduler_with_source_and_thread(paused);
        let mut threads = vec![(first, calls)];
        for i in 1..n {
            let calls = Arc::new(ThreadCalls::default());
            let id = scheduler.threads.insert(ThreadEntry {
                thread: Box::new(RecordingThread {
                    name: format!("test-thread-{i}"),
                    capabilities: HashThreadCapabilities::default(),
                    calls: calls.clone(),
                }),
                hashrate: HashrateEstimator::new(Duration::from_secs(60)),
                expected: Some(HashRate::from_terahashes(1.0)),
            });
            threads.push((id, calls));
        }
        scheduler.stagger = stagger.map(|spacing| Stagger {
            spacing,
            share_timeout: STAGGER_SHARE_TIMEOUT,
            episode: None,
        });
        (scheduler, source_id, threads)
    }

    fn tasks_per_thread(threads: &[(ThreadId, Arc<ThreadCalls>)]) -> Vec<usize> {
        threads
            .iter()
            .map(|(_, c)| c.updates.load(Ordering::SeqCst) + c.replaces.load(Ordering::SeqCst))
            .collect()
    }

    async fn job_arrives(
        scheduler: &mut Scheduler,
        source_id: SourceId,
        share_channels: &mut ShareStream,
    ) {
        let job = (*test_template()).clone();
        scheduler
            .assign_job_to_threads(AssignMode::Update, source_id, job, share_channels)
            .await;
    }

    /// Three chains on one supply, started together, are one load step the
    /// size of all three. Staggered, the first job reaches one of them.
    #[tokio::test(start_paused = true)]
    async fn a_staggered_start_gives_the_first_job_to_one_chain() {
        let mut share_channels = ShareStream::new();
        // Today's behaviour, for contrast: every chain at once.
        let (mut together, source, threads) = scheduler_with_threads(3, false, None);
        job_arrives(&mut together, source, &mut share_channels).await;
        assert_eq!(tasks_per_thread(&threads), vec![1, 1, 1]);

        let (mut staggered, source, threads) =
            scheduler_with_threads(3, false, Some(Duration::from_secs(5)));
        job_arrives(&mut staggered, source, &mut share_channels).await;
        assert_eq!(tasks_per_thread(&threads), vec![1, 0, 0]);
    }

    #[tokio::test(start_paused = true)]
    async fn the_next_chain_follows_a_share_and_the_spacing_and_not_before() {
        let mut share_channels = ShareStream::new();
        let (mut scheduler, source, threads) =
            scheduler_with_threads(3, false, Some(Duration::from_secs(5)));
        job_arrives(&mut scheduler, source, &mut share_channels).await;

        // The spacing alone is not enough: no share yet.
        tokio::time::advance(Duration::from_secs(6)).await;
        scheduler
            .advance_release(tokio::time::Instant::now(), &mut share_channels)
            .await;
        assert_eq!(tasks_per_thread(&threads), vec![1, 0, 0]);

        // A share alone is not enough either, from a fresh release.
        let (mut scheduler, source, threads) =
            scheduler_with_threads(3, false, Some(Duration::from_secs(5)));
        job_arrives(&mut scheduler, source, &mut share_channels).await;
        scheduler.note_release_share(threads[0].0);
        tokio::time::advance(Duration::from_secs(4)).await;
        scheduler
            .advance_release(tokio::time::Instant::now(), &mut share_channels)
            .await;
        assert_eq!(tasks_per_thread(&threads), vec![1, 0, 0]);

        // Both: the second chain gets the current job, in ITS slice.
        tokio::time::advance(Duration::from_secs(1)).await;
        scheduler
            .advance_release(tokio::time::Instant::now(), &mut share_channels)
            .await;
        assert_eq!(tasks_per_thread(&threads), vec![1, 1, 0]);
        let full = crate::job_source::Extranonce2Range::new(4).unwrap();
        let slices = full.split(3).unwrap();
        assert_eq!(
            threads[1].1.ranges.lock().unwrap()[0],
            Some(slices[1].clone())
        );

        // A share from the FIRST chain does not release the third.
        scheduler.note_release_share(threads[0].0);
        tokio::time::advance(Duration::from_secs(10)).await;
        scheduler
            .advance_release(tokio::time::Instant::now(), &mut share_channels)
            .await;
        assert_eq!(tasks_per_thread(&threads), vec![1, 1, 0]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_job_arriving_mid_start_reaches_only_the_released_chains() {
        let mut share_channels = ShareStream::new();
        let (mut scheduler, source, threads) =
            scheduler_with_threads(3, false, Some(Duration::from_secs(5)));
        job_arrives(&mut scheduler, source, &mut share_channels).await;
        scheduler.note_release_share(threads[0].0);
        tokio::time::advance(Duration::from_secs(5)).await;
        scheduler
            .advance_release(tokio::time::Instant::now(), &mut share_channels)
            .await;
        assert_eq!(tasks_per_thread(&threads), vec![1, 1, 0]);

        job_arrives(&mut scheduler, source, &mut share_channels).await;
        assert_eq!(tasks_per_thread(&threads), vec![2, 2, 0]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_chain_that_returns_no_share_holds_the_rest_idle() {
        let mut share_channels = ShareStream::new();
        let (mut scheduler, source, threads) =
            scheduler_with_threads(3, false, Some(Duration::from_secs(5)));
        job_arrives(&mut scheduler, source, &mut share_channels).await;
        tokio::time::advance(STAGGER_SHARE_TIMEOUT).await;
        scheduler
            .advance_release(tokio::time::Instant::now(), &mut share_channels)
            .await;
        assert!(
            scheduler
                .stagger
                .as_ref()
                .unwrap()
                .episode
                .as_ref()
                .unwrap()
                .held
        );
        // A late share does not undo the hold.
        scheduler.note_release_share(threads[0].0);
        tokio::time::advance(Duration::from_secs(60)).await;
        scheduler
            .advance_release(tokio::time::Instant::now(), &mut share_channels)
            .await;
        assert_eq!(tasks_per_thread(&threads), vec![1, 0, 0]);
    }

    #[tokio::test(start_paused = true)]
    async fn one_chain_is_not_staggered() {
        let mut share_channels = ShareStream::new();
        let (mut scheduler, source, threads) =
            scheduler_with_threads(1, false, Some(Duration::from_secs(5)));
        job_arrives(&mut scheduler, source, &mut share_channels).await;
        assert_eq!(tasks_per_thread(&threads), vec![1]);
        tokio::time::advance(Duration::from_secs(60)).await;
        scheduler
            .advance_release(tokio::time::Instant::now(), &mut share_channels)
            .await;
        assert!(
            !scheduler
                .stagger
                .as_ref()
                .unwrap()
                .episode
                .as_ref()
                .unwrap()
                .held
        );
    }

    fn idles(threads: &[(ThreadId, Arc<ThreadCalls>)]) -> Vec<usize> {
        threads
            .iter()
            .map(|(_, c)| c.idles.load(Ordering::SeqCst))
            .collect()
    }

    async fn pause(scheduler: &mut Scheduler, share_channels: &mut ShareStream) {
        let (telemetry_tx, _telemetry_rx) = watch::channel(MinerTelemetry::default());
        let (reply, reply_rx) = oneshot::channel();
        scheduler
            .handle_api_command(
                SchedulerCommand::PauseMining { reply },
                &telemetry_tx,
                share_channels,
            )
            .await;
        assert!(reply_rx.await.unwrap().is_ok());
    }

    /// A pause takes every chain's load off at once: one step the size of all
    /// of them on a shared supply. Staggered, each comes off as its own.
    #[tokio::test(start_paused = true)]
    async fn a_staggered_pause_idles_one_chain_per_spacing() {
        let mut share_channels = ShareStream::new();
        // Today, for contrast: all at once.
        let (mut together, source, threads) = scheduler_with_threads(3, false, None);
        job_arrives(&mut together, source, &mut share_channels).await;
        pause(&mut together, &mut share_channels).await;
        assert_eq!(idles(&threads), vec![1, 1, 1]);

        let (mut scheduler, source, threads) = scheduler_with_threads(3, false, None);
        scheduler.stop_stagger = Some(StopStagger {
            spacing: Duration::from_secs(5),
            queue: Default::default(),
            last_at: tokio::time::Instant::now(),
        });
        job_arrives(&mut scheduler, source, &mut share_channels).await;
        assert_eq!(scheduler.tasks.len(), 3);
        pause(&mut scheduler, &mut share_channels).await;
        assert_eq!(
            idles(&threads),
            vec![1, 0, 0],
            "the pause idles the first chain only"
        );
        assert_eq!(scheduler.tasks.len(), 2);
        tokio::time::advance(Duration::from_secs(4)).await;
        scheduler
            .advance_stop(tokio::time::Instant::now(), &mut share_channels)
            .await;
        assert_eq!(idles(&threads), vec![1, 0, 0]);
        tokio::time::advance(Duration::from_secs(1)).await;
        scheduler
            .advance_stop(tokio::time::Instant::now(), &mut share_channels)
            .await;
        assert_eq!(idles(&threads), vec![1, 1, 0]);
        tokio::time::advance(Duration::from_secs(5)).await;
        scheduler
            .advance_stop(tokio::time::Instant::now(), &mut share_channels)
            .await;
        assert_eq!(idles(&threads), vec![1, 1, 1]);
        assert!(scheduler.tasks.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_resume_mid_stop_finishes_the_stop_first() {
        let mut share_channels = ShareStream::new();
        let (mut scheduler, source, threads) = scheduler_with_threads(3, false, None);
        scheduler.stop_stagger = Some(StopStagger {
            spacing: Duration::from_secs(5),
            queue: Default::default(),
            last_at: tokio::time::Instant::now(),
        });
        job_arrives(&mut scheduler, source, &mut share_channels).await;
        pause(&mut scheduler, &mut share_channels).await;
        let (telemetry_tx, _telemetry_rx) = watch::channel(MinerTelemetry::default());
        let (reply, reply_rx) = oneshot::channel();
        scheduler
            .handle_api_command(
                SchedulerCommand::ResumeMining { reply },
                &telemetry_tx,
                &mut share_channels,
            )
            .await;
        assert!(reply_rx.await.unwrap().is_ok());
        assert_eq!(
            idles(&threads),
            vec![1, 1, 1],
            "every chain stopped before the resume"
        );
        assert_eq!(
            scheduler.tasks.len(),
            3,
            "and each holds one task again, not two"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_chain_going_away_holds_the_start_instead_of_aborting() {
        let mut share_channels = ShareStream::new();
        let (mut scheduler, source, threads) =
            scheduler_with_threads(3, false, Some(Duration::from_secs(5)));
        job_arrives(&mut scheduler, source, &mut share_channels).await;
        // Chain 0's thread disconnects before any share.
        scheduler.threads.remove(threads[0].0);
        let live: Vec<ThreadId> = vec![threads[1].0, threads[2].0];
        scheduler
            .stagger
            .as_mut()
            .unwrap()
            .episode
            .as_mut()
            .unwrap()
            .reconcile(&live);
        tokio::time::advance(STAGGER_SHARE_TIMEOUT).await;
        scheduler
            .advance_release(tokio::time::Instant::now(), &mut share_channels)
            .await;
        assert!(
            scheduler
                .stagger
                .as_ref()
                .unwrap()
                .episode
                .as_ref()
                .unwrap()
                .held
        );
        // And a new job does not release anything past the hold.
        job_arrives(&mut scheduler, source, &mut share_channels).await;
        assert_eq!(tasks_per_thread(&threads[1..]), vec![0, 0]);
    }

    #[tokio::test(start_paused = true)]
    async fn no_chain_is_released_while_there_is_no_job_to_give_it() {
        let mut share_channels = ShareStream::new();
        let (mut scheduler, source, threads) =
            scheduler_with_threads(2, false, Some(Duration::from_secs(5)));
        job_arrives(&mut scheduler, source, &mut share_channels).await;
        scheduler.note_release_share(threads[0].0);
        scheduler.sources.get_mut(source).unwrap().last_job = None; // the pool went away
        tokio::time::advance(Duration::from_secs(20)).await;
        scheduler
            .advance_release(tokio::time::Instant::now(), &mut share_channels)
            .await;
        assert_eq!(tasks_per_thread(&threads), vec![1, 0]);
        assert!(
            !scheduler
                .stagger
                .as_ref()
                .unwrap()
                .episode
                .as_ref()
                .unwrap()
                .held
        );
        // The pool is back: the release goes ahead.
        scheduler.sources.get_mut(source).unwrap().last_job = Some(test_template());
        scheduler
            .advance_release(tokio::time::Instant::now(), &mut share_channels)
            .await;
        assert_eq!(tasks_per_thread(&threads), vec![1, 1]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_chain_joining_mid_start_never_gets_a_slice_inside_another() {
        let mut share_channels = ShareStream::new();
        let (mut scheduler, source, threads) =
            scheduler_with_threads(2, false, Some(Duration::from_secs(5)));
        job_arrives(&mut scheduler, source, &mut share_channels).await; // chain 0: split(2)[0]
        // A third chain reports late and joins the queue.
        let calls = Arc::new(ThreadCalls::default());
        let late = scheduler.threads.insert(ThreadEntry {
            thread: Box::new(RecordingThread {
                name: "late".into(),
                capabilities: HashThreadCapabilities::default(),
                calls: calls.clone(),
            }),
            hashrate: HashrateEstimator::new(Duration::from_secs(60)),
            expected: Some(HashRate::from_terahashes(1.0)),
        });
        scheduler
            .assign_cached_jobs_to_thread(late, "late", &mut share_channels)
            .await;
        for _ in 0..2 {
            let last = {
                let ep = scheduler
                    .stagger
                    .as_ref()
                    .unwrap()
                    .episode
                    .as_ref()
                    .unwrap();
                ep.order[ep.released - 1]
            };
            scheduler.note_release_share(last);
            tokio::time::advance(Duration::from_secs(5)).await;
            scheduler
                .advance_release(tokio::time::Instant::now(), &mut share_channels)
                .await;
        }
        // Every chain's CURRENT slice, disjoint from every other's.
        let mut current = Vec::new();
        for c in threads
            .iter()
            .map(|(_, c)| c)
            .chain(std::iter::once(&calls))
        {
            current.push(
                c.ranges
                    .lock()
                    .unwrap()
                    .last()
                    .cloned()
                    .flatten()
                    .expect("a range"),
            );
        }
        for (i, a) in current.iter().enumerate() {
            for b in &current[i + 1..] {
                assert!(
                    a.max < b.min || b.max < a.min,
                    "overlapping slices: {a:?} {b:?}"
                );
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_pause_mid_stop_stops_every_remaining_chain_at_once() {
        let mut share_channels = ShareStream::new();
        let (mut scheduler, source, threads) = scheduler_with_threads(3, false, None);
        scheduler.stop_stagger = Some(StopStagger {
            spacing: Duration::from_secs(5),
            queue: Default::default(),
            last_at: tokio::time::Instant::now(),
        });
        job_arrives(&mut scheduler, source, &mut share_channels).await;
        pause(&mut scheduler, &mut share_channels).await;
        assert_eq!(idles(&threads), vec![1, 0, 0]);
        tokio::time::advance(Duration::from_secs(4)).await;
        pause(&mut scheduler, &mut share_channels).await;
        assert_eq!(
            idles(&threads),
            vec![1, 1, 1],
            "the second pause must not restart the clock"
        );
    }

    /// D7. A resume handed every thread the cached job through the new-thread
    /// path, which gives each the WHOLE extranonce2 range: after a pause every
    /// chain hashed the same space until the next job re-split it.
    #[tokio::test]
    async fn a_resume_splits_the_range_instead_of_giving_each_chain_all_of_it() {
        let mut share_channels = ShareStream::new();
        let (mut scheduler, _source, threads) = scheduler_with_threads(3, true, None);
        let (telemetry_tx, _telemetry_rx) = watch::channel(MinerTelemetry::default());
        let (reply, reply_rx) = oneshot::channel();
        scheduler
            .handle_api_command(
                SchedulerCommand::ResumeMining { reply },
                &telemetry_tx,
                &mut share_channels,
            )
            .await;
        assert!(reply_rx.await.unwrap().is_ok());
        let ranges: Vec<_> = threads
            .iter()
            .map(|(_, c)| c.ranges.lock().unwrap()[0].clone().expect("a range"))
            .collect();
        for (i, a) in ranges.iter().enumerate() {
            for b in &ranges[i + 1..] {
                assert!(
                    a.max < b.min || b.max < a.min,
                    "overlapping ranges after resume: {a:?} {b:?}"
                );
            }
        }
    }

    /// Observer mode's load-bearing guarantee: a board that attaches while
    /// paused is left idle. On a machine whose hashboards share one PSU rail,
    /// an unasked-for job is current nobody budgeted for.
    #[tokio::test]
    async fn paused_scheduler_leaves_an_attaching_thread_idle() {
        let (mut scheduler, _source_id, thread_id, calls) = scheduler_with_source_and_thread(true);
        let mut share_channels = ShareStream::new();

        scheduler
            .assign_cached_jobs_to_thread(thread_id, "test-thread", &mut share_channels)
            .await;

        assert_eq!(calls.updates.load(Ordering::SeqCst), 0, "no work assigned");
        assert_eq!(calls.replaces.load(Ordering::SeqCst), 0, "no work assigned");
        assert!(scheduler.tasks.is_empty(), "no task recorded");
    }

    /// The same thread gets the cached job once mining resumes, which is what
    /// makes observer mode a starting state rather than a dead end.
    #[tokio::test]
    async fn resuming_assigns_the_cached_job() {
        let (mut scheduler, _source_id, thread_id, calls) = scheduler_with_source_and_thread(true);
        let mut share_channels = ShareStream::new();
        let (telemetry_tx, _telemetry_rx) = watch::channel(MinerTelemetry::default());
        let (reply, reply_rx) = oneshot::channel();

        scheduler
            .handle_api_command(
                SchedulerCommand::ResumeMining { reply },
                &telemetry_tx,
                &mut share_channels,
            )
            .await;

        assert!(reply_rx.await.unwrap().is_ok());
        assert!(!scheduler.paused);
        assert_eq!(
            calls.updates.load(Ordering::SeqCst),
            1,
            "resume should hand the cached job to the waiting thread"
        );
        assert_eq!(scheduler.tasks.len(), 1);
        let _ = thread_id;
    }

    /// Pausing has to stop hardware that is already running, not just refuse
    /// the next job: before this, the API reported `paused` while every thread
    /// kept mining.
    #[tokio::test]
    async fn pausing_idles_running_threads_and_drops_their_tasks() {
        let (mut scheduler, _source_id, thread_id, calls) = scheduler_with_source_and_thread(false);
        let mut share_channels = ShareStream::new();

        scheduler
            .assign_cached_jobs_to_thread(thread_id, "test-thread", &mut share_channels)
            .await;
        assert_eq!(
            calls.updates.load(Ordering::SeqCst),
            1,
            "mining before pause"
        );
        assert_eq!(scheduler.tasks.len(), 1);

        let (telemetry_tx, _telemetry_rx) = watch::channel(MinerTelemetry::default());
        let (reply, reply_rx) = oneshot::channel();
        scheduler
            .handle_api_command(
                SchedulerCommand::PauseMining { reply },
                &telemetry_tx,
                &mut share_channels,
            )
            .await;

        assert!(reply_rx.await.unwrap().is_ok());
        assert!(scheduler.paused);
        assert_eq!(
            calls.idles.load(Ordering::SeqCst),
            1,
            "every thread must be told to go idle"
        );
        assert!(scheduler.tasks.is_empty(), "tasks dropped with the work");
    }

    /// A job arriving while paused reaches no thread.
    #[tokio::test]
    async fn paused_scheduler_drops_an_incoming_job() {
        let (mut scheduler, source_id, _thread_id, calls) = scheduler_with_source_and_thread(true);
        let mut share_channels = ShareStream::new();

        scheduler
            .assign_job_to_threads(
                AssignMode::Replace,
                source_id,
                (*test_template()).clone(),
                &mut share_channels,
            )
            .await;

        assert_eq!(calls.updates.load(Ordering::SeqCst), 0);
        assert_eq!(calls.replaces.load(Ordering::SeqCst), 0);
        assert!(scheduler.tasks.is_empty());
    }

    #[test]
    fn scheduler_target_zero_hashrate_passthrough() {
        let source_target = Difficulty::from(1024).to_target();
        let result = Scheduler::compute_scheduler_target(HashRate::from(0), source_target);
        assert_eq!(result, source_target);
    }

    #[test]
    fn scheduler_target_passthrough_when_in_range() {
        // Pick a source target that falls between the two bounds.
        // At 1 TH/s the bounds span roughly difficulty 23 (easiest)
        // to difficulty 233 (hardest). Difficulty 100 sits in between.
        let hashrate = HashRate::from_terahashes(1.0);
        let source_target = Difficulty::from(100).to_target();
        let result = Scheduler::compute_scheduler_target(hashrate, source_target);
        assert_eq!(result, source_target);
    }

    #[test]
    fn scheduler_target_clamps_hard_source_to_easier() {
        // Pool difficulty much higher than what our hashrate warrants.
        // The scheduler should ease it to the measurement floor so the
        // estimator gets samples.
        let hashrate = HashRate::from_terahashes(1.0);
        let very_hard = Difficulty::from(1_000_000).to_target();
        let result = Scheduler::compute_scheduler_target(hashrate, very_hard);

        let measurement_target = MEASUREMENT_SHARE_RATE.to_target(hashrate);
        assert_eq!(
            result, measurement_target,
            "should clamp to measurement floor"
        );
        assert!(result > very_hard, "clamped target should be easier");
    }

    #[test]
    fn scheduler_target_clamps_easy_source_to_harder() {
        // Pool difficulty absurdly low -- would flood the scheduler.
        // The scheduler should harden it to the flood ceiling.
        let hashrate = HashRate::from_terahashes(1.0);
        let very_easy = Target::MAX;
        let result = Scheduler::compute_scheduler_target(hashrate, very_easy);

        let flood_cap_target = FLOOD_CAP_RATE.to_target(hashrate);
        assert_eq!(result, flood_cap_target, "should clamp to flood ceiling");
        assert!(result < very_easy, "clamped target should be harder");
    }

    /// Regression test: compute_scheduler_target produces a result
    /// without panicking across a wide range of hashrates.
    #[test]
    fn scheduler_target_across_hashrates() {
        let source_target = Difficulty::from(1).to_target();
        for hashrate in [
            HashRate::from(5),
            HashRate::from(5_000),
            HashRate::from_megahashes(5.0),
            HashRate::from_gigahashes(500.0),
            HashRate::from_terahashes(1.0),
            HashRate::from_terahashes(100.0),
        ] {
            let _result = Scheduler::compute_scheduler_target(hashrate, source_target);
        }
    }

    #[test]
    fn startup_gate_opens_on_completion_when_all_reported() {
        let mut gate = StartupGate::new();
        gate.record_registered();
        gate.record_registered();
        gate.record_reported();
        gate.record_reported();
        assert!(gate.is_holding());
        gate.record_enumeration_complete();
        assert!(!gate.is_holding());
    }

    #[test]
    fn startup_gate_opens_on_last_report_after_completion() {
        let mut gate = StartupGate::new();
        gate.record_registered();
        gate.record_registered();
        gate.record_enumeration_complete();
        assert!(gate.is_holding());
        gate.record_reported();
        assert!(gate.is_holding());
        gate.record_reported();
        assert!(!gate.is_holding());
    }

    #[test]
    fn startup_gate_holds_for_completion_even_after_all_reported() {
        let mut gate = StartupGate::new();
        gate.record_registered();
        gate.record_reported();
        assert!(gate.is_holding());
        gate.record_enumeration_complete();
        assert!(!gate.is_holding());
    }

    #[test]
    fn startup_gate_opens_on_completion_with_no_threads() {
        let mut gate = StartupGate::new();
        gate.record_enumeration_complete();
        assert!(!gate.is_holding());
    }

    #[test]
    fn startup_gate_timeout_forces_open() {
        let mut gate = StartupGate::new();
        gate.record_registered();
        gate.record_enumeration_complete();
        assert!(gate.is_holding());
        gate.record_timeout();
        assert!(!gate.is_holding());
    }

    #[test]
    fn startup_gate_stays_open_after_opening() {
        let mut gate = StartupGate::new();
        gate.record_registered();
        gate.record_enumeration_complete();
        gate.record_reported();
        assert!(!gate.is_holding());
        // Later events leave it open.
        gate.record_reported();
        gate.record_enumeration_complete();
        gate.record_timeout();
        assert!(!gate.is_holding());
    }
}
