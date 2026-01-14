//! CPU mining backend.
//!
//! Provides a virtual mining board that uses CPU cores for SHA-256 hashing.
//! Useful for testing and development without physical ASIC hardware.
//!
//! # Configuration
//!
//! Enable via environment variables:
//!
//! - `MUJINA_CPUMINER_THREADS=N` - Number of mining threads (presence enables)
//! - `MUJINA_CPUMINER_DUTY=P` - Duty cycle percentage (default: 50)

mod config;
mod hasher;
mod thread;

pub use config::CpuMinerConfig;
pub use thread::CpuHashThread;
