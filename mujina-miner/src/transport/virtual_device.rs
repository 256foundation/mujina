//! Generic virtual device transport.
//!
//! Virtual boards are injected by configuration rather than hardware discovery.

/// Transport events for generic virtual devices.
#[derive(Debug)]
pub enum TransportEvent {
    /// A virtual device was connected.
    VirtualDeviceConnected(VirtualDeviceInfo),

    /// A virtual device was disconnected.
    VirtualDeviceDisconnected { device_id: String },
}

/// Information about a generic virtual device.
#[derive(Debug, Clone)]
pub struct VirtualDeviceInfo {
    /// Virtual board type identifier.
    pub device_type: String,

    /// Unique identifier for this virtual instance.
    pub device_id: String,
}
