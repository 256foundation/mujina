//! Windows serial-port discovery for USB devices.
//!
//! `main` discovers USB devices with nusb (cross-platform), but mapping a
//! device to its COM ports is platform-specific. On Windows we enumerate COM
//! ports via `serialport`/`tokio-serial` and match them to the nusb device by
//! VID:PID:serial.
//!
//! Windows assigns COM numbers from the registry with no relation to USB
//! interface order, so we order the returned ports by USB interface number
//! (interface 0 first) to keep control/data assignment stable across machines.
//! This requires the `usbportinfo-interface` feature on `serialport`.

use std::time::Duration;

use anyhow::{Result, bail};
use nusb::DeviceInfo;
use tokio_serial::{SerialPortType, available_ports};

/// Build the platform device path. On Windows we key devices by
/// `vid:pid:serial` (lowercased) so the value can be matched against the
/// COM-port enumeration, which is the only identifier the two APIs share.
pub fn device_path(device: &DeviceInfo) -> String {
    make_key(
        device.vendor_id(),
        device.product_id(),
        device.serial_number(),
    )
}

/// Normalized `vid:pid:serial` key. Serial is lowercased because nusb and
/// `serialport` can report it in different cases on Windows.
fn make_key(vid: u16, pid: u16, serial: Option<&str>) -> String {
    format!(
        "{:04x}:{:04x}:{}",
        vid,
        pid,
        serial.unwrap_or("no-serial").to_lowercase()
    )
}

/// Find the COM ports for the USB device identified by `device_path` (a
/// `vid:pid:serial` key from [`device_path`]), ordered by USB interface
/// number. Retries until at least `expected` ports appear or the timeout
/// elapses, since COM ports can lag slightly behind USB enumeration.
pub async fn get_serial_ports(device_path: &str, expected: usize) -> Result<Vec<String>> {
    const RETRY_INTERVAL: Duration = Duration::from_millis(250);
    const MAX_ATTEMPTS: usize = 24; // ~6 seconds
    // After a hotplug, a COM port can appear in the enumeration slightly before
    // its device node is openable (CreateFile returns ERROR_FILE_NOT_FOUND).
    // Once all expected ports are present, wait this long and re-check so the
    // caller's subsequent open() succeeds.
    const SETTLE: Duration = Duration::from_millis(1000);

    let mut last_found = 0;
    for _ in 0..MAX_ATTEMPTS {
        let ports = matching_ports(device_path)?;
        if ports.len() >= expected {
            tokio::time::sleep(SETTLE).await;
            let settled = matching_ports(device_path)?;
            if settled.len() >= expected {
                return Ok(settled);
            }
        }
        last_found = ports.len();
        tokio::time::sleep(RETRY_INTERVAL).await;
    }

    bail!(
        "expected {expected} serial ports for USB device {device_path}, \
         found {last_found} after retries"
    );
}

/// Enumerate COM ports belonging to the given `vid:pid:serial` device,
/// ordered by USB interface number (falling back to port name).
fn matching_ports(device_path: &str) -> Result<Vec<String>> {
    let mut matched: Vec<(Option<u8>, String)> = Vec::new();

    for port in available_ports()? {
        if let SerialPortType::UsbPort(usb) = &port.port_type
            && make_key(usb.vid, usb.pid, usb.serial_number.as_deref()) == device_path
        {
            matched.push((usb.interface, port.port_name.clone()));
        }
    }

    // Windows COM numbers don't follow interface order; sort by interface
    // (interface 0 = control) so control/data mapping is deterministic.
    matched.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    Ok(matched.into_iter().map(|(_, name)| name).collect())
}
