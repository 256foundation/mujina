pub(crate) mod bitaxe;
pub(crate) mod bzm2;
pub mod cpu;
pub(crate) mod emberone;
pub mod pattern;

use anyhow::Result;
use async_trait::async_trait;
use std::{error::Error, fmt, future::Future, pin::Pin};
use tokio::sync::{mpsc, oneshot, watch};

use crate::{
    api_client::types::BoardState, asic::hash_thread::HashThread, transport::UsbDeviceInfo,
};

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
    async fn shutdown(&mut self) -> Result<()>;

    /// Create hash threads for this board.
    ///
    /// Transfers serial channel ownership to threads. Board retains peripheral
    /// control (power, cooling, monitoring) and thread shutdown authority.
    ///
    /// Board-to-thread shutdown is implementation-specific (not exposed through
    /// HashThread trait). Call board.shutdown() to trigger thread shutdown.
    async fn create_hash_threads(&mut self) -> Result<Vec<Box<dyn HashThread>>>;
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

/// Registration data returned by board factory functions.
///
/// Bundles the channels needed for the rest of the system to communicate
/// with a board. The backplane forwards this to the API server after
/// creating a board.
pub struct BoardRegistration {
    /// Watch receiver for the board's current state.
    pub state_rx: watch::Receiver<BoardState>,

    /// Optional command sender for board-specific control and queries.
    pub command_tx: Option<mpsc::Sender<BoardCommand>>,
}

/// Board-specific control and query commands exposed to higher layers.
pub enum BoardCommand {
    /// Query one BZM2 ASIC's internal DTS/VS telemetry through a live hash thread.
    QueryBzm2DtsVs {
        thread_index: usize,
        asic: u8,
        reply: oneshot::Sender<Result<(), BoardError>>,
    },
    QueryBzm2Noop {
        thread_index: usize,
        asic: u8,
        reply: oneshot::Sender<Result<[u8; 3], BoardError>>,
    },
    QueryBzm2Loopback {
        thread_index: usize,
        asic: u8,
        payload: Vec<u8>,
        reply: oneshot::Sender<Result<Vec<u8>, BoardError>>,
    },
    ReadBzm2Register {
        thread_index: usize,
        asic: u8,
        engine_address: u16,
        offset: u8,
        count: u8,
        reply: oneshot::Sender<Result<Vec<u8>, BoardError>>,
    },
    WriteBzm2Register {
        thread_index: usize,
        asic: u8,
        engine_address: u16,
        offset: u8,
        value: Vec<u8>,
        reply: oneshot::Sender<Result<(), BoardError>>,
    },
    DiscoverBzm2Engines {
        thread_index: usize,
        asic: u8,
        tdm_prediv_raw: u32,
        tdm_counter: u8,
        timeout_ms: Option<u32>,
        reply: oneshot::Sender<Result<(), BoardError>>,
    },
}

/// Helper type for async board factory functions
type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Factory function signature for creating a board from USB device info.
///
/// The factory is responsible for:
///
/// 1. Opening hardware resources (serial ports, etc.)
/// 2. Creating a `watch::channel<BoardState>` seeded with the board's
///    identity (model, serial) and storing the sender in the board
/// 3. Initializing the board hardware
/// 4. Returning the board and a [`BoardRegistration`] containing the
///    watch receiver
///
/// The backplane calls the factory when a matching USB device is
/// discovered, then forwards the [`BoardRegistration`] to the API
/// server.
pub type BoardFactoryFn =
    fn(UsbDeviceInfo) -> BoxFuture<'static, Result<(Box<dyn Board + Send>, BoardRegistration)>>;

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

/// Factory function signature for creating a virtual board.
///
/// Same contract as [`BoardFactoryFn`] (create watch channel, seed with
/// identity, return [`BoardRegistration`]), but virtual boards don't
/// receive USB device info---they're configured via environment
/// variables or other means.
pub type VirtualBoardFactoryFn =
    fn() -> BoxFuture<'static, Result<(Box<dyn Board + Send>, BoardRegistration)>>;

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
