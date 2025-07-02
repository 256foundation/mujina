//! Board lifecycle and hotplug management.
//!
//! This module manages the discovery, creation, and lifecycle of mining
//! boards. It listens for transport events, looks up board types in the
//! registry, and creates appropriate board instances.

use crate::board::{Board, BoardDescriptor};
use crate::error::Result;
use crate::transport::{TransportEvent, UsbDeviceInfo};
use std::collections::HashMap;
use tokio::sync::mpsc;

/// Board registry that uses inventory to find registered boards.
pub struct BoardRegistry;

impl BoardRegistry {
    /// Find a board descriptor that can handle this USB device.
    pub fn find_descriptor(&self, vid: u16, pid: u16) -> Option<&'static BoardDescriptor> {
        inventory::iter::<BoardDescriptor>().find(|desc| desc.vid == vid && desc.pid == pid)
    }

    /// Create a board from USB device info.
    pub async fn create_board(&self, device: UsbDeviceInfo) -> Result<Box<dyn Board + Send>> {
        let desc = self
            .find_descriptor(device.vid, device.pid)
            .ok_or_else(|| {
                crate::error::Error::Other(format!(
                    "No board registered for {:04x}:{:04x}",
                    device.vid, device.pid
                ))
            })?;

        tracing::info!("Creating {} board from USB device", desc.name);
        (desc.create_fn)(device).await
    }
}

/// Board manager that handles board lifecycle.
pub struct BoardManager {
    registry: BoardRegistry,
    boards: HashMap<String, Box<dyn Board + Send>>,
    event_rx: mpsc::Receiver<TransportEvent>,
    /// Channel to send initialized boards to the scheduler
    scheduler_tx: mpsc::Sender<Box<dyn Board + Send>>,
}

impl BoardManager {
    /// Create a new board manager.
    pub fn new(
        event_rx: mpsc::Receiver<TransportEvent>,
        scheduler_tx: mpsc::Sender<Box<dyn Board + Send>>,
    ) -> Self {
        Self {
            registry: BoardRegistry,
            boards: HashMap::new(),
            event_rx,
            scheduler_tx,
        }
    }

    /// Run the board manager event loop.
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
    async fn handle_usb_event(
        &mut self,
        event: crate::transport::usb::TransportEvent,
    ) -> Result<()> {
        use crate::transport::usb::TransportEvent;

        match event {
            TransportEvent::UsbDeviceConnected(device_info) => {
                let vid = device_info.vid;
                let pid = device_info.pid;
                tracing::info!("USB device connected: {:04x}:{:04x}", vid, pid);

                // Try to create a board from this USB device
                match self.registry.create_board(device_info).await {
                    Ok(board) => {
                        let board_info = board.board_info();
                        let board_id = board_info
                            .serial_number
                            .clone()
                            .unwrap_or_else(|| "unknown".to_string());

                        tracing::info!("Created {} board (serial: {})", board_info.model, board_id);

                        // Send to scheduler
                        if let Err(e) = self.scheduler_tx.send(board).await {
                            tracing::error!("Failed to send board to scheduler: {}", e);
                        } else {
                            tracing::info!("Board {} sent to scheduler", board_id);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to create board for device {:04x}:{:04x}: {}",
                            vid,
                            pid,
                            e
                        );
                    }
                }
            }
            TransportEvent::UsbDeviceDisconnected { device_path } => {
                tracing::info!("USB device disconnected: {}", device_path);
                // TODO: Remove board from active boards and notify scheduler
            }
        }

        Ok(())
    }
}
