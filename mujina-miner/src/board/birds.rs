//! BIRDS mining board support.
//!
//! The BIRDS board is a mining board with 4 BZM2 ASIC chips, communicating via
//! USB using two serial ports: a control UART for GPIO/I2C and a data UART for
//! ASIC communication with 8-bit to 9-bit serial translation.
//!
//! This module follows the same split of responsibilities as the BM13xx-backed
//! boards:
//! - the board owns USB discovery, power sequencing, and reset control
//! - the [`Bzm2Thread`] owns chip bring-up after the data path is handed off
//! - board-only protocol helpers stay local so they can be unit tested without
//!   requiring attached hardware

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;
use tokio::time::{Duration, sleep};
use tokio_serial::SerialPortBuilderExt;
use tokio_util::codec::{FramedRead, FramedWrite};

use super::{
    Board, BoardDescriptor, BoardError, BoardInfo,
    pattern::{BoardPattern, Match, StringMatch},
};
use crate::{
    api_client::types::BoardState,
    asic::{
        bzm2::{FrameCodec, HexBytes, init, thread::Bzm2Thread},
        hash_thread::{AsicEnable, BoardPeripherals, HashThread, ThreadRemovalSignal},
    },
    error::Error,
    transport::{
        UsbDeviceInfo,
        serial::{SerialControl, SerialReader, SerialWriter},
    },
};

/// Number of BZM2 ASICs on a BIRDS board.
const ASICS_PER_BOARD: usize = 4;

/// Default baud rate for the BIRDS data UART (5 Mbps).
const DATA_UART_BAUD: u32 = 5_000_000;

/// Baud rate for the BIRDS control UART.
const CONTROL_UART_BAUD: u32 = 115_200;

/// BIRDS control GPIO: 5V power enable.
const GPIO_5V_EN: u8 = 1;
/// BIRDS control GPIO: VR power enable.
const GPIO_VR_EN: u8 = 0;
/// BIRDS control GPIO: ASIC reset (active-low).
const GPIO_ASIC_RST: u8 = 2;
/// BIRDS control board ID for 5V/ASIC reset GPIO operations.
const CTRL_ID_POWER_RESET: u8 = 0xAB;
/// BIRDS control board ID for VR GPIO operations.
const CTRL_ID_VR: u8 = 0xAA;
/// Control protocol page for GPIO.
const CTRL_PAGE_GPIO: u8 = 0x06;

#[derive(Debug, Clone, PartialEq, Eq)]
struct BirdsPorts {
    control_port: String,
    data_port: String,
}

impl BirdsPorts {
    fn from_slice(serial_ports: &[String]) -> Result<Self, BoardError> {
        if serial_ports.len() != 2 {
            return Err(BoardError::InitializationFailed(format!(
                "BIRDS requires exactly 2 serial ports, found {}",
                serial_ports.len()
            )));
        }

        Ok(Self {
            control_port: serial_ports[0].clone(),
            data_port: serial_ports[1].clone(),
        })
    }

    fn from_device_info(device_info: &UsbDeviceInfo) -> Result<Self, BoardError> {
        let serial_ports = device_info.serial_ports().map_err(|e| {
            BoardError::InitializationFailed(format!("Failed to enumerate serial ports: {}", e))
        })?;
        Self::from_slice(serial_ports)
    }
}

fn build_gpio_write_packet(dev_id: u8, pin: u8, value_high: bool) -> [u8; 7] {
    [
        0x07,
        0x00,
        dev_id,
        0x00,
        CTRL_PAGE_GPIO,
        pin,
        if value_high { 0x01 } else { 0x00 },
    ]
}

fn validate_gpio_ack(dev_id: u8, pin: u8, ack: [u8; 4]) -> Result<(), BoardError> {
    if ack[2] != dev_id {
        return Err(BoardError::HardwareControl(format!(
            "GPIO ack ID mismatch for pin {}: expected 0x{:02x}, got 0x{:02x}",
            pin, dev_id, ack[2]
        )));
    }

    Ok(())
}

