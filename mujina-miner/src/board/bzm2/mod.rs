use std::sync::{Arc, Mutex};

use anyhow::Result as AnyhowResult;
use async_trait::async_trait;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use super::{BackplaneConnector, BoardInfo, VirtualBoardDescriptor};
use crate::api::commands::BoardCommand;
use crate::{
    api_client::types::{BoardTelemetry, ThreadTelemetry},
    asic::{
        bzm2::{Bzm2Thread, Bzm2ThreadConfig, Bzm2ThreadHandle},
        hash_thread::{
            HashTask, HashThread, HashThreadCapabilities, HashThreadEvent, HashThreadStatus,
        },
    },
    tracing::prelude::*,
    transport::{SerialControl, SerialStream},
};

mod bringup;
mod calibration;
mod commands;
mod config;
mod monitor;
mod telemetry;
#[cfg(all(test, unix))]
mod test_support;

use calibration::{Bzm2AppliedOperatingState, Bzm2BusLayout};
pub use config::Bzm2RuntimeConfig;
use monitor::Bzm2RuntimeMeasurementCache;
use telemetry::{
    merge_power_readings, merge_temperature_readings, publish_thread_status,
    publish_thread_telemetry,
};

// Register this board type with the inventory system
inventory::submit! {
    VirtualBoardDescriptor {
        device_type: "bzm2",
        name: "BZM2",
        create_fn: || Box::pin(create_bzm2_board()),
    }
}

async fn create_bzm2_board() -> AnyhowResult<BackplaneConnector> {
    let config = Bzm2RuntimeConfig::from_env()
        .ok_or_else(|| anyhow::anyhow!("BZM2 not configured (MUJINA_BZM2_SERIAL not set)"))?;

    let serial = config.device_id();
    let initial_state = BoardTelemetry {
        name: serial.clone(),
        model: "BZM2".into(),
        serial: Some(serial),
        ..Default::default()
    };
    let (telemetry_tx, telemetry_rx) = watch::channel(initial_state);
    let (command_tx, command_rx) = mpsc::channel(16);

    let mut board = Bzm2Board::new(config, telemetry_tx, command_rx);
    let info = board.board_info();

    // Bring-up, enumeration, calibration, and the monitor/command loops
    // all happen here; the returned threads are ready for the scheduler.
    let threads = board.create_hash_threads().await?;

    let shutdown = Box::pin(async move {
        if let Err(err) = board.shutdown().await {
            warn!(error = %err, "BZM2 board shutdown reported an error");
        }
    });

    Ok(BackplaneConnector {
        info,
        threads,
        telemetry_rx,
        command_tx: Some(command_tx),
        shutdown: Some(shutdown),
    })
}

/// Errors raised by BZM2 board bring-up and hardware control.
#[derive(Debug)]
pub enum BoardError {
    /// Board initialization failed (bring-up, serial open, calibration).
    InitializationFailed(String),
    /// Serial or file I/O failure while talking to the board.
    Communication(std::io::Error),
    /// A hardware control operation (rails, reset, clocks) failed.
    HardwareControl(String),
}

impl std::fmt::Display for BoardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BoardError::InitializationFailed(msg) => {
                write!(f, "board initialization failed: {msg}")
            }
            BoardError::Communication(err) => write!(f, "board communication error: {err}"),
            BoardError::HardwareControl(msg) => write!(f, "hardware control error: {msg}"),
        }
    }
}

impl std::error::Error for BoardError {}

impl From<std::io::Error> for BoardError {
    fn from(err: std::io::Error) -> Self {
        BoardError::Communication(err)
    }
}

pub struct Bzm2Board {
    config: Bzm2RuntimeConfig,
    bringup_applied: bool,
    shutdown_handles: Vec<Bzm2ThreadHandle>,
    serial_controls: Vec<SerialControl>,
    bus_layouts: Arc<Mutex<Vec<Bzm2BusLayout>>>,
    applied_operating_state: Arc<Mutex<Bzm2AppliedOperatingState>>,
    runtime_measurements: Arc<Mutex<Bzm2RuntimeMeasurementCache>>,
    telemetry_tx: watch::Sender<BoardTelemetry>,
    command_rx: Option<mpsc::Receiver<BoardCommand>>,
    monitor_shutdown: Option<watch::Sender<bool>>,
    monitor_task: Option<JoinHandle<()>>,
    command_shutdown: Option<watch::Sender<bool>>,
    command_task: Option<JoinHandle<()>>,
}

