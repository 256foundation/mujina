//! macOS-specific USB platform support.
//!
//! Serial port discovery is not yet implemented on macOS.

use anyhow::{Result, bail};
use nusb::DeviceInfo;

/// Produce a device path string from IOKit metadata.
pub fn device_path(device: &DeviceInfo) -> String {
    format!("{:?}", device.id())
}

/// Search for serial ports associated with a USB device.
///
/// Not yet implemented on macOS.
pub async fn get_serial_ports(_device_path: &str, _expected: usize) -> Result<Vec<String>> {
    bail!("serial port discovery is not yet implemented for macOS")
}
