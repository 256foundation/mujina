//! BIRDS mining board support (stub).
//!
//! The BIRDS board is a mining board with 4 BZM2 ASIC chips, communicating via
//! USB using two serial ports: a control UART for GPIO/I2C and a data UART for
//! ASIC communication with 8-bit to 9-bit serial translation.
//!
//! This is currently a stub implementation pending full BZM2 ASIC support.

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{Duration, sleep};
use tokio_serial::SerialPortBuilderExt;

use super::{
    Board, BoardDescriptor, BoardError, BoardInfo,
    pattern::{BoardPattern, Match, StringMatch},
};
use crate::{
    asic::{bzm2::smoke, hash_thread::HashThread},
    error::Error,
    transport::UsbDeviceInfo,
};

/// Number of BZM2 ASICs on a BIRDS board.
const ASICS_PER_BOARD: usize = 4;

/// Default baud rate for the BIRDS data UART (5 Mbps).
const DATA_UART_BAUD: u32 = 5_000_000;

/// Baud rate for the BIRDS control UART.
const CONTROL_UART_BAUD: u32 = 115_200;

/// BIRDS control GPIO: 5V power enable.
const GPIO_5V_EN: u8 = 1;
/// BIRDS control GPIO: ASIC reset (active-low).
const GPIO_ASIC_RST: u8 = 2;
/// BIRDS control board ID for 5V/ASIC reset GPIO operations.
const CTRL_ID_POWER_RESET: u8 = 0xAB;
/// Control protocol page for GPIO.
const CTRL_PAGE_GPIO: u8 = 0x06;

/// BIRDS mining board.
pub struct BirdsBoard {
    device_info: UsbDeviceInfo,
}

impl BirdsBoard {
    /// Create a new BIRDS board instance.
    pub fn new(device_info: UsbDeviceInfo) -> Result<Self, BoardError> {
        Ok(Self { device_info })
    }

    /// Early bring-up init path.
    ///
    /// Until full thread integration lands, we run a basic UART smoke test
    /// (NOOP + READREG ASIC_ID) during board initialization.
    pub async fn initialize(&mut self) -> Result<(), BoardError> {
        let (control_port, data_port) = {
            let serial_ports = self.device_info.serial_ports().map_err(|e| {
                BoardError::InitializationFailed(format!("Failed to enumerate serial ports: {}", e))
            })?;

            if serial_ports.len() != 2 {
                return Err(BoardError::InitializationFailed(format!(
                    "BIRDS requires exactly 2 serial ports, found {}",
                    serial_ports.len()
                )));
            }

            (serial_ports[0].clone(), serial_ports[1].clone())
        };

        tracing::info!(
            serial = ?self.device_info.serial_number,
            control_port = %control_port,
            data_port = %data_port,
            data_baud = DATA_UART_BAUD,
            control_baud = CONTROL_UART_BAUD,
            asics = ASICS_PER_BOARD,
            "Running BIRDS ASIC smoke test during initialization"
        );

        // Match known-good bring-up sequence from reference scripts:
        // 1) Enable 5V rail
        // 2) Pulse ASIC reset low/high
        // 3) Wait for UART startup
        self.bringup_power_and_reset(&control_port).await?;

        let result = smoke::run_smoke(&data_port, 0).await.map_err(|e| {
            BoardError::InitializationFailed(format!("BIRDS ASIC smoke test failed: {:#}", e))
        })?;

        tracing::info!(
            logical_asic = result.logical_asic,
            asic_hw_id = result.asic_hw_id,
            asic_id = format_args!("0x{:08x}", result.asic_id),
            "BIRDS ASIC smoke test succeeded"
        );

        Ok(())
    }

    async fn bringup_power_and_reset(&self, control_port: &str) -> Result<(), BoardError> {
        let mut control_stream = tokio_serial::new(control_port, CONTROL_UART_BAUD)
            .open_native_async()
            .map_err(|e| {
                BoardError::InitializationFailed(format!(
                    "Failed to open BIRDS control port {}: {}",
                    control_port, e
                ))
            })?;

        Self::control_gpio_write(&mut control_stream, GPIO_5V_EN, true).await?;
        sleep(Duration::from_millis(100)).await;

        Self::control_gpio_write(&mut control_stream, GPIO_ASIC_RST, false).await?;
        sleep(Duration::from_millis(100)).await;

        Self::control_gpio_write(&mut control_stream, GPIO_ASIC_RST, true).await?;
        sleep(Duration::from_millis(1000)).await;

        Ok(())
    }

    async fn control_gpio_write(
        stream: &mut tokio_serial::SerialStream,
        pin: u8,
        value_high: bool,
    ) -> Result<(), BoardError> {
        // Packet format: [len:u16_le][id][bus][page][cmd=pin][value]
        // For BIRDS, id is the board target (0xAB for 5V/RST).
        let packet: [u8; 7] = [
            0x07,
            0x00,
            CTRL_ID_POWER_RESET,
            0x00,
            CTRL_PAGE_GPIO,
            pin,
            if value_high { 0x01 } else { 0x00 },
        ];
        stream.write_all(&packet).await.map_err(|e| {
            BoardError::HardwareControl(format!(
                "Failed to write GPIO control packet (pin {}): {}",
                pin, e
            ))
        })?;

        // Ack is 4 bytes. Byte[2] should echo board id.
        let mut ack = [0u8; 4];
        stream.read_exact(&mut ack).await.map_err(|e| {
            BoardError::HardwareControl(format!(
                "Failed to read GPIO control ack (pin {}): {}",
                pin, e
            ))
        })?;
        if ack[2] != CTRL_ID_POWER_RESET {
            return Err(BoardError::HardwareControl(format!(
                "GPIO ack ID mismatch for pin {}: expected 0x{:02x}, got 0x{:02x}",
                pin, CTRL_ID_POWER_RESET, ack[2]
            )));
        }

        Ok(())
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
    let mut board = BirdsBoard::new(device)
        .map_err(|e| Error::Hardware(format!("Failed to create board: {}", e)))?;

    board
        .initialize()
        .await
        .map_err(|e| Error::Hardware(format!("Failed to initialize BIRDS board: {}", e)))?;

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
