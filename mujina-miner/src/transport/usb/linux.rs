//! Linux udev-based USB discovery implementation.
//!
//! This module discovers USB devices and monitors for hotplug events using
//! libudev on Linux systems.
//!
//! ## Architecture
//!
//! USB monitoring runs in a dedicated OS thread with its own single-threaded
//! Tokio runtime. This design addresses the fact that udev types are !Send
//! (they contain raw C pointers that libudev requires stay on one thread).
//! By running in a dedicated thread with a single-threaded runtime, we get:
//!
//! - Clean async/await code using tokio-udev
//! - tokio::select! for elegant shutdown handling
//! - No unsafe code or manual polling
//! - Proper blocking behavior (waits for events efficiently)
//!
//! ## Device Discovery
//!
//! The implementation uses udev to:
//! - Enumerate existing USB devices at startup
//! - Extract VID/PID/serial number from device attributes
//! - Find associated serial port (tty) devices
//! - Monitor for add/remove events via async udev socket
//!
//! ## Serial Port Ordering
//!
//! When a USB device has multiple serial ports (e.g., /dev/ttyACM0, /dev/ttyACM1),
//! they are sorted by device node name for consistent ordering across
//! reconnections. This is critical for boards that expect a specific port for
//! control vs data communication.

use anyhow::{Context, Result};

use super::{TransportEvent as UsbEvent, UsbDeviceInfo};
use crate::{tracing::prelude::*, transport::TransportEvent};
use futures::stream::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Extracted USB device properties from udev.
struct DeviceProperties {
    vid: u16,
    pid: u16,
    serial_number: Option<String>,
    manufacturer: Option<String>,
    product: Option<String>,
}

/// Find serial port devices (tty) associated with a USB device.
///
/// Takes a USB device sysfs path (e.g., "/sys/devices/pci0000:00/...") and
/// returns serial port device nodes (e.g., ["/dev/ttyACM0", "/dev/ttyACM1"]).
/// Ports are sorted by device node name for consistent ordering.
///
/// This is a public function so UsbDeviceInfo can lazily scan for serial ports
/// without needing access to the original udev::Device reference.
pub(super) fn find_serial_ports_for_device(device_path: &str) -> Result<Vec<String>> {
    let mut ports = Vec::new();

    // Create an enumerator to find tty devices
    let mut enumerator = udev::Enumerator::new().context("failed to create enumerator")?;

    // Look for tty subsystem devices
    enumerator
        .match_subsystem("tty")
        .context("failed to filter by subsystem")?;

    // Scan all tty devices and check if they're children of our USB device
    for tty_device in enumerator
        .scan_devices()
        .context("failed to scan devices")?
    {
        // Check if this tty device is a descendant of our USB device
        // by walking up the parent chain
        let mut current = Some(tty_device.clone());
        let mut is_child = false;

        while let Some(dev) = current {
            if dev.syspath().to_str() == Some(device_path) {
                is_child = true;
                break;
            }
            current = dev.parent();
        }

        if is_child
            && let Some(devnode) = tty_device.devnode()
            && let Some(path_str) = devnode.to_str()
        {
            // Get the device node (e.g., /dev/ttyACM0)
            ports.push(path_str.to_string());
        }
    }

    // Sort ports by name for consistent ordering
    // This ensures /dev/ttyACM0 comes before /dev/ttyACM1
    ports.sort();

    Ok(ports)
}

/// Linux udev-based USB discovery implementation.
pub struct LinuxUdevDiscovery {
    // Future: Add state fields if needed for monitoring
}

impl LinuxUdevDiscovery {
    /// Create a new Linux USB discovery instance.
    pub fn new() -> Result<Self> {
        // Verification happens at first use - if libudev isn't available,
        // the udev::Enumerator or MonitorBuilder calls will fail
        Ok(Self {})
    }

    /// Extract VID, PID, serial, manufacturer, and product from a udev device.
    ///
    /// VID and PID are found in device attributes as hexadecimal strings and are required.
    /// Serial number, manufacturer, and product strings are optional.
    fn extract_device_properties(&self, device: &udev::Device) -> Result<DeviceProperties> {
        // Extract VID (vendor ID) - typically 4 hex digits like "0403"
        let vid_str = device
            .attribute_value("idVendor")
            .and_then(|v| v.to_str())
            .ok_or_else(|| anyhow::anyhow!("missing idVendor attribute"))?;

        let vid = u16::from_str_radix(vid_str, 16)
            .with_context(|| format!("invalid VID '{}'", vid_str))?;

        // Extract PID (product ID) - typically 4 hex digits like "6015"
        let pid_str = device
            .attribute_value("idProduct")
            .and_then(|v| v.to_str())
            .ok_or_else(|| anyhow::anyhow!("missing idProduct attribute"))?;

        let pid = u16::from_str_radix(pid_str, 16)
            .with_context(|| format!("invalid PID '{}'", pid_str))?;

        // Extract serial number (optional)
        let serial_number = device
            .attribute_value("serial")
            .and_then(|v| v.to_str())
            .map(|s| s.trim().to_string());

        // Extract manufacturer string (optional, trim trailing whitespace)
        let manufacturer = device
            .attribute_value("manufacturer")
            .and_then(|v| v.to_str())
            .map(|s| s.trim().to_string());

        // Extract product string (optional, trim trailing whitespace)
        let product = device
            .attribute_value("product")
            .and_then(|v| v.to_str())
            .map(|s| s.trim().to_string());

        Ok(DeviceProperties {
            vid,
            pid,
            serial_number,
            manufacturer,
            product,
        })
    }

