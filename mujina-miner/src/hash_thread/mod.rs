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
//!    nonce and emits ShareFound only for shares meeting task.share_target.
//!    Controls message volume to scheduler.
//!
//! 3. **JobTemplate.share_target (scheduler-to-source filter):**
//!    Scheduler performs final filtering before pool submission. Only shares
//!    meeting this threshold are forwarded to the source.
//!
//! This provides scheduler with frequent monitoring data (task.share_target)
//! while limiting pool submissions (template.share_target). Message volume
//! is manageable: ~1-2 shares/sec to scheduler, fewer to pool.

pub mod bm13xx;
pub mod task;

use async_trait::async_trait;
use tokio::sync::mpsc;

use self::task::{HashTask, Share};
use crate::types::HashRate;

/// HashThread capabilities reported to scheduler for work assignment decisions.
#[derive(Debug, Clone)]
pub struct HashThreadCapabilities {
    /// Estimated hashrate
    pub hashrate_estimate: HashRate,
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

/// Events emitted by HashThreads back to the scheduler.
///
/// When a thread shuts down (USB unplug, fault, user request, etc.), it closes
/// its event channel instead of sending an event. The scheduler detects channel
/// closure and handles thread removal.
#[derive(Debug)]
pub enum HashThreadEvent {
    /// Valid share found (already filtered by pool_target)
    ShareFound(Share),

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

    #[error("Shutdown timeout")]
    ShutdownTimeout,

    #[error("Chip initialization failed: {0}")]
    InitializationFailed(String),
}

/// Signal from board to hash thread for shutdown coordination.
///
/// Board sends this via watch channel to signal thread shutdown. Thread checks
/// in its select loop and exits when signal changes from Running.
///
/// The reason is useful for logging but thread behavior is identical for all
/// non-Running variants: clean up and exit.
#[derive(Clone, Debug, PartialEq)]
pub enum ThreadRemovalSignal {
    /// Thread should continue running normally
    Running,

    /// Board was unplugged from USB
    BoardDisconnected,

    /// Board detected hardware fault (overheating, power issue, etc.)
    HardwareFault { description: String },

    /// User requested board disable via API
    UserRequested,

    /// Graceful system shutdown
    Shutdown,
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
    /// Get thread capabilities for scheduling decisions
    fn capabilities(&self) -> &HashThreadCapabilities;

    /// Update current work (shares from old work still valid)
    ///
    /// Thread continues hashing old work until new work is ready. Late-arriving
    /// shares from the old work can still be submitted (they're valuable).
    /// Returns the old task for potential resumption (None if thread was idle).
    ///
    /// Used when pool sends updated job (difficulty change, new transactions in
    /// mempool) but the work is fundamentally still valid.
    async fn update_work(
        &mut self,
        new_work: HashTask,
    ) -> std::result::Result<Option<HashTask>, HashThreadError>;

    /// Replace current work (old work invalidated)
    ///
    /// Old work is immediately invalid - discard it and don't submit shares
    /// from it. Returns the old task for tracking purposes (None if thread was
    /// idle).
    ///
    /// Used when blockchain tip changes (new prevhash) or pool signals
    /// clean_jobs.
    async fn replace_work(
        &mut self,
        new_work: HashTask,
    ) -> std::result::Result<Option<HashTask>, HashThreadError>;

    /// Put thread in idle state (low power, no hashing)
    ///
    /// Returns the current task if thread was working (None if already idle).
    /// Thread enters low-power mode, stops hashing.
    async fn go_idle(&mut self) -> std::result::Result<Option<HashTask>, HashThreadError>;

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
