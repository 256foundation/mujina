//! Register conversations with chips on the serial chain.
//!
//! The BM13xx protocol has no request ids, so [`Driver`] correlates
//! reads by content: a reply must match the requested chip address
//! and register address. The driver owns the command sink, the
//! register response channel, and the timing policy. Verbs take the
//! driver by mutable reference, so conversations cannot interleave
//! on the wire.

use futures::SinkExt;
use std::marker::PhantomData;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::time::timeout;

use super::command::{ChipCommandSink, Destination, ReadRegister, RegisterCommand, WriteRegister};
use super::register::{Register, RegisterAddress};
use super::response::RegisterResponse;
use crate::tracing::prelude::*;

pub struct Driver<W, E> {
    commands: W,
    responses: mpsc::Receiver<RegisterResponse>,
    _error: PhantomData<fn() -> E>,
}

impl<W, E> Driver<W, E>
where
    W: ChipCommandSink<E> + Unpin,
    E: std::error::Error + Send + Sync + 'static,
{
    pub fn new(commands: W, responses: mpsc::Receiver<RegisterResponse>) -> Self {
        Self {
            commands,
            responses,
            _error: PhantomData,
        }
    }

    /// Writes a register value; no reply is expected.
    pub async fn write(
        &mut self,
        destination: Destination,
        register: Register,
    ) -> Result<(), DriverError<E>> {
        self.commands
            .send(RegisterCommand::WriteRegister(WriteRegister {
                destination,
                register,
            }))
            .await
            .map_err(DriverError::Send)
    }

    /// Reads a register from one chip.
    ///
    /// Drains stale responses, sends the read command, and accepts
    /// only a reply matching both chip address and register address,
    /// discarding mismatches. Each attempt runs under a deadline;
    /// exhausting the attempts returns [`DriverError::NoResponse`].
    pub async fn read(
        &mut self,
        chip_address: u8,
        register_address: RegisterAddress,
    ) -> Result<Register, DriverError<E>> {
        // A reference driver waits 50 ms per read on a direct UART;
        // doubled to allow for USB CDC bridge latency
        const ATTEMPT_TIMEOUT: Duration = Duration::from_millis(100);
        const ATTEMPTS: u32 = 3;

        self.drain_stale();

        for attempt in 1..=ATTEMPTS {
            self.commands
                .send(RegisterCommand::ReadRegister(ReadRegister {
                    destination: Destination::Chip(chip_address),
                    register_address,
                }))
                .await
                .map_err(DriverError::Send)?;

            match timeout(
                ATTEMPT_TIMEOUT,
                self.matching_response(chip_address, register_address),
            )
            .await
            {
                Ok(result) => return result,
                Err(_elapsed) => {
                    debug!(chip_address, register = ?register_address, attempt, "Register read attempt timed out");
                }
            }
        }

        Err(DriverError::NoResponse {
            chip_address,
            register_address,
            attempts: ATTEMPTS,
        })
    }

    /// Reads a register from every chip at once.
    ///
    /// Drains stale responses, sends a broadcast read, and collects
    /// replies for the requested register until a quiet window
    /// passes with none new. May return an empty collection; the
    /// caller judges completeness.
    pub async fn broadcast_read(
        &mut self,
        register_address: RegisterAddress,
    ) -> Result<Vec<RegisterResponse>, DriverError<E>> {
        // Collection ends after this long with no new reply. At
        // 115200 baud a reply frame lasts about a millisecond, so
        // this window dominates any plausible gap between chips
        // relaying their replies down the chain.
        const QUIET_WINDOW: Duration = Duration::from_millis(500);

        self.drain_stale();

        self.commands
            .send(RegisterCommand::ReadRegister(ReadRegister {
                destination: Destination::Broadcast,
                register_address,
            }))
            .await
            .map_err(DriverError::Send)?;

        let mut replies = Vec::new();
        loop {
            match timeout(QUIET_WINDOW, self.responses.recv()).await {
                Ok(Some(response)) if response.register.address() == register_address => {
                    replies.push(response);
                }
                Ok(Some(response)) => {
                    warn!(?response, expected = ?register_address, "Discarding mismatched register response");
                }
                Ok(None) => return Err(DriverError::ResponsesClosed),
                Err(_elapsed) => return Ok(replies),
            }
        }
    }

    /// Writes a register value and verifies it by reading it back.
    pub async fn write_readback(
        &mut self,
        chip_address: u8,
        register: Register,
    ) -> Result<(), DriverError<E>> {
        let address = register.address();
        self.write(Destination::Chip(chip_address), register.clone())
            .await?;

        let actual = self.read(chip_address, address).await?;
        if actual != register {
            return Err(DriverError::ReadbackMismatch {
                expected: register,
                actual,
            });
        }

        Ok(())
    }

    /// Awaits a response matching the chip and register addresses,
    /// discarding others.
    async fn matching_response(
        &mut self,
        chip_address: u8,
        register_address: RegisterAddress,
    ) -> Result<Register, DriverError<E>> {
        loop {
            let Some(response) = self.responses.recv().await else {
                return Err(DriverError::ResponsesClosed);
            };

            if response.chip_address == chip_address
                && response.register.address() == register_address
            {
                return Ok(response.register);
            }

            warn!(?response, expected_chip = chip_address, expected = ?register_address, "Discarding mismatched register response");
        }
    }

    fn drain_stale(&mut self) {
        while let Ok(stale) = self.responses.try_recv() {
            warn!(?stale, "Discarding stale register response");
        }
    }
}

