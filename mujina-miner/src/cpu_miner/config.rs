//! Configuration for CPU miner.
//!
//! Parses environment variables to configure the CPU mining backend.

use crate::tracing::prelude::*;

/// Upper bound on CPU mining threads.
///
/// The CPU backend exists for development and testing, so this is far
/// above any real core count; the bound keeps a typo in
/// `MUJINA_CPUMINER_THREADS` from spawning an absurd number of threads.
const MAX_THREAD_COUNT: usize = 4096;

/// CPU miner configuration parsed from environment variables.
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

impl CpuMinerConfig {
    /// Parse configuration from environment variables.
    ///
    /// Returns `Some(config)` if `MUJINA_CPUMINER_THREADS` is set,
    /// `None` otherwise.
    ///
    /// # Environment Variables
    ///
    /// - `MUJINA_CPUMINER_THREADS`: Number of threads (presence enables CPU mining)
    /// - `MUJINA_CPUMINER_DUTY`: Duty cycle % (default: 50, clamped to 1-100)
    pub fn from_env() -> Option<Self> {
        let thread_count: usize = std::env::var("MUJINA_CPUMINER_THREADS")
            .ok()
            .and_then(|s| s.parse().ok())?;

        if thread_count > MAX_THREAD_COUNT {
            warn!(
                requested = thread_count,
                max = MAX_THREAD_COUNT,
                "MUJINA_CPUMINER_THREADS clamped"
            );
        }
        let thread_count = thread_count.min(MAX_THREAD_COUNT);

        let duty_percent = std::env::var("MUJINA_CPUMINER_DUTY")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(50)
            .clamp(1, 100);

        Some(Self {
            thread_count,
            duty_percent,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    #[serial]
    fn test_from_env_disabled_when_not_set() {
        // SAFETY: Test runs serially, no concurrent env access
        unsafe { std::env::remove_var("MUJINA_CPUMINER_THREADS") };

        let config = CpuMinerConfig::from_env();
        assert!(config.is_none());
    }

    #[test]
    #[serial]
    fn test_duty_clamped_to_valid_range() {
        // SAFETY: Test runs serially, no concurrent env access
        unsafe {
            // Upper bound: 150 -> 100
            std::env::set_var("MUJINA_CPUMINER_THREADS", "99");
            std::env::set_var("MUJINA_CPUMINER_DUTY", "150");
        }
        if let Some(config) = CpuMinerConfig::from_env() {
            assert_eq!(config.duty_percent, 100);
        }

        // SAFETY: Test runs serially, no concurrent env access
        unsafe {
            // Lower bound: 0 -> 1
            std::env::set_var("MUJINA_CPUMINER_THREADS", "99");
            std::env::set_var("MUJINA_CPUMINER_DUTY", "0");
        }
        if let Some(config) = CpuMinerConfig::from_env() {
            assert_eq!(config.duty_percent, 1);
        }
    }

    #[test]
    #[serial]
    fn test_thread_count_clamped_to_max() {
        // SAFETY: Test runs serially, no concurrent env access
        unsafe {
            std::env::set_var("MUJINA_CPUMINER_THREADS", "100000");
        }

        let config = CpuMinerConfig::from_env().unwrap();
        assert_eq!(config.thread_count, MAX_THREAD_COUNT);

        // SAFETY: Test runs serially, no concurrent env access
        unsafe { std::env::remove_var("MUJINA_CPUMINER_THREADS") };
    }
}
