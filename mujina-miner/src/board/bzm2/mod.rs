use anyhow::Result as AnyhowResult;
use async_trait::async_trait;
use tokio::sync::{mpsc, watch};

use super::{BackplaneConnector, BoardInfo, VirtualBoardDescriptor};
use crate::{
    api_client::types::{BoardTelemetry, ThreadTelemetry},
    asic::{
        bzm2::{Bzm2Thread, Bzm2ThreadConfig, Bzm2ThreadHandle},
        hash_thread::{
            HashTask, HashThread, HashThreadCapabilities, HashThreadEvent, HashThreadStatus,
        },
    },
    tracing::prelude::*,
};

mod calibration;
mod config;
mod telemetry;
#[cfg(all(test, unix))]
mod test_support;

pub use config::Bzm2RuntimeConfig;
use telemetry::{publish_thread_status, publish_thread_telemetry};

// Register this board type with the inventory system
inventory::submit! {
    VirtualBoardDescriptor {
        device_type: "bzm2",
        name: "BZM2",
        create_fn: || Box::pin(create_bzm2_board()),
    }
}

/// Errors raised by BZM2 board bring-up and hardware control.
#[derive(Debug)]
pub enum BoardError {
    /// Board initialization failed (bring-up, serial open, calibration).
    InitializationFailed(String),
    /// Serial or file I/O failure while talking to the board.
    Communication(std::io::Error),
}

impl std::fmt::Display for BoardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BoardError::InitializationFailed(msg) => {
                write!(f, "board initialization failed: {msg}")
            }
            BoardError::Communication(err) => write!(f, "board communication error: {err}"),
        }
    }
}

impl std::error::Error for BoardError {}

impl From<std::io::Error> for BoardError {
    fn from(err: std::io::Error) -> Self {
        BoardError::Communication(err)
    }
}

pub struct Bzm2Board {
    config: Bzm2RuntimeConfig,
    shutdown_handles: Vec<Bzm2ThreadHandle>,
    telemetry_tx: watch::Sender<BoardTelemetry>,
}

impl Bzm2Board {
    pub fn new(config: Bzm2RuntimeConfig, telemetry_tx: watch::Sender<BoardTelemetry>) -> Self {
        Self {
            config,
            shutdown_handles: Vec::new(),
            telemetry_tx,
        }
    }
}

impl Bzm2Board {
    fn board_info(&self) -> BoardInfo {
        BoardInfo {
            model: "BZM2".into(),
            firmware_version: None,
            serial_number: Some(self.config.device_id()),
        }
    }

    async fn shutdown(&mut self) -> AnyhowResult<()> {
        // A deliberate shutdown, so a refused stop is still worth saying: it
        // means a thread was never asked, and the rails are about to come down
        // underneath it.
        for handle in &self.shutdown_handles {
            // Only a live-but-not-listening thread is worth a warning here.
            // A deliberate shutdown reaches threads that have already stopped
            // as a matter of course.
            if handle.shutdown().still_running() {
                warn!(
                    "a hash thread is alive and would not accept its stop; shutting down around it"
                );
            }
        }
        self.shutdown_handles.clear();
        self.telemetry_tx.send_modify(|state| {
            for thread in &mut state.threads {
                thread.is_active = false;
                thread.hashrate = 0;
            }
        });
        Ok(())
    }

