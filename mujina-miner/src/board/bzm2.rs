use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use super::{Board, BoardCommand, BoardError, BoardInfo, VirtualBoardDescriptor};
use crate::{
    api_client::types::{BoardState, Fan, PowerMeasurement, TemperatureSensor, ThreadState},
    asic::{
        bzm2::{
            Bzm2AsicMeasurement, Bzm2AsicTopology, Bzm2BoardCalibrationInput, Bzm2BringupPlan,
            Bzm2CalibrationConstraints, Bzm2CalibrationMode, Bzm2CalibrationPlanner,
            Bzm2ClockController, Bzm2DomainMeasurement, Bzm2OperatingClass, Bzm2PerformanceMode,
            Bzm2Pll, Bzm2SavedOperatingPoint, Bzm2Thread, Bzm2ThreadConfig, Bzm2ThreadHandle,
            Bzm2UartController, Bzm2VoltageDomain, FileGpioPin, FilePowerRail, GpioResetLine,
            VoltageStackStep, control::Bzm2PowerRail,
        },
        hash_thread::{
            HashTask, HashThread, HashThreadCapabilities, HashThreadError, HashThreadEvent,
            HashThreadStatus, HashThreadTelemetryUpdate,
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
const DEFAULT_CALIBRATION_SITE_TEMP_C: f32 = 20.0;
const DEFAULT_CALIBRATION_POST1_DIVIDER: u8 = 0;
const DEFAULT_CALIBRATION_LOCK_TIMEOUT_MS: u64 = 1_000;
const DEFAULT_CALIBRATION_LOCK_POLL_MS: u64 = 100;
const DEFAULT_ENUMERATION_MAX_ASICS_PER_BUS: u16 = 100;
const DEFAULT_BRINGUP_PRE_POWER_MS: u64 = 10;
const DEFAULT_BRINGUP_POST_POWER_MS: u64 = 25;
const DEFAULT_BRINGUP_RELEASE_RESET_MS: u64 = 25;

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
    pub calibration: Bzm2CalibrationConfig,
    pub enumeration: Bzm2EnumerationConfig,
    pub bringup: Bzm2BringupConfig,
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
            .and_then(|value| parse_u32(&value))
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
        let calibration = Bzm2CalibrationConfig::from_env(serial_paths.len());
        let bringup = Bzm2BringupConfig::from_env();

        Some(Self {
            serial_paths: serial_paths.clone(),
            baud_rate,
            timestamp_count,
            nonce_gap,
            dispatch_interval,
            nominal_hashrate_ths,
            dts_vs_generation,
            telemetry: Bzm2TelemetryConfig::from_env(),
            enumeration: Bzm2EnumerationConfig::from_env(serial_paths.len(), &calibration),
            bringup,
            calibration,
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

#[derive(Debug, Clone)]
pub struct Bzm2EnumerationConfig {
    pub enabled: bool,
    pub start_id: u8,
    pub max_asics_per_bus: Vec<u16>,
}

impl Default for Bzm2EnumerationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            start_id: 0,
            max_asics_per_bus: vec![DEFAULT_ENUMERATION_MAX_ASICS_PER_BUS],
        }
    }
}

impl Bzm2EnumerationConfig {
    fn from_env(serial_count: usize, calibration: &Bzm2CalibrationConfig) -> Self {
        let mut max_asics_per_bus = parse_csv_numbers::<u16>("MUJINA_BZM2_ENUM_MAX_ASICS_PER_BUS")
            .unwrap_or_else(|| {
                if calibration.asics_per_bus.iter().any(|count| *count > 1) {
                    calibration.asics_per_bus.clone()
                } else if serial_count == 0 {
                    Vec::new()
                } else {
                    vec![DEFAULT_ENUMERATION_MAX_ASICS_PER_BUS; serial_count]
                }
            });
        if max_asics_per_bus.is_empty() && serial_count > 0 {
            max_asics_per_bus = vec![DEFAULT_ENUMERATION_MAX_ASICS_PER_BUS; serial_count];
        }

        Self {
            enabled: env_flag_any(&["MUJINA_BZM2_ENUMERATE_CHAIN", "MUJINA_BZM2_AUTO_ENUMERATE"]),
            start_id: env::var("MUJINA_BZM2_ENUM_START_ID")
                .ok()
                .and_then(|value| value.parse::<u8>().ok())
                .unwrap_or(0),
            max_asics_per_bus,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Bzm2BringupConfig {
    pub enabled: bool,
    pub rail_set_paths: Vec<String>,
    pub rail_write_scales: Vec<f32>,
    pub domain_rail_indices: Vec<usize>,
    pub rail_enable_paths: Vec<String>,
    pub rail_enable_values: Vec<String>,
    pub rail_vin: Vec<SensorSpec>,
    pub rail_vout: Vec<SensorSpec>,
    pub rail_current: Vec<SensorSpec>,
    pub rail_power: Vec<SensorSpec>,
    pub rail_temperature: Vec<SensorSpec>,
    pub reset_path: Option<String>,
    pub reset_active_low: bool,
    pub plan: Bzm2BringupPlan,
}

impl Default for Bzm2BringupConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            rail_set_paths: Vec::new(),
            rail_write_scales: Vec::new(),
            domain_rail_indices: Vec::new(),
            rail_enable_paths: Vec::new(),
            rail_enable_values: Vec::new(),
            rail_vin: Vec::new(),
            rail_vout: Vec::new(),
            rail_current: Vec::new(),
            rail_power: Vec::new(),
            rail_temperature: Vec::new(),
            reset_path: None,
            reset_active_low: true,
            plan: Bzm2BringupPlan {
                pre_power_delay: Duration::from_millis(DEFAULT_BRINGUP_PRE_POWER_MS),
                post_power_delay: Duration::from_millis(DEFAULT_BRINGUP_POST_POWER_MS),
                release_reset_delay: Duration::from_millis(DEFAULT_BRINGUP_RELEASE_RESET_MS),
                ..Default::default()
            },
        }
    }
}

impl Bzm2BringupConfig {
    fn from_env() -> Self {
        let rail_set_paths = env_csv_strings_any(&[
            "MUJINA_BZM2_RAIL_SET_PATHS",
            "MUJINA_BZM2_BRINGUP_RAIL_SET_PATHS",
        ]);
        let rail_target_volts = parse_csv_numbers::<f32>("MUJINA_BZM2_RAIL_TARGET_VOLTS")
            .or_else(|| parse_csv_numbers::<f32>("MUJINA_BZM2_BRINGUP_RAIL_TARGET_VOLTS"))
            .unwrap_or_default();
        let rail_write_scales = parse_csv_numbers::<f32>("MUJINA_BZM2_RAIL_WRITE_SCALES")
            .or_else(|| parse_csv_numbers::<f32>("MUJINA_BZM2_BRINGUP_RAIL_WRITE_SCALES"))
            .unwrap_or_default();
        let domain_rail_indices =
            parse_csv_numbers_any::<usize>(&["MUJINA_BZM2_DOMAIN_RAIL_INDICES"])
                .unwrap_or_default();
        let rail_enable_paths = env_csv_strings_any(&[
            "MUJINA_BZM2_RAIL_ENABLE_PATHS",
            "MUJINA_BZM2_BRINGUP_RAIL_ENABLE_PATHS",
        ]);
        let rail_enable_values = env_csv_strings_any(&[
            "MUJINA_BZM2_RAIL_ENABLE_VALUES",
            "MUJINA_BZM2_BRINGUP_RAIL_ENABLE_VALUES",
        ]);
        let rail_vin = sensor_specs_from_env(
            &["MUJINA_BZM2_RAIL_VIN_PATHS"],
            &["MUJINA_BZM2_RAIL_VIN_SCALES"],
            DEFAULT_VOLTAGE_SCALE,
        );
        let rail_vout = sensor_specs_from_env(
            &["MUJINA_BZM2_RAIL_VOUT_PATHS"],
            &["MUJINA_BZM2_RAIL_VOUT_SCALES"],
            DEFAULT_VOLTAGE_SCALE,
        );
        let rail_current = sensor_specs_from_env(
            &["MUJINA_BZM2_RAIL_CURRENT_PATHS"],
            &["MUJINA_BZM2_RAIL_CURRENT_SCALES"],
            DEFAULT_CURRENT_SCALE,
        );
        let rail_power = sensor_specs_from_env(
            &["MUJINA_BZM2_RAIL_POWER_PATHS"],
            &["MUJINA_BZM2_RAIL_POWER_SCALES"],
            DEFAULT_POWER_SCALE,
        );
        let rail_temperature = sensor_specs_from_env(
            &["MUJINA_BZM2_RAIL_TEMP_PATHS"],
            &["MUJINA_BZM2_RAIL_TEMP_SCALES"],
            DEFAULT_BOARD_TEMP_SCALE,
        );
        let reset_path = env_var_any(&["MUJINA_BZM2_RESET_PATH", "MUJINA_BZM2_BRINGUP_RESET_PATH"]);
        let enabled = env_flag_any(&["MUJINA_BZM2_ENABLE_BRINGUP", "MUJINA_BZM2_BRINGUP_ENABLE"])
            || !rail_set_paths.is_empty()
            || reset_path.is_some();

        let mut plan = Bzm2BringupPlan {
            assert_reset_before_power: env_flag_default_any(
                &["MUJINA_BZM2_ASSERT_RESET_BEFORE_POWER"],
                true,
            ),
            pre_power_delay: Duration::from_millis(
                env::var("MUJINA_BZM2_BRINGUP_PRE_POWER_MS")
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(DEFAULT_BRINGUP_PRE_POWER_MS),
            ),
            post_power_delay: Duration::from_millis(
                env::var("MUJINA_BZM2_BRINGUP_POST_POWER_MS")
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(DEFAULT_BRINGUP_POST_POWER_MS),
            ),
            release_reset_delay: Duration::from_millis(
                env::var("MUJINA_BZM2_BRINGUP_RELEASE_RESET_MS")
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(DEFAULT_BRINGUP_RELEASE_RESET_MS),
            ),
            ..Default::default()
        };
        plan.steps = rail_set_paths
            .iter()
            .enumerate()
            .filter_map(|(index, _)| {
                rail_target_volts
                    .get(index)
                    .or_else(|| rail_target_volts.last())
                    .copied()
                    .map(|voltage| VoltageStackStep {
                        rail_index: index,
                        voltage,
                        settle_for: Duration::ZERO,
                    })
            })
            .collect();

        Self {
            enabled,
            rail_set_paths,
            rail_write_scales,
            domain_rail_indices,
            rail_enable_paths,
            rail_enable_values,
            rail_vin,
            rail_vout,
            rail_current,
            rail_power,
            rail_temperature,
            reset_path,
            reset_active_low: env_flag_default_any(&["MUJINA_BZM2_RESET_ACTIVE_LOW"], true),
            plan,
        }
    }

    fn build_rails(&self) -> Vec<FilePowerRail> {
        self.rail_set_paths
            .iter()
            .enumerate()
            .map(|(index, path)| {
                let write_scale = *self
                    .rail_write_scales
                    .get(index)
                    .or_else(|| self.rail_write_scales.last())
                    .unwrap_or(&1.0);
                let mut rail = FilePowerRail::new(path.clone(), write_scale);
                if let Some(enable_path) = self
                    .rail_enable_paths
                    .get(index)
                    .or_else(|| self.rail_enable_paths.last())
                {
                    let enable_value = self
                        .rail_enable_values
                        .get(index)
                        .or_else(|| self.rail_enable_values.last())
                        .cloned()
                        .unwrap_or_else(|| "1".into());
                    rail = rail.with_enable(enable_path.clone(), enable_value);
                }
                rail
            })
            .collect()
    }

    fn build_reset_line(&self) -> Option<GpioResetLine<FileGpioPin>> {
        self.reset_path.as_ref().map(|path| {
            GpioResetLine::new(
                FileGpioPin::new(path.clone(), "1", "0"),
                self.reset_active_low,
            )
        })
    }

    fn rail_index_for_domain(&self, domain_id: u16) -> Option<usize> {
        self.domain_rail_indices
            .get(domain_id as usize)
            .copied()
            .or_else(|| {
                let fallback = domain_id as usize;
                (fallback < self.rail_set_paths.len()).then_some(fallback)
            })
    }

    fn has_telemetry(&self) -> bool {
        !self.rail_vin.is_empty()
            || !self.rail_vout.is_empty()
            || !self.rail_current.is_empty()
            || !self.rail_power.is_empty()
            || !self.rail_temperature.is_empty()
    }

    fn snapshot_telemetry(&self) -> Bzm2TelemetrySnapshot {
        let rail_count = [
            self.rail_set_paths.len(),
            self.rail_vin.len(),
            self.rail_vout.len(),
            self.rail_current.len(),
            self.rail_power.len(),
            self.rail_temperature.len(),
        ]
        .into_iter()
        .max()
        .unwrap_or(0);

        let mut temperatures = Vec::new();
        let mut powers = Vec::new();
        for index in 0..rail_count {
            let vin = self.rail_vin.get(index).and_then(SensorSpec::read);
            let vout = self.rail_vout.get(index).and_then(SensorSpec::read);
            let current = self.rail_current.get(index).and_then(SensorSpec::read);
            let power = self
                .rail_power
                .get(index)
                .and_then(SensorSpec::read)
                .or_else(|| match (vout, current) {
                    (Some(voltage_v), Some(current_a)) => Some(voltage_v * current_a),
                    _ => None,
                });
            let temperature_c = self.rail_temperature.get(index).and_then(SensorSpec::read);

            if let Some(temperature_c) = temperature_c {
                temperatures.push(TemperatureSensor {
                    name: format!("rail{}-regulator", index),
                    temperature_c: Some(temperature_c),
                });
            }
            if vin.is_some() {
                powers.push(PowerMeasurement {
                    name: format!("rail{}-input", index),
                    voltage_v: vin,
                    current_a: None,
                    power_w: None,
                });
            }
            if vout.is_some() || current.is_some() || power.is_some() {
                powers.push(PowerMeasurement {
                    name: format!("rail{}-output", index),
                    voltage_v: vout,
                    current_a: current,
                    power_w: power,
                });
            }
        }

        Bzm2TelemetrySnapshot {
            fans: Vec::new(),
            temperatures,
            powers,
            trip_reason: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Bzm2CalibrationConfig {
    pub enabled: bool,
    pub apply_saved_operating_point: bool,
    pub operating_class: Bzm2OperatingClass,
    pub performance_mode: Bzm2PerformanceMode,
    pub mode: Bzm2CalibrationMode,
    pub per_stack_clocking: bool,
    pub force_retune: bool,
    pub asics_per_bus: Vec<u16>,
    pub asics_per_domain: Vec<u16>,
    pub domain_voltage_offsets_mv: Vec<i32>,
    pub profile_path: Option<PathBuf>,
    pub site_temp_c: Option<f32>,
    pub pll_post1_divider: u8,
    pub skip_lock_check: bool,
    pub lock_timeout: Duration,
    pub lock_poll_interval: Duration,
}

impl Default for Bzm2CalibrationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            apply_saved_operating_point: true,
            operating_class: Bzm2OperatingClass::Generic,
            performance_mode: Bzm2PerformanceMode::Standard,
            mode: Bzm2CalibrationMode::default(),
            per_stack_clocking: false,
            force_retune: false,
            asics_per_bus: vec![1],
            asics_per_domain: vec![1],
            domain_voltage_offsets_mv: Vec::new(),
            profile_path: None,
            site_temp_c: None,
            pll_post1_divider: DEFAULT_CALIBRATION_POST1_DIVIDER,
            skip_lock_check: false,
            lock_timeout: Duration::from_millis(DEFAULT_CALIBRATION_LOCK_TIMEOUT_MS),
            lock_poll_interval: Duration::from_millis(DEFAULT_CALIBRATION_LOCK_POLL_MS),
        }
    }
}

impl Bzm2CalibrationConfig {
    fn from_env(serial_count: usize) -> Self {
        let mut config = Self {
            enabled: env_flag("MUJINA_BZM2_CALIBRATE") || env_flag("MUJINA_BZM2_ENABLE_PNP"),
            apply_saved_operating_point: env_flag_default_any(
                &[
                    "MUJINA_BZM2_APPLY_SAVED_OPERATING_POINT",
                    "MUJINA_BZM2_REPLAY_STORED_CALIBRATION",
                ],
                true,
            ),
            operating_class: env_var_any(&["MUJINA_BZM2_OPERATING_CLASS", "MUJINA_BZM2_BOARD_BIN"])
                .as_deref()
                .and_then(parse_operating_class)
                .unwrap_or(Bzm2OperatingClass::Generic),
            performance_mode: env_var_any(&[
                "MUJINA_BZM2_PERFORMANCE_MODE",
                "MUJINA_BZM2_MINING_STRATEGY",
            ])
            .as_deref()
            .and_then(parse_performance_mode)
            .unwrap_or(Bzm2PerformanceMode::Standard),
            mode: Bzm2CalibrationMode {
                sweep_strategy: env_flag_any(&[
                    "MUJINA_BZM2_SWEEP_MODE",
                    "MUJINA_BZM2_SWEEP_STRATEGY",
                ]),
                sweep_voltage: env_flag("MUJINA_BZM2_SWEEP_VOLTAGE"),
                sweep_frequency: env_flag("MUJINA_BZM2_SWEEP_FREQUENCY"),
                sweep_pass_rate: env_flag("MUJINA_BZM2_SWEEP_PASS_RATE"),
            },
            per_stack_clocking: env_flag_any(&[
                "MUJINA_BZM2_PER_STACK_CLOCKING",
                "MUJINA_BZM2_SPLIT_STACK_FREQUENCY",
            ]),
            force_retune: env_flag_any(&[
                "MUJINA_BZM2_FORCE_RETUNE",
                "MUJINA_BZM2_FORCE_RECALIBRATION",
            ]),
            asics_per_bus: parse_csv_numbers::<u16>("MUJINA_BZM2_ASICS_PER_BUS").unwrap_or_else(
                || {
                    if serial_count == 0 {
                        Vec::new()
                    } else {
                        vec![1; serial_count]
                    }
                },
            ),
            asics_per_domain: parse_csv_numbers::<u16>("MUJINA_BZM2_ASICS_PER_DOMAIN")
                .unwrap_or_else(|| vec![1]),
            domain_voltage_offsets_mv: parse_csv_numbers::<i32>(
                "MUJINA_BZM2_DOMAIN_VOLTAGE_OFFSETS_MV",
            )
            .unwrap_or_default(),
            profile_path: env_var_any(&[
                "MUJINA_BZM2_SAVED_OPERATING_POINT_PATH",
                "MUJINA_BZM2_CALIBRATION_PROFILE",
            ])
            .map(PathBuf::from),
            site_temp_c: env_f32_any(&["MUJINA_BZM2_SITE_TEMP_C", "MUJINA_BZM2_AMBIENT_TEMP_C"]),
            pll_post1_divider: env::var("MUJINA_BZM2_CALIBRATION_POST1_DIVIDER")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(DEFAULT_CALIBRATION_POST1_DIVIDER),
            skip_lock_check: env_flag("MUJINA_BZM2_CALIBRATION_SKIP_LOCK_CHECK"),
            lock_timeout: Duration::from_millis(
                env::var("MUJINA_BZM2_CALIBRATION_LOCK_TIMEOUT_MS")
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(DEFAULT_CALIBRATION_LOCK_TIMEOUT_MS),
            ),
            lock_poll_interval: Duration::from_millis(
                env::var("MUJINA_BZM2_CALIBRATION_LOCK_POLL_MS")
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(DEFAULT_CALIBRATION_LOCK_POLL_MS),
            ),
        };

        if config.asics_per_bus.is_empty() && serial_count > 0 {
            config.asics_per_bus = vec![1; serial_count];
        }
        if config.asics_per_domain.is_empty() {
            config.asics_per_domain = vec![1];
        }

        config
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct Bzm2BusLayout {
    serial_path: String,
    asic_start: u16,
    asic_count: u16,
}

impl Bzm2BusLayout {
    fn contains(&self, global_asic_id: u16) -> bool {
        global_asic_id >= self.asic_start && global_asic_id < self.asic_start + self.asic_count
    }

    fn local_asic_id(&self, global_asic_id: u16) -> Option<u8> {
        self.contains(global_asic_id)
            .then_some((global_asic_id - self.asic_start) as u8)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct Bzm2PersistedCalibrationProfile {
    schema_version: u32,
    #[serde(alias = "board_bin")]
    operating_class: String,
    #[serde(alias = "strategy")]
    performance_mode: String,
    asics_per_bus: Vec<u16>,
    pll_post1_divider: u8,
    #[serde(alias = "calibration")]
    saved_state: Bzm2SavedOperatingPoint,
}

impl Bzm2PersistedCalibrationProfile {
    const SCHEMA_VERSION: u32 = 1;

    fn is_compatible(
        &self,
        calibration: &Bzm2CalibrationConfig,
        bus_layouts: &[Bzm2BusLayout],
    ) -> bool {
        self.schema_version == Self::SCHEMA_VERSION
            && self.operating_class == operating_class_name(calibration.operating_class)
            && self.performance_mode == performance_mode_name(calibration.performance_mode)
            && self.pll_post1_divider == calibration.pll_post1_divider
            && self.asics_per_bus
                == bus_layouts
                    .iter()
                    .map(|bus| bus.asic_count)
                    .collect::<Vec<_>>()
            && self.saved_state.per_asic_pll_mhz.len()
                == bus_layouts
                    .iter()
                    .map(|bus| bus.asic_count as usize)
                    .sum::<usize>()
    }
}

#[derive(Debug, Clone)]
struct Bzm2LoadedCalibrationProfile {
    persisted: Option<Bzm2PersistedCalibrationProfile>,
    saved_state: Bzm2SavedOperatingPoint,
}

pub struct Bzm2Board {
    config: Bzm2VirtualDeviceConfig,
    bringup_applied: bool,
    shutdown_handles: Vec<Bzm2ThreadHandle>,
    serial_controls: Vec<SerialControl>,
    state_tx: watch::Sender<BoardState>,
    command_rx: Option<mpsc::Receiver<BoardCommand>>,
    monitor_shutdown: Option<watch::Sender<bool>>,
    monitor_task: Option<JoinHandle<()>>,
    command_shutdown: Option<watch::Sender<bool>>,
    command_task: Option<JoinHandle<()>>,
}

impl Bzm2Board {
    pub fn new(
        config: Bzm2VirtualDeviceConfig,
        state_tx: watch::Sender<BoardState>,
        command_rx: mpsc::Receiver<BoardCommand>,
    ) -> Self {
        Self {
            config,
            bringup_applied: false,
            shutdown_handles: Vec::new(),
            serial_controls: Vec::new(),
            state_tx,
            command_rx: Some(command_rx),
            monitor_shutdown: None,
            monitor_task: None,
            command_shutdown: None,
            command_task: None,
        }
    }

    async fn apply_bringup_sequence(&mut self) -> Result<(), BoardError> {
        if self.bringup_applied || !self.config.bringup.enabled {
            return Ok(());
        }

        let mut rails = self.config.bringup.build_rails();
        let mut reset_line = self.config.bringup.build_reset_line();
        self.config
            .bringup
            .plan
            .apply(&mut rails, reset_line.as_mut())
            .await
            .map_err(|err| {
                BoardError::InitializationFailed(format!("BZM2 bring-up sequence failed: {err}"))
            })?;
        self.bringup_applied = true;
        Ok(())
    }

    async fn apply_shutdown_sequence(&mut self) -> Result<(), BoardError> {
        if !self.bringup_applied || !self.config.bringup.enabled {
            return Ok(());
        }

        let mut rails = self.config.bringup.build_rails();
        let mut reset_line = self.config.bringup.build_reset_line();
        self.config
            .bringup
            .plan
            .shutdown(&mut rails, reset_line.as_mut())
            .await
            .map_err(|err| {
                BoardError::HardwareControl(format!("BZM2 shutdown sequence failed: {err}"))
            })?;
        self.bringup_applied = false;
        Ok(())
    }

    fn spawn_monitor(&mut self) {
        if (!self.config.telemetry.is_enabled() && !self.config.bringup.has_telemetry())
            || self.monitor_task.is_some()
        {
            return;
        }

        let telemetry = self.config.telemetry.clone();
        let rail_telemetry = self.config.bringup.clone();
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
                        let rail_snapshot = rail_telemetry.snapshot_telemetry();
                        let total_stats = serial_controls.iter().fold((0u64, 0u64), |acc, control| {
                            let stats = control.stats();
                            (acc.0 + stats.bytes_read, acc.1 + stats.bytes_written)
                        });
                        let _ = state_tx.send_modify(|state| {
                            state.fans = snapshot.fans.clone();
                            merge_temperature_readings(&mut state.temperatures, &snapshot.temperatures);
                            merge_power_readings(&mut state.powers, &snapshot.powers);
                            merge_temperature_readings(&mut state.temperatures, &rail_snapshot.temperatures);
                            merge_power_readings(&mut state.powers, &rail_snapshot.powers);
                        });
                        trace!(board = %board_name, bytes_read = total_stats.0, bytes_written = total_stats.1, "BZM2 board telemetry updated");
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

    fn spawn_command_loop(&mut self) {
        if self.command_task.is_some() {
            return;
        }
        let Some(mut command_rx) = self.command_rx.take() else {
            return;
        };

        let state_tx = self.state_tx.clone();
        let shutdown_handles = self.shutdown_handles.clone();
        let board_name = self.config.device_id();
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        self.command_shutdown = Some(shutdown_tx);

        self.command_task = Some(tokio::spawn(async move {
            loop {
                tokio::select! {
                    command = command_rx.recv() => {
                        let Some(command) = command else {
                            break;
                        };
                        match command {
                            BoardCommand::QueryBzm2DtsVs { thread_index, asic, reply } => {
                                let result = async {
                                    let handle = shutdown_handles.get(thread_index).ok_or_else(|| {
                                        BoardError::HardwareControl(format!(
                                            "invalid BZM2 thread index {thread_index} for board {board_name}"
                                        ))
                                    })?;
                                    let update = handle
                                        .query_dts_vs(asic)
                                        .await
                                        .map_err(|err| BoardError::HardwareControl(err.to_string()))?;
                                    publish_thread_telemetry(&state_tx, &update);
                                    Ok(())
                                }
                                .await;
                                let _ = reply.send(result);
                            }
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

    async fn resolve_bus_layouts(&self) -> Result<Vec<Bzm2BusLayout>, BoardError> {
        let configured = build_bus_layouts(
            &self.config.serial_paths,
            &self.config.calibration.asics_per_bus,
        );
        if !self.config.enumeration.enabled {
            return Ok(configured);
        }

        let discovered = self.enumerate_bus_layouts().await?;
        if should_fallback_to_configured_bus_layouts(&discovered, &configured) {
            warn!(
                board = %self.config.device_id(),
                "BZM2 startup enumeration found no ASICs on the default id; falling back to configured bus topology"
            );
            return Ok(configured);
        }

        Ok(discovered)
    }

    async fn enumerate_bus_layouts(&self) -> Result<Vec<Bzm2BusLayout>, BoardError> {
        let mut counts = Vec::with_capacity(self.config.serial_paths.len());

        for (index, serial_path) in self.config.serial_paths.iter().enumerate() {
            let max_asics = *self
                .config
                .enumeration
                .max_asics_per_bus
                .get(index)
                .or_else(|| self.config.enumeration.max_asics_per_bus.last())
                .unwrap_or(&DEFAULT_ENUMERATION_MAX_ASICS_PER_BUS);
            let max_asics = max_asics.min(u8::MAX as u16) as u8;

            let stream = SerialStream::new(serial_path, self.config.baud_rate).map_err(|err| {
                BoardError::InitializationFailed(format!(
                    "Failed to open BZM2 enumeration transport {}: {}",
                    serial_path, err
                ))
            })?;
            let (reader, writer, _control) = stream.split();
            let mut uart = Bzm2UartController::new(reader, writer);
            let assigned = uart
                .enumerate_chain(max_asics, self.config.enumeration.start_id)
                .await
                .map_err(|err| {
                    BoardError::InitializationFailed(format!(
                        "BZM2 startup enumeration failed on {}: {}",
                        serial_path, err
                    ))
                })?;
            counts.push(assigned.len() as u16);
            info!(
                board = %self.config.device_id(),
                serial_path,
                asic_count = assigned.len(),
                "BZM2 startup enumeration completed"
            );
        }

        Ok(build_discovered_bus_layouts(
            &self.config.serial_paths,
            &counts,
        ))
    }

    async fn execute_live_calibration(
        &self,
        bus_layouts: &[Bzm2BusLayout],
    ) -> Result<(), BoardError> {
        let calibration = &self.config.calibration;
        if !calibration.enabled {
            return Ok(());
        }

        let total_asics = bus_layouts
            .iter()
            .map(|layout| layout.asic_count as usize)
            .sum::<usize>();
        if total_asics == 0 {
            return Ok(());
        }

        let loaded_profile =
            load_saved_operating_point_profile(calibration.profile_path.as_deref())
                .map_err(BoardError::InitializationFailed)?;
        if calibration.apply_saved_operating_point && !calibration.force_retune {
            if let Some(profile) = loaded_profile
                .as_ref()
                .and_then(|loaded| loaded.persisted.as_ref())
                .filter(|profile| profile.is_compatible(calibration, &bus_layouts))
            {
                self.apply_saved_operating_point(&bus_layouts, profile)
                    .await?;
                info!(
                    board = %self.config.device_id(),
                    asic_count = profile.saved_state.per_asic_pll_mhz.len(),
                    "BZM2 replayed saved operating point profile"
                );
                return Ok(());
            }
        }

        let telemetry = self.config.telemetry.snapshot();
        let site_temp_c = calibration
            .site_temp_c
            .or_else(|| snapshot_temperature(&telemetry, "board"))
            .or_else(|| snapshot_temperature(&telemetry, "asic"))
            .unwrap_or(DEFAULT_CALIBRATION_SITE_TEMP_C);
        let saved_operating_point = loaded_profile
            .as_ref()
            .map(|loaded| loaded.saved_state.clone());
        let (voltage_domains, domain_lookup) = build_voltage_domains(
            total_asics as u16,
            &calibration.asics_per_domain,
            &calibration.domain_voltage_offsets_mv,
        );
        let asics = build_topology(&bus_layouts, &domain_lookup);
        let alive_asics = asics.iter().filter(|asic| asic.alive).count().max(1);
        let per_asic_throughput = saved_operating_point
            .as_ref()
            .map(|stored| stored.board_throughput_ths / alive_asics as f32);
        let shared_temp = snapshot_temperature(&telemetry, "asic")
            .or_else(|| snapshot_temperature(&telemetry, "board"));
        let asic_measurements = asics
            .iter()
            .map(|asic| Bzm2AsicMeasurement {
                asic_id: asic.asic_id,
                temperature_c: shared_temp,
                throughput_ths: per_asic_throughput,
                average_pass_rate: None,
                pll_pass_rates: [None, None],
            })
            .collect::<Vec<_>>();
        let shared_domain_power = snapshot_input_power(&telemetry).map(|power| {
            if voltage_domains.is_empty() {
                power
            } else {
                power / voltage_domains.len() as f32
            }
        });
        let domain_measurements = voltage_domains
            .iter()
            .map(|domain| Bzm2DomainMeasurement {
                domain_id: domain.domain_id,
                measured_voltage_mv: None,
                measured_power_w: shared_domain_power,
            })
            .collect::<Vec<_>>();

        let planner = Bzm2CalibrationPlanner;
        let plan = planner.plan(&Bzm2BoardCalibrationInput {
            operating_class: calibration.operating_class,
            site_temp_c,
            target_mode: calibration.performance_mode,
            mode: calibration.mode,
            per_stack_clocking: calibration.per_stack_clocking,
            voltage_domains: voltage_domains.clone(),
            asics: asics.clone(),
            saved_operating_point,
            domain_measurements,
            asic_measurements,
            constraints: Bzm2CalibrationConstraints::default(),
            force_retune: calibration.force_retune,
        });
        let per_domain_voltage_mv = plan
            .domain_plans
            .iter()
            .map(|domain| (domain.domain_id, domain.voltage_mv))
            .collect::<BTreeMap<_, _>>();
        self.apply_domain_voltage_map(&per_domain_voltage_mv)
            .await?;

        let per_asic_pll_mhz = plan
            .asic_plans
            .iter()
            .map(|plan| (plan.asic_id, plan.pll_frequencies_mhz))
            .collect::<BTreeMap<_, _>>();
        self.apply_frequency_map(
            &bus_layouts,
            [plan.initial_frequency_mhz; 2],
            &per_asic_pll_mhz,
        )
        .await?;

        if let Some(profile_path) = calibration.profile_path.as_deref() {
            let saved_operating_point = Bzm2SavedOperatingPoint {
                board_voltage_mv: average_u32(
                    plan.domain_plans.iter().map(|domain| domain.voltage_mv),
                )
                .unwrap_or(plan.desired_voltage_mv),
                board_throughput_ths: estimate_planned_hashrate(
                    &plan,
                    self.config.nominal_hashrate_ths as f32,
                    self.config.serial_paths.len(),
                ),
                per_domain_voltage_mv,
                per_asic_pll_mhz,
            };
            let profile = Bzm2PersistedCalibrationProfile {
                schema_version: Bzm2PersistedCalibrationProfile::SCHEMA_VERSION,
                operating_class: operating_class_name(calibration.operating_class).into(),
                performance_mode: performance_mode_name(calibration.performance_mode).into(),
                asics_per_bus: bus_layouts.iter().map(|bus| bus.asic_count).collect(),
                pll_post1_divider: calibration.pll_post1_divider,
                saved_state: saved_operating_point,
            };
            store_calibration_profile(profile_path, &profile)
                .map_err(BoardError::InitializationFailed)?;
        }

        info!(board = %self.config.device_id(), reuse_saved_operating_point = plan.reuse_saved_operating_point, needs_retune = plan.needs_retune, initial_frequency_mhz = plan.initial_frequency_mhz, asic_count = plan.asic_plans.len(), "BZM2 live calibration completed");
        Ok(())
    }

    async fn apply_saved_operating_point(
        &self,
        bus_layouts: &[Bzm2BusLayout],
        profile: &Bzm2PersistedCalibrationProfile,
    ) -> Result<(), BoardError> {
        self.apply_domain_voltage_map(&profile.saved_state.per_domain_voltage_mv)
            .await?;
        for bus in bus_layouts {
            if bus.asic_count == 0 {
                continue;
            }
            let initial_frequencies = [0usize, 1usize].map(|pll_index| {
                average_f32(
                    (bus.asic_start..bus.asic_start + bus.asic_count)
                        .filter_map(|asic_id| profile.saved_state.per_asic_pll_mhz.get(&asic_id))
                        .map(|frequencies| frequencies[pll_index]),
                )
                .unwrap_or(DEFAULT_CALIBRATION_SITE_TEMP_C)
            });
            self.apply_bus_frequency_map(
                bus,
                initial_frequencies,
                &profile.saved_state.per_asic_pll_mhz,
            )
            .await?;
        }
        Ok(())
    }

    async fn apply_domain_voltage_map(
        &self,
        per_domain_voltage_mv: &BTreeMap<u16, u32>,
    ) -> Result<(), BoardError> {
        if per_domain_voltage_mv.is_empty() {
            return Ok(());
        }
        if self.config.bringup.rail_set_paths.is_empty() {
            warn!(
                board = %self.config.device_id(),
                ?per_domain_voltage_mv,
                "planner produced per-domain voltages, but no BZM2 rail control path is configured"
            );
            return Ok(());
        }

        let mut rail_targets_mv = BTreeMap::<usize, u32>::new();
        for (&domain_id, &voltage_mv) in per_domain_voltage_mv {
            let rail_index = self
                .config
                .bringup
                .rail_index_for_domain(domain_id)
                .ok_or_else(|| {
                    BoardError::HardwareControl(format!(
                        "BZM2 domain {domain_id} has no mapped rail index"
                    ))
                })?;
            if rail_index >= self.config.bringup.rail_set_paths.len() {
                return Err(BoardError::HardwareControl(format!(
                    "BZM2 domain {domain_id} mapped to rail {rail_index}, but only {} rail set paths are configured",
                    self.config.bringup.rail_set_paths.len()
                )));
            }
            match rail_targets_mv.entry(rail_index) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(voltage_mv);
                }
                std::collections::btree_map::Entry::Occupied(entry)
                    if *entry.get() != voltage_mv =>
                {
                    return Err(BoardError::HardwareControl(format!(
                        "BZM2 rail {rail_index} received conflicting domain voltages: {}mV vs {}mV",
                        entry.get(),
                        voltage_mv
                    )));
                }
                std::collections::btree_map::Entry::Occupied(_) => {}
            }
        }

        let mut rails = self.config.bringup.build_rails();
        for (rail_index, voltage_mv) in rail_targets_mv {
            let rail = rails.get_mut(rail_index).ok_or_else(|| {
                BoardError::HardwareControl(format!(
                    "BZM2 rail {rail_index} is missing from configured rail controls"
                ))
            })?;
            rail.set_voltage(voltage_mv as f32 / 1000.0)
                .await
                .map_err(|err| {
                    BoardError::HardwareControl(format!(
                        "Failed to apply BZM2 domain voltage {voltage_mv}mV on rail {rail_index}: {err}"
                    ))
                })?;
        }
        Ok(())
    }

    async fn apply_frequency_map(
        &self,
        bus_layouts: &[Bzm2BusLayout],
        initial_frequencies_mhz: [f32; 2],
        per_asic_pll_mhz: &BTreeMap<u16, [f32; 2]>,
    ) -> Result<(), BoardError> {
        for bus in bus_layouts {
            self.apply_bus_frequency_map(bus, initial_frequencies_mhz, per_asic_pll_mhz)
                .await?;
        }
        Ok(())
    }

    async fn apply_bus_frequency_map(
        &self,
        bus: &Bzm2BusLayout,
        initial_frequencies_mhz: [f32; 2],
        per_asic_pll_mhz: &BTreeMap<u16, [f32; 2]>,
    ) -> Result<(), BoardError> {
        if bus.asic_count == 0 {
            return Ok(());
        }
        let stream = SerialStream::new(&bus.serial_path, self.config.baud_rate).map_err(|err| {
            BoardError::InitializationFailed(format!(
                "Failed to open BZM2 calibration transport {}: {}",
                bus.serial_path, err
            ))
        })?;
        let (reader, writer, _control) = stream.split();
        let mut clock = Bzm2ClockController::new(reader, writer);

        for (pll, frequency_mhz) in [Bzm2Pll::Pll0, Bzm2Pll::Pll1]
            .into_iter()
            .zip(initial_frequencies_mhz)
        {
            clock
                .broadcast_pll_frequency(
                    pll,
                    frequency_mhz,
                    self.config.calibration.pll_post1_divider,
                )
                .await
                .map_err(|err| calibration_error(&bus.serial_path, err))?;
            clock
                .broadcast_enable_pll(pll)
                .await
                .map_err(|err| calibration_error(&bus.serial_path, err))?;
        }

        if !self.config.calibration.skip_lock_check {
            for local_asic in 0..bus.asic_count {
                for pll in [Bzm2Pll::Pll0, Bzm2Pll::Pll1] {
                    clock
                        .wait_for_pll_lock(
                            local_asic as u8,
                            pll,
                            self.config.calibration.lock_timeout,
                            self.config.calibration.lock_poll_interval,
                        )
                        .await
                        .map_err(|err| calibration_error(&bus.serial_path, err))?;
                }
            }
        }

        for asic_id in bus.asic_start..bus.asic_start + bus.asic_count {
            let Some(frequencies_mhz) = per_asic_pll_mhz.get(&asic_id) else {
                continue;
            };
            let local_asic = bus
                .local_asic_id(asic_id)
                .expect("bus layout must contain loop asic id");
            for (index, frequency_mhz) in frequencies_mhz.iter().enumerate() {
                let pll = if index == 0 {
                    Bzm2Pll::Pll0
                } else {
                    Bzm2Pll::Pll1
                };
                clock
                    .set_pll_frequency(
                        local_asic,
                        pll,
                        *frequency_mhz,
                        self.config.calibration.pll_post1_divider,
                    )
                    .await
                    .map_err(|err| calibration_error(&bus.serial_path, err))?;
                clock
                    .enable_pll(local_asic, pll)
                    .await
                    .map_err(|err| calibration_error(&bus.serial_path, err))?;
                if !self.config.calibration.skip_lock_check {
                    clock
                        .wait_for_pll_lock(
                            local_asic,
                            pll,
                            self.config.calibration.lock_timeout,
                            self.config.calibration.lock_poll_interval,
                        )
                        .await
                        .map_err(|err| calibration_error(&bus.serial_path, err))?;
                }
            }
        }

        Ok(())
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
        let _ = self.state_tx.send_modify(|state| {
            for thread in &mut state.threads {
                thread.is_active = false;
                thread.hashrate = 0;
            }
        });
        self.apply_shutdown_sequence().await?;
        Ok(())
    }

    async fn create_hash_threads(&mut self) -> Result<Vec<Box<dyn HashThread>>, BoardError> {
        let mut threads: Vec<Box<dyn HashThread>> = Vec::new();
        let mut thread_states = Vec::new();
        self.apply_bringup_sequence().await?;
        let bus_layouts = self.resolve_bus_layouts().await?;
        let initial_snapshot = self.config.telemetry.snapshot();
        let initial_rail_snapshot = self.config.bringup.snapshot_telemetry();
        let _ = self.state_tx.send_modify(|state| {
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
        let _ = self.state_tx.send_modify(|state| {
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
        self.spawn_command_loop();
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
                match &event {
                    HashThreadEvent::StatusUpdate(status) => {
                        publish_thread_status(&state_tx, thread_index, status);
                    }
                    HashThreadEvent::TelemetryUpdate(update) => {
                        publish_thread_telemetry(&state_tx, update);
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

fn publish_thread_telemetry(
    state_tx: &watch::Sender<BoardState>,
    update: &HashThreadTelemetryUpdate,
) {
    let _ = state_tx.send_modify(|state| {
        merge_temperature_readings(
            &mut state.temperatures,
            &update
                .temperatures
                .iter()
                .map(|reading| TemperatureSensor {
                    name: reading.name.clone(),
                    temperature_c: reading.temperature_c,
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

fn merge_temperature_readings(
    existing: &mut Vec<TemperatureSensor>,
    updates: &[TemperatureSensor],
) {
    for update in updates {
        if let Some(sensor) = existing
            .iter_mut()
            .find(|sensor| sensor.name == update.name)
        {
            sensor.temperature_c = update.temperature_c;
        } else {
            existing.push(update.clone());
        }
    }
}

fn merge_power_readings(existing: &mut Vec<PowerMeasurement>, updates: &[PowerMeasurement]) {
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

fn build_bus_layouts(serial_paths: &[String], asics_per_bus: &[u16]) -> Vec<Bzm2BusLayout> {
    build_bus_layouts_with_minimum(serial_paths, asics_per_bus, 1)
}

fn build_discovered_bus_layouts(
    serial_paths: &[String],
    asics_per_bus: &[u16],
) -> Vec<Bzm2BusLayout> {
    build_bus_layouts_with_minimum(serial_paths, asics_per_bus, 0)
}

fn build_bus_layouts_with_minimum(
    serial_paths: &[String],
    asics_per_bus: &[u16],
    minimum_asic_count: u16,
) -> Vec<Bzm2BusLayout> {
    let mut next_asic = 0u16;
    serial_paths
        .iter()
        .enumerate()
        .map(|(index, path)| {
            let asic_count = *asics_per_bus
                .get(index)
                .or_else(|| asics_per_bus.last())
                .unwrap_or(&1)
                .max(&minimum_asic_count);
            let layout = Bzm2BusLayout {
                serial_path: path.clone(),
                asic_start: next_asic,
                asic_count,
            };
            next_asic = next_asic.saturating_add(asic_count);
            layout
        })
        .collect()
}

fn should_fallback_to_configured_bus_layouts(
    discovered: &[Bzm2BusLayout],
    configured: &[Bzm2BusLayout],
) -> bool {
    let discovered_total = discovered
        .iter()
        .map(|layout| layout.asic_count as usize)
        .sum::<usize>();
    let configured_total = configured
        .iter()
        .map(|layout| layout.asic_count as usize)
        .sum::<usize>();
    discovered_total == 0 && configured_total > 0
}

fn build_voltage_domains(
    total_asics: u16,
    asics_per_domain: &[u16],
    domain_voltage_offsets_mv: &[i32],
) -> (Vec<Bzm2VoltageDomain>, BTreeMap<u16, u16>) {
    let mut domains = Vec::new();
    let mut lookup = BTreeMap::new();
    let mut domain_id = 0u16;
    let mut asic_start = 0u16;
    while asic_start < total_asics {
        let requested = *asics_per_domain
            .get(domain_id as usize)
            .or_else(|| asics_per_domain.last())
            .unwrap_or(&total_asics)
            .max(&1);
        let asic_end = (asic_start.saturating_add(requested)).min(total_asics);
        let asic_ids = (asic_start..asic_end).collect::<Vec<_>>();
        for asic_id in &asic_ids {
            lookup.insert(*asic_id, domain_id);
        }
        domains.push(Bzm2VoltageDomain {
            domain_id,
            asic_ids,
            voltage_offset_mv: *domain_voltage_offsets_mv
                .get(domain_id as usize)
                .or_else(|| domain_voltage_offsets_mv.last())
                .unwrap_or(&0),
            max_power_w: None,
        });
        domain_id = domain_id.saturating_add(1);
        asic_start = asic_end;
    }
    (domains, lookup)
}

fn build_topology(
    bus_layouts: &[Bzm2BusLayout],
    domain_lookup: &BTreeMap<u16, u16>,
) -> Vec<Bzm2AsicTopology> {
    let mut asics = Vec::new();
    for layout in bus_layouts {
        for asic_id in layout.asic_start..layout.asic_start + layout.asic_count {
            asics.push(Bzm2AsicTopology {
                asic_id,
                domain_id: *domain_lookup.get(&asic_id).unwrap_or(&0),
                pll_count: 2,
                alive: true,
            });
        }
    }
    asics
}

fn load_saved_operating_point_profile(
    path: Option<&Path>,
) -> Result<Option<Bzm2LoadedCalibrationProfile>, String> {
    let Some(path) = path else {
        return Ok(None);
    };
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path).map_err(|err| {
        format!(
            "Failed to read calibration profile {}: {}",
            path.display(),
            err
        )
    })?;

    if let Ok(profile) = serde_json::from_str::<Bzm2PersistedCalibrationProfile>(&raw) {
        return Ok(Some(Bzm2LoadedCalibrationProfile {
            saved_state: profile.saved_state.clone(),
            persisted: Some(profile),
        }));
    }

    serde_json::from_str::<Bzm2SavedOperatingPoint>(&raw)
        .map(|saved_state| {
            Some(Bzm2LoadedCalibrationProfile {
                persisted: None,
                saved_state,
            })
        })
        .map_err(|err| {
            format!(
                "Failed to parse calibration profile {}: {}",
                path.display(),
                err
            )
        })
}

fn store_calibration_profile(
    path: &Path,
    profile: &Bzm2PersistedCalibrationProfile,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            format!(
                "Failed to create calibration profile directory {}: {}",
                parent.display(),
                err
            )
        })?;
    }
    let raw = serde_json::to_string_pretty(profile)
        .map_err(|err| format!("Failed to serialize calibration profile: {}", err))?;
    fs::write(path, raw).map_err(|err| {
        format!(
            "Failed to write calibration profile {}: {}",
            path.display(),
            err
        )
    })
}

fn estimate_planned_hashrate(
    plan: &crate::asic::bzm2::Bzm2CalibrationPlan,
    nominal_hashrate_ths: f32,
    thread_count: usize,
) -> f32 {
    let nominal_board_hashrate = nominal_hashrate_ths * thread_count.max(1) as f32;
    let average_frequency_mhz = if plan.asic_plans.is_empty() {
        plan.desired_clock_mhz
    } else {
        plan.asic_plans
            .iter()
            .map(|asic| (asic.pll_frequencies_mhz[0] + asic.pll_frequencies_mhz[1]) / 2.0)
            .sum::<f32>()
            / plan.asic_plans.len() as f32
    };
    let ratio = if plan.desired_clock_mhz > 0.0 {
        average_frequency_mhz / plan.desired_clock_mhz
    } else {
        1.0
    };
    nominal_board_hashrate * ratio.max(0.1)
}

fn snapshot_temperature(snapshot: &Bzm2TelemetrySnapshot, name: &str) -> Option<f32> {
    snapshot
        .temperatures
        .iter()
        .find(|sensor| sensor.name == name)
        .and_then(|sensor| sensor.temperature_c)
}

fn snapshot_input_power(snapshot: &Bzm2TelemetrySnapshot) -> Option<f32> {
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

fn env_var_any(keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| env::var(key).ok())
}

fn env_csv_strings_any(keys: &[&str]) -> Vec<String> {
    env_var_any(keys)
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn env_flag(key: &str) -> bool {
    env_var_any(&[key]).as_deref().is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn env_flag_any(keys: &[&str]) -> bool {
    env_var_any(keys).as_deref().is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn env_flag_default_any(keys: &[&str], default: bool) -> bool {
    env_var_any(keys)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(default)
}

fn env_f32(key: &str) -> Option<f32> {
    env_f32_any(&[key])
}

fn env_f32_any(keys: &[&str]) -> Option<f32> {
    env_var_any(keys).and_then(|value| value.parse().ok())
}

fn parse_u32(value: &str) -> Option<u32> {
    let trimmed = value.trim();
    if let Some(hex) = trimmed.strip_prefix("0x") {
        u32::from_str_radix(hex, 16).ok()
    } else {
        trimmed.parse().ok()
    }
}

fn parse_csv_numbers<T>(key: &str) -> Option<Vec<T>>
where
    T: std::str::FromStr,
{
    let value = env::var(key).ok()?;
    let parsed = value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.parse().ok())
        .collect::<Option<Vec<_>>>()?;
    Some(parsed)
}

fn parse_csv_numbers_any<T>(keys: &[&str]) -> Option<Vec<T>>
where
    T: std::str::FromStr,
{
    keys.iter().find_map(|key| parse_csv_numbers::<T>(key))
}

fn sensor_specs_from_env(
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

fn parse_operating_class(value: &str) -> Option<Bzm2OperatingClass> {
    match value.trim().to_ascii_lowercase().as_str() {
        "generic" => Some(Bzm2OperatingClass::Generic),
        "early-validation" | "early_validation" | "dvt1" => {
            Some(Bzm2OperatingClass::EarlyValidation)
        }
        "production-validation" | "production_validation" | "pvt" => {
            Some(Bzm2OperatingClass::ProductionValidation)
        }
        "stack-tuned-a" | "stack_tuned_a" | "dvt2-bin1" | "dvt2_bin1" | "dvt2bin1" | "bin1" => {
            Some(Bzm2OperatingClass::StackTunedA)
        }
        "stack-tuned-b" | "stack_tuned_b" | "dvt2-bin2" | "dvt2_bin2" | "dvt2bin2" | "bin2" => {
            Some(Bzm2OperatingClass::StackTunedB)
        }
        "extended-headroom" | "extended_headroom" | "plus" => {
            Some(Bzm2OperatingClass::ExtendedHeadroom)
        }
        "extended-headroom-b"
        | "extended_headroom_b"
        | "plus-ebin2"
        | "plus_ebin2"
        | "plusebin2"
        | "ebin2" => Some(Bzm2OperatingClass::ExtendedHeadroomB),
        _ => None,
    }
}

fn parse_performance_mode(value: &str) -> Option<Bzm2PerformanceMode> {
    match value.trim().to_ascii_lowercase().as_str() {
        "max-throughput" | "max_throughput" | "high" | "high-performance" | "high_performance"
        | "performance" => Some(Bzm2PerformanceMode::MaxThroughput),
        "standard" | "balanced" => Some(Bzm2PerformanceMode::Standard),
        "efficiency" | "low" | "low-power" | "low_power" => Some(Bzm2PerformanceMode::Efficiency),
        _ => None,
    }
}

fn average_u32(values: impl Iterator<Item = u32>) -> Option<u32> {
    let mut total = 0u64;
    let mut count = 0u64;
    for value in values {
        total += value as u64;
        count += 1;
    }
    (count > 0).then_some((total / count) as u32)
}

fn average_f32(values: impl Iterator<Item = f32>) -> Option<f32> {
    let mut total = 0.0f32;
    let mut count = 0usize;
    for value in values {
        total += value;
        count += 1;
    }
    (count > 0).then_some(total / count as f32)
}

fn operating_class_name(operating_class: Bzm2OperatingClass) -> &'static str {
    match operating_class {
        Bzm2OperatingClass::Generic => "generic",
        Bzm2OperatingClass::EarlyValidation => "early-validation",
        Bzm2OperatingClass::ProductionValidation => "production-validation",
        Bzm2OperatingClass::StackTunedA => "stack-tuned-a",
        Bzm2OperatingClass::StackTunedB => "stack-tuned-b",
        Bzm2OperatingClass::ExtendedHeadroom => "extended-headroom",
        Bzm2OperatingClass::ExtendedHeadroomB => "extended-headroom-b",
    }
}

fn performance_mode_name(performance_mode: Bzm2PerformanceMode) -> &'static str {
    match performance_mode {
        Bzm2PerformanceMode::MaxThroughput => "max-throughput",
        Bzm2PerformanceMode::Standard => "standard",
        Bzm2PerformanceMode::Efficiency => "efficiency",
    }
}

fn calibration_error(serial_path: &str, err: impl std::fmt::Display) -> BoardError {
    BoardError::InitializationFailed(format!(
        "BZM2 calibration failed on {}: {}",
        serial_path, err
    ))
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
    let (command_tx, command_rx) = mpsc::channel(16);

    let board = Bzm2Board::new(config, state_tx, command_rx);
    let registration = super::BoardRegistration {
        state_rx,
        command_tx: Some(command_tx),
    };
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
    use crate::asic::bzm2::protocol::{OPCODE_UART_NOOP, encode_noop, encode_write_register};
    use nix::pty::openpty;
    use std::fs;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn spawn_chain_emulator(
        master: std::os::fd::OwnedFd,
        chain_len: u8,
        start_id: u8,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let mut file = fs::File::from(master);
            for offset in 0..chain_len {
                let mut noop_request =
                    vec![0u8; encode_noop(crate::asic::bzm2::DEFAULT_ASIC_ID).len()];
                file.read_exact(&mut noop_request).unwrap();
                assert_eq!(
                    noop_request,
                    encode_noop(crate::asic::bzm2::DEFAULT_ASIC_ID)
                );
                file.write_all(&[
                    crate::asic::bzm2::DEFAULT_ASIC_ID,
                    OPCODE_UART_NOOP,
                    b'B',
                    b'Z',
                    b'2',
                ])
                .unwrap();

                let assigned = start_id.saturating_add(offset);
                let expected_write = encode_write_register(
                    crate::asic::bzm2::DEFAULT_ASIC_ID,
                    crate::asic::bzm2::NOTCH_REG,
                    0x0b,
                    &(assigned as u32).to_le_bytes(),
                );
                let mut write_request = vec![0u8; expected_write.len()];
                file.read_exact(&mut write_request).unwrap();
                assert_eq!(write_request, expected_write);

                let mut assigned_noop = vec![0u8; encode_noop(assigned).len()];
                file.read_exact(&mut assigned_noop).unwrap();
                assert_eq!(assigned_noop, encode_noop(assigned));
                file.write_all(&[assigned, OPCODE_UART_NOOP, b'B', b'Z', b'2'])
                    .unwrap();
            }

            let mut final_probe = vec![0u8; encode_noop(crate::asic::bzm2::DEFAULT_ASIC_ID).len()];
            file.read_exact(&mut final_probe).unwrap();
            assert_eq!(final_probe, encode_noop(crate::asic::bzm2::DEFAULT_ASIC_ID));
        })
    }

    #[tokio::test]
    async fn live_calibration_persists_profile() {
        let pty = openpty(None, None).unwrap();
        let serial_path = fs::read_link(format!("/proc/self/fd/{}", pty.slave.as_raw_fd()))
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let profile_path = std::env::temp_dir().join(format!(
            "bzm2-profile-{}-{}.json",
            std::process::id(),
            unique
        ));
        let rail0_path = std::env::temp_dir().join(format!("bzm2-domain-rail0-{unique}.txt"));
        let rail1_path = std::env::temp_dir().join(format!("bzm2-domain-rail1-{unique}.txt"));

        let config = Bzm2VirtualDeviceConfig {
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
                rail_set_paths: vec![
                    rail0_path.to_string_lossy().into_owned(),
                    rail1_path.to_string_lossy().into_owned(),
                ],
                rail_write_scales: vec![1000.0, 1000.0],
                ..Default::default()
            },
            calibration: Bzm2CalibrationConfig {
                enabled: true,
                asics_per_bus: vec![2],
                asics_per_domain: vec![1],
                domain_voltage_offsets_mv: vec![0, 100],
                profile_path: Some(profile_path.clone()),
                skip_lock_check: true,
                ..Default::default()
            },
        };
        let (state_tx, _state_rx) = watch::channel(BoardState {
            name: "bzm2-test".into(),
            model: "BZM2".into(),
            serial: Some("bzm2-test".into()),
            ..Default::default()
        });
        let board = Bzm2Board::new(config, state_tx, mpsc::channel(1).1);
        let bus_layouts = board.resolve_bus_layouts().await.unwrap();

        board.execute_live_calibration(&bus_layouts).await.unwrap();

        let profile = load_saved_operating_point_profile(Some(&profile_path))
            .unwrap()
            .unwrap();
        assert_eq!(profile.saved_state.per_asic_pll_mhz.len(), 2);
        assert_eq!(profile.saved_state.per_domain_voltage_mv.len(), 2);
        assert_eq!(
            fs::read_to_string(&rail0_path).unwrap().trim(),
            profile
                .saved_state
                .per_domain_voltage_mv
                .get(&0)
                .unwrap()
                .to_string()
        );
        assert_eq!(
            fs::read_to_string(&rail1_path).unwrap().trim(),
            profile
                .saved_state
                .per_domain_voltage_mv
                .get(&1)
                .unwrap()
                .to_string()
        );
        assert!(profile.persisted.is_some());

        let _ = fs::remove_file(profile_path);
        let _ = fs::remove_file(rail0_path);
        let _ = fs::remove_file(rail1_path);
        drop(pty);
    }

    #[tokio::test]
    async fn stored_profile_replays_on_restart_without_rewrite() {
        let pty = openpty(None, None).unwrap();
        let serial_path = fs::read_link(format!("/proc/self/fd/{}", pty.slave.as_raw_fd()))
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let profile_path = std::env::temp_dir().join(format!(
            "bzm2-replay-{}-{}.json",
            std::process::id(),
            unique
        ));
        let rail0_path = std::env::temp_dir().join(format!("bzm2-replay-rail0-{unique}.txt"));
        let rail1_path = std::env::temp_dir().join(format!("bzm2-replay-rail1-{unique}.txt"));
        let persisted = Bzm2PersistedCalibrationProfile {
            schema_version: Bzm2PersistedCalibrationProfile::SCHEMA_VERSION,
            operating_class: operating_class_name(Bzm2OperatingClass::Generic).into(),
            performance_mode: performance_mode_name(Bzm2PerformanceMode::Standard).into(),
            asics_per_bus: vec![2],
            pll_post1_divider: DEFAULT_CALIBRATION_POST1_DIVIDER,
            saved_state: Bzm2SavedOperatingPoint {
                board_voltage_mv: 17_500,
                board_throughput_ths: 80.0,
                per_domain_voltage_mv: BTreeMap::from([(0, 17_450), (1, 17_600)]),
                per_asic_pll_mhz: BTreeMap::from([
                    (0, [1_100.0, 1_125.0]),
                    (1, [1_150.0, 1_175.0]),
                ]),
            },
        };
        let original = serde_json::to_string_pretty(&persisted).unwrap();
        fs::write(&profile_path, &original).unwrap();

        let config = Bzm2VirtualDeviceConfig {
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
                rail_set_paths: vec![
                    rail0_path.to_string_lossy().into_owned(),
                    rail1_path.to_string_lossy().into_owned(),
                ],
                rail_write_scales: vec![1000.0, 1000.0],
                ..Default::default()
            },
            calibration: Bzm2CalibrationConfig {
                enabled: true,
                apply_saved_operating_point: true,
                asics_per_bus: vec![2],
                profile_path: Some(profile_path.clone()),
                skip_lock_check: true,
                ..Default::default()
            },
        };
        let (state_tx, _state_rx) = watch::channel(BoardState {
            name: "bzm2-test".into(),
            model: "BZM2".into(),
            serial: Some("bzm2-test".into()),
            ..Default::default()
        });
        let board = Bzm2Board::new(config, state_tx, mpsc::channel(1).1);
        let bus_layouts = board.resolve_bus_layouts().await.unwrap();

        board.execute_live_calibration(&bus_layouts).await.unwrap();

        assert_eq!(fs::read_to_string(&profile_path).unwrap(), original);
        assert_eq!(fs::read_to_string(&rail0_path).unwrap().trim(), "17450");
        assert_eq!(fs::read_to_string(&rail1_path).unwrap().trim(), "17600");

        let _ = fs::remove_file(profile_path);
        let _ = fs::remove_file(rail0_path);
        let _ = fs::remove_file(rail1_path);
        drop(pty);
    }

    #[test]
    fn build_bus_layouts_assigns_global_ranges() {
        let layouts = build_bus_layouts(&["/dev/ttyUSB0".into(), "/dev/ttyUSB1".into()], &[4, 6]);
        assert_eq!(layouts[0].asic_start, 0);
        assert_eq!(layouts[0].asic_count, 4);
        assert_eq!(layouts[1].asic_start, 4);
        assert_eq!(layouts[1].asic_count, 6);
    }

    #[tokio::test]
    async fn resolve_bus_layouts_uses_startup_enumeration_counts() {
        let pty = openpty(None, None).unwrap();
        let master = pty.master;
        let slave = pty.slave;
        let serial_path = fs::read_link(format!("/proc/self/fd/{}", slave.as_raw_fd()))
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let emulator = spawn_chain_emulator(master, 2, 0);

        let config = Bzm2VirtualDeviceConfig {
            serial_paths: vec![serial_path],
            baud_rate: DEFAULT_BAUD_RATE,
            timestamp_count: crate::asic::bzm2::protocol::DEFAULT_TIMESTAMP_COUNT,
            nonce_gap: crate::asic::bzm2::protocol::DEFAULT_NONCE_GAP,
            dispatch_interval: Duration::from_millis(50),
            nominal_hashrate_ths: DEFAULT_NOMINAL_HASHRATE_THS,
            dts_vs_generation: crate::asic::bzm2::protocol::DtsVsGeneration::Gen2,
            telemetry: Bzm2TelemetryConfig::default(),
            enumeration: Bzm2EnumerationConfig {
                enabled: true,
                start_id: 0,
                max_asics_per_bus: vec![4],
            },
            bringup: Bzm2BringupConfig::default(),
            calibration: Bzm2CalibrationConfig::default(),
        };
        let (state_tx, _state_rx) = watch::channel(BoardState {
            name: "bzm2-test".into(),
            model: "BZM2".into(),
            serial: Some("bzm2-test".into()),
            ..Default::default()
        });
        let board = Bzm2Board::new(config, state_tx, mpsc::channel(1).1);

        let layouts = board.resolve_bus_layouts().await.unwrap();
        assert_eq!(layouts.len(), 1);
        assert_eq!(layouts[0].asic_count, 2);

        emulator.join().unwrap();
    }

    #[tokio::test]
    async fn resolve_bus_layouts_falls_back_to_configured_counts_when_default_id_is_silent() {
        let pty = openpty(None, None).unwrap();
        let master = pty.master;
        let slave = pty.slave;
        let serial_path = fs::read_link(format!("/proc/self/fd/{}", slave.as_raw_fd()))
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let emulator = std::thread::spawn(move || {
            let mut file = fs::File::from(master);
            let mut probe = vec![0u8; encode_noop(crate::asic::bzm2::DEFAULT_ASIC_ID).len()];
            file.read_exact(&mut probe).unwrap();
            assert_eq!(probe, encode_noop(crate::asic::bzm2::DEFAULT_ASIC_ID));
            file.write_all(&[
                crate::asic::bzm2::DEFAULT_ASIC_ID,
                OPCODE_UART_NOOP,
                b'N',
                b'O',
                b'P',
            ])
            .unwrap();
        });

        let config = Bzm2VirtualDeviceConfig {
            serial_paths: vec![serial_path],
            baud_rate: DEFAULT_BAUD_RATE,
            timestamp_count: crate::asic::bzm2::protocol::DEFAULT_TIMESTAMP_COUNT,
            nonce_gap: crate::asic::bzm2::protocol::DEFAULT_NONCE_GAP,
            dispatch_interval: Duration::from_millis(50),
            nominal_hashrate_ths: DEFAULT_NOMINAL_HASHRATE_THS,
            dts_vs_generation: crate::asic::bzm2::protocol::DtsVsGeneration::Gen2,
            telemetry: Bzm2TelemetryConfig::default(),
            enumeration: Bzm2EnumerationConfig {
                enabled: true,
                start_id: 0,
                max_asics_per_bus: vec![4],
            },
            bringup: Bzm2BringupConfig::default(),
            calibration: Bzm2CalibrationConfig {
                asics_per_bus: vec![3],
                ..Default::default()
            },
        };
        let (state_tx, _state_rx) = watch::channel(BoardState {
            name: "bzm2-test".into(),
            model: "BZM2".into(),
            serial: Some("bzm2-test".into()),
            ..Default::default()
        });
        let board = Bzm2Board::new(config, state_tx, mpsc::channel(1).1);

        let layouts = board.resolve_bus_layouts().await.unwrap();
        assert_eq!(layouts.len(), 1);
        assert_eq!(layouts[0].asic_count, 3);

        emulator.join().unwrap();
    }

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

        let config = Bzm2VirtualDeviceConfig {
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
                plan: Bzm2BringupPlan {
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
        let (state_tx, _state_rx) = watch::channel(BoardState {
            name: "bzm2-test".into(),
            model: "BZM2".into(),
            serial: Some("bzm2-test".into()),
            ..Default::default()
        });
        let mut board = Bzm2Board::new(config, state_tx, mpsc::channel(1).1);

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

        let config = Bzm2VirtualDeviceConfig {
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
        let (state_tx, state_rx) = watch::channel(BoardState {
            name: "bzm2-test".into(),
            model: "BZM2".into(),
            serial: Some("bzm2-test".into()),
            ..Default::default()
        });
        let mut board = Bzm2Board::new(config, state_tx, mpsc::channel(1).1);

        let _threads = board.create_hash_threads().await.unwrap();
        let state = state_rx.borrow().clone();
        assert!(state.temperatures.iter().any(|sensor| {
            sensor.name == "rail0-regulator"
                && sensor
                    .temperature_c
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
            enumeration: Bzm2EnumerationConfig::default(),
            bringup: Bzm2BringupConfig::default(),
            calibration: Bzm2CalibrationConfig::default(),
        };
        let (state_tx, mut state_rx) = watch::channel(BoardState {
            name: "bzm2-test".into(),
            model: "BZM2".into(),
            serial: Some("bzm2-test".into()),
            ..Default::default()
        });
        let mut board = Bzm2Board::new(config, state_tx, mpsc::channel(1).1);

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
    fn publish_thread_telemetry_updates_board_state() {
        let (state_tx, state_rx) = watch::channel(BoardState {
            name: "bzm2-test".into(),
            model: "BZM2".into(),
            serial: Some("bzm2-test".into()),
            temperatures: vec![TemperatureSensor {
                name: "host-board-temp".into(),
                temperature_c: Some(52.0),
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
            &state_tx,
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

        let state = state_rx.borrow().clone();
        assert_eq!(state.temperatures.len(), 2);
        assert!(
            state.temperatures.iter().any(
                |sensor| sensor.name == "host-board-temp" && sensor.temperature_c == Some(52.0)
            )
        );
        assert!(state.temperatures.iter().any(
            |sensor| sensor.name == "ttyUSB0-asic-2-dts" && sensor.temperature_c == Some(64.5)
        ));
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
