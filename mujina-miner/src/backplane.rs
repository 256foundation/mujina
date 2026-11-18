//! Backplane for board communication and lifecycle management.
//!
//! The Backplane acts as the communication substrate between mining boards and
//! the scheduler. Like a hardware backplane, it provides connection points for
//! boards to plug into, routes events between components, and manages board
//! lifecycle (hotplug, emergency shutdown, etc.).

use anyhow::Result;
use std::collections::{HashMap, HashSet};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{StreamExt, StreamMap};
use tokio_util::sync::CancellationToken;

use futures::future::BoxFuture;

use crate::{
    api::BoardRegistration,
    board::{BackplaneConnector, BoardDescriptor, BoardInfo, VirtualBoardRegistry},
    scheduler::ThreadRegistration,
    tracing::prelude::*,
    transport::{Device, DeviceKey, TransportEvent, UsbDeviceInfo},
};

/// Board registry that uses inventory to find registered boards.
pub struct BoardRegistry;

impl BoardRegistry {
    /// Find the best matching board descriptor for this USB device.
    ///
    /// Uses pattern matching with specificity scoring to select the most
    /// appropriate board handler. When multiple patterns match, the one
    /// with the highest specificity score wins.
    ///
    /// Returns None if no registered boards match the device.
    pub fn find_descriptor(&self, device: &UsbDeviceInfo) -> Option<&'static BoardDescriptor> {
        inventory::iter::<BoardDescriptor>()
            .filter(|desc| desc.pattern.matches(device))
            .max_by_key(|desc| desc.pattern.specificity())
    }
}

/// Backplane that connects boards to the scheduler.
///
/// Acts as the communication substrate between mining boards and the work
/// scheduler. Boards plug into the backplane, which routes their events and
/// manages their lifecycle.
pub struct Backplane {
    registry: BoardRegistry,
    virtual_registry: VirtualBoardRegistry,
    /// Active boards, each under the key of the device it runs on
    boards: HashMap<DeviceKey, ActiveBoard>,
    /// One event receiver per transport; the count sets how many initial
    /// enumeration completions to wait for.
    event_rxs: Vec<mpsc::Receiver<TransportEvent>>,
    /// Channel to register hash threads with the scheduler
    scheduler_tx: mpsc::Sender<ThreadRegistration>,
    /// Channel to forward board registrations to the API server
    board_reg_tx: mpsc::Sender<BoardRegistration>,
}

impl Backplane {
    /// Create a new backplane.
    pub fn new(
        event_rxs: Vec<mpsc::Receiver<TransportEvent>>,
        scheduler_tx: mpsc::Sender<ThreadRegistration>,
        board_reg_tx: mpsc::Sender<BoardRegistration>,
    ) -> Self {
        Self {
            registry: BoardRegistry,
            virtual_registry: VirtualBoardRegistry,
            boards: HashMap::new(),
            event_rxs,
            scheduler_tx,
            board_reg_tx,
        }
    }

    /// Runs the backplane until shutdown is requested, then stops all
    /// boards.
    pub async fn run(mut self, shutdown: CancellationToken) {
        tokio::select! {
            _ = self.route_events() => {}
            _ = shutdown.cancelled() => {}
        }

        // Boards outlive their transports: the CPU miner's closes as
        // soon as its board is up. Stop them only on shutdown.
        shutdown.cancelled().await;
        self.shutdown_all_boards().await;
    }

