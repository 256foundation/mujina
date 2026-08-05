//! Configuration tree for mujina-miner.
//!
//! Populated from environment variables.

use crate::stratum_v1::StratumV1PoolConfig;

/// Root of the miner's configuration tree.
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub sources: Vec<SourceKind>,
}

impl Config {
    /// Read the configuration tree from environment variables.
    pub fn from_env() -> Self {
        Self {
            sources: stratum_v1_pool_from_env()
                .into_iter()
                .map(SourceKind::StratumV1)
                .collect(),
        }
    }
}

/// What kind of job source this is.
///
/// Tagged by `kind` rather than assuming every source is a pool.
/// "Source" is already the vocabulary the rest of the codebase uses
/// for this concept (the `job_source` module, `SourceRegistration`,
/// `SourceEvent`), and it covers more than pool connections: the dummy
/// source used when none is configured, and, over time, other
/// protocols (Stratum v2 and whatever comes after). `StratumV1` is the
/// only variant today; adding a source kind means adding a variant
/// here, not reshaping this one.
#[derive(Debug, Clone)]
pub enum SourceKind {
    StratumV1(StratumV1PoolConfig),
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
        assert!(config.sources.is_empty());
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

        let sources = Config::from_env().sources;
        assert_eq!(sources.len(), 1);
        let SourceKind::StratumV1(pool) = &sources[0];
        assert_eq!(pool.url, "stratum+tcp://pool.example:3333");
        assert_eq!(pool.username, "mujina-testing");
        assert_eq!(pool.password, None);
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

        let sources = Config::from_env().sources;
        assert_eq!(sources.len(), 1);
        let SourceKind::StratumV1(pool) = &sources[0];
        assert_eq!(pool.username, "alice.worker1");
        assert_eq!(pool.password, Some("hunter2".to_string()));
    }
}
