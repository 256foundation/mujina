//! BM13xx HashThread implementation.
//!
//! This module provides the HashThread implementation for BM13xx family ASIC
//! chips (BM1362, BM1366, BM1370, etc.). A BM13xxThread represents a chain of
//! BM13xx chips connected via a shared serial bus.
//!
//! The thread is implemented as an actor task that monitors the serial bus for
//! chip responses, filters shares, and manages work assignment.

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use futures::{sink::Sink, stream::Stream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_stream::StreamExt;

use super::{
    task::HashTask, HashThread, HashThreadCapabilities, HashThreadError, HashThreadEvent,
    HashThreadStatus,
};
use crate::{
    asic::bm13xx::{self, protocol},
    board::bitaxe::ThreadRemovalSignal,
};

/// Command messages sent from scheduler to thread
#[derive(Debug)]
enum ThreadCommand {
    /// Update work (old shares still valid)
    UpdateWork {
        new_task: HashTask,
        response_tx: oneshot::Sender<std::result::Result<Option<HashTask>, HashThreadError>>,
    },

    /// Replace work (old shares invalid)
    ReplaceWork {
        new_task: HashTask,
        response_tx: oneshot::Sender<std::result::Result<Option<HashTask>, HashThreadError>>,
    },

    /// Go idle (stop hashing, low power)
    GoIdle {
        response_tx: oneshot::Sender<std::result::Result<Option<HashTask>, HashThreadError>>,
    },

    /// Shutdown the thread
    #[expect(unused)]
    Shutdown,
}

/// BM13xx HashThread implementation.
///
/// Represents a chain of BM13xx chips as a schedulable worker. The thread
/// manages serial communication with chips, filters shares, and reports events.
pub struct BM13xxThread {
    /// Channel for sending commands to the actor
    command_tx: mpsc::Sender<ThreadCommand>,

    /// Event receiver (taken by scheduler)
    event_rx: Option<mpsc::Receiver<HashThreadEvent>>,

    /// Cached capabilities
    capabilities: HashThreadCapabilities,

    /// Shared status (updated by actor task)
    status: Arc<RwLock<HashThreadStatus>>,
}

impl BM13xxThread {
    /// Create a new BM13xx thread with Stream/Sink for chip communication
    ///
    /// Thread starts idle (no task assigned). Takes ownership of streams for
    /// direct communication with BM13xx chips.
    ///
    /// # Arguments
    /// * `chip_responses` - Stream of decoded responses from chips
    /// * `chip_commands` - Sink for sending encoded commands to chips
    /// * `removal_rx` - Watch channel for board-triggered removal
    pub fn new<R, W>(
        chip_responses: R,
        chip_commands: W,
        removal_rx: watch::Receiver<ThreadRemovalSignal>,
    ) -> Self
    where
        R: Stream<Item = Result<protocol::Response, std::io::Error>> + Unpin + Send + 'static,
        W: Sink<protocol::Command> + Unpin + Send + 'static,
        W::Error: std::fmt::Debug,
    {
        let (cmd_tx, cmd_rx) = mpsc::channel(10);
        let (evt_tx, evt_rx) = mpsc::channel(100);

        let status = Arc::new(RwLock::new(HashThreadStatus::default()));
        let status_clone = Arc::clone(&status);

        // Spawn the actor task (streams moved into task)
        tokio::spawn(async move {
            bm13xx_thread_actor(
                cmd_rx,
                evt_tx,
                removal_rx,
                status_clone,
                chip_responses,
                chip_commands,
            )
            .await;
        });

        Self {
            command_tx: cmd_tx,
            event_rx: Some(evt_rx),
            capabilities: HashThreadCapabilities {
                hashrate_estimate: 1_000_000_000.0, // Stub: 1 GH/s
            },
            status,
        }
    }
}

#[async_trait]
impl HashThread for BM13xxThread {
    fn capabilities(&self) -> &HashThreadCapabilities {
        &self.capabilities
    }

    async fn update_work(
        &mut self,
        new_work: HashTask,
    ) -> std::result::Result<Option<HashTask>, HashThreadError> {
        let (response_tx, response_rx) = oneshot::channel();

        self.command_tx
            .send(ThreadCommand::UpdateWork {
                new_task: new_work,
                response_tx,
            })
            .await
            .map_err(|_| HashThreadError::ChannelClosed("command channel closed".into()))?;

        response_rx
            .await
            .map_err(|_| HashThreadError::WorkAssignmentFailed("no response from thread".into()))?
    }

    async fn replace_work(
        &mut self,
        new_work: HashTask,
    ) -> std::result::Result<Option<HashTask>, HashThreadError> {
        let (response_tx, response_rx) = oneshot::channel();

        self.command_tx
            .send(ThreadCommand::ReplaceWork {
                new_task: new_work,
                response_tx,
            })
            .await
            .map_err(|_| HashThreadError::ChannelClosed("command channel closed".into()))?;

        response_rx
            .await
            .map_err(|_| HashThreadError::WorkAssignmentFailed("no response from thread".into()))?
    }

    async fn go_idle(&mut self) -> std::result::Result<Option<HashTask>, HashThreadError> {
        let (response_tx, response_rx) = oneshot::channel();

        self.command_tx
            .send(ThreadCommand::GoIdle { response_tx })
            .await
            .map_err(|_| HashThreadError::ChannelClosed("command channel closed".into()))?;

        response_rx
            .await
            .map_err(|_| HashThreadError::WorkAssignmentFailed("no response from thread".into()))?
    }

    fn take_event_receiver(&mut self) -> Option<mpsc::Receiver<HashThreadEvent>> {
        self.event_rx.take()
    }

    fn status(&self) -> HashThreadStatus {
        self.status.read().unwrap().clone()
    }
}

/// Internal actor task for BM13xxThread.
///
/// This runs as an independent Tokio task and handles:
/// - Commands from scheduler (update/replace work, go idle, shutdown)
/// - Removal signal from board (USB unplug, fault, etc.)
/// - Serial communication with chips (TODO)
/// - Share filtering and event emission (TODO)
///
/// Thread starts idle (no task). Scheduler assigns work when available.
async fn bm13xx_thread_actor<R, W>(
    mut cmd_rx: mpsc::Receiver<ThreadCommand>,
    evt_tx: mpsc::Sender<HashThreadEvent>,
    mut removal_rx: watch::Receiver<ThreadRemovalSignal>,
    status: Arc<RwLock<HashThreadStatus>>,
    mut chip_responses: R,
    _chip_commands: W,
) where
    R: Stream<Item = Result<bm13xx::protocol::Response, std::io::Error>> + Unpin,
    W: Sink<bm13xx::protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    // Thread starts idle (no task)
    let mut current_task: Option<HashTask> = None;

    loop {
        tokio::select! {
            // Removal signal (highest priority)
            _ = removal_rx.changed() => {
                let signal = removal_rx.borrow().clone();  // Clone to avoid holding borrow across await
                match signal {
                    ThreadRemovalSignal::Running => {
                        // False alarm - still running
                    }
                    reason => {
                        tracing::info!("BM13xx thread removal: {:?}", reason);

                        // Send going offline event
                        evt_tx.send(HashThreadEvent::GoingOffline).await.ok();

                        // Update status
                        {
                            let mut s = status.write().unwrap();
                            s.is_active = false;
                        }

                        break;  // Exit actor loop on any removal reason
                    }
                }
            }

            // Commands from scheduler
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    ThreadCommand::UpdateWork { new_task, response_tx } => {
                        let job_desc = if let Some(ref old) = current_task {
                            format!("old={}, new={}", old.job_id, new_task.job_id)
                        } else {
                            format!("from idle, new={}", new_task.job_id)
                        };
                        tracing::debug!("BM13xx thread updating work: {}", job_desc);

                        // Return old task (None if was idle)
                        let old_task = current_task.replace(new_task);

                        // Update status
                        {
                            let mut s = status.write().unwrap();
                            s.is_active = true;  // Now has work
                        }

                        response_tx.send(Ok(old_task)).ok();

                        // TODO: Send new job to chips via serial
                    }

                    ThreadCommand::ReplaceWork { new_task, response_tx } => {
                        let job_desc = if let Some(ref old) = current_task {
                            format!("old={}, new={}", old.job_id, new_task.job_id)
                        } else {
                            format!("from idle, new={}", new_task.job_id)
                        };
                        tracing::debug!("BM13xx thread replacing work: {}", job_desc);

                        // Return old task (None if was idle)
                        let old_task = current_task.replace(new_task);

                        // Update status
                        {
                            let mut s = status.write().unwrap();
                            s.is_active = true;  // Now has work
                        }

                        response_tx.send(Ok(old_task)).ok();

                        // TODO: Send new job to chips via serial, invalidate old shares
                    }

                    ThreadCommand::GoIdle { response_tx } => {
                        tracing::debug!("BM13xx thread going idle");

                        // Take current task and go idle
                        let old_task = current_task.take();

                        // Update status
                        {
                            let mut s = status.write().unwrap();
                            s.is_active = false;  // Now idle
                        }

                        response_tx.send(Ok(old_task)).ok();

                        // TODO: Put chips in low power mode
                    }

                    ThreadCommand::Shutdown => {
                        tracing::info!("BM13xx thread shutting down");
                        evt_tx.send(HashThreadEvent::GoingOffline).await.ok();
                        break;
                    }
                }
            }

            // Chip responses from serial stream
            Some(result) = chip_responses.next() => {
                match result {
                    Ok(response) => {
                        match response {
                            bm13xx::protocol::Response::Nonce { nonce, job_id, version, midstate_num, subcore_id } => {
                                tracing::debug!(
                                    "Chip nonce: job_id={}, nonce=0x{:08x}, version=0x{:04x}, midstate={}, subcore={}",
                                    job_id, nonce, version, midstate_num, subcore_id
                                );
                                // TODO: Calculate hash, filter by pool_target, emit ShareFound event
                            }

                            bm13xx::protocol::Response::ReadRegister { chip_address, register } => {
                                tracing::trace!("Register read from chip {}: {:?}", chip_address, register);
                                // Ignore register reads for now
                            }
                        }
                    }

                    Err(e) => {
                        tracing::error!("Serial decode error: {:?}", e);
                        // TODO: Emit error event, potentially trigger going offline if persistent
                    }
                }
            }
        }
    }

    tracing::debug!("BM13xx thread actor exiting");
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use std::time::Duration;

    /// Create a mock response stream from a vector of responses
    ///
    /// Returns a stream that yields the given responses as Ok results.
    /// Useful for testing thread behavior with specific message sequences.
    fn mock_response_stream(
        responses: Vec<bm13xx::protocol::Response>,
    ) -> impl Stream<Item = Result<bm13xx::protocol::Response, std::io::Error>> {
        stream::iter(responses.into_iter().map(Ok))
    }

    /// Create a mock command sink that discards all commands
    ///
    /// Returns a sink that accepts commands but does nothing with them.
    /// Useful for tests that don't care about outgoing commands.
    fn mock_command_sink() -> futures::sink::Drain<bm13xx::protocol::Command> {
        futures::sink::drain()
    }

    #[tokio::test]
    async fn test_thread_creation() {
        let (_removal_tx, removal_rx) = watch::channel(ThreadRemovalSignal::Running);

        // Create empty streams for testing
        let responses = mock_response_stream(vec![]);
        let commands = mock_command_sink();

        let thread = BM13xxThread::new(responses, commands, removal_rx);

        // Thread ID is based on task, not a debug name
        assert_eq!(thread.capabilities().hashrate_estimate, 1_000_000_000.0);
    }

    #[tokio::test]
    async fn test_removal_signal_triggers_going_offline() {
        let (removal_tx, removal_rx) = watch::channel(ThreadRemovalSignal::Running);
        let mut thread = BM13xxThread::new(
            mock_response_stream(vec![]),
            mock_command_sink(),
            removal_rx,
        );

        // Take event receiver
        let mut event_rx = thread.take_event_receiver().unwrap();

        // Trigger removal with specific reason
        removal_tx
            .send(ThreadRemovalSignal::BoardDisconnected)
            .unwrap();

        // Should receive GoingOffline event
        let event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .expect("timeout waiting for event")
            .expect("event channel closed");

        matches!(event, HashThreadEvent::GoingOffline);
    }

    #[tokio::test]
    async fn test_update_work_from_idle() {
        let (_removal_tx, removal_rx) = watch::channel(ThreadRemovalSignal::Running);
        let mut thread = BM13xxThread::new(
            mock_response_stream(vec![]),
            mock_command_sink(),
            removal_rx,
        );

        // Update from idle (None) to active task
        let task = HashTask::active(123, 1);
        let old_task = thread.update_work(task).await.unwrap();

        assert!(
            old_task.is_none(),
            "Should return None when updating from idle"
        );
    }

    #[tokio::test]
    async fn test_update_work_from_active() {
        let (_removal_tx, removal_rx) = watch::channel(ThreadRemovalSignal::Running);
        let mut thread = BM13xxThread::new(
            mock_response_stream(vec![]),
            mock_command_sink(),
            removal_rx,
        );

        // First assign work
        let task1 = HashTask::active(456, 1);
        thread.update_work(task1).await.unwrap();

        tokio::time::sleep(Duration::from_millis(10)).await;

        // Update to new work
        let task2 = HashTask::active(789, 1);
        let old_task = thread.update_work(task2).await.unwrap();

        assert!(old_task.is_some(), "Should return old task");
        assert_eq!(old_task.unwrap().job_id, 456);
    }

    #[tokio::test]
    async fn test_replace_work() {
        let (_removal_tx, removal_rx) = watch::channel(ThreadRemovalSignal::Running);
        let mut thread = BM13xxThread::new(
            mock_response_stream(vec![]),
            mock_command_sink(),
            removal_rx,
        );

        // First update to active work
        let task1 = HashTask::active(456, 1);
        thread.update_work(task1).await.unwrap();

        // Give actor time to process
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Replace it
        let task2 = HashTask::active(789, 2);
        let old_task = thread.replace_work(task2).await.unwrap();

        assert!(old_task.is_some(), "Should return old task");
        assert_eq!(old_task.unwrap().job_id, 456, "Should return old task");
    }

    #[tokio::test]
    async fn test_go_idle() {
        let (_removal_tx, removal_rx) = watch::channel(ThreadRemovalSignal::Running);
        let mut thread = BM13xxThread::new(
            mock_response_stream(vec![]),
            mock_command_sink(),
            removal_rx,
        );

        // Assign work first
        thread.update_work(HashTask::dummy(100)).await.unwrap();

        tokio::time::sleep(Duration::from_millis(10)).await;

        // Go idle
        let old_task = thread.go_idle().await.unwrap();

        assert!(old_task.is_some(), "Should return old task when going idle");
        assert_eq!(old_task.unwrap().job_id, 100);
    }

    #[tokio::test]
    async fn test_go_idle_when_already_idle() {
        let (_removal_tx, removal_rx) = watch::channel(ThreadRemovalSignal::Running);
        let mut thread = BM13xxThread::new(
            mock_response_stream(vec![]),
            mock_command_sink(),
            removal_rx,
        );

        // Thread starts idle, try to go idle again
        let old_task = thread.go_idle().await.unwrap();

        assert!(old_task.is_none(), "Should return None when already idle");
    }

    #[tokio::test]
    async fn test_status_updates() {
        let (_removal_tx, removal_rx) = watch::channel(ThreadRemovalSignal::Running);
        let mut thread = BM13xxThread::new(
            mock_response_stream(vec![]),
            mock_command_sink(),
            removal_rx,
        );

        let status_before = thread.status();
        assert!(
            !status_before.is_active,
            "Should start inactive (idle, no task)"
        );

        // Update to active work
        thread.update_work(HashTask::active(789, 1)).await.unwrap();

        // Give actor time to update status
        tokio::time::sleep(Duration::from_millis(10)).await;

        let status_after = thread.status();
        assert!(
            status_after.is_active,
            "Should be active after receiving task"
        );

        // Go idle
        thread.go_idle().await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;

        let status_idle = thread.status();
        assert!(
            !status_idle.is_active,
            "Should be inactive after going idle"
        );
    }

    #[tokio::test]
    async fn test_thread_processes_known_good_nonce() {
        use crate::job_source::test_blocks::block_881423;

        let (_removal_tx, removal_rx) = watch::channel(ThreadRemovalSignal::Running);

        // Create stream with known-good nonce from block 881423
        let responses = vec![bm13xx::protocol::Response::Nonce {
            nonce: block_881423::NONCE, // 0x5d6472f7 - actual winning nonce
            job_id: 1,
            // Extract version bits - chip returns top 16 bits it can roll
            version: (block_881423::VERSION.to_consensus() >> 16) as u16,
            midstate_num: 0,
            subcore_id: 0,
        }];

        let chip_responses = mock_response_stream(responses);
        let chip_commands = mock_command_sink();

        let thread = BM13xxThread::new(chip_responses, chip_commands, removal_rx);

        // Give actor time to process the nonce
        tokio::time::sleep(Duration::from_millis(50)).await;

        // For now, just verify thread doesn't crash when processing nonce
        // TODO: When ShareFound events are implemented, verify event is emitted
        let _status = thread.status();
        // Thread should still be running (not crashed)
    }
}
