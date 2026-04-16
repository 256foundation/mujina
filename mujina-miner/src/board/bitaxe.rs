use anyhow::{Context as _, Result, anyhow, bail};
use async_trait::async_trait;
use futures::sink::SinkExt;
use std::{
    pin::Pin,
    sync::{Arc, Mutex as StdMutex},
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, ReadBuf},
    sync::{Mutex, watch},
    time::{self, Instant, MissedTickBehavior},
};
use tokio_serial::SerialPortBuilderExt;
use tokio_stream::StreamExt;
use tokio_util::{
    codec::{FramedRead, FramedWrite},
    sync::CancellationToken,
};

use crate::{
    api_client::types::{BoardTelemetry, Fan, PowerMeasurement, TemperatureSensor},
    asic::{
        ChipInfo,
        bm13xx::{self, BM13xxProtocol, protocol::Command, thread::BM13xxThread},
        hash_thread::{AsicEnable, BoardPeripherals, HashThread, ThreadRemovalSignal},
    },
    hw_trait::{
        gpio::{Gpio, GpioPin, PinValue},
        i2c::I2c,
    },
    mgmt_protocol::{
        ControlChannel,
        bitaxe_raw::{
            ResponseFormat,
            gpio::{BitaxeRawGpioController, BitaxeRawGpioPin},
            i2c::BitaxeRawI2c,
        },
    },
    peripheral::{
        emc2101::{Emc2101, Percent},
        tps546::{Tps546, Tps546Config},
    },
    tracing::prelude::*,
    transport::{
        UsbDeviceInfo,
        serial::{SerialReader, SerialStream, SerialWriter},
    },
    types::Temperature,
};

use super::{
    BackplaneConnector, BoardInfo,
    pattern::{Match, StringMatch},
};

// Register this board type with the inventory system
inventory::submit! {
    crate::board::BoardDescriptor {
        pattern: crate::board::pattern::BoardPattern {
            vid: Match::Any,
            pid: Match::Any,
            bcd_device: Match::Any,
            manufacturer: Match::Specific(StringMatch::Exact("OSMU")),
            product: Match::Specific(StringMatch::Exact("Bitaxe")),
            serial_pattern: Match::Any,
        },
        name: "Bitaxe Gamma",
        create_fn: |device| Box::pin(create_from_usb(device)),
    }
}

