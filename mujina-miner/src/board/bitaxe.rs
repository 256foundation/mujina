use async_trait::async_trait;
use futures::sink::SinkExt;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::{
    io::{AsyncRead, AsyncWriteExt, ReadBuf},
    time,
};
use tokio_serial::SerialStream;
use tokio_stream::StreamExt;
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::asic::bm13xx::{self, protocol::Command, BM13xxProtocol};
use crate::asic::{ChipInfo, MiningJob};
use crate::board::{Board, BoardError, BoardEvent, BoardInfo, JobCompleteReason};
use crate::tracing::prelude::*;

/// A wrapper around AsyncRead that traces raw bytes as they're read
struct TracingReader<R> {
    inner: R,
    name: &'static str,
}

impl<R: AsyncRead + Unpin> TracingReader<R> {
    fn new(inner: R, name: &'static str) -> Self {
        Self { inner, name }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for TracingReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before_len = buf.filled().len();

        // Call the inner reader
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);

        // If we read some bytes, trace them
        if let Poll::Ready(Ok(())) = &result {
            let after_len = buf.filled().len();
            if after_len > before_len {
                let new_bytes = &buf.filled()[before_len..after_len];
                trace!(
                    "{} RX: {} bytes => {:02x?}",
                    self.name,
                    new_bytes.len(),
                    new_bytes
                );
            }
        }

        result
    }
}

/// Bitaxe Gamma hashboard abstraction.
///
/// The Bitaxe Gamma running bitaxe-raw firmware provides a control interface for managing the
/// hashboard, including GPIO reset control and board initialization sequences.
pub struct BitaxeBoard {
    /// Serial control channel for board management commands
    control: SerialStream,
    /// Writer for sending commands to chips
    data_writer: FramedWrite<tokio::io::WriteHalf<SerialStream>, bm13xx::FrameCodec>,
    /// Reader for receiving responses from chips (moved to event monitor during initialize)
    data_reader:
        Option<FramedRead<TracingReader<tokio::io::ReadHalf<SerialStream>>, bm13xx::FrameCodec>>,
    /// Protocol handler for chip communication
    protocol: BM13xxProtocol,
    /// Discovered chip information (passive record-keeping)
    chip_infos: Vec<ChipInfo>,
    /// Channel for sending board events
    event_tx: Option<tokio::sync::mpsc::Sender<BoardEvent>>,
    /// Channel for receiving board events (stored after initialization)
    event_rx: Option<tokio::sync::mpsc::Receiver<BoardEvent>>,
    /// Current job ID
    current_job_id: Option<u64>,
    /// Job ID counter for cycling through 0-127
    next_job_id: u8,
}

impl BitaxeBoard {
    /// Creates a new BitaxeBoard instance with the provided serial streams.
    ///
    /// # Arguments
    /// * `control` - Serial stream for sending board control commands
    /// * `data` - Serial stream for chip communication
    ///
    /// # Returns
    /// A new BitaxeBoard instance ready for hardware operations
    ///
    /// # Design Note
    /// In the future, a DeviceManager will create boards when USB devices
    /// are detected (by VID/PID) and pass already-opened serial streams.
    pub fn new(control: SerialStream, data: SerialStream) -> Self {
        // Split the data stream immediately
        let (reader, writer) = tokio::io::split(data);

        // Wrap the reader with tracing
        let tracing_reader = TracingReader::new(reader, "Data");

        BitaxeBoard {
            control,
            data_writer: FramedWrite::new(writer, bm13xx::FrameCodec::default()),
            data_reader: Some(FramedRead::new(
                tracing_reader,
                bm13xx::FrameCodec::default(),
            )),
            protocol: BM13xxProtocol::new(),
            chip_infos: Vec::new(),
            event_tx: None,
            event_rx: None,
            current_job_id: None,
            next_job_id: 0,
        }
    }

