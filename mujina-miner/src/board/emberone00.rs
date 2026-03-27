//! emberOne/00 hash board support
//!
//! The emberOne/00 has 12 BM1362 ASIC chips, communicating via USB
//! using the bitaxe-raw protocol (same as Bitaxe boards).

use std::time::Duration;

use crate::tracing::prelude::*;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{self, MissedTickBehavior};
use tokio_serial::SerialPortBuilderExt;

use super::{
    Board, BoardDescriptor, BoardInfo,
    pattern::{BoardPattern, Match, StringMatch},
};
use crate::{
    api_client::types::{BoardTelemetry, TemperatureSensor},
    asic::hash_thread::HashThread,
    mgmt_protocol::{
        ControlChannel,
        bitaxe_raw::{DeviceVersion, ResponseFormat, i2c::BitaxeRawI2c, led::BitaxeRawLed, system},
    },
    peripheral::{
        led::{CalibratedLed, ColorProfile, Status, StatusLed},
        tmp451::Tmp451,
        tmp1075::Tmp1075,
    },
    transport::UsbDeviceInfo,
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

/// I2C device addresses on the emberOne/00 board.
mod i2c_addr {
    pub const TMP1075_LEFT: u8 = 0x4A;
    pub const TMP451_RIGHT: u8 = 0x49;
}

/// Select response format based on firmware version.
///
/// Firmware minor >= 1 uses v1 response format with explicit
/// status byte. Older firmware uses v0.
fn response_format(version: &DeviceVersion) -> ResponseFormat {
    if version.firmware_minor() >= 1 {
        ResponseFormat::V1
    } else {
        ResponseFormat::V0
    }
}

async fn create_from_usb(
    device: UsbDeviceInfo,
) -> Result<(Box<dyn Board + Send>, super::BoardRegistration)> {
    let serial_ports = device.serial_ports()?;
    if serial_ports.len() != 2 {
        bail!(
            "emberOne/00 requires exactly 2 serial ports, found {}",
            serial_ports.len()
        );
    }

    debug!(
        serial = ?device.serial_number,
        control = %serial_ports[0],
        data = %serial_ports[1],
        "Opening emberOne/00 serial ports"
    );

    let control_port = tokio_serial::new(&serial_ports[0], 115200)
        .open_native_async()
        .context("failed to open control port")?;
    let version = DeviceVersion::from_bcd(device.bcd_device);
    let format = response_format(&version);
    let control = ControlChannel::new(control_port, format);

    let i2c = BitaxeRawI2c::new(control.clone());

    let led = BitaxeRawLed::new(control.clone());
    let led = CalibratedLed::new(Box::new(led), ColorProfile::SK6812);
    let status_led = StatusLed::new(Box::new(led), Status::Initializing);

    let mut temp_left = Tmp1075::new(i2c.clone(), i2c_addr::TMP1075_LEFT);
    temp_left
        .init()
        .await
        .context("TMP1075 (left) init failed")?;

    let mut temp_right = Tmp451::new(i2c.clone(), i2c_addr::TMP451_RIGHT);
    temp_right
        .init()
        .await
        .context("TMP451 (right) init failed")?;
    // The remote diode reads high compared to the board sensors
    // at ambient. This offset brings the reading in line at room
    // temperature. Revisit once the ASICs are running to
    // distinguish constant offset from ideality (n-factor) error.
    const REMOTE_OFFSET_C: i8 = -7;
    temp_right
        .set_remote_offset(REMOTE_OFFSET_C)
        .await
        .context("TMP451 remote offset failed")?;

    let serial = device.serial_number.clone();
    let board_name = format!("emberone00-{}", serial.as_deref().unwrap_or("unknown"));
    let initial_telemetry = BoardTelemetry {
        name: board_name.clone(),
        model: "emberOne/00".into(),
        serial,
        ..Default::default()
    };
    let (telemetry_tx, telemetry_rx) = watch::channel(initial_telemetry);

    let monitor_handle = spawn_monitor(temp_left, temp_right, telemetry_tx);

    let board = EmberOne00 {
        device_info: device,
        control,
        status_led,
        monitor_handle,
    };

    let registration = super::BoardRegistration { telemetry_rx };
    Ok((Box::new(board), registration))
}

/// Spawn a task that periodically reads sensors and publishes telemetry.
fn spawn_monitor(
    mut temp_left: Tmp1075<BitaxeRawI2c>,
    mut temp_right: Tmp451<BitaxeRawI2c>,
    telemetry_tx: watch::Sender<BoardTelemetry>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        const INTERVAL: Duration = Duration::from_secs(5);
        let mut ticker = time::interval(INTERVAL);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        // Discard first tick (fires immediately)
        ticker.tick().await;

        loop {
            ticker.tick().await;

            let left_c = match temp_left.read().await {
                Ok(reading) => Some(reading.as_degrees_c()),
                Err(e) => {
                    warn!("TMP1075 (left) read failed: {}", e);
                    None
                }
            };

            let right_c = match temp_right.read_local().await {
                Ok(reading) => Some(reading.as_degrees_c()),
                Err(e) => {
                    warn!("TMP451 (right) local read failed: {}", e);
                    None
                }
            };

            // TODO: attribute to the chip's hash thread once implemented
            let chip0_c = match temp_right.read_remote().await {
                Ok(reading) => Some(reading.as_degrees_c()),
                Err(e) => {
                    if !matches!(e, crate::peripheral::tmp451::Error::RemoteDiodeOpen) {
                        warn!("TMP451 remote read failed: {}", e);
                    }
                    None
                }
            };

            telemetry_tx.send_modify(|t| {
                t.temperatures = vec![
                    TemperatureSensor {
                        name: "pcb-left".into(),
                        temperature_c: left_c,
                    },
                    TemperatureSensor {
                        name: "pcb-right".into(),
                        temperature_c: right_c,
                    },
                    TemperatureSensor {
                        name: "chip-0".into(),
                        temperature_c: chip0_c,
                    },
                ];
            });
        }
    })
}

/// emberOne/00 hash board
pub struct EmberOne00 {
    device_info: UsbDeviceInfo,
    control: ControlChannel,
    status_led: StatusLed,
    monitor_handle: JoinHandle<()>,
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
        self.monitor_handle.abort();
        self.status_led.off().await;
        let _ = system::reboot(&self.control).await;
        Ok(())
    }

    async fn create_hash_threads(&mut self) -> Result<Vec<Box<dyn HashThread>>> {
        bail!("emberOne/00 hash threads not yet implemented")
    }
}

#[cfg(test)]
mod tests {}

#[cfg(test)]
mod integration_tests;
