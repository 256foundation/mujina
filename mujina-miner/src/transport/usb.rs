//! USB transport implementation.
//!
//! This module handles USB device discovery and hotplug events using
//! the nusb crate for cross-platform support. It provides raw device
//! information without any knowledge of what the devices are or how
//! they should be configured.
//!
//! ## Platform Support
//!
//! - **Linux**: nusb (enumeration/hotplug) + udev (serial port lookup)
//! - **macOS**: nusb (enumeration/hotplug) + IOKit (serial port lookup)

use std::collections::HashMap;
use std::fmt;

use crate::tracing::prelude::*;
use anyhow::Result;
use futures::stream::StreamExt;
use nusb::hotplug::{HotplugEvent, HotplugWatch};
use nusb::{DeviceId, DeviceInfo};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{Device, DeviceKey, TransportEvent};

// Platform-specific serial port discovery, aliased to a common name
// so call sites are platform-independent.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as platform;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as platform;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
use windows as platform;

// On unsupported platforms, serial port discovery is a stub so the
// miner still compiles (e.g., for CPU mining). If this is ever
// called, something has gone wrong because a board matched a USB
// device on a platform where we can't find its serial ports.
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod platform {
    use anyhow::{Result, bail};
    use nusb::DeviceInfo;

    pub fn device_path(device: &DeviceInfo) -> String {
        format!("{:?}", device.id())
    }

    pub async fn get_serial_ports(_device_path: &str, _expected: usize) -> Result<Vec<String>> {
        bail!("serial port discovery is not supported on this platform");
    }
}

/// Where a USB device is attached, in the platform's terms.
///
/// On Linux this is the sysfs path (e.g.,
/// "/sys/devices/pci0000:00/..."); on macOS it encodes the IOKit
/// location ID (e.g., "IOKit:0x14200000"). The transport reports the
/// same path when a device connects and when it disconnects.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DevicePath(String);

impl DevicePath {
    /// Creates a device path from the platform's text for it.
    pub fn new(path: impl Into<String>) -> Self {
        Self(path.into())
    }

    /// Returns the path as the platform wrote it.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DevicePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Information about a discovered USB device.
#[derive(Debug)]
pub struct UsbDeviceInfo {
    /// USB vendor ID
    pub vid: u16,
    /// USB product ID
    pub pid: u16,
    /// USB device release number (bcdDevice).
    /// BCD-encoded version number (e.g., 0x0500 = version 5.00).
    pub bcd_device: u16,
    /// Device serial number (if available)
    pub serial_number: Option<String>,
    /// Manufacturer string (if available)
    pub manufacturer: Option<String>,
    /// Product string (if available)
    pub product: Option<String>,
    /// Where the device is attached, used for serial port discovery
    /// and disconnect correlation.
    pub device_path: DevicePath,
}

impl UsbDeviceInfo {
    /// Get serial ports associated with this USB device.
    ///
    /// Waits for at least `expected` device nodes to appear and
    /// become accessible, since serial ports may not be ready
    /// immediately after a USB device is connected. The count is
    /// needed because ports can appear incrementally and a partial
    /// result is not useful. Returns an error if the count is not
    /// reached after retries are exhausted.
    ///
    /// Only call this for devices expected to have serial ports,
    /// since the retry logic makes it expensive for devices that
    /// don't.
    pub async fn get_serial_ports(&self, expected: usize) -> Result<Vec<String>> {
        platform::get_serial_ports(self.device_path.as_str(), expected).await
    }
}

/// USB transport discovery.
pub struct UsbTransport {
    event_tx: mpsc::Sender<TransportEvent>,
}

impl UsbTransport {
    /// Create a new USB transport.
    pub fn new(event_tx: mpsc::Sender<TransportEvent>) -> Self {
        Self { event_tx }
    }

