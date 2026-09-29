//! HashThread abstraction for schedulable mining workers.
//!
//! A HashThread represents a schedulable group of hashing engines that work
//! together to execute mining tasks. The scheduler assigns work to HashThreads
//! without needing to know about the underlying hardware topology (single chip,
//! chip chain, engine groups, etc.).
//!
//! # Share Processing (Three-Layer Filtering)
//!
//! Share filtering happens at three independent levels:
//!
//! 1. **Chip TicketMask (hardware pre-filter):**
//!    Thread configures chip with low difficulty for frequent health signals.
//!    Chip only reports nonces meeting this hardware threshold.
//!
//! 2. **HashTask.share_target (thread-to-scheduler filter):**
//!    Scheduler sets when assigning work. Thread computes hash for every chip
//!    nonce and sends shares meeting task.share_target via the task's channel.
//!    Controls message volume to scheduler.
//!
//! 3. **JobTemplate.share_target (scheduler-to-source filter):**
//!    Scheduler performs final filtering before submission. Only shares
//!    meeting this threshold are forwarded to the source.
//!
//! This provides scheduler with frequent monitoring data (task.share_target)
//! while limiting pool submissions (template.share_target). Message volume
//! is manageable: ~1-2 shares/sec to scheduler, fewer to pool.

use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use async_trait::async_trait;
use bitcoin::BlockHash;
use bitcoin::block::Version;
use bitcoin::pow::Target;
use tokio::sync::mpsc;

use crate::api_client::types::AsicFaultBits;
use crate::job_source::{Extranonce2, Extranonce2Range, JobTemplate};
use crate::types::HashRate;
use bitcoin::pow::Work;

/// HashThread capabilities reported to scheduler for work assignment decisions.
///
/// Placeholder for future non-rate capabilities. Expected hashrate is a
/// runtime signal, declared via `HashThreadEvent::ExpectedHashRate`.
#[derive(Debug, Clone, Default)]
pub struct HashThreadCapabilities {
    // Future capabilities:
    // pub can_roll_version: bool,
    // pub version_roll_bits: u32,
    // pub can_roll_ntime: bool,
    // pub ntime_range: Option<std::ops::Range<u32>>,
    // pub can_iterate_extranonce2: bool,
}

/// Current runtime status of a HashThread.
#[derive(Debug, Clone, Default)]
pub struct HashThreadStatus {
    /// Current hashrate estimate
    pub hashrate: HashRate,

    /// Number of shares found (at chip target level, before pool filtering)
    pub chip_shares_found: u64,

    /// Number of shares submitted to pool (after filtering)
    pub pool_shares_submitted: u64,

    /// Number of hardware errors detected
    pub hardware_errors: u64,

    /// Current chip temperature if available
    pub temperature_c: Option<f32>,

    /// Whether thread is actively working
    pub is_active: bool,
}

/// Temperature reading reported by a hash thread.
#[derive(Debug, Clone, PartialEq)]
pub struct HashThreadTemperatureReading {
    pub name: String,
    pub temperature_c: Option<f32>,
}

/// Voltage/current/power reading reported by a hash thread.
#[derive(Debug, Clone, PartialEq)]
pub struct HashThreadPowerReading {
    pub name: String,
    pub voltage_v: Option<f32>,
    pub current_a: Option<f32>,
    pub power_w: Option<f32>,
}

/// Telemetry update reported by a hash thread.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HashThreadTelemetryUpdate {
    pub temperatures: Vec<HashThreadTemperatureReading>,
    pub powers: Vec<HashThreadPowerReading>,
    /// Which ASIC these readings came from, when they were observed, and
    /// what it reported alongside them. `None` where the readings are not
    /// per-ASIC (a board-level sensor poll).
    ///
    /// One field for the whole update rather than a stamp per reading: every
    /// reading in one update is decoded from one frame, so they share one
    /// observation time and one set of fault bits. Stamping them separately
    /// would be several copies of one arrival, free to drift.
    pub asic: Option<HashThreadAsicObservation>,
}

