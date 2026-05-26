//! Error types for Stratum V2 protocol.
//!
//! Variants are classified as fatal (explicit pool rejection, unusable channel)
//! or transient (I/O, timeout) so callers can decide whether to retry or abort.

use stratum_apps::stratum_core::channels_sv2::extranonce_manager::ExtranoncePrefixError;
use thiserror::Error;

/// Errors that can occur during Stratum V2 client operation.
#[derive(Error, Debug)]
pub enum StratumV2Error {
    /// TCP connection failure.
    ///
    /// Transient: may resolve on retry (pool restart, network outage).
    #[error("connection to {endpoint} failed: {source}")]
    ConnectionFailed {
        /// Pool `host:port` the connection was attempted against.
        endpoint: String,
        /// Underlying socket error.
        #[source]
        source: std::io::Error,
    },

    /// Pool rejected our `SetupConnection` message.
    ///
    /// Fatal: indicates protocol version or capability mismatch that won't
    /// resolve without a configuration change.
    #[error("pool rejected setup connection: {0}")]
    SetupRejected(String),

    /// Pool rejected our `OpenExtendedMiningChannel` message.
    ///
    /// Fatal: typically caused by invalid user identity or unsupported
    /// extranonce size request.
    #[error("pool rejected open channel: {0}")]
    OpenChannelRejected(String),

    /// Pool-assigned extranonce size is outside `[MIN_EXTRANONCE_SIZE, MAX_EXTRANONCE_SIZE]`.
    #[error("pool assigned extranonce size outside acceptable range")]
    ExtranonceSizeMismatch,

    /// Pool answered `SetupConnection` with `REQUIRES_FIXED_VERSION`.
    ///
    /// Fatal: the miner rolls the version field, and a pool that forbids it
    /// gives the same answer on every retry.
    #[error("pool requires a fixed version field, but the miner rolls it")]
    FixedVersionRequired,

    /// DNS resolution failed.
    ///
    /// Transient: may resolve on retry.
    #[error("DNS resolution failed for {host}: {error}")]
    DnsResolutionFailed {
        /// Host name that failed to resolve.
        host: String,
        /// Resolver error, rendered as text.
        error: String,
    },

    /// Network or framed I/O error.
    ///
    /// Transient: triggers reconnect.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Protocol-level error (unexpected message, serialization failure).
    #[error("protocol error: {0}")]
    Protocol(String),

    /// Noise handshake failed.
    ///
    /// Transient: triggers reconnect.
    #[error("noise handshake failed: {0}")]
    Handshake(String),

    /// A pool did not answer, or did not read what was sent, within its
    /// budget.
    ///
    /// Transient: without this, a pool that completes the handshake and then
    /// goes silent leaves the setup exchange waiting forever, and one that
    /// stops reading its socket leaves a write blocked forever.
    #[error("timed out waiting for {0}")]
    Timeout(String),

    /// Client was shut down via cancellation token.
    #[error("client shutdown")]
    Shutdown,

    /// Pool sent `Reconnect` before channel setup completed; caller should
    /// reconnect to the endpoint it names.
    #[error("pool requested reconnect to {host}:{port} during setup")]
    ReconnectDuringSetup {
        /// Host the next connection should target.
        host: String,
        /// Port the next connection should target.
        port: u16,
    },
}

impl StratumV2Error {
    /// Returns `true` if the error is unrecoverable and should not be retried.
    ///
    /// Explicit pool rejections, a required fixed version, and an
    /// out-of-range extranonce allocation are fatal: none of them fixes
    /// itself without a configuration change, and a pool answers a retry the
    /// same way it answered the first attempt.  Everything else (network
    /// errors, handshake failures, timeouts, DNS failures) may resolve on
    /// retry or reconnect.
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            StratumV2Error::SetupRejected(_)
                | StratumV2Error::OpenChannelRejected(_)
                | StratumV2Error::ExtranonceSizeMismatch
                | StratumV2Error::FixedVersionRequired
        )
    }
}

/// Maps a `binary_sv2` encoding failure, such as a string too long for
/// `STR0_255`, to a protocol error.
///
/// Written by hand because `binary_sv2::Error` does not implement
/// `std::error::Error`, which `#[from]` requires.
impl From<stratum_apps::stratum_core::binary_sv2::Error> for StratumV2Error {
    fn from(e: stratum_apps::stratum_core::binary_sv2::Error) -> Self {
        StratumV2Error::Protocol(format!("invalid SV2 value: {e}"))
    }
}

/// Maps an extranonce prefix the channel cannot hold to a protocol error.
impl From<ExtranoncePrefixError> for StratumV2Error {
    fn from(e: ExtranoncePrefixError) -> Self {
        StratumV2Error::Protocol(format!("invalid extranonce prefix: {e}"))
    }
}

/// Convenient Result type for Stratum V2 operations.
pub type StratumV2Result<T> = Result<T, StratumV2Error>;
