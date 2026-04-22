//! Runtime configuration for the CPU miner board.

/// Configuration for a CPU miner board instance.
#[derive(Debug, Clone)]
pub struct CpuMinerConfig {
    /// Number of mining threads to spawn.
    pub thread_count: usize,

    /// Target CPU duty cycle percentage (1-100).
    ///
    /// Controls duty cycling: at 80%, each thread hashes for 800ms then
    /// sleeps for 200ms per second. Useful for avoiding alerts on cloud
    /// instances that monitor for sustained CPU usage.
    pub duty_percent: u8,
}