/// When one ASIC's readings arrived, and what it reported with them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HashThreadAsicObservation {
    /// The id this ASIC answers to on its own chain -- the id the readings
    /// in this update are named with.
    pub asic_id: u8,
    /// Monotonic, never wall time: an age measured across a wall-clock step
    /// is not a measurement.
    pub observed_at: Instant,
    /// The fault bits this frame credibly reported, or `None` where it
    /// reported none that can be believed -- a generation whose frames carry
    /// no fault bits at all, or a frame the decoder judged mis-parsed.
    /// Unavailable, which is not the same as none asserted.
    pub faults: Option<AsicFaultBits>,
}

/// Events emitted by HashThreads back to the scheduler.
///
/// When a thread shuts down (USB unplug, fault, user request, etc.), it closes
/// its event channel instead of sending an event. The scheduler detects channel
/// closure and handles thread removal.
///
/// Note: Shares are sent via the task's dedicated `share_tx` channel, not
/// through this event channel. This separates share routing (task-specific)
/// from general thread events.
#[derive(Debug)]
pub enum HashThreadEvent {
    /// Work approaching exhaustion (warning to scheduler)
    WorkDepletionWarning {
        /// Estimated remaining time in milliseconds
        estimated_remaining_ms: u64,
    },

    /// Work completely exhausted
    WorkExhausted {
        /// Number of EN2 values searched
        en2_searched: u64,
    },

    /// Periodic status update
    StatusUpdate(HashThreadStatus),

    /// Feed-forward hashrate the thread declares it expects to deliver.
    ///
    /// Emitted after `configure()` and whenever the expectation changes.
    ExpectedHashRate(HashRate),

    /// Additional telemetry update
    TelemetryUpdate(HashThreadTelemetryUpdate),
}

/// Error types for HashThread operations.
#[derive(Debug, thiserror::Error)]
pub enum HashThreadError {
    #[error("Thread has been shut down")]
    ThreadOffline,

    #[error("Channel closed: {0}")]
    ChannelClosed(String),

    #[error("Work assignment failed: {0}")]
    WorkAssignmentFailed(String),

    #[error("Preemption failed: {0}")]
    PreemptionFailed(String),

    #[error("Telemetry query failed: {0}")]
    TelemetryQueryFailed(String),

    /// A device asserted a hardware fault, observed while answering a query.
    ///
    /// Distinct from `TelemetryQueryFailed` on purpose. The query did not
    /// fail: it succeeded, and the answer is bad news. Collapsing the two
    /// meant an operator asking a suspicious chain for its temperature was
    /// told the question had failed, at exactly the moment they most needed
    /// the answer.
    #[error("Hardware fault reported: {0}")]
    HardwareFaultReported(String),

    #[error("Diagnostics failed: {0}")]
    DiagnosticsFailed(String),

    #[error("Shutdown timeout")]
    ShutdownTimeout,

    #[error("Chip initialization failed: {0}")]
    InitializationFailed(String),
}

/// HashThread trait - the scheduler's view of a schedulable worker.
///
/// A HashThread represents a group of hashing engines that can be assigned work
/// as a unit. The scheduler interacts with threads through this trait without
/// needing to know about the underlying hardware topology.
///
/// Threads are autonomous actors that:
/// - Operate their hardware
/// - Report events asynchronously
#[async_trait]
pub trait HashThread: Send {
    /// Human-readable name for logging (e.g., "Bitaxe Gamma (e2f56f9b)")
    fn name(&self) -> &str;

    /// Get thread capabilities for scheduling decisions
    fn capabilities(&self) -> &HashThreadCapabilities;

    /// Configure the thread and declare its expected hashrate.
    ///
    /// Records settings, computes the expected hashrate, and emits
    /// `HashThreadEvent::ExpectedHashRate`. The thread becomes eligible for
    /// work; chip bring-up happens later, on the first job assignment.
    async fn configure(&mut self) -> Result<()>;

    /// Update current task (shares from old task still valid)
    ///
    /// Thread continues hashing old task until new task is ready. Late-arriving
    /// shares from the old task can still be submitted (they're valuable).
    /// Returns the old task for potential resumption (None if thread was idle).
    ///
    /// Used when pool sends updated job (difficulty change, new transactions in
    /// mempool) but the work is fundamentally still valid.
    async fn update_task(&mut self, new_task: HashTask) -> Result<Option<HashTask>>;

