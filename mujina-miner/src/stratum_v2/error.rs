//! Error types for Stratum V2 protocol.
//!
//! Variants are classified as fatal (misconfigured key, explicit pool rejection)
//! or transient (I/O, timeout) so callers can decide whether to retry or abort.

use std::net::SocketAddr;

use thiserror::Error;

/// Errors that can occur during Stratum V2 client operation.
#[derive(Error, Debug)]
pub enum StratumV2Error {
    /// DNS resolution or TCP connection failure.
    ///
    /// Transient — may resolve on retry (DNS propagation, pool restart).
    #[error("connection to {addr} failed: {source}")]
    ConnectionFailed {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },

    /// Pool rejected our `SetupConnection` message.
    ///
    /// Fatal — indicates protocol version or capability mismatch that won't
    /// resolve without a configuration change.
    #[error("pool rejected setup connection: {0}")]
    SetupRejected(String),

    /// Pool rejected our `OpenExtendedMiningChannel` message.
    ///
    /// Fatal — typically caused by invalid user identity or unsupported
    /// extranonce size request.
    #[error("pool rejected open channel: {0}")]
    OpenChannelRejected(String),

    /// Pool-assigned extranonce size is outside `[MIN_EXTRANONCE_SIZE, MAX_EXTRANONCE_SIZE]`.
    #[error("pool assigned extranonce size outside acceptable range")]
    ExtranonceSizeMismatch,

    /// Authority public key is invalid for Noise handshake.
    ///
    /// Fatal — the configured key is malformed. No retry can fix this.
    #[error("invalid authority public key: {0}")]
    InvalidAuthorityKey(String),

    /// DNS resolution failed.
    ///
    /// Transient — may resolve on retry.
    #[error("DNS resolution failed for {host}: {error}")]
    DnsResolutionFailed { host: String, error: String },

    /// Network or framed I/O error.
    ///
    /// Transient — triggers reconnect.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Protocol-level error (unexpected message, serialization failure).
    #[error("protocol error: {0}")]
    Protocol(String),

    /// Client was shut down via cancellation token.
    #[error("client shutdown")]
    Shutdown,

    /// Pool sent `Reconnect` before channel setup completed; caller should reconnect.
    #[error("pool requested reconnect during setup")]
    ReconnectDuringSetup,
}

impl StratumV2Error {
    /// Whether this error is unrecoverable and should not be retried.
    ///
    /// Authorization failures, key errors, and explicit pool rejections are
    /// fatal — they won't fix themselves without a configuration change.
    /// Everything else (network errors, timeouts, DNS failures, extranonce
    /// size mismatches) may resolve on retry or reconnect.
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            StratumV2Error::SetupRejected(_)
                | StratumV2Error::OpenChannelRejected(_)
                | StratumV2Error::InvalidAuthorityKey(_)
                | StratumV2Error::ExtranonceSizeMismatch
        )
    }
}

/// Convenient Result type for Stratum V2 operations.
pub type StratumV2Result<T> = Result<T, StratumV2Error>;
