//! Error types for BZM2 protocol operations.

use thiserror::Error;

/// Validation failures detected while encoding or decoding BZM2 frames.
#[derive(Error, Debug)]
pub enum ProtocolError {
    /// A register write command was constructed without any payload bytes.
    #[error("register write payload cannot be empty")]
    EmptyWritePayload,

    /// A register write payload exceeded the 8-bit on-wire length field.
    #[error("register write payload too large: {0} bytes")]
    WritePayloadTooLarge(usize),

    /// READREG only supports 1-, 2-, or 4-byte responses.
    #[error("invalid read register byte count: {0} (expected 1, 2, or 4)")]
    InvalidReadRegCount(u8),

    /// WRITEJOB only accepts `job_ctl` values that the hardware understands.
    #[error("invalid job control value: {0} (expected 1 or 3)")]
    InvalidJobControl(u8),

    /// The codec was asked to decode a READREG response size it does not
    /// implement.
    #[error("unsupported read register response size: {0} (expected 1 or 4)")]
    UnsupportedReadRegResponseSize(usize),

    /// A frame exceeded what the bridge format can encode in one command.
    #[error("frame too large to encode: {0} bytes")]
    FrameTooLarge(usize),

    /// A NOOP response did not return the expected `2ZB` signature bytes.
    #[error("invalid NOOP signature: {0:02x?}")]
    InvalidNoopSignature([u8; 3]),

    /// The decoder saw a response opcode that is not currently supported.
    #[error("unsupported response opcode: 0x{0:02x}")]
    UnsupportedResponseOpcode(u8),
}
