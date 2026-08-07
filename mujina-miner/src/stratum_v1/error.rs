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
    /// Authorization failures and invalid URLs are fatal---wrong
    /// credentials and malformed addresses won't fix themselves.
    /// Everything else (network errors, timeouts, subscription
    /// failures from overloaded pools) may resolve on retry.
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            StratumError::AuthorizationFailed(_) | StratumError::InvalidUrl(_)
        )
    }
}

/// Convenient Result type for Stratum operations.
pub type StratumResult<T> = Result<T, StratumError>;
