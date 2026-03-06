use std::env;
use std::fs;
use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use super::{Board, BoardError, BoardInfo, VirtualBoardDescriptor};
use crate::{
    api_client::types::{BoardState, Fan, PowerMeasurement, TemperatureSensor, ThreadState},
    asic::{
        bzm2::{Bzm2Thread, Bzm2ThreadConfig, Bzm2ThreadHandle},
        hash_thread::{
            HashTask, HashThread, HashThreadCapabilities, HashThreadError, HashThreadEvent,
            HashThreadStatus,
        },
    },
    tracing::prelude::*,
    transport::{SerialControl, SerialStream},
};

const DEFAULT_BAUD_RATE: u32 = 5_000_000;
const DEFAULT_DISPATCH_INTERVAL_MS: u64 = 500;
const DEFAULT_NOMINAL_HASHRATE_THS: f64 = 40.0;
const DEFAULT_TELEMETRY_INTERVAL_SECS: u64 = 5;
const DEFAULT_ASIC_TEMP_SCALE: f32 = 0.001;
const DEFAULT_BOARD_TEMP_SCALE: f32 = 0.001;
const DEFAULT_FAN_RPM_SCALE: f32 = 1.0;
const DEFAULT_FAN_PERCENT_SCALE: f32 = 1.0;
const DEFAULT_VOLTAGE_SCALE: f32 = 0.001;
const DEFAULT_CURRENT_SCALE: f32 = 0.001;
const DEFAULT_POWER_SCALE: f32 = 0.000001;

#[derive(Debug, Clone)]
pub struct Bzm2VirtualDeviceConfig {
    pub serial_paths: Vec<String>,
    pub baud_rate: u32,
    pub timestamp_count: u8,
    pub nonce_gap: u32,
    pub dispatch_interval: Duration,
    pub nominal_hashrate_ths: f64,
    pub dts_vs_generation: crate::asic::bzm2::protocol::DtsVsGeneration,
    pub telemetry: Bzm2TelemetryConfig,
}

