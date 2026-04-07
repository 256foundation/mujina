//! Configuration management for mujina-miner.
//!
//! Loads configuration from multiple sources in priority order (highest wins):
//!
//! 1. CLI flags (caller merges these on top after calling `Config::load`)
//! 2. Environment variables — prefix `MUJINA`, separator `__`
//!    e.g. `MUJINA__POOL__URL=stratum+tcp://pool.example.com:3333`
//! 3. User config file — path from `MUJINA_CONFIG_FILE_PATH` env var, or the
//!    `--config` path passed as `cli_config_path` to `Config::load_with`
//! 4. Default config file — `/etc/mujina/mujina.yaml`
//! 5. Hard-coded defaults — `Default` impls on each struct (lowest priority)
//!
//! See `docs/configuration.md` and `configs/mujina.example.yaml` for the full
//! key reference.

use std::path::PathBuf;

use config::{Environment, File, FileFormat};
use serde::{Deserialize, Serialize};

const DEFAULT_CONFIG_PATH: &str = "/etc/mujina/mujina.yaml";
const DEFAULT_CONFIG_PATH_ENV_VAR: &str = "MUJINA_DEFAULT_CONFIG_PATH";
const CONFIG_FILE_ENV_VAR: &str = "MUJINA_CONFIG_FILE_PATH";
const ENV_PREFIX: &str = "MUJINA";
const ENV_SEPARATOR: &str = "__";

/// Returns the path to the default system config file.
///
/// Normally `/etc/mujina/mujina.yaml`. Override via `MUJINA_DEFAULT_CONFIG_PATH`
/// (useful in tests to avoid requiring root access to `/etc`).
fn default_config_path() -> String {
    std::env::var(DEFAULT_CONFIG_PATH_ENV_VAR)
        .unwrap_or_else(|_| DEFAULT_CONFIG_PATH.to_string())
}

// ---------------------------------------------------------------------------
// Top-level config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub daemon: DaemonConfig,
    pub api: ApiConfig,
    pub pool: PoolConfig,
    pub backplane: BackplaneConfig,
    pub boards: BoardsConfig,
    pub hash_thread: HashThreadConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            daemon: DaemonConfig::default(),
            api: ApiConfig::default(),
            pool: PoolConfig::default(),
            backplane: BackplaneConfig::default(),
            boards: BoardsConfig::default(),
            hash_thread: HashThreadConfig::default(),
        }
    }
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
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            url: None,
            user: "mujina-testing".to_string(),
            password: "x".to_string(),
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

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct BoardsConfig {
    pub bitaxe: BitaxeConfig,
    pub cpu_miner: CpuMinerConfig,
}

impl Default for BoardsConfig {
    fn default() -> Self {
        Self {
            bitaxe: BitaxeConfig::default(),
            cpu_miner: CpuMinerConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct BitaxeConfig {
    pub temp_limit_c: f32,
    pub fan_min_pct: u8,
    pub fan_max_pct: u8,
    pub power_limit_w: Option<f32>,
}

impl Default for BitaxeConfig {
    fn default() -> Self {
        Self {
            temp_limit_c: 85.0,
            fan_min_pct: 20,
            fan_max_pct: 100,
            power_limit_w: None,
        }
    }
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

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct HashThreadConfig {
    pub chip_target_difficulty: u32,
}

impl Default for HashThreadConfig {
    fn default() -> Self {
        Self {
            chip_target_difficulty: 256,
        }
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

impl Config {
    /// Load configuration using the standard source hierarchy.
    ///
    /// The user config path is read from `MUJINA_CONFIG_FILE_PATH` if set.
    /// To supply a path from a CLI `--config` flag instead, use
    /// [`Config::load_with`].
    pub fn load() -> anyhow::Result<Self> {
        Self::load_with(None)
    }

    /// Load configuration, optionally overriding the user config file path.
    ///
    /// `cli_config_path` corresponds to the `--config` CLI flag and takes
    /// precedence over `MUJINA_CONFIG_FILE_PATH`.
    pub fn load_with(cli_config_path: Option<PathBuf>) -> anyhow::Result<Self> {
        let mut builder = config::Config::builder()
            // Layer 4 (lowest): default system config file
            .add_source(
                File::with_name(&default_config_path())
                    .format(FileFormat::Yaml)
                    .required(false),
            );

        // Layer 3: user-specified config file (CLI flag beats env var)
        let user_path = cli_config_path
            .map(|p| p.to_string_lossy().into_owned())
            .or_else(|| std::env::var(CONFIG_FILE_ENV_VAR).ok());

        if let Some(path) = user_path {
            builder = builder.add_source(
                File::with_name(&path)
                    .format(FileFormat::Yaml)
                    .required(true),
            );
        }

        // Layer 2 (highest file-based): environment variables
        builder = builder.add_source(
            Environment::with_prefix(ENV_PREFIX)
                .separator(ENV_SEPARATOR)
                .try_parsing(true),
        );

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
        assert_eq!(cfg.boards.bitaxe.temp_limit_c, 85.0);
        assert_eq!(cfg.hash_thread.chip_target_difficulty, 256);
    }

    #[test]
    fn load_with_no_files_uses_defaults() {
        // Verify load succeeds when no config file is present (the default
        // path won't exist in a dev environment).
        let result = Config::load_with(None);
        assert!(result.is_ok(), "load_with(None) failed: {:?}", result);
    }
}
