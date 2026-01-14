//! Physical transport layer for board connections.
//!
//! This module handles discovery of mining boards across different
//! physical transports (USB, PCIe, Ethernet, etc). Each transport
//! implementation provides device discovery and emits transport-specific
//! events when devices are connected or disconnected.

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

/// Generic transport event that can represent different transport types.
#[derive(Debug)]
pub enum TransportEvent {
    /// USB device event
    Usb(usb::TransportEvent),

    /// CPU miner virtual device event
    Cpu(cpu::TransportEvent),
}

/// Common trait for transport discovery (future enhancement).
///
/// Each transport implementation could implement this trait to provide
/// a consistent interface for device discovery across different transports.
#[async_trait::async_trait]
pub trait TransportDiscovery: Send + Sync {
    /// Start discovering devices on this transport.
    async fn start_discovery(&self) -> crate::error::Result<()>;

    /// Stop discovery and clean up resources.
    async fn stop_discovery(&self) -> crate::error::Result<()>;
}
