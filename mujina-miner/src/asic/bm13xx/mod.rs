//! BM13xx family chip support.
//!
//! This module provides protocol implementation and utilities for
//! communicating with BM13xx series mining chips (BM1366, BM1370, etc).

pub mod chain;
pub mod chip_config;
pub mod codec;
pub mod command;
pub mod crc;
pub mod error;
pub mod register;
pub mod response;
pub mod thread;
pub mod topology;

#[cfg(test)]
pub mod test_data;

// Re-export commonly used types
pub use codec::FrameCodec;
pub use register::Register;
pub use response::Response;