impl Bzm2Board {
    pub fn new(
        config: Bzm2RuntimeConfig,
        telemetry_tx: watch::Sender<BoardTelemetry>,
        command_rx: mpsc::Receiver<BoardCommand>,
    ) -> Self {
        Self {
            config,
            bringup_applied: false,
            shutdown_handles: Vec::new(),
            serial_controls: Vec::new(),
            bus_layouts: Arc::new(Mutex::new(Vec::new())),
            applied_operating_state: Arc::new(Mutex::new(Bzm2AppliedOperatingState::default())),
            runtime_measurements: Arc::new(Mutex::new(Bzm2RuntimeMeasurementCache::default())),
            telemetry_tx,
            command_rx: Some(command_rx),
            monitor_shutdown: None,
            monitor_task: None,
            command_shutdown: None,
            command_task: None,
        }
    }
}

impl Bzm2Board {
    fn board_info(&self) -> BoardInfo {
        BoardInfo {
            model: "BZM2".into(),
            firmware_version: None,
            serial_number: Some(self.config.device_id()),
        }
    }

    async fn shutdown(&mut self) -> AnyhowResult<()> {
        if let Some(tx) = self.monitor_shutdown.take() {
            let _ = tx.send(true);
        }
        if let Some(tx) = self.command_shutdown.take() {
            let _ = tx.send(true);
        }
        if let Some(handle) = self.monitor_task.take() {
            let _ = handle.await;
        }
        if let Some(handle) = self.command_task.take() {
            let _ = handle.await;
        }
        for handle in &self.shutdown_handles {
            handle.shutdown();
        }
        self.shutdown_handles.clear();
        self.serial_controls.clear();
        self.command_rx = None;
        self.telemetry_tx.send_modify(|state| {
            for thread in &mut state.threads {
                thread.is_active = false;
                thread.hashrate = 0;
            }
        });
        self.apply_shutdown_sequence().await?;
        Ok(())
    }

    async fn create_hash_threads(&mut self) -> AnyhowResult<Vec<Box<dyn HashThread>>> {
        let mut threads: Vec<Box<dyn HashThread>> = Vec::new();
        let mut thread_states = Vec::new();
        self.apply_bringup_sequence().await?;
        let bus_layouts = self.resolve_bus_layouts().await?;
        *self.bus_layouts.lock().unwrap_or_else(|e| e.into_inner()) = bus_layouts.clone();
        let initial_snapshot = self.config.telemetry.snapshot();
        let initial_rail_snapshot = self.config.bringup.snapshot_telemetry();
        self.telemetry_tx.send_modify(|state| {
            state.fans = initial_snapshot.fans.clone();
            merge_temperature_readings(&mut state.temperatures, &initial_snapshot.temperatures);
            merge_power_readings(&mut state.powers, &initial_snapshot.powers);
            merge_temperature_readings(
                &mut state.temperatures,
                &initial_rail_snapshot.temperatures,
            );
            merge_power_readings(&mut state.powers, &initial_rail_snapshot.powers);
        });

        self.execute_live_calibration(&bus_layouts).await?;
        let post_calibration_rail_snapshot = self.config.bringup.snapshot_telemetry();
        self.telemetry_tx.send_modify(|state| {
            merge_temperature_readings(
                &mut state.temperatures,
                &post_calibration_rail_snapshot.temperatures,
            );
            merge_power_readings(&mut state.powers, &post_calibration_rail_snapshot.powers);
        });

        for (index, serial_path) in self.config.serial_paths.iter().enumerate() {
            let stream = SerialStream::new(serial_path, self.config.baud_rate).map_err(|err| {
                BoardError::InitializationFailed(format!(
                    "Failed to open BZM2 serial transport {}: {}",
                    serial_path, err
                ))
            })?;
            let (reader, writer, control) = stream.split();
            let thread_name = format!("BZM2 UART {}", index);
            let mut config = Bzm2ThreadConfig::new(serial_path.clone(), self.config.baud_rate);
            config.timestamp_count = self.config.timestamp_count;
            config.nonce_gap = self.config.nonce_gap;
            config.dispatch_interval = self.config.dispatch_interval;
            config.nominal_hashrate_ths = self.config.nominal_hashrate_ths;
            config.dts_vs_generation = self.config.dts_vs_generation;

            self.serial_controls.push(control.clone());
            let thread = Bzm2Thread::new(thread_name.clone(), reader, writer, control, config);
            self.shutdown_handles.push(thread.shutdown_handle());
            thread_states.push(ThreadTelemetry {
                name: thread_name,
                hashrate: 0,
                is_active: false,
            });
            threads.push(Box::new(Bzm2ManagedThread::new(
                Box::new(thread),
                self.telemetry_tx.clone(),
                index,
            )));
        }

        self.telemetry_tx.send_modify(|state| {
            state.threads = thread_states.clone();
        });

        self.spawn_monitor();
        self.spawn_command_loop();
        Ok(threads)
    }
}

