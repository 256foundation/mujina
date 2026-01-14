//! CPU mining HashThread implementation.
//!
//! Provides the scheduler-facing interface for a CPU mining thread.
//! Uses the actor pattern with a dedicated `std::thread` for hashing.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, RwLock,
};

use async_trait::async_trait;
use tokio::sync::mpsc as tokio_mpsc;

use super::hasher::{self, MinerCommand};
use crate::{
    asic::hash_thread::{
        HashTask, HashThread, HashThreadCapabilities, HashThreadError, HashThreadEvent,
        HashThreadStatus,
    },
    types::HashRate,
};

/// CPU mining thread implementing the HashThread trait.
///
/// Wraps a `std::thread` running the CPU mining loop and provides
/// the async interface expected by the scheduler.
pub struct CpuHashThread {
    /// Human-readable name for logging.
    name: String,

    /// Channel for sending commands to the mining thread.
    command_tx: mpsc::Sender<MinerCommand>,

    /// Event sender (kept alive to prevent channel closure).
    /// Not currently used but must remain open for scheduler to consider thread active.
    #[expect(dead_code)]
    event_tx: tokio_mpsc::Sender<HashThreadEvent>,

    /// Event receiver (taken by scheduler).
    event_rx: Option<tokio_mpsc::Receiver<HashThreadEvent>>,

    /// Shared status for queries.
    status: Arc<RwLock<HashThreadStatus>>,

    /// Cached capabilities.
    capabilities: HashThreadCapabilities,

    /// Shutdown flag shared with mining thread.
    shutdown: Arc<AtomicBool>,

    /// Handle to the mining thread (for join on drop).
    _thread_handle: Option<std::thread::JoinHandle<()>>,
}

impl CpuHashThread {
    /// Create a new CPU mining thread.
    ///
    /// Spawns a dedicated `std::thread` for CPU-bound hashing.
    ///
    /// # Arguments
    ///
    /// * `name` - Human-readable name for logging
    /// * `duty_percent` - Target CPU duty cycle (1-100)
    pub fn new(name: String, duty_percent: u8) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (evt_tx, evt_rx) = tokio_mpsc::channel(100);

        let status = Arc::new(RwLock::new(HashThreadStatus::default()));
        let shutdown = Arc::new(AtomicBool::new(false));

        // Clone for the spawned thread
        let status_clone = Arc::clone(&status);
        let shutdown_clone = Arc::clone(&shutdown);
        let thread_name = name.clone();

        // Spawn the mining thread
        let handle = std::thread::Builder::new()
            .name(format!("cpu-miner-{}", name))
            .spawn(move || {
                hasher::run_mining_loop(
                    thread_name,
                    cmd_rx,
                    status_clone,
                    duty_percent,
                    shutdown_clone,
                );
            })
            .expect("Failed to spawn CPU mining thread");

        Self {
            name,
            command_tx: cmd_tx,
            event_tx: evt_tx,
            event_rx: Some(evt_rx),
            status,
            capabilities: HashThreadCapabilities {
                // Conservative estimate: ~5 MH/s per core on modern hardware
                hashrate_estimate: HashRate::from_megahashes(5.0),
            },
            shutdown,
            _thread_handle: Some(handle),
        }
    }

    /// Signal the mining thread to shut down.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = self.command_tx.send(MinerCommand::Shutdown);
    }
}

impl Drop for CpuHashThread {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[async_trait]
impl HashThread for CpuHashThread {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> &HashThreadCapabilities {
        &self.capabilities
    }

    async fn update_task(
        &mut self,
        new_task: HashTask,
    ) -> Result<Option<HashTask>, HashThreadError> {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();

        self.command_tx
            .send(MinerCommand::UpdateTask {
                task: new_task,
                response_tx,
            })
            .map_err(|_| HashThreadError::ChannelClosed("command channel closed".into()))?;

        response_rx
            .await
            .map_err(|_| HashThreadError::WorkAssignmentFailed("no response from thread".into()))?
    }

    async fn replace_task(
        &mut self,
        new_task: HashTask,
    ) -> Result<Option<HashTask>, HashThreadError> {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();

        self.command_tx
            .send(MinerCommand::ReplaceTask {
                task: new_task,
                response_tx,
            })
            .map_err(|_| HashThreadError::ChannelClosed("command channel closed".into()))?;

        response_rx
            .await
            .map_err(|_| HashThreadError::WorkAssignmentFailed("no response from thread".into()))?
    }

    async fn go_idle(&mut self) -> Result<Option<HashTask>, HashThreadError> {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();

        self.command_tx
            .send(MinerCommand::GoIdle { response_tx })
            .map_err(|_| HashThreadError::ChannelClosed("command channel closed".into()))?;

        response_rx
            .await
            .map_err(|_| HashThreadError::WorkAssignmentFailed("no response from thread".into()))?
    }

    fn take_event_receiver(&mut self) -> Option<tokio_mpsc::Receiver<HashThreadEvent>> {
        self.event_rx.take()
    }

    fn status(&self) -> HashThreadStatus {
        self.status.read().unwrap().clone()
    }
}