/// BIRDS mining board.
pub struct BirdsBoard {
    device_info: UsbDeviceInfo,
    state_tx: watch::Sender<BoardState>,
    control_port: Option<String>,
    data_reader: Option<FramedRead<SerialReader, FrameCodec>>,
    data_writer: Option<FramedWrite<SerialWriter, FrameCodec>>,
    data_control: Option<SerialControl>,
    thread_shutdown: Option<watch::Sender<ThreadRemovalSignal>>,
}

impl BirdsBoard {
    /// Create a new BIRDS board instance.
    pub fn new(device_info: UsbDeviceInfo) -> Result<Self, BoardError> {
        let serial = device_info.serial_number.clone();
        let initial_state = BoardState {
            name: format!("birds-{}", serial.as_deref().unwrap_or("unknown")),
            model: "BIRDS".into(),
            serial,
            ..Default::default()
        };
        let (state_tx, _) = watch::channel(initial_state);

        Ok(Self {
            device_info,
            state_tx,
            control_port: None,
            data_reader: None,
            data_writer: None,
            data_control: None,
            thread_shutdown: None,
        })
    }

    /// Early bring-up init path.
    ///
    /// During board initialization we verify that control sequencing works and
    /// that at least one ASIC answers protocol-level initialization traffic
    /// before exposing the data channel to a hashing thread.
    pub async fn initialize(&mut self) -> Result<(), BoardError> {
        let BirdsPorts {
            control_port,
            data_port,
        } = BirdsPorts::from_device_info(&self.device_info)?;

        tracing::info!(
            serial = ?self.device_info.serial_number,
            control_port = %control_port,
            data_port = %data_port,
            data_baud = DATA_UART_BAUD,
            control_baud = CONTROL_UART_BAUD,
            asics = ASICS_PER_BOARD,
            "Running BIRDS ASIC data-port initialization"
        );

        // Match known-good bring-up sequence from birds_asyncio.py:
        // 1) VR off and settle
        // 2) Enable 5V rail
        // 3) Enable VR
        // 4) Pulse ASIC reset low/high
        // 5) Wait for UART startup
        self.bringup_power_and_reset(&control_port).await?;
        self.control_port = Some(control_port);

        let initialized_data_port =
            init::initialize_data_port(&data_port, 0)
                .await
                .map_err(|e| {
                    BoardError::InitializationFailed(format!(
                        "BIRDS ASIC data-port initialization failed: {:#}",
                        e
                    ))
                })?;
        let result = initialized_data_port.probe;

        tracing::info!(
            logical_asic = result.logical_asic,
            asic_hw_id = result.asic_hw_id,
            asic_id = format_args!("0x{:08x}", result.asic_id),
            "BIRDS ASIC data-port initialization succeeded"
        );

        self.data_reader = Some(initialized_data_port.reader);
        self.data_writer = Some(initialized_data_port.writer);
        self.data_control = Some(initialized_data_port.control);

        Ok(())
    }

    fn open_control_stream(control_port: &str) -> Result<tokio_serial::SerialStream, BoardError> {
        tokio_serial::new(control_port, CONTROL_UART_BAUD)
            .open_native_async()
            .map_err(|e| {
                BoardError::InitializationFailed(format!(
                    "Failed to open BIRDS control port {}: {}",
                    control_port, e
                ))
            })
    }

    async fn set_asic_reset(control_port: &str, value_high: bool) -> Result<(), BoardError> {
        let mut control_stream = Self::open_control_stream(control_port)?;
        Self::control_gpio_write(
            &mut control_stream,
            CTRL_ID_POWER_RESET,
            GPIO_ASIC_RST,
            value_high,
        )
        .await
    }

    fn thread_name_for_serial(serial_number: Option<&str>) -> String {
        match serial_number {
            Some(serial) => format!("BIRDS-{}", &serial[..8.min(serial.len())]),
            None => "BIRDS".to_string(),
        }
    }