struct Bzm2ManagedThread {
    inner: Box<dyn HashThread>,
    telemetry_tx: watch::Sender<BoardTelemetry>,
    thread_index: usize,
}

impl Bzm2ManagedThread {
    fn new(
        inner: Box<dyn HashThread>,
        telemetry_tx: watch::Sender<BoardTelemetry>,
        thread_index: usize,
    ) -> Self {
        Self {
            inner,
            telemetry_tx,
            thread_index,
        }
    }

    fn publish_status(&self, status: &HashThreadStatus) {
        publish_thread_status(&self.telemetry_tx, self.thread_index, status);
    }
}

#[async_trait]
impl HashThread for Bzm2ManagedThread {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn capabilities(&self) -> &HashThreadCapabilities {
        self.inner.capabilities()
    }

    async fn configure(&mut self) -> AnyhowResult<()> {
        self.inner.configure().await
    }

    async fn update_task(&mut self, new_task: HashTask) -> AnyhowResult<Option<HashTask>> {
        let result = self.inner.update_task(new_task).await;
        self.publish_status(&self.inner.status());
        result
    }

    async fn replace_task(&mut self, new_task: HashTask) -> AnyhowResult<Option<HashTask>> {
        let result = self.inner.replace_task(new_task).await;
        self.publish_status(&self.inner.status());
        result
    }

    async fn go_idle(&mut self) -> AnyhowResult<Option<HashTask>> {
        let result = self.inner.go_idle().await;
        self.publish_status(&self.inner.status());
        result
    }

    fn take_event_receiver(&mut self) -> Option<mpsc::Receiver<HashThreadEvent>> {
        let mut inner_rx = self.inner.take_event_receiver()?;
        let (event_tx, event_rx) = mpsc::channel(64);
        let telemetry_tx = self.telemetry_tx.clone();
        let thread_index = self.thread_index;
        tokio::spawn(async move {
            while let Some(event) = inner_rx.recv().await {
                match &event {
                    HashThreadEvent::StatusUpdate(status) => {
                        publish_thread_status(&telemetry_tx, thread_index, status);
                    }
                    HashThreadEvent::TelemetryUpdate(update) => {
                        publish_thread_telemetry(&telemetry_tx, update);
                    }
                    _ => {}
                }
                if event_tx.send(event).await.is_err() {
                    break;
                }
            }
        });
        Some(event_rx)
    }