/// Create a Bitaxe board from USB device info.
async fn create_from_usb(device: UsbDeviceInfo) -> Result<BackplaneConnector> {
    let serial_ports = device.get_serial_ports(2).await?;

    debug!(
        serial = ?device.serial_number,
        control = %serial_ports[0],
        data = %serial_ports[1],
        "Opening Bitaxe Gamma serial ports"
    );

    // Open control port, create management channel and I2C bus
    let control_port = tokio_serial::new(&serial_ports[0], 115200).open_native_async()?;
    let control_channel = ControlChannel::new(control_port, ResponseFormat::V0);
    let mut i2c = BitaxeRawI2c::new(control_channel.clone());

    // Open data port for chip communication
    let data_stream =
        SerialStream::new(&serial_ports[1], 115200).context("failed to open data port")?;
    let (data_reader, data_writer, _data_control) = data_stream.split();
    let tracing_reader = TracingReader::new(data_reader, "Data");
    let mut data_reader = FramedRead::new(tracing_reader, bm13xx::FrameCodec);
    let mut data_writer = FramedWrite::new(data_writer, bm13xx::FrameCodec);

    // Get reset pin
    const ASIC_RESET_PIN: u8 = 0;
    let mut gpio_controller = BitaxeRawGpioController::new(control_channel);
    let mut reset_pin = gpio_controller.pin(ASIC_RESET_PIN).await?;

    // Hold ASIC in reset during power configuration
    reset_pin.write(PinValue::Low).await?;

    // Initialize peripherals
    i2c.set_frequency(100_000).await?;

    let emc2101 = init_fan_controller(i2c.clone()).await?;
    let regulator = Arc::new(Mutex::new(init_power_controller(i2c.clone()).await?));

    time::sleep(Duration::from_millis(500)).await;

    // Release ASIC from reset for discovery
    debug!("De-asserting ASIC nRST");
    reset_pin.write(PinValue::High).await?;

    time::sleep(Duration::from_millis(200)).await;

    // Version mask and chip discovery
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
        data_writer
            .send(version_cmd)
            .await
            .context("failed to send config command")?;
        time::sleep(Duration::from_millis(5)).await;
    }

    time::sleep(Duration::from_millis(10)).await;

    let chip_infos = discover_chips(&mut data_reader, &mut data_writer).await?;

    debug!(count = chip_infos.len(), "Discovered chips");

    // Verify expected BM1370 chip
    const EXPECTED_CHIP_ID: [u8; 2] = [0x13, 0x70];
    if let Some(first_chip) = chip_infos.first()
        && first_chip.chip_id != EXPECTED_CHIP_ID
    {
        bail!(
            "wrong chip type for Bitaxe Gamma: expected BM1370 ({:02x}{:02x}), found {:02x}{:02x}",
            EXPECTED_CHIP_ID[0],
            EXPECTED_CHIP_ID[1],
            first_chip.chip_id[0],
            first_chip.chip_id[1]
        );
    }

    // Put chip back in reset before handing off to hash thread
    reset_pin.write(PinValue::Low).await?;

    // Create hash thread
    let (thread_shutdown_tx, thread_shutdown_rx) = watch::channel(ThreadRemovalSignal::Running);

    let thread_name = match &device.serial_number {
        Some(serial) => format!("Bitaxe-Gamma-{}", &serial[..8.min(serial.len())]),
        None => "Bitaxe-Gamma".to_string(),
    };

    let asic_enable = BitaxeAsicEnable {
        nrst_pin: reset_pin,
        enabled_since: Arc::new(StdMutex::new(None)),
    };
    let asic_enable_monitor = asic_enable.clone();
    let peripherals = BoardPeripherals {
        asic_enable: Some(Box::new(asic_enable)),
        voltage_regulator: None,
    };

    let thread = BM13xxThread::new(
        thread_name,
        data_reader,
        data_writer,
        peripherals,
        thread_shutdown_rx,
    );
    let threads: Vec<Box<dyn HashThread>> = vec![Box::new(thread)];

    debug!("Bitaxe board initialized with {} chips", chip_infos.len());

    // Telemetry channel seeded with board identity
    let serial = device.serial_number.clone();
    let board_name = format!("bitaxe-{}", serial.as_deref().unwrap_or("unknown"));
    let initial_state = BoardTelemetry {
        name: board_name.clone(),
        model: "Bitaxe Gamma".into(),
        serial: serial.clone(),
        ..Default::default()
    };
    let (telemetry_tx, telemetry_rx) = watch::channel(initial_state);

    let info = BoardInfo {
        model: "Bitaxe Gamma".to_string(),
        firmware_version: Some("bitaxe-raw".to_string()),
        serial_number: device.serial_number.clone(),
    };

    // Assemble internal state and spawn the board monitor
    let bitaxe = Bitaxe {
        emc2101,
        regulator,
        thread_shutdown: thread_shutdown_tx,
        board_name,
        board_model: "Bitaxe Gamma",
        board_serial: serial,
        bad_thermal_count: 0,
        asic_enable: asic_enable_monitor,
    };

    let cancel = CancellationToken::new();
    let monitor_handle = tokio::spawn(bitaxe.run_monitor(telemetry_tx, cancel.clone()));

    let shutdown = Box::pin(async move {
        cancel.cancel();
        let _ = monitor_handle.await;
    });

    Ok(BackplaneConnector {
        info,
        threads,
        telemetry_rx,
        shutdown: Some(shutdown),
    })
}

