//! Backplane for board communication and lifecycle management.
//!
//! The Backplane acts as the communication substrate between mining boards and
//! the scheduler. Like a hardware backplane, it provides connection points for
//! boards to plug into, routes events between components, and manages board
//! lifecycle (hotplug, emergency shutdown, etc.).

use crate::{
    board::{Board, BoardDescriptor},
    error::Result,
    hash_thread::HashThread,
    tracing::prelude::*,
    transport::{usb::TransportEvent as UsbTransportEvent, TransportEvent, UsbDeviceInfo},
};
use std::collections::HashMap;
use tokio::sync::mpsc;

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

    /// Create a board from USB device info.
    pub async fn create_board(&self, device: UsbDeviceInfo) -> Result<Box<dyn Board + Send>> {
        let desc = self.find_descriptor(&device).ok_or_else(|| {
            let mfr = device
                .manufacturer
                .as_ref()
                .map(|s| s.as_str())
                .unwrap_or("None");
            let prod = device
                .product
                .as_ref()
                .map(|s| s.as_str())
                .unwrap_or("None");
            crate::error::Error::Other(format!(
                "No board registered for VID={:04x} PID={:04x} Manufacturer={} Product={}",
                device.vid, device.pid, mfr, prod
            ))
        })?;

        tracing::debug!(
            board = desc.name,
            specificity = desc.pattern.specificity(),
            "Pattern matched, creating board"
        );
        (desc.create_fn)(device).await
    }
}

/// Backplane that connects boards to the scheduler.
///
/// Acts as the communication substrate between mining boards and the work
/// scheduler. Boards plug into the backplane, which routes their events and
/// manages their lifecycle.
pub struct Backplane {
    registry: BoardRegistry,
    /// Active boards managed by the backplane
    boards: HashMap<String, Box<dyn Board + Send>>,
    event_rx: mpsc::Receiver<TransportEvent>,
    /// Channel to send hash threads to the scheduler
    scheduler_tx: mpsc::Sender<Vec<Box<dyn HashThread>>>,
}

impl Backplane {
    /// Create a new backplane.
    pub fn new(
        event_rx: mpsc::Receiver<TransportEvent>,
        scheduler_tx: mpsc::Sender<Vec<Box<dyn HashThread>>>,
    ) -> Self {
        Self {
            registry: BoardRegistry,
            boards: HashMap::new(),
            event_rx,
            scheduler_tx,
        }
    }

    /// Run the backplane event loop.
    pub async fn run(&mut self) -> Result<()> {
        while let Some(event) = self.event_rx.recv().await {
            match event {
                TransportEvent::Usb(usb_event) => {
                    self.handle_usb_event(usb_event).await?;
                } // Future: handle other transport types
            }
        }

        Ok(())
    }

    /// Handle USB transport events.
    async fn handle_usb_event(&mut self, event: UsbTransportEvent) -> Result<()> {
        match event {
            UsbTransportEvent::UsbDeviceConnected(device_info) => {
                let vid = device_info.vid;
                let pid = device_info.pid;

                // Try to create a board from this USB device
                match self.registry.create_board(device_info).await {
                    Ok(mut board) => {
                        let board_info = board.board_info();
                        let board_id = board_info
                            .serial_number
                            .clone()
                            .unwrap_or_else(|| "unknown".to_string());

                        debug!(
                            board = %board_info.model,
                            serial = %board_id,
                            "Board created, initializing hash threads"
                        );

                        // Create hash threads from the board
                        match board.create_hash_threads().await {
                            Ok(threads) => {
                                let thread_count = threads.len();

                                // Store board for lifecycle management
                                self.boards.insert(board_id.clone(), board);

                                // Send threads to scheduler
                                if let Err(e) = self.scheduler_tx.send(threads).await {
                                    tracing::error!(
                                        board = %board_info.model,
                                        error = %e,
                                        "Failed to send threads to scheduler"
                                    );
                                } else {
                                    // Single consolidated info message - board is ready
                                    info!(
                                        board = %board_info.model,
                                        serial = %board_id,
                                        threads = thread_count,
                                        vid = %format!("{:04x}", vid),
                                        pid = %format!("{:04x}", pid),
                                        "Board ready"
                                    );
                                }
                            }
                            Err(e) => {
                                tracing::error!(
                                    board = %board_info.model,
                                    serial = %board_id,
                                    error = %e,
                                    "Failed to create hash threads"
                                );
                            }
                        }
                    }
                    Err(e) => {
                        trace!(
                            vid = %format!("{:04x}", vid),
                            pid = %format!("{:04x}", pid),
                            error = %e,
                            "No board match for USB device"
                        );
                    }
                }
            }
            UsbTransportEvent::UsbDeviceDisconnected { device_path } => {
                debug!(device_path = %device_path, "USB device disconnected");

                // Find and shutdown the board
                // Note: Current design uses serial number as key, but we get device_path
                // in disconnect event. For single-board setups this works fine.
                // TODO: Maintain device_path -> board_id mapping for multi-board support
                let board_ids: Vec<String> = self.boards.keys().cloned().collect();
                for board_id in board_ids {
                    if let Some(mut board) = self.boards.remove(&board_id) {
                        let model = board.board_info().model;
                        debug!(board = %model, serial = %board_id, "Shutting down board");

                        match board.shutdown().await {
                            Ok(()) => {
                                info!(
                                    board = %model,
                                    serial = %board_id,
                                    "Board disconnected"
                                );
                            }
                            Err(e) => {
                                tracing::error!(
                                    board = %model,
                                    serial = %board_id,
                                    error = %e,
                                    "Failed to shutdown board"
                                );
                            }
                        }
                        // Don't re-insert - board is removed
                        break; // For now, assume one board per device
                    }
                }
            }
        }

        Ok(())
    }
}
