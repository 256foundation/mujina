//! TCP connection management with line-delimited I/O.
//!
//! Stratum v1 uses newline-delimited JSON over TCP. This module provides a
//! wrapper around tokio's TCP stream that handles buffered reading and writing
//! of complete JSON-RPC messages. The [`Transport`] trait abstracts message
//! I/O, allowing channel-based mocks for deterministic testing.

use async_trait::async_trait;

use super::error::{StratumError, StratumResult};
use super::messages::JsonRpcMessage;
use crate::tracing::prelude::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

/// Message-level I/O for Stratum protocol.
///
/// Abstracts reading and writing JSON-RPC messages so the client can
/// run over TCP (production) or channels (tests).
#[async_trait]
pub trait Transport: Send {
    /// Read one complete JSON-RPC message.
    ///
    /// Returns `None` on clean connection close (EOF).
    async fn read_message(&mut self) -> StratumResult<Option<JsonRpcMessage>>;

    /// Write a JSON-RPC message.
    async fn write_message(&mut self, msg: &JsonRpcMessage) -> StratumResult<()>;
}

/// Buffered TCP connection for Stratum protocol.
///
/// Wraps a TCP stream with buffered readers/writers optimized for
/// line-delimited JSON messages. Messages are automatically serialized
/// and deserialized, with newlines added/stripped.
pub struct Connection {
    /// Buffered reader for incoming messages
    reader: BufReader<OwnedReadHalf>,

    /// Buffered writer for outgoing messages
    writer: BufWriter<OwnedWriteHalf>,

    /// Line buffer for reading messages
    line_buf: String,
}

impl Connection {
    /// Create a new connection from a TCP stream.
    pub fn new(stream: TcpStream) -> Self {
        // Split the stream for independent reading and writing
        let (read_half, write_half) = stream.into_split();

        Self {
            reader: BufReader::new(read_half),
            writer: BufWriter::new(write_half),
            line_buf: String::with_capacity(4096),
        }
    }

    /// Connect to a Stratum pool.
    ///
    /// Parses the URL, establishes TCP connection, and wraps it in a buffered
    /// connection. Supports both `stratum+tcp://` and plain `tcp://` schemes.
    pub async fn connect(url: &str) -> StratumResult<Self> {
        // Parse URL
        let url = url
            .strip_prefix("stratum+tcp://")
            .or_else(|| url.strip_prefix("tcp://"))
            .unwrap_or(url);

        debug!(url = %url, "Connecting to pool");

        // Connect
        let stream = TcpStream::connect(url)
            .await
            .map_err(|e| StratumError::ConnectionFailed(e.to_string()))?;

        debug!("Connected to pool");

        Ok(Self::new(stream))
    }
}

#[async_trait]
impl Transport for Connection {
    async fn read_message(&mut self) -> StratumResult<Option<JsonRpcMessage>> {
        loop {
            self.line_buf.clear();

            let n = self
                .reader
                .read_line(&mut self.line_buf)
                .await
                .map_err(StratumError::Io)?;

            if n == 0 {
                // EOF - connection closed
                return Ok(None);
            }

            let line = self.line_buf.trim();
            if line.is_empty() {
                // Empty line, skip and read next
                continue;
            }

            trace!(rx = %line, "Received message");

            let msg = serde_json::from_str(line).map_err(|e| {
                StratumError::InvalidMessage(format!("Failed to parse JSON: {}, line: {}", e, line))
            })?;

            return Ok(Some(msg));
        }
    }

    async fn write_message(&mut self, msg: &JsonRpcMessage) -> StratumResult<()> {
        let json = serde_json::to_string(msg)?;
        trace!(tx = %json, "Sending message");

        self.writer.write_all(json.as_bytes()).await?;
        self.writer.write_all(b"\n").await?;
        self.writer.flush().await?;

        Ok(())
    }
}

/// Forwarding impl so `Box<dyn Transport>` satisfies `impl Transport`.
///
/// `async_trait` doesn't auto-derive this, so spell it out. This lets
/// `Connector::connect()` return `Box<dyn Transport>` and callers can
/// pass it straight into `run_with_transport()`.
#[async_trait]
impl Transport for Box<dyn Transport> {
    async fn read_message(&mut self) -> StratumResult<Option<JsonRpcMessage>> {
        (**self).read_message().await
    }

    async fn write_message(&mut self, msg: &JsonRpcMessage) -> StratumResult<()> {
        (**self).write_message(msg).await
    }
}

