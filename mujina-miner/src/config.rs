//! Configuration management for mujina-miner.
//!
//! This module handles loading and validating configuration from TOML files,
//! environment variables, and command-line arguments. It supports hot-reload
//! via file watching.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use stratum_apps::key_utils::Secp256k1PublicKey;
use thiserror::Error;

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
    pub url: String,
    pub worker: String,
    pub password: Option<String>,
    #[serde(default)]
    pub priority: u32,
}

/// Error returned by [`PoolEndpoint::parse`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseEndpointError {
    #[error(
        "stratum2+tcp:// requires an authority public key in the path: stratum2+tcp://host:port/key"
    )]
    MissingAuthorityKey,
    #[error("invalid authority public key: {0}")]
    InvalidAuthorityKey(String),
    #[error("missing port in endpoint (expected host:port)")]
    MissingPort,
    #[error("invalid port number '{0}'")]
    InvalidPort(String),
    #[error("empty host in endpoint")]
    EmptyHost,
    #[error("unsupported pool scheme '{0}'")]
    UnsupportedScheme(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum PoolProtocol {
    StratumV1,
    /// Stratum V1 over SSL/TLS (`stratum+ssl://`).
    StratumV1Ssl,
    /// Stratum V1 over SSL/TLS (`stratum+tls://`).
    StratumV1Tls,
    StratumV2,
}

/// Parsed pool connection endpoint.
///
/// Produced by [`PoolEndpoint::parse`] from the `url` field of [`PoolConfig`].
///
/// Recognised URL forms:
/// - `stratum2+tcp://host:port/authority_key` → [`PoolProtocol::StratumV2`]
/// - `stratum+tcp://host:port` → [`PoolProtocol::StratumV1`]
/// - `stratum+ssl://host:port` → [`PoolProtocol::StratumV1Ssl`]
/// - `stratum+tls://host:port` → [`PoolProtocol::StratumV1Tls`]
/// - `host:port` (bare) → [`PoolProtocol::StratumV1`]
#[derive(Debug, Clone)]
pub struct PoolEndpoint {
    pub host: String,
    pub port: u16,
    pub protocol: PoolProtocol,
    /// Present only for [`PoolProtocol::StratumV2`]; used in Noise_NX handshake.
    ///
    /// Private to enforce the invariant that V2 endpoints always carry a key
    /// and V1 endpoints never do. Use [`PoolEndpoint::authority_pubkey`] to read it.
    authority_pubkey: Option<Secp256k1PublicKey>,
}

impl PoolEndpoint {
    /// Parse a pool URL string into a `PoolEndpoint`.
    ///
    /// Recognised URL forms:
    /// - `stratum2+tcp://host:port/authority_key` → [`PoolProtocol::StratumV2`]
    /// - `stratum+tcp://host:port` → [`PoolProtocol::StratumV1`]
    /// - `stratum+ssl://host:port` → [`PoolProtocol::StratumV1Ssl`]
    /// - `stratum+tls://host:port` → [`PoolProtocol::StratumV1Tls`]
    /// - `host:port` (bare) → [`PoolProtocol::StratumV1`]
    ///
    /// Any other `scheme://` prefix returns [`ParseEndpointError::UnsupportedScheme`].
    pub fn parse(url: &str) -> Result<Self, ParseEndpointError> {
        // V2 is the only scheme that carries an authority key, so it parses on
        // its own path.
        if let Some(rest) = url.strip_prefix("stratum2+tcp://") {
            return Self::parse_v2(rest);
        }

        // The V1-family schemes differ only in their transport label.
        const V1_SCHEMES: [(&str, PoolProtocol); 3] = [
            ("stratum+tcp://", PoolProtocol::StratumV1),
            ("stratum+ssl://", PoolProtocol::StratumV1Ssl),
            ("stratum+tls://", PoolProtocol::StratumV1Tls),
        ];
        for (prefix, protocol) in V1_SCHEMES {
            if let Some(rest) = url.strip_prefix(prefix) {
                return Self::parse_v1(rest, protocol);
            }
        }

        if let Some(scheme_end) = url.find("://") {
            return Err(ParseEndpointError::UnsupportedScheme(
                url[..scheme_end].to_string(),
            ));
        }

        // Bare host:port defaults to plain V1.
        Self::parse_v1(url, PoolProtocol::StratumV1)
    }

    fn parse_v1(host_port: &str, protocol: PoolProtocol) -> Result<Self, ParseEndpointError> {
        let (host, port) = parse_host_port(host_port)?;
        Ok(Self {
            host,
            port,
            protocol,
            authority_pubkey: None,
        })
    }

    fn parse_v2(rest: &str) -> Result<Self, ParseEndpointError> {
        let (host_port, key_segment) = rest
            .split_once('/')
            .ok_or(ParseEndpointError::MissingAuthorityKey)?;
        if key_segment.is_empty() {
            return Err(ParseEndpointError::MissingAuthorityKey);
        }
        let (host, port) = parse_host_port(host_port)?;
        let authority_pubkey = key_segment
            .parse::<Secp256k1PublicKey>()
            .map_err(|e| ParseEndpointError::InvalidAuthorityKey(e.to_string()))?;
        Ok(Self {
            host,
            port,
            protocol: PoolProtocol::StratumV2,
            authority_pubkey: Some(authority_pubkey),
        })
    }

    /// Returns the authority public key for Stratum V2 endpoints (`None` for V1).
    pub fn authority_pubkey(&self) -> Option<Secp256k1PublicKey> {
        self.authority_pubkey
    }
}

impl FromStr for PoolEndpoint {
    type Err = ParseEndpointError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

fn parse_host_port(s: &str) -> Result<(String, u16), ParseEndpointError> {
    let colon = s.rfind(':').ok_or(ParseEndpointError::MissingPort)?;
    let host_part = &s[..colon];
    let port_str = &s[colon + 1..];

    // Strip brackets from IPv6 addresses like [::1].
    let host = if host_part.starts_with('[') && host_part.ends_with(']') {
        host_part[1..host_part.len() - 1].to_string()
    } else {
        host_part.to_string()
    };

    if host.is_empty() {
        return Err(ParseEndpointError::EmptyHost);
    }

    let port = port_str
        .parse::<u16>()
        .map_err(|_| ParseEndpointError::InvalidPort(port_str.to_string()))?;

    Ok((host, port))
}

impl PoolConfig {
    /// Parse [`PoolConfig::url`] into a [`PoolEndpoint`].
    pub fn endpoint(&self) -> Result<PoolEndpoint, ParseEndpointError> {
        PoolEndpoint::parse(&self.url)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    // A known-good Base58Check-encoded SV2 authority public key (from stratum-apps test suite).
    const VALID_KEY: &str = "9bDuixKmZqAJnrmP746n8zU1wyAQRrus7th9dxnkPg6RzQvCnan";
    // Same key with the last character swapped — invalid checksum.
    const BAD_CHECKSUM_KEY: &str = "9bDuixKmZqAJnrmP746n8zU1wyAQRrus7th9dxnkPg6RzQvCnam";

    #[test]
    fn parse_stratum_v1_scheme() {
        let ep = PoolEndpoint::parse("stratum+tcp://pool.example.com:3333").unwrap();
        assert_eq!(ep.host, "pool.example.com");
        assert_eq!(ep.port, 3333);
        assert_eq!(ep.protocol, PoolProtocol::StratumV1);
        assert!(ep.authority_pubkey().is_none());
    }

    #[test]
    fn parse_stratum_v1_ssl_scheme() {
        let ep = PoolEndpoint::parse("stratum+ssl://pool.example.com:3333").unwrap();
        assert_eq!(ep.host, "pool.example.com");
        assert_eq!(ep.port, 3333);
        assert_eq!(ep.protocol, PoolProtocol::StratumV1Ssl);
        assert!(ep.authority_pubkey().is_none());
    }

    #[test]
    fn parse_stratum_v1_tls_scheme() {
        let ep = PoolEndpoint::parse("stratum+tls://pool.example.com:3333").unwrap();
        assert_eq!(ep.host, "pool.example.com");
        assert_eq!(ep.port, 3333);
        assert_eq!(ep.protocol, PoolProtocol::StratumV1Tls);
        assert!(ep.authority_pubkey().is_none());
    }

    #[test]
    fn parse_bare_host_port() {
        let ep = PoolEndpoint::parse("pool.example.com:3333").unwrap();
        assert_eq!(ep.host, "pool.example.com");
        assert_eq!(ep.port, 3333);
        assert_eq!(ep.protocol, PoolProtocol::StratumV1);
        assert!(ep.authority_pubkey().is_none());
    }

    #[test]
    fn parse_ipv4_bare() {
        let ep = PoolEndpoint::parse("192.168.1.1:3333").unwrap();
        assert_eq!(ep.host, "192.168.1.1");
        assert_eq!(ep.port, 3333);
        assert_eq!(ep.protocol, PoolProtocol::StratumV1);
    }

    #[test]
    fn parse_ipv6_bracketed() {
        let ep = PoolEndpoint::parse("[::1]:3333").unwrap();
        assert_eq!(ep.host, "::1");
        assert_eq!(ep.port, 3333);
        assert_eq!(ep.protocol, PoolProtocol::StratumV1);
    }

    #[test]
    fn parse_stratum_v2_valid() {
        let url = format!("stratum2+tcp://pool.example.com:3336/{VALID_KEY}");
        let ep = PoolEndpoint::parse(&url).unwrap();
        assert_eq!(ep.host, "pool.example.com");
        assert_eq!(ep.port, 3336);
        assert_eq!(ep.protocol, PoolProtocol::StratumV2);
        assert!(ep.authority_pubkey().is_some());
    }

    #[test]
    fn parse_v2_ipv6_with_key() {
        let url = format!("stratum2+tcp://[::1]:3336/{VALID_KEY}");
        let ep = PoolEndpoint::parse(&url).unwrap();
        assert_eq!(ep.host, "::1");
        assert_eq!(ep.port, 3336);
        assert_eq!(ep.protocol, PoolProtocol::StratumV2);
    }

    #[test]
    fn parse_v2_missing_key_no_slash() {
        let err = PoolEndpoint::parse("stratum2+tcp://pool.example.com:3336").unwrap_err();
        assert_eq!(err, ParseEndpointError::MissingAuthorityKey);
    }

    #[test]
    fn parse_v2_missing_key_trailing_slash() {
        let err = PoolEndpoint::parse("stratum2+tcp://pool.example.com:3336/").unwrap_err();
        assert_eq!(err, ParseEndpointError::MissingAuthorityKey);
    }

    #[test]
    fn parse_v2_malformed_key_bad_checksum() {
        let url = format!("stratum2+tcp://pool.example.com:3336/{BAD_CHECKSUM_KEY}");
        let err = PoolEndpoint::parse(&url).unwrap_err();
        assert!(matches!(err, ParseEndpointError::InvalidAuthorityKey(_)));
    }

    #[test]
    fn parse_v2_malformed_key_garbage() {
        let err =
            PoolEndpoint::parse("stratum2+tcp://pool.example.com:3336/notavalidkey").unwrap_err();
        assert!(matches!(err, ParseEndpointError::InvalidAuthorityKey(_)));
    }

    #[test]
    fn parse_unsupported_scheme_errors() {
        for url in [
            "tcp://pool.example.com:3333",
            "stratum://pool.example.com:3333",
            "http://pool.example.com:3333",
        ] {
            let err = PoolEndpoint::parse(url).unwrap_err();
            assert!(
                matches!(err, ParseEndpointError::UnsupportedScheme(_)),
                "expected UnsupportedScheme for {url}, got {err:?}"
            );
        }
    }

    #[test]
    fn pool_config_endpoint_helper() {
        let cfg = PoolConfig {
            url: format!("stratum+tcp://pool.example.com:3333"),
            worker: "worker".to_string(),
            password: None,
            priority: 0,
        };
        let ep = cfg.endpoint().unwrap();
        assert_eq!(ep.protocol, PoolProtocol::StratumV1);
    }
}
