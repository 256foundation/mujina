//! Error types for BZM2 protocol operations.

use thiserror::Error;

#[derive(Error, Debug)]
pub enum ProtocolError {
    #[error("register write payload cannot be empty")]
    EmptyWritePayload,

    #[error("register write payload too large: {0} bytes")]
    WritePayloadTooLarge(usize),

    #[error("invalid read register byte count: {0} (expected 1, 2, or 4)")]
    InvalidReadRegCount(u8),

    #[error("invalid job control value: {0} (expected 1 or 3)")]
    InvalidJobControl(u8),

    #[error("unsupported read register response size: {0} (expected 1 or 4)")]
    UnsupportedReadRegResponseSize(usize),

    #[error("frame too large to encode: {0} bytes")]
    FrameTooLarge(usize),

    #[error("invalid NOOP signature: {0:02x?}")]
    InvalidNoopSignature([u8; 3]),

    #[error("unsupported response opcode: 0x{0:02x}")]
    UnsupportedResponseOpcode(u8),
}
