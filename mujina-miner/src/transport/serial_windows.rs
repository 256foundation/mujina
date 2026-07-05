//! Windows serial stream implementation using tokio-serial.
//!
//! ## Why this exists
//!
//! `transport/serial.rs` is a Unix-only implementation built directly on raw
//! file descriptors and termios (via `rustix`) so that a `SerialControl`
//! handle can reconfigure the port (in particular, change the baud rate) even
//! after the stream has been split into concurrent read/write halves.
//!
//! Windows has no raw-fd/termios model, so this module reimplements the same
//! public API on top of `tokio-serial`, which itself wraps `serialport-rs`'s
//! Windows backend (`SetCommState`/`SetCommTimeouts` via the Win32 comm API).
//! Shared ownership + a `parking_lot::RwLock` around the underlying
//! `tokio_serial::SerialStream` gives `SerialControl` mutable access to
//! reconfigure the port while `SerialReader`/`SerialWriter` are in use
//! elsewhere, mirroring the atomic/lock design of the Unix implementation.
//!
//! ## Known behavioral differences vs. the Unix implementation
//!
//! - `SerialControl::set_baud_rate` on Unix drains the output buffer before
//!   applying the new baud rate. `serialport-rs`'s Windows backend
//!   reconfigures the port's DCB directly and does not itself guarantee
//!   queued writes have drained first. Because `set_baud_rate` is a
//!   synchronous, `&self` method (to match the cross-platform API), it cannot
//!   `.await` a flush here. Callers relying on drain-before-switch semantics
//!   should ensure the writer has finished before calling it.
//! - I/O errors are surfaced as the underlying `io::Error` from `tokio-serial`
//!   rather than being classified into `SerialError::Disconnected` /
//!   `SerialError::HardwareError`, since POSIX errno values have no reliable
//!   Windows equivalent.
//! - `SerialStream::from_fd` has no Windows analog and is intentionally not
//!   provided. On Unix it is `#[cfg(test)] pub(crate)` and only used by that
//!   module's own PTY-based unit tests; no code outside `serial.rs` calls it.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use parking_lot::RwLock;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_serial::{SerialPort, SerialPortBuilderExt, SerialStream as TokioSerialStream};

/// Parity configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parity {
    None = 0,
    Odd = 1,
    Even = 2,
}

impl From<Parity> for tokio_serial::Parity {
    fn from(p: Parity) -> Self {
        match p {
            Parity::None => tokio_serial::Parity::None,
            Parity::Odd => tokio_serial::Parity::Odd,
            Parity::Even => tokio_serial::Parity::Even,
        }
    }
}

/// Serial port configuration.
#[derive(Debug, Clone, Copy)]
pub struct SerialConfig {
    pub baud_rate: u32,
    pub data_bits: u8,
    pub stop_bits: u8,
    pub parity: Parity,
}

impl Default for SerialConfig {
    fn default() -> Self {
        Self {
            baud_rate: 115200,
            data_bits: 8,
            stop_bits: 1,
            parity: Parity::None,
        }
    }
}

