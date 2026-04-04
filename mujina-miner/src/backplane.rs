//! Backplane for board communication and lifecycle management.
//!
//! The Backplane acts as the communication substrate between mining boards and
//! the scheduler. Like a hardware backplane, it provides connection points for
//! boards to plug into, routes events between components, and manages board
//! lifecycle (hotplug, emergency shutdown, etc.).

use anyhow::Result;
use std::collections::HashMap;
use tokio::sync::mpsc;

use futures::future::BoxFuture;

use crate::{
    api::BoardRegistration,
    asic::hash_thread::HashThread,
    board::{BackplaneConnector, BoardDescriptor, BoardInfo, VirtualBoardRegistry},
    tracing::prelude::*,
    transport::{
        TransportEvent, UsbDeviceInfo, cpu::TransportEvent as CpuTransportEvent,
        usb::TransportEvent as UsbTransportEvent,
    },
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
    /// Active boards managed by the backplane
    boards: HashMap<String, ActiveBoard>,
    event_rx: mpsc::Receiver<TransportEvent>,
    /// Channel to send hash threads to the scheduler
    scheduler_tx: mpsc::Sender<Box<dyn HashThread>>,
    /// Channel to forward board registrations to the API server
    board_reg_tx: mpsc::Sender<BoardRegistration>,
}

impl Backplane {
    /// Create a new backplane.
    pub fn new(
        event_rx: mpsc::Receiver<TransportEvent>,
        scheduler_tx: mpsc::Sender<Box<dyn HashThread>>,
        board_reg_tx: mpsc::Sender<BoardRegistration>,
    ) -> Self {
        Self {
            registry: BoardRegistry,
            virtual_registry: VirtualBoardRegistry,
            boards: HashMap::new(),
            event_rx,
            scheduler_tx,
            board_reg_tx,
        }
    }

    /// Run the backplane event loop.
    pub async fn run(&mut self) -> Result<()> {
        while let Some(event) = self.event_rx.recv().await {
            match event {
                TransportEvent::Usb(usb_event) => {
                    self.handle_usb_event(usb_event).await?;
                }
                TransportEvent::Cpu(cpu_event) => {
                    self.handle_cpu_event(cpu_event).await?;
                }
            }
        }

        Ok(())
    }

    /// Shutdown all boards managed by this backplane.
    pub async fn shutdown_all_boards(&mut self) {
        let board_ids: Vec<String> = self.boards.keys().cloned().collect();

        for board_id in board_ids {
            if let Some(mut board) = self.boards.remove(&board_id) {
                board.shutdown().await;
                info!(
                    board = %board.info.model,
                    serial = %board_id,
                    "Board stopped"
                );
            }
        }
    }

    /// Route a board connection's parts to where they belong.
    async fn start_board(&mut self, board_id: String, conn: BackplaneConnector) {
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

        info!(
            board = %info.model,
            serial = %board_id,
            threads = threads.len(),
            "Board started."
        );

        for thread in threads {
            if let Err(e) = self.scheduler_tx.send(thread).await {
                error!(
                    board = %info.model,
                    error = %e,
                    "Failed to send thread to scheduler"
                );
                break;
            }
        }

        self.boards.insert(board_id, ActiveBoard { info, shutdown });
    }

    /// Handle USB transport events.
    async fn handle_usb_event(&mut self, event: UsbTransportEvent) -> Result<()> {
        match event {
            UsbTransportEvent::UsbDeviceConnected(device_info) => {
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

                let board_id = conn
                    .info
                    .serial_number
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string());

                self.start_board(board_id, conn).await;
            }
            UsbTransportEvent::UsbDeviceDisconnected { device_path: _ } => {
                // Find and shutdown the board
                // Note: Current design uses serial number as key, but we get device_path
                // in disconnect event. For single-board setups this works fine.
                // TODO: Maintain device_path -> board_id mapping for multi-board support
                let board_ids: Vec<String> = self.boards.keys().cloned().collect();
                for board_id in board_ids {
                    if let Some(mut board) = self.boards.remove(&board_id) {
                        board.shutdown().await;
                        info!(
                            board = %board.info.model,
                            serial = %board_id,
                            "Board disconnected"
                        );
                        break; // For now, assume one board per device
                    }
                }
            }
        }

        Ok(())
    }

    /// Handle CPU miner transport events.
    async fn handle_cpu_event(&mut self, event: CpuTransportEvent) -> Result<()> {
        match event {
            CpuTransportEvent::CpuDeviceConnected(device_info) => {
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

                let board_id = device_info.device_id.clone();
                self.start_board(board_id, conn).await;
            }
            CpuTransportEvent::CpuDeviceDisconnected { device_id } => {
                if let Some(mut board) = self.boards.remove(&device_id) {
                    board.shutdown().await;
                    info!(board = %board.info.model, serial = %device_id, "Board disconnected");
                }
            }
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
    async fn shutdown(&mut self) {
        if let Some(fut) = self.shutdown.take() {
            fut.await;
        }
    }
}
