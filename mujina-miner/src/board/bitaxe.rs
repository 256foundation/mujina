use async_trait::async_trait;
use futures::sink::SinkExt;
use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, ReadBuf},
    sync::{Mutex, watch},
    time,
};
use tokio_stream::StreamExt;
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::{
    api::commands::BoardCommand,
    api_client::types::{BoardState, Fan, PowerMeasurement, TemperatureSensor},
    asic::{
        ChipInfo,
        bm13xx::{self, BM13xxProtocol, protocol::Command, thread::BM13xxThread},
        hash_thread::{BoardPeripherals, HashThread, ThreadRemovalSignal},
    },
    hw_trait::{
        gpio::{Gpio, GpioPin, PinValue},
        i2c::I2c,
    },
    mgmt_protocol::{
        ControlChannel,
        bitaxe_raw::{
            gpio::{BitaxeRawGpioController, BitaxeRawGpioPin},
            i2c::BitaxeRawI2c,
        },
    },
    peripheral::{
        emc2101::{Emc2101, Percent},
        tps546::{Tps546, Tps546Config},
    },
    tracing::prelude::*,
    transport::serial::{SerialControl, SerialReader, SerialStream, SerialWriter},
};

use super::{
    Board, BoardError, BoardInfo,
    pattern::{Match, StringMatch},
};

/// Adapter implementing `AsicEnable` for Bitaxe's GPIO-based reset control.
struct BitaxeAsicEnable {
    /// Reset pin (directly controls nRST on the BM1370)
    nrst_pin: BitaxeRawGpioPin,
}

#[async_trait]
impl crate::asic::hash_thread::AsicEnable for BitaxeAsicEnable {
    async fn enable(&mut self) -> anyhow::Result<()> {
        // Release reset (nRST is active-low, so High = running)
        self.nrst_pin
            .write(PinValue::High)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to release reset: {}", e))
    }

    async fn disable(&mut self) -> anyhow::Result<()> {
        // Assert reset (nRST is active-low, so Low = reset)
        self.nrst_pin
            .write(PinValue::Low)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to assert reset: {}", e))
    }
}

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
    control_channel: ControlChannel,
    /// ASIC reset (active low)
    asic_nrst: Option<BitaxeRawGpioPin>,
    /// I2C bus controller
    i2c: BitaxeRawI2c,
    /// Fan controller (board-controlled only, not shared with thread)
    fan_controller: Option<Emc2101<BitaxeRawI2c>>,
    /// Voltage regulator (shared with thread, cached state)
    regulator: Option<Arc<Mutex<Tps546<BitaxeRawI2c>>>>,
    /// Writer for sending commands to chips (transferred to hash thread)
    data_writer: Option<FramedWrite<SerialWriter, bm13xx::FrameCodec>>,
    /// Reader for receiving responses from chips (transferred to hash thread)
    data_reader: Option<FramedRead<TracingReader<SerialReader>, bm13xx::FrameCodec>>,
    /// Control handle for data channel (for baud rate changes)
    #[expect(dead_code, reason = "will be used when baud rate change is fixed")]
    data_control: SerialControl,
    /// Discovered chip information (passive record-keeping)
    chip_infos: Vec<ChipInfo>,
    /// Thread shutdown signal (board-to-thread implementation detail)
    thread_shutdown: Option<watch::Sender<ThreadRemovalSignal>>,
    /// Handle for the statistics task
    stats_task_handle: Option<tokio::task::JoinHandle<()>>,
    /// Serial number from USB device info
    serial_number: Option<String>,
    /// Channel for publishing board state to the API server.
    /// Taken by `spawn_stats_monitor` which publishes periodic snapshots.
    state_tx: Option<watch::Sender<BoardState>>,
    /// Command receiver from the API layer.
    /// Taken by `spawn_stats_monitor` so the monitoring loop can handle commands.
    cmd_rx: Option<tokio::sync::mpsc::Receiver<BoardCommand>>,
}

impl BitaxeBoard {
    /// GPIO pin number for ASIC reset control (active low)
    const ASIC_RESET_PIN: u8 = 0;

