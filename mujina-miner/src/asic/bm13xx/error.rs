//! Error types for BM13xx protocol operations

use thiserror::Error;

#[derive(Error, Debug)]
pub enum ProtocolError {
    #[error("Invalid register address: 0x{0:02x}")]
    InvalidRegisterAddress(u8),

    #[error("Invalid response type: 0x{0:02x}")]
    InvalidResponseType(u8),

    #[error("Cannot write to read-only register: {0:?}")]
    ReadOnlyRegister(super::protocol::RegisterAddress),

    #[error("Invalid frame format")]
    InvalidFrame,

    #[error("Buffer too small: need {need} bytes, have {have}")]
    BufferTooSmall { need: usize, have: usize },

    #[error("Invalid frequency: {mhz} MHz (must be between 50-800 MHz)")]
    InvalidFrequency { mhz: u32 },
}
