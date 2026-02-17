//! BIRDS mining board support (stub).
//!
//! The BIRDS board is a mining board with 4 BZM2 ASIC chips, communicating via
//! USB using two serial ports: a control UART for GPIO/I2C and a data UART for
//! ASIC communication with 8-bit to 9-bit serial translation.

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
    asic::{
        bzm2::{FrameCodec, smoke, thread::Bzm2Thread},
        hash_thread::{AsicEnable, BoardPeripherals, HashThread, ThreadRemovalSignal},
    },
    error::Error,
    transport::{
        UsbDeviceInfo,
        serial::{SerialControl, SerialReader, SerialStream, SerialWriter},
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

fn format_hex(data: &[u8]) -> String {
    data.iter()
        .map(|byte| format!("{:02X}", byte))
        .collect::<Vec<_>>()
        .join(" ")
}

/// BIRDS mining board.
pub struct BirdsBoard {
    device_info: UsbDeviceInfo,
    control_port: Option<String>,
    data_reader: Option<FramedRead<SerialReader, FrameCodec>>,
    data_writer: Option<FramedWrite<SerialWriter, FrameCodec>>,
    data_control: Option<SerialControl>,
    thread_shutdown: Option<watch::Sender<ThreadRemovalSignal>>,
}

impl BirdsBoard {
    /// Create a new BIRDS board instance.
    pub fn new(device_info: UsbDeviceInfo) -> Result<Self, BoardError> {
        Ok(Self {
            device_info,
            control_port: None,
            data_reader: None,
            data_writer: None,
            data_control: None,
            thread_shutdown: None,
        })
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

        // Match known-good bring-up sequence from birds_asyncio.py:
        // 1) VR off and settle
        // 2) Enable 5V rail
        // 3) Enable VR
        // 4) Pulse ASIC reset low/high
        // 5) Wait for UART startup
        self.bringup_power_and_reset(&control_port).await?;
        self.control_port = Some(control_port);

        let result = smoke::run_smoke(&data_port, 0).await.map_err(|e| {
            BoardError::InitializationFailed(format!("BIRDS ASIC smoke test failed: {:#}", e))
        })?;

        tracing::info!(
            logical_asic = result.logical_asic,
            asic_hw_id = result.asic_hw_id,
            asic_id = format_args!("0x{:08x}", result.asic_id),
            "BIRDS ASIC smoke test succeeded"
        );

        let data_stream = SerialStream::new(&data_port, DATA_UART_BAUD).map_err(|e| {
            BoardError::InitializationFailed(format!("Failed to open BIRDS data port: {}", e))
        })?;
        let (data_reader, data_writer, data_control) = data_stream.split();
        self.data_reader = Some(FramedRead::new(data_reader, FrameCodec::default()));
        self.data_writer = Some(FramedWrite::new(data_writer, FrameCodec::default()));
        self.data_control = Some(data_control);

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
        let packet: [u8; 7] = [
            0x07,
            0x00,
            dev_id,
            0x00,
            CTRL_PAGE_GPIO,
            pin,
            if value_high { 0x01 } else { 0x00 },
        ];
        tracing::debug!(
            dev_id = format_args!("0x{:02X}", dev_id),
            pin,
            value = if value_high { 1 } else { 0 },
            tx = %format_hex(&packet),
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
            rx = %format_hex(&ack),
            "BIRDS ctrl gpio rx"
        );
        if ack[2] != dev_id {
            return Err(BoardError::HardwareControl(format!(
                "GPIO ack ID mismatch for pin {}: expected 0x{:02x}, got 0x{:02x}",
                pin, dev_id, ack[2]
            )));
        }

        Ok(())
    }

    async fn hold_in_reset(&self) -> Result<(), BoardError> {
        let control_port = self.control_port.as_ref().ok_or_else(|| {
            BoardError::InitializationFailed("BIRDS control port not initialized".into())
        })?;

        let mut control_stream = tokio_serial::new(control_port, CONTROL_UART_BAUD)
            .open_native_async()
            .map_err(|e| {
                BoardError::InitializationFailed(format!(
                    "Failed to open BIRDS control port {}: {}",
                    control_port, e
                ))
            })?;

        Self::control_gpio_write(
            &mut control_stream,
            CTRL_ID_POWER_RESET,
            GPIO_ASIC_RST,
            false,
        )
        .await
    }
}

struct BirdsAsicEnable {
    control_port: String,
}

#[async_trait]
impl AsicEnable for BirdsAsicEnable {
    async fn enable(&mut self) -> anyhow::Result<()> {
        let mut control_stream = tokio_serial::new(&self.control_port, CONTROL_UART_BAUD)
            .open_native_async()
            .map_err(|e| anyhow::anyhow!("failed to open control port: {}", e))?;
        BirdsBoard::control_gpio_write(
            &mut control_stream,
            CTRL_ID_POWER_RESET,
            GPIO_ASIC_RST,
            true,
        )
        .await
        .map_err(|e| anyhow::anyhow!("failed to release BZM2 reset: {}", e))
    }

    async fn disable(&mut self) -> anyhow::Result<()> {
        let mut control_stream = tokio_serial::new(&self.control_port, CONTROL_UART_BAUD)
            .open_native_async()
            .map_err(|e| anyhow::anyhow!("failed to open control port: {}", e))?;
        BirdsBoard::control_gpio_write(
            &mut control_stream,
            CTRL_ID_POWER_RESET,
            GPIO_ASIC_RST,
            false,
        )
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
        if let Some(ref tx) = self.thread_shutdown {
            if let Err(e) = tx.send(ThreadRemovalSignal::Shutdown) {
                tracing::warn!("Failed to send shutdown signal to BIRDS thread: {}", e);
            }
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

        let thread_name = match &self.device_info.serial_number {
            Some(serial) => format!("BIRDS-{}", &serial[..8.min(serial.len())]),
            None => "BIRDS".to_string(),
        };

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
