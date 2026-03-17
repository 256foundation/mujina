//! macOS USB discovery implementation stub.
//!
//! This module provides a stub implementation for macOS that will be replaced
//! with a proper IOKit-based implementation in the future.
//!
//! ## Future Implementation
//!
//! When implementing macOS support:
//! - Use IOKit framework for USB device enumeration
//! - Use IOKit notification ports for hotplug events
//! - Map IOKit device properties to UsbDeviceInfo
//! - Handle macOS-specific device paths and serial port naming

use anyhow::Result;

use crate::transport::TransportEvent;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// macOS IOKit-based USB discovery (stub).
pub struct MacOsIoKitDiscovery;

impl MacOsIoKitDiscovery {
    /// Create a new macOS USB discovery instance.
    pub fn new() -> Result<Self> {
        anyhow::bail!("USB discovery is not yet implemented for macOS")
    }
}

impl super::UsbDiscoveryImpl for MacOsIoKitDiscovery {
    fn monitor_blocking(
        self: Box<Self>,
        _event_tx: mpsc::Sender<TransportEvent>,
        _shutdown: CancellationToken,
    ) -> Result<()> {
        unimplemented!("macOS USB monitoring not yet implemented")
    }
}