    /// Routes transport events until every transport closes. An
    /// event that fails is logged, and routing goes on to the next.
    async fn route_events(&mut self) {
        // Multiplex the per-transport receivers. The number of transports is
        // how many initial-enumeration completions to wait for.
        let mut streams: StreamMap<usize, ReceiverStream<TransportEvent>> = StreamMap::new();
        for (i, rx) in std::mem::take(&mut self.event_rxs).into_iter().enumerate() {
            streams.insert(i, ReceiverStream::new(rx));
        }
        let transport_count = streams.len();
        let mut completed: HashSet<usize> = HashSet::new();
        let mut completion_sent = false;

        // No transports means nothing to enumerate; let the scheduler proceed.
        if transport_count == 0 {
            self.send_enumeration_complete().await;
            completion_sent = true;
        }

        while let Some((transport, event)) = streams.next().await {
            match event {
                TransportEvent::Connected(device) => {
                    if let Err(e) = self.handle_connected(device).await {
                        error!(error = %e, "Failed to handle a connected device");
                    }
                }
                TransportEvent::Disconnected(key) => {
                    if let Err(e) = self.handle_disconnected(key).await {
                        error!(error = %e, "Failed to handle a disconnected device");
                    }
                }
                TransportEvent::InitialEnumerationComplete => {
                    completed.insert(transport);
                    if !completion_sent && completed.len() == transport_count {
                        self.send_enumeration_complete().await;
                        completion_sent = true;
                    }
                }
            }
        }
    }

    /// Shutdown all boards managed by this backplane.
    async fn shutdown_all_boards(&mut self) {
        let keys: Vec<DeviceKey> = self.boards.keys().cloned().collect();

        for key in keys {
            if let Some(mut board) = self.boards.remove(&key) {
                board.shutdown().await;
                info!(
                    board = %board.info.model,
                    serial = %board.serial(),
                    "Board stopped"
                );
            }
        }
    }

    /// Routes a board connection's parts to where they belong.
    ///
    /// The board is kept under the key of the device it runs on. A
    /// board already under that key is stopped first.
    async fn start_board(&mut self, key: DeviceKey, conn: BackplaneConnector) {
        if let Some(mut replaced) = self.boards.remove(&key) {
            warn!(
                board = %replaced.info.model,
                serial = %replaced.serial(),
                "Board replaced by another on the same device"
            );
            replaced.shutdown().await;
        }

        let BackplaneConnector {
            info,
            threads,
            telemetry_rx,
            shutdown,
        } = conn;

        let registration = BoardRegistration { telemetry_rx };
        if let Err(e) = self.board_reg_tx.send(registration).await {
            error!(
                board = %info.model,
                error = %e,
                "Failed to register board with API server"
            );
        }

        let board = ActiveBoard { info, shutdown };
        info!(
            board = %board.info.model,
            serial = %board.serial(),
            threads = threads.len(),
            "Board started."
        );

        for thread in threads {
            if let Err(e) = self
                .scheduler_tx
                .send(ThreadRegistration::Thread(thread))
                .await
            {
                error!(
                    board = %board.info.model,
                    error = %e,
                    "Failed to send thread to scheduler"
                );
                break;
            }
        }

        self.boards.insert(key, board);
    }

    /// Tell the scheduler that startup enumeration across all transports is
    /// done. Sent on the thread channel, after every starting thread, so FIFO
    /// ordering guarantees the scheduler has registered them all first.
    async fn send_enumeration_complete(&mut self) {
        if self
            .scheduler_tx
            .send(ThreadRegistration::InitialEnumerationComplete)
            .await
            .is_err()
        {
            debug!("Scheduler dropped before initial enumeration completed");
        }
    }

