//! I2C hardware abstraction trait.

use super::Result;
use async_trait::async_trait;

/// I2C-specific errors
#[derive(Debug, thiserror::Error)]
pub enum I2cError {
    /// No acknowledgment from device
    #[error("No acknowledgment from device at address 0x{0:02x}")]
    NoAck(u8),

    /// Bus arbitration lost
    #[error("Bus arbitration lost")]
    ArbitrationLost,

    /// Bus error
    #[error("Bus error")]
    BusError,

    /// Other I2C error
    #[error("I2C error: {0}")]
    Other(String),
}

/// I2C bus abstraction
#[async_trait]
pub trait I2c: Send + Sync {
    /// Write data to an I2C device.
    async fn write(&mut self, addr: u8, data: &[u8]) -> Result<()>;

    /// Read data from an I2C device.
    async fn read(&mut self, addr: u8, buffer: &mut [u8]) -> Result<()>;

    /// Write data then read from an I2C device (repeated start).
    async fn write_read(&mut self, addr: u8, write: &[u8], read: &mut [u8]) -> Result<()>;

    /// Set the I2C bus frequency in Hz.
    async fn set_frequency(&mut self, hz: u32) -> Result<()>;
}
