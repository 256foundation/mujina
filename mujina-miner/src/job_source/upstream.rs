//! Upstream endpoints that job sources connect to.
//!
//! The far end of a pool connection can be a pool, a proxy, or a job
//! declarator; the SV2 specification calls it the upstream.  This module
//! parses the URL naming it into an [`Endpoint`] that the code building
//! sources reads.

use std::str::FromStr;
use stratum_apps::key_utils::Secp256k1PublicKey;
use thiserror::Error;

/// Parsed upstream connection endpoint.
///
/// Recognised URL forms:
/// - `stratum2+tcp://host:port/authority_key` -> [`Protocol::StratumV2`]
/// - `stratum+tcp://host:port` -> [`Protocol::StratumV1`]
/// - `host:port` (bare) -> [`Protocol::StratumV1`]
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// Host name or IP address, with any IPv6 brackets removed.
    pub host: String,
    /// TCP port.
    pub port: u16,
    /// Wire protocol implied by the URL scheme.
    pub protocol: Protocol,
}

/// Wire protocol an upstream endpoint speaks.
#[derive(Debug, Clone, Copy)]
pub enum Protocol {
    /// Stratum V1 over plain TCP, spelled `stratum+tcp://`.
    StratumV1,
    /// Stratum V2 over plain TCP with a Noise handshake, spelled
    /// `stratum2+tcp://`.
    StratumV2 {
        /// Public key that authenticates the upstream in the Noise_NX
        /// handshake.
        authority_key: Secp256k1PublicKey,
    },
}

/// Error returned by [`Endpoint::parse`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseEndpointError {
    /// A `stratum2+tcp://` URL carried no authority key path segment.
    #[error(
        "stratum2+tcp:// requires an authority public key in the path: stratum2+tcp://host:port/key"
    )]
    MissingAuthorityKey,
    /// The authority key segment is not a valid Base58Check public key, or
    /// extra path segments followed it.
    #[error("invalid authority public key: {0}")]
    InvalidAuthorityKey(String),
    /// The endpoint has no `:port` suffix.
    #[error("missing port in endpoint (expected host:port)")]
    MissingPort,
    /// The port is not a decimal number in `u16` range.
    #[error("invalid port number '{0}'")]
    InvalidPort(String),
    /// The host part is empty.
    #[error("empty host in endpoint")]
    EmptyHost,
    /// The URL carries a scheme the miner does not speak.
    #[error("unsupported pool scheme '{0}'")]
    UnsupportedScheme(String),
}

impl Endpoint {
    /// Parses an upstream URL string into an [`Endpoint`].
    ///
    /// Recognised URL forms:
    /// - `stratum2+tcp://host:port/authority_key` -> [`Protocol::StratumV2`]
    /// - `stratum+tcp://host:port` -> [`Protocol::StratumV1`]
    /// - `host:port` (bare) -> [`Protocol::StratumV1`]
    ///
    /// Any other `scheme://` prefix returns [`ParseEndpointError::UnsupportedScheme`].
    pub fn parse(url: &str) -> Result<Self, ParseEndpointError> {
        // V2 is the only scheme that carries an authority key, so it parses on
        // its own path.
        if let Some(rest) = url.strip_prefix("stratum2+tcp://") {
            return Self::parse_v2(rest);
        }

        if let Some(rest) = url.strip_prefix("stratum+tcp://") {
            return Self::parse_v1(rest);
        }

        if let Some(scheme_end) = url.find("://") {
            return Err(ParseEndpointError::UnsupportedScheme(
                url[..scheme_end].to_string(),
            ));
        }

        // Bare host:port defaults to plain V1.
        Self::parse_v1(url)
    }

    fn parse_v1(rest: &str) -> Result<Self, ParseEndpointError> {
        // Strip any path component (e.g. /worker).  V1 pool URLs often embed
        // the worker name in the path; the miner reads it from the `worker`
        // config field instead.
        let host_port = rest.split_once('/').map_or(rest, |(hp, _)| hp);
        let (host, port) = parse_host_port(host_port)?;
        Ok(Self {
            host,
            port,
            protocol: Protocol::StratumV1,
        })
    }

    fn parse_v2(rest: &str) -> Result<Self, ParseEndpointError> {
        let (host_port, key_segment) = rest
            .split_once('/')
            .ok_or(ParseEndpointError::MissingAuthorityKey)?;
        if key_segment.is_empty() {
            return Err(ParseEndpointError::MissingAuthorityKey);
        }
        // Reject extra path segments (e.g. /key/worker), since the authority
        // key must be a single path component with no further slashes.
        if key_segment.contains('/') {
            return Err(ParseEndpointError::InvalidAuthorityKey(
                "unexpected path segments after key; \
                 expected stratum2+tcp://host:port/<key>"
                    .to_string(),
            ));
        }
        let (host, port) = parse_host_port(host_port)?;
        let authority_key = key_segment
            .parse::<Secp256k1PublicKey>()
            .map_err(|e| ParseEndpointError::InvalidAuthorityKey(e.to_string()))?;
        Ok(Self {
            host,
            port,
            protocol: Protocol::StratumV2 { authority_key },
        })
    }
}

impl FromStr for Endpoint {
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

#[cfg(test)]
mod tests {
    use super::*;

