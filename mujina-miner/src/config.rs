//! Configuration tree for mujina-miner.
//!
//! Populated from environment variables.

use crate::stratum_v1::StratumV1PoolConfig;

/// Root of the miner's configuration tree.
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub pools: Vec<StratumV1PoolConfig>,
}

impl Config {
    /// Read the configuration tree from environment variables.
    pub fn from_env() -> Self {
        Self {
            pools: stratum_v1_pool_from_env().into_iter().collect(),
        }
    }
}

/// Read Stratum v1 pool configuration from environment variables.
///
/// Returns `None` if `MUJINA_POOL_URL` is unset, in which case the
/// caller should fall back to a dummy job source.
///
/// # Environment Variables
///
/// - `MUJINA_POOL_URL`: Pool address (e.g. stratum+tcp://host:3333)
/// - `MUJINA_POOL_USER`: Worker username (default: "mujina-testing")
/// - `MUJINA_POOL_PASS`: Worker password, if the pool requires one
fn stratum_v1_pool_from_env() -> Option<StratumV1PoolConfig> {
    let url = std::env::var("MUJINA_POOL_URL").ok()?;
    let username =
        std::env::var("MUJINA_POOL_USER").unwrap_or_else(|_| "mujina-testing".to_string());
    let password = std::env::var("MUJINA_POOL_PASS").ok();

    Some(StratumV1PoolConfig {
        url,
        username,
        password,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    #[serial]
    fn from_env_returns_none_when_url_unset() {
        // SAFETY: Test runs serially, no concurrent env access
        unsafe { std::env::remove_var("MUJINA_POOL_URL") };

        let config = Config::from_env();
        assert!(config.pools.is_empty());
    }

    #[test]
    #[serial]
    fn from_env_defaults_username_and_omits_password_when_unset() {
        // SAFETY: Test runs serially, no concurrent env access
        unsafe {
            std::env::set_var("MUJINA_POOL_URL", "stratum+tcp://pool.example:3333");
            std::env::remove_var("MUJINA_POOL_USER");
            std::env::remove_var("MUJINA_POOL_PASS");
        }

        let pools = Config::from_env().pools;
        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].url, "stratum+tcp://pool.example:3333");
        assert_eq!(pools[0].username, "mujina-testing");
        assert_eq!(pools[0].password, None);
    }

    #[test]
    #[serial]
    fn from_env_reads_username_and_password_when_set() {
        // SAFETY: Test runs serially, no concurrent env access
        unsafe {
            std::env::set_var("MUJINA_POOL_URL", "stratum+tcp://pool.example:3333");
            std::env::set_var("MUJINA_POOL_USER", "alice.worker1");
            std::env::set_var("MUJINA_POOL_PASS", "hunter2");
        }

        let pools = Config::from_env().pools;
        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].username, "alice.worker1");
        assert_eq!(pools[0].password, Some("hunter2".to_string()));
    }
}
