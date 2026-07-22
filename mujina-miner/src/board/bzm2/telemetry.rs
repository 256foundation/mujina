//! Sensor polling and board telemetry publishing for the BZM2 board.

use std::env;
use std::fs;
use std::time::Duration;

use tokio::sync::watch;

use crate::api_client::types::{
    AsicState, BoardTelemetry, Bzm2ClockReportResponse, Bzm2DllClockStatus, Bzm2PllClockStatus,
    EngineCoordinate, Fan, PowerMeasurement, TemperatureSensor,
};
use crate::asic::bzm2::Bzm2DiscoveredEngineMap;
use crate::asic::hash_thread::{HashThreadStatus, HashThreadTelemetryUpdate};
use crate::tuning::blockscale::Bzm2SavedEngineTopology;
use crate::types::Temperature;

use super::config::{
    DEFAULT_ASIC_TEMP_SCALE, DEFAULT_BOARD_TEMP_SCALE, DEFAULT_CURRENT_SCALE,
    DEFAULT_FAN_PERCENT_SCALE, DEFAULT_FAN_RPM_SCALE, DEFAULT_POWER_SCALE,
    DEFAULT_TELEMETRY_INTERVAL_SECS, DEFAULT_VOLTAGE_SCALE, env_csv_strings_any, env_f32,
    parse_csv_numbers_any,
};

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
    pub(super) fn from_env() -> Self {
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

    pub(super) fn is_enabled(&self) -> bool {
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

    pub(super) fn snapshot(&self) -> Bzm2TelemetrySnapshot {
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
            .or_else(|| voltage_v.zip(current_a).map(|(v, c)| v * c));

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
                temperature: asic_temp.map(Temperature::from_celsius),
            });
        }
        if self.board_temp.is_some() || board_temp.is_some() {
            temperatures.push(TemperatureSensor {
                name: "board".into(),
                temperature: board_temp.map(Temperature::from_celsius),
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
        if let (Some(limit), Some(value)) = (self.max_asic_temp_c, asic_temp)
            && value > limit
        {
            return Some(format!(
                "ASIC temperature {:.1}C exceeded limit {:.1}C",
                value, limit
            ));
        }
        if let (Some(limit), Some(value)) = (self.max_board_temp_c, board_temp)
            && value > limit
        {
            return Some(format!(
                "Board temperature {:.1}C exceeded limit {:.1}C",
                value, limit
            ));
        }
        if let (Some(limit), Some(value)) = (self.max_input_power_w, input_power_w)
            && value > limit
        {
            return Some(format!(
                "Input power {:.1}W exceeded limit {:.1}W",
                value, limit
            ));
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

    pub(super) fn read(&self) -> Option<f32> {
        let raw = fs::read_to_string(&self.path).ok()?;
        parse_scaled_sensor_value(&raw, self.scale)
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct Bzm2TelemetrySnapshot {
    pub(super) fans: Vec<Fan>,
    pub(super) temperatures: Vec<TemperatureSensor>,
    pub(super) powers: Vec<PowerMeasurement>,
    pub(super) trip_reason: Option<String>,
}

pub(super) fn publish_thread_status(
    telemetry_tx: &watch::Sender<BoardTelemetry>,
    thread_index: usize,
    status: &HashThreadStatus,
) {
    telemetry_tx.send_modify(|state| {
        if let Some(thread) = state.threads.get_mut(thread_index) {
            thread.hashrate = status.hashrate.0;
            thread.is_active = status.is_active;
        }
    });
}

pub(super) fn publish_thread_telemetry(
    telemetry_tx: &watch::Sender<BoardTelemetry>,
    update: &HashThreadTelemetryUpdate,
) {
    telemetry_tx.send_modify(|state| {
        merge_temperature_readings(
            &mut state.temperatures,
            &update
                .temperatures
                .iter()
                .map(|reading| TemperatureSensor {
                    name: reading.name.clone(),
                    temperature: reading.temperature_c.map(Temperature::from_celsius),
                })
                .collect::<Vec<_>>(),
        );
        merge_power_readings(
            &mut state.powers,
            &update
                .powers
                .iter()
                .map(|reading| PowerMeasurement {
                    name: reading.name.clone(),
                    voltage_v: reading.voltage_v,
                    current_a: reading.current_a,
                    power_w: reading.power_w,
                })
                .collect::<Vec<_>>(),
        );
    });
}

pub(super) fn publish_discovered_engine_map(
    telemetry_tx: &watch::Sender<BoardTelemetry>,
    thread_index: usize,
    serial_path: &str,
    discovery: &Bzm2DiscoveredEngineMap,
) {
    upsert_asic_state(
        telemetry_tx,
        thread_index,
        serial_path,
        discovery.asic,
        discovery.present_count() as u16,
        discovery
            .missing
            .iter()
            .map(|engine| EngineCoordinate {
                row: engine.row,
                col: engine.col,
            })
            .collect(),
    );
}

pub(super) fn publish_saved_engine_topology(
    telemetry_tx: &watch::Sender<BoardTelemetry>,
    thread_index: usize,
    serial_path: &str,
    asic_id: u8,
    topology: &Bzm2SavedEngineTopology,
) {
    upsert_asic_state(
        telemetry_tx,
        thread_index,
        serial_path,
        asic_id,
        topology.active_engine_count,
        topology
            .missing_engines
            .iter()
            .map(|engine| EngineCoordinate {
                row: engine.row,
                col: engine.col,
            })
            .collect(),
    );
}

fn upsert_asic_state(
    telemetry_tx: &watch::Sender<BoardTelemetry>,
    thread_index: usize,
    serial_path: &str,
    asic_id: u8,
    active_engine_count: u16,
    missing_engines: Vec<EngineCoordinate>,
) {
    telemetry_tx.send_modify(|state| {
        if let Some(asic) = state
            .asics
            .iter_mut()
            .find(|asic| asic.thread_index == Some(thread_index) && asic.id == asic_id)
        {
            asic.serial_path = Some(serial_path.to_owned());
            asic.discovered_engine_count = Some(active_engine_count);
            asic.missing_engines = missing_engines.clone();
        } else {
            state.asics.push(AsicState {
                id: asic_id,
                thread_index: Some(thread_index),
                serial_path: Some(serial_path.to_owned()),
                discovered_engine_count: Some(active_engine_count),
                missing_engines: missing_engines.clone(),
            });
        }
        state
            .asics
            .sort_by_key(|asic| (asic.thread_index.unwrap_or(usize::MAX), asic.id));
    });
}

pub(super) fn merge_temperature_readings(
    existing: &mut Vec<TemperatureSensor>,
    updates: &[TemperatureSensor],
) {
    for update in updates {
        if let Some(sensor) = existing
            .iter_mut()
            .find(|sensor| sensor.name == update.name)
        {
            sensor.temperature = update.temperature;
        } else {
            existing.push(update.clone());
        }
    }
}

pub(super) fn merge_power_readings(
    existing: &mut Vec<PowerMeasurement>,
    updates: &[PowerMeasurement],
) {
    for update in updates {
        if let Some(sensor) = existing
            .iter_mut()
            .find(|sensor| sensor.name == update.name)
        {
            sensor.voltage_v = update.voltage_v;
            sensor.current_a = update.current_a;
            sensor.power_w = update.power_w;
        } else {
            existing.push(update.clone());
        }
    }
}

pub(super) fn map_clock_report(
    report: crate::asic::bzm2::Bzm2ClockDebugReport,
) -> Bzm2ClockReportResponse {
    Bzm2ClockReportResponse {
        asic: report.asic,
        pll0: Bzm2PllClockStatus {
            enable_register: report.pll0.enable_register,
            misc_register: report.pll0.misc_register,
            enabled: report.pll0.enabled,
            locked: report.pll0.locked,
        },
        pll1: Bzm2PllClockStatus {
            enable_register: report.pll1.enable_register,
            misc_register: report.pll1.misc_register,
            enabled: report.pll1.enabled,
            locked: report.pll1.locked,
        },
        dll0: Bzm2DllClockStatus {
            control2: report.dll0.control2,
            control5: report.dll0.control5,
            coarsecon: report.dll0.coarsecon,
            fincon: report.dll0.fincon,
            freeze_valid: report.dll0.freeze_valid,
            locked: report.dll0.locked,
            fincon_valid: report.dll0.fincon_valid,
        },
        dll1: Bzm2DllClockStatus {
            control2: report.dll1.control2,
            control5: report.dll1.control5,
            coarsecon: report.dll1.coarsecon,
            fincon: report.dll1.fincon,
            freeze_valid: report.dll1.freeze_valid,
            locked: report.dll1.locked,
            fincon_valid: report.dll1.fincon_valid,
        },
    }
}

pub(super) fn snapshot_temperature(snapshot: &Bzm2TelemetrySnapshot, name: &str) -> Option<f32> {
    snapshot
        .temperatures
        .iter()
        .find(|sensor| sensor.name == name)
        .and_then(|sensor| sensor.temperature.map(Temperature::as_degrees_c))
}

pub(super) fn snapshot_input_power(snapshot: &Bzm2TelemetrySnapshot) -> Option<f32> {
    snapshot
        .powers
        .iter()
        .find(|power| power.name == "input")
        .and_then(|power| power.power_w)
}

fn parse_scaled_sensor_value(raw: &str, scale: f32) -> Option<f32> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse::<f32>().ok().map(|value| value * scale)
}

pub(super) fn sensor_specs_from_env(
    paths_keys: &[&str],
    scales_keys: &[&str],
    default_scale: f32,
) -> Vec<SensorSpec> {
    let paths = env_csv_strings_any(paths_keys);
    let scales = parse_csv_numbers_any::<f32>(scales_keys).unwrap_or_default();
    paths
        .into_iter()
        .enumerate()
        .map(|(index, path)| SensorSpec {
            path,
            scale: *scales
                .get(index)
                .or_else(|| scales.last())
                .unwrap_or(&default_scale),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_client::types::ThreadTelemetry;

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
    fn publish_thread_telemetry_updates_board_state() {
        let (telemetry_tx, telemetry_rx) = watch::channel(BoardTelemetry {
            name: "bzm2-test".into(),
            model: "BZM2".into(),
            serial: Some("bzm2-test".into()),
            temperatures: vec![TemperatureSensor {
                name: "host-board-temp".into(),
                temperature: Some(Temperature::from_celsius(52.0)),
            }],
            powers: vec![PowerMeasurement {
                name: "host-input".into(),
                voltage_v: Some(12.0),
                current_a: Some(10.0),
                power_w: Some(120.0),
            }],
            ..Default::default()
        });

        publish_thread_telemetry(
            &telemetry_tx,
            &HashThreadTelemetryUpdate {
                temperatures: vec![crate::asic::hash_thread::HashThreadTemperatureReading {
                    name: "ttyUSB0-asic-2-dts".into(),
                    temperature_c: Some(64.5),
                }],
                powers: vec![crate::asic::hash_thread::HashThreadPowerReading {
                    name: "ttyUSB0-asic-2-vs-ch0".into(),
                    voltage_v: Some(0.78),
                    current_a: None,
                    power_w: None,
                }],
            },
        );

        let state = telemetry_rx.borrow().clone();
        assert_eq!(state.temperatures.len(), 2);
        assert!(
            state
                .temperatures
                .iter()
                .any(|sensor| sensor.name == "host-board-temp"
                    && sensor.temperature.map(Temperature::as_degrees_c) == Some(52.0))
        );
        assert!(
            state
                .temperatures
                .iter()
                .any(|sensor| sensor.name == "ttyUSB0-asic-2-dts"
                    && sensor.temperature.map(Temperature::as_degrees_c) == Some(64.5))
        );
        assert_eq!(state.powers.len(), 2);
        assert!(
            state
                .powers
                .iter()
                .any(|sensor| sensor.name == "host-input" && sensor.voltage_v == Some(12.0))
        );
        assert!(
            state
                .powers
                .iter()
                .any(|sensor| sensor.name == "ttyUSB0-asic-2-vs-ch0"
                    && sensor.voltage_v == Some(0.78))
        );
    }

    #[test]
    fn publish_thread_status_updates_state_slot() {
        let (telemetry_tx, telemetry_rx) = watch::channel(BoardTelemetry {
            name: "bzm2-test".into(),
            model: "BZM2".into(),
            serial: Some("bzm2-test".into()),
            threads: vec![ThreadTelemetry {
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

        publish_thread_status(&telemetry_tx, 0, &status);

        let state = telemetry_rx.borrow().clone();
        assert_eq!(
            state.threads[0].hashrate,
            crate::types::HashRate::from_terahashes(42.0).0
        );
        assert!(state.threads[0].is_active);
    }

    #[test]
    fn publish_discovered_engine_map_updates_board_state() {
        let (telemetry_tx, telemetry_rx) = watch::channel(BoardTelemetry {
            name: "bzm2-test".into(),
            model: "BZM2".into(),
            serial: Some("bzm2-test".into()),
            ..Default::default()
        });

        publish_discovered_engine_map(
            &telemetry_tx,
            1,
            "/dev/ttyUSB1",
            &Bzm2DiscoveredEngineMap {
                asic: 2,
                present: vec![
                    crate::asic::bzm2::Bzm2EngineCoordinate::new(0, 0),
                    crate::asic::bzm2::Bzm2EngineCoordinate::new(0, 1),
                ],
                missing: vec![
                    crate::asic::bzm2::Bzm2EngineCoordinate::new(3, 7),
                    crate::asic::bzm2::Bzm2EngineCoordinate::new(5, 11),
                ],
            },
        );

        let state = telemetry_rx.borrow().clone();
        assert_eq!(state.asics.len(), 1);
        assert_eq!(state.asics[0].id, 2);
        assert_eq!(state.asics[0].thread_index, Some(1));
        assert_eq!(state.asics[0].serial_path.as_deref(), Some("/dev/ttyUSB1"));
        assert_eq!(state.asics[0].discovered_engine_count, Some(2));
        assert_eq!(
            state.asics[0].missing_engines,
            vec![
                EngineCoordinate { row: 3, col: 7 },
                EngineCoordinate { row: 5, col: 11 },
            ]
        );
    }
}