#[derive(Debug, Error)]
pub enum DriverError<E> {
    #[error("command send failed: {0}")]
    Send(E),

    #[error(
        "no response from chip 0x{chip_address:02x} for {register_address:?} \
         after {attempts} attempts"
    )]
    NoResponse {
        chip_address: u8,
        register_address: RegisterAddress,
        attempts: u32,
    },

    #[error("register response channel closed")]
    ResponsesClosed,

    #[error("readback mismatch: wrote {expected:?}, read {actual:?}")]
    ReadbackMismatch {
        expected: Register,
        actual: Register,
    },
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};

    use futures::Sink;

    use super::super::command::JobCommand;
    use super::super::register::{ChipId, ChipModel, Log2Difficulty, TicketMask};
    use super::*;
    use crate::types::Difficulty;

    /// Sink recording register commands and signalling each send.
    struct RecordingSink {
        sent: Arc<Mutex<Vec<RegisterCommand>>>,
        events: mpsc::UnboundedSender<()>,
    }

    impl Sink<RegisterCommand> for RecordingSink {
        type Error = io::Error;

        fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, item: RegisterCommand) -> Result<(), io::Error> {
            self.sent.lock().unwrap().push(item);
            self.events.send(()).ok();
            Ok(())
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    /// The driver never sends jobs; required by the sink bound only.
    impl Sink<JobCommand> for RecordingSink {
        type Error = io::Error;

        fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, _: JobCommand) -> Result<(), io::Error> {
            Ok(())
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    struct Harness {
        driver: Driver<RecordingSink, io::Error>,
        responses: mpsc::Sender<RegisterResponse>,
        sent: Arc<Mutex<Vec<RegisterCommand>>>,
        sends: mpsc::UnboundedReceiver<()>,
    }

    fn harness() -> Harness {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let (events, sends) = mpsc::unbounded_channel();
        let sink = RecordingSink {
            sent: sent.clone(),
            events,
        };
        let (responses, responses_rx) = mpsc::channel(16);

        Harness {
            driver: Driver::new(sink, responses_rx),
            responses,
            sent,
            sends,
        }
    }

    fn chip_id_response(chip_address: u8) -> RegisterResponse {
        RegisterResponse {
            chip_address,
            register: Register::ChipId(ChipId {
                model: ChipModel::BM1370,
                core_count: 0,
                address: chip_address,
            }),
        }
    }

    fn ticket_mask(difficulty: u64) -> Register {
        Register::TicketMask(TicketMask::new(Log2Difficulty::from_difficulty(
            Difficulty::from(difficulty),
        )))
    }

    #[tokio::test(start_paused = true)]
    async fn read_returns_matching_response() {
        let mut h = harness();

        let (result, _) = tokio::join!(h.driver.read(0x08, RegisterAddress::ChipId), async {
            h.sends.recv().await;
            h.responses.send(chip_id_response(0x08)).await.unwrap();
        });

        let register = result.unwrap();
        assert!(matches!(register, Register::ChipId(_)));
    }

    #[tokio::test(start_paused = true)]
    async fn read_discards_mismatched_responses() {
        let mut h = harness();

        let (result, _) = tokio::join!(h.driver.read(0x08, RegisterAddress::ChipId), async {
            h.sends.recv().await;
            // Wrong chip, then right chip but wrong register, then
            // the matching reply
            h.responses.send(chip_id_response(0x02)).await.unwrap();
            h.responses
                .send(RegisterResponse {
                    chip_address: 0x08,
                    register: ticket_mask(256),
                })
                .await
                .unwrap();
            h.responses.send(chip_id_response(0x08)).await.unwrap();
        });

        let register = result.unwrap();
        let Register::ChipId(chip_id) = register else {
            panic!("Expected ChipId register");
        };
        assert_eq!(chip_id.address, 0x08);
    }

    #[tokio::test(start_paused = true)]
    async fn read_drains_stale_responses() {
        let mut h = harness();

        // A would-be match delivered before the read must not
        // satisfy it
        h.responses.send(chip_id_response(0x08)).await.unwrap();

        let result = h.driver.read(0x08, RegisterAddress::ChipId).await;

        assert!(matches!(result, Err(DriverError::NoResponse { .. })));
    }

    #[tokio::test(start_paused = true)]
    async fn read_retries_after_deadline() {
        let mut h = harness();

        let (result, _) = tokio::join!(h.driver.read(0x08, RegisterAddress::ChipId), async {
            // Let the first attempt starve; answer the second
            h.sends.recv().await;
            h.sends.recv().await;
            h.responses.send(chip_id_response(0x08)).await.unwrap();
        });

        assert!(result.is_ok());
        assert_eq!(h.sent.lock().unwrap().len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn read_exhausts_attempts() {
        let mut h = harness();

        let result = h.driver.read(0x08, RegisterAddress::ChipId).await;

        let Err(DriverError::NoResponse { attempts, .. }) = result else {
            panic!("Expected NoResponse error");
        };
        // The error reports as many attempts as commands were sent,
        // and the policy retried at least once
        assert_eq!(h.sent.lock().unwrap().len() as u32, attempts);
        assert!(attempts > 1);
    }

    #[tokio::test(start_paused = true)]
    async fn read_errors_when_responses_close() {
        let mut h = harness();
        drop(h.responses);

        let result = h.driver.read(0x08, RegisterAddress::ChipId).await;

        assert!(matches!(result, Err(DriverError::ResponsesClosed)));
        // The closed channel short-circuits the remaining attempts
        assert_eq!(h.sent.lock().unwrap().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn write_sends_one_command() {
        let mut h = harness();

        h.driver
            .write(Destination::Broadcast, ticket_mask(256))
            .await
            .unwrap();

        let sent = h.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        let RegisterCommand::WriteRegister(write) = &sent[0] else {
            panic!("Expected WriteRegister command");
        };
        assert_eq!(write.destination, Destination::Broadcast);
        assert_eq!(write.register, ticket_mask(256));
    }

    #[tokio::test(start_paused = true)]
    async fn broadcast_read_collects_until_quiet() {
        let mut h = harness();

        let (result, _) = tokio::join!(h.driver.broadcast_read(RegisterAddress::ChipId), async {
            h.sends.recv().await;
            for address in [0x00, 0x02, 0x04] {
                h.responses.send(chip_id_response(address)).await.unwrap();
            }
            // A reply for a different register is not collected
            h.responses
                .send(RegisterResponse {
                    chip_address: 0x06,
                    register: ticket_mask(256),
                })
                .await
                .unwrap();
        });

        let replies = result.unwrap();
        let addresses: Vec<u8> = replies.iter().map(|r| r.chip_address).collect();
        assert_eq!(addresses, [0x00, 0x02, 0x04]);

        let sent = h.sent.lock().unwrap();
        let RegisterCommand::ReadRegister(read) = &sent[0] else {
            panic!("Expected ReadRegister command");
        };
        assert_eq!(read.destination, Destination::Broadcast);
    }

    #[tokio::test(start_paused = true)]
    async fn broadcast_read_returns_empty_when_silent() {
        let mut h = harness();

        let result = h.driver.broadcast_read(RegisterAddress::ChipId).await;

        assert!(result.unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn write_readback_accepts_matching_value() {
        let mut h = harness();

        let (result, _) = tokio::join!(h.driver.write_readback(0x08, ticket_mask(256)), async {
            // One send for the write, one for the readback read
            h.sends.recv().await;
            h.sends.recv().await;
            h.responses
                .send(RegisterResponse {
                    chip_address: 0x08,
                    register: ticket_mask(256),
                })
                .await
                .unwrap();
        });

        assert!(result.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn write_readback_rejects_changed_value() {
        let mut h = harness();

        let (result, _) = tokio::join!(h.driver.write_readback(0x08, ticket_mask(256)), async {
            h.sends.recv().await;
            h.sends.recv().await;
            h.responses
                .send(RegisterResponse {
                    chip_address: 0x08,
                    register: ticket_mask(1024),
                })
                .await
                .unwrap();
        });

        assert!(matches!(result, Err(DriverError::ReadbackMismatch { .. })));
    }
}
