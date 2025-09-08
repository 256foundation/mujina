use async_trait::async_trait;
use futures::sink::SinkExt;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::{
    io::{AsyncRead, ReadBuf},
    time,
};
use tokio_stream::StreamExt;
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::asic::bm13xx::{self, protocol::Command, BM13xxProtocol};
use crate::asic::{ChipInfo, MiningJob};
use crate::board::{Board, BoardError, BoardEvent, BoardInfo, JobCompleteReason};
use crate::hw_trait::gpio::{Gpio, GpioPin, PinValue};
use crate::hw_trait::i2c::I2c;
use crate::mgmt_protocol::{ControlChannel, BitaxeRawGpio};
use crate::mgmt_protocol::bitaxe_raw::i2c::BitaxeRawI2c;
use crate::peripheral::emc2101::Emc2101;
use crate::peripheral::tps546::{Tps546, Tps546Config};
use crate::tracing::prelude::*;
use crate::transport::serial::{SerialStream, SerialControl, SerialReader, SerialWriter};

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
    /// Control channel for board management
    #[expect(dead_code, reason = "Will be used for direct control protocol access")]
    control_channel: ControlChannel,
    /// GPIO controller
    gpio: BitaxeRawGpio,
    /// I2C bus controller
    i2c: BitaxeRawI2c,
    /// Fan controller (EMC2101)
    fan_controller: Option<Emc2101<BitaxeRawI2c>>,
    /// Power management controller (TPS546D24A)
    power_controller: Option<Tps546<BitaxeRawI2c>>,
    /// Writer for sending commands to chips
    data_writer: FramedWrite<SerialWriter, bm13xx::FrameCodec>,
    /// Reader for receiving responses from chips (moved to event monitor during initialize)
    data_reader: Option<FramedRead<TracingReader<SerialReader>, bm13xx::FrameCodec>>,
    /// Control handle for data channel (for baud rate changes)
    data_control: SerialControl,
    /// Protocol handler for chip communication
    #[expect(dead_code, reason = "Will be used for protocol-specific operations")]
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
    /// Handle for the statistics task
    stats_task_handle: Option<tokio::task::JoinHandle<()>>,
}

impl BitaxeBoard {
    /// GPIO pin number for ASIC reset control (active low)
    const ASIC_RESET_PIN: u8 = 0;

    /// Bitaxe Gamma board configuration
    /// The Gamma uses a BM1370 chip and runs at 1Mbps after initialization
    const TARGET_BAUD_RATE: u32 = 1_000_000;
    const CHIP_BAUD_REGISTER: bm13xx::protocol::BaudRate = bm13xx::protocol::BaudRate::Baud1M;
    const EXPECTED_CHIP_ID: [u8; 2] = [0x13, 0x70]; // BM1370

    /// Creates a new BitaxeBoard instance with the provided serial streams.
    ///
    /// # Arguments
    /// * `control` - Serial stream for sending board control commands
    /// * `data_path` - Path to the data serial port (e.g., "/dev/ttyACM1")
    ///
    /// # Returns
    /// A new BitaxeBoard instance ready for hardware operations
    ///
    /// # Design Note
    /// In the future, a DeviceManager will create boards when USB devices
    /// are detected (by VID/PID) and pass already-opened serial streams.
    pub fn new(control: tokio_serial::SerialStream, data_path: &str) -> Result<Self, BoardError> {
        // Create control channel, GPIO and I2C controllers
        let control_channel = ControlChannel::new(control);
        let gpio = BitaxeRawGpio::new(control_channel.clone());
        let i2c = BitaxeRawI2c::new(control_channel.clone());

        // Create SerialStream for data channel at initial baud rate
        let data_stream = SerialStream::new(data_path, 115200)
            .map_err(|e| BoardError::InitializationFailed(format!("Failed to open data port: {}", e)))?;
        let (data_reader, data_writer, data_control) = data_stream.split();

        // Wrap the data reader with tracing
        let tracing_reader = TracingReader::new(data_reader, "Data");

        Ok(BitaxeBoard {
            control_channel,
            gpio,
            i2c,
            fan_controller: None,
            power_controller: None,
            data_writer: FramedWrite::new(data_writer, bm13xx::FrameCodec::default()),
            data_reader: Some(FramedRead::new(
                tracing_reader,
                bm13xx::FrameCodec::default(),
            )),
            data_control,
            protocol: BM13xxProtocol::new(),
            chip_infos: Vec::new(),
            event_tx: None,
            event_rx: None,
            current_job_id: None,
            next_job_id: 0,
            stats_task_handle: None,
        })
    }