/// Internal state owned by the board monitor task.
///
/// The factory assembles this and moves it into `run_monitor()`.
struct Bitaxe {
    emc2101: Emc2101<BitaxeRawI2c>,
    regulator: Arc<Mutex<Tps546<BitaxeRawI2c>>>,
    thread_shutdown: watch::Sender<ThreadRemovalSignal>,
    board_name: String,
    board_model: &'static str,
    board_serial: Option<String>,
    /// Consecutive bad thermal readings (I2C error, out-of-range, or
    /// above emergency threshold). Triggers emergency shutdown.
    bad_thermal_count: u32,
    asic_enable: BitaxeAsicEnable,
}

impl Bitaxe {
    async fn run_monitor(
        mut self,
        telemetry_tx: watch::Sender<BoardTelemetry>,
        cancel: CancellationToken,
    ) {
        let mut tick = time::interval(Duration::from_secs(2));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut last_log = Instant::now();

        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if self.monitor_tick(&telemetry_tx, &mut last_log).await.is_err() {
                        self.shutdown().await;
                        return;
                    }
                }
                _ = cancel.cancelled() => {
                    self.shutdown().await;
                    if let Err(e) = self.emc2101.set_fan_speed(Percent::new_clamped(25)).await {
                        warn!("Failed to reduce fan speed: {}", e);
                    }
                    return;
                }
            }
        }
    }

    /// Run one monitoring cycle. Returns `Err` on thermal emergency.
    ///
    /// Reads all sensors, classifies the temperature reading, publishes
    /// telemetry, and logs a periodic summary.
    ///
    /// Temperature readings fall into four categories:
    /// - I2C errors or diode faults: no temperature available.
    /// - Out of plausible range (outside 0..120 C): implausible,
    ///   treated as a bad reading.
    /// - Above the emergency threshold: valid but dangerous.
    /// - Normal: valid and safe, resets the bad-reading counter.
    ///
    /// Any category except normal increments a consecutive
    /// bad-reading counter. After BAD_READING_LIMIT consecutive
    /// bad readings, the board shuts down.
    async fn monitor_tick(
        &mut self,
        tx: &watch::Sender<BoardTelemetry>,
        last_log: &mut Instant,
    ) -> Result<(), ()> {
        // Read all sensors in one pass
        let raw_temp = self.emc2101.get_external_temperature().await;
        let fan_percent = self.emc2101.get_fan_speed().await.ok().map(u8::from);
        let fan_rpm = self.emc2101.get_rpm().await.ok();

        let (vin_mv, vout_mv, iout_ma, power_mw, vr_temp) = {
            let mut reg = self.regulator.lock().await;

            if let Err(e) = reg.check_status().await {
                error!("Power controller fault: {}", e);
                if let Err(e) = reg.clear_faults().await {
                    error!("Failed to clear faults: {}", e);
                }
            }

            (
                reg.get_vin().await.ok(),
                reg.get_vout().await.ok(),
                reg.get_iout().await.ok(),
                reg.get_power().await.ok(),
                reg.get_temperature().await.ok(),
            )
        };

        const EXPECTED_MIN_C: f32 = 0.0;
        const EXPECTED_MAX_C: f32 = 120.0;
        const EMERGENCY_TEMP_C: f32 = 80.0;
        const BAD_READING_LIMIT: u32 = 3;
        // The EMC2101 measures temperature via a diode on the ASIC
        // die. When the ASIC comes out of reset, the resulting
        // electrical transient corrupts the first few ADC conversions.
        // Wait for the measurement to settle before trusting readings.
        const DIODE_SETTLE: Duration = Duration::from_millis(500);
        let diode_ready = self
            .asic_enable
            .enabled_since()
            .is_some_and(|since| since.elapsed() >= DIODE_SETTLE);
        let asic_temp = if diode_ready {
            match raw_temp {
                Ok(t) if !(EXPECTED_MIN_C..=EXPECTED_MAX_C).contains(&t) => {
                    self.bad_thermal_count += 1;
                    trace!(temp_c = t, "Discarding out-of-range temperature reading");
                    None
                }
                Ok(t) if t >= EMERGENCY_TEMP_C => {
                    self.bad_thermal_count += 1;
                    warn!(
                        temp_c = t,
                        consecutive = self.bad_thermal_count,
                        "Temperature above emergency threshold"
                    );
                    Some(t)
                }
                Ok(t) => {
                    self.bad_thermal_count = 0;
                    Some(t)
                }
                Err(e) => {
                    self.bad_thermal_count += 1;
                    warn!("Temperature read failed: {}", e);
                    None
                }
            }
        } else {
            self.bad_thermal_count = 0;
            None
        };

        // Without reliable temperature readings we cannot operate
        // safely. Shut down the board.
        if self.bad_thermal_count >= BAD_READING_LIMIT {
            error!(
                consecutive = self.bad_thermal_count,
                "THERMAL EMERGENCY: shutting down board"
            );
            if let Err(e) = self.emc2101.set_fan_speed(Percent::FULL).await {
                error!("Failed to set fan speed: {}", e);
            }
            return Err(());
        }

        // Publish telemetry
        let _ = tx.send(BoardTelemetry {
            name: self.board_name.clone(),
            model: self.board_model.into(),
            serial: self.board_serial.clone(),
            fans: vec![Fan {
                name: "fan".into(),
                rpm: fan_rpm,
                percent: fan_percent,
                target_percent: None,
            }],
            temperatures: vec![
                TemperatureSensor {
                    name: "asic".into(),
                    temperature: asic_temp.map(Temperature::from_celsius),
                },
                TemperatureSensor {
                    name: "vr".into(),
                    temperature: vr_temp.map(|t| Temperature::from_celsius(t as f32)),
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
            threads: Vec::new(), // TODO: populate from hash thread telemetry
        });

        // Periodic log
        const LOG_INTERVAL: Duration = Duration::from_secs(30);
        if last_log.elapsed() >= LOG_INTERVAL {
            *last_log = Instant::now();
            info!(
                board = %self.board_model,
                serial = ?self.board_serial,
                asic_temp_c = ?asic_temp,
                fan_percent = ?fan_percent,
                fan_rpm = ?fan_rpm,
                vr_temp_c = ?vr_temp,
                power_w = ?power_mw.map(|mw| mw as f32 / 1000.0),
                current_a = ?iout_ma.map(|ma| ma as f32 / 1000.0),
                vin_v = ?vin_mv.map(|mv| mv as f32 / 1000.0),
                vout_v = ?vout_mv.map(|mv| mv as f32 / 1000.0),
                "Board status"
            );
        }

        Ok(())
    }

    async fn shutdown(&mut self) {
        if let Err(e) = self.thread_shutdown.send(ThreadRemovalSignal::Shutdown) {
            warn!("Failed to send shutdown signal to threads: {}", e);
        } else {
            time::sleep(Duration::from_millis(200)).await;
        }

        if let Err(e) = self.asic_enable.disable().await {
            warn!("Failed to hold chips in reset: {}", e);
        }

        match self.regulator.lock().await.set_vout(0.0).await {
            Ok(()) => debug!("Core voltage turned off"),
            Err(e) => warn!("Failed to turn off core voltage: {}", e),
        }
    }
}

async fn init_fan_controller(i2c: BitaxeRawI2c) -> Result<Emc2101<BitaxeRawI2c>> {
    let mut fan = Emc2101::new(i2c);
    fan.init().await.context("EMC2101 init failed")?;
    fan.set_fan_speed(Percent::FULL)
        .await
        .context("failed to set initial fan speed")?;
    debug!("Fan speed set to 100%");
    Ok(fan)
}

async fn init_power_controller(i2c: BitaxeRawI2c) -> Result<Tps546<BitaxeRawI2c>> {
    let config = Tps546Config {
        phase: 0x00,
        frequency_switch_khz: 650,

        vin_on: 4.8,
        vin_off: 4.5,
        vin_uv_warn_limit: 0.0, // Disabled due to TI bug
        vin_ov_fault_limit: 6.5,
        vin_ov_fault_response: 0xB7,

        vout_scale_loop: 0.25,
        vout_min: 1.0,
        vout_max: 2.0,
        vout_command: 1.15,

        vout_ov_fault_limit: 1.25,
        vout_ov_warn_limit: 1.16,
        vout_margin_high: 1.10,
        vout_margin_low: 0.90,
        vout_uv_warn_limit: 0.90,
        vout_uv_fault_limit: 0.75,

        iout_oc_warn_limit: 25.0,
        iout_oc_fault_limit: 30.0,
        iout_oc_fault_response: 0xC0,

        ot_warn_limit: 105,
        ot_fault_limit: 145,
        ot_fault_response: 0xFF,

        ton_delay: 0,
        ton_rise: 3,
        ton_max_fault_limit: 0,
        ton_max_fault_response: 0x3B,
        toff_delay: 0,
        toff_fall: 0,

        pin_detect_override: 0xFFFF,
    };

    let mut tps546 = Tps546::new(i2c, config);

    tps546
        .init()
        .await
        .context("power controller init failed")?;

    time::sleep(Duration::from_millis(100)).await;

    const DEFAULT_VOUT: f32 = 1.15;
    tps546
        .set_vout(DEFAULT_VOUT)
        .await
        .context("failed to set core voltage")?;
    debug!("Core voltage set to {DEFAULT_VOUT}V");

    time::sleep(Duration::from_millis(500)).await;

    match tps546.get_vout().await {
        Ok(mv) => debug!("Core voltage readback: {:.3}V", mv as f32 / 1000.0),
        Err(e) => warn!("Failed to read core voltage: {}", e),
    }

    if let Err(e) = tps546.dump_configuration().await {
        warn!("Failed to dump TPS546 configuration: {}", e);
    }

    Ok(tps546)
}

async fn discover_chips(
    reader: &mut FramedRead<TracingReader<SerialReader>, bm13xx::FrameCodec>,
    writer: &mut FramedWrite<SerialWriter, bm13xx::FrameCodec>,
) -> Result<Vec<ChipInfo>> {
    let discover_cmd = BM13xxProtocol::discover_chips();

    writer
        .send(discover_cmd)
        .await
        .context("failed to send chip discovery command")?;

    let mut chip_infos = Vec::new();
    let timeout = Duration::from_millis(500);
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
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

                        chip_infos.push(ChipInfo {
                            chip_id,
                            core_count: core_count.into(),
                            address,
                            supports_version_rolling: true,
                        });
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
            _ = time::sleep_until(deadline) => {
                break;
            }
        }
    }

    if chip_infos.is_empty() {
        bail!("no chips discovered");
    }
    Ok(chip_infos)
}

