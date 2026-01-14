pub(crate) mod bitaxe;
pub mod cpu;
pub(crate) mod emberone;
pub mod pattern;

use async_trait::async_trait;
use std::{error::Error, fmt, future::Future, pin::Pin};

use crate::{asic::hash_thread::HashThread, transport::UsbDeviceInfo};

/// Represents a mining board containing one or more ASIC chips.
///
/// A board manages hardware peripherals (power, cooling, monitoring) and
/// creates hash threads that handle chip communication. The backplane creates
/// boards via factory functions and manages their lifecycle through this trait.
#[async_trait]
pub trait Board: Send {
    /// Board identification and metadata.
    fn board_info(&self) -> BoardInfo;

    /// Gracefully shutdown the board.
    ///
    /// This should stop all mining activity and put the hardware in a safe
    /// state. The exact implementation is board-specific but typically includes
    /// stopping hashing and ensuring chips are in a low-power or reset state.
    async fn shutdown(&mut self) -> Result<(), BoardError>;

    /// Create hash threads for this board.
    ///
    /// Transfers serial channel ownership to threads. Board retains peripheral
    /// control (power, cooling, monitoring) and thread shutdown authority.
    ///
    /// Board-to-thread shutdown is implementation-specific (not exposed through
    /// HashThread trait). Call board.shutdown() to trigger thread shutdown.
    async fn create_hash_threads(&mut self) -> Result<Vec<Box<dyn HashThread>>, BoardError>;
}

/// Information about a board
#[derive(Debug, Clone)]
pub struct BoardInfo {
    /// Board model/type (e.g., "Bitaxe Gamma")
    pub model: String,
    /// Board firmware version if available
    pub firmware_version: Option<String>,
    /// Serial number if available
    pub serial_number: Option<String>,
}

/// Board-specific errors
#[derive(Debug)]
pub enum BoardError {
    /// Hardware initialization failed
    InitializationFailed(String),
    /// Communication error with board
    Communication(std::io::Error),
    /// GPIO or hardware control error
    HardwareControl(String),
}

impl fmt::Display for BoardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BoardError::InitializationFailed(msg) => {
                write!(f, "Board initialization failed: {}", msg)
            }
            BoardError::Communication(err) => write!(f, "Board communication error: {}", err),
            BoardError::HardwareControl(msg) => write!(f, "Hardware control error: {}", msg),
        }
    }
}

impl Error for BoardError {}

impl From<std::io::Error> for BoardError {
    fn from(err: std::io::Error) -> Self {
        BoardError::Communication(err)
    }
}

/// Helper type for async board factory functions
type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Type alias for board factory function
pub type BoardFactoryFn =
    fn(UsbDeviceInfo) -> BoxFuture<'static, crate::error::Result<Box<dyn Board + Send>>>;

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

// ---------------------------------------------------------------------------
// Virtual board support (CPU miner, test boards, etc.)
// ---------------------------------------------------------------------------

/// Type alias for virtual board factory function.
///
/// Unlike USB boards, virtual boards don't receive device info---they're
/// configured via environment variables or other means.
pub type VirtualBoardFactoryFn =
    fn() -> BoxFuture<'static, crate::error::Result<Box<dyn Board + Send>>>;

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
