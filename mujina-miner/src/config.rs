//! Configuration management for mujina-miner.
//!
//! This module handles loading and validating configuration from TOML files,
//! environment variables, and command-line arguments. It supports hot-reload
//! via file watching.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Main configuration structure for the miner.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// Daemon configuration
    pub daemon: DaemonConfig,

    /// Pool configuration
    pub pools: Vec<PoolConfig>,

    /// Hardware configuration
    pub hardware: HardwareConfig,

    /// API server configuration
    pub api: ApiConfig,
}

/// Daemon process configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DaemonConfig {
    /// PID file location
    pub pid_file: Option<PathBuf>,

    /// Log level
    pub log_level: String,

    /// Use systemd notification
    #[serde(default)]
    pub systemd: bool,
}

/// Pool connection configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PoolConfig {
    /// Pool URL (stratum+tcp://...)
    pub url: String,

    /// Worker name
    pub worker: String,

    /// Password (if required)
    pub password: Option<String>,

    /// Priority (lower is higher priority)
    #[serde(default)]
    pub priority: u32,
}

/// Hardware configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HardwareConfig {
    /// Temperature limits
    pub temp_limit: f32,

    /// Fan control settings
    pub fan_min_rpm: u32,
    pub fan_max_rpm: u32,

    /// Power limits
    pub power_limit: Option<f32>,
}

/// API server configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ApiConfig {
    /// Listen address
    pub listen: String,

    /// Enable TLS
    #[serde(default)]
    pub tls: bool,

    /// TLS certificate path
    pub cert_path: Option<PathBuf>,

    /// TLS key path
    pub key_path: Option<PathBuf>,
}

impl Config {
    /// Load configuration from the default location.
    pub fn load() -> anyhow::Result<Self> {
        // TODO: Implement config loading from /etc/mujina/mujina.toml
        // and ~/.config/mujina/mujina.toml with proper merging
        unimplemented!("Config loading not yet implemented")
    }

    /// Load configuration from a specific file.
    pub fn load_from(_path: &Path) -> anyhow::Result<Self> {
        // TODO: Implement TOML parsing
        unimplemented!("Config loading not yet implemented")
    }
}