impl Bzm2VirtualDeviceConfig {
    pub fn from_env() -> Option<Self> {
        let raw_paths = env::var("MUJINA_BZM2_SERIAL")
            .ok()
            .or_else(|| env::var("MUJINA_BZM2_SERIAL_PATHS").ok())?;

        let serial_paths: Vec<String> = raw_paths
            .split(',')
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(ToOwned::to_owned)
            .collect();
        if serial_paths.is_empty() {
            return None;
        }

        let baud_rate = env::var("MUJINA_BZM2_BAUD")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(DEFAULT_BAUD_RATE);
        let timestamp_count = env::var("MUJINA_BZM2_TIMESTAMP_COUNT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(crate::asic::bzm2::protocol::DEFAULT_TIMESTAMP_COUNT);
        let nonce_gap = env::var("MUJINA_BZM2_NONCE_GAP")
            .ok()
            .and_then(|value| {
                let trimmed = value.trim();
                if let Some(hex) = trimmed.strip_prefix("0x") {
                    u32::from_str_radix(hex, 16).ok()
                } else {
                    trimmed.parse().ok()
                }
            })
            .unwrap_or(crate::asic::bzm2::protocol::DEFAULT_NONCE_GAP);
        let dispatch_interval = Duration::from_millis(
            env::var("MUJINA_BZM2_DISPATCH_MS")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(DEFAULT_DISPATCH_INTERVAL_MS),
        );
        let nominal_hashrate_ths = env::var("MUJINA_BZM2_HASHRATE_THS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(DEFAULT_NOMINAL_HASHRATE_THS);
        let dts_vs_generation = env::var("MUJINA_BZM2_DTS_VS_GEN")
            .ok()
            .as_deref()
            .and_then(crate::asic::bzm2::protocol::DtsVsGeneration::from_env_value)
            .unwrap_or(crate::asic::bzm2::protocol::DtsVsGeneration::Gen2);

        Some(Self {
            serial_paths,
            baud_rate,
            timestamp_count,
            nonce_gap,
            dispatch_interval,
            nominal_hashrate_ths,
            dts_vs_generation,
            telemetry: Bzm2TelemetryConfig::from_env(),
        })
    }

    pub fn device_id(&self) -> String {
        let suffix = self
            .serial_paths
            .iter()
            .map(|path| {
                Path::new(path)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(path)
                    .chars()
                    .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("-");
        format!("bzm2-{}", suffix)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Bzm2TelemetryConfig {
    pub poll_interval: Duration,
    pub asic_temp: Option<SensorSpec>,
    pub board_temp: Option<SensorSpec>,
    pub fan_rpm: Option<SensorSpec>,
    pub fan_percent: Option<SensorSpec>,
    pub input_voltage: Option<SensorSpec>,
    pub input_current: Option<SensorSpec>,
    pub input_power: Option<SensorSpec>,
    pub max_asic_temp_c: Option<f32>,
    pub max_board_temp_c: Option<f32>,
    pub max_input_power_w: Option<f32>,
}

impl Bzm2TelemetryConfig {
    fn from_env() -> Self {
        Self {
            poll_interval: Duration::from_secs(
                env::var("MUJINA_BZM2_TELEMETRY_INTERVAL_SECS")
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(DEFAULT_TELEMETRY_INTERVAL_SECS),
            ),
            asic_temp: SensorSpec::from_env(
                "MUJINA_BZM2_ASIC_TEMP_PATH",
                "MUJINA_BZM2_ASIC_TEMP_SCALE",
                DEFAULT_ASIC_TEMP_SCALE,
            ),
            board_temp: SensorSpec::from_env(
                "MUJINA_BZM2_BOARD_TEMP_PATH",
                "MUJINA_BZM2_BOARD_TEMP_SCALE",
                DEFAULT_BOARD_TEMP_SCALE,
            ),
            fan_rpm: SensorSpec::from_env(
                "MUJINA_BZM2_FAN_RPM_PATH",
                "MUJINA_BZM2_FAN_RPM_SCALE",
                DEFAULT_FAN_RPM_SCALE,
            ),
            fan_percent: SensorSpec::from_env(
                "MUJINA_BZM2_FAN_PERCENT_PATH",
                "MUJINA_BZM2_FAN_PERCENT_SCALE",
                DEFAULT_FAN_PERCENT_SCALE,
            ),
            input_voltage: SensorSpec::from_env(
                "MUJINA_BZM2_INPUT_VOLTAGE_PATH",
                "MUJINA_BZM2_INPUT_VOLTAGE_SCALE",
                DEFAULT_VOLTAGE_SCALE,
            ),
            input_current: SensorSpec::from_env(
                "MUJINA_BZM2_INPUT_CURRENT_PATH",
                "MUJINA_BZM2_INPUT_CURRENT_SCALE",
                DEFAULT_CURRENT_SCALE,
            ),
            input_power: SensorSpec::from_env(
                "MUJINA_BZM2_INPUT_POWER_PATH",
                "MUJINA_BZM2_INPUT_POWER_SCALE",
                DEFAULT_POWER_SCALE,
            ),
            max_asic_temp_c: env_f32("MUJINA_BZM2_MAX_ASIC_TEMP_C"),
            max_board_temp_c: env_f32("MUJINA_BZM2_MAX_BOARD_TEMP_C"),
            max_input_power_w: env_f32("MUJINA_BZM2_MAX_INPUT_POWER_W"),
        }
    }

    fn is_enabled(&self) -> bool {
        self.asic_temp.is_some()
            || self.board_temp.is_some()
            || self.fan_rpm.is_some()
            || self.fan_percent.is_some()
            || self.input_voltage.is_some()
            || self.input_current.is_some()
            || self.input_power.is_some()
            || self.max_asic_temp_c.is_some()
            || self.max_board_temp_c.is_some()
            || self.max_input_power_w.is_some()
    }

    fn snapshot(&self) -> Bzm2TelemetrySnapshot {
        let asic_temp = self.asic_temp.as_ref().and_then(SensorSpec::read);
        let board_temp = self.board_temp.as_ref().and_then(SensorSpec::read);
        let fan_rpm = self
            .fan_rpm
            .as_ref()
            .and_then(SensorSpec::read)
            .map(|v| v.round() as u32);
        let fan_percent = self
            .fan_percent
            .as_ref()
            .and_then(SensorSpec::read)
            .map(|v| v.round().clamp(0.0, 100.0) as u8);
        let voltage_v = self.input_voltage.as_ref().and_then(SensorSpec::read);
        let current_a = self.input_current.as_ref().and_then(SensorSpec::read);
        let power_w = self
            .input_power
            .as_ref()
            .and_then(SensorSpec::read)
            .or_else(|| match (voltage_v, current_a) {
                (Some(voltage_v), Some(current_a)) => Some(voltage_v * current_a),
                _ => None,
            });

        let fans = if fan_rpm.is_some() || fan_percent.is_some() {
            vec![Fan {
                name: "fan".into(),
                rpm: fan_rpm,
                percent: fan_percent,
                target_percent: None,
            }]
        } else {
            Vec::new()
        };

        let mut temperatures = Vec::new();
        if self.asic_temp.is_some() || asic_temp.is_some() {
            temperatures.push(TemperatureSensor {
                name: "asic".into(),
                temperature_c: asic_temp,
            });
        }
        if self.board_temp.is_some() || board_temp.is_some() {
            temperatures.push(TemperatureSensor {
                name: "board".into(),
                temperature_c: board_temp,
            });
        }

        let powers = if self.input_voltage.is_some()
            || self.input_current.is_some()
            || self.input_power.is_some()
            || power_w.is_some()
        {
            vec![PowerMeasurement {
                name: "input".into(),
                voltage_v,
                current_a,
                power_w,
            }]
        } else {
            Vec::new()
        };

        let trip_reason = self.trip_reason(asic_temp, board_temp, power_w);

        Bzm2TelemetrySnapshot {
            fans,
            temperatures,
            powers,
            trip_reason,
        }
    }

    fn trip_reason(
        &self,
        asic_temp: Option<f32>,
        board_temp: Option<f32>,
        input_power_w: Option<f32>,
    ) -> Option<String> {
        if let (Some(limit), Some(value)) = (self.max_asic_temp_c, asic_temp) {
            if value > limit {
                return Some(format!(
                    "ASIC temperature {:.1}C exceeded limit {:.1}C",
                    value, limit
                ));
            }
        }

        if let (Some(limit), Some(value)) = (self.max_board_temp_c, board_temp) {
            if value > limit {
                return Some(format!(
                    "Board temperature {:.1}C exceeded limit {:.1}C",
                    value, limit
                ));
            }
        }

        if let (Some(limit), Some(value)) = (self.max_input_power_w, input_power_w) {
            if value > limit {
                return Some(format!(
                    "Input power {:.1}W exceeded limit {:.1}W",
                    value, limit
                ));
            }
        }

        None
    }
}

#[derive(Debug, Clone)]
pub struct SensorSpec {
    pub path: String,
    pub scale: f32,
}

impl SensorSpec {
    fn from_env(path_var: &str, scale_var: &str, default_scale: f32) -> Option<Self> {
        let path = env::var(path_var).ok()?;
        let scale = env::var(scale_var)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(default_scale);
        Some(Self { path, scale })
    }

    fn read(&self) -> Option<f32> {
        let raw = fs::read_to_string(&self.path).ok()?;
        parse_scaled_sensor_value(&raw, self.scale)
    }
}

#[derive(Debug, Clone, Default)]
struct Bzm2TelemetrySnapshot {
    fans: Vec<Fan>,
    temperatures: Vec<TemperatureSensor>,
    powers: Vec<PowerMeasurement>,
    trip_reason: Option<String>,
}

pub struct Bzm2Board {
    config: Bzm2VirtualDeviceConfig,
    shutdown_handles: Vec<Bzm2ThreadHandle>,
    serial_controls: Vec<SerialControl>,
    state_tx: watch::Sender<BoardState>,
    monitor_shutdown: Option<watch::Sender<bool>>,
    monitor_task: Option<JoinHandle<()>>,
}

impl Bzm2Board {
    pub fn new(config: Bzm2VirtualDeviceConfig, state_tx: watch::Sender<BoardState>) -> Self {
        Self {
            config,
            shutdown_handles: Vec::new(),
            serial_controls: Vec::new(),
            state_tx,
            monitor_shutdown: None,
            monitor_task: None,
        }
    }

    fn spawn_monitor(&mut self) {
        if !self.config.telemetry.is_enabled() || self.monitor_task.is_some() {
            return;
        }

        let telemetry = self.config.telemetry.clone();
        let state_tx = self.state_tx.clone();
        let shutdown_handles = self.shutdown_handles.clone();
        let serial_controls = self.serial_controls.clone();
        let board_name = self.config.device_id();
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        self.monitor_shutdown = Some(shutdown_tx);

        self.monitor_task = Some(tokio::spawn(async move {
            let mut interval = tokio::time::interval(telemetry.poll_interval);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let snapshot = telemetry.snapshot();
                        let total_stats = serial_controls.iter().fold((0u64, 0u64), |acc, control| {
                            let stats = control.stats();
                            (acc.0 + stats.bytes_read, acc.1 + stats.bytes_written)
                        });

                        let _ = state_tx.send_modify(|state| {
                            state.fans = snapshot.fans.clone();
                            state.temperatures = snapshot.temperatures.clone();
                            state.powers = snapshot.powers.clone();
                        });

                        trace!(
                            board = %board_name,
                            bytes_read = total_stats.0,
                            bytes_written = total_stats.1,
                            "BZM2 board telemetry updated"
                        );

                        if let Some(reason) = snapshot.trip_reason.clone() {
                            warn!(board = %board_name, reason = %reason, "BZM2 safety trip triggered");
                            for handle in &shutdown_handles {
                                handle.shutdown();
                            }
                            let _ = state_tx.send_modify(|state| {
                                for thread in &mut state.threads {
                                    thread.is_active = false;
                                    thread.hashrate = 0;
                                }
                            });
                            break;
                        }
                    }
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                    }
                }
            }
        }));
    }
}

#[async_trait]
impl Board for Bzm2Board {
    fn board_info(&self) -> BoardInfo {
        BoardInfo {
            model: "BZM2".into(),
            firmware_version: None,
            serial_number: Some(self.config.device_id()),
        }
    }

    async fn shutdown(&mut self) -> Result<(), BoardError> {
        if let Some(tx) = self.monitor_shutdown.take() {
            let _ = tx.send(true);
        }
        if let Some(handle) = self.monitor_task.take() {
            let _ = handle.await;
        }

        for handle in &self.shutdown_handles {
            handle.shutdown();
        }
        self.shutdown_handles.clear();
        self.serial_controls.clear();
        let _ = self.state_tx.send_modify(|state| {
            for thread in &mut state.threads {
                thread.is_active = false;
                thread.hashrate = 0;
            }
        });
        Ok(())
    }

    async fn create_hash_threads(&mut self) -> Result<Vec<Box<dyn HashThread>>, BoardError> {
        let mut threads: Vec<Box<dyn HashThread>> = Vec::new();
        let mut thread_states = Vec::new();

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
            thread_states.push(ThreadState {
                name: thread_name,
                hashrate: 0,
                is_active: false,
            });
            threads.push(Box::new(Bzm2ManagedThread::new(
                Box::new(thread),
                self.state_tx.clone(),
                index,
            )));
        }

        let _ = self.state_tx.send_modify(|state| {
            state.threads = thread_states.clone();
        });

        self.spawn_monitor();

        Ok(threads)
    }
}

struct Bzm2ManagedThread {
    inner: Box<dyn HashThread>,
    state_tx: watch::Sender<BoardState>,
    thread_index: usize,
}

impl Bzm2ManagedThread {
    fn new(
        inner: Box<dyn HashThread>,
        state_tx: watch::Sender<BoardState>,
        thread_index: usize,
    ) -> Self {
        Self {
            inner,
            state_tx,
            thread_index,
        }
    }

