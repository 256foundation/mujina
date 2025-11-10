//! Board management protocol implementations.
//!
//! This module provides protocol implementations for managing hash boards,
//! such as bitaxe-raw protocol. These protocols handle GPIO control, I2C
//! passthrough, ADC readings, and other board management functions.

pub mod bitaxe_raw;

// Re-export commonly used types
pub use bitaxe_raw::channel::ControlChannel;
pub use bitaxe_raw::gpio::{BitaxeRawGpioController, BitaxeRawGpioPin};
