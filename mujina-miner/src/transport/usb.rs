//! USB transport implementation.
//!
//! This module handles USB device discovery and hotplug events.
//! It provides raw device information without any knowledge of
//! what the devices are or how they should be configured.

use crate::error::Result;
use crate::tracing::prelude::*;
use tokio::sync::mpsc;

/// Information about a discovered USB device.
#[derive(Debug, Clone)]
pub struct UsbDeviceInfo {
    /// USB vendor ID
    pub vid: u16,
    /// USB product ID  
    pub pid: u16,
    /// Device serial number (if available)
    pub serial_number: Option<String>,
    /// USB device path (e.g., "/sys/bus/usb/devices/1-1.2")
    pub device_path: String,
    /// Serial port device nodes associated with this USB device
    /// (e.g., ["/dev/ttyACM0", "/dev/ttyACM1"])
    pub serial_ports: Vec<String>,
    // Future: other interfaces like HID, mass storage, etc.
}

/// Transport event emitted when devices are discovered or disconnected.
#[derive(Debug)]
pub enum TransportEvent {
    /// A USB device was connected
    UsbDeviceConnected(UsbDeviceInfo),

    /// A USB device was disconnected
    UsbDeviceDisconnected { device_path: String },
}

/// USB transport discovery.
pub struct UsbTransport {
    event_tx: mpsc::Sender<super::TransportEvent>,
}

impl UsbTransport {
    /// Create a new USB transport.
    pub fn new(event_tx: mpsc::Sender<super::TransportEvent>) -> Self {
        Self { event_tx }
    }

    /// Start discovery task.
    ///
    /// For now, this immediately "discovers" hardcoded devices.
    /// In the future, this will monitor udev events for USB hotplug.
    pub async fn start_discovery(&self) -> Result<()> {
        info!("Starting USB discovery (hardcoded for now)");

        // Hardcoded discovery - simulating what real USB enumeration would find
        let device = UsbDeviceInfo {
            vid: 0x0403,
            pid: 0x6015, // Matches Bitaxe Gamma registration
            serial_number: Some("BITAXE001".to_string()),
            device_path: "/sys/bus/usb/devices/1-1.2".to_string(),
            serial_ports: vec!["/dev/ttyACM0".to_string(), "/dev/ttyACM1".to_string()],
        };

        info!(
            "Discovered USB device: {:04x}:{:04x} with {} serial ports",
            device.vid,
            device.pid,
            device.serial_ports.len()
        );

        // Emit discovery event wrapped in generic transport event
        let usb_event = TransportEvent::UsbDeviceConnected(device);
        let event = super::TransportEvent::Usb(usb_event);

        if let Err(e) = self.event_tx.send(event).await {
            error!("Failed to send transport event: {}", e);
        }

        Ok(())
    }
}

// Future enhancement: Real USB device enumeration
//
// The discovery process would:
// 1. Monitor udev events for USB device add/remove
// 2. When a device is added, read its descriptors (VID, PID, serial)
// 3. Enumerate child devices to find serial ports, HID interfaces, etc.
// 4. Build a UsbDeviceInfo with all discovered information
// 5. Send a UsbDeviceConnected event
//
// The transport layer doesn't know or care:
// - What baud rate to use for serial ports
// - Which port is for control vs data
// - Whether this is even a mining board
//
// That knowledge belongs in the board implementations.
