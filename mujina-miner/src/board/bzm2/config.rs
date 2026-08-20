//! Environment-driven configuration for the BZM2 board driver.

use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::tuning::blockscale::{Bzm2CalibrationMode, Bzm2OperatingClass, Bzm2PerformanceMode};

use super::bringup::Bzm2BringupConfig;
use super::telemetry::Bzm2TelemetryConfig;

pub(super) const DEFAULT_BAUD_RATE: u32 = 5_000_000;
const DEFAULT_DISPATCH_INTERVAL_MS: u64 = 500;
pub(super) const DEFAULT_NOMINAL_HASHRATE_THS: f64 = 40.0;
pub(super) const DEFAULT_TELEMETRY_INTERVAL_SECS: u64 = 5;
pub(super) const DEFAULT_ASIC_TEMP_SCALE: f32 = 0.001;
pub(super) const DEFAULT_BOARD_TEMP_SCALE: f32 = 0.001;
pub(super) const DEFAULT_FAN_RPM_SCALE: f32 = 1.0;
pub(super) const DEFAULT_FAN_PERCENT_SCALE: f32 = 1.0;
pub(super) const DEFAULT_VOLTAGE_SCALE: f32 = 0.001;
pub(super) const DEFAULT_CURRENT_SCALE: f32 = 0.001;
pub(super) const DEFAULT_POWER_SCALE: f32 = 0.000001;
pub(super) const DEFAULT_CALIBRATION_SITE_TEMP_C: f32 = 20.0;
pub(super) const DEFAULT_CALIBRATION_POST1_DIVIDER: u8 = 0;
const DEFAULT_CALIBRATION_LOCK_TIMEOUT_MS: u64 = 1_000;
const DEFAULT_CALIBRATION_LOCK_POLL_MS: u64 = 100;
pub(super) const DEFAULT_CALIBRATION_REPLAY_FREQ_MHZ: f32 = 800.0;
const DEFAULT_CALIBRATION_ENGINE_DISCOVERY_TDM_PREDIV_RAW: u32 = 0x0f;
const DEFAULT_CALIBRATION_ENGINE_DISCOVERY_TDM_COUNTER: u8 = 16;
const DEFAULT_CALIBRATION_ENGINE_DISCOVERY_TIMEOUT_MS: u64 = 100;
const DEFAULT_RUNTIME_RETUNE_PERSISTENCE_POLLS: u8 = 3;
const DEFAULT_RUNTIME_RETUNE_THERMAL_C: f32 = 85.0;
const DEFAULT_RUNTIME_RETUNE_VOLTAGE_IMBALANCE_MV: u32 = 150;
pub(super) const DEFAULT_ENUMERATION_MAX_ASICS_PER_BUS: u16 = 100;
pub(super) const DEFAULT_BRINGUP_PRE_POWER_MS: u64 = 10;
pub(super) const DEFAULT_BRINGUP_POST_POWER_MS: u64 = 25;
pub(super) const DEFAULT_BRINGUP_RELEASE_RESET_MS: u64 = 25;
pub(super) const DEFAULT_ENGINE_DISCOVERY_TIMEOUT_MS: u64 = 100;