    // A known-good Base58Check-encoded SV2 authority public key (from stratum-apps test suite).
    const VALID_KEY: &str = "9bDuixKmZqAJnrmP746n8zU1wyAQRrus7th9dxnkPg6RzQvCnan";
    // Same key with the last character swapped, so its checksum is invalid.
    const BAD_CHECKSUM_KEY: &str = "9bDuixKmZqAJnrmP746n8zU1wyAQRrus7th9dxnkPg6RzQvCnam";

    #[test]
    fn parse_stratum_v1_scheme() {
        let ep = Endpoint::parse("stratum+tcp://pool.example.com:3333").unwrap();
        assert_eq!(ep.host, "pool.example.com");
        assert_eq!(ep.port, 3333);
        assert!(matches!(ep.protocol, Protocol::StratumV1));
    }

    #[test]
    fn parse_bare_host_port() {
        let ep = Endpoint::parse("pool.example.com:3333").unwrap();
        assert_eq!(ep.host, "pool.example.com");
        assert_eq!(ep.port, 3333);
        assert!(matches!(ep.protocol, Protocol::StratumV1));
    }

    #[test]
    fn parse_ipv4_bare() {
        let ep = Endpoint::parse("192.168.1.1:3333").unwrap();
        assert_eq!(ep.host, "192.168.1.1");
        assert_eq!(ep.port, 3333);
        assert!(matches!(ep.protocol, Protocol::StratumV1));
    }

    #[test]
    fn parse_ipv6_bracketed() {
        let ep = Endpoint::parse("[::1]:3333").unwrap();
        assert_eq!(ep.host, "::1");
        assert_eq!(ep.port, 3333);
        assert!(matches!(ep.protocol, Protocol::StratumV1));
    }

    #[test]
    fn parse_stratum_v2_valid() {
        let url = format!("stratum2+tcp://pool.example.com:3336/{VALID_KEY}");
        let ep = Endpoint::parse(&url).unwrap();
        assert_eq!(ep.host, "pool.example.com");
        assert_eq!(ep.port, 3336);
        assert!(matches!(ep.protocol, Protocol::StratumV2 { .. }));
    }

    /// `stratum+tcp://` URLs with a path component (e.g. an embedded worker
    /// name) must still parse; the path is stripped and ignored.
    #[test]
    fn parse_stratum_v1_with_path_strips_it() {
        let ep = Endpoint::parse("stratum+tcp://pool.example.com:3333/myworker.1").unwrap();
        assert_eq!(ep.host, "pool.example.com");
        assert_eq!(ep.port, 3333);
        assert!(matches!(ep.protocol, Protocol::StratumV1));
    }

    /// `stratum2+tcp://` URLs with extra path segments after the key must fail
    /// with a clear error rather than blaming the key itself.
    #[test]
    fn parse_v2_extra_path_segments_rejected() {
        let url = format!("stratum2+tcp://pool.example.com:3336/{VALID_KEY}/worker");
        let err = Endpoint::parse(&url).unwrap_err();
        assert!(
            matches!(err, ParseEndpointError::InvalidAuthorityKey(ref msg)
                if msg.contains("unexpected path segments")),
            "expected InvalidAuthorityKey with 'unexpected path segments', got {err:?}"
        );
    }

    #[test]
    fn parse_v2_ipv6_with_key() {
        let url = format!("stratum2+tcp://[::1]:3336/{VALID_KEY}");
        let ep = Endpoint::parse(&url).unwrap();
        assert_eq!(ep.host, "::1");
        assert_eq!(ep.port, 3336);
        assert!(matches!(ep.protocol, Protocol::StratumV2 { .. }));
    }

    #[test]
    fn parse_v2_missing_key_no_slash() {
        let err = Endpoint::parse("stratum2+tcp://pool.example.com:3336").unwrap_err();
        assert_eq!(err, ParseEndpointError::MissingAuthorityKey);
    }

    #[test]
    fn parse_v2_missing_key_trailing_slash() {
        let err = Endpoint::parse("stratum2+tcp://pool.example.com:3336/").unwrap_err();
        assert_eq!(err, ParseEndpointError::MissingAuthorityKey);
    }

    #[test]
    fn parse_v2_malformed_key_bad_checksum() {
        let url = format!("stratum2+tcp://pool.example.com:3336/{BAD_CHECKSUM_KEY}");
        let err = Endpoint::parse(&url).unwrap_err();
        assert!(matches!(err, ParseEndpointError::InvalidAuthorityKey(_)));
    }

    #[test]
    fn parse_v2_malformed_key_garbage() {
        let err = Endpoint::parse("stratum2+tcp://pool.example.com:3336/notavalidkey").unwrap_err();
        assert!(matches!(err, ParseEndpointError::InvalidAuthorityKey(_)));
    }

    #[test]
    fn parse_unsupported_scheme_errors() {
        for url in [
            "tcp://pool.example.com:3333",
            "stratum://pool.example.com:3333",
            "stratum+ssl://pool.example.com:3333",
            "stratum+tls://pool.example.com:3333",
            "http://pool.example.com:3333",
        ] {
            let err = Endpoint::parse(url).unwrap_err();
            assert!(
                matches!(err, ParseEndpointError::UnsupportedScheme(_)),
                "expected UnsupportedScheme for {url}, got {err:?}"
            );
        }
    }
}
