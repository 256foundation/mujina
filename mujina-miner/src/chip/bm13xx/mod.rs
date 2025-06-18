//! BM13xx family chip support.
//!
//! This module provides protocol implementation and utilities for
//! communicating with BM13xx series mining chips (BM1366, BM1370, etc).

pub mod protocol;
pub(super) mod crc;  // Make visible to protocol module

// Re-export commonly used types from protocol
pub use protocol::{
    Response,
    Register,
    FrameCodec,
};

// Re-export the protocol handler
pub use protocol::BM13xxProtocol;