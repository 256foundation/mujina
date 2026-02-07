//! emberOne/00 hash board support
//!
//! The emberOne/00 has 12 BM1362 ASIC chips, communicating via USB
//! using the bitaxe-raw protocol (same as Bitaxe boards).

use anyhow::{Result, bail};
use async_trait::async_trait;
use tokio::sync::watch;

use super::{
    Board, BoardDescriptor, BoardInfo,
    pattern::{BoardPattern, Match, StringMatch},
};
use crate::{
    api_client::types::BoardTelemetry, asic::hash_thread::HashThread, transport::UsbDeviceInfo,
};

// Register this board type with the inventory system
inventory::submit! {
    BoardDescriptor {
        pattern: BoardPattern {
            vid: Match::Any,
            pid: Match::Any,
            bcd_device: Match::Any,
            manufacturer: Match::Specific(StringMatch::Exact("256F")),
            product: Match::Specific(StringMatch::Exact("EmberOne00")),
            serial_pattern: Match::Any,
        },
        name: "emberOne/00",
        create_fn: |device| Box::pin(create_from_usb(device)),
    }
}

// Factory function to create an emberOne/00 board from USB device info
async fn create_from_usb(
    device: UsbDeviceInfo,
) -> Result<(Box<dyn Board + Send>, super::BoardRegistration)> {
    let serial = device.serial_number.clone();
    let initial_telemetry = BoardTelemetry {
        name: format!("emberone00-{}", serial.as_deref().unwrap_or("unknown")),
        model: "emberOne/00".into(),
        serial,
        ..Default::default()
    };
    let (telemetry_tx, telemetry_rx) = watch::channel(initial_telemetry);

    let board = EmberOne00::new(device, telemetry_tx);

    let registration = super::BoardRegistration { telemetry_rx };
    Ok((Box::new(board), registration))
}

/// emberOne/00 hash board
pub struct EmberOne00 {
    device_info: UsbDeviceInfo,

    /// Channel for publishing board telemetry to the API server.
    #[expect(dead_code, reason = "will publish telemetry in a follow-up commit")]
    telemetry_tx: watch::Sender<BoardTelemetry>,
}

#[async_trait]
impl Board for EmberOne00 {
    fn board_info(&self) -> BoardInfo {
        BoardInfo {
            model: "emberOne/00".to_string(),
            firmware_version: None,
            serial_number: self.device_info.serial_number.clone(),
        }
    }

    async fn shutdown(&mut self) -> Result<()> {
        Ok(())
    }

    async fn create_hash_threads(&mut self) -> Result<Vec<Box<dyn HashThread>>> {
        bail!("emberOne/00 hash threads not yet implemented")
    }
}

impl EmberOne00 {
    /// Create a new emberOne/00 board instance.
    pub fn new(device_info: UsbDeviceInfo, telemetry_tx: watch::Sender<BoardTelemetry>) -> Self {
        Self {
            device_info,
            telemetry_tx,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn board_info_propagates_serial_number() {
        let device = UsbDeviceInfo {
            serial_number: Some("S12345".to_string()),
            device_path: "/sys/devices/test".to_string(),
            ..Default::default()
        };

        let (telemetry_tx, _) = watch::channel(BoardTelemetry::default());
        let board = EmberOne00::new(device, telemetry_tx);

        assert_eq!(board.board_info().serial_number.as_deref(), Some("S12345"),);
    }
}
