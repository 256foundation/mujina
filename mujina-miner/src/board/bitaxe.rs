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

use crate::asic::bm13xx::{
    self,
    protocol::{Command, Hashrate, ReportingInterval, ReportingRate, TicketMask},
    BM13xxProtocol,
};
use crate::asic::{ChipInfo, MiningJob};
use crate::board::{Board, BoardError, BoardEvent, BoardInfo, JobCompleteReason};
use crate::hw_trait::gpio::{Gpio, GpioPin, PinValue};
use crate::hw_trait::i2c::I2c;
use crate::mgmt_protocol::bitaxe_raw::i2c::BitaxeRawI2c;
use crate::mgmt_protocol::{BitaxeRawGpio, ControlChannel};
use crate::peripheral::emc2101::Emc2101;
use crate::peripheral::tps546::{Tps546, Tps546Config};
use crate::tracing::prelude::*;
use crate::transport::serial::{SerialControl, SerialReader, SerialStream, SerialWriter};

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

        // Trace any bytes received
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
    /// Reader for receiving responses from chips (transferred to event monitor during initialize)
    data_reader: Option<FramedRead<TracingReader<SerialReader>, bm13xx::FrameCodec>>,
    /// Control handle for data channel (for baud rate changes)
    data_control: SerialControl,
    /// Protocol handler for chip communication
    protocol: BM13xxProtocol,
    /// Discovered chip information (passive record-keeping)
    chip_infos: Vec<ChipInfo>,
    /// Channel for sending board events
    event_tx: Option<tokio::sync::mpsc::Sender<BoardEvent>>,
    /// Channel for receiving board events (populated during initialization)
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
        let data_stream = SerialStream::new(data_path, 115200).map_err(|e| {
            BoardError::InitializationFailed(format!("Failed to open data port: {}", e))
        })?;
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
        let mut reset_pin =
            self.gpio.pin(Self::ASIC_RESET_PIN).await.map_err(|e| {
                BoardError::HardwareControl(format!("Failed to get reset pin: {}", e))
            })?;

        // Set reset high (inactive)
        tracing::debug!(
            "De-asserting ASIC reset (GPIO {} = high)",
            Self::ASIC_RESET_PIN
        );
        reset_pin.write(PinValue::High).await.map_err(|e| {
            BoardError::HardwareControl(format!("Failed to de-assert reset: {}", e))
        })?;

        Ok(())
    }

    /// Hold the mining chips in reset state.
    ///
    /// This function sets the reset line low and keeps it there,
    /// effectively disabling all connected mining chips. This is used
    /// during shutdown to ensure chips are in a safe, non-hashing state.
    pub async fn hold_in_reset(&mut self) -> Result<(), BoardError> {
        // Get the ASIC reset pin
        let mut reset_pin =
            self.gpio.pin(Self::ASIC_RESET_PIN).await.map_err(|e| {
                BoardError::HardwareControl(format!("Failed to get reset pin: {}", e))
            })?;

        // Hold reset low (active)
        tracing::debug!(
            "Holding ASIC in reset (GPIO {} = low)",
            Self::ASIC_RESET_PIN
        );
        reset_pin
            .write(PinValue::Low)
            .await
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
            .map_err(BoardError::Communication)
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
            .map_err(BoardError::Communication)?;

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

                // Set initial output voltage, default for BM1370 from esp-miner
                const DEFAULT_VOUT: f32 = 1.2;
                match tps546.set_vout(DEFAULT_VOUT).await {
                    Ok(()) => {
                        info!("Core voltage set to {DEFAULT_VOUT}V");

                        // Wait for voltage to stabilize
                        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

                        // Verify voltage
                        match tps546.get_vout().await {
                            Ok(mv) => info!("Core voltage readback: {:.3}V", mv as f32 / 1000.0),
                            Err(e) => warn!("Failed to read core voltage: {}", e),
                        }

                        // Dump complete configuration for debugging
                        if let Err(e) = tps546.dump_configuration().await {
                            warn!("Failed to dump TPS546 configuration: {}", e);
                        }
                    }
                    Err(e) => {
                        error!("Failed to set initial core voltage: {}", e);
                        return Err(BoardError::InitializationFailed(format!(
                            "Failed to set core voltage: {}",
                            e
                        )));
                    }
                }

                self.power_controller = Some(tps546);
                Ok(())
            }
            Err(e) => {
                error!("Failed to initialize TPS546D24A power controller: {}", e);
                Err(BoardError::InitializationFailed(format!(
                    "Power controller init failed: {}",
                    e
                )))
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

    /// Generate frequency ramp steps for smooth PLL transitions
    ///
    /// Calculates PLL configurations for each frequency step from start to target.
    /// Uses the same algorithm as esp-miner for BM1370 chips.
    fn generate_frequency_ramp_steps(
        start_mhz: f32,
        target_mhz: f32,
        step_mhz: f32,
    ) -> Vec<bm13xx::protocol::PllConfig> {
        let mut configs = Vec::new();
        let mut current = start_mhz;

        // Generate steps from start to target
        while current <= target_mhz {
            if let Some(config) = Self::calculate_pll_for_frequency(current) {
                configs.push(config);
            } else {
                tracing::warn!("Failed to calculate PLL for {:.2} MHz, skipping", current);
            }

            current += step_mhz;

            // Prevent overshoot while ensuring target is included
            if current > target_mhz && (current - step_mhz) < target_mhz {
                current = target_mhz;
            }
        }

        configs
    }

    /// Calculate PLL configuration for a specific frequency
    ///
    /// This follows the BM1370/esp-miner algorithm exactly:
    /// - Crystal frequency: 25 MHz
    /// - ref_div: 2 or 1 (prefer 2)
    /// - post_div1: 1-7 (must be >= post_div2)
    /// - post_div2: 1-7
    /// - fb_div: 0xa0-0xef (160-239)
    ///
    /// Formula: freq = 25 * fb_div / (ref_div * post_div1 * post_div2)
    ///
    /// Note: esp-miner uses a "first-found" algorithm rather than finding the optimal
    /// configuration. This implementation matches that behavior for consistency.
    fn calculate_pll_for_frequency(target_freq: f32) -> Option<bm13xx::protocol::PllConfig> {
        const CRYSTAL_FREQ: f32 = 25.0;
        const MAX_FREQ_ERROR: f32 = 1.0; // Maximum acceptable frequency error in MHz

        let mut best_fb_div = 0u8;
        let mut best_ref_div = 0u8;
        let mut best_post_div1 = 0u8;
        let mut best_post_div2 = 0u8;
        let mut min_error = 10.0; // esp-miner starts with min_difference = 10

        // Follow esp-miner's exact loop order to get same results
        // Stops at first solution under max_diff (1.0 MHz)
        for ref_div in [2, 1] {
            if best_fb_div != 0 {
                break;
            } // Stop once solution is found

            for post_div1 in (1..=7).rev() {
                if best_fb_div != 0 {
                    break;
                }

                for post_div2 in (1..=7).rev() {
                    if best_fb_div != 0 {
                        break;
                    }

                    if post_div1 >= post_div2 {
                        // Calculate required feedback divider (esp-miner uses round())
                        let fb_div_f =
                            (post_div1 * post_div2) as f32 * target_freq * ref_div as f32
                                / CRYSTAL_FREQ;
                        let fb_div = fb_div_f.round() as u8;

                        // Check if fb_div is in valid range
                        if (0xa0..=0xef).contains(&fb_div) {
                            // Calculate actual frequency with this configuration
                            let actual_freq = CRYSTAL_FREQ * fb_div as f32
                                / (ref_div * post_div1 * post_div2) as f32;
                            let error = (actual_freq - target_freq).abs();

                            // esp-miner accepts first solution with error < min_difference AND < max_diff
                            if error < min_error && error < MAX_FREQ_ERROR {
                                best_fb_div = fb_div;
                                best_ref_div = ref_div;
                                best_post_div1 = post_div1;
                                best_post_div2 = post_div2;
                                min_error = error;
                                // esp-miner stops at first solution meeting criteria
                            }
                        }
                    }
                }
            }
        }

        if best_fb_div == 0 {
            return None;
        }

        // Encode post dividers as per hardware format
        let post_div = ((best_post_div1 - 1) << 4) | (best_post_div2 - 1);

        // PllConfig::new() automatically calculates the flag (0x40 or 0x50) based on VCO frequency
        Some(bm13xx::protocol::PllConfig::new(
            best_fb_div,
            best_ref_div,
            post_div,
        ))
    }

    /// Spawn a task to periodically log management statistics
    fn spawn_stats_monitor(&mut self) {
        // Clone data needed for the monitoring task
        let i2c = self.i2c.clone();
        let chip_count = self.chip_infos.len();

        // Clone event channel for reporting critical faults
        let event_tx = self.event_tx.clone();

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

                // Check power status - critical faults will return error
                if let Err(e) = power.check_status().await {
                    // Log the critical fault
                    error!("CRITICAL: Power controller fault detected: {}", e);

                    // Send BoardFault event
                    if let Some(ref tx) = event_tx {
                        let fault_event = BoardEvent::BoardFault {
                            component: "power_controller".to_string(),
                            fault: e.to_string(),
                            recoverable: false, // Power faults are critical
                        };
                        if let Err(send_err) = tx.send(fault_event).await {
                            error!("Failed to send board fault event: {}", send_err);
                        }
                    }

                    // Try to clear the fault once
                    warn!("Attempting to clear power controller faults...");
                    if let Err(clear_err) = power.clear_faults().await {
                        error!("Failed to clear faults: {}", clear_err);
                    }

                    // Continue monitoring - let scheduler decide what to do
                    // Don't exit the task, just skip this iteration
                    continue;
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
        // Phase 1: Hold ASIC in reset during power configuration
        tracing::info!("Holding ASIC in reset during power initialization");
        self.hold_in_reset().await?;

        // Phase 2: Initialize power controller while ASIC is in reset
        // This ensures stable core voltage before chip operation
        tracing::info!("Initializing power management");

        // Set I2C bus frequency to 100kHz for all devices
        self.i2c.set_frequency(100_000).await.map_err(|e| {
            BoardError::InitializationFailed(format!("Failed to set I2C frequency: {}", e))
        })?;

        // Initialize fan controller first to test I2C
        self.init_fan_controller().await?;

        // Initialize power controller and set core voltage
        self.init_power_controller().await?;

        // Wait for voltage to fully stabilize after PMIC init
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Phase 3: Release ASIC from reset now that power is stable
        tracing::info!("Releasing ASIC from reset");
        self.release_reset().await?;

        // Give ASIC time to stabilize after reset release
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Phase 4: Initial communication at 115200 baud
        tracing::info!("Starting chip initialization at 115200 baud");

        // Step 1: Version Mask Configuration (Enable 11-bit responses)
        // The very first commands sent are to configure the version mask.
        // This MUST be done before chip discovery to enable 11-bit response frames.
        // Send 3 times as per reference implementation
        tracing::debug!("Sending version mask configuration (3 times)");
        for i in 1..=3 {
            tracing::trace!("Version mask send {}/3", i);
            let version_cmd = Command::WriteRegister {
                broadcast: true, // Broadcast to all chips
                chip_address: 0x00,
                register: bm13xx::protocol::Register::VersionMask(
                    bm13xx::protocol::VersionMask::full_rolling(),
                ),
            };
            self.send_config_command(version_cmd).await?;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // Small delay after version rolling configuration
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Step 2: Chip Discovery
        self.discover_chips().await?;

        tracing::info!("Discovered {} chip(s)", self.chip_infos.len());

        // Verify expected BM1370 chip was found
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

        // Step 3: Pre-Configuration (after 1-second pause per reference)
        tracing::info!("Waiting 1 second before configuration...");
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Send version mask again
        tracing::debug!("Sending version mask again after discovery");
        let version_cmd = Command::WriteRegister {
            broadcast: true,
            chip_address: 0x00,
            register: bm13xx::protocol::Register::VersionMask(
                bm13xx::protocol::VersionMask::full_rolling(),
            ),
        };
        self.send_config_command(version_cmd).await?;

        // InitControl register = 0x0700
        let init_control_cmd = Command::WriteRegister {
            broadcast: true,
            chip_address: 0x00,
            register: bm13xx::protocol::Register::InitControl {
                raw_value: 0x00000700,
            },
        };
        self.send_config_command(init_control_cmd).await?;

        // MiscControl register = 0xC100F0
        let misc_control_cmd = Command::WriteRegister {
            broadcast: true,
            chip_address: 0x00,
            register: bm13xx::protocol::Register::MiscControl {
                raw_value: 0x00C100F0,
            },
        };
        self.send_config_command(misc_control_cmd).await?;

        // ChainInactive command
        self.send_config_command(Command::ChainInactive).await?;

        // SetChipAddress command for addr=0x00
        let set_addr_cmd = Command::SetChipAddress { chip_address: 0x00 };
        self.send_config_command(set_addr_cmd).await?;

        // BM1370 requires additional initialization after chip discovery
        // Step 4: Core Configuration (Broadcast)
        tracing::debug!("Sending broadcast core configuration");

        // CoreRegister = 0x8B0080 (sent as big-endian: 0x80 0x00 0x8B 0x00)
        let core_reg_cmd1 = Command::WriteRegister {
            broadcast: true,
            chip_address: 0x00,
            register: bm13xx::protocol::Register::Core {
                raw_value: 0x8000_8B00, // Big-endian encoding
            },
        };
        self.send_config_command(core_reg_cmd1).await?;

        // CoreRegister = 0x0C8080 (sent as big-endian: 0x80 0x00 0x80 0x0C)
        let core_reg_cmd2 = Command::WriteRegister {
            broadcast: true,
            chip_address: 0x00,
            register: bm13xx::protocol::Register::Core {
                raw_value: 0x8000_800C, // Big-endian encoding
            },
        };
        self.send_config_command(core_reg_cmd2).await?;

        // Set ticket mask for nonce reporting
        // Bitaxe Gamma: ~500 GH/s, want ~1 nonce/sec
        let reporting_interval = ReportingInterval::from_rate(
            Hashrate::gibihashes_per_sec(500.0),
            ReportingRate::nonces_per_sec(1.0),
        );
        let ticket_mask = TicketMask::new(reporting_interval);
        let ticket_mask_cmd = Command::WriteRegister {
            broadcast: true,
            chip_address: 0x00,
            register: bm13xx::protocol::Register::TicketMask(ticket_mask),
        };
        self.send_config_command(ticket_mask_cmd).await?;

        // IoDriverStrength = 0x11110100
        let io_strength_cmd = Command::WriteRegister {
            broadcast: true,
            chip_address: 0x00,
            register: bm13xx::protocol::Register::IoDriverStrength(
                bm13xx::protocol::IoDriverStrength::normal(),
            ),
        };
        self.send_config_command(io_strength_cmd).await?;

        // Step 5: Chip-Specific Configuration (addr=0x00)
        tracing::debug!("Sending chip-specific configuration for address 0x00");

        // InitControl = 0xF0010700 (chip-specific, not broadcast)
        let init_control_specific = Command::WriteRegister {
            broadcast: false, // Not broadcast - specific to chip 0x00
            chip_address: 0x00,
            register: bm13xx::protocol::Register::InitControl {
                raw_value: 0xF0010700,
            },
        };
        self.send_config_command(init_control_specific).await?;

        // MiscControl = 0xC100F0 (chip-specific)
        let misc_control_specific = Command::WriteRegister {
            broadcast: false,
            chip_address: 0x00,
            register: bm13xx::protocol::Register::MiscControl {
                raw_value: 0x00C100F0,
            },
        };
        self.send_config_command(misc_control_specific).await?;

        // CoreRegister = 0x8B0080 (chip-specific)
        let core_reg_specific1 = Command::WriteRegister {
            broadcast: false,
            chip_address: 0x00,
            register: bm13xx::protocol::Register::Core {
                raw_value: 0x8000_8B00, // Big-endian
            },
        };
        self.send_config_command(core_reg_specific1).await?;

        // CoreRegister = 0x0C8080 (chip-specific)
        let core_reg_specific2 = Command::WriteRegister {
            broadcast: false,
            chip_address: 0x00,
            register: bm13xx::protocol::Register::Core {
                raw_value: 0x8000_800C, // Big-endian
            },
        };
        self.send_config_command(core_reg_specific2).await?;

        // CoreRegister = 0xAA820080 (chip-specific)
        let core_reg_specific3 = Command::WriteRegister {
            broadcast: false,
            chip_address: 0x00,
            register: bm13xx::protocol::Register::Core {
                raw_value: 0x8000_82AA, // Big-endian encoding
            },
        };
        self.send_config_command(core_reg_specific3).await?;

        // Step 6: Additional Settings
        tracing::debug!("Sending additional settings");

        // MiscSettings = 0x80440000 (bytes: 00 00 44 80)
        let misc_b9_cmd = Command::WriteRegister {
            broadcast: true,
            chip_address: 0x00,
            register: bm13xx::protocol::Register::MiscSettings {
                raw_value: 0x80440000, // Little-endian: bytes 00 00 44 80
            },
        };
        self.send_config_command(misc_b9_cmd).await?;

        // Register 0x54: Analog mux control (temperature diode)
        let analog_mux_cmd = Command::WriteRegister {
            broadcast: true,
            chip_address: 0x00,
            register: bm13xx::protocol::Register::AnalogMux {
                raw_value: 0x02000000, // Little-endian: bytes 00 00 00 02
            },
        };
        self.send_config_command(analog_mux_cmd).await?;

        // Send misc B9 again (esp-miner does this)
        let misc_b9_cmd2 = Command::WriteRegister {
            broadcast: true,
            chip_address: 0x00,
            register: bm13xx::protocol::Register::MiscSettings {
                raw_value: 0x80440000, // Little-endian: bytes 00 00 44 80
            },
        };
        self.send_config_command(misc_b9_cmd2).await?;

        // Additional CoreRegister = 0xEE8D0080 (after misc settings)
        let core_reg_final = Command::WriteRegister {
            broadcast: true,
            chip_address: 0x00,
            register: bm13xx::protocol::Register::Core {
                raw_value: 0x8000_8DEE, // Big-endian encoding
            },
        };
        self.send_config_command(core_reg_final).await?;

        // Step 7: Frequency Ramping (56.25 MHz → 525 MHz)
        // The frequency is ramped up gradually through many steps
        // Following esp-miner's approach: 6.25 MHz steps with 100ms delays
        tracing::info!("Starting frequency ramping from 56.25 MHz to 525 MHz");

        // Generate frequency steps programmatically
        // Start at 56.25 MHz, increment by 6.25 MHz up to 525 MHz
        let frequency_steps = Self::generate_frequency_ramp_steps(56.25, 525.0, 6.25);

        tracing::debug!("Ramping through {} frequency steps", frequency_steps.len());

        for (i, pll_config) in frequency_steps.iter().enumerate() {
            let pll_cmd = Command::WriteRegister {
                broadcast: true,
                chip_address: 0x00,
                register: bm13xx::protocol::Register::PllDivider(*pll_config),
            };
            self.send_config_command(pll_cmd).await?;

            // Wait ~100ms between frequency steps (matching esp-miner)
            tokio::time::sleep(Duration::from_millis(100)).await;

            if i % 10 == 0 || i == frequency_steps.len() - 1 {
                tracing::trace!("Frequency ramp step {}/{}", i + 1, frequency_steps.len());
            }
        }

        tracing::info!("Frequency ramping complete");

        // Step 8: Final Configuration
        // After frequency ramping is complete

        // NonceRange = 0xB51E0000 (value from reference implementation)
        let nonce_range_value = 0xB51E0000; // Raw value from captures
        let nonce_range_cmd = Command::WriteRegister {
            broadcast: true,
            chip_address: 0x00,
            register: bm13xx::protocol::Register::NonceRange(
                // Create NonceRangeConfig with the exact value
                // Note: This might need a custom constructor in the protocol module
                bm13xx::protocol::NonceRangeConfig::from_raw(nonce_range_value),
            ),
        };
        self.send_config_command(nonce_range_cmd).await?;

        // UartBaud = 0x00023011 (1M baud) - sent after frequency ramping
        tracing::info!(
            "Sending baud rate change command to BM1370 for {} baud",
            Self::TARGET_BAUD_RATE
        );
        let baud_cmd = Command::WriteRegister {
            broadcast: true,
            chip_address: 0x00,
            register: bm13xx::protocol::Register::UartBaud(Self::CHIP_BAUD_REGISTER),
        };
        self.send_config_command(baud_cmd).await?;

        // Give chip time to process baud rate change
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Change host baud rate to match
        tracing::info!(
            "Changing host serial port from 115200 to {} baud",
            Self::TARGET_BAUD_RATE
        );
        self.data_control
            .set_baud_rate(Self::TARGET_BAUD_RATE)
            .map_err(|e| {
                BoardError::InitializationFailed(format!("Failed to change baud rate: {}", e))
            })?;

        // Wait for baud rate change to stabilize
        tokio::time::sleep(Duration::from_millis(100)).await;

        tracing::info!(
            "Baud rate change complete, continuing at {} baud",
            Self::TARGET_BAUD_RATE
        );

        // Step 9: Version Mask Re-Send (final)
        // After all configuration, send version mask once more
        tracing::debug!("Sending final version mask configuration");
        let version_cmd_final = Command::WriteRegister {
            broadcast: true,
            chip_address: 0x00,
            register: bm13xx::protocol::Register::VersionMask(
                bm13xx::protocol::VersionMask::full_rolling(),
            ),
        };
        self.send_config_command(version_cmd_final).await?;

        // Create event channel
        let (tx, rx) = tokio::sync::mpsc::channel(100);
        self.event_tx = Some(tx);
        self.event_rx = Some(rx);

        // TODO: Spawn task to monitor chip responses and emit events
        self.spawn_event_monitor();

        // Spawn statistics monitoring task
        self.spawn_stats_monitor();

        // Return a dummy receiver for trait compatibility
        // Actual receiver is retrieved via take_event_receiver()
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
            .map_err(BoardError::Communication)?;

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

    // Event receiver is retrieved by the scheduler using take_event_receiver()

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pll_calculations_match_reference() {
        // Test cases from the Bitaxe Gamma protocol capture
        // Format: (freq_mhz, expected_fb_div, expected_ref_div, expected_post_div)
        // Note: The capture shows bytes [55 AA 51 09 00 08 FLAG FB_DIV REF POST CRC]
        // Format: (freq_mhz, expected_flag, expected_fb_div, expected_ref_div, expected_post_div)
        let test_cases = vec![
            // From the capture document:
            (62.50, 0x50, 0xD2, 0x02, 0x65), // tx: [55 AA 51 09 00 08 50 D2 02 65 05]
            (68.75, 0x50, 0xE7, 0x02, 0x65), // tx: [55 AA 51 09 00 08 50 E7 02 65 1C]
            (75.00, 0x50, 0xD2, 0x02, 0x64), // tx: [55 AA 51 09 00 08 50 D2 02 64 00]
            (81.25, 0x50, 0xE4, 0x02, 0x64), // tx: [55 AA 51 09 00 08 50 E4 02 64 14]
            (87.50, 0x50, 0xC4, 0x02, 0x63), // tx: [55 AA 51 09 00 08 50 C4 02 63 18]
            (93.75, 0x50, 0xD2, 0x02, 0x63), // tx: [55 AA 51 09 00 08 50 D2 02 63 1B]
            (100.00, 0x50, 0xE0, 0x02, 0x63), // tx: [55 AA 51 09 00 08 50 E0 02 63 00]
            (525.00, 0x50, 0xD2, 0x02, 0x40), // tx: [55 AA 51 09 00 08 50 D2 02 40 05] (final)
        ];

        for (freq_mhz, expected_flag, expected_fb, expected_ref, expected_post) in test_cases {
            let config = BitaxeBoard::calculate_pll_for_frequency(freq_mhz)
                .unwrap_or_else(|| panic!("Failed to calculate PLL for {} MHz", freq_mhz));

            assert_eq!(
                config.flag, expected_flag,
                "Flag mismatch for {} MHz: expected 0x{:02X}, got 0x{:02X}",
                freq_mhz, expected_flag, config.flag
            );

            assert_eq!(
                config.fb_div, expected_fb,
                "FB divider mismatch for {} MHz: expected 0x{:02X}, got 0x{:02X}",
                freq_mhz, expected_fb, config.fb_div
            );

            assert_eq!(
                config.ref_div, expected_ref,
                "Ref divider mismatch for {} MHz: expected {}, got {}",
                freq_mhz, expected_ref, config.ref_div
            );

            assert_eq!(
                config.post_div, expected_post,
                "Post divider mismatch for {} MHz: expected 0x{:02X}, got 0x{:02X}",
                freq_mhz, expected_post, config.post_div
            );

            // Verify the frequency calculation
            let post_div1 = ((config.post_div >> 4) & 0xF) + 1;
            let post_div2 = (config.post_div & 0xF) + 1;
            let calculated_freq =
                25.0 * config.fb_div as f32 / (config.ref_div * post_div1 * post_div2) as f32;

            assert!(
                (calculated_freq - freq_mhz).abs() < 1.0,
                "Frequency calculation error for {} MHz: calculated {} MHz",
                freq_mhz,
                calculated_freq
            );
        }
    }

    #[test]
    fn test_frequency_ramp_generation() {
        // Verify correct number of steps are generated
        let steps = BitaxeBoard::generate_frequency_ramp_steps(56.25, 525.0, 6.25);

        // Should have steps from 56.25 to 525.0 in 6.25 MHz increments
        // That's (525 - 56.25) / 6.25 + 1 = 75.0 steps
        assert_eq!(steps.len(), 76, "Expected 76 frequency steps");

        // Verify first and last frequencies by calculating them back
        if let Some(first) = steps.first() {
            let post_div1 = ((first.post_div >> 4) & 0xF) + 1;
            let post_div2 = (first.post_div & 0xF) + 1;
            let first_freq = 25.0 * (first.fb_div & 0xFF) as f32
                / (first.ref_div * post_div1 * post_div2) as f32;
            assert!(
                (first_freq - 56.25).abs() < 1.0,
                "First frequency should be ~56.25 MHz"
            );
        }

        if let Some(last) = steps.last() {
            let post_div1 = ((last.post_div >> 4) & 0xF) + 1;
            let post_div2 = (last.post_div & 0xF) + 1;
            let last_freq =
                25.0 * (last.fb_div & 0xFF) as f32 / (last.ref_div * post_div1 * post_div2) as f32;
            assert!(
                (last_freq - 525.0).abs() < 1.0,
                "Last frequency should be ~525 MHz"
            );
        }
    }

    #[test]
    fn test_pll_flag_setting() {
        // Test that the VCO-based flag is set correctly
        // Flag is 0x50 when VCO frequency >= 2400 MHz, 0x40 otherwise
        // VCO frequency = fb_div * 25.0 / ref_div

        // Low frequency (100 MHz) - VCO will be >= 2400, so should use 0x50
        let low_freq = BitaxeBoard::calculate_pll_for_frequency(100.0).unwrap();
        assert_eq!(low_freq.flag, 0x50, "Should have 0x50 flag for 100 MHz");

        // High frequency (525 MHz) - VCO will be >= 2400, so should use 0x50
        let high_freq = BitaxeBoard::calculate_pll_for_frequency(525.0).unwrap();
        assert_eq!(high_freq.flag, 0x50, "Should have 0x50 flag for 525 MHz");

        // The 0x40 flag would be used for configurations with VCO < 2400 MHz,
        // which aren't typically reached in the 56.25-525 MHz output range
    }
}