#[derive(Debug, Clone)]
pub struct Bzm2RuntimeConfig {
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

impl Bzm2RuntimeConfig {
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
pub struct Bzm2CalibrationConfig {
    pub enabled: bool,
    pub apply_saved_operating_point: bool,
    pub discover_engine_topology: bool,
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
    pub engine_discovery_tdm_prediv_raw: u32,
    pub engine_discovery_tdm_counter: u8,
    pub engine_discovery_timeout: Duration,
    pub runtime_retune_enabled: bool,
    pub runtime_retune_persistence_polls: u8,
    pub runtime_retune_thermal_c: f32,
    pub runtime_retune_voltage_imbalance_mv: u32,
}

impl Default for Bzm2CalibrationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            apply_saved_operating_point: true,
            discover_engine_topology: true,
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
            engine_discovery_tdm_prediv_raw: DEFAULT_CALIBRATION_ENGINE_DISCOVERY_TDM_PREDIV_RAW,
            engine_discovery_tdm_counter: DEFAULT_CALIBRATION_ENGINE_DISCOVERY_TDM_COUNTER,
            engine_discovery_timeout: Duration::from_millis(
                DEFAULT_CALIBRATION_ENGINE_DISCOVERY_TIMEOUT_MS,
            ),
            runtime_retune_enabled: true,
            runtime_retune_persistence_polls: DEFAULT_RUNTIME_RETUNE_PERSISTENCE_POLLS,
            runtime_retune_thermal_c: DEFAULT_RUNTIME_RETUNE_THERMAL_C,
            runtime_retune_voltage_imbalance_mv: DEFAULT_RUNTIME_RETUNE_VOLTAGE_IMBALANCE_MV,
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
            discover_engine_topology: env_flag_default_any(
                &[
                    "MUJINA_BZM2_CALIBRATION_DISCOVER_ENGINES",
                    "MUJINA_BZM2_DISCOVER_ENGINES_FOR_CALIBRATION",
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
            engine_discovery_tdm_prediv_raw: env_var_any(&[
                "MUJINA_BZM2_ENGINE_DISCOVERY_TDM_PREDIV_RAW",
                "MUJINA_BZM2_CALIBRATION_ENGINE_DISCOVERY_TDM_PREDIV_RAW",
            ])
            .as_deref()
            .and_then(parse_u32_any_radix)
            .unwrap_or(DEFAULT_CALIBRATION_ENGINE_DISCOVERY_TDM_PREDIV_RAW),
            engine_discovery_tdm_counter: env_var_any(&[
                "MUJINA_BZM2_ENGINE_DISCOVERY_TDM_COUNTER",
                "MUJINA_BZM2_CALIBRATION_ENGINE_DISCOVERY_TDM_COUNTER",
            ])
            .as_deref()
            .and_then(parse_u8_any_radix)
            .unwrap_or(DEFAULT_CALIBRATION_ENGINE_DISCOVERY_TDM_COUNTER),
            engine_discovery_timeout: Duration::from_millis(
                env_var_any(&[
                    "MUJINA_BZM2_ENGINE_DISCOVERY_TIMEOUT_MS",
                    "MUJINA_BZM2_CALIBRATION_ENGINE_DISCOVERY_TIMEOUT_MS",
                ])
                .as_deref()
                .and_then(parse_u64_any_radix)
                .unwrap_or(DEFAULT_CALIBRATION_ENGINE_DISCOVERY_TIMEOUT_MS),
            ),
            runtime_retune_enabled: env_flag_default_any(
                &[
                    "MUJINA_BZM2_RUNTIME_RETUNE",
                    "MUJINA_BZM2_ENABLE_RUNTIME_RETUNE",
                ],
                true,
            ),
            runtime_retune_persistence_polls: env_var_any(&[
                "MUJINA_BZM2_RUNTIME_RETUNE_PERSISTENCE_POLLS",
                "MUJINA_BZM2_RETUNE_PERSISTENCE_POLLS",
            ])
            .as_deref()
            .and_then(parse_u8_any_radix)
            .unwrap_or(DEFAULT_RUNTIME_RETUNE_PERSISTENCE_POLLS),
            runtime_retune_thermal_c: env_f32_any(&[
                "MUJINA_BZM2_RUNTIME_RETUNE_THERMAL_C",
                "MUJINA_BZM2_RETUNE_THERMAL_C",
            ])
            .unwrap_or(DEFAULT_RUNTIME_RETUNE_THERMAL_C),
            runtime_retune_voltage_imbalance_mv: env_var_any(&[
                "MUJINA_BZM2_RUNTIME_RETUNE_VOLTAGE_IMBALANCE_MV",
                "MUJINA_BZM2_RETUNE_VOLTAGE_IMBALANCE_MV",
            ])
            .as_deref()
            .and_then(parse_u32_any_radix)
            .unwrap_or(DEFAULT_RUNTIME_RETUNE_VOLTAGE_IMBALANCE_MV),
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

pub(super) fn env_var_any(keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| env::var(key).ok())
}

pub(super) fn parse_u8_any_radix(raw: &str) -> Option<u8> {
    parse_u64_any_radix(raw).and_then(|value| u8::try_from(value).ok())
}

pub(super) fn parse_u32_any_radix(raw: &str) -> Option<u32> {
    parse_u64_any_radix(raw).and_then(|value| u32::try_from(value).ok())
}

pub(super) fn parse_u64_any_radix(raw: &str) -> Option<u64> {
    let trimmed = raw.trim();
    if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        u64::from_str_radix(hex, 16).ok()
    } else {
        trimmed.parse::<u64>().ok()
    }
}

pub(super) fn env_csv_strings_any(keys: &[&str]) -> Vec<String> {
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

pub(super) fn env_flag(key: &str) -> bool {
    env_var_any(&[key]).as_deref().is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

pub(super) fn env_flag_any(keys: &[&str]) -> bool {
    env_var_any(keys).as_deref().is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

pub(super) fn env_flag_default_any(keys: &[&str], default: bool) -> bool {
    env_var_any(keys)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(default)
}

pub(super) fn env_f32(key: &str) -> Option<f32> {
    env_f32_any(&[key])
}

pub(super) fn env_f32_any(keys: &[&str]) -> Option<f32> {
    env_var_any(keys).and_then(|value| value.parse().ok())
}

pub(super) fn parse_u32(value: &str) -> Option<u32> {
    let trimmed = value.trim();
    if let Some(hex) = trimmed.strip_prefix("0x") {
        u32::from_str_radix(hex, 16).ok()
    } else {
        trimmed.parse().ok()
    }
}

pub(super) fn parse_csv_numbers<T>(key: &str) -> Option<Vec<T>>
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

pub(super) fn parse_csv_numbers_any<T>(keys: &[&str]) -> Option<Vec<T>>
where
    T: std::str::FromStr,
{
    keys.iter().find_map(|key| parse_csv_numbers::<T>(key))
}

pub(super) fn parse_operating_class(value: &str) -> Option<Bzm2OperatingClass> {
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

pub(super) fn parse_performance_mode(value: &str) -> Option<Bzm2PerformanceMode> {
    match value.trim().to_ascii_lowercase().as_str() {
        "max-throughput" | "max_throughput" | "high" | "high-performance" | "high_performance"
        | "performance" => Some(Bzm2PerformanceMode::MaxThroughput),
        "standard" | "balanced" => Some(Bzm2PerformanceMode::Standard),
        "efficiency" | "low" | "low-power" | "low_power" => Some(Bzm2PerformanceMode::Efficiency),
        _ => None,
    }
}

pub(super) fn average_u32(values: impl Iterator<Item = u32>) -> Option<u32> {
    let mut total = 0u64;
    let mut count = 0u64;
    for value in values {
        total += value as u64;
        count += 1;
    }
    (count > 0).then_some((total / count) as u32)
}

pub(super) fn average_f32(values: impl Iterator<Item = f32>) -> Option<f32> {
    let mut total = 0.0f32;
    let mut count = 0usize;
    for value in values {
        total += value;
        count += 1;
    }
    (count > 0).then_some(total / count as f32)
}

pub(super) fn operating_class_name(operating_class: Bzm2OperatingClass) -> &'static str {
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

pub(super) fn performance_mode_name(performance_mode: Bzm2PerformanceMode) -> &'static str {
    match performance_mode {
        Bzm2PerformanceMode::MaxThroughput => "max-throughput",
        Bzm2PerformanceMode::Standard => "standard",
        Bzm2PerformanceMode::Efficiency => "efficiency",
    }
}
