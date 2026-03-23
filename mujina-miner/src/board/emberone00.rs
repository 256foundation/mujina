//! emberOne/00 hash board support
//!
//! The emberOne/00 has 12 BM1362 ASIC chips, communicating via USB
//! using the bitaxe-raw protocol (same as Bitaxe boards).

use crate::tracing::prelude::*;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use tokio::sync::watch;
use tokio_serial::SerialPortBuilderExt;

use super::{
    Board, BoardDescriptor, BoardInfo,
    pattern::{BoardPattern, Match, StringMatch},
};
use crate::{
    api_client::types::BoardTelemetry,
    asic::hash_thread::HashThread,
    mgmt_protocol::{
        ControlChannel,
        bitaxe_raw::{ResponseFormat, led::BitaxeRawLed, system},
    },
    peripheral::led::{CalibratedLed, ColorProfile, Status, StatusLed},
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

/// Select response format based on firmware version in bcdDevice.
///
/// Firmware minor >= 1 uses v1 response format with explicit
/// status byte. Older firmware uses v0.
fn response_format(bcd_device: Option<u16>) -> ResponseFormat {
    match bcd_device {
        Some(bcd) if (bcd & 0x00F0) >= 0x0010 => ResponseFormat::V1,
        _ => ResponseFormat::V0,
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
    let format = response_format(device.bcd_device);
    let channel = ControlChannel::new(control_port, format);

    let led = BitaxeRawLed::new(channel.clone());
    let led = CalibratedLed::new(Box::new(led), ColorProfile::SK6812);
    let status_led = StatusLed::new(Box::new(led), Status::Initializing);

    let serial = device.serial_number.clone();
    let initial_telemetry = BoardTelemetry {
        name: format!("emberone00-{}", serial.as_deref().unwrap_or("unknown")),
        model: "emberOne/00".into(),
        serial,
        ..Default::default()
    };
    let (telemetry_tx, telemetry_rx) = watch::channel(initial_telemetry);

    let board = EmberOne00 {
        device_info: device,
        channel,
        status_led,
        telemetry_tx,
    };

    let registration = super::BoardRegistration { telemetry_rx };
    Ok((Box::new(board), registration))
}

/// emberOne/00 hash board
pub struct EmberOne00 {
    device_info: UsbDeviceInfo,
    channel: ControlChannel,
    status_led: StatusLed,

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
        self.status_led.off().await;
        let _ = system::reboot(&self.channel).await;
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
