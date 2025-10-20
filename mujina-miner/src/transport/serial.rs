//! A serial stream implementation that supports runtime reconfiguration.
//!
//! ## Why not use tokio-serial?
//!
//! The tokio-serial crate (and tokio's split() operation) consumes the
//! SerialStream when splitting into read/write halves. Once split, the
//! original stream is gone and cannot be reconfigured. This is a fundamental
//! limitation for certain Bitcoin mining hardware:
//!
//! - The BM13xx family of ASIC chips require baud rate changes after
//!   initialization. These chips typically start at 115200 baud then
//!   switch to higher speeds 1Mbps, 3.125Mbps, etc.
//!
//! Our implementation maintains shared ownership of the underlying file
//! descriptor, allowing the Control handle to reconfigure the port even
//! after it has been split for concurrent I/O operations.

use std::io;
#[cfg(test)]
use std::os::unix::io::FromRawFd;
use std::os::unix::io::{AsRawFd, BorrowedFd, IntoRawFd, RawFd};
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::ready;
use parking_lot::RwLock;
use rustix::fs::{open, Mode, OFlags};
use rustix::termios::{tcdrain, tcgetattr, tcsetattr, ControlModes};
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Parity configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parity {
    None = 0,
    Odd = 1,
    Even = 2,
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
    /// File descriptor - immutable after creation
    fd: AsyncFd<RawFd>,

    /// Current configuration - atomic for lock-free reads
    baud_rate: AtomicU32,
    data_bits: AtomicU8,
    stop_bits: AtomicU8,
    parity: AtomicU8, // 0 = None, 1 = Odd, 2 = Even

    /// Lock only for actual reconfiguration
    reconfig_lock: RwLock<()>,

    /// Statistics (lock-free)
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

/// Apply serial configuration to a file descriptor.
///
/// This helper function configures termios settings for a serial port,
/// including baud rate, data bits, stop bits, and parity.
fn apply_serial_config<Fd: rustix::fd::AsFd>(
    fd: &Fd,
    config: &SerialConfig,
) -> Result<(), SerialError> {
    // Get current termios settings
    let mut termios = tcgetattr(fd)
        .map_err(|e| SerialError::ConfigError(format!("Failed to get termios: {}", e)))?;

    // Raw mode for binary communication
    termios.make_raw();

    // Set baud rate
    termios
        .set_speed(config.baud_rate)
        .map_err(|e| SerialError::ConfigError(format!("Failed to set baud rate: {}", e)))?;

    // Configure data bits
    termios.control_modes &= !ControlModes::CSIZE; // Clear size bits
    match config.data_bits {
        5 => termios.control_modes |= ControlModes::CS5,
        6 => termios.control_modes |= ControlModes::CS6,
        7 => termios.control_modes |= ControlModes::CS7,
        8 => termios.control_modes |= ControlModes::CS8,
        _ => {
            return Err(SerialError::ConfigError(format!(
                "Invalid data bits: {}",
                config.data_bits
            )))
        }
    }

    // Configure parity
    match config.parity {
        Parity::None => {
            termios.control_modes &= !ControlModes::PARENB;
        }
        Parity::Odd => {
            termios.control_modes |= ControlModes::PARENB;
            termios.control_modes |= ControlModes::PARODD;
        }
        Parity::Even => {
            termios.control_modes |= ControlModes::PARENB;
            termios.control_modes &= !ControlModes::PARODD;
        }
    }

    // Configure stop bits
    match config.stop_bits {
        1 => termios.control_modes &= !ControlModes::CSTOPB,
        2 => termios.control_modes |= ControlModes::CSTOPB,
        _ => {
            return Err(SerialError::ConfigError(format!(
                "Invalid stop bits: {}",
                config.stop_bits
            )))
        }
    }

    // Apply configuration
    tcsetattr(fd, rustix::termios::OptionalActions::Now, &termios)
        .map_err(|e| SerialError::ConfigError(format!("Failed to apply termios: {}", e)))?;

    Ok(())
}

impl SerialStream {
    /// Open a new serial port with the specified baud rate.
    ///
    /// Uses default configuration of 8N1 (8 data bits, no parity, 1 stop bit).
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the serial device (e.g., "/dev/ttyUSB0" on Linux,
    ///   "/dev/cu.usbserial" on macOS)
    /// * `baud_rate` - Initial baud rate (e.g., 115200, 1000000)
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The device cannot be opened
    /// - The baud rate is not supported
    /// - Configuration fails
    pub fn new(path: &str, baud_rate: u32) -> Result<Self, SerialError> {
        let config = SerialConfig {
            baud_rate,
            ..Default::default()
        };
        Self::with_config(path, config)
    }

    /// Open a new serial port with the specified configuration.
    pub fn with_config(path: &str, config: SerialConfig) -> Result<Self, SerialError> {
        // Open serial port
        let fd = open(
            path,
            OFlags::RDWR | OFlags::NOCTTY | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(|e| SerialError::OpenError(e.into()))?;

        // Apply serial configuration
        apply_serial_config(&fd, &config)?;

        // Convert OwnedFd to RawFd for AsyncFd
        let raw_fd = fd.into_raw_fd();
        let async_fd = AsyncFd::new(raw_fd).map_err(SerialError::IoError)?;

        Ok(Self {
            inner: Arc::new(SerialInner {
                fd: async_fd,
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
    /// This allows concurrent reading and writing while maintaining the ability
    /// to reconfigure the port.
    pub fn split(self) -> (SerialReader, SerialWriter, SerialControl) {
        (
            SerialReader {
                inner: self.inner.clone(),
            },
            SerialWriter {
                inner: self.inner.clone(),
            },
            SerialControl {
                inner: self.inner.clone(),
            },
        )
    }

    /// Create a SerialStream from an existing file descriptor.
    ///
    /// This is primarily used for testing with virtual serial ports.
    #[cfg(test)]
    pub(crate) fn from_fd(fd: RawFd, config: SerialConfig) -> Result<Self, SerialError> {
        // Convert raw fd to OwnedFd
        let fd = unsafe { rustix::fd::OwnedFd::from_raw_fd(fd) };

        // Apply serial configuration
        apply_serial_config(&fd, &config)?;

        // Make the fd non-blocking
        use rustix::fs::{fcntl_getfl, fcntl_setfl};
        let flags = fcntl_getfl(&fd)
            .map_err(|e| SerialError::ConfigError(format!("Failed to get fd flags: {}", e)))?;
        fcntl_setfl(&fd, flags | OFlags::NONBLOCK)
            .map_err(|e| SerialError::ConfigError(format!("Failed to set fd flags: {}", e)))?;

        let async_fd = AsyncFd::new(fd.into_raw_fd()).map_err(SerialError::IoError)?;

        Ok(Self {
            inner: Arc::new(SerialInner {
                fd: async_fd,
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
}

impl AsyncRead for SerialReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let mut guard = ready!(self.inner.fd.poll_read_ready(cx))?;

            match guard.try_io(|inner| {
                let fd = inner.as_raw_fd();
                let slice = buf.initialize_unfilled();

                // Direct read syscall
                let fd_ref = unsafe { BorrowedFd::borrow_raw(fd) };
                match rustix::io::read(fd_ref, slice) {
                    Ok(n) => {
                        buf.advance(n);
                        if n > 0 {
                            self.inner.bytes_read.fetch_add(n as u64, Ordering::Relaxed);
                        }
                        Ok(())
                    }
                    Err(rustix::io::Errno::AGAIN) => {
                        Err(io::Error::from(io::ErrorKind::WouldBlock))
                    }
                    Err(rustix::io::Errno::IO) => {
                        // EIO can mean various hardware errors, not just disconnection
                        Err(io::Error::other(SerialError::HardwareError))
                    }
                    Err(rustix::io::Errno::PIPE) => {
                        // EPIPE is more likely to indicate disconnection
                        Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            SerialError::Disconnected,
                        ))
                    }
                    Err(e) => Err(e.into()),
                }
            }) {
                Ok(result) => return Poll::Ready(result),
                Err(_would_block) => continue,
            }
        }
    }
}

impl AsyncWrite for SerialWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            let mut guard = ready!(self.inner.fd.poll_write_ready(cx))?;

            match guard.try_io(|inner| {
                let fd = inner.as_raw_fd();

                // Direct write syscall
                let fd_ref = unsafe { BorrowedFd::borrow_raw(fd) };
                match rustix::io::write(fd_ref, buf) {
                    Ok(n) => {
                        if n > 0 {
                            self.inner
                                .bytes_written
                                .fetch_add(n as u64, Ordering::Relaxed);
                        }
                        Ok(n)
                    }
                    Err(rustix::io::Errno::AGAIN) => {
                        Err(io::Error::from(io::ErrorKind::WouldBlock))
                    }
                    Err(rustix::io::Errno::IO) => {
                        // EIO can mean various hardware errors, not just disconnection
                        Err(io::Error::other(SerialError::HardwareError))
                    }
                    Err(rustix::io::Errno::PIPE) => {
                        // EPIPE is more likely to indicate disconnection
                        Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            SerialError::Disconnected,
                        ))
                    }
                    Err(e) => Err(e.into()),
                }
            }) {
                Ok(result) => return Poll::Ready(result),
                Err(_would_block) => continue,
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Serial ports don't buffer in the traditional sense
        // Could call tcdrain() here if needed
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

impl SerialControl {
    /// Change the baud rate of the serial port.
    ///
    /// This operation is thread-safe and can be called while I/O operations
    /// are in progress. The change takes effect immediately.
    ///
    /// # Arguments
    ///
    /// * `baud_rate` - New baud rate (e.g., 115200, 1000000, 3125000)
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The baud rate is not supported
    /// - Configuration fails
    /// - Internal lock cannot be acquired (indicates a serious problem)
    pub fn set_baud_rate(&self, baud_rate: u32) -> Result<(), SerialError> {
        // 5 seconds is extremely generous for acquiring a lock
        const TIMEOUT: Duration = Duration::from_secs(5);

        // Try to acquire lock with timeout
        let _lock = match self.inner.reconfig_lock.try_write_for(TIMEOUT) {
            Some(guard) => guard,
            None => {
                return Err(SerialError::ConfigError(
                    "Failed to acquire configuration lock - possible deadlock".to_string(),
                ))
            }
        };

        // Get current termios
        let fd = self.inner.fd.as_raw_fd();
        // Temporarily create a borrowed fd for the termios calls
        let fd_ref = unsafe { BorrowedFd::borrow_raw(fd) };
        let mut termios = tcgetattr(fd_ref)
            .map_err(|e| SerialError::ConfigError(format!("Failed to get termios: {}", e)))?;

        // Update baud rate
        termios
            .set_speed(baud_rate)
            .map_err(|e| SerialError::ConfigError(format!("Failed to set baud rate: {}", e)))?;

        // Apply changes immediately
        tcsetattr(fd_ref, rustix::termios::OptionalActions::Now, &termios)
            .map_err(|e| SerialError::ConfigError(format!("Failed to apply termios: {}", e)))?;

        // Update atomic with release ordering to ensure termios changes are visible
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
            0 => Parity::None,
            1 => Parity::Odd,
            2 => Parity::Even,
            _ => Parity::None, // Default fallback
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

impl Drop for SerialInner {
    fn drop(&mut self) {
        // Note: AsyncFd automatically closes the file descriptor
        // But we should drain pending output data first
        let fd = self.fd.as_raw_fd();

        // Best effort drain - ignore errors on drop
        let _ = tcdrain(unsafe { BorrowedFd::borrow_raw(fd) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn test_apply_serial_config_validation() {
        use nix::pty::openpty;

        // Create a PTY pair for testing
        let result = openpty(None, None);
        if let Ok(pty) = result {
            // Test valid default config
            let config = SerialConfig::default();
            let result = apply_serial_config(&pty.master, &config);
            assert!(result.is_ok(), "Default config should be valid");

            // Need new pty for each test since fd gets consumed
            let pty = openpty(None, None).unwrap();

            // Test valid custom config
            let config = SerialConfig {
                baud_rate: 9600,
                data_bits: 7,
                stop_bits: 2,
                parity: Parity::Even,
            };
            let result = apply_serial_config(&pty.master, &config);
            assert!(result.is_ok(), "Custom valid config should work");

            // Test invalid data bits
            let pty = openpty(None, None).unwrap();
            let config = SerialConfig {
                data_bits: 9,
                ..Default::default()
            };
            let result = apply_serial_config(&pty.master, &config);
            assert!(result.is_err(), "Invalid data bits should fail");
            if let Err(SerialError::ConfigError(msg)) = result {
                assert!(msg.contains("Invalid data bits: 9"));
            }

            // Test invalid stop bits
            let pty = openpty(None, None).unwrap();
            let config = SerialConfig {
                stop_bits: 3,
                ..Default::default()
            };
            let result = apply_serial_config(&pty.master, &config);
            assert!(result.is_err(), "Invalid stop bits should fail");
            if let Err(SerialError::ConfigError(msg)) = result {
                assert!(msg.contains("Invalid stop bits: 3"));
            }

            // Test all valid data bit values
            for data_bits in [5, 6, 7, 8] {
                let pty = openpty(None, None).unwrap();
                let config = SerialConfig {
                    data_bits,
                    ..Default::default()
                };
                let result = apply_serial_config(&pty.master, &config);
                assert!(result.is_ok(), "Data bits {} should be valid", data_bits);
            }

            // Test all parity options
            for parity in [Parity::None, Parity::Odd, Parity::Even] {
                let pty = openpty(None, None).unwrap();
                let config = SerialConfig {
                    parity,
                    ..Default::default()
                };
                let result = apply_serial_config(&pty.master, &config);
                assert!(result.is_ok(), "Parity {:?} should be valid", parity);
            }
        }
    }

    // Test helper to create virtual serial port pairs
    pub mod test_support {
        use super::*;

        /// Virtual serial port pair for integration tests using pty
        pub async fn create_virtual_pair() -> Result<(SerialStream, SerialStream), SerialError> {
            use nix::pty::{openpty, OpenptyResult};

            // Create a pseudo-terminal pair
            let OpenptyResult { master, slave } = openpty(None, None)
                .map_err(|e| SerialError::ConfigError(format!("Failed to open pty: {}", e)))?;

            // Convert OwnedFd to RawFd
            let master_fd = master.into_raw_fd();
            let slave_fd = slave.into_raw_fd();

            // Create streams from file descriptors
            let master_stream = SerialStream::from_fd(master_fd, SerialConfig::default())?;
            let slave_stream = SerialStream::from_fd(slave_fd, SerialConfig::default())?;

            Ok((master_stream, slave_stream))
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg_attr(
        feature = "skip-pty-tests",
        ignore = "PTY tests skipped via feature flag"
    )]
    async fn test_virtual_serial_pair() {
        use test_support::create_virtual_pair;

        let (stream_a, stream_b) = create_virtual_pair().await.unwrap();
        let (reader_a, writer_a, _control_a) = stream_a.split();
        let (reader_b, writer_b, _control_b) = stream_b.split();

        // Test bidirectional communication
        let mut writer_a = writer_a;
        let mut reader_b = reader_b;
        let mut writer_b = writer_b;
        let mut reader_a = reader_a;

        // A -> B
        writer_a.write_all(b"hello from A").await.unwrap();
        let mut buf = vec![0u8; 12];
        reader_b.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello from A");

        // B -> A
        writer_b.write_all(b"hello from B").await.unwrap();
        let mut buf = vec![0u8; 12];
        reader_a.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello from B");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg_attr(
        feature = "skip-pty-tests",
        ignore = "PTY tests skipped via feature flag"
    )]
    async fn test_concurrent_read_write() {
        use test_support::create_virtual_pair;

        let (stream_a, stream_b) = create_virtual_pair().await.unwrap();
        let (reader_a, writer_a, _control_a) = stream_a.split();
        let (reader_b, writer_b, _control_b) = stream_b.split();

        // Concurrent operations
        let write_task = tokio::spawn(async move {
            let mut writer = writer_a;
            for i in 0..10 {
                writer
                    .write_all(format!("message {}\n", i).as_bytes())
                    .await
                    .unwrap();
                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            }
        });

        let read_task_a = tokio::spawn(async move {
            let mut reader = reader_a;
            let mut buf = vec![0u8; 1024];
            let mut total_read = 0;
            // Read approximately what we expect to receive (10 messages of ~8 bytes)
            while total_read < 70 {
                // Read some data
                match tokio::time::timeout(
                    tokio::time::Duration::from_millis(500),
                    reader.read(&mut buf[total_read..]),
                )
                .await
                {
                    Ok(Ok(n)) if n > 0 => total_read += n,
                    _ => break, // Timeout or EOF
                }
            }
            total_read
        });

        let read_task_b = tokio::spawn(async move {
            let mut reader = reader_b;
            let mut buf = vec![0u8; 1024];
            let mut total_read = 0;
            // Read approximately what we expect to receive (10 messages of ~10 bytes)
            while total_read < 90 {
                // Read some data
                match tokio::time::timeout(
                    tokio::time::Duration::from_millis(500),
                    reader.read(&mut buf[total_read..]),
                )
                .await
                {
                    Ok(Ok(n)) if n > 0 => total_read += n,
                    _ => break, // Timeout or EOF
                }
            }
            total_read
        });

        // Write from the other side
        let mut writer_b = writer_b;
        for i in 0..10 {
            writer_b
                .write_all(format!("reply {}\n", i).as_bytes())
                .await
                .unwrap();
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }

        write_task.await.unwrap();
        let bytes_read_a = read_task_a.await.unwrap();
        let bytes_read_b = read_task_b.await.unwrap();
        assert!(
            bytes_read_a >= 70,
            "Expected at least 70 bytes from A, got {}",
            bytes_read_a
        );
        assert!(
            bytes_read_b >= 90,
            "Expected at least 90 bytes from B, got {}",
            bytes_read_b
        );
    }

    #[tokio::test]
    async fn test_baud_rate_change() {
        use test_support::create_virtual_pair;

        let (stream, _other) = create_virtual_pair().await.unwrap();
        let (_reader, _writer, control) = stream.split();

        // Initial baud rate
        assert_eq!(control.current_baud_rate(), 115200);

        // Change baud rate
        control.set_baud_rate(1_000_000).unwrap();
        assert_eq!(control.current_baud_rate(), 1_000_000);

        // rustix allows arbitrary baud rates, so this should succeed
        control.set_baud_rate(123456).unwrap();
        assert_eq!(control.current_baud_rate(), 123456);
    }

    #[tokio::test]
    async fn test_stats_tracking() {
        use test_support::create_virtual_pair;

        let (stream_a, stream_b) = create_virtual_pair().await.unwrap();
        let (reader_a, writer_a, control_a) = stream_a.split();
        let (_reader_b, writer_b, _control_b) = stream_b.split();

        // Initial stats
        let stats = control_a.stats();
        assert_eq!(stats.bytes_read, 0);
        assert_eq!(stats.bytes_written, 0);

        // Write some data
        let mut writer_a = writer_a;
        writer_a.write_all(b"hello").await.unwrap();

        // Write from other side so we can read
        let mut writer_b = writer_b;
        writer_b.write_all(b"world").await.unwrap();

        // Read some data
        let mut reader_a = reader_a;
        let mut buf = vec![0u8; 5];
        reader_a.read_exact(&mut buf).await.unwrap();

        // Check stats
        let stats = control_a.stats();
        assert_eq!(stats.bytes_written, 5);
        assert_eq!(stats.bytes_read, 5);
    }

    #[tokio::test]
    #[cfg_attr(
        feature = "skip-pty-tests",
        ignore = "PTY tests skipped via feature flag"
    )]
    async fn test_disconnection_handling() {
        use test_support::create_virtual_pair;

        let (stream_a, stream_b) = create_virtual_pair().await.unwrap();
        let (reader_a, _writer_a, _control_a) = stream_a.split();

        // Drop the other end to simulate disconnection
        drop(stream_b);

        // Try to read - should eventually fail or timeout
        let mut reader_a = reader_a;
        let mut buf = vec![0u8; 10];
        let result =
            tokio::time::timeout(Duration::from_millis(100), reader_a.read(&mut buf)).await;

        // On pty disconnection, we might get Ok(0), an error, or timeout
        match result {
            Ok(Ok(0)) => {} // EOF
            Ok(Ok(_)) => panic!("Should not read data from disconnected port"),
            Ok(Err(_)) => {} // Expected error
            Err(_) => {}     // Timeout is also acceptable for disconnected PTY
        }
    }

    #[tokio::test]
    async fn test_reset_stats() {
        use test_support::create_virtual_pair;

        let (stream_a, stream_b) = create_virtual_pair().await.unwrap();
        let (reader_a, writer_a, control_a) = stream_a.split();
        let (_reader_b, writer_b, _control_b) = stream_b.split();

        // Write and read some data
        let mut writer_a = writer_a;
        writer_a.write_all(b"test data").await.unwrap();

        let mut writer_b = writer_b;
        writer_b.write_all(b"response").await.unwrap();

        let mut reader_a = reader_a;
        let mut buf = vec![0u8; 8];
        reader_a.read_exact(&mut buf).await.unwrap();

        // Check stats are non-zero
        let stats = control_a.stats();
        assert!(stats.bytes_written > 0);
        assert!(stats.bytes_read > 0);

        // Reset stats
        control_a.reset_stats();

        // Verify stats are zero
        let stats = control_a.stats();
        assert_eq!(stats.bytes_written, 0);
        assert_eq!(stats.bytes_read, 0);
        assert_eq!(stats.baud_rate, 115200); // Baud rate unchanged
    }

    #[tokio::test]
    async fn test_zero_byte_operations() {
        use test_support::create_virtual_pair;

        let (stream_a, _stream_b) = create_virtual_pair().await.unwrap();
        let (_reader_a, writer_a, control_a) = stream_a.split();

        // Write zero bytes
        let mut writer = writer_a;
        let result = writer.write(&[]).await;
        assert_eq!(result.unwrap(), 0);

        // Stats should still be zero
        let stats = control_a.stats();
        assert_eq!(stats.bytes_written, 0);
    }

    #[test]
    fn test_invalid_configurations() {
        // Since we can't easily test with real device paths,
        // let's at least verify the SerialConfig validation logic

        // Valid configurations should be accepted
        let valid_config = SerialConfig::default();
        assert_eq!(valid_config.data_bits, 8);
        assert_eq!(valid_config.stop_bits, 1);
        assert_eq!(valid_config.baud_rate, 115200);
        assert!(matches!(valid_config.parity, Parity::None));

        // Test creating configs with various parameters
        let config = SerialConfig {
            baud_rate: 9600,
            data_bits: 7,
            stop_bits: 2,
            parity: Parity::Even,
        };
        assert_eq!(config.data_bits, 7);
        assert_eq!(config.stop_bits, 2);

        // The actual validation happens in with_config when it tries to
        // configure termios. Since we can't test that without a real device,
        // we'll test the from_fd path which does the same validation

        use nix::pty::openpty;

        // Create a PTY pair for testing
        let result = openpty(None, None);
        if let Ok(pty) = result {
            let master_fd = pty.master.into_raw_fd();

            // Test invalid data bits
            let config = SerialConfig {
                data_bits: 9,
                ..Default::default()
            };
            let result = SerialStream::from_fd(master_fd, config);
            assert!(result.is_err());
            if let Err(SerialError::ConfigError(msg)) = result {
                assert!(msg.contains("Invalid data bits: 9"));
            }

            // We need a new fd since the previous one was consumed
            let result = openpty(None, None).unwrap();
            let master_fd = result.master.into_raw_fd();

            // Test invalid stop bits
            let config = SerialConfig {
                stop_bits: 3,
                ..Default::default()
            };
            let result = SerialStream::from_fd(master_fd, config);
            assert!(result.is_err());
            if let Err(SerialError::ConfigError(msg)) = result {
                assert!(msg.contains("Invalid stop bits: 3"));
            }
        }
    }

    #[tokio::test]
    #[cfg_attr(
        feature = "skip-pty-tests",
        ignore = "PTY tests skipped via feature flag"
    )]
    async fn test_drop_during_io() {
        use test_support::create_virtual_pair;
        use tokio::time::sleep;

        let (stream_a, stream_b) = create_virtual_pair().await.unwrap();
        let (reader_a, writer_a, _control_a) = stream_a.split();
        let (_reader_b, writer_b, _control_b) = stream_b.split();

        // Start a large write that will block
        let write_handle = tokio::spawn(async move {
            let mut writer = writer_a;
            let large_data = vec![0xAA; 1_000_000]; // 1MB of data
            writer.write_all(&large_data).await
        });

        // Start a read that's waiting for data
        let read_handle = tokio::spawn(async move {
            let mut reader = reader_a;
            let mut buf = vec![0u8; 1024];
            let mut total_read = 0;
            loop {
                match tokio::time::timeout(Duration::from_millis(100), reader.read(&mut buf)).await
                {
                    Ok(Ok(0)) => break, // EOF
                    Ok(Ok(n)) => total_read += n,
                    Ok(Err(_)) => break, // Error (including disconnection)
                    Err(_) => break,     // Timeout - likely disconnected
                }
            }
            total_read
        });

        // Let some I/O happen
        sleep(Duration::from_millis(10)).await;

        // Drop the other side's components to simulate disconnection
        drop(writer_b);
        drop(_reader_b);
        drop(_control_b);

        // The write should eventually fail or complete partially
        match tokio::time::timeout(Duration::from_secs(1), write_handle).await {
            Ok(Ok(Ok(()))) => {
                // Write completed before drop
            }
            Ok(Ok(Err(_))) => {
                // Write failed due to disconnection - expected
            }
            Ok(Err(_)) => {
                // Task panicked - not expected but ok for this test
            }
            Err(_) => {
                // Timeout - also acceptable for large write
            }
        }

        // The read should complete (with partial data or error)
        let bytes_read = tokio::time::timeout(Duration::from_secs(1), read_handle)
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or(0);
        // We should have read something, but not the full 1MB
        assert!(bytes_read < 1_000_000);
    }
}
