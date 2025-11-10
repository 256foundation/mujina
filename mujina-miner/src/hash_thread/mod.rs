//! HashThread abstraction for schedulable mining workers.
//!
//! A HashThread represents a schedulable group of hashing engines that work
//! together to execute mining tasks. The scheduler assigns work to HashThreads
//! without needing to know about the underlying hardware topology (single chip,
//! chip chain, engine groups, etc.).
//!
//! # Share Processing
//!
//! HashThreads forward ALL shares from chips to the scheduler:
//!
//! 1. Chips are configured with low difficulty targets (e.g., diff 100-1000)
//!    for frequent health signals
//! 2. Thread computes hash for every chip share (required to determine
//!    what difficulty it meets)
//! 3. Thread forwards all shares via ShareFound events (since hash is already
//!    computed, filtering is trivial)
//! 4. Scheduler performs final filtering against job target
//!
//! This design keeps thread implementation simple, provides scheduler with
//! ground truth for per-thread hashrate, and centralizes filtering logic.
//! Message volume is manageable: ~1-2 shares/sec per chip at typical targets.

pub mod bm13xx;
pub mod task;

use async_trait::async_trait;
use tokio::sync::mpsc;

use self::task::{HashTask, Share};

/// HashThread capabilities reported to scheduler for work assignment decisions.
#[derive(Debug, Clone)]
pub struct HashThreadCapabilities {
    /// Estimated hashrate in H/s
    pub hashrate_estimate: f64,
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
    /// Current hashrate estimate in H/s
    pub hashrate: f64,

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

    /// Thread going offline (any reason: USB unplug, fault, user request, shutdown)
    GoingOffline,
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