    async fn create_hash_threads(&mut self) -> AnyhowResult<Vec<Box<dyn HashThread>>> {
        let mut threads: Vec<Box<dyn HashThread>> = Vec::new();
        let mut thread_states = Vec::new();
        let bus_layouts = self.resolve_bus_layouts().await?;
        self.telemetry_tx.send_modify(|_state| {});

        for (index, serial_path) in self.config.serial_paths.iter().enumerate() {
            let stream = crate::transport::serial::open_with_platform_cflag(
                serial_path,
                self.config.baud_rate,
            )
            .map_err(|err| {
                BoardError::InitializationFailed(format!(
                    "Failed to open BZM2 serial transport {}: {}",
                    serial_path, err
                ))
            })?;
            let (reader, writer, control) = config::split_chain_port(stream);
            let thread_name = format!("BZM2 UART {}", index);
            let mut config = Bzm2ThreadConfig::new(
                serial_path.clone(),
                self.config.baud_rate,
                self.config.nominal_hashrate_ths,
            );
            config.timestamp_count = self.config.timestamp_count;
            config.nonce_gap = self.config.nonce_gap;
            config.result_min_difficulty = self.config.result_min_difficulty;
            config.dispatch_interval = self.config.dispatch_interval;
            config.dts_vs_generation = self.config.dts_vs_generation;
            // Hand the thread the chain it is actually addressing. Without this
            // it cannot key result bookkeeping by (asic, engine), and it reports
            // a single-ASIC nameplate for the whole bus.
            // WIRE ids, not the global ones the layout carries. The thread
            // compares these against addresses that arrive on the chain, and a
            // chain is addressed from `start_id` on every bus regardless of
            // where the bus sits in the machine-wide numbering.
            config.asic_ids = bus_layouts
                .iter()
                .find(|layout| layout.serial_path == *serial_path)
                .map(|layout| layout.wire_asic_ids(self.config.enumeration.start_id))
                .filter(|ids| !ids.is_empty())
                .unwrap_or_else(|| vec![self.config.enumeration.start_id]);

            let thread = Bzm2Thread::new(thread_name.clone(), reader, writer, control, config);
            self.shutdown_handles.push(thread.shutdown_handle());
            thread_states.push(ThreadTelemetry {
                name: thread_name,
                hashrate: 0,
                is_active: false,
            });
            threads.push(Box::new(Bzm2ManagedThread::new(
                Box::new(thread),
                self.telemetry_tx.clone(),
                index,
            )));
        }

        self.telemetry_tx.send_modify(|state| {
            state.threads = thread_states.clone();
        });

        Ok(threads)
    }
}

struct Bzm2ManagedThread {
    inner: Box<dyn HashThread>,
    telemetry_tx: watch::Sender<BoardTelemetry>,
    thread_index: usize,
}

impl Bzm2ManagedThread {
    fn new(
        inner: Box<dyn HashThread>,
        telemetry_tx: watch::Sender<BoardTelemetry>,
        thread_index: usize,
    ) -> Self {
        Self {
            inner,
            telemetry_tx,
            thread_index,
        }
    }

    fn publish_status(&self, status: &HashThreadStatus) {
        publish_thread_status(&self.telemetry_tx, self.thread_index, status);
    }
}

#[async_trait]
impl HashThread for Bzm2ManagedThread {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn capabilities(&self) -> &HashThreadCapabilities {
        self.inner.capabilities()
    }

    async fn configure(&mut self) -> AnyhowResult<()> {
        self.inner.configure().await
    }

    async fn update_task(&mut self, new_task: HashTask) -> AnyhowResult<Option<HashTask>> {
        let result = self.inner.update_task(new_task).await;
        self.publish_status(&self.inner.status());
        result
    }

    async fn replace_task(&mut self, new_task: HashTask) -> AnyhowResult<Option<HashTask>> {
        let result = self.inner.replace_task(new_task).await;
        self.publish_status(&self.inner.status());
        result
    }

    async fn go_idle(&mut self) -> AnyhowResult<Option<HashTask>> {
        let result = self.inner.go_idle().await;
        self.publish_status(&self.inner.status());
        result
    }

    fn take_event_receiver(&mut self) -> Option<mpsc::Receiver<HashThreadEvent>> {
        let inner_rx = self.inner.take_event_receiver()?;
        let (event_tx, event_rx) = mpsc::channel(64);
        tokio::spawn(forward_thread_events(
            inner_rx,
            event_tx,
            self.telemetry_tx.clone(),
            self.thread_index,
        ));
        Some(event_rx)
    }

    fn status(&self) -> HashThreadStatus {
        self.inner.status()
    }
}
/// Relay one chain's events: its telemetry into the board's own state, and
/// what the scheduler acts on to the scheduler.
///
/// **TELEMETRY STOPS HERE.** Every event used to be forwarded, and the
/// scheduler does nothing with a telemetry update but trace it. A chain on a
/// full stream sends about a hundred of them every 200 ms, into two 64-slot
/// queues that only the scheduler's loop drains -- and that loop awaits each
/// chain's `update_task` in turn. While it waits on one chain, another's
/// queues fill, that chain's actor blocks sending telemetry, and it can then
/// never answer the `update_task` the scheduler sends it next: a deadlock
/// from the second chain onward (the pre-mining review, from the channel
/// sizes; one chain cannot reach it, which is why a single-chain run did not).
///
/// A status update is latest-wins and is already in the board state when
/// this forwards it, so one that finds the scheduler's queue full is dropped
/// and counted rather than awaited. Everything else is awaited as before.
async fn forward_thread_events(
    mut inner_rx: mpsc::Receiver<HashThreadEvent>,
    event_tx: mpsc::Sender<HashThreadEvent>,
    telemetry_tx: watch::Sender<BoardTelemetry>,
    thread_index: usize,
) {
    let mut dropped_status: u64 = 0;
    while let Some(event) = inner_rx.recv().await {
        match &event {
            HashThreadEvent::TelemetryUpdate(update) => {
                publish_thread_telemetry(&telemetry_tx, thread_index, update);
                continue;
            }
            HashThreadEvent::StatusUpdate(status) => {
                publish_thread_status(&telemetry_tx, thread_index, status);
                match event_tx.try_send(event) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        dropped_status += 1;
                        if dropped_status == 1 || dropped_status.is_multiple_of(1000) {
                            debug!(
                                thread_index,
                                dropped_status,
                                "Scheduler queue full: status update dropped (the board state has it)"
                            );
                        }
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => break,
                }
                continue;
            }
            _ => {}
        }
        if event_tx.send(event).await.is_err() {
            break;
        }
    }
}

