//! BZM2 HashThread implementation.
//!
//! This is the first thread integration pass: it mirrors the BM13xx actor
//! structure and wiring while keeping mining/job execution minimal until
//! WRITEJOB/READRESULT support lands.

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use futures::{SinkExt, sink::Sink, stream::Stream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_stream::StreamExt;

use super::protocol;
use crate::{
    asic::hash_thread::{
        BoardPeripherals, HashTask, HashThread, HashThreadCapabilities, HashThreadError,
        HashThreadEvent, HashThreadStatus, ThreadRemovalSignal,
    },
    tracing::prelude::*,
    types::HashRate,
};

#[derive(Debug)]
enum ThreadCommand {
    UpdateTask {
        new_task: HashTask,
        response_tx: oneshot::Sender<std::result::Result<Option<HashTask>, HashThreadError>>,
    },
    ReplaceTask {
        new_task: HashTask,
        response_tx: oneshot::Sender<std::result::Result<Option<HashTask>, HashThreadError>>,
    },
    GoIdle {
        response_tx: oneshot::Sender<std::result::Result<Option<HashTask>, HashThreadError>>,
    },
    #[expect(unused)]
    Shutdown,
}

/// HashThread wrapper for a BZM2 board worker.
pub struct Bzm2Thread {
    name: String,
    command_tx: mpsc::Sender<ThreadCommand>,
    event_rx: Option<mpsc::Receiver<HashThreadEvent>>,
    capabilities: HashThreadCapabilities,
    status: Arc<RwLock<HashThreadStatus>>,
}

impl Bzm2Thread {
    pub fn new<R, W>(
        name: String,
        chip_responses: R,
        chip_commands: W,
        peripherals: BoardPeripherals,
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

        tokio::spawn(async move {
            bzm2_thread_actor(
                cmd_rx,
                evt_tx,
                removal_rx,
                status_clone,
                chip_responses,
                chip_commands,
                peripherals,
            )
            .await;
        });

        Self {
            name,
            command_tx: cmd_tx,
            event_rx: Some(evt_rx),
            capabilities: HashThreadCapabilities {
                hashrate_estimate: HashRate::default(),
            },
            status,
        }
    }
}

#[async_trait]
impl HashThread for Bzm2Thread {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> &HashThreadCapabilities {
        &self.capabilities
    }

    async fn update_task(
        &mut self,
        new_task: HashTask,
    ) -> std::result::Result<Option<HashTask>, HashThreadError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ThreadCommand::UpdateTask {
                new_task,
                response_tx,
            })
            .await
            .map_err(|_| HashThreadError::ChannelClosed("command channel closed".into()))?;

        response_rx
            .await
            .map_err(|_| HashThreadError::WorkAssignmentFailed("no response from thread".into()))?
    }

    async fn replace_task(
        &mut self,
        new_task: HashTask,
    ) -> std::result::Result<Option<HashTask>, HashThreadError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ThreadCommand::ReplaceTask {
                new_task,
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
        self.status.read().expect("status lock poisoned").clone()
    }
}

async fn initialize_chip<W>(
    chip_commands: &mut W,
    peripherals: &mut BoardPeripherals,
) -> Result<(), HashThreadError>
where
    W: Sink<protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    if let Some(ref mut asic_enable) = peripherals.asic_enable {
        asic_enable.enable().await.map_err(|e| {
            HashThreadError::InitializationFailed(format!("failed to enable ASIC: {}", e))
        })?;
    }

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    chip_commands
        .send(protocol::Command::Noop {
            asic_hw_id: protocol::DEFAULT_ASIC_ID,
        })
        .await
        .map_err(|e| {
            HashThreadError::InitializationFailed(format!("failed to send BZM2 NOOP: {:?}", e))
        })?;

    Ok(())
}

async fn bzm2_thread_actor<R, W>(
    mut cmd_rx: mpsc::Receiver<ThreadCommand>,
    evt_tx: mpsc::Sender<HashThreadEvent>,
    mut removal_rx: watch::Receiver<ThreadRemovalSignal>,
    status: Arc<RwLock<HashThreadStatus>>,
    mut chip_responses: R,
    mut chip_commands: W,
    mut peripherals: BoardPeripherals,
) where
    R: Stream<Item = Result<protocol::Response, std::io::Error>> + Unpin,
    W: Sink<protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    if let Some(ref mut asic_enable) = peripherals.asic_enable
        && let Err(e) = asic_enable.disable().await
    {
        warn!(error = %e, "Failed to disable BZM2 ASIC on thread startup");
    }

    let mut chip_initialized = false;
    let mut current_task: Option<HashTask> = None;
    let mut status_ticker = tokio::time::interval(std::time::Duration::from_secs(5));
    status_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = removal_rx.changed() => {
                let signal = removal_rx.borrow().clone();
                if signal != ThreadRemovalSignal::Running {
                    {
                        let mut s = status.write().expect("status lock poisoned");
                        s.is_active = false;
                    }
                    break;
                }
            }

            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    ThreadCommand::UpdateTask { new_task, response_tx } => {
                        if !chip_initialized {
                            if let Err(e) = initialize_chip(&mut chip_commands, &mut peripherals).await {
                                error!(error = %e, "BZM2 chip initialization failed");
                                let _ = response_tx.send(Err(e));
                                continue;
                            }
                            chip_initialized = true;
                        }

                        let old_task = current_task.replace(new_task);
                        {
                            let mut s = status.write().expect("status lock poisoned");
                            s.is_active = true;
                        }
                        let _ = response_tx.send(Ok(old_task));
                    }
                    ThreadCommand::ReplaceTask { new_task, response_tx } => {
                        if !chip_initialized {
                            if let Err(e) = initialize_chip(&mut chip_commands, &mut peripherals).await {
                                error!(error = %e, "BZM2 chip initialization failed");
                                let _ = response_tx.send(Err(e));
                                continue;
                            }
                            chip_initialized = true;
                        }

                        let old_task = current_task.replace(new_task);
                        {
                            let mut s = status.write().expect("status lock poisoned");
                            s.is_active = true;
                        }
                        let _ = response_tx.send(Ok(old_task));
                    }
                    ThreadCommand::GoIdle { response_tx } => {
                        let old_task = current_task.take();
                        {
                            let mut s = status.write().expect("status lock poisoned");
                            s.is_active = false;
                        }
                        let _ = response_tx.send(Ok(old_task));
                    }
                    ThreadCommand::Shutdown => {
                        break;
                    }
                }
            }

            Some(result) = chip_responses.next() => {
                match result {
                    Ok(protocol::Response::Noop { asic_hw_id, signature }) => {
                        trace!(asic_hw_id, signature = ?signature, "BZM2 NOOP response");
                    }
                    Ok(protocol::Response::ReadReg { asic_hw_id, data }) => {
                        trace!(asic_hw_id, data = ?data, "BZM2 READREG response");
                    }
                    Err(e) => {
                        warn!(error = %e, "Error reading BZM2 response stream");
                    }
                }
            }

            _ = status_ticker.tick() => {
                let snapshot = status.read().expect("status lock poisoned").clone();
                let _ = evt_tx.send(HashThreadEvent::StatusUpdate(snapshot)).await;
            }
        }
    }
}
