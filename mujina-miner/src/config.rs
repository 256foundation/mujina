//! Configuration management for mujina-miner.
//!
//! Loads configuration from multiple sources in priority order (highest wins):
//!
//! 1. CLI flags (caller merges these on top after calling `Config::load`)
//! 2. Environment variables — prefix `MUJINA`, separator `__`
//!    e.g. `MUJINA__POOL__URL=stratum+tcp://pool.example.com:3333`
//! 3. Config file specified via `--config` (passed as `cli_config_path`)
//! 4. Default config file — `/etc/mujina/mujina.yaml` (optional, not required)
//! 5. Hard-coded defaults — `Default` impls on each struct (lowest priority)
//!
//! See `docs/configuration.md` and `configs/mujina.example.yaml` for the full
//! key reference.

use std::path::PathBuf;

use config::{Environment, File, FileFormat};
use serde::{Deserialize, Serialize};
use tracing::debug;

const DEFAULT_CONFIG_PATH: &str = "/etc/mujina/mujina.yaml";
const ENV_PREFIX: &str = "MUJINA";
const ENV_SEPARATOR: &str = "__";

// ---------------------------------------------------------------------------
// Top-level config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub daemon: DaemonConfig,
    pub api: ApiConfig,
    pub pool: PoolConfig,
    pub backplane: BackplaneConfig,
    pub boards: BoardsConfig,
}

// ---------------------------------------------------------------------------
// Subsection structs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    pub log_level: String,
    pub pid_file: Option<PathBuf>,
    pub systemd: bool,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            log_level: "info".to_string(),
            pid_file: None,
            systemd: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApiConfig {
    pub listen: String,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:7785".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PoolConfig {
    pub url: Option<String>,
    pub user: String,
    pub password: String,
    /// Target share rate in shares per minute for the forced-rate wrapper.
    /// When set, overrides the pool's share target to achieve this rate.
    /// Intended for CPU mining tests against pools that set difficulty too
    /// high for software hashers. `None` disables the wrapper.
    pub forced_rate: Option<f64>,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            url: None,
            user: "mujina-testing".to_string(),
            password: "x".to_string(),
            forced_rate: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct BackplaneConfig {
    pub usb_enabled: bool,
}

impl Default for BackplaneConfig {
    fn default() -> Self {
        Self { usb_enabled: true }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct BoardsConfig {
    pub cpu_miner: CpuMinerConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CpuMinerConfig {
    pub enabled: bool,
    pub threads: usize,
    pub duty_percent: u8,
}

impl Default for CpuMinerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            threads: 1,
            duty_percent: 50,
        }
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

impl Config {
    /// Load configuration using the standard source hierarchy.
    ///
    /// Equivalent to `Config::load_with(None, &[])`. Use when no `--config`
    /// flag or `--set` overrides were supplied on the command line.
    pub fn load() -> anyhow::Result<Self> {
        Self::load_with(None, &[])
    }

    /// Load configuration with an optional config file and `--set` overrides.
    ///
    /// Priority order (highest wins):
    /// 1. `overrides` — `--set key=value` pairs, applied in order
    /// 2. `MUJINA__*` environment variables
    /// 3. `cli_config_path` — `--config` file, required to exist if supplied
    /// 4. `/etc/mujina/mujina.yaml` — optional system default
    /// 5. Hard-coded `Default` impls
    pub fn load_with(
        cli_config_path: Option<PathBuf>,
        overrides: &[(String, String)],
    ) -> anyhow::Result<Self> {
        // Log config file sources so startup problems are easy to diagnose.
        let default_exists = std::path::Path::new(DEFAULT_CONFIG_PATH).exists();
        debug!(
            path = DEFAULT_CONFIG_PATH,
            exists = default_exists,
            "Default config file"
        );
        match &cli_config_path {
            Some(path) => debug!(path = %path.display(), "--config file"),
            None => debug!("--config: not specified"),
        }

        let mut builder = config::Config::builder()
            // Layer 4 (lowest file): default system config file
            .add_source(
                File::with_name(DEFAULT_CONFIG_PATH)
                    .format(FileFormat::Yaml)
                    .required(false),
            );

        // Layer 3: --config file
        if let Some(path) = cli_config_path {
            builder = builder.add_source(
                File::with_name(&path.to_string_lossy())
                    .format(FileFormat::Yaml)
                    .required(true),
            );
        }

        // Layer 2: environment variables
        builder = builder.add_source(
            Environment::with_prefix(ENV_PREFIX)
                .separator(ENV_SEPARATOR)
                .try_parsing(true),
        );

        // Layer 1 (highest): --set key=value overrides
        for (key, value) in overrides {
            builder = builder.set_override(key.as_str(), value.as_str())?;
        }

        Ok(builder.build()?.try_deserialize::<Config>()?)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let cfg = Config::default();
        assert_eq!(cfg.daemon.log_level, "info");
        assert!(!cfg.daemon.systemd);
        assert_eq!(cfg.api.listen, "127.0.0.1:7785");
        assert!(cfg.pool.url.is_none());
        assert!(cfg.backplane.usb_enabled);
        assert!(!cfg.boards.cpu_miner.enabled);
    }

    #[test]
    fn load_with_no_files_uses_defaults() {
        // Verify load succeeds when no config file is present (the default
        // path won't exist in a dev environment).
        let result = Config::load_with(None, &[]);
        assert!(result.is_ok(), "load_with(None) failed: {:?}", result);
    }
}
