//! Demultiplexes the framed RX stream by response kind.
//!
//! One serial wire carries unsolicited nonce reports interleaved
//! with replies to register conversations. [`Reader`] spawns a task
//! that owns the framed RX stream and routes each response onto its
//! own bounded channel. Each channel then carries a single kind of
//! traffic, so a consumer handles only the kind it cares about, and
//! neither kind can starve the other.

use futures::{Stream, StreamExt};
use std::io;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::response::{NonceResponse, RegisterResponse, Response};
use crate::tracing::prelude::*;

/// Owner of the spawned demux task.
pub struct Reader {
    task: JoinHandle<()>,
}

impl Reader {
    /// Spawns a task routing responses from the stream onto the
    /// returned channels.
    ///
    /// The task alone holds the channel senders: when the stream
    /// ends or fails, or when a receiver is dropped, the task exits
    /// and both channels close. Channel closure is the only
    /// lifecycle signal.
    pub fn spawn<R>(stream: R) -> (Self, ReaderChannels)
    where
        R: Stream<Item = Result<Response, io::Error>> + Unpin + Send + 'static,
    {
        let (nonce_tx, nonces) = mpsc::channel(CHANNEL_CAPACITY);
        let (register_tx, register_responses) = mpsc::channel(CHANNEL_CAPACITY);
        let task = tokio::spawn(run(stream, nonce_tx, register_tx));

        (
            Self { task },
            ReaderChannels {
                nonces,
                register_responses,
            },
        )
    }
}

/// Aborts the task so a discarded reader releases its stream.
impl Drop for Reader {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Receiving ends of the demuxed response channels.
pub struct ReaderChannels {
    pub nonces: mpsc::Receiver<NonceResponse>,
    pub register_responses: mpsc::Receiver<RegisterResponse>,
}

// Sized so a broadcast read's reply burst from the largest possible
// chain (128 chips) fits the register channel, and so the nonce
// channel absorbs a long conversation's worth of nonces at
// ticket-mask rates.
const CHANNEL_CAPACITY: usize = 256;

async fn run<R>(
    mut stream: R,
    nonce_tx: mpsc::Sender<NonceResponse>,
    register_tx: mpsc::Sender<RegisterResponse>,
) where
    R: Stream<Item = Result<Response, io::Error>> + Unpin,
{
    while let Some(item) = stream.next().await {
        let delivered = match item {
            Ok(Response::Nonce(nonce)) => nonce_tx.send(nonce).await.is_ok(),
            Ok(Response::ReadRegister(response)) => register_tx.send(response).await.is_ok(),
            Err(error) => {
                warn!(%error, "Chip response stream failed");
                break;
            }
        };

        // A failed send means the receiver is gone
        if !delivered {
            break;
        }
    }

    debug!("Chip response reader exiting");
}

#[cfg(test)]
mod tests {
    use futures::stream;

    use super::*;
    use crate::asic::bm13xx::register::{ChipId, ChipModel, Register};
    use crate::job_source::GeneralPurposeBits;

    fn nonce(job_id: u8) -> Result<Response, io::Error> {
        Ok(Response::Nonce(NonceResponse {
            nonce: 0x12345678,
            job_id,
            midstate_num: 0,
            version: GeneralPurposeBits::new([0, 0]),
            subcore_id: 0,
        }))
    }

    fn register_read(chip_address: u8) -> Result<Response, io::Error> {
        Ok(Response::ReadRegister(RegisterResponse {
            chip_address,
            register: Register::ChipId(ChipId {
                model: ChipModel::BM1370,
                core_count: 0,
                address: chip_address,
            }),
        }))
    }

    #[tokio::test]
    async fn routes_responses_by_kind() {
        let frames = vec![nonce(1), register_read(2), nonce(3), register_read(4)];
        let (_reader, mut channels) = Reader::spawn(stream::iter(frames));

        assert_eq!(channels.nonces.recv().await.unwrap().job_id, 1);
        assert_eq!(channels.nonces.recv().await.unwrap().job_id, 3);

        let first = channels.register_responses.recv().await.unwrap();
        assert_eq!(first.chip_address, 2);
        let second = channels.register_responses.recv().await.unwrap();
        assert_eq!(second.chip_address, 4);
    }

    #[tokio::test]
    async fn channels_close_when_stream_ends() {
        let (_reader, mut channels) = Reader::spawn(stream::iter(vec![nonce(1)]));

        assert!(channels.nonces.recv().await.is_some());
        assert!(channels.nonces.recv().await.is_none());
        assert!(channels.register_responses.recv().await.is_none());
    }

    #[tokio::test]
    async fn channels_close_on_stream_error() {
        let frames = vec![
            nonce(1),
            Err(io::Error::other("device gone")),
            // Never delivered; the reader exits on the error
            nonce(9),
        ];
        let (_reader, mut channels) = Reader::spawn(stream::iter(frames));

        assert_eq!(channels.nonces.recv().await.unwrap().job_id, 1);
        assert!(channels.nonces.recv().await.is_none());
        assert!(channels.register_responses.recv().await.is_none());
    }
}
