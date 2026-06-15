//! Windows COM port-based USB discovery implementation.
//!
//! This module discovers USB devices on Windows using the serialport crate's
//! cross-platform port enumeration API.
//!
//! ## Architecture
//!
//! USB monitoring runs in a dedicated OS thread since we use blocking I/O
//! for port enumeration. This matches the pattern used on Linux but adapts
//! for Windows COM ports instead of udev.
//!
//! ## Device Discovery
//!
//! The implementation uses serialport to:
//! - Enumerate existing COM ports at startup
//! - Extract VID/PID/serial number from port information
//! - Group ports by parent USB device (using VID/PID/serial)
//! - Emit events for discovered devices
//!
//! ## Limitations
//!
//! Unlike Linux's udev which provides real-time hotplug events, this Windows
//! implementation performs periodic polling. This is a reasonable trade-off
//! for now, as device connection/disconnection is relatively infrequent.

use super::{TransportEvent as UsbEvent, UsbDeviceInfo};
use crate::{error::Result, tracing::prelude::*, transport::TransportEvent};
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tokio_serial::SerialPortType;

/// Windows USB discovery implementation using COM port enumeration.
pub struct WindowsSerialDiscovery {
    // No state needed - we'll enumerate ports on each poll
}

impl WindowsSerialDiscovery {
    /// Create a new Windows serial discovery implementation.
    pub fn new() -> Result<Self> {
        Ok(Self {})
    }
}

impl super::UsbDiscoveryImpl for WindowsSerialDiscovery {
    fn monitor_blocking(
        self: Box<Self>,
        event_tx: mpsc::Sender<super::super::TransportEvent>,
        shutdown: CancellationToken,
    ) -> Result<()> {
        info!("Starting Windows COM port USB discovery");

        // Track known devices by a unique key (VID:PID:SERIAL)
        let mut known_devices: HashMap<String, Vec<String>> = HashMap::new();

        // Initial enumeration
        match enumerate_usb_devices() {
            Ok(devices) => {
                debug!("Initial enumeration found {} USB devices", devices.len());
                for (device_key, device_info, ports) in devices {
                    debug!("Found USB device: {} with ports: {:?}", device_key, ports);
                    debug!(
                        "  VID:PID = {:04x}:{:04x}, Manufacturer: {:?}, Product: {:?}",
                        device_info.vid,
                        device_info.pid,
                        device_info.manufacturer,
                        device_info.product
                    );
                    known_devices.insert(device_key, ports.clone());
                    let event = TransportEvent::Usb(UsbEvent::UsbDeviceConnected(device_info));
                    if event_tx.blocking_send(event).is_err() {
                        info!("USB event channel closed, shutting down");
                        return Ok(());
                    }
                }
            }
            Err(e) => {
                warn!("Initial port enumeration failed: {}", e);
            }
        }

        // Poll for changes every 2 seconds
        let poll_interval = Duration::from_secs(2);

        loop {
            // Check for shutdown
            if shutdown.is_cancelled() {
                info!("USB monitor shutting down");
                break;
            }

            // Sleep for poll interval (check shutdown periodically)
            std::thread::sleep(poll_interval);

            // Enumerate again and compare
            match enumerate_usb_devices() {
                Ok(current_devices) => {
                    let current_keys: HashSet<_> =
                        current_devices.iter().map(|(k, _, _)| k.clone()).collect();
                    let known_keys: HashSet<_> = known_devices.keys().cloned().collect();

                    // Find new devices (in current but not in known)
                    for device_key in current_keys.difference(&known_keys) {
                        if let Some((_, device_info, ports)) = current_devices
                            .iter()
                            .find(|(k, _, _)| k == device_key)
                        {
                            debug!("New USB device detected: {}", device_key);
                            known_devices.insert(device_key.clone(), ports.clone());
                            let event =
                                TransportEvent::Usb(UsbEvent::UsbDeviceConnected(device_info.clone()));
                            if event_tx.blocking_send(event).is_err() {
                                info!("USB event channel closed, shutting down");
                                return Ok(());
                            }
                        }
                    }

                    // Find removed devices (in known but not in current)
                    for device_key in known_keys.difference(&current_keys) {
                        debug!("USB device disconnected: {}", device_key);
                        // Remove from tracking
                        known_devices.remove(device_key);
                        // Send disconnect event (use device_key as device_path)
                        let event = TransportEvent::Usb(UsbEvent::UsbDeviceDisconnected {
                            device_path: device_key.clone(),
                        });
                        if event_tx.blocking_send(event).is_err() {
                            info!("USB event channel closed, shutting down");
                            return Ok(());
                        }
                    }
                }
                Err(e) => {
                    warn!("Port enumeration failed: {}", e);
                }
            }
        }

        Ok(())
    }
}

/// Enumerate USB devices by discovering COM ports.
///
/// Returns a vector of (device_key, UsbDeviceInfo, port_names) tuples.
/// The device_key is a unique identifier like "c0de:cafe:12345678".
fn enumerate_usb_devices() -> Result<Vec<(String, UsbDeviceInfo, Vec<String>)>> {
    // Use tokio_serial to enumerate all available ports
    let ports = tokio_serial::available_ports()
        .map_err(|e| crate::error::Error::Other(format!("Failed to enumerate COM ports: {}", e)))?;

    // Group ports by USB device (VID:PID:serial)
    let mut device_map: HashMap<String, (tokio_serial::UsbPortInfo, Vec<String>)> = HashMap::new();

    for port in ports {
        // Only process USB serial ports (not native COM ports or Bluetooth)
        if let SerialPortType::UsbPort(usb_info) = &port.port_type {
            // Create a unique key for this USB device
            let device_key = format!(
                "{:04x}:{:04x}:{}",
                usb_info.vid,
                usb_info.pid,
                usb_info
                    .serial_number
                    .as_deref()
                    .unwrap_or("no-serial")
            );

            // Add this port to the device's port list
            device_map
                .entry(device_key)
                .or_insert_with(|| (usb_info.clone(), Vec::new()))
                .1
                .push(port.port_name.clone());
        }
    }

    // Convert to result format
    let mut devices = Vec::new();
    for (device_key, (usb_info, mut ports)) in device_map {
        // Sort ports for consistent ordering (COM3 before COM11)
        ports.sort();

        // Create UsbDeviceInfo
        let device_info = create_device_info(usb_info, device_key.clone(), ports.clone());
        devices.push((device_key, device_info, ports));
    }

    Ok(devices)
}

/// Create a UsbDeviceInfo from Windows USB port information.
fn create_device_info(
    usb_info: tokio_serial::UsbPortInfo,
    device_path: String,
    ports: Vec<String>,
) -> UsbDeviceInfo {
    UsbDeviceInfo {
        vid: usb_info.vid,
        pid: usb_info.pid,
        serial_number: usb_info.serial_number.clone(),
        manufacturer: usb_info.manufacturer.clone(),
        product: usb_info.product.clone(),
        device_path,
        serial_ports: OnceLock::from(Ok(ports)),
    }
}

/// Find serial ports for a device (Windows implementation).
///
/// On Windows, we already cached the ports in UsbDeviceInfo during enumeration,
/// so this function is just for API compatibility.
pub(super) fn find_serial_ports_for_device(_device_path: &str) -> Result<Vec<String>> {
    // On Windows, serial ports are already cached in UsbDeviceInfo
    // This function exists for API compatibility but shouldn't be called
    // since we populate serial_ports during device creation
    Ok(vec![])
}
