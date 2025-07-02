pub(crate) mod bitaxe;

use async_trait::async_trait;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use tokio::sync::mpsc;

use crate::asic::{ChipError, ChipInfo, MiningJob, NonceResult};
use crate::transport::UsbDeviceInfo;

/// Events emitted by a board during operation.
#[derive(Debug, Clone)]
pub enum BoardEvent {
    /// A chip found a valid nonce
    NonceFound(NonceResult),

    /// A mining job has completed
    JobComplete {
        job_id: u64,
        reason: JobCompleteReason,
    },

    /// An error occurred with a specific chip
    ChipError {
        chip_address: u8,
        error: String, // String because ChipError might not be Clone
    },

    /// A chip's status changed (e.g., temperature, frequency)
    ChipStatusUpdate {
        chip_address: u8,
        temperature_c: Option<f32>,
        frequency_mhz: Option<u32>,
    },
}

/// Reasons why a mining job completed.
#[derive(Debug, Clone, Copy)]
pub enum JobCompleteReason {
    /// The chip reported that it finished searching its nonce range
    RangeExhausted,

    /// The board estimated completion based on elapsed time
    TimeoutEstimate,

    /// The job was cancelled by the user
    Cancelled,

    /// A new job was sent, implicitly completing the previous one
    Superseded,
}

/// Represents a mining board containing one or more ASIC chips.
///
/// A board provides the interface between the host system and mining chips,
/// handling hardware initialization, reset, chip discovery, and mining operations.
///
/// Board implementations communicate asynchronously via an event stream rather
/// than polling, allowing efficient handling of nonces and job completion.
#[async_trait]
pub trait Board: Send {
    /// Reset the board hardware.
    ///
    /// This typically involves toggling GPIO pins or sending reset commands
    /// to bring the board to a known state.
    async fn reset(&mut self) -> Result<(), BoardError>;

    /// Hold the board's chips in reset state.
    ///
    /// This keeps the reset line asserted (low) to ensure chips remain
    /// disabled. Used during shutdown to ensure a safe state.
    async fn hold_in_reset(&mut self) -> Result<(), BoardError>;

    /// Initialize the board and discover connected chips.
    ///
    /// After initialization, the board is ready to receive mining jobs.
    /// Returns a receiver for board events.
    async fn initialize(&mut self) -> Result<mpsc::Receiver<BoardEvent>, BoardError>;

    /// Get the number of discovered chips on this board.
    fn chip_count(&self) -> usize;

    /// Get information about discovered chips.
    fn chip_infos(&self) -> &[ChipInfo];

    /// Send a mining job to all chips on this board.
    ///
    /// Each chip will work on the same job but search different nonce ranges
    /// automatically based on their internal core architecture.
    ///
    /// The job completion will be reported via the event stream as
    /// `BoardEvent::JobComplete`.
    async fn send_job(&mut self, job: &MiningJob) -> Result<(), BoardError>;

    /// Cancel the current mining job.
    ///
    /// This will trigger a `BoardEvent::JobComplete` with reason `Cancelled`.
    async fn cancel_job(&mut self, job_id: u64) -> Result<(), BoardError>;

    /// Get board identification/info
    fn board_info(&self) -> BoardInfo;

    /// Get the event receiver for this board.
    ///
    /// This should be called after initialization to receive board events.
    /// Returns None if the board hasn't been initialized yet.
    fn take_event_receiver(&mut self) -> Option<mpsc::Receiver<BoardEvent>>;
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
    /// Chip-related error
    Chip(ChipError),
    /// GPIO or hardware control error
    HardwareControl(String),
    /// Other error
    Other(Box<dyn Error + Send + Sync>),
}

impl fmt::Display for BoardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BoardError::InitializationFailed(msg) => {
                write!(f, "Board initialization failed: {}", msg)
            }
            BoardError::Communication(err) => write!(f, "Board communication error: {}", err),
            BoardError::Chip(err) => write!(f, "Chip error: {}", err),
            BoardError::HardwareControl(msg) => write!(f, "Hardware control error: {}", msg),
            BoardError::Other(err) => write!(f, "Board error: {}", err),
        }
    }
}

impl Error for BoardError {}

impl From<std::io::Error> for BoardError {
    fn from(err: std::io::Error) -> Self {
        BoardError::Communication(err)
    }
}

impl From<ChipError> for BoardError {
    fn from(err: ChipError) -> Self {
        BoardError::Chip(err)
    }
}

/// Helper type for async board factory functions
type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Board descriptor that gets collected by inventory.
///
/// Board implementors use `inventory::submit!` to register their board type
/// with the system. The board manager will automatically discover all registered
/// boards at runtime.
pub struct BoardDescriptor {
    /// USB vendor ID this board handles
    pub vid: u16,
    /// USB product ID this board handles
    pub pid: u16,
    /// Human-readable board name (e.g., "Bitaxe Gamma")
    pub name: &'static str,
    /// Factory function to create the board from USB device info
    pub create_fn:
        fn(UsbDeviceInfo) -> BoxFuture<'static, crate::error::Result<Box<dyn Board + Send>>>,
}

// This creates the inventory collection for board descriptors
inventory::collect!(BoardDescriptor);
