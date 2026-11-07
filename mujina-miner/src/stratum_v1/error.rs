//! Error types for Stratum v1 protocol.

use thiserror::Error;

/// Stratum protocol errors.
#[derive(Error, Debug)]
pub enum StratumError {
    /// Network I/O error
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON parsing or serialization error
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// Invalid message format received from pool
    #[error("Invalid message format: {0}")]
    InvalidMessage(String),

    /// Message from pool exceeded the maximum accepted length
    ///
    /// Always terminates the connection; the caller reconnects with
    /// backoff.
    #[error("Message exceeds {0}-byte limit")]
    MessageTooLarge(usize),

    /// Pool returned an error response
    #[error("Pool error: {0}")]
    PoolError(String),

    /// Connection error
    #[error("Connection failed: {0}")]
    ConnectionFailed(String),

    /// Subscription failed
    #[error("Subscription failed: {0}")]
    SubscriptionFailed(String),

    /// Authorization failed
    #[error("Authorization failed: {0}")]
    AuthorizationFailed(String),

    /// Unexpected response (wrong ID, missing fields, etc.)
    #[error("Unexpected response: {0}")]
    UnexpectedResponse(String),

    /// Missing required field in message
    #[error("Missing required field: {0}")]
    MissingField(String),

    /// Invalid URL format
    #[error("Invalid URL: {0}")]
    InvalidUrl(String),

    /// Connection lost
    #[error("Connection lost")]
    Disconnected,

    /// Timeout waiting for response
    #[error("Timeout waiting for response")]
    Timeout,
}

impl StratumError {
    /// Whether this error is unrecoverable and should not be retried.
    ///
    /// Only invalid URLs are fatal---a malformed address won't fix
    /// itself. Everything else, including authorization failures, is
    /// retried with backoff: stratum is unencrypted, so a "rejected"
    /// authorize may come from a MITM or a transient pool error rather
    /// than from truly wrong credentials, and giving up would let a
    /// single forged packet halt mining until a manual restart.
    pub fn is_fatal(&self) -> bool {
        matches!(self, StratumError::InvalidUrl(_))
    }
}

/// Convenient Result type for Stratum operations.
pub type StratumResult<T> = Result<T, StratumError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_invalid_urls_are_fatal() {
        assert!(StratumError::InvalidUrl("x".into()).is_fatal());
        assert!(!StratumError::AuthorizationFailed("x".into()).is_fatal());
        assert!(!StratumError::Disconnected.is_fatal());
        assert!(!StratumError::Timeout.is_fatal());
    }
}