    /// Performs a momentary reset of the mining chips via GPIO control.
    ///
    /// This function toggles the reset line low for 100ms, then high for 100ms
    /// to properly reset all connected mining chips.
    ///
    /// # Hardware Protocol
    /// - RSTN_LO: Pulls reset line low (active reset)
    /// - RSTN_HI: Releases reset line high (normal operation)
    /// - 100ms delays ensure proper reset timing for BM13xx chips
    ///
    /// # Errors
    /// Returns an error if serial communication fails during reset sequence
    ///
    /// # TODO
    /// Replace raw byte commands with proper codec and high-level message types
    pub async fn momentary_reset(&mut self) -> Result<(), std::io::Error> {
        const RSTN_LO: &[u8] = &[0x07, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00];
        const RSTN_HI: &[u8] = &[0x07, 0x00, 0x00, 0x00, 0x06, 0x00, 0x01];
        const WAIT: Duration = Duration::from_millis(100);

        tracing::debug!("Control TX: RSTN_LO => {:02x?}", RSTN_LO);
        self.control.write_all(RSTN_LO).await?;
        self.control.flush().await?;
        time::sleep(WAIT).await;

        tracing::debug!("Control TX: RSTN_HI => {:02x?}", RSTN_HI);
        self.control.write_all(RSTN_HI).await?;
        self.control.flush().await?;
        time::sleep(WAIT).await;

        Ok(())
    }

    /// Hold the mining chips in reset state.
    ///
    /// This function pulls the reset line low and keeps it there,
    /// effectively disabling all connected mining chips. This is used
    /// during shutdown to ensure chips are in a safe, non-hashing state.
    ///
    /// # Hardware Protocol
    /// - RSTN_LO: Pulls reset line low (active reset)
    ///
    /// # Errors
    /// Returns an error if serial communication fails
    pub async fn hold_in_reset(&mut self) -> Result<(), std::io::Error> {
        const RSTN_LO: &[u8] = &[0x07, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00];

        tracing::debug!("Control TX: RSTN_LO (holding in reset) => {:02x?}", RSTN_LO);
        self.control.write_all(RSTN_LO).await?;
        self.control.flush().await?;

        Ok(())
    }

    /// Discover chips connected to this board.
    ///
    /// Sends broadcast ReadRegister commands and collects responses
    /// to identify all chips on the serial bus.
    /// Send a configuration command to the chips.
    ///
    /// This is used during initialization to configure PLL, version rolling, etc.
    pub async fn send_config_command(&mut self, command: Command) -> Result<(), BoardError> {
        self.data_writer
            .send(command)
            .await
            .map_err(|e| BoardError::Communication(e))
    }