    fn status(&self) -> HashThreadStatus {
        self.inner.status()
    }
}
#[cfg(all(test, unix))]
mod tests {
    use super::bringup::Bzm2BringupConfig;
    use super::config::{
        Bzm2CalibrationConfig, Bzm2EnumerationConfig, DEFAULT_BAUD_RATE,
        DEFAULT_NOMINAL_HASHRATE_THS,
    };
    use super::telemetry::{Bzm2TelemetryConfig, SensorSpec};
    use super::*;
    use crate::board::power::{VoltageStackBringupPlan, VoltageStackStep};
    use crate::types::Temperature;
    use nix::pty::openpty;
    use std::fs;
    use std::os::fd::AsRawFd;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[tokio::test]
    async fn create_hash_threads_applies_bringup_and_shutdown_sequences() {
        let pty = openpty(None, None).unwrap();
        let serial_path = fs::read_link(format!("/proc/self/fd/{}", pty.slave.as_raw_fd()))
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let rail0_path = std::env::temp_dir().join(format!("bzm2-rail0-{unique}.txt"));
        let rail1_path = std::env::temp_dir().join(format!("bzm2-rail1-{unique}.txt"));
        let enable0_path = std::env::temp_dir().join(format!("bzm2-enable0-{unique}.txt"));
        let enable1_path = std::env::temp_dir().join(format!("bzm2-enable1-{unique}.txt"));
        let reset_path = std::env::temp_dir().join(format!("bzm2-reset-{unique}.txt"));

        let config = Bzm2RuntimeConfig {
            serial_paths: vec![serial_path],
            baud_rate: DEFAULT_BAUD_RATE,
            timestamp_count: crate::asic::bzm2::protocol::DEFAULT_TIMESTAMP_COUNT,
            nonce_gap: crate::asic::bzm2::protocol::DEFAULT_NONCE_GAP,
            dispatch_interval: Duration::from_millis(50),
            nominal_hashrate_ths: DEFAULT_NOMINAL_HASHRATE_THS,
            dts_vs_generation: crate::asic::bzm2::protocol::DtsVsGeneration::Gen2,
            telemetry: Bzm2TelemetryConfig::default(),
            enumeration: Bzm2EnumerationConfig::default(),
            bringup: Bzm2BringupConfig {
                enabled: true,
                rail_set_paths: vec![
                    rail0_path.to_string_lossy().into_owned(),
                    rail1_path.to_string_lossy().into_owned(),
                ],
                rail_write_scales: vec![1000.0, 1000.0],
                domain_rail_indices: Vec::new(),
                rail_enable_paths: vec![
                    enable0_path.to_string_lossy().into_owned(),
                    enable1_path.to_string_lossy().into_owned(),
                ],
                rail_enable_values: vec!["EN".into(), "ON".into()],
                rail_vin: Vec::new(),
                rail_vout: Vec::new(),
                rail_current: Vec::new(),
                rail_power: Vec::new(),
                rail_temperature: Vec::new(),
                reset_path: Some(reset_path.to_string_lossy().into_owned()),
                reset_active_low: true,
                plan: VoltageStackBringupPlan {
                    pre_power_delay: Duration::ZERO,
                    post_power_delay: Duration::ZERO,
                    release_reset_delay: Duration::ZERO,
                    steps: vec![
                        VoltageStackStep {
                            rail_index: 0,
                            voltage: 1.1,
                            settle_for: Duration::ZERO,
                        },
                        VoltageStackStep {
                            rail_index: 1,
                            voltage: 1.25,
                            settle_for: Duration::ZERO,
                        },
                    ],
                    ..Default::default()
                },
            },
            calibration: Bzm2CalibrationConfig::default(),
        };
        let (telemetry_tx, _telemetry_rx) = watch::channel(BoardTelemetry {
            name: "bzm2-test".into(),
            model: "BZM2".into(),
            serial: Some("bzm2-test".into()),
            ..Default::default()
        });
        let mut board = Bzm2Board::new(config, telemetry_tx, mpsc::channel(1).1);

        let _threads = board.create_hash_threads().await.unwrap();

        assert_eq!(fs::read_to_string(&rail0_path).unwrap(), "1100");
        assert_eq!(fs::read_to_string(&rail1_path).unwrap(), "1250");
        assert_eq!(fs::read_to_string(&enable0_path).unwrap(), "EN");
        assert_eq!(fs::read_to_string(&enable1_path).unwrap(), "ON");
        assert_eq!(fs::read_to_string(&reset_path).unwrap(), "1");

        board.shutdown().await.unwrap();

        assert_eq!(fs::read_to_string(&rail0_path).unwrap(), "0");
        assert_eq!(fs::read_to_string(&rail1_path).unwrap(), "0");
        assert_eq!(fs::read_to_string(&reset_path).unwrap(), "0");

        let _ = fs::remove_file(rail0_path);
        let _ = fs::remove_file(rail1_path);
        let _ = fs::remove_file(enable0_path);
        let _ = fs::remove_file(enable1_path);
        let _ = fs::remove_file(reset_path);
        drop(pty);
    }