    fn publish_status(&self, status: &HashThreadStatus) {
        publish_thread_status(&self.state_tx, self.thread_index, status);
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

    async fn update_task(
        &mut self,
        new_task: HashTask,
    ) -> Result<Option<HashTask>, HashThreadError> {
        let result = self.inner.update_task(new_task).await;
        self.publish_status(&self.inner.status());
        result
    }

    async fn replace_task(
        &mut self,
        new_task: HashTask,
    ) -> Result<Option<HashTask>, HashThreadError> {
        let result = self.inner.replace_task(new_task).await;
        self.publish_status(&self.inner.status());
        result
    }

    async fn go_idle(&mut self) -> Result<Option<HashTask>, HashThreadError> {
        let result = self.inner.go_idle().await;
        self.publish_status(&self.inner.status());
        result
    }

    fn take_event_receiver(&mut self) -> Option<mpsc::Receiver<HashThreadEvent>> {
        let mut inner_rx = self.inner.take_event_receiver()?;
        let (event_tx, event_rx) = mpsc::channel(64);
        let state_tx = self.state_tx.clone();
        let thread_index = self.thread_index;

        tokio::spawn(async move {
            while let Some(event) = inner_rx.recv().await {
                if let HashThreadEvent::StatusUpdate(ref status) = event {
                    publish_thread_status(&state_tx, thread_index, status);
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

fn publish_thread_status(
    state_tx: &watch::Sender<BoardState>,
    thread_index: usize,
    status: &HashThreadStatus,
) {
    let _ = state_tx.send_modify(|state| {
        if let Some(thread) = state.threads.get_mut(thread_index) {
            thread.hashrate = status.hashrate.0;
            thread.is_active = status.is_active;
        }
    });
}

fn parse_scaled_sensor_value(raw: &str, scale: f32) -> Option<f32> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse::<f32>().ok().map(|value| value * scale)
}

fn env_f32(key: &str) -> Option<f32> {
    env::var(key).ok().and_then(|value| value.parse().ok())
}

async fn create_bzm2_board()
-> crate::error::Result<(Box<dyn Board + Send>, super::BoardRegistration)> {
    let config = Bzm2VirtualDeviceConfig::from_env().ok_or_else(|| {
        crate::error::Error::Config("BZM2 not configured (MUJINA_BZM2_SERIAL not set)".into())
    })?;

    let serial = config.device_id();
    let initial_state = BoardState {
        name: serial.clone(),
        model: "BZM2".into(),
        serial: Some(serial),
        ..Default::default()
    };
    let (state_tx, state_rx) = watch::channel(initial_state);

    let board = Bzm2Board::new(config, state_tx);
    let registration = super::BoardRegistration { state_rx };
    Ok((Box::new(board), registration))
}

inventory::submit! {
    VirtualBoardDescriptor {
        device_type: "bzm2",
        name: "BZM2",
        create_fn: || Box::pin(create_bzm2_board()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::pty::openpty;
    use std::fs;
    use std::os::fd::AsRawFd;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[tokio::test]
    async fn board_safety_trip_closes_scheduler_event_stream() {
        let pty = openpty(None, None).unwrap();
        let serial_path = fs::read_link(format!("/proc/self/fd/{}", pty.slave.as_raw_fd()))
            .unwrap()
            .to_string_lossy()
            .into_owned();

        let sensor_path = std::env::temp_dir().join(format!(
            "bzm2-trip-{}-{}.txt",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&sensor_path, "90\n").unwrap();

        let config = Bzm2VirtualDeviceConfig {
            serial_paths: vec![serial_path],
            baud_rate: DEFAULT_BAUD_RATE,
            timestamp_count: crate::asic::bzm2::protocol::DEFAULT_TIMESTAMP_COUNT,
            nonce_gap: crate::asic::bzm2::protocol::DEFAULT_NONCE_GAP,
            dispatch_interval: Duration::from_millis(50),
            nominal_hashrate_ths: DEFAULT_NOMINAL_HASHRATE_THS,
            dts_vs_generation: crate::asic::bzm2::protocol::DtsVsGeneration::Gen2,
            telemetry: Bzm2TelemetryConfig {
                poll_interval: Duration::from_millis(20),
                asic_temp: Some(SensorSpec {
                    path: sensor_path.to_string_lossy().into_owned(),
                    scale: 1.0,
                }),
                max_asic_temp_c: Some(80.0),
                ..Default::default()
            },
        };
        let (state_tx, mut state_rx) = watch::channel(BoardState {
            name: "bzm2-test".into(),
            model: "BZM2".into(),
            serial: Some("bzm2-test".into()),
            ..Default::default()
        });
        let mut board = Bzm2Board::new(config, state_tx);

        let mut threads = board.create_hash_threads().await.unwrap();
        let mut event_rx = threads[0].take_event_receiver().unwrap();

        let closed = tokio::time::timeout(Duration::from_secs(1), async {
            while event_rx.recv().await.is_some() {}
        })
        .await;
        assert!(
            closed.is_ok(),
            "event stream should close after safety trip"
        );

        let state = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let snapshot = state_rx.borrow().clone();
                if snapshot
                    .temperatures
                    .iter()
                    .any(|sensor| sensor.name == "asic" && sensor.temperature_c == Some(90.0))
                {
                    break snapshot;
                }
                state_rx.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert_eq!(state.threads[0].hashrate, 0);

        board.shutdown().await.unwrap();
        let _ = fs::remove_file(sensor_path);
        drop(pty);
    }
    #[test]
    fn parse_scaled_sensor_value_applies_scale() {
        let parsed = parse_scaled_sensor_value("42500\n", 0.001).unwrap();
        assert!((parsed - 42.5).abs() < 0.001);
        assert_eq!(parse_scaled_sensor_value("", 0.001), None);
        assert_eq!(parse_scaled_sensor_value("nope", 1.0), None);
    }

    #[test]
    fn telemetry_trip_detects_thresholds() {
        let telemetry = Bzm2TelemetryConfig {
            max_asic_temp_c: Some(80.0),
            max_input_power_w: Some(1200.0),
            ..Default::default()
        };

        assert!(
            telemetry
                .trip_reason(Some(81.0), None, None)
                .unwrap()
                .contains("ASIC temperature")
        );
        assert!(telemetry.trip_reason(None, None, Some(1250.0)).is_some());
        assert!(
            telemetry
                .trip_reason(Some(75.0), None, Some(1100.0))
                .is_none()
        );
    }

    #[test]
    fn publish_thread_status_updates_state_slot() {
        let (state_tx, state_rx) = watch::channel(BoardState {
            name: "bzm2-test".into(),
            model: "BZM2".into(),
            serial: Some("bzm2-test".into()),
            threads: vec![ThreadState {
                name: "BZM2 UART 0".into(),
                hashrate: 0,
                is_active: false,
            }],
            ..Default::default()
        });

        let status = HashThreadStatus {
            hashrate: crate::types::HashRate::from_terahashes(42.0),
            is_active: true,
            ..Default::default()
        };

        publish_thread_status(&state_tx, 0, &status);

        let state = state_rx.borrow().clone();
        assert_eq!(
            state.threads[0].hashrate,
            crate::types::HashRate::from_terahashes(42.0).0
        );
        assert!(state.threads[0].is_active);
    }
}