/// GPIO-based ASIC reset control that records when the ASIC was
/// last enabled.
#[derive(Clone)]
struct BitaxeAsicEnable {
    nrst_pin: BitaxeRawGpioPin,
    enabled_since: Arc<StdMutex<Option<Instant>>>,
}

impl BitaxeAsicEnable {
    /// When the ASIC was last taken out of reset, or `None` if it
    /// is currently in reset. Safe to call from another task.
    fn enabled_since(&self) -> Option<Instant> {
        *self.enabled_since.lock().expect("enabled_since lock")
    }
}

#[async_trait]
impl AsicEnable for BitaxeAsicEnable {
    async fn enable(&mut self) -> Result<()> {
        self.nrst_pin
            .write(PinValue::High)
            .await
            .map_err(|e| anyhow!("failed to release reset: {}", e))?;
        *self.enabled_since.lock().expect("enabled_since lock") = Some(Instant::now());
        Ok(())
    }

    async fn disable(&mut self) -> Result<()> {
        self.nrst_pin
            .write(PinValue::Low)
            .await
            .map_err(|e| anyhow!("failed to assert reset: {}", e))?;
        *self.enabled_since.lock().expect("enabled_since lock") = None;
        Ok(())
    }
}

/// A wrapper around AsyncRead that traces raw bytes as they're read.
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

        let result = Pin::new(&mut self.inner).poll_read(cx, buf);

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