/// Serial port error types.
#[derive(Debug, thiserror::Error)]
pub enum SerialError {
    #[error("Failed to open serial port: {0}")]
    OpenError(#[source] io::Error),

    #[error("Unsupported baud rate: {0}")]
    UnsupportedBaudRate(u32),

    #[error("Configuration failed: {0}")]
    ConfigError(String),

    #[error("I/O error: {0}")]
    IoError(#[from] io::Error),

    #[error("Serial port disconnected")]
    Disconnected,

    #[error("Hardware error on serial port")]
    HardwareError,

    #[error("Operation timed out")]
    Timeout,
}

/// Statistics for a serial port.
#[derive(Debug, Clone, Copy)]
pub struct SerialStats {
    /// Total bytes read from the port.
    pub bytes_read: u64,
    /// Total bytes written to the port.
    pub bytes_written: u64,
    /// Current baud rate.
    pub baud_rate: u32,
}

/// A serial stream implementation that supports runtime reconfiguration.
pub struct SerialStream {
    inner: Arc<SerialInner>,
}

struct SerialInner {
    /// The underlying tokio-serial stream. Guarded by a lock so
    /// `SerialControl` can reconfigure it (e.g. change baud rate) while
    /// `SerialReader`/`SerialWriter` are concurrently performing I/O.
    ///
    /// `tokio_serial::SerialStream` is `Send`, so `RwLock<TokioSerialStream>`
    /// is `Send + Sync` without needing any `unsafe impl`.
    stream: RwLock<TokioSerialStream>,

    /// Current configuration - atomic for lock-free reads.
    baud_rate: AtomicU32,
    data_bits: AtomicU8,
    stop_bits: AtomicU8,
    parity: AtomicU8, // 0 = None, 1 = Odd, 2 = Even

    /// Lock held only for the duration of an actual reconfiguration.
    reconfig_lock: RwLock<()>,

    /// Statistics (lock-free).
    bytes_read: AtomicU64,
    bytes_written: AtomicU64,
}

/// Reader half of a split serial stream.
pub struct SerialReader {
    inner: Arc<SerialInner>,
}

/// Writer half of a split serial stream.
pub struct SerialWriter {
    inner: Arc<SerialInner>,
}

/// Control handle for a split serial stream.
pub struct SerialControl {
    inner: Arc<SerialInner>,
}

/// Build a `tokio_serial::SerialStream` from a `SerialConfig`.
fn build_stream(path: &str, config: &SerialConfig) -> Result<TokioSerialStream, SerialError> {
    let data_bits = match config.data_bits {
        5 => tokio_serial::DataBits::Five,
        6 => tokio_serial::DataBits::Six,
        7 => tokio_serial::DataBits::Seven,
        8 => tokio_serial::DataBits::Eight,
        _ => {
            return Err(SerialError::ConfigError(format!(
                "Invalid data bits: {}",
                config.data_bits
            )));
        }
    };

    let stop_bits = match config.stop_bits {
        1 => tokio_serial::StopBits::One,
        2 => tokio_serial::StopBits::Two,
        _ => {
            return Err(SerialError::ConfigError(format!(
                "Invalid stop bits: {}",
                config.stop_bits
            )));
        }
    };

    tokio_serial::new(path, config.baud_rate)
        .data_bits(data_bits)
        .stop_bits(stop_bits)
        .parity(config.parity.into())
        .timeout(Duration::from_millis(100))
        .open_native_async()
        .map_err(|e| SerialError::OpenError(io::Error::other(e)))
}

impl SerialStream {
    /// Open a new serial port with the specified baud rate.
    ///
    /// Uses default configuration of 8N1 (8 data bits, no parity, 1 stop bit).
    pub fn new(path: &str, baud_rate: u32) -> Result<Self, SerialError> {
        let config = SerialConfig {
            baud_rate,
            ..Default::default()
        };
        Self::with_config(path, config)
    }

    /// Open a new serial port with the specified configuration.
    ///
    /// This is synchronous to match the Unix implementation's API.
    /// `open_native_async` does not block on I/O; it constructs the OS handle
    /// and binds it to the current Tokio reactor (a `Handle` must already be
    /// current, i.e. this must be called from within a Tokio runtime context,
    /// exactly as on the Unix path via `AsyncFd::new`).
    pub fn with_config(path: &str, config: SerialConfig) -> Result<Self, SerialError> {
        let stream = build_stream(path, &config)?;

        Ok(Self {
            inner: Arc::new(SerialInner {
                stream: RwLock::new(stream),
                baud_rate: AtomicU32::new(config.baud_rate),
                data_bits: AtomicU8::new(config.data_bits),
                stop_bits: AtomicU8::new(config.stop_bits),
                parity: AtomicU8::new(config.parity as u8),
                reconfig_lock: RwLock::new(()),
                bytes_read: AtomicU64::new(0),
                bytes_written: AtomicU64::new(0),
            }),
        })
    }

    /// Split the stream into reader, writer, and control handles.
    ///
    /// This allows concurrent reading and writing while maintaining the
    /// ability to reconfigure the port.
    pub fn split(self) -> (SerialReader, SerialWriter, SerialControl) {
        (
            SerialReader {
                inner: self.inner.clone(),
            },
            SerialWriter {
                inner: self.inner.clone(),
            },
            SerialControl { inner: self.inner },
        )
    }
}

impl AsyncRead for SerialReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let mut stream = self.inner.stream.write();
        let result = Pin::new(&mut *stream).poll_read(cx, buf);
        if result.is_ready() {
            let n = buf.filled().len() - before;
            if n > 0 {
                self.inner.bytes_read.fetch_add(n as u64, Ordering::Relaxed);
            }
        }
        result
    }
}