    async fn bringup_power_and_reset(&self, control_port: &str) -> Result<(), BoardError> {
        let mut control_stream = Self::open_control_stream(control_port)?;

        Self::control_gpio_write(&mut control_stream, CTRL_ID_VR, GPIO_VR_EN, false).await?;
        sleep(Duration::from_millis(2000)).await;

        Self::control_gpio_write(&mut control_stream, CTRL_ID_POWER_RESET, GPIO_5V_EN, true)
            .await?;
        sleep(Duration::from_millis(100)).await;

        Self::control_gpio_write(&mut control_stream, CTRL_ID_VR, GPIO_VR_EN, true).await?;
        sleep(Duration::from_millis(100)).await;

        Self::control_gpio_write(
            &mut control_stream,
            CTRL_ID_POWER_RESET,
            GPIO_ASIC_RST,
            false,
        )
        .await?;
        sleep(Duration::from_millis(100)).await;

        Self::control_gpio_write(
            &mut control_stream,
            CTRL_ID_POWER_RESET,
            GPIO_ASIC_RST,
            true,
        )
        .await?;
        sleep(Duration::from_millis(1000)).await;

        Ok(())
    }

    async fn control_gpio_write(
        stream: &mut tokio_serial::SerialStream,
        dev_id: u8,
        pin: u8,
        value_high: bool,
    ) -> Result<(), BoardError> {
        // Packet format: [len:u16_le][id][bus][page][cmd=pin][value].
        let packet = build_gpio_write_packet(dev_id, pin, value_high);
        tracing::debug!(
            dev_id = format_args!("0x{:02X}", dev_id),
            pin,
            value = if value_high { 1 } else { 0 },
            tx = %HexBytes(&packet),
            "BIRDS ctrl gpio tx"
        );
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
        tracing::debug!(
            dev_id = format_args!("0x{:02X}", dev_id),
            pin,
            rx = %HexBytes(&ack),
            "BIRDS ctrl gpio rx"
        );
        validate_gpio_ack(dev_id, pin, ack)
    }

    async fn hold_in_reset(&self) -> Result<(), BoardError> {
        let control_port = self.control_port.as_ref().ok_or_else(|| {
            BoardError::InitializationFailed("BIRDS control port not initialized".into())
        })?;

        Self::set_asic_reset(control_port, false).await
    }
}

struct BirdsAsicEnable {
    control_port: String,
}

#[async_trait]
impl AsicEnable for BirdsAsicEnable {
    async fn enable(&mut self) -> anyhow::Result<()> {
        BirdsBoard::set_asic_reset(&self.control_port, true)
            .await
            .map_err(|e| anyhow::anyhow!("failed to release BZM2 reset: {}", e))
    }

    async fn disable(&mut self) -> anyhow::Result<()> {
        BirdsBoard::set_asic_reset(&self.control_port, false)
            .await
            .map_err(|e| anyhow::anyhow!("failed to assert BZM2 reset: {}", e))
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
        if let Some(ref tx) = self.thread_shutdown
            && let Err(e) = tx.send(ThreadRemovalSignal::Shutdown)
        {
            tracing::warn!("Failed to send shutdown signal to BIRDS thread: {}", e);
        }

        self.hold_in_reset().await?;
        Ok(())
    }

    async fn create_hash_threads(&mut self) -> Result<Vec<Box<dyn HashThread>>, BoardError> {
        let (removal_tx, removal_rx) = watch::channel(ThreadRemovalSignal::Running);
        self.thread_shutdown = Some(removal_tx);

        let data_reader = self
            .data_reader
            .take()
            .ok_or(BoardError::InitializationFailed(
                "No BIRDS data reader available".into(),
            ))?;
        let data_writer = self
            .data_writer
            .take()
            .ok_or(BoardError::InitializationFailed(
                "No BIRDS data writer available".into(),
            ))?;

        let control_port = self
            .control_port
            .clone()
            .ok_or(BoardError::InitializationFailed(
                "No BIRDS control port available".into(),
            ))?;
        let asic_enable = BirdsAsicEnable { control_port };
        let peripherals = BoardPeripherals {
            asic_enable: Some(Box::new(asic_enable)),
            voltage_regulator: None,
        };

        let thread_name = Self::thread_name_for_serial(self.device_info.serial_number.as_deref());

        let thread = Bzm2Thread::new(
            thread_name,
            data_reader,
            data_writer,
            peripherals,
            removal_rx,
            ASICS_PER_BOARD as u8,
        );
        Ok(vec![Box::new(thread)])
    }
}