    /// Replace current task (old task invalidated)
    ///
    /// Old task is immediately invalid - discard it and don't submit shares
    /// from it. Returns the old task for tracking purposes (None if thread was
    /// idle).
    ///
    /// Used when blockchain tip changes (new prevhash) or pool signals
    /// clean_jobs.
    async fn replace_task(&mut self, new_task: HashTask) -> Result<Option<HashTask>>;

    /// Put thread in idle state (low power, no hashing)
    ///
    /// Returns the current task if thread was working (None if already idle).
    /// Thread enters low-power mode, stops hashing.
    async fn go_idle(&mut self) -> Result<Option<HashTask>>;

    /// Take ownership of the event receiver for this thread
    ///
    /// Called once by scheduler after thread creation. The scheduler uses this
    /// to receive events (shares, status updates, etc.) from the thread.
    fn take_event_receiver(&mut self) -> Option<mpsc::Receiver<HashThreadEvent>>;

    /// Get current runtime status
    ///
    /// This is cached and may be slightly stale (updated periodically by
    /// thread's status updates).
    fn status(&self) -> HashThreadStatus;
}

// ---------------------------------------------------------------------------
// Work assignment types
// ---------------------------------------------------------------------------

/// Work assignment from scheduler to hash thread.
///
/// Contains the job template, allocated extranonce2 range, and a channel for
/// returning shares. The scheduler creates a channel for each task; shares
/// sent on that channel implicitly route back to the correct source.
///
/// If a thread has no HashTask (None), it's idle (low power, no hashing).
#[derive(Clone)]
pub struct HashTask {
    /// Job template (block header fields, merkle info, etc.)
    pub template: Arc<JobTemplate>,

    /// Extranonce2 range allocated to this thread.
    ///
    /// None for header-only mining (Stratum v2). Current HashThread
    /// implementations require EN2 iteration, so None will cause errors
    /// until header-only support is added.
    pub en2_range: Option<Extranonce2Range>,

    /// Extranonce2 value.
    ///
    /// When scheduler assigns work: starting EN2.
    /// When stored as snapshot: the EN2 value that was used.
    /// None for header-only mining (Stratum v2).
    pub en2: Option<Extranonce2>,

    /// Share target for thread-to-scheduler submission threshold.
    ///
    /// Thread sends shares meeting this target via `share_tx`. Allows
    /// scheduler to control message volume independently from pool submission
    /// difficulty. Typically set easier than source threshold for monitoring.
    pub share_target: Target,

    /// Current ntime value
    ///
    /// May be rolled forward during mining. To start, uses the job's time field.
    pub ntime: u32,

    /// Channel for submitting shares back to scheduler.
    ///
    /// Scheduler creates this channel and keeps the receiver. Thread sends
    /// valid shares here; channel ownership implicitly routes to correct source.
    pub share_tx: mpsc::Sender<Share>,
}

impl fmt::Debug for HashTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HashTask")
            .field("template", &self.template)
            .field("en2_range", &self.en2_range)
            .field("en2", &self.en2)
            .field("share_target", &self.share_target)
            .field("ntime", &self.ntime)
            .field("share_tx", &"<channel>")
            .finish()
    }
}

/// Valid share found by a HashThread.
///
/// Contains the nonce and computed hash, plus header fields needed for pool
/// submission. Routing to the correct source is implicit: the scheduler knows
/// which source owns the channel that delivered this share.
#[derive(Debug, Clone)]
pub struct Share {
    /// Winning nonce
    pub nonce: u32,

    /// Computed block hash
    pub hash: BlockHash,

    /// Full block version with rolled bits applied.
    ///
    /// Contains the complete version field as it appears in the block header.
    pub version: Version,

    /// Block timestamp
    pub ntime: u32,

    /// Extranonce2 value used (None for header-only mining in Stratum v2)
    pub extranonce2: Option<Extranonce2>,

    /// Expected work this share represents for hashrate calculation.
    ///
    /// Computed from the task's share target via `Target::to_work()`.
    /// Uses the threshold target (not achieved difficulty) for stable
    /// estimates; achieved difficulty has high variance from lucky
    /// shares.
    pub expected_work: Work,
}

impl From<(Share, String)> for crate::job_source::Share {
    fn from((share, job_id): (Share, String)) -> Self {
        Self {
            job_id,
            nonce: share.nonce,
            time: share.ntime,
            version: share.version,
            extranonce2: share.extranonce2,
        }
    }
}