    /// Performs a momentary reset of the mining chips via GPIO control.
    ///
    /// This function toggles the reset line low for 100ms, then high for 100ms
    /// to properly reset all connected mining chips.
    pub async fn momentary_reset(&mut self) -> Result<(), BoardError> {
        const WAIT: Duration = Duration::from_millis(100);

        // Assert reset
        self.hold_in_reset().await?;
        time::sleep(WAIT).await;

        // De-assert reset
        self.release_reset().await?;
        time::sleep(WAIT).await;

        Ok(())
    }

    /// Release the mining chips from reset state.
    async fn release_reset(&mut self) -> Result<(), BoardError> {
        // Get the ASIC reset pin
        let mut reset_pin = self.gpio.pin(Self::ASIC_RESET_PIN).await
            .map_err(|e| BoardError::HardwareControl(format!("Failed to get reset pin: {}", e)))?;

        // Set reset high (inactive)
        tracing::debug!("De-asserting ASIC reset (GPIO {} = high)", Self::ASIC_RESET_PIN);
        reset_pin.write(PinValue::High).await
            .map_err(|e| BoardError::HardwareControl(format!("Failed to de-assert reset: {}", e)))?;

        Ok(())
    }

    /// Hold the mining chips in reset state.
    ///
    /// This function sets the reset line low and keeps it there,
    /// effectively disabling all connected mining chips. This is used
    /// during shutdown to ensure chips are in a safe, non-hashing state.
    pub async fn hold_in_reset(&mut self) -> Result<(), BoardError> {
        // Get the ASIC reset pin
        let mut reset_pin = self.gpio.pin(Self::ASIC_RESET_PIN).await
            .map_err(|e| BoardError::HardwareControl(format!("Failed to get reset pin: {}", e)))?;

        // Hold reset low (active)
        tracing::debug!("Holding ASIC in reset (GPIO {} = low)", Self::ASIC_RESET_PIN);
        reset_pin.write(PinValue::Low).await
            .map_err(|e| BoardError::HardwareControl(format!("Failed to hold reset: {}", e)))?;

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
    #[expect(dead_code, reason = "Will be used for batch configuration")]
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

    /// Initialize the power controller
    async fn init_power_controller(&mut self) -> Result<(), BoardError> {
        // Set I2C frequency to 100kHz for PMBus devices
        self.i2c.set_frequency(100_000).await
            .map_err(|e| BoardError::InitializationFailed(
                format!("Failed to set I2C frequency: {}", e)
            ))?;

        // Clone the I2C bus for the power controller
        let power_i2c = self.i2c.clone();
        let config = Tps546Config::bitaxe_gamma();
        let mut tps546 = Tps546::new(power_i2c, config);

        // Initialize the TPS546
        match tps546.init().await {
            Ok(()) => {
                info!("TPS546D24A power controller initialized");

                // Delay before setting voltage
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

                // Set initial output voltage (1.15V - BM1370 default from esp-miner)
                match tps546.set_vout(1.15).await {
                    Ok(()) => {
                        info!("Core voltage set to 1.15V");

                        // Wait for voltage to stabilize
                        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

                        // Verify voltage
                        match tps546.get_vout().await {
                            Ok(mv) => info!("Core voltage readback: {:.3}V", mv as f32 / 1000.0),
                            Err(e) => warn!("Failed to read core voltage: {}", e),
                        }
                    }
                    Err(e) => {
                        error!("Failed to set initial core voltage: {}", e);
                        return Err(BoardError::InitializationFailed(
                            format!("Failed to set core voltage: {}", e)
                        ));
                    }
                }

                self.power_controller = Some(tps546);
                Ok(())
            }
            Err(e) => {
                error!("Failed to initialize TPS546D24A power controller: {}", e);
                Err(BoardError::InitializationFailed(
                    format!("Power controller init failed: {}", e)
                ))
            }
        }
    }

    /// Initialize the fan controller
    async fn init_fan_controller(&mut self) -> Result<(), BoardError> {
        // Clone the I2C bus for the fan controller
        let fan_i2c = self.i2c.clone();
        let mut fan = Emc2101::new(fan_i2c);

        // Initialize the EMC2101
        match fan.init().await {
            Ok(()) => {
                info!("EMC2101 fan controller initialized");

                // Set initial fan speed to 50%
                const INITIAL_FAN_PERCENT: u8 = 50;
                if let Err(e) = fan.set_pwm_percent(INITIAL_FAN_PERCENT).await {
                    warn!("Failed to set initial fan speed: {}", e);
                }

                self.fan_controller = Some(fan);
                Ok(())
            }
            Err(e) => {
                warn!("Failed to initialize EMC2101 fan controller: {}", e);
                // Continue without fan control - not critical for operation
                Ok(())
            }
        }
    }

    /// Spawn a task to periodically log management statistics
    fn spawn_stats_monitor(&mut self) {
        // Clone what we need for the task
        let i2c = self.i2c.clone();
        let chip_count = self.chip_infos.len();

        let handle = tokio::spawn(async move {
            const STATS_INTERVAL: Duration = Duration::from_secs(30);
            let mut interval = tokio::time::interval(STATS_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            // Create new controllers for the stats task
            let mut fan = Emc2101::new(i2c.clone());
            let config = Tps546Config::bitaxe_gamma();
            let mut power = Tps546::new(i2c, config);

            loop {
                interval.tick().await;

                // Read temperature
                let temp = match fan.get_external_temperature().await {
                    Ok(t) => format!("{:.1}°C", t),
                    Err(_) => "N/A".to_string(),
                };

                // Read fan PWM duty
                let fan_pwm = match fan.get_pwm_percent().await {
                    Ok(p) => format!("{}%", p),
                    Err(_) => "N/A".to_string(),
                };

                // Read fan RPM (if TACH is connected)
                let fan_rpm = match fan.get_tach_count().await {
                    Ok(count) => {
                        trace!("TACH count: 0x{:04x}", count);
                        match fan.get_rpm().await {
                            Ok(rpm) if rpm > 0 => format!("{} RPM", rpm),
                            Ok(_) => format!("0 RPM (TACH: 0x{:04x})", count),
                            Err(_) => "N/A".to_string(),
                        }
                    }
                    Err(e) => {
                        trace!("Failed to read TACH: {}", e);
                        "N/A".to_string()
                    }
                };

                // Read power stats
                let vin = match power.get_vin().await {
                    Ok(mv) => format!("{:.2}V", mv as f32 / 1000.0),
                    Err(_) => "N/A".to_string(),
                };

                let vout = match power.get_vout().await {
                    Ok(mv) => {
                        let volts = mv as f32 / 1000.0;
                        if volts < 1.0 {
                            warn!("Core voltage low: {:.3}V", volts);
                        }
                        format!("{:.3}V", volts)
                    }
                    Err(_) => "N/A".to_string(),
                };

                let iout = match power.get_iout().await {
                    Ok(ma) => format!("{:.2}A", ma as f32 / 1000.0),
                    Err(_) => "N/A".to_string(),
                };

                let power_w = match power.get_power().await {
                    Ok(mw) => format!("{:.1}W", mw as f32 / 1000.0),
                    Err(_) => "N/A".to_string(),
                };

                let vr_temp = match power.get_temperature().await {
                    Ok(t) => format!("{}°C", t),
                    Err(_) => "N/A".to_string(),
                };

                // Check power status
                if let Err(e) = power.check_status().await {
                    warn!("Power controller status check failed: {}", e);
                }

                info!(
                    "Board stats - Chips: {}, ASIC: {}, Fan: {} ({}), VR: {}, Power: {} @ {} (Vin: {}, Vout: {})",
                    chip_count, temp, fan_pwm, fan_rpm, vr_temp, power_w, iout, vin, vout
                );
            }
        });

        self.stats_task_handle = Some(handle);
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

        // Phase 1: Initialize power controller FIRST (before chip communication)
        // This ensures stable core voltage before chip configuration
        tracing::info!("Initializing power management");

        // Initialize fan controller first to test I2C
        self.init_fan_controller().await?;

        // Initialize power controller and set core voltage
        self.init_power_controller().await?;

        // Wait for voltage to fully stabilize after PMIC init
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Phase 2: Initial communication at 115200 baud
        tracing::info!("Starting chip initialization at 115200 baud");

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

        // Verify we found the expected BM1370 chip
        if let Some(first_chip) = self.chip_infos.first() {
            if first_chip.chip_id != Self::EXPECTED_CHIP_ID {
                return Err(BoardError::InitializationFailed(format!(
                    "Wrong chip type for Bitaxe Gamma: expected BM1370 ({:02x}{:02x}), found {:02x}{:02x}",
                    Self::EXPECTED_CHIP_ID[0], Self::EXPECTED_CHIP_ID[1],
                    first_chip.chip_id[0], first_chip.chip_id[1]
                )));
            }
            tracing::debug!("Found expected BM1370 chip");
        }

        // BM1370 requires additional initialization after chip discovery
        if self.chip_infos.len() > 0 {
            // Send core register control commands
            let core_reg_cmd1 = Command::WriteRegister {
                all: true,
                chip_address: 0x00,
                register: bm13xx::protocol::Register::CoreRegister {
                    raw_value: 0x8000_8B00,
                },
            };
            self.send_config_command(core_reg_cmd1).await?;

            let core_reg_cmd2 = Command::WriteRegister {
                all: true,
                chip_address: 0x00,
                register: bm13xx::protocol::Register::CoreRegister {
                    raw_value: 0x8000_800C,
                },
            };
            self.send_config_command(core_reg_cmd2).await?;

            let core_reg_cmd3 = Command::WriteRegister {
                all: true,
                chip_address: 0x00,
                register: bm13xx::protocol::Register::CoreRegister {
                    raw_value: 0x8000_8DEE,
                },
            };
            self.send_config_command(core_reg_cmd3).await?;

            // Phase 3: Send baud rate change command to chip
            // Bitaxe Gamma always changes from 115200 to 1Mbps
            tracing::info!("Sending baud rate change command to BM1370 for {} baud", Self::TARGET_BAUD_RATE);
            let baud_cmd = Command::WriteRegister {
                all: true,
                chip_address: 0x00,
                register: bm13xx::protocol::Register::UartBaud(Self::CHIP_BAUD_REGISTER),
            };
            self.send_config_command(baud_cmd).await?;

            // Give chip time to process baud rate change
            tokio::time::sleep(Duration::from_millis(50)).await;

            // Phase 3: Change host baud rate to match
            tracing::info!("Changing host serial port from 115200 to {} baud", Self::TARGET_BAUD_RATE);
            self.data_control.set_baud_rate(Self::TARGET_BAUD_RATE)
                .map_err(|e| BoardError::InitializationFailed(
                    format!("Failed to change baud rate: {}", e)
                ))?;

            // Wait for baud rate change to stabilize
            tokio::time::sleep(Duration::from_millis(100)).await;

            tracing::info!("Baud rate change complete, continuing at {} baud", Self::TARGET_BAUD_RATE);

            // Start with lower frequency like esp-miner does
            // esp-miner starts at 62.5MHz and ramps up to target
            let start_freq_config = bm13xx::protocol::PllConfig::new(
                0x00A0, // Start lower: 62.5MHz (fb_div 0xA0, no 0x40 flag)
                0x02,   // ref_div
                0x41,   // post_div
            );
            let start_pll_cmd = Command::WriteRegister {
                all: true,
                chip_address: 0x00,
                register: bm13xx::protocol::Register::PllDivider(start_freq_config),
            };
            self.send_config_command(start_pll_cmd).await?;

            // Small delay for initial PLL to stabilize
            tokio::time::sleep(Duration::from_millis(200)).await;

            // Now set target frequency (200 MHz)
            let target_pll_config = bm13xx::protocol::PllConfig::new(
                0x40A0, // Target: 200MHz (fb_div 0xA0 with 0x40 flag)
                0x02,   // ref_div
                0x41,   // post_div
            );
            let target_pll_cmd = Command::WriteRegister {
                all: true,
                chip_address: 0x00,
                register: bm13xx::protocol::Register::PllDivider(target_pll_config),
            };
            self.send_config_command(target_pll_cmd).await?;

            // Longer delay for target frequency to stabilize
            tokio::time::sleep(Duration::from_millis(300)).await;

            // Set ticket mask for difficulty
            // Register 0x14: ticket mask (difficulty control)
            let difficulty_mask = bm13xx::protocol::DifficultyMask::from_difficulty(256);
            let ticket_mask_cmd = Command::WriteRegister {
                all: true,
                chip_address: 0x00,
                register: bm13xx::protocol::Register::TicketMask(difficulty_mask),
            };
            self.send_config_command(ticket_mask_cmd).await?;

            // Additional misc settings from esp-miner
            // Register 0xB9: Unknown misc settings
            let misc_b9_cmd = Command::WriteRegister {
                all: true,
                chip_address: 0x00,
                register: bm13xx::protocol::Register::MiscSettings {
                    raw_value: 0x00004480,
                },
            };
            self.send_config_command(misc_b9_cmd).await?;

            // Register 0x54: Analog mux control (temperature diode)
            let analog_mux_cmd = Command::WriteRegister {
                all: true,
                chip_address: 0x00,
                register: bm13xx::protocol::Register::AnalogMux {
                    raw_value: 0x00000002,
                },
            };
            self.send_config_command(analog_mux_cmd).await?;

            // Send misc B9 again (esp-miner does this)
            let misc_b9_cmd2 = Command::WriteRegister {
                all: true,
                chip_address: 0x00,
                register: bm13xx::protocol::Register::MiscSettings {
                    raw_value: 0x00004480,
                },
            };
            self.send_config_command(misc_b9_cmd2).await?;

            // Register 0x10: Hash counting register (nonce range)
            // Use S21 Pro value (0x1EB5) that esp-miner uses for BM1370
            // This is critical for proper nonce generation and return
            let hash_count_cmd = Command::WriteRegister {
                all: true,
                chip_address: 0x00,
                register: bm13xx::protocol::Register::NonceRange(
                    // Use the S21 Pro configuration that esp-miner uses
                    bm13xx::protocol::NonceRangeConfig::multi_chip(65)
                ),
            };
            self.send_config_command(hash_count_cmd).await?;
        }

        // Create event channel
        let (tx, rx) = tokio::sync::mpsc::channel(100);
        self.event_tx = Some(tx);
        self.event_rx = Some(rx);

        // TODO: Spawn task to monitor chip responses and emit events
        self.spawn_event_monitor();

        // Spawn statistics monitoring task
        self.spawn_stats_monitor();

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
        // Convert the job into a BM13xx command
        let command = self.protocol.encode_mining_job(job, self.next_job_id);

        // Send the job command
        self.data_writer
            .send(command)
            .await
            .map_err(|e| BoardError::Communication(e))?;

        // Update job tracking
        self.current_job_id = Some(job.job_id);

        // Increment job ID counter
        self.next_job_id = (self.next_job_id + 24) % 128;

        // Spawn job completion timer
        self.spawn_job_timer(self.current_job_id);

        tracing::info!(
            "Sent job {} to {} chip(s) with internal ID {}",
            job.job_id,
            self.chip_infos.len(),
            self.next_job_id.saturating_sub(24)
        );

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

    async fn shutdown(&mut self) -> Result<(), BoardError> {
        tracing::info!("Shutting down Bitaxe board");

        // Send chain inactive command to stop all chips from hashing
        let command = Command::ChainInactive;
        self.send_config_command(command).await?;

        // Give chips time to stop hashing
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Hold chips in reset to ensure they stay in a safe state
        self.hold_in_reset().await?;

        // Turn off core voltage
        if let Some(ref mut power) = self.power_controller {
            match power.set_vout(0.0).await {
                Ok(()) => info!("Core voltage turned off"),
                Err(e) => warn!("Failed to turn off core voltage: {}", e),
            }
        }

        // Cancel the statistics monitoring task
        if let Some(handle) = self.stats_task_handle.take() {
            handle.abort();
        }

        tracing::info!("Bitaxe board shutdown complete");
        Ok(())
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

    // Create the board with the control port and data port path
    let mut board = BitaxeBoard::new(control_port, &device.serial_ports[1])
        .map_err(|e| crate::error::Error::Hardware(format!("Failed to create board: {}", e)))?;

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
