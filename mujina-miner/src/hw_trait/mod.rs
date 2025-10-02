//! Hardware abstraction layer traits.
//!
//! This module defines the core hardware interface traits (I2C, SPI, GPIO,
//! Serial) that allow drivers to work with different underlying
//! implementations, whether direct Linux hardware access or tunneled
//! through management protocols.

pub mod adc;
pub mod gpio;
pub mod i2c;

// Re-export traits
pub use adc::{Adc, AdcChannel};
pub use gpio::{Gpio, GpioPin, PinMode, PinValue};
pub use i2c::{I2c, I2cError};

/// Common error type for hardware operations
#[derive(Debug, thiserror::Error)]
pub enum HwError {
    /// I/O error from underlying transport
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// I2C-specific errors
    #[error("I2C error: {0}")]
    I2c(#[from] I2cError),

    /// Invalid parameter or argument
    #[error("Invalid parameter: {0}")]
    InvalidParameter(String),

    /// Operation not supported by hardware
    #[error("Operation not supported: {0}")]
    NotSupported(String),

    /// Timeout waiting for hardware response
    #[error("Hardware timeout")]
    Timeout,

    /// Other hardware-specific error
    #[error("Hardware error: {0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, HwError>;
