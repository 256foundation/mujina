//! Message types for source-coordinator communication.
//!
//! This module implements the return-addressed envelope pattern for communication
//! between job sources and coordinators (scheduler, test harness, etc.).
//!
//! # Architecture
//!
//! ## SourceHandle - Arc-Based Identity
//!
//! `SourceHandle` uses Arc pointer equality for identity instead of explicit IDs.
//! Each `SourceHandle::new()` call creates a unique Arc allocation, providing
//! automatic identity without coordination:
//!
//! ```ignore
//! let handle1 = SourceHandle::new("pool-a".into(), tx1);
//! let handle2 = SourceHandle::new("pool-a".into(), tx2);
//! let handle3 = handle1.clone();
//!
//! assert_ne!(handle1, handle2);  // Different Arc pointers
//! assert_eq!(handle1, handle3);  // Same Arc pointer (cloned)
//! ```
//!
//! ## Communication Pattern
//!
//! Sources send events through a cloneable sender they're given at construction.
//! They receive commands through a unique receiver. The handle serves as the
//! return address—coordinators store it when receiving events and use it to
//! route commands back.
//!
//! ## Message Flow
//!
//! ```text
//! Source                          Coordinator
//!   |                                  |
//!   | send (handle, NewJob)            |
//!   |--------------------------------->|
//!   |                                  | (stores handle with job)
//!   |                                  |
//!   |    handle.submit_share(share)    |
//!   |<---------------------------------|
//!   | recv SubmitShare                 |
//! ```

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc;

use super::{JobTemplate, Share};

/// Handle to a job source (identity + communication).
///
/// This is a cloneable handle that serves three purposes:
/// 1. **Identity** - Arc pointer equality provides unique identity
/// 2. **Properties** - Query name and other metadata
/// 3. **Communication** - Send commands to the source
///
/// Handles are cheap to clone (just increments Arc refcount) and can be freely
/// passed between tasks, stored in collections, and used as HashMap keys.
#[derive(Clone, Debug)]
pub struct SourceHandle {
    inner: Arc<SourceHandleInner>,
}

#[derive(Debug)]
struct SourceHandleInner {
    name: String,
    command_tx: mpsc::Sender<SourceCommand>,
}

impl SourceHandle {
    /// Create a new source handle.
    ///
    /// Each call creates a unique handle via Arc allocation. The Arc pointer
    /// address becomes the handle's identity.
    pub fn new(name: String, command_tx: mpsc::Sender<SourceCommand>) -> Self {
        Self {
            inner: Arc::new(SourceHandleInner { name, command_tx }),
        }
    }

    /// Get the source name.
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// Submit a share to this source.
    pub async fn submit_share(&self, share: Share) -> Result<()> {
        self.inner
            .command_tx
            .send(SourceCommand::SubmitShare(share))
            .await
            .map_err(|_| anyhow::anyhow!("source disconnected"))
    }
}

// Hash based on Arc pointer address
impl Hash for SourceHandle {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.inner).hash(state);
    }
}

// Equality based on Arc pointer equality
impl PartialEq for SourceHandle {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Eq for SourceHandle {}

/// Events from sources (push, source-initiated).
///
/// Sources emit events when something happens (new work available, state change).
/// Events are passive notifications—they report what happened, not request action.
#[derive(Debug)]
pub enum SourceEvent {
    /// Update with new job (immediate preemption, old shares still valid).
    ///
    /// Scheduler immediately preempts all tasks from this source and assigns new
    /// work. Late-arriving shares from previous jobs should still be submitted.
    /// Typical case: pool sends updated job (difficulty change, new transactions
    /// in mempool).
    UpdateJob(JobTemplate),

    /// Replace all jobs (hard invalidation).
    ///
    /// Old work is no longer valid - discard current tasks and start fresh
    /// immediately. Don't submit shares from previous jobs. Used when blockchain
    /// tip changes (new prevhash) or pool signals clean_jobs.
    ReplaceJob(JobTemplate),

    /// Clear all jobs (no replacement).
    ///
    /// All current work is invalid, but source has no new job available yet.
    /// Scheduler should cancel all work from this source and wait for new job.
    /// Used during pool disconnection or when awaiting new block.
    ClearJobs,
}

/// Commands to sources (pull, coordinator-initiated).
///
/// Commands are active directives from the coordinator to the source.
/// They request the source to perform an action.
#[derive(Debug)]
pub enum SourceCommand {
    /// Submit this share to the pool/destination.
    SubmitShare(Share),
}