/// Factory for creating transport connections.
///
/// Production code uses [`TcpConnector`] (TCP via [`Connection::connect`]);
/// tests use [`MockConnector`] to inject channel-backed transports.
#[async_trait]
pub trait Connector: Send {
    /// Create a new transport connection.
    async fn connect(&mut self) -> StratumResult<Box<dyn Transport>>;
}

/// Connects to a Stratum pool over TCP.
pub struct TcpConnector {
    url: String,
}

impl TcpConnector {
    pub fn new(url: String) -> Self {
        Self { url }
    }
}

#[async_trait]
impl Connector for TcpConnector {
    async fn connect(&mut self) -> StratumResult<Box<dyn Transport>> {
        let conn = Connection::connect(&self.url).await?;
        Ok(Box::new(conn))
    }
}

/// Channel-based transport for deterministic testing.
///
/// Backed by tokio mpsc channels rather than TCP, so it works with
/// `tokio::time::pause()` without triggering auto-advance on real I/O.
/// Create a pair with [`MockTransport::pair()`]; the transport is the
/// client's side, the handle is the test's side.
#[cfg(test)]
pub(crate) struct MockTransport {
    rx: tokio::sync::mpsc::UnboundedReceiver<JsonRpcMessage>,
    tx: tokio::sync::mpsc::UnboundedSender<JsonRpcMessage>,
}

/// Test-side handle for a [`MockTransport`].
///
/// Use `send()` to feed messages to the client and `recv()` to read
/// messages the client wrote.
#[cfg(test)]
pub(crate) struct MockTransportHandle {
    tx: tokio::sync::mpsc::UnboundedSender<JsonRpcMessage>,
    rx: tokio::sync::mpsc::UnboundedReceiver<JsonRpcMessage>,
}

#[cfg(test)]
impl MockTransport {
    /// Create a linked (transport, handle) pair.
    pub fn pair() -> (Self, MockTransportHandle) {
        let (client_tx, handle_rx) = tokio::sync::mpsc::unbounded_channel();
        let (handle_tx, client_rx) = tokio::sync::mpsc::unbounded_channel();

        let transport = MockTransport {
            rx: client_rx,
            tx: client_tx,
        };
        let handle = MockTransportHandle {
            tx: handle_tx,
            rx: handle_rx,
        };
        (transport, handle)
    }
}

#[cfg(test)]
#[async_trait]
impl Transport for MockTransport {
    async fn read_message(&mut self) -> StratumResult<Option<JsonRpcMessage>> {
        match self.rx.recv().await {
            Some(msg) => Ok(Some(msg)),
            None => Ok(None),
        }
    }

    async fn write_message(&mut self, msg: &JsonRpcMessage) -> StratumResult<()> {
        self.tx
            .send(msg.clone())
            .map_err(|_| StratumError::Disconnected)
    }
}

#[cfg(test)]
impl MockTransportHandle {
    /// Send a message to the client.
    pub fn send(&self, msg: JsonRpcMessage) {
        self.tx.send(msg).expect("transport dropped");
    }

    /// Receive a message the client wrote.
    pub async fn recv(&mut self) -> JsonRpcMessage {
        self.rx.recv().await.expect("transport dropped")
    }
}

/// Connector that pulls pre-built transports from a channel.
///
/// Each call to `connect()` receives the next `MockTransport` from the
/// channel, letting tests supply exactly the transports they need.
#[cfg(test)]
pub(crate) struct MockConnector {
    rx: tokio::sync::mpsc::Receiver<MockTransport>,
}

#[cfg(test)]
impl MockConnector {
    pub fn new(rx: tokio::sync::mpsc::Receiver<MockTransport>) -> Self {
        Self { rx }
    }
}

#[cfg(test)]
#[async_trait]
impl Connector for MockConnector {
    async fn connect(&mut self) -> StratumResult<Box<dyn Transport>> {
        match self.rx.recv().await {
            Some(transport) => Ok(Box::new(transport)),
            None => Err(StratumError::ConnectionFailed(
                "no more mock transports".into(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn test_message_roundtrip() {
        // Create a local test server
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Spawn server task
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut conn = Connection::new(socket);

            // Echo messages back
            while let Ok(Some(msg)) = conn.read_message().await {
                conn.write_message(&msg).await.unwrap();
            }
        });

        // Connect client
        let stream = TcpStream::connect(addr).await.unwrap();
        let mut conn = Connection::new(stream);

        // Send a message
        let request = JsonRpcMessage::request(1, "test.method", json!(["param1", "param2"]));
        conn.write_message(&request).await.unwrap();

        // Read it back
        let response = conn.read_message().await.unwrap().unwrap();
        assert_eq!(response.id(), Some(1));
        assert_eq!(response.method(), Some("test.method"));
    }
}
