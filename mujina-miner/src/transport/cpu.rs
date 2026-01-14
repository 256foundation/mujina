//! CPU miner virtual transport.
//!
//! Provides transport events for the CPU mining backend. Unlike USB transport,
//! these events are synthesized at startup based on environment configuration
//! rather than discovered from hardware.

/// Transport events for CPU miner virtual devices.
#[derive(Debug)]
pub enum TransportEvent {
    /// A CPU miner "device" was connected (enabled via environment).
    CpuDeviceConnected(CpuDeviceInfo),

    /// A CPU miner "device" was disconnected.
    CpuDeviceDisconnected { device_id: String },
}

/// Information about a CPU miner virtual device.
#[derive(Debug, Clone)]
pub struct CpuDeviceInfo {
    /// Unique identifier for this virtual device.
    pub device_id: String,

    /// Number of mining threads to create.
    pub thread_count: usize,

    /// Target CPU duty cycle percentage (1-100).
    pub duty_percent: u8,
}
