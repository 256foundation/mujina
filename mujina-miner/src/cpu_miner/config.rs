//! Configuration for CPU miner.
//!
//! Parses environment variables to configure the CPU mining backend.

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
        let thread_count = std::env::var("MUJINA_CPUMINER_THREADS")
            .ok()
            .and_then(|s| s.parse().ok())?;

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
        std::env::remove_var("MUJINA_CPUMINER_THREADS");

        let config = CpuMinerConfig::from_env();
        assert!(config.is_none());
    }

    #[test]
    #[serial]
    fn test_duty_clamped_to_valid_range() {
        // Upper bound: 150 -> 100
        std::env::set_var("MUJINA_CPUMINER_THREADS", "99");
        std::env::set_var("MUJINA_CPUMINER_DUTY", "150");
        if let Some(config) = CpuMinerConfig::from_env() {
            assert_eq!(config.duty_percent, 100);
        }

        // Lower bound: 0 -> 1
        std::env::set_var("MUJINA_CPUMINER_THREADS", "99");
        std::env::set_var("MUJINA_CPUMINER_DUTY", "0");
        if let Some(config) = CpuMinerConfig::from_env() {
            assert_eq!(config.duty_percent, 1);
        }
    }
}
