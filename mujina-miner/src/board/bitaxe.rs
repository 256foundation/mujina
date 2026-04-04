use anyhow::{Context as _, Result, anyhow, bail};
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
use tokio_serial::SerialPortBuilderExt;
use tokio_stream::StreamExt;
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::{
    api_client::types::{BoardTelemetry, Fan, PowerMeasurement, TemperatureSensor},
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

    let fan_controller = init_fan_controller(i2c.clone()).await?;
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
        nrst_pin: reset_pin.clone(),
    };
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
    let initial_state = BoardTelemetry {
        name: format!("bitaxe-{}", serial.as_deref().unwrap_or("unknown")),
        model: "Bitaxe Gamma".into(),
        serial,
        ..Default::default()
    };
    let (telemetry_tx, telemetry_rx) = watch::channel(initial_state);

    let info = BoardInfo {
        model: "Bitaxe Gamma".to_string(),
        firmware_version: Some("bitaxe-raw".to_string()),
        serial_number: device.serial_number.clone(),
    };

    // Assemble internal state for the monitor and shutdown
    let mut bitaxe = Bitaxe {
        asic_nrst: reset_pin,
        i2c,
        fan_controller,
        regulator,
        thread_shutdown: thread_shutdown_tx,
        stats_task_handle: None,
        serial_number: device.serial_number,
        telemetry_tx: Some(telemetry_tx),
    };

    bitaxe.spawn_stats_monitor();

    let shutdown = Box::pin(async move {
        bitaxe.shutdown().await;
    });

    Ok(BackplaneConnector {
        info,
        threads,
        telemetry_rx,
        shutdown: Some(shutdown),
    })
}

/// Internal state for the board monitor and shutdown sequence.
///
/// The factory moves this into a spawned task. It holds only what
/// the stats monitor and graceful shutdown need.
struct Bitaxe {
    asic_nrst: BitaxeRawGpioPin,
    i2c: BitaxeRawI2c,
    fan_controller: Option<Emc2101<BitaxeRawI2c>>,
    regulator: Arc<Mutex<Tps546<BitaxeRawI2c>>>,
    thread_shutdown: watch::Sender<ThreadRemovalSignal>,
    stats_task_handle: Option<tokio::task::JoinHandle<()>>,
    serial_number: Option<String>,
    /// Taken by `spawn_stats_monitor` which publishes periodic snapshots.
    telemetry_tx: Option<watch::Sender<BoardTelemetry>>,
}

impl Bitaxe {
    fn board_info(&self) -> BoardInfo {
        BoardInfo {
            model: "Bitaxe Gamma".to_string(),
            firmware_version: Some("bitaxe-raw".to_string()),
            serial_number: self.serial_number.clone(),
        }
    }

    async fn shutdown(&mut self) {
        // Signal hash threads to shut down gracefully
        if let Err(e) = self.thread_shutdown.send(ThreadRemovalSignal::Shutdown) {
            warn!("Failed to send shutdown signal to threads: {}", e);
        } else {
            time::sleep(Duration::from_millis(200)).await;
        }

        // Hold chips in reset
        if let Err(e) = self.asic_nrst.write(PinValue::Low).await {
            warn!("Failed to hold chips in reset: {}", e);
        }

        // Turn off core voltage
        match self.regulator.lock().await.set_vout(0.0).await {
            Ok(()) => debug!("Core voltage turned off"),
            Err(e) => warn!("Failed to turn off core voltage: {}", e),
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
    }

    /// Spawn a task to periodically log and publish board telemetry.
    fn spawn_stats_monitor(&mut self) {
        let i2c = self.i2c.clone();
        let regulator = self.regulator.clone();

        let board_info = self.board_info();
        let board_name = format!(
            "bitaxe-{}",
            board_info.serial_number.as_deref().unwrap_or("unknown")
        );
        let board_model = board_info.model.clone();
        let board_serial = board_info.serial_number.clone();

        let telemetry_tx = self
            .telemetry_tx
            .take()
            .expect("telemetry_tx must be present when spawning stats monitor");

        let handle = tokio::spawn(async move {
            const STATS_INTERVAL: Duration = Duration::from_secs(5);
            let mut interval = time::interval(STATS_INTERVAL);
            interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

            let mut fan_ctrl = Emc2101::new(i2c);

            const LOG_INTERVAL: Duration = Duration::from_secs(30);
            let mut last_log = time::Instant::now();

            // Discard first tick (fires immediately, ADC readings may not be settled)
            interval.tick().await;

            loop {
                interval.tick().await;

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

                let _ = telemetry_tx.send(BoardTelemetry {
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
                    threads: Vec::new(),
                });

                if last_log.elapsed() >= LOG_INTERVAL {
                    last_log = time::Instant::now();
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
        });

        self.stats_task_handle = Some(handle);
    }
}

async fn init_fan_controller(i2c: BitaxeRawI2c) -> Result<Option<Emc2101<BitaxeRawI2c>>> {
    let mut fan = Emc2101::new(i2c);

    match fan.init().await {
        Ok(()) => {
            match fan.set_fan_speed(Percent::FULL).await {
                Ok(()) => debug!("Fan speed set to 100%"),
                Err(e) => warn!("Failed to set fan speed: {}", e),
            }
            Ok(Some(fan))
        }
        Err(e) => {
            warn!("Failed to initialize EMC2101 fan controller: {}", e);
            Ok(None)
        }
    }
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
    let deadline = time::Instant::now() + timeout;

    while time::Instant::now() < deadline {
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

/// Adapter implementing `AsicEnable` for Bitaxe's GPIO-based reset control.
struct BitaxeAsicEnable {
    nrst_pin: BitaxeRawGpioPin,
}

#[async_trait]
impl crate::asic::hash_thread::AsicEnable for BitaxeAsicEnable {
    async fn enable(&mut self) -> Result<()> {
        self.nrst_pin
            .write(PinValue::High)
            .await
            .map_err(|e| anyhow!("failed to release reset: {}", e))
    }

    async fn disable(&mut self) -> Result<()> {
        self.nrst_pin
            .write(PinValue::Low)
            .await
            .map_err(|e| anyhow!("failed to assert reset: {}", e))
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
