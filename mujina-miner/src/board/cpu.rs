//! CPU mining board implementation.
//!
//! Provides a virtual board that uses CPU cores for SHA-256 hashing.
//! Configured via environment variables, creates one HashThread per core.

use async_trait::async_trait;

use super::{Board, BoardError, BoardInfo, VirtualBoardDescriptor};
use crate::{
    asic::hash_thread::HashThread,
    cpu_miner::{CpuHashThread, CpuMinerConfig},
};

/// CPU mining board.
///
/// A virtual board that spawns CPU-based mining threads. Unlike hardware
/// boards, this doesn't require any physical devices---it's configured
/// entirely via environment variables.
pub struct CpuBoard {
    /// Configuration parsed from environment.
    config: CpuMinerConfig,

    /// Threads created by this board (kept for shutdown).
    threads: Vec<CpuHashThread>,
}

impl CpuBoard {
    /// Create a new CPU mining board from environment configuration.
    pub fn new(config: CpuMinerConfig) -> Self {
        Self {
            config,
            threads: Vec::new(),
        }
    }
}

#[async_trait]
impl Board for CpuBoard {
    fn board_info(&self) -> BoardInfo {
        BoardInfo {
            model: "CPU Miner".into(),
            firmware_version: None,
            serial_number: Some(format!(
                "cpu-{}x{}%",
                self.config.thread_count, self.config.duty_percent
            )),
        }
    }

    async fn shutdown(&mut self) -> Result<(), BoardError> {
        // Signal all threads to stop
        for thread in &self.threads {
            thread.shutdown();
        }
        self.threads.clear();
        Ok(())
    }

    async fn create_hash_threads(&mut self) -> Result<Vec<Box<dyn HashThread>>, BoardError> {
        let mut threads: Vec<Box<dyn HashThread>> = Vec::new();

        for i in 0..self.config.thread_count {
            let thread = CpuHashThread::new(format!("CPU Core {}", i), self.config.duty_percent);
            threads.push(Box::new(thread));
        }

        Ok(threads)
    }
}

// ---------------------------------------------------------------------------
// Virtual board registration
// ---------------------------------------------------------------------------

/// Factory function for creating CpuBoard instances.
async fn create_cpu_board() -> crate::error::Result<Box<dyn Board + Send>> {
    let config = CpuMinerConfig::from_env().ok_or_else(|| {
        crate::error::Error::Config("CPU miner not configured (MUJINA_CPU_MINER not set)".into())
    })?;

    Ok(Box::new(CpuBoard::new(config)))
}

inventory::submit! {
    VirtualBoardDescriptor {
        device_type: "cpu_miner",
        name: "CPU Miner",
        create_fn: || Box::pin(create_cpu_board()),
    }
}