    /// Starts a board for a connected device, if the device is one.
    async fn handle_connected(&mut self, device: Device) -> Result<()> {
        let key = device.key();

        match device {
            Device::Usb(device_info) => {
                let Some(descriptor) = self.registry.find_descriptor(&device_info) else {
                    return Ok(());
                };

                info!(
                    board = descriptor.name,
                    vid = %format!("{:04x}", device_info.vid),
                    pid = %format!("{:04x}", device_info.pid),
                    manufacturer = ?device_info.manufacturer,
                    product = ?device_info.product,
                    serial = ?device_info.serial_number,
                    "Hash board connected via USB."
                );

                let conn = match (descriptor.create_fn)(device_info).await {
                    Ok(conn) => conn,
                    Err(e) => {
                        error!(
                            board = descriptor.name,
                            error = %e,
                            "Failed to create board"
                        );
                        return Ok(());
                    }
                };

                self.start_board(key, conn).await;
            }
            Device::Cpu(device_info) => {
                let Some(descriptor) = self.virtual_registry.find("cpu_miner") else {
                    error!("No virtual board descriptor found for cpu_miner");
                    return Ok(());
                };

                info!(
                    board = descriptor.name,
                    threads = device_info.thread_count,
                    duty = device_info.duty_percent,
                    "CPU miner board connected."
                );

                let conn = match (descriptor.create_fn)().await {
                    Ok(conn) => conn,
                    Err(e) => {
                        error!(
                            board = descriptor.name,
                            error = %e,
                            "Failed to create CPU miner board"
                        );
                        return Ok(());
                    }
                };

                self.start_board(key, conn).await;
            }
        }

        Ok(())
    }

    /// Stops the board on a disconnected device.
    ///
    /// Most devices that a transport reports are not boards, and their
    /// removal changes nothing here.
    async fn handle_disconnected(&mut self, key: DeviceKey) -> Result<()> {
        if let Some(mut board) = self.boards.remove(&key) {
            board.shutdown().await;
            info!(
                board = %board.info.model,
                serial = %board.serial(),
                "Board disconnected"
            );
        }

        Ok(())
    }
}

/// Per-board state the backplane keeps for lifecycle management.
struct ActiveBoard {
    info: BoardInfo,
    shutdown: Option<BoxFuture<'static, ()>>,
}

impl ActiveBoard {
    /// Returns the board's serial number for a log entry.
    fn serial(&self) -> &str {
        self.info.serial_number.as_deref().unwrap_or("unknown")
    }

