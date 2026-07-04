//! Error types for BM13xx protocol operations

use thiserror::Error;

#[derive(Error, Debug)]
pub enum ProtocolError {
    #[error("Invalid register address: 0x{0:02x}")]
    InvalidRegisterAddress(u8),

    #[error("Invalid response type: 0x{0:02x}")]
    InvalidResponseType(u8),

    #[error("Cannot write to read-only register: {0:?}")]
    ReadOnlyRegister(super::register::RegisterAddress),

    #[error("Invalid frame format")]
    InvalidFrame,

    #[error("Unknown chip id: {:02x}{:02x}", .0[0], .0[1])]
    UnknownChipId([u8; 2]),

    #[error("Buffer too small: need {need} bytes, have {have}")]
    BufferTooSmall { need: usize, have: usize },
}