    /// Start discovery and monitoring.
    ///
    /// Creates the hotplug watch synchronously so setup errors
    /// propagate to the caller, then spawns an async task for
    /// enumeration and event processing.
    pub async fn start_discovery(&self, shutdown: CancellationToken) -> Result<()> {
        let hotplug = nusb::watch_devices()?;
        let event_tx = self.event_tx.clone();

        tokio::spawn(async move {
            if let Err(e) = monitor(event_tx, shutdown, hotplug).await {
                error!("USB monitoring failed: {}", e);
            }
        });

        Ok(())
    }
}

/// Enumerate existing devices, then watch for hotplug events.
///
/// The `hotplug` watch is created before enumeration begins so that
/// devices connected during enumeration appear in the stream rather
/// than being silently lost. This may produce duplicates (a device
/// in both the enumeration results and as a Connected event), which
/// are filtered out via the known_devices map.
async fn monitor(
    event_tx: mpsc::Sender<TransportEvent>,
    shutdown: CancellationToken,
    mut hotplug: HotplugWatch,
) -> Result<()> {
    // Track device_path by DeviceId so disconnect events can report
    // the same device_path that was sent on connect.
    let mut known_devices: HashMap<DeviceId, DevicePath> = HashMap::new();

    // Initial enumeration
    for device in nusb::list_devices().await? {
        let info = build_device_info(&device);
        trace!(
            vid = %format!("{:04x}", info.vid),
            pid = %format!("{:04x}", info.pid),
            bcd_device = %format!("{:04x}", info.bcd_device),
            manufacturer = ?info.manufacturer,
            product = ?info.product,
            "USB device enumerated"
        );

        known_devices.insert(device.id(), info.device_path.clone());

        let event = TransportEvent::Connected(Device::Usb(info));
        if event_tx.send(event).await.is_err() {
            debug!("Event receiver dropped during enumeration");
            return Ok(());
        }
    }

    // Signal that the initial scan is done, before watching for hotplug.
    if event_tx
        .send(TransportEvent::InitialEnumerationComplete)
        .await
        .is_err()
    {
        debug!("Event receiver dropped during enumeration");
        return Ok(());
    }

    // Watch for hotplug events
    loop {
        tokio::select! {
            event = hotplug.next() => {
                let Some(event) = event else {
                    warn!("USB hotplug stream ended");
                    return Ok(());
                };

                let event = match event {
                    HotplugEvent::Connected(device) => {
                        // Deduplicate: a race between the hotplug
                        // watch and initial enumeration can deliver
                        // a device from both paths.
                        if known_devices.contains_key(&device.id()) {
                            continue;
                        }
                        let info = build_device_info(&device);
                        trace!(
                            vid = %format!("{:04x}", info.vid),
                            pid = %format!("{:04x}", info.pid),
                            bcd_device = %format!("{:04x}", info.bcd_device),
                            manufacturer = ?info.manufacturer,
                            product = ?info.product,
                            "USB device added"
                        );
                        known_devices.insert(device.id(), info.device_path.clone());
                        TransportEvent::Connected(Device::Usb(info))
                    }
                    HotplugEvent::Disconnected(id) => {
                        let Some(device_path) = known_devices.remove(&id) else {
                            warn!(?id, "disconnect for unknown device");
                            continue;
                        };
                        trace!(%device_path, "USB device removed");
                        TransportEvent::Disconnected(DeviceKey::Usb(device_path))
                    }
                };

                if event_tx.send(event).await.is_err() {
                    debug!("Event receiver dropped, exiting USB monitor");
                    return Ok(());
                }
            }
            _ = shutdown.cancelled() => {
                return Ok(());
            }
        }
    }
}

/// Build a `UsbDeviceInfo` from an nusb `DeviceInfo`.
fn build_device_info(device: &DeviceInfo) -> UsbDeviceInfo {
    UsbDeviceInfo {
        vid: device.vendor_id(),
        pid: device.product_id(),
        bcd_device: device.device_version(),
        serial_number: device.serial_number().map(str::to_string),
        manufacturer: device.manufacturer_string().map(str::to_string),
        product: device.product_string().map(str::to_string),
        device_path: DevicePath::new(platform::device_path(device)),
    }
}

#[cfg(test)]
impl Default for UsbDeviceInfo {
    fn default() -> Self {
        Self {
            vid: 0,
            pid: 0,
            bcd_device: 0,
            serial_number: None,
            manufacturer: None,
            product: None,
            device_path: DevicePath::new(""),
        }
    }
}
