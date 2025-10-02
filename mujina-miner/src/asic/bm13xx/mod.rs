//! BM13xx family chip support.
//!
//! This module provides protocol implementation and utilities for
//! communicating with BM13xx series mining chips (BM1366, BM1370, etc).

pub mod crc;
pub mod error;
pub mod protocol;

// Re-export commonly used types
pub use protocol::{FrameCodec, Register, Response};

// Re-export the protocol handler
pub use protocol::BM13xxProtocol;