    async fn shutdown(&mut self) {
        if let Some(fut) = self.shutdown.take() {
            fut.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use tokio::{sync::watch, time};

    use super::*;
    use crate::api_client::types::BoardTelemetry;
    use crate::transport::{cpu, usb::DevicePath};

    /// A board with no threads that sets `stopped` when the
    /// backplane stops it.
    fn test_board(stopped: &Arc<AtomicBool>) -> BackplaneConnector {
        let (_telemetry_tx, telemetry_rx) = watch::channel(BoardTelemetry::default());
        BackplaneConnector {
            info: BoardInfo {
                model: "Test Board".into(),
                firmware_version: None,
                serial_number: None,
            },
            threads: Vec::new(),
            telemetry_rx,
            shutdown: Some(Box::pin({
                let stopped = stopped.clone();
                async move { stopped.store(true, Ordering::SeqCst) }
            })),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn boards_run_until_shutdown_after_transports_close() {
        // A transport that closes once its board is up, as the CPU
        // miner's does.
        let (transport_tx, transport_rx) = mpsc::channel(1);
        drop(transport_tx);
        let (scheduler_tx, _scheduler_rx) = mpsc::channel(10);
        let (board_reg_tx, _board_reg_rx) = mpsc::channel(10);
        let mut backplane = Backplane::new(vec![transport_rx], scheduler_tx, board_reg_tx);

        let stopped = Arc::new(AtomicBool::new(false));
        let key = DeviceKey::Cpu(cpu::DeviceId::new("test-board"));
        backplane.start_board(key, test_board(&stopped)).await;

        let shutdown = CancellationToken::new();
        let mut run = tokio::spawn(backplane.run(shutdown.clone()));

        // Time is paused, so the runtime skips straight to the timeout
        // once the backplane goes idle.
        let finished_early = time::timeout(Duration::from_secs(3600), &mut run)
            .await
            .is_ok();
        assert!(!finished_early, "backplane exited before shutdown");
        assert!(
            !stopped.load(Ordering::SeqCst),
            "board stopped before shutdown"
        );

        shutdown.cancel();
        run.await.unwrap();
        assert!(
            stopped.load(Ordering::SeqCst),
            "board not stopped on shutdown"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn unrelated_usb_removal_leaves_boards_running() {
        let (transport_tx, transport_rx) = mpsc::channel(1);
        let (scheduler_tx, _scheduler_rx) = mpsc::channel(10);
        let (board_reg_tx, _board_reg_rx) = mpsc::channel(10);
        let mut backplane = Backplane::new(vec![transport_rx], scheduler_tx, board_reg_tx);

        let stopped = Arc::new(AtomicBool::new(false));
        let key = DeviceKey::Usb(DevicePath::new("/sys/devices/test-board"));
        backplane.start_board(key, test_board(&stopped)).await;

        let shutdown = CancellationToken::new();
        let run = tokio::spawn(backplane.run(shutdown.clone()));

        // Some other USB device, such as a keyboard, is unplugged.
        let removed = DeviceKey::Usb(DevicePath::new("/sys/devices/some-other-device"));
        transport_tx
            .send(TransportEvent::Disconnected(removed))
            .await
            .unwrap();

        // The clock is paused, so this returns once the backplane
        // has handled the event and gone idle.
        time::sleep(Duration::from_secs(1)).await;
        assert!(
            !stopped.load(Ordering::SeqCst),
            "board stopped by an unrelated removal"
        );

        shutdown.cancel();
        run.await.unwrap();
        assert!(
            stopped.load(Ordering::SeqCst),
            "board not stopped on shutdown"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn usb_removal_stops_only_the_board_on_that_device() {
        let (transport_tx, transport_rx) = mpsc::channel(1);
        let (scheduler_tx, _scheduler_rx) = mpsc::channel(10);
        let (board_reg_tx, _board_reg_rx) = mpsc::channel(10);
        let mut backplane = Backplane::new(vec![transport_rx], scheduler_tx, board_reg_tx);

        let removed = Arc::new(AtomicBool::new(false));
        let removed_key = DeviceKey::Usb(DevicePath::new("/sys/devices/removed-board"));
        backplane
            .start_board(removed_key.clone(), test_board(&removed))
            .await;

        let kept = Arc::new(AtomicBool::new(false));
        let kept_key = DeviceKey::Usb(DevicePath::new("/sys/devices/kept-board"));
        backplane.start_board(kept_key, test_board(&kept)).await;

        let shutdown = CancellationToken::new();
        let run = tokio::spawn(backplane.run(shutdown.clone()));

        transport_tx
            .send(TransportEvent::Disconnected(removed_key))
            .await
            .unwrap();

        // The clock is paused, so this returns once the backplane
        // has handled the event and gone idle.
        time::sleep(Duration::from_secs(1)).await;
        assert!(
            removed.load(Ordering::SeqCst),
            "board not stopped when its device was removed"
        );
        assert!(
            !kept.load(Ordering::SeqCst),
            "board stopped by another board's removal"
        );

        shutdown.cancel();
        run.await.unwrap();
        assert!(kept.load(Ordering::SeqCst), "board not stopped on shutdown");
    }

    #[tokio::test]
    async fn a_second_board_on_a_device_stops_the_first() {
        let (_transport_tx, transport_rx) = mpsc::channel(1);
        let (scheduler_tx, _scheduler_rx) = mpsc::channel(10);
        let (board_reg_tx, _board_reg_rx) = mpsc::channel(10);
        let mut backplane = Backplane::new(vec![transport_rx], scheduler_tx, board_reg_tx);

        let first = Arc::new(AtomicBool::new(false));
        let second = Arc::new(AtomicBool::new(false));
        let key = DeviceKey::Usb(DevicePath::new("/sys/devices/test-board"));
        backplane.start_board(key.clone(), test_board(&first)).await;
        backplane.start_board(key, test_board(&second)).await;

        assert!(first.load(Ordering::SeqCst), "replaced board not stopped");
        assert!(!second.load(Ordering::SeqCst), "new board stopped");
    }
}
