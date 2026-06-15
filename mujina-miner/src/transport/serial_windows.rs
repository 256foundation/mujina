//! Windows serial stream implementation using tokio-serial.
//!
//! This provides a Windows-compatible version of SerialStream that matches
//! the Unix implementation's API but uses tokio-serial internally.

use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
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

impl From<tokio_serial::Parity> for Parity {
    fn from(p: tokio_serial::Parity) -> Self {
        match p {
            tokio_serial::Parity::None => Parity::None,
            tokio_serial::Parity::Odd => Parity::Odd,
            tokio_serial::Parity::Even => Parity::Even,
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
    /// The underlying tokio-serial stream
    stream: RwLock<TokioSerialStream>,

    /// Current configuration - atomic for lock-free reads
    baud_rate: AtomicU32,
    data_bits: AtomicU8,
    stop_bits: AtomicU8,
    parity: AtomicU8, // 0 = None, 1 = Odd, 2 = Even

    /// Lock for reconfiguration
    reconfig_lock: RwLock<()>,

    /// Statistics (lock-free)
    bytes_read: AtomicU64,
    bytes_written: AtomicU64,
}

// SAFETY: SerialInner is Send because:
// - The TokioSerialStream is always accessed through an RwLock, which provides proper synchronization
// - All atomic fields are inherently Send
// - We never expose raw pointers or unsynchronized mutable access
unsafe impl Send for SerialInner {}

// SAFETY: SerialInner is Sync because:
// - All access to the TokioSerialStream goes through RwLock, preventing concurrent mutations
// - Atomic fields are inherently Sync
// - The RwLock<()> for reconfiguration ensures exclusive access during config changes
unsafe impl Sync for SerialInner {}

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

impl SerialStream {
    /// Open a new serial port with the specified baud rate.
    ///
    /// Uses default configuration of 8N1 (8 data bits, no parity, 1 stop bit).
    /// This is a blocking wrapper around the async `open()` method for API compatibility.
    pub fn new(path: &str, baud_rate: u32) -> Result<Self, SerialError> {
        let config = SerialConfig {
            baud_rate,
            ..Default::default()
        };
        // Block on the async open
        tokio::runtime::Handle::current().block_on(Self::open(path, config))
    }

    /// Open a serial port with the specified configuration (async version).
    pub async fn open(path: &str, config: SerialConfig) -> Result<Self, SerialError> {
        // Build the serial port
        let stream = tokio_serial::new(path, config.baud_rate)
            .data_bits(match config.data_bits {
                5 => tokio_serial::DataBits::Five,
                6 => tokio_serial::DataBits::Six,
                7 => tokio_serial::DataBits::Seven,
                8 => tokio_serial::DataBits::Eight,
                _ => return Err(SerialError::ConfigError(format!("Invalid data bits: {}", config.data_bits))),
            })
            .stop_bits(match config.stop_bits {
                1 => tokio_serial::StopBits::One,
                2 => tokio_serial::StopBits::Two,
                _ => return Err(SerialError::ConfigError(format!("Invalid stop bits: {}", config.stop_bits))),
            })
            .parity(config.parity.into())
            .timeout(Duration::from_millis(100))
            .open_native_async()
            .map_err(|e| SerialError::ConfigError(format!("Failed to open serial port: {}", e)))?;

        let inner = SerialInner {
            stream: RwLock::new(stream),
            baud_rate: AtomicU32::new(config.baud_rate),
            data_bits: AtomicU8::new(config.data_bits),
            stop_bits: AtomicU8::new(config.stop_bits),
            parity: AtomicU8::new(config.parity as u8),
            reconfig_lock: RwLock::new(()),
            bytes_read: AtomicU64::new(0),
            bytes_written: AtomicU64::new(0),
        };

        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// Split the stream into reader, writer, and control handles.
    pub fn split(self) -> (SerialReader, SerialWriter, SerialControl) {
        (
            SerialReader {
                inner: Arc::clone(&self.inner),
            },
            SerialWriter {
                inner: Arc::clone(&self.inner),
            },
            SerialControl {
                inner: self.inner,
            },
        )
    }

    /// Get current statistics.
    pub fn stats(&self) -> SerialStats {
        SerialStats {
            bytes_read: self.inner.bytes_read.load(Ordering::Relaxed),
            bytes_written: self.inner.bytes_written.load(Ordering::Relaxed),
            baud_rate: self.inner.baud_rate.load(Ordering::Relaxed),
        }
    }
}

impl AsyncRead for SerialStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let mut stream = self.inner.stream.write();
        let result = Pin::new(&mut *stream).poll_read(cx, buf);
        let bytes_read = buf.filled().len() - before;
        self.inner.bytes_read.fetch_add(bytes_read as u64, Ordering::Relaxed);
        result
    }
}

impl AsyncWrite for SerialStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut stream = self.inner.stream.write();
        let result = Pin::new(&mut *stream).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = &result {
            self.inner.bytes_written.fetch_add(*n as u64, Ordering::Relaxed);
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

impl AsyncRead for SerialReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let mut stream = self.inner.stream.write();
        let result = Pin::new(&mut *stream).poll_read(cx, buf);
        let bytes_read = buf.filled().len() - before;
        self.inner.bytes_read.fetch_add(bytes_read as u64, Ordering::Relaxed);
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
        if let Poll::Ready(Ok(n)) = &result {
            self.inner.bytes_written.fetch_add(*n as u64, Ordering::Relaxed);
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
    /// Change the baud rate at runtime.
    pub async fn set_baud_rate(&self, baud_rate: u32) -> Result<(), SerialError> {
        let _guard = self.inner.reconfig_lock.write();

        let mut stream = self.inner.stream.write();
        stream.set_baud_rate(baud_rate)
            .map_err(|e| SerialError::ConfigError(format!("Failed to set baud rate: {}", e)))?;

        self.inner.baud_rate.store(baud_rate, Ordering::Relaxed);
        Ok(())
    }

    /// Get current baud rate.
    pub fn baud_rate(&self) -> u32 {
        self.inner.baud_rate.load(Ordering::Relaxed)
    }

    /// Get current statistics.
    pub fn stats(&self) -> SerialStats {
        SerialStats {
            bytes_read: self.inner.bytes_read.load(Ordering::Relaxed),
            bytes_written: self.inner.bytes_written.load(Ordering::Relaxed),
            baud_rate: self.inner.baud_rate.load(Ordering::Relaxed),
        }
    }
}