    /// Build a UsbDeviceInfo from a udev device.
    ///
    /// Extracts basic device properties but does NOT scan for serial ports.
    /// Serial ports are discovered lazily when UsbDeviceInfo::serial_ports()
    /// is called, avoiding expensive enumeration for devices that won't be used.
    fn build_device_info(&self, device: &udev::Device) -> Result<UsbDeviceInfo> {
        // Extract basic device properties
        let props = self.extract_device_properties(device)?;

        // Get device path
        let device_path = device
            .syspath()
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("invalid device path"))?
            .to_string();

        Ok(UsbDeviceInfo {
            vid: props.vid,
            pid: props.pid,
            serial_number: props.serial_number,
            manufacturer: props.manufacturer,
            product: props.product,
            device_path,
            serial_ports: std::sync::OnceLock::new(),
        })
    }

    /// Enumerate currently connected USB devices.
    fn enumerate_devices(&self) -> Result<Vec<UsbDeviceInfo>> {
        // Create enumerator for USB devices
        let mut enumerator = udev::Enumerator::new().context("failed to create enumerator")?;

        // Filter for USB subsystem
        enumerator
            .match_subsystem("usb")
            .context("failed to match subsystem")?;

        // Scan devices and build info for each
        // We filter to actual devices (not interfaces) by checking for idVendor/idProduct
        let mut devices = Vec::new();
        for device in enumerator
            .scan_devices()
            .context("failed to scan devices")?
        {
            // Skip if this doesn't have idVendor (means it's an interface, not a device)
            if device.attribute_value("idVendor").is_none() {
                continue;
            }

            // Try to build device info, skip devices that fail
            match self.build_device_info(&device) {
                Ok(info) => devices.push(info),
                Err(e) => {
                    // Log but continue - some USB devices may not have complete info
                    trace!(error = %e, "Skipping device");
                }
            }
        }

        Ok(devices)
    }
}

impl super::UsbDiscoveryImpl for LinuxUdevDiscovery {
    fn monitor_blocking(
        self: Box<Self>,
        event_tx: mpsc::Sender<crate::transport::TransportEvent>,
        shutdown: CancellationToken,
    ) -> Result<()> {
        // Create a single-threaded Tokio runtime for this thread.
        //
        // Why do this? The udev types are !Send (contain raw C pointers), so they
        // can't be used with Tokio's multi-threaded runtime. However, we CAN use
        // async/await within a single thread. By creating a current_thread runtime
        // here, we get:
        // - Clean async/await code with tokio-udev
        // - tokio::select! for monitoring both events and shutdown
        // - No unsafe code or manual polling
        // - All udev types stay on this thread (satisfying !Send requirement)
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to create runtime")?;

        // Run the async monitoring loop on this thread's runtime
        runtime.block_on(async {
            // Initial enumeration - send Connected events for existing devices
            for device_info in self.enumerate_devices()? {
                let usb_event = UsbEvent::UsbDeviceConnected(device_info);
                let transport_event = TransportEvent::Usb(usb_event);

                if event_tx.send(transport_event).await.is_err() {
                    info!("Event receiver dropped during enumeration");
                    return Ok(());
                }
            }

            // Create async udev monitor using tokio-udev
            let builder = tokio_udev::MonitorBuilder::new()
                .context("failed to create monitor")?
                .match_subsystem("usb")
                .context("failed to filter monitor")?;

            let socket = builder
                .listen()
                .context("failed to listen")?;

            let mut monitor = tokio_udev::AsyncMonitorSocket::new(socket)
                .context("failed to create async socket")?;

            // Event loop using tokio::select! to wait on both events and shutdown
            // This is clean, safe async code with no manual polling required
            loop {
                tokio::select! {
                    // Wait for USB hotplug event
                    event_result = monitor.next() => {
                        let event = match event_result {
                            Some(Ok(e)) => e,
                            Some(Err(e)) => {
                                error!("Error from USB monitor: {}", e);
                                continue;
                            }
                            None => {
                                warn!("USB monitor stream ended");
                                return Ok(());
                            }
                        };

                        let device = event.device();

                        // Build transport event based on event type
                        let transport_event = match event.event_type() {
                            tokio_udev::EventType::Add => {
                                // Skip if device doesn't have VID (it's an interface, not a device)
                                if device.attribute_value("idVendor").is_none() {
                                    continue;
                                }

                                match self.build_device_info(&device) {
                                    Ok(device_info) => {
                                        debug!(
                                            vid = %format!("{:04x}", device_info.vid),
                                            pid = %format!("{:04x}", device_info.pid),
                                            manufacturer = ?device_info.manufacturer,
                                            product = ?device_info.product,
                                            "USB device added"
                                        );
                                        Some(UsbEvent::UsbDeviceConnected(device_info))
                                    }
                                    Err(e) => {
                                        trace!(error = %e, "Failed to build device info");
                                        None
                                    }
                                }
                            }

                            tokio_udev::EventType::Remove => {
                                device.syspath().to_str().map(|syspath| UsbEvent::UsbDeviceDisconnected {
                                    device_path: syspath.to_string(),
                                })
                            }

                            // Ignore other event types (change, bind, unbind, etc.)
                            _ => None,
                        };

                        // Send the event if we built one
                        if let Some(usb_event) = transport_event {
                            let transport_event = TransportEvent::Usb(usb_event);
                            if event_tx.send(transport_event).await.is_err() {
                                info!("Event receiver dropped, exiting USB monitor");
                                return Ok(());
                            }
                        }
                    }

                    // Wait for shutdown signal
                    _ = shutdown.cancelled() => {
                        return Ok(());
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_discovery() {
        let discovery = LinuxUdevDiscovery::new();
        assert!(discovery.is_ok());
    }
}
