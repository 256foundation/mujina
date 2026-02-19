//! USB transport implementation.
//!
//! This module handles USB device discovery and hotplug events.
//! It provides raw device information without any knowledge of
//! what the devices are or how they should be configured.
//!
//! ## Platform Support
//!
//! - **Linux**: Uses udev for device enumeration and hotplug monitoring
//! - **macOS**: Stub implementation (IOKit support planned for future)

use crate::{error::Result, tracing::prelude::*};
use std::sync::OnceLock;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Information about a discovered USB device.
#[derive(Debug)]
pub struct UsbDeviceInfo {
    /// USB vendor ID
    pub vid: u16,
    /// USB product ID
    pub pid: u16,
    /// Device serial number (if available)
    pub serial_number: Option<String>,
    /// Manufacturer string (if available)
    pub manufacturer: Option<String>,
    /// Product string (if available)
    pub product: Option<String>,
    /// USB device path (e.g., "/sys/bus/usb/devices/1-1.2")
    pub device_path: String,
    /// Serial port device nodes associated with this USB device.
    /// Lazily populated on first access via serial_ports() method.
    /// Stores a Result so we can cache both success and failure.
    serial_ports: OnceLock<Result<Vec<String>>>,
    // Future: other interfaces like HID, mass storage, etc.
}

impl UsbDeviceInfo {
    /// Get serial ports associated with this USB device.
    ///
    /// Scans for serial port device nodes (e.g., /dev/ttyACM0, /dev/ttyUSB0)
    /// on first call and caches the result. Returns cached value on subsequent calls.
    ///
    /// This lazy approach avoids expensive serial port enumeration for devices
    /// that won't be used (USB hubs, keyboards, etc.), only scanning when a board
    /// implementation actually needs the serial ports.
    pub fn serial_ports(&self) -> Result<&[String]> {
        self.serial_ports
            .get_or_init(|| {
                #[cfg(target_os = "linux")]
                {
                    linux::find_serial_ports_for_device(&self.device_path)
                }
                #[cfg(target_os = "macos")]
                {
                    macos::find_serial_ports_for_device(&self.device_path)
                }
                #[cfg(not(any(target_os = "linux", target_os = "macos")))]
                {
                    Ok(vec![])
                }
            })
            .as_ref()
            .map(|v| v.as_slice())
            .map_err(|e| crate::error::Error::Other(e.to_string()))
    }

    /// Create a UsbDeviceInfo for testing purposes.
    ///
    /// Serial ports are not scanned and will be empty when accessed.
    #[cfg(test)]
    pub fn new_for_test(
        vid: u16,
        pid: u16,
        serial_number: Option<String>,
        manufacturer: Option<String>,
        product: Option<String>,
        device_path: String,
    ) -> Self {
        Self {
            vid,
            pid,
            serial_number,
            manufacturer,
            product,
            device_path,
            serial_ports: OnceLock::new(),
        }
    }
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

    /// Start discovery and monitoring.
    ///
    /// Spawns a dedicated thread to monitor USB devices using the synchronous
    /// udev API. Returns immediately after spawning the thread.
    ///
    /// # Architecture
    ///
    /// USB monitoring runs in a dedicated OS thread rather than an async task
    /// because the underlying udev C library provides no thread-safety guarantees.
    /// The udev types contain raw C pointers that are !Send, meaning they cannot
    /// safely cross thread boundaries. By running the monitor in a dedicated
    /// thread, we:
    ///
    /// 1. Keep udev objects confined to a single OS thread (as libudev requires)
    /// 2. Use the simpler synchronous udev API (no async overhead)
    /// 3. Extract device info into Send types before sending via channels
    /// 4. Allow the main async runtime to spawn this without Send constraints
    ///
    /// The thread monitors the udev socket (a file descriptor) using blocking I/O,
    /// which is efficient for infrequent hotplug events. The overhead of one
    /// dedicated thread is negligible compared to the complexity of trying to
    /// make udev work with Tokio's multi-threaded work-stealing scheduler.
    ///
    /// # Shutdown
    ///
    /// The monitoring thread checks the provided `CancellationToken` and exits
    /// gracefully when cancellation is requested.
    pub async fn start_discovery(&self, shutdown: CancellationToken) -> Result<()> {
        // Create platform-specific discovery implementation
        let discovery = create_discovery()?;

        // Clone the event sender for the monitoring thread
        let event_tx = self.event_tx.clone();

        // Spawn dedicated monitoring thread
        std::thread::Builder::new()
            .name("usb-monitor".to_string())
            .spawn(move || {
                if let Err(e) = discovery.monitor_blocking(event_tx, shutdown) {
                    error!("USB monitoring failed: {}", e);
                }
            })
            .map_err(|e| {
                crate::error::Error::Other(format!("Failed to spawn USB monitor thread: {}", e))
            })?;

        Ok(())
    }
}

// Platform-specific implementations
#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "macos")]
mod macos;

/// Internal trait for platform-specific USB discovery implementations.
///
/// This trait is not exposed publicly - only `UsbTransport` uses it internally.
///
/// Implementations run in a dedicated std::thread to avoid Send issues with
/// udev's raw C pointers. The synchronous API is used since USB hotplug events
/// are infrequent and don't benefit from async overhead.
trait UsbDiscoveryImpl: Send + Sync {
    /// Monitor for USB device add/remove events (blocking).
    ///
    /// This method runs in a dedicated thread and blocks until an error occurs,
    /// the channel is closed, or shutdown is requested. It sends TransportEvent
    /// messages through the provided channel as devices are connected or
    /// disconnected.
    ///
    /// # Implementation Requirements
    ///
    /// 1. Perform initial enumeration and send Connected events for existing devices
    /// 2. Enter blocking monitoring loop for hotplug events
    /// 3. Check shutdown token periodically (at least on each event)
    /// 4. Exit gracefully on shutdown, channel closure, or fatal error
    ///
    /// # Parameters
    ///
    /// * `event_tx` - Channel for sending transport events (use `blocking_send`)
    /// * `shutdown` - Token to signal shutdown (check `is_cancelled()`)
    fn monitor_blocking(
        self: Box<Self>,
        event_tx: mpsc::Sender<super::TransportEvent>,
        shutdown: CancellationToken,
    ) -> Result<()>;
}

/// Create a platform-specific USB discovery implementation.
///
/// Returns a boxed trait object that implements USB discovery for the
/// current platform.
fn create_discovery() -> Result<Box<dyn UsbDiscoveryImpl>> {
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(linux::LinuxUdevDiscovery::new()?))
    }

    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(macos::MacOsIoKitDiscovery::new()?))
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        compile_error!("USB discovery is not implemented for this platform");
    }
}
