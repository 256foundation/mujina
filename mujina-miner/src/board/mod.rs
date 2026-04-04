pub(crate) mod bitaxe;
pub(crate) mod cpu;
pub(crate) mod emberone00;
pub mod pattern;

use anyhow::Result;
use futures::future::BoxFuture;
use tokio::sync::watch;

use crate::{
    api_client::types::BoardTelemetry, asic::hash_thread::HashThread, transport::UsbDeviceInfo,
};

/// Returned by board factory functions with everything the backplane
/// needs to integrate a board into the system.
pub struct BackplaneConnector {
    /// Board identification and metadata.
    pub info: BoardInfo,

    /// Hash threads ready to be scheduled.
    pub threads: Vec<Box<dyn HashThread>>,

    /// Watch receiver for the board's telemetry stream.
    pub telemetry_rx: watch::Receiver<BoardTelemetry>,

    /// Shuts down the board when awaited. `None` if the board has
    /// no shutdown work to do.
    pub shutdown: Option<BoxFuture<'static, ()>>,
}

/// Information about a board.
#[derive(Debug, Clone)]
pub struct BoardInfo {
    /// Board model/type (e.g., "Bitaxe Gamma")
    pub model: String,
    /// Board firmware version if available
    pub firmware_version: Option<String>,
    /// Serial number if available
    pub serial_number: Option<String>,
}

/// Factory function signature for creating a board from USB device info.
///
/// The factory is responsible for:
///
/// 1. Opening hardware resources (serial ports, etc.)
/// 2. Creating a `watch::channel<BoardTelemetry>` seeded with the
///    board's identity (model, serial)
/// 3. Initializing the board hardware
/// 4. Creating hash threads
/// 5. Returning a [`BackplaneConnector`] with all of the above
///
/// The backplane calls the factory when a matching USB device is
/// discovered.
pub type BoardFactoryFn = fn(UsbDeviceInfo) -> BoxFuture<'static, Result<BackplaneConnector>>;

/// Board descriptor that gets collected by inventory.
///
/// Board implementors use `inventory::submit!` to register their board type
/// with the system. The backplane will automatically discover all registered
/// boards at runtime.
///
/// ## Pattern Matching
///
/// Each descriptor includes a pattern that specifies which devices it can handle.
/// When multiple descriptors match a device, the one with the highest specificity
/// score is selected. This allows generic fallback handlers while ensuring
/// specific boards are matched correctly.
pub struct BoardDescriptor {
    /// Pattern for matching USB devices
    pub pattern: pattern::BoardPattern,
    /// Human-readable board name (e.g., "Bitaxe Gamma")
    pub name: &'static str,
    /// Factory function to create the board from USB device info
    pub create_fn: BoardFactoryFn,
}

// This creates the inventory collection for board descriptors
inventory::collect!(BoardDescriptor);

/// Factory function signature for creating a virtual board.
///
/// Same contract as [`BoardFactoryFn`], but virtual boards don't
/// receive USB device info. They are configured via environment
/// variables or other means.
pub type VirtualBoardFactoryFn = fn() -> BoxFuture<'static, Result<BackplaneConnector>>;

/// Descriptor for virtual boards (CPU miner, test boards, etc.).
///
/// Virtual boards are registered via `inventory::submit!` like USB boards,
/// but match on a device type string rather than USB patterns.
pub struct VirtualBoardDescriptor {
    /// Device type identifier (e.g., "cpu_miner")
    pub device_type: &'static str,
    /// Human-readable board name (e.g., "CPU Miner")
    pub name: &'static str,
    /// Factory function to create the board
    pub create_fn: VirtualBoardFactoryFn,
}

inventory::collect!(VirtualBoardDescriptor);

/// Registry for virtual board descriptors.
pub struct VirtualBoardRegistry;

impl VirtualBoardRegistry {
    /// Find a virtual board descriptor by device type.
    pub fn find(&self, device_type: &str) -> Option<&'static VirtualBoardDescriptor> {
        inventory::iter::<VirtualBoardDescriptor>().find(|desc| desc.device_type == device_type)
    }
}
