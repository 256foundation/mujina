//! Control channel for bitaxe-raw protocol.
//!
//! This module provides a control channel abstraction that handles
//! packet ID management and request/response correlation.

use futures::SinkExt;
use std::io;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time;
use tokio_serial::SerialStream;
use tokio_stream::StreamExt;
use tokio_util::codec::{FramedRead, FramedWrite};

use super::{ControlCodec, Packet, Response};
use tracing::trace;

/// Control channel for bitaxe-raw protocol communication.
///
/// This channel handles packet ID allocation and request/response matching.
/// It can be cloned to allow multiple components to share the same channel.
#[derive(Clone)]
pub struct ControlChannel {
    inner: Arc<Mutex<ControlChannelInner>>,
}

struct ControlChannelInner {
    writer: FramedWrite<tokio::io::WriteHalf<SerialStream>, ControlCodec>,
    reader: FramedRead<tokio::io::ReadHalf<SerialStream>, ControlCodec>,
    next_id: u8,
}

impl ControlChannel {
    /// Create a new control channel from a serial stream.
    pub fn new(stream: SerialStream) -> Self {
        let (reader, writer) = tokio::io::split(stream);
        Self {
            inner: Arc::new(Mutex::new(ControlChannelInner {
                writer: FramedWrite::new(writer, ControlCodec::default()),
                reader: FramedRead::new(reader, ControlCodec::default()),
                next_id: 0,
            })),
        }
    }

    /// Send a raw packet and wait for response.
    pub async fn send_packet(&self, mut packet: Packet) -> io::Result<Response> {
        let mut inner = self.inner.lock().await;

        // Assign packet ID
        packet.id = inner.next_id;
        inner.next_id = inner.next_id.wrapping_add(1);
        let expected_id = packet.id;

        trace!(
            "Sending control packet: id={}, page={:?}, command={:#02x}, data_len={}",
            packet.id,
            packet.page,
            packet.command,
            packet.data.len()
        );
        trace!("Control packet data: {:02x?}", packet.data);

        // Send the packet
        inner.writer.send(packet).await?;

        // Wait for response with matching ID
        let timeout = Duration::from_secs(1);
        let response = time::timeout(timeout, async {
            match inner.reader.next().await {
                Some(Ok(resp)) => {
                    if resp.id != expected_id {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "Response ID mismatch: expected {}, got {}",
                                expected_id, resp.id
                            ),
                        ));
                    }
                    Ok(resp)
                }
                Some(Err(e)) => Err(e),
                None => Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Control stream closed",
                )),
            }
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Control command timeout"))??;

        // Check for protocol errors
        if let Some(error) = response.error() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("Control protocol error: {:?}", error),
            ));
        }

        Ok(response)
    }
}
