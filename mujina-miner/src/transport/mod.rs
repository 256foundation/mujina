//! Physical transport layer for board connections.
//!
//! This module handles discovery of mining boards across different
//! physical transports (USB, PCIe, Ethernet, etc). Each transport
//! implementation provides device discovery and emits transport-specific
//! events when devices are connected or disconnected.

use anyhow::Result;

pub mod cpu;
pub mod nine_bit;
pub mod serial;
pub mod usb;

// Re-export transport implementations
pub use cpu::CpuDeviceInfo;
pub use serial::{
    Parity, SerialConfig, SerialControl, SerialError, SerialReader, SerialStats, SerialStream,
    SerialWriter,
};
pub use usb::{UsbDeviceInfo, UsbTransport};

/// Generic transport event that can represent different transport types.
#[derive(Debug)]
pub enum TransportEvent {
    /// USB device event
    Usb(usb::TransportEvent),

    /// CPU miner virtual device event
    Cpu(cpu::TransportEvent),

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