    #[tokio::test]
    async fn create_hash_threads_publishes_rail_telemetry() {
        let pty = openpty(None, None).unwrap();
        let serial_path = fs::read_link(format!("/proc/self/fd/{}", pty.slave.as_raw_fd()))
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let vin_path = std::env::temp_dir().join(format!("bzm2-vin-{unique}.txt"));
        let vout_path = std::env::temp_dir().join(format!("bzm2-vout-{unique}.txt"));
        let current_path = std::env::temp_dir().join(format!("bzm2-current-{unique}.txt"));
        let power_path = std::env::temp_dir().join(format!("bzm2-power-{unique}.txt"));
        let temp_path = std::env::temp_dir().join(format!("bzm2-temp-{unique}.txt"));
        fs::write(&vin_path, "12000\n").unwrap();
        fs::write(&vout_path, "850\n").unwrap();
        fs::write(&current_path, "1500\n").unwrap();
        fs::write(&power_path, "1275\n").unwrap();
        fs::write(&temp_path, "47000\n").unwrap();

        let config = Bzm2RuntimeConfig {
            serial_paths: vec![serial_path],
            baud_rate: DEFAULT_BAUD_RATE,
            timestamp_count: crate::asic::bzm2::protocol::DEFAULT_TIMESTAMP_COUNT,
            nonce_gap: crate::asic::bzm2::protocol::DEFAULT_NONCE_GAP,
            dispatch_interval: Duration::from_millis(50),
            nominal_hashrate_ths: DEFAULT_NOMINAL_HASHRATE_THS,
            dts_vs_generation: crate::asic::bzm2::protocol::DtsVsGeneration::Gen2,
            telemetry: Bzm2TelemetryConfig::default(),
            enumeration: Bzm2EnumerationConfig::default(),
            bringup: Bzm2BringupConfig {
                rail_vin: vec![SensorSpec {
                    path: vin_path.to_string_lossy().into_owned(),
                    scale: 0.001,
                }],
                rail_vout: vec![SensorSpec {
                    path: vout_path.to_string_lossy().into_owned(),
                    scale: 0.001,
                }],
                rail_current: vec![SensorSpec {
                    path: current_path.to_string_lossy().into_owned(),
                    scale: 0.001,
                }],
                rail_power: vec![SensorSpec {
                    path: power_path.to_string_lossy().into_owned(),
                    scale: 0.001,
                }],
                rail_temperature: vec![SensorSpec {
                    path: temp_path.to_string_lossy().into_owned(),
                    scale: 0.001,
                }],
                ..Default::default()
            },
            calibration: Bzm2CalibrationConfig::default(),
        };
        let (telemetry_tx, telemetry_rx) = watch::channel(BoardTelemetry {
            name: "bzm2-test".into(),
            model: "BZM2".into(),
            serial: Some("bzm2-test".into()),
            ..Default::default()
        });
        let mut board = Bzm2Board::new(config, telemetry_tx, mpsc::channel(1).1);

        let _threads = board.create_hash_threads().await.unwrap();
        let state = telemetry_rx.borrow().clone();
        assert!(state.temperatures.iter().any(|sensor| {
            sensor.name == "rail0-regulator"
                && sensor
                    .temperature
                    .map(Temperature::as_degrees_c)
                    .is_some_and(|value| (value - 47.0).abs() < 0.001)
        }));
        assert!(state.powers.iter().any(|power| {
            power.name == "rail0-input"
                && power
                    .voltage_v
                    .is_some_and(|value| (value - 12.0).abs() < 0.001)
        }));
        assert!(state.powers.iter().any(|power| {
            power.name == "rail0-output"
                && power
                    .voltage_v
                    .is_some_and(|value| (value - 0.85).abs() < 0.001)
                && power
                    .current_a
                    .is_some_and(|value| (value - 1.5).abs() < 0.001)
                && power
                    .power_w
                    .is_some_and(|value| (value - 1.275).abs() < 0.001)
        }));

        board.shutdown().await.unwrap();

        let _ = fs::remove_file(vin_path);
        let _ = fs::remove_file(vout_path);
        let _ = fs::remove_file(current_path);
        let _ = fs::remove_file(power_path);
        let _ = fs::remove_file(temp_path);
        drop(pty);
    }
}