/// Bounds telemetry publication by TIME rather than by wire rate.
///
/// The failure this exists to prevent, measured on hardware rather than
/// imagined. On 2026-09-18 a write-enabled handover left the BZM2 DTS/VS
/// sensor stream running at **13,850 frames per second** (397,516 frames in
/// 28.7 s, measured in our captures). The
/// frame handler did two things per frame:
///
/// - took a **blocking** `RwLock` write on the shared thread status, which
///   every API handler must read, and
/// - `send().await` into a **64-slot** channel.
///
/// On the two-core control board the API then answered **nothing at all** for
/// the entire window -- not one HTTP status line in 29 seconds, while the
/// daemon sat healthy with its listener bound. The same build under a
/// dry-run policy saw only 454 frames per second and served most requests, so
/// this had read as an intermittent API fault for days.
///
/// **Nothing is dropped that a consumer could have used.** A temperature is a
/// *level*, not an event: the newest reading for a device IS its current one,
/// so keeping the latest per device and publishing on a timer loses no
/// information a reader could have observed -- nobody polls at 13.8 kHz. What
/// is bounded is how often the shared lock is taken and the channel is fed.
///
/// **The protection path does not come through here.** The thermal interlock
/// observes every frame directly, before coalescing, so a spike cannot hide in
/// a window. For the same reason the level published to the shared status is
/// the window's **hottest** reading rather than whichever device reported
/// last: a hotspot is the number an operator needs, and last-writer-wins made
/// it arbitrary.
#[derive(Debug)]
pub struct TelemetryCoalescer {
    latest: std::collections::BTreeMap<u8, HashThreadTelemetryUpdate>,
    board_level: Vec<HashThreadTelemetryUpdate>,
    hottest_c: Option<f32>,
    window_start: Option<std::time::Instant>,
    interval: std::time::Duration,
    observed: u64,
    published: u64,
}

impl TelemetryCoalescer {
    pub fn new(interval: std::time::Duration) -> Self {
        Self {
            latest: std::collections::BTreeMap::new(),
            board_level: Vec::new(),
            hottest_c: None,
            window_start: None,
            interval,
            observed: 0,
            published: 0,
        }
    }

    /// Readings taken in, whether or not they were ever published.
    pub fn observed(&self) -> u64 {
        self.observed
    }

    /// Readings handed out. The ratio against [`Self::observed`] is what the
    /// shared lock and the channel were spared.
    pub fn published(&self) -> u64 {
        self.published
    }

    /// Take one update. Cheap: no lock, no channel, no allocation beyond the
    /// per-device slot it replaces.
    pub fn observe(&mut self, update: HashThreadTelemetryUpdate, now: std::time::Instant) {
        self.observed = self.observed.saturating_add(1);
        self.window_start.get_or_insert(now);

        for reading in &update.temperatures {
            if let Some(t) = reading.temperature_c {
                self.hottest_c = Some(match self.hottest_c {
                    Some(h) if h >= t => h,
                    _ => t,
                });
            }
        }

        match update.asic {
            Some(observation) => {
                self.latest.insert(observation.asic_id, update);
            }
            // Board-level polls are not per-device, so there is no slot to
            // replace and nothing to coalesce them against. They are rare by
            // construction; keeping them all is correct.
            None => self.board_level.push(update),
        }
    }

    /// Whether the window has run long enough to publish.
    pub fn due(&self, now: std::time::Instant) -> bool {
        match self.window_start {
            Some(start) => now.saturating_duration_since(start) >= self.interval,
            None => false,
        }
    }

    /// Hand back everything held, and the level to publish alongside it.
    ///
    /// Returns an empty vector and `None` when nothing has been observed, so
    /// a caller can drain unconditionally.
    pub fn drain(
        &mut self,
        now: std::time::Instant,
    ) -> (Vec<HashThreadTelemetryUpdate>, Option<f32>) {
        let mut out: Vec<HashThreadTelemetryUpdate> = self.board_level.drain(..).collect();
        out.extend(std::mem::take(&mut self.latest).into_values());
        let hottest = self.hottest_c.take();
        self.window_start = if out.is_empty() { None } else { Some(now) };
        self.published = self.published.saturating_add(out.len() as u64);
        (out, hottest)
    }
}

