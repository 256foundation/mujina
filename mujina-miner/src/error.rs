//! Common error types for mujina-miner.
//!
//! This module provides a centralized Error enum using thiserror,
//! with conversions from underlying error types used throughout the crate.

use thiserror::Error;

/// Main error type for mujina-miner operations.
#[derive(Error, Debug)]
pub enum Error {
    /// I/O errors from tokio or std
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Serial port errors
    #[error("Serial port error: {0}")]
    Serial(#[from] tokio_serial::Error),

    /// Configuration errors
    #[error("Configuration error: {0}")]
    Config(String),

    /// Protocol errors
    #[error("Protocol error: {0}")]
    Protocol(String),

    /// Hardware communication errors
    #[error("Hardware error: {0}")]
    Hardware(String),

    /// Pool communication errors
    #[error("Pool error: {0}")]
    Pool(String),

    /// API errors
    #[error("API error: {0}")]
    Api(String),

    /// Generic errors for development
    #[error("{0}")]
    Other(String),
}

/// Convenience type alias for Results using our Error type.
pub type Result<T> = std::result::Result<T, Error>;
