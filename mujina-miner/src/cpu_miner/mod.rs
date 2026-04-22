//! CPU mining backend.
//!
//! Provides a virtual mining board that uses CPU cores for SHA-256 hashing.
//! Useful for testing and development without physical ASIC hardware.
//!
//! # Configuration
//!
//! Enable via the config system (see `docs/configuration.md`):
//!
//! ```yaml
//! boards:
//!   cpu_miner:
//!     enabled: true
//!     threads: 2
//!     duty_percent: 50
//! ```

mod config;
mod hasher;
mod thread;

pub use config::CpuMinerConfig;
pub use thread::CpuHashThread;