async fn create_bzm2_board() -> AnyhowResult<BackplaneConnector> {
    let config = Bzm2RuntimeConfig::from_env()
        .ok_or_else(|| anyhow::anyhow!("BZM2 not configured (MUJINA_BZM2_SERIAL not set)"))?;

    let serial = config.device_id();
    let initial_state = BoardTelemetry {
        name: serial.clone(),
        model: "BZM2".into(),
        serial: Some(serial),
        ..Default::default()
    };
    let (telemetry_tx, telemetry_rx) = watch::channel(initial_state);

    let mut board = Bzm2Board::new(config, telemetry_tx);
    let info = board.board_info();

    // Bring-up, enumeration, calibration, and the monitor/command loops
    // all happen here; the returned threads are ready for the scheduler.
    let threads = board.create_hash_threads().await?;

    let shutdown = Box::pin(async move {
        if let Err(err) = board.shutdown().await {
            warn!(error = %err, "BZM2 board shutdown reported an error");
        }
    });

    Ok(BackplaneConnector {
        info,
        threads,
        telemetry_rx,
        command_tx: None,
        shutdown: Some(shutdown),
    })
}

#[cfg(all(test, unix))]
mod tests {

    use super::*;

    use std::time::Duration;

    /// A chain streaming telemetry must never wait on a scheduler that is not
    /// reading: while the scheduler awaits another chain's `update_task`,
    /// nothing drains this chain's queue, and a chain blocked on telemetry
    /// cannot answer the scheduler when it is asked next.
    #[tokio::test]
    async fn a_chain_never_waits_on_the_scheduler_to_publish_telemetry() {
        let (inner_tx, inner_rx) = mpsc::channel(64);
        let (event_tx, mut event_rx) = mpsc::channel(64);
        let (telemetry_tx, _telemetry_rx) = watch::channel(BoardTelemetry::default());
        tokio::spawn(forward_thread_events(inner_rx, event_tx, telemetry_tx, 0));

        // Ten seconds of one chain's telemetry, and its status updates, with
        // the scheduler reading nothing.
        let streamed = tokio::time::timeout(Duration::from_secs(5), async {
            for _ in 0..5_000 {
                inner_tx
                    .send(HashThreadEvent::TelemetryUpdate(
                        crate::asic::hash_thread::HashThreadTelemetryUpdate {
                            temperatures: Vec::new(),
                            powers: Vec::new(),
                            asic: None,
                        },
                    ))
                    .await
                    .unwrap();
            }
            for _ in 0..500 {
                inner_tx
                    .send(HashThreadEvent::StatusUpdate(HashThreadStatus::default()))
                    .await
                    .unwrap();
            }
        })
        .await;
        assert!(
            streamed.is_ok(),
            "the chain blocked on a scheduler that was not reading"
        );
        assert!(
            event_rx.len() <= 64,
            "the scheduler's queue grew past its bound"
        );

        // An event the scheduler acts on still arrives once it reads again, and
        // nothing on the way to it is telemetry.
        inner_tx
            .send(HashThreadEvent::ExpectedHashRate(
                crate::types::HashRate::from_terahashes(1.0),
            ))
            .await
            .unwrap();
        let mut status_seen = 0;
        let arrived = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match event_rx.recv().await {
                    Some(HashThreadEvent::ExpectedHashRate(_)) => return true,
                    Some(HashThreadEvent::TelemetryUpdate(_)) => {
                        panic!("telemetry reached the scheduler")
                    }
                    Some(_) => status_seen += 1,
                    None => return false,
                }
            }
        })
        .await;
        assert_eq!(arrived, Ok(true), "the rate report never arrived");
        let _ = status_seen;
    }
}
