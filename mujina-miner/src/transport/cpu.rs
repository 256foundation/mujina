//! CPU miner virtual transport.
//!
//! Describes the virtual devices of the CPU mining backend. Unlike USB
//! devices, these are synthesized at startup based on environment
//! configuration rather than discovered from hardware.

/// Identifies a CPU miner virtual device.
///
/// The transport reports the same ID when a device connects and when
/// it disconnects.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DeviceId(String);

impl DeviceId {
    /// Creates an ID from its text.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

/// Information about a CPU miner virtual device.
#[derive(Debug, Clone)]
pub struct CpuDeviceInfo {
    /// Unique identifier for this virtual device.
    pub device_id: DeviceId,

    /// Number of mining threads to create.
    pub thread_count: usize,

    /// Target CPU duty cycle percentage (1-100).
    pub duty_percent: u8,
}
