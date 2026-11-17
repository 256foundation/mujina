//! Physical transport layer for board connections.
//!
//! This module handles discovery of mining boards across different
//! physical transports (USB, PCIe, Ethernet, etc). Each transport
//! implementation provides device discovery and reports each device
//! that is connected or disconnected.

use anyhow::Result;

pub mod cpu;
pub mod serial;
pub mod usb;

// Re-export transport implementations
pub use cpu::CpuDeviceInfo;
pub use serial::{
    Parity, SerialConfig, SerialControl, SerialError, SerialReader, SerialStats, SerialStream,
    SerialWriter,
};
pub use usb::{UsbDeviceInfo, UsbTransport};

/// A device that a transport has found.
#[derive(Debug)]
pub enum Device {
    /// A USB device.
    Usb(UsbDeviceInfo),

    /// A CPU miner virtual device.
    Cpu(CpuDeviceInfo),
}

/// Identifies a device for as long as it is connected.
///
/// A transport reports the same key when the device connects and when
/// it disconnects.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum DeviceKey {
    /// A USB device, by where it is attached.
    Usb(usb::DevicePath),

    /// A CPU miner virtual device.
    Cpu(cpu::DeviceId),
}

/// An event from a transport.
#[derive(Debug)]
pub enum TransportEvent {
    /// A device was connected.
    Connected(Device),

    /// A device was disconnected.
    Disconnected(DeviceKey),

    /// The transport finished its initial device scan.
    ///
    /// Emitted once per transport, after its starting devices and before any
    /// later hotplug events. The backplane waits for one from every transport
    /// before telling the scheduler that startup enumeration is complete.
    InitialEnumerationComplete,
}

/// Common trait for transport discovery (future enhancement).
///
/// Each transport implementation could implement this trait to provide
/// a consistent interface for device discovery across different transports.
#[async_trait::async_trait]
pub trait TransportDiscovery: Send + Sync {
    /// Start discovering devices on this transport.
    async fn start_discovery(&self) -> Result<()>;

    /// Stop discovery and clean up resources.
    async fn stop_discovery(&self) -> Result<()>;
}
