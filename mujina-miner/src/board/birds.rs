//! BIRDS mining board support (stub).
//!
//! The BIRDS board is a mining board with 4 BZM2 ASIC chips, communicating via
//! USB using two serial ports: a control UART for GPIO/I2C and a data UART for
//! ASIC communication with 8-bit to 9-bit serial translation.
//!
//! This is currently a stub implementation pending full BZM2 ASIC support.

use async_trait::async_trait;

use super::{
    Board, BoardDescriptor, BoardError, BoardInfo,
    pattern::{BoardPattern, Match, StringMatch},
};
use crate::{asic::hash_thread::HashThread, error::Error, transport::UsbDeviceInfo};

/// Number of BZM2 ASICs on a BIRDS board.
#[expect(dead_code, reason = "will be used during ASIC init")]
const ASICS_PER_BOARD: usize = 4;

/// Default baud rate for the BIRDS data UART (5 Mbps).
#[expect(dead_code, reason = "will be used when opening data port")]
const DATA_UART_BAUD: u32 = 5_000_000;

/// Baud rate for the BIRDS control UART.
#[expect(dead_code, reason = "will be used when opening control port")]
const CONTROL_UART_BAUD: u32 = 115_200;

/// BIRDS mining board.
pub struct BirdsBoard {
    device_info: UsbDeviceInfo,
}

impl BirdsBoard {
    /// Create a new BIRDS board instance.
    pub fn new(device_info: UsbDeviceInfo) -> Result<Self, BoardError> {
        Ok(Self { device_info })
    }
}

#[async_trait]
impl Board for BirdsBoard {
    fn board_info(&self) -> BoardInfo {
        BoardInfo {
            model: "BIRDS".to_string(),
            firmware_version: None,
            serial_number: self.device_info.serial_number.clone(),
        }
    }

    async fn shutdown(&mut self) -> Result<(), BoardError> {
        tracing::info!("BIRDS stub shutdown (no-op)");
        Ok(())
    }

    async fn create_hash_threads(&mut self) -> Result<Vec<Box<dyn HashThread>>, BoardError> {
        Err(BoardError::InitializationFailed(
            "BIRDS not yet implemented".into(),
        ))
    }
}

// Factory function to create BIRDS board from USB device info
async fn create_from_usb(device: UsbDeviceInfo) -> crate::error::Result<Box<dyn Board + Send>> {
    let board = BirdsBoard::new(device)
        .map_err(|e| Error::Hardware(format!("Failed to create board: {}", e)))?;

    Ok(Box::new(board))
}

// Register this board type with the inventory system
inventory::submit! {
    BoardDescriptor {
        pattern: BoardPattern {
            vid: Match::Any,
            pid: Match::Any,
            manufacturer: Match::Specific(StringMatch::Exact("OSMU")),
            product: Match::Specific(StringMatch::Exact("BIRDS")),
            serial_pattern: Match::Any,
        },
        name: "BIRDS",
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
            Some("BIRDS".to_string()),
            Some("Mining Board".to_string()),
            "/sys/devices/test".to_string(),
        );

        let board = BirdsBoard::new(device);
        assert!(board.is_ok());

        let board = board.unwrap();
        assert_eq!(board.board_info().model, "BIRDS");
    }
}