impl AsyncWrite for SerialWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut stream = self.inner.stream.write();
        let result = Pin::new(&mut *stream).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = &result
            && *n > 0
        {
            self.inner
                .bytes_written
                .fetch_add(*n as u64, Ordering::Relaxed);
        }
        result
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut stream = self.inner.stream.write();
        Pin::new(&mut *stream).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut stream = self.inner.stream.write();
        Pin::new(&mut *stream).poll_shutdown(cx)
    }
}

impl SerialControl {
    /// Change the baud rate of the serial port.
    ///
    /// Thread-safe and callable while I/O is in progress. See the module-level
    /// docs for a note on drain semantics differing from the Unix version.
    pub fn set_baud_rate(&self, baud_rate: u32) -> Result<(), SerialError> {
        const TIMEOUT: Duration = Duration::from_secs(5);

        let _lock = match self.inner.reconfig_lock.try_write_for(TIMEOUT) {
            Some(guard) => guard,
            None => {
                return Err(SerialError::ConfigError(
                    "Failed to acquire configuration lock - possible deadlock".to_string(),
                ));
            }
        };

        let mut stream = self.inner.stream.write();
        stream
            .set_baud_rate(baud_rate)
            .map_err(|e| SerialError::ConfigError(format!("Failed to set baud rate: {}", e)))?;
        drop(stream);

        self.inner.baud_rate.store(baud_rate, Ordering::Release);

        Ok(())
    }

    /// Get the current baud rate.
    pub fn current_baud_rate(&self) -> u32 {
        self.inner.baud_rate.load(Ordering::Acquire)
    }

    /// Get the current data bits configuration.
    pub fn current_data_bits(&self) -> u8 {
        self.inner.data_bits.load(Ordering::Acquire)
    }

    /// Get the current stop bits configuration.
    pub fn current_stop_bits(&self) -> u8 {
        self.inner.stop_bits.load(Ordering::Acquire)
    }

    /// Get the current parity configuration.
    pub fn current_parity(&self) -> Parity {
        match self.inner.parity.load(Ordering::Acquire) {
            1 => Parity::Odd,
            2 => Parity::Even,
            _ => Parity::None,
        }
    }

    /// Get the current serial port configuration.
    pub fn current_config(&self) -> SerialConfig {
        SerialConfig {
            baud_rate: self.current_baud_rate(),
            data_bits: self.current_data_bits(),
            stop_bits: self.current_stop_bits(),
            parity: self.current_parity(),
        }
    }

    /// Get statistics about the serial port.
    pub fn stats(&self) -> SerialStats {
        SerialStats {
            bytes_read: self.inner.bytes_read.load(Ordering::Relaxed),
            bytes_written: self.inner.bytes_written.load(Ordering::Relaxed),
            baud_rate: self.current_baud_rate(),
        }
    }

    /// Reset the statistics counters to zero.
    pub fn reset_stats(&self) {
        self.inner.bytes_read.store(0, Ordering::Relaxed);
        self.inner.bytes_written.store(0, Ordering::Relaxed);
    }
}