    /// Bitaxe Gamma board configuration
    /// The Gamma uses a BM1370 chip and runs at 1Mbps after initialization
    #[expect(dead_code, reason = "will be used when baud rate change is fixed")]
    const TARGET_BAUD_RATE: u32 = 1_000_000;
    #[expect(dead_code, reason = "will be used when baud rate change is fixed")]
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
    pub fn new(
        control: tokio_serial::SerialStream,
        data_path: &str,
        serial_number: Option<String>,
        state_tx: watch::Sender<BoardState>,
        cmd_rx: tokio::sync::mpsc::Receiver<BoardCommand>,
    ) -> Result<Self, BoardError> {
        // Create control channel and I2C controller
        let control_channel = ControlChannel::new(control);
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
            asic_nrst: None,
            i2c,
            fan_controller: None,
            regulator: None,
            data_writer: Some(FramedWrite::new(data_writer, bm13xx::FrameCodec)),
            data_reader: Some(FramedRead::new(tracing_reader, bm13xx::FrameCodec)),
            data_control,
            chip_infos: Vec::new(),
            thread_shutdown: None,
            stats_task_handle: None,
            serial_number,
            state_tx: Some(state_tx),
            cmd_rx: Some(cmd_rx),
        })
    }

    /// Performs a momentary reset of the mining chips via GPIO control.
    ///
    /// This function toggles the reset line low for 100ms, then high for 100ms
    /// to properly reset all connected mining chips.
    #[expect(dead_code, reason = "will be used for error recovery")]
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
        let reset_pin = self
            .asic_nrst
            .as_mut()
            .ok_or_else(|| BoardError::HardwareControl("Reset pin not initialized".to_string()))?;

        // Set reset high (inactive - active low signal)
        debug!(
            "De-asserting ASIC nRST (GPIO {} = high)",
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
        let reset_pin = self
            .asic_nrst
            .as_mut()
            .ok_or_else(|| BoardError::HardwareControl("Reset pin not initialized".to_string()))?;

        // Hold reset low (active - active low signal)
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
            .as_mut()
            .expect("data_writer should be available during initialization")
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
            .as_mut()
            .expect("data_writer should be available during chip discovery")
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
                            debug!("Discovered chip {:?} ({:02x}{:02x}) at address {address}",
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
                            warn!("Unexpected response during chip discovery");
                        }
                        Some(Err(e)) => {
                            error!("Error during chip discovery: {e}");
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

    /// Initialize the power controller
    async fn init_power_controller(&mut self) -> Result<(), BoardError> {
        // Clone the I2C bus for the power controller
        let power_i2c = self.i2c.clone();

        // Bitaxe Gamma power configuration for TPS546D24A
        let config = Tps546Config {
            // Phase and frequency
            phase: 0x00,
            frequency_switch_khz: 650,

            // Input voltage thresholds
            vin_on: 4.8,
            vin_off: 4.5,
            vin_uv_warn_limit: 0.0, // Disabled due to TI bug
            vin_ov_fault_limit: 6.5,
            vin_ov_fault_response: 0xB7, // Immediate shutdown, 6 retries, 7xTON_RISE delay

            // Output voltage configuration
            vout_scale_loop: 0.25,
            vout_min: 1.0,
            vout_max: 2.0,
            vout_command: 1.15, // BM1370 default voltage

            // Output voltage protection (relative to vout_command)
            vout_ov_fault_limit: 1.25, // 125% of VOUT_COMMAND
            vout_ov_warn_limit: 1.16,  // 116% of VOUT_COMMAND
            vout_margin_high: 1.10,    // 110% of VOUT_COMMAND
            vout_margin_low: 0.90,     // 90% of VOUT_COMMAND
            vout_uv_warn_limit: 0.90,  // 90% of VOUT_COMMAND
            vout_uv_fault_limit: 0.75, // 75% of VOUT_COMMAND

            // Output current protection
            iout_oc_warn_limit: 25.0,
            iout_oc_fault_limit: 30.0,
            iout_oc_fault_response: 0xC0, // Shutdown immediately, no retries

            // Temperature protection
            ot_warn_limit: 105,      // degC
            ot_fault_limit: 145,     // degC
            ot_fault_response: 0xFF, // Infinite retries

            // Timing configuration
            ton_delay: 0,
            ton_rise: 3,
            ton_max_fault_limit: 0,
            ton_max_fault_response: 0x3B, // 3 retries, 91ms delay
            toff_delay: 0,
            toff_fall: 0,

            // Pin configuration
            pin_detect_override: 0xFFFF,
        };

        let mut tps546 = Tps546::new(power_i2c, config);

        // Initialize the TPS546
        match tps546.init().await {
            Ok(()) => {
                // Delay before setting voltage
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

                // Set initial output voltage, default for BM1370 from esp-miner
                const DEFAULT_VOUT: f32 = 1.15;
                match tps546.set_vout(DEFAULT_VOUT).await {
                    Ok(()) => {
                        debug!("Core voltage set to {DEFAULT_VOUT}V");

                        // Wait for voltage to stabilize
                        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

                        // Verify voltage
                        match tps546.get_vout().await {
                            Ok(mv) => debug!("Core voltage readback: {:.3}V", mv as f32 / 1000.0),
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

                self.regulator = Some(Arc::new(Mutex::new(tps546)));
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
                // Set fan to full speed until closed-loop control is implemented
                match fan.set_fan_speed(Percent::FULL).await {
                    Ok(()) => {
                        debug!("Fan speed set to 100%");
                    }
                    Err(e) => {
                        warn!("Failed to set fan speed: {}", e);
                    }
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
    #[cfg(test)]
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
                warn!("Failed to calculate PLL for {:.2} MHz, skipping", current);
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
    #[cfg(test)]
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

    /// Initialize the board and discover connected chips.
    ///
    /// After initialization, the board is ready for `create_hash_threads()`.
    pub async fn initialize(&mut self) -> Result<(), BoardError> {
        // Create GPIO controller and get reset pin handle
        let mut gpio_controller = BitaxeRawGpioController::new(self.control_channel.clone());
        let reset_pin = gpio_controller
            .pin(Self::ASIC_RESET_PIN)
            .await
            .map_err(|e| {
                BoardError::InitializationFailed(format!("Failed to get reset pin: {}", e))
            })?;
        self.asic_nrst = Some(reset_pin);

        // Phase 1: Hold ASIC in reset during power configuration
        trace!("Holding ASIC in reset during power initialization");
        self.hold_in_reset().await?;

        // Phase 2: Initialize power controller while ASIC is in reset
        self.i2c.set_frequency(100_000).await.map_err(|e| {
            BoardError::InitializationFailed(format!("Failed to set I2C frequency: {}", e))
        })?;

        self.init_fan_controller().await?;
        self.init_power_controller().await?;

        tokio::time::sleep(Duration::from_millis(500)).await;

        // Phase 3: Release ASIC from reset for discovery
        debug!("Releasing ASIC from reset for discovery");
        self.release_reset().await?;

        tokio::time::sleep(Duration::from_millis(200)).await;

        // Phase 4: Version mask and chip discovery
        debug!("Sending version mask configuration (3 times)");
        for i in 1..=3 {
            trace!("Version mask send {}/3", i);
            let version_cmd = Command::WriteRegister {
                broadcast: true,
                chip_address: 0x00,
                register: bm13xx::protocol::Register::VersionMask(
                    bm13xx::protocol::VersionMask::full_rolling(),
                ),
            };
            self.send_config_command(version_cmd).await?;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        tokio::time::sleep(Duration::from_millis(10)).await;

        self.discover_chips().await?;

        debug!(count = self.chip_infos.len(), "Discovered chips");

        // Verify expected BM1370 chip was found
        if let Some(first_chip) = self.chip_infos.first()
            && first_chip.chip_id != Self::EXPECTED_CHIP_ID
        {
            return Err(BoardError::InitializationFailed(format!(
                "Wrong chip type for Bitaxe Gamma: expected BM1370 ({:02x}{:02x}), found {:02x}{:02x}",
                Self::EXPECTED_CHIP_ID[0],
                Self::EXPECTED_CHIP_ID[1],
                first_chip.chip_id[0],
                first_chip.chip_id[1]
            )));
        }

        // Put chip back in reset
        self.hold_in_reset().await?;

        // Spawn statistics monitoring task
        self.spawn_stats_monitor();

        Ok(())
    }

    /// Number of discovered chips on this board.
    pub fn chip_count(&self) -> usize {
        self.chip_infos.len()
    }

    /// Spawn a task to periodically log and publish board telemetry.
    fn spawn_stats_monitor(&mut self) {
        // Clone data needed for the monitoring task
        let i2c = self.i2c.clone();

        // Clone the regulator Arc for stats monitoring
        let regulator = self
            .regulator
            .clone()
            .expect("Regulator must be initialized before spawning stats monitor");

        // Capture board info for logging
        let board_info = self.board_info();
        let board_name = format!(
            "bitaxe-{}",
            board_info.serial_number.as_deref().unwrap_or("unknown")
        );
        let board_model = board_info.model.clone();
        let board_serial = board_info.serial_number.clone();

        // Take the state sender so this task owns publishing
        let state_tx = self
            .state_tx
            .take()
            .expect("state_tx must be present when spawning stats monitor");

        // Take the command receiver so this task handles API commands
        let mut cmd_rx = self
            .cmd_rx
            .take()
            .expect("cmd_rx must be present when spawning stats monitor");

        let handle = tokio::spawn(async move {
            const STATS_INTERVAL: Duration = Duration::from_secs(5);
            let mut interval = tokio::time::interval(STATS_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            // Create fan controller for the stats task
            let mut fan_ctrl = Emc2101::new(i2c);

            const LOG_INTERVAL: Duration = Duration::from_secs(30);
            let mut last_log = tokio::time::Instant::now();

            // Discard first tick (fires immediately, ADC readings may not be settled)
            interval.tick().await;

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        // -- Read sensor values --

                        let asic_temp = fan_ctrl.get_external_temperature().await.ok();
                        let fan_percent = fan_ctrl.get_fan_speed().await.ok().map(u8::from);
                        let fan_rpm = fan_ctrl.get_rpm().await.ok();

                        let (vin_mv, vout_mv, iout_ma, power_mw, vr_temp) = {
                            let mut reg = regulator.lock().await;
                            (
                                reg.get_vin().await.ok(),
                                reg.get_vout().await.ok(),
                                reg.get_iout().await.ok(),
                                reg.get_power().await.ok(),
                                reg.get_temperature().await.ok(),
                            )
                        };

                        if let Some(mv) = vout_mv {
                            let volts = mv as f32 / 1000.0;
                            if volts < 1.0 {
                                warn!("Core voltage low: {:.3}V", volts);
                            }
                        }

                        // Check power status -- critical faults will return error
                        {
                            let mut reg = regulator.lock().await;
                            if let Err(e) = reg.check_status().await {
                                error!("CRITICAL: Power controller fault detected: {}", e);

                                warn!("Attempting to clear power controller faults...");
                                if let Err(clear_err) = reg.clear_faults().await {
                                    error!("Failed to clear faults: {}", clear_err);
                                }

                                continue;
                            }
                        }

                        // -- Publish BoardState --

                        let _ = state_tx.send(BoardState {
                            name: board_name.clone(),
                            model: board_model.clone(),
                            serial: board_serial.clone(),
                            fans: vec![Fan {
                                name: "fan".into(),
                                rpm: fan_rpm,
                                percent: fan_percent,
                                target_percent: None,
                            }],
                            temperatures: vec![
                                TemperatureSensor {
                                    name: "asic".into(),
                                    temperature_c: asic_temp,
                                },
                                TemperatureSensor {
                                    name: "vr".into(),
                                    temperature_c: vr_temp.map(|t| t as f32),
                                },
                            ],
                            powers: vec![
                                PowerMeasurement {
                                    name: "input".into(),
                                    voltage_v: vin_mv.map(|mv| mv as f32 / 1000.0),
                                    current_a: None,
                                    power_w: None,
                                },
                                PowerMeasurement {
                                    name: "core".into(),
                                    voltage_v: vout_mv.map(|mv| mv as f32 / 1000.0),
                                    current_a: iout_ma.map(|ma| ma as f32 / 1000.0),
                                    power_w: power_mw.map(|mw| mw as f32 / 1000.0),
                                },
                            ],
                            threads: Vec::new(),
                        });

                        // -- Log summary (throttled) --

                        if last_log.elapsed() >= LOG_INTERVAL {
                            last_log = tokio::time::Instant::now();
                            info!(
                                board = %board_model,
                                serial = ?board_serial,
                                asic_temp_c = ?asic_temp,
                                fan_percent = ?fan_percent,
                                fan_rpm = ?fan_rpm,
                                vr_temp_c = ?vr_temp,
                                power_w = ?power_mw.map(|mw| mw as f32 / 1000.0),
                                current_a = ?iout_ma.map(|ma| ma as f32 / 1000.0),
                                vin_v = ?vin_mv.map(|mv| mv as f32 / 1000.0),
                                vout_v = ?vout_mv.map(|mv| mv as f32 / 1000.0),
                                "Board status."
                            );
                        }
                    }

                    Some(cmd) = cmd_rx.recv() => {
                        handle_board_command(cmd, &regulator).await;
                    }
                }
            }
        });

        self.stats_task_handle = Some(handle);
    }
}

/// Handle a single board command received from the API layer.
async fn handle_board_command(
    cmd: BoardCommand,
    regulator: &Arc<Mutex<crate::peripheral::tps546::Tps546<crate::mgmt_protocol::bitaxe_raw::i2c::BitaxeRawI2c>>>,
) {
    match cmd {
        BoardCommand::SetVoltage { domain, voltage_v, reply } => {
            if domain != "core" {
                let _ = reply.send(Err(anyhow::anyhow!("unknown power domain: {}", domain)));
                return;
            }
            let result = regulator.lock().await.set_vout(voltage_v).await;
            let _ = reply.send(result);
        }
        BoardCommand::SetFanTarget { reply, .. } => {
            let _ = reply.send(Err(anyhow::anyhow!("fan target control not yet implemented")));
        }
    }
}

#[async_trait]
impl Board for BitaxeBoard {
    fn board_info(&self) -> BoardInfo {
        BoardInfo {
            model: "Bitaxe Gamma".to_string(),
            firmware_version: Some("bitaxe-raw".to_string()),
            serial_number: self.serial_number.clone(),
        }
    }

    async fn shutdown(&mut self) -> Result<(), BoardError> {
        // Signal hash threads to shut down gracefully
        if let Some(ref tx) = self.thread_shutdown {
            if let Err(e) = tx.send(ThreadRemovalSignal::Shutdown) {
                warn!("Failed to send shutdown signal to threads: {}", e);
            } else {
                debug!("Sent shutdown signal to hash threads");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }

        // Hold chips in reset
        self.hold_in_reset().await?;

        // Turn off core voltage
        if let Some(ref regulator) = self.regulator {
            match regulator.lock().await.set_vout(0.0).await {
                Ok(()) => debug!("Core voltage turned off"),
                Err(e) => warn!("Failed to turn off core voltage: {}", e),
            }
        }

        // Reduce fan speed (no more heat generation)
        if let Some(ref mut fan) = self.fan_controller {
            let shutdown_speed = Percent::new_clamped(25);
            if let Err(e) = fan.set_fan_speed(shutdown_speed).await {
                warn!("Failed to set fan speed: {}", e);
            }
        }

        // Cancel the statistics monitoring task
        if let Some(handle) = self.stats_task_handle.take() {
            handle.abort();
        }

        Ok(())
    }

    async fn create_hash_threads(&mut self) -> Result<Vec<Box<dyn HashThread>>, BoardError> {
        // Create removal signal channel (starts as Running)
        let (removal_tx, removal_rx) = watch::channel(ThreadRemovalSignal::Running);

        // Store removal signal sender for later shutdown
        self.thread_shutdown = Some(removal_tx);

        // Take ownership of serial I/O streams
        let data_reader = self
            .data_reader
            .take()
            .ok_or(BoardError::InitializationFailed(
                "No data reader available - already taken or not initialized".into(),
            ))?;

        let data_writer = self
            .data_writer
            .take()
            .ok_or(BoardError::InitializationFailed(
                "No data writer available - already taken or not initialized".into(),
            ))?;

        // Create ASIC enable adapter (wraps GPIO pin)
        let nrst_pin = self
            .asic_nrst
            .clone()
            .ok_or(BoardError::InitializationFailed(
                "Reset pin not initialized".into(),
            ))?;
        let asic_enable = BitaxeAsicEnable { nrst_pin };

        // Bundle peripherals for thread
        let peripherals = BoardPeripherals {
            asic_enable: Some(Box::new(asic_enable)),
            voltage_regulator: None, // Not used by hash thread yet
        };

        // Build thread name from board model and serial
        let thread_name = match &self.serial_number {
            Some(serial) => format!("Bitaxe-Gamma-{}", &serial[..8.min(serial.len())]),
            None => "Bitaxe-Gamma".to_string(),
        };

        // Create BM13xxThread with streams and peripherals
        let thread = BM13xxThread::new(
            thread_name,
            data_reader,
            data_writer,
            peripherals,
            removal_rx,
        );

        debug!("Created BM13xx hash thread from BitaxeBoard");

        Ok(vec![Box::new(thread)])
    }
}

// Factory function to create a Bitaxe board from USB device info
async fn create_from_usb(
    device: crate::transport::UsbDeviceInfo,
) -> crate::error::Result<(Box<dyn Board + Send>, super::BoardRegistration)> {
    use tokio_serial::SerialPortBuilderExt;

    // Get serial ports
    let serial_ports = device.serial_ports()?;

    // Bitaxe Gamma requires exactly 2 serial ports
    if serial_ports.len() != 2 {
        return Err(crate::error::Error::Hardware(format!(
            "Bitaxe Gamma requires exactly 2 serial ports, found {}",
            serial_ports.len()
        )));
    }

    debug!(
        serial = ?device.serial_number,
        control = %serial_ports[0],
        data = %serial_ports[1],
        "Opening Bitaxe Gamma serial ports"
    );

    // Open control port at 115200 baud
    let control_port = tokio_serial::new(&serial_ports[0], 115200).open_native_async()?;

    // Create watch channel for board state, seeded with identity
    let serial = device.serial_number.clone();
    let initial_state = BoardState {
        name: format!("bitaxe-{}", serial.as_deref().unwrap_or("unknown")),
        model: "Bitaxe Gamma".into(),
        serial,
        ..Default::default()
    };
    let (state_tx, state_rx) = watch::channel(initial_state);

    // Create command channel for API-to-board control
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<crate::api::commands::BoardCommand>(16);

    // Create the board with the control port and data port path
    let mut board = BitaxeBoard::new(
        control_port,
        &serial_ports[1],
        device.serial_number.clone(),
        state_tx,
        cmd_rx,
    )
    .map_err(|e| crate::error::Error::Hardware(format!("Failed to create board: {}", e)))?;

    // Initialize the board (reset, discover chips, start event monitoring)
    board
        .initialize()
        .await
        .map_err(|e| crate::error::Error::Hardware(format!("Failed to initialize board: {}", e)))?;

    debug!(
        "Bitaxe board initialized successfully with {} chips",
        board.chip_count()
    );

    let registration = super::BoardRegistration {
        state_rx,
        cmd_tx: Some(cmd_tx),
    };
    Ok((Box::new(board), registration))
}

// Register this board type with the inventory system
inventory::submit! {
    crate::board::BoardDescriptor {
        pattern: crate::board::pattern::BoardPattern {
            vid: Match::Any,
            pid: Match::Any,
            manufacturer: Match::Specific(StringMatch::Exact("OSMU")),
            product: Match::Specific(StringMatch::Exact("Bitaxe")),
            serial_pattern: Match::Any,
        },
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
            let first_freq =
                25.0 * first.fb_div as f32 / (first.ref_div * post_div1 * post_div2) as f32;
            assert!(
                (first_freq - 56.25).abs() < 1.0,
                "First frequency should be ~56.25 MHz"
            );
        }

        if let Some(last) = steps.last() {
            let post_div1 = ((last.post_div >> 4) & 0xF) + 1;
            let post_div2 = (last.post_div & 0xF) + 1;
            let last_freq =
                25.0 * last.fb_div as f32 / (last.ref_div * post_div1 * post_div2) as f32;
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
