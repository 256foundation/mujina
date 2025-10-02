//! GPIO hardware abstraction trait.

use super::Result;
use async_trait::async_trait;

/// GPIO pin value
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinValue {
    Low,
    High,
}

impl From<bool> for PinValue {
    fn from(value: bool) -> Self {
        if value {
            PinValue::High
        } else {
            PinValue::Low
        }
    }
}

impl From<PinValue> for bool {
    fn from(value: PinValue) -> Self {
        matches!(value, PinValue::High)
    }
}

/// GPIO pin mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinMode {
    Input,
    Output,
}

/// GPIO pin abstraction
#[async_trait]
pub trait GpioPin: Send + Sync {
    /// Set the pin mode (input or output).
    async fn set_mode(&mut self, mode: PinMode) -> Result<()>;

    /// Write a value to the pin (must be in output mode).
    async fn write(&mut self, value: PinValue) -> Result<()>;

    /// Read the current value of the pin.
    async fn read(&mut self) -> Result<PinValue>;
}

/// GPIO controller abstraction
#[async_trait]
pub trait Gpio: Send + Sync {
    /// Type of pin references this controller returns
    type Pin: GpioPin;

    /// Get a reference to a specific GPIO pin.
    async fn pin(&mut self, number: u8) -> Result<Self::Pin>;
}