// Factory function to create BIRDS board from USB device info
async fn create_from_usb(
    device: UsbDeviceInfo,
) -> crate::error::Result<(Box<dyn Board + Send>, super::BoardRegistration)> {
    let mut board = BirdsBoard::new(device)
        .map_err(|e| Error::Hardware(format!("Failed to create board: {}", e)))?;

    board
        .initialize()
        .await
        .map_err(|e| Error::Hardware(format!("Failed to initialize BIRDS board: {}", e)))?;

    let registration = super::BoardRegistration {
        state_rx: board.state_tx.subscribe(),
    };
    Ok((Box::new(board), registration))
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

    fn test_device(serial: Option<&str>) -> UsbDeviceInfo {
        UsbDeviceInfo::new_for_test(
            0xc0de,
            0xcafe,
            serial.map(str::to_string),
            Some("BIRDS".to_string()),
            Some("Mining Board".to_string()),
            "/sys/devices/test".to_string(),
        )
    }

    #[test]
    fn test_board_creation() {
        let board = BirdsBoard::new(test_device(Some("TEST001")));
        assert!(board.is_ok());

        let board = board.unwrap();
        assert_eq!(board.board_info().model, "BIRDS");
    }

    #[test]
    fn test_birds_ports_requires_exactly_two_serial_ports() {
        let ports = vec!["/dev/ttyACM0".to_string()];
        let error = BirdsPorts::from_slice(&ports).expect_err("one port should be rejected");
        assert_eq!(
            error.to_string(),
            "Board initialization failed: BIRDS requires exactly 2 serial ports, found 1"
        );
    }

    #[test]
    fn test_birds_ports_preserves_control_and_data_order() {
        let ports = vec!["/dev/ttyACM0".to_string(), "/dev/ttyACM1".to_string()];
        let birds_ports = BirdsPorts::from_slice(&ports).unwrap();
        assert_eq!(birds_ports.control_port, "/dev/ttyACM0");
        assert_eq!(birds_ports.data_port, "/dev/ttyACM1");
    }

    #[test]
    fn test_build_gpio_write_packet_layout() {
        let packet = build_gpio_write_packet(CTRL_ID_POWER_RESET, GPIO_ASIC_RST, true);
        assert_eq!(
            packet,
            [
                0x07,
                0x00,
                CTRL_ID_POWER_RESET,
                0x00,
                CTRL_PAGE_GPIO,
                GPIO_ASIC_RST,
                0x01
            ]
        );
    }

    #[test]
    fn test_validate_gpio_ack_accepts_matching_device_id() {
        let ack = [0x04, 0x00, CTRL_ID_VR, 0x00];
        assert!(validate_gpio_ack(CTRL_ID_VR, GPIO_VR_EN, ack).is_ok());
    }

    #[test]
    fn test_validate_gpio_ack_rejects_mismatched_device_id() {
        let ack = [0x04, 0x00, CTRL_ID_POWER_RESET, 0x00];
        let error =
            validate_gpio_ack(CTRL_ID_VR, GPIO_VR_EN, ack).expect_err("mismatched ack must fail");
        assert_eq!(
            error.to_string(),
            format!(
                "Hardware control error: GPIO ack ID mismatch for pin {}: expected 0x{:02x}, got 0x{:02x}",
                GPIO_VR_EN, CTRL_ID_VR, CTRL_ID_POWER_RESET
            )
        );
    }

    #[test]
    fn test_thread_name_uses_serial_prefix() {
        assert_eq!(
            BirdsBoard::thread_name_for_serial(Some("1234567890")),
            "BIRDS-12345678"
        );
    }

    #[test]
    fn test_thread_name_falls_back_when_serial_is_missing() {
        assert_eq!(BirdsBoard::thread_name_for_serial(None), "BIRDS");
    }
}