#[cfg(test)]
mod telemetry_coalescer_tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn reading(asic: u8, temp: f32) -> HashThreadTelemetryUpdate {
        HashThreadTelemetryUpdate {
            temperatures: vec![HashThreadTemperatureReading {
                name: format!("asic-{asic}-dts"),
                temperature_c: Some(temp),
            }],
            powers: Vec::new(),
            asic: Some(HashThreadAsicObservation {
                asic_id: asic,
                observed_at: Instant::now(),
                faults: None,
            }),
        }
    }

    #[test]
    fn the_stream_that_silenced_the_api_costs_a_bounded_number_of_publications() {
        // The measured shape: 397,516 frames from 100 ASICs over 28.7 s, the
        // rate at which the control board's API stopped answering entirely.
        const FRAMES: u64 = 397_516;
        const ASICS: u8 = 100;
        let window = Duration::from_millis(28_700);

        let mut c = TelemetryCoalescer::new(Duration::from_millis(500));
        let start = Instant::now();
        let step = window / FRAMES as u32;

        let mut publications = 0u64;
        let mut flushes = 0u64;
        for i in 0..FRAMES {
            let now = start + step * i as u32;
            c.observe(reading((i % ASICS as u64) as u8, 40.0), now);
            if c.due(now) {
                let (batch, _hottest) = c.drain(now);
                flushes += 1;
                publications += batch.len() as u64;
            }
        }

        // Unbounded this was 397,516 lock writes and 397,516 sends into a
        // 64-slot channel. Bounded, it is one flush per interval.
        assert!(
            flushes <= 60,
            "{flushes} flushes over 28.7 s is not bounded"
        );
        assert!(
            FRAMES / publications >= 50,
            "expected a 50x reduction at least, got {}x",
            FRAMES / publications
        );
        assert_eq!(c.observed(), FRAMES, "every reading must still be counted");

        // And no device is starved: a flush carries every ASIC that reported
        // in the window, so a slow device is not crowded out by a fast one.
        let (final_batch, _) = c.drain(start + window);
        let ids: std::collections::BTreeSet<u8> = final_batch
            .iter()
            .filter_map(|u| u.asic.map(|o| o.asic_id))
            .collect();
        assert!(
            ids.len() >= 50,
            "a flush carried only {} devices; coalescing must not drop devices",
            ids.len()
        );
    }

    #[test]
    fn the_hottest_reading_in_a_window_is_the_one_published() {
        // Regression against last-writer-wins. A spike that arrives mid-window
        // must not be erased by a cooler reading behind it -- that is the
        // number an operator looks at.
        let mut c = TelemetryCoalescer::new(Duration::from_millis(500));
        let t0 = Instant::now();
        c.observe(reading(0, 45.0), t0);
        c.observe(reading(1, 88.5), t0 + Duration::from_millis(10));
        c.observe(reading(2, 46.0), t0 + Duration::from_millis(20));

        let (batch, hottest) = c.drain(t0 + Duration::from_millis(500));
        assert_eq!(
            hottest,
            Some(88.5),
            "the spike was published, not the last reading"
        );
        assert_eq!(batch.len(), 3, "every device that reported is carried");
    }

    #[test]
    fn a_device_that_reports_repeatedly_keeps_only_its_newest_reading() {
        // A temperature is a level. Ten readings from one device inside one
        // window are not ten facts; the last is the current one.
        let mut c = TelemetryCoalescer::new(Duration::from_millis(500));
        let t0 = Instant::now();
        for i in 0..10 {
            c.observe(reading(7, 50.0 + i as f32), t0 + Duration::from_millis(i));
        }
        let (batch, _) = c.drain(t0 + Duration::from_millis(500));
        assert_eq!(batch.len(), 1, "one device, one slot");
        assert_eq!(batch[0].temperatures[0].temperature_c, Some(59.0));
        assert_eq!(c.observed(), 10, "but all ten were counted");
    }

    #[test]
    fn draining_an_empty_coalescer_is_harmless() {
        let mut c = TelemetryCoalescer::new(Duration::from_millis(500));
        let (batch, hottest) = c.drain(Instant::now());
        assert!(batch.is_empty());
        assert_eq!(hottest, None);
    }
}