    /// Send multiple configuration commands in sequence.
    pub async fn send_config_commands(&mut self, commands: Vec<Command>) -> Result<(), BoardError> {
        for command in commands {
            self.send_config_command(command).await?;
            // Small delay between commands to avoid overwhelming the chip
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok(())
    }

    async fn discover_chips(&mut self) -> Result<(), BoardError> {
        // Get a mutable reference to the reader
        let reader = self.data_reader.as_mut().ok_or_else(|| {
            BoardError::InitializationFailed("Data reader already taken".to_string())
        })?;

        // Send a broadcast read to discover chips
        let discover_cmd = BM13xxProtocol::discover_chips();

        self.data_writer
            .send(discover_cmd)
            .await
            .map_err(|e| BoardError::Communication(e))?;

        // Wait a bit for responses
        let timeout = Duration::from_millis(500);
        let deadline = tokio::time::Instant::now() + timeout;

        while tokio::time::Instant::now() < deadline {
            tokio::select! {
                response = reader.next() => {
                    match response {
                        Some(Ok(bm13xx::Response::ReadRegister {
                            chip_address: _,
                            register: bm13xx::Register::ChipId { chip_type, core_count, address }
                        })) => {
                            let chip_id = chip_type.id_bytes();
                            tracing::info!("Discovered chip {:?} ({:02x}{:02x}) at address {address}",
                                         chip_type, chip_id[0], chip_id[1]);

                            let chip_info = ChipInfo {
                                chip_id,
                                core_count: core_count.into(),
                                address,
                                supports_version_rolling: true, // BM1370 supports this
                            };

                            self.chip_infos.push(chip_info);
                        }
                        Some(Ok(_)) => {
                            tracing::warn!("Unexpected response during chip discovery");
                        }
                        Some(Err(e)) => {
                            tracing::error!("Error during chip discovery: {e}");
                        }
                        None => break,
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    break;
                }
            }
        }

        if self.chip_infos.is_empty() {
            Err(BoardError::InitializationFailed(
                "No chips discovered".to_string(),
            ))
        } else {
            Ok(())
        }
    }

    /// Spawn a job completion timer task
    fn spawn_job_timer(&self, job_id: Option<u64>) {
        if let (Some(job_id), Some(tx)) = (job_id, &self.event_tx) {
            let event_tx = tx.clone();

            tokio::spawn(async move {
                // BM1370 at ~500 GH/s takes about 8.6 seconds to search 2^32 nonces
                // Add some buffer time
                tokio::time::sleep(Duration::from_secs(10)).await;

                // Send job complete event
                let _ = event_tx
                    .send(BoardEvent::JobComplete {
                        job_id,
                        reason: JobCompleteReason::TimeoutEstimate,
                    })
                    .await;
            });
        }
    }

    /// Spawn a task to monitor chip responses and emit events
    fn spawn_event_monitor(&mut self) {
        let Some(event_tx) = self.event_tx.clone() else {
            tracing::error!("Cannot spawn event monitor without event channel");
            return;
        };

        // Take ownership of the data reader for the monitoring task
        let data_reader = self
            .data_reader
            .take()
            .expect("Data reader should be available during initialization");

        // Clone current job ID for the task
        let current_job_id = self.current_job_id;

        // Spawn the monitoring task
        tokio::spawn(async move {
            let mut reader = data_reader;

            loop {
                match reader.next().await {
                    Some(Ok(response)) => {
                        match response {
                            bm13xx::Response::Nonce {
                                nonce,
                                job_id,
                                midstate_num: _,
                                version,
                                subcore_id: _,
                            } => {
                                // Extract core ID from nonce (bits 25-31)
                                let core_id = ((nonce >> 25) & 0x7f) as u8;

                                // Send nonce found event
                                let nonce_result = crate::asic::NonceResult {
                                    job_id: job_id as u64,
                                    nonce,
                                    hash: [0; 32], // TODO: Calculate actual hash if needed
                                };

                                if event_tx
                                    .send(BoardEvent::NonceFound(nonce_result))
                                    .await
                                    .is_err()
                                {
                                    tracing::error!("Failed to send nonce event, receiver dropped");
                                    break;
                                }

                                tracing::debug!(
                                    "Nonce found: job_id={}, nonce=0x{:08x}, core={}, version=0x{:04x}",
                                    job_id, nonce, core_id, version
                                );
                            }
                            bm13xx::Response::ReadRegister {
                                chip_address,
                                register,
                            } => {
                                // Log register reads but don't emit events for them
                                tracing::trace!(
                                    "Register read from chip {}: {:?}",
                                    chip_address,
                                    register
                                );
                            }
                        }
                    }
                    Some(Err(e)) => {
                        tracing::error!("Error decoding response: {}", e);

                        // Send chip error event
                        if event_tx
                            .send(BoardEvent::ChipError {
                                chip_address: 0, // TODO: Get actual chip address
                                error: format!("Decode error: {}", e),
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    None => {
                        tracing::error!("Data stream closed unexpectedly");
                        break;
                    }
                }
            }

            tracing::info!("Event monitor task exiting");
        });

        // Spawn job completion timer outside the main monitoring task
        self.spawn_job_timer(current_job_id);
    }
}

#[async_trait]
impl Board for BitaxeBoard {
    async fn reset(&mut self) -> Result<(), BoardError> {
        // Use the existing momentary_reset method
        self.momentary_reset().await?;
        Ok(())
    }

    async fn hold_in_reset(&mut self) -> Result<(), BoardError> {
        // Call the inherent method, not the trait method
        BitaxeBoard::hold_in_reset(self).await?;
        Ok(())
    }

    async fn initialize(&mut self) -> Result<tokio::sync::mpsc::Receiver<BoardEvent>, BoardError> {
        // Reset the board first
        self.reset().await?;

        // Enable version rolling before chip discovery (as seen in serial captures)
        // Write 0xFFFF0090 to register 0xA4 to enable version rolling
        let version_cmd = Command::WriteRegister {
            all: true, // Broadcast to all chips
            chip_address: 0x00,
            register: bm13xx::protocol::Register::VersionMask(
                bm13xx::protocol::VersionMask::full_rolling(),
            ),
        };
        self.send_config_command(version_cmd).await?;

        // Small delay after version rolling configuration
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Discover connected chips
        self.discover_chips().await?;

        tracing::info!("Board initialized with {} chip(s)", self.chip_infos.len());

        // Create event channel
        let (tx, rx) = tokio::sync::mpsc::channel(100);
        self.event_tx = Some(tx);
        self.event_rx = Some(rx);

        // TODO: Spawn task to monitor chip responses and emit events
        self.spawn_event_monitor();

        // Return a dummy receiver for backward compatibility
        // The real receiver is stored and can be retrieved with take_event_receiver()
        let (dummy_tx, dummy_rx) = tokio::sync::mpsc::channel(1);
        drop(dummy_tx);
        Ok(dummy_rx)
    }

    fn chip_count(&self) -> usize {
        self.chip_infos.len()
    }

    fn chip_infos(&self) -> &[ChipInfo] {
        &self.chip_infos
    }

    async fn send_job(&mut self, job: &MiningJob) -> Result<(), BoardError> {
        // Encode the job for BM1370
        let command = self.protocol.encode_mining_job(job, self.next_job_id);

        tracing::debug!(
            "Sending job {} (internal ID {}) to chips - target: {:02x?}, ntime: {}, nbits: {:08x}",
            job.job_id,
            self.next_job_id,
            &job.target[..8],
            job.ntime,
            job.nbits
        );

        // Send the job to all chips (BM1370 uses broadcast for jobs)
        self.data_writer
            .send(command)
            .await
            .map_err(|e| BoardError::Communication(e))?;

        tracing::debug!("Job {} sent successfully", job.job_id);

        // Store current job ID mapping
        self.current_job_id = Some(job.job_id);

        // Increment job ID counter (cycles through 0-127 by steps of 24)
        self.next_job_id = (self.next_job_id + 24) % 128;

        // Spawn a job completion timer
        self.spawn_job_timer(Some(job.job_id));

        Ok(())
    }

    async fn cancel_job(&mut self, job_id: u64) -> Result<(), BoardError> {
        // TODO: Implement job cancellation
        // This might involve sending a new dummy job or reset command

        if let Some(tx) = &self.event_tx {
            let _ = tx
                .send(BoardEvent::JobComplete {
                    job_id,
                    reason: JobCompleteReason::Cancelled,
                })
                .await;
        }

        Ok(())
    }

    fn board_info(&self) -> BoardInfo {
        BoardInfo {
            model: "Bitaxe Gamma".to_string(),
            firmware_version: Some("bitaxe-raw".to_string()),
            serial_number: None, // Could be read from the board in future
        }
    }

    fn take_event_receiver(&mut self) -> Option<tokio::sync::mpsc::Receiver<BoardEvent>> {
        self.event_rx.take()
    }
}

// Factory function to create a Bitaxe board from USB device info
async fn create_from_usb(
    device: crate::transport::UsbDeviceInfo,
) -> crate::error::Result<Box<dyn Board + Send>> {
    use tokio_serial::SerialPortBuilderExt;

    // Bitaxe Gamma requires exactly 2 serial ports
    if device.serial_ports.len() != 2 {
        return Err(crate::error::Error::Hardware(format!(
            "Bitaxe Gamma requires exactly 2 serial ports, found {}",
            device.serial_ports.len()
        )));
    }

    tracing::info!(
        "Opening Bitaxe Gamma serial ports: control={}, data={}",
        device.serial_ports[0],
        device.serial_ports[1]
    );

    // Open control port at 115200 baud
    let control_port = tokio_serial::new(&device.serial_ports[0], 115200).open_native_async()?;

    // Open data port at 115200 baud
    let data_port = tokio_serial::new(&device.serial_ports[1], 115200).open_native_async()?;

    // Create the board
    let mut board = BitaxeBoard::new(control_port, data_port);

    // Initialize the board (reset, discover chips, start event monitoring)
    let _dummy_rx = board
        .initialize()
        .await
        .map_err(|e| crate::error::Error::Hardware(format!("Failed to initialize board: {}", e)))?;

    // The real event receiver is stored in the board and can be retrieved
    // by the scheduler using take_event_receiver()

    tracing::info!(
        "Bitaxe board initialized successfully with {} chips",
        board.chip_count()
    );

    Ok(Box::new(board))
}

// Register this board type with the inventory system
inventory::submit! {
    crate::board::BoardDescriptor {
        vid: 0x0403,
        pid: 0x6015,
        name: "Bitaxe Gamma",
        create_fn: |device| Box::pin(create_from_usb(device)),
    }
}
