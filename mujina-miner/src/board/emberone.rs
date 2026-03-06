//! EmberOne mining board support (stub).
//!
//! The EmberOne is a mining board with 12 BM1362 ASIC chips, communicating via
//! USB using the bitaxe-raw protocol (same as Bitaxe boards).
//!
//! This is currently a stub implementation pending full support.

use anyhow::{Result, bail};
use async_trait::async_trait;
use tokio::sync::watch;

use super::{
    Board, BoardDescriptor, BoardInfo,
    pattern::{BoardPattern, Match, StringMatch},
};
use crate::{
    api_client::types::BoardState, asic::hash_thread::HashThread, transport::UsbDeviceInfo,
};

/// EmberOne mining board (stub).
pub struct EmberOne {
    device_info: UsbDeviceInfo,

    /// Channel for publishing board state to the API server.
    #[expect(dead_code, reason = "will publish telemetry in a follow-up commit")]
    state_tx: watch::Sender<BoardState>,
}

impl EmberOne {
    /// Create a new EmberOne board instance.
    pub fn new(device_info: UsbDeviceInfo, state_tx: watch::Sender<BoardState>) -> Result<Self> {
        Ok(Self {
            device_info,
            state_tx,
        })
    }
}

#[async_trait]
impl Board for EmberOne {
    fn board_info(&self) -> BoardInfo {
        BoardInfo {
            model: "EmberOne".to_string(),
            firmware_version: None,
            serial_number: self.device_info.serial_number.clone(),
        }
    }

    async fn shutdown(&mut self) -> Result<()> {
        tracing::info!("EmberOne stub shutdown (no-op)");
        Ok(())
    }

    async fn create_hash_threads(&mut self) -> Result<Vec<Box<dyn HashThread>>> {
        bail!("EmberOne hash threads not yet implemented")
    }
}

// Factory function to create EmberOne board from USB device info
async fn create_from_usb(
    device: UsbDeviceInfo,
) -> Result<(Box<dyn Board + Send>, super::BoardRegistration)> {
    let serial = device.serial_number.clone();
    let initial_state = BoardState {
        name: format!("emberone-{}", serial.as_deref().unwrap_or("unknown")),
        model: "EmberOne".into(),
        serial,
        ..Default::default()
    };
    let (state_tx, state_rx) = watch::channel(initial_state);

    let board = EmberOne::new(device, state_tx)?;

    let registration = super::BoardRegistration {
        state_rx,
        command_tx: None,
    };
    Ok((Box::new(board), registration))
}

// Register this board type with the inventory system
inventory::submit! {
    BoardDescriptor {
        pattern: BoardPattern {
            vid: Match::Any,
            pid: Match::Any,
            manufacturer: Match::Specific(StringMatch::Exact("256F")),
            product: Match::Specific(StringMatch::Exact("EmberOne00")),
            serial_pattern: Match::Any,
        },
        name: "EmberOne",
        create_fn: |device| Box::pin(create_from_usb(device)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_board_creation() {
        let device = UsbDeviceInfo::new_for_test(
            0xc0de,
            0xcafe,
            Some("TEST001".to_string()),
            Some("EmberOne".to_string()),
            Some("Mining Board".to_string()),
            "/sys/devices/test".to_string(),
        );

        let (state_tx, _state_rx) = watch::channel(BoardState {
            name: format!(
                "emberone-{}",
                device.serial_number.as_deref().unwrap_or("unknown")
            ),
            model: "EmberOne".into(),
            serial: device.serial_number.clone(),
            ..Default::default()
        });

        let board = EmberOne::new(device, state_tx);
        assert!(board.is_ok());

        let board = board.unwrap();
        assert_eq!(board.board_info().model, "EmberOne");
    }
}
