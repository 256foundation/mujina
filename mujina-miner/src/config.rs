//! Configuration tree for mujina-miner.
//!
//! Populated from environment variables today. File-based layers, the
//! full source-precedence cascade, and persistence are added in later
//! increments.

/// Root of the miner's configuration tree.
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub pool: Option<PoolConfig>,
}

impl Config {
    /// Read the configuration tree from environment variables.
    pub fn from_env() -> Self {
        Self {
            pool: PoolConfig::from_env(),
        }
    }
}

/// Pool connection configuration.
///
/// Deliberately has no `Serialize` implementation: this type carries a
/// plaintext password, and the API must never be able to echo it back
/// by accident. Handlers that expose pool configuration map this to a
/// redacted view type instead. `Debug` is hand-implemented for the same
/// reason -- the derived version would print the password.
#[derive(Clone)]
pub struct PoolConfig {
    pub url: String,
    pub username: String,
    pub password: String,
    /// Whether `MUJINA_POOL_PASS` was explicitly set. `password` always
    /// has a value (falling back to a placeholder), so this is the only
    /// reliable signal for "was a password configured."
    pub password_set: bool,
}

impl PoolConfig {
    /// Read pool configuration from environment variables.
    ///
    /// Returns `None` if `MUJINA_POOL_URL` is unset, in which case the
    /// caller should fall back to a dummy job source.
    ///
    /// # Environment Variables
    ///
    /// - `MUJINA_POOL_URL`: Pool address (e.g. stratum+tcp://host:3333)
    /// - `MUJINA_POOL_USER`: Worker username (default: "mujina-testing")
    /// - `MUJINA_POOL_PASS`: Worker password (default: "x")
    fn from_env() -> Option<Self> {
        let url = std::env::var("MUJINA_POOL_URL").ok()?;
        let username =
            std::env::var("MUJINA_POOL_USER").unwrap_or_else(|_| "mujina-testing".to_string());
        let password_env = std::env::var("MUJINA_POOL_PASS").ok();
        let password_set = password_env.is_some();
        let password = password_env.unwrap_or_else(|| "x".to_string());

        Some(Self {
            url,
            username,
            password,
            password_set,
        })
    }
}

impl std::fmt::Debug for PoolConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PoolConfig")
            .field("url", &self.url)
            .field("username", &self.username)
            .field("password", &"[redacted]")
            .field("password_set", &self.password_set)
            .finish()
    }
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
        assert!(config.pool.is_none());
    }

    #[test]
    #[serial]
    fn from_env_applies_defaults_when_user_and_pass_unset() {
        // SAFETY: Test runs serially, no concurrent env access
        unsafe {
            std::env::set_var("MUJINA_POOL_URL", "stratum+tcp://pool.example:3333");
            std::env::remove_var("MUJINA_POOL_USER");
            std::env::remove_var("MUJINA_POOL_PASS");
        }

        let pool = Config::from_env().pool.expect("pool should be present");
        assert_eq!(pool.url, "stratum+tcp://pool.example:3333");
        assert_eq!(pool.username, "mujina-testing");
        assert_eq!(pool.password, "x");
        assert!(!pool.password_set);
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

        let pool = Config::from_env().pool.expect("pool should be present");
        assert_eq!(pool.username, "alice.worker1");
        assert_eq!(pool.password, "hunter2");
        assert!(pool.password_set);
    }
}
