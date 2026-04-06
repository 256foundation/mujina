//! BZM2 ASIC family support.
//!
//! The BZM2 implementation is split into focused modules:
//! - [`protocol`] owns wire-format types and the Tokio codec.
//! - [`thread`] owns the `HashThread` actor and chip bring-up sequence.
//! - [`init`] owns board-time transport probing before the hash thread takes
//!   over the UART.
//! - [`error`] contains protocol-specific validation errors.
//!
//! BIRDS boards use this module for both board-time initialization and
//! production hashing. Keeping the low-level helpers centralized avoids board
//! code having to duplicate protocol details.

use std::fmt;

/// Wrapper for formatting byte slices as space-separated uppercase hex.
pub(crate) struct HexBytes<'a>(pub(crate) &'a [u8]);

impl fmt::Display for HexBytes<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, byte) in self.0.iter().enumerate() {
            if i > 0 {
                write!(f, " ")?;
            }
            write!(f, "{:02X}", byte)?;
        }
        Ok(())
    }
}

pub mod error;
pub mod init;
pub mod protocol;
pub mod thread;

pub use error::ProtocolError;
pub use protocol::{Bzm2Protocol, Command, FrameCodec, Opcode, ReadRegData, Response};
