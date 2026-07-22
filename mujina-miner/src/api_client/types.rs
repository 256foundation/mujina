//! API data transfer objects.
//!
//! These types define the API contract shared between the server and
//! clients (CLI, TUI). See `docs/api.md` (at the repository root)
//! for the full API contract documentation, including conventions
//! for null values and units.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::types::Temperature;

/// Full miner telemetry snapshot.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct MinerTelemetry {
    pub uptime_secs: u64,
    /// Aggregate hashrate in hashes per second.
    pub hashrate: u64,
    pub shares_submitted: u64,
    pub paused: bool,
    pub boards: Vec<BoardTelemetry>,
    pub sources: Vec<SourceTelemetry>,
}

/// Board telemetry snapshot.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct BoardTelemetry {
    /// URL-friendly identifier (e.g. "bitaxe-e2f56f9b").
    pub name: String,
    pub model: String,
    pub serial: Option<String>,
    pub fans: Vec<Fan>,
    pub temperatures: Vec<TemperatureSensor>,
    pub powers: Vec<PowerMeasurement>,
    pub threads: Vec<ThreadTelemetry>,
    /// Per-ASIC topology/diagnostics state (multi-ASIC boards only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub asics: Vec<AsicState>,
    /// BZM2 runtime tuning state (BZM2 boards only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bzm2_tuning: Option<Bzm2TuningState>,
}

/// Fan status.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Fan {
    pub name: String,
    /// Measured RPM, or null if the tachometer read failed.
    pub rpm: Option<u32>,
    /// Measured duty cycle, or null if the read failed.
    pub percent: Option<u8>,
    /// Target duty cycle, or null if the fan is in automatic mode.
    pub target_percent: Option<u8>,
}

/// Temperature sensor reading.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct TemperatureSensor {
    pub name: String,
    #[serde(rename = "temperature_c")]
    #[schema(value_type = Option<f32>)]
    pub temperature: Option<Temperature>,
}

/// Voltage, current, and power from a single measurement point.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct PowerMeasurement {
    pub name: String,
    pub voltage_v: Option<f32>,
    pub current_a: Option<f32>,
    pub power_w: Option<f32>,
}

/// Per-thread telemetry.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct ThreadTelemetry {
    pub name: String,
    /// Hashrate in hashes per second.
    pub hashrate: u64,
    pub is_active: bool,
}

/// Per-ASIC runtime topology or diagnostics state.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct AsicState {
    pub id: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discovered_engine_count: Option<u16>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_engines: Vec<EngineCoordinate>,
}

/// Physical engine coordinate on one ASIC.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema, PartialEq, Eq)]
pub struct EngineCoordinate {
    pub row: u8,
    pub col: u8,
}

/// BZM2 runtime tuning measurements derived from live mining operation.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct Bzm2TuningState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub board_throughput_hs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reuse_saved_operating_point: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub needs_retune: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desired_voltage_mv: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desired_clock_mhz: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desired_accept_ratio: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retune_pending: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retune_reasons: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saved_operating_point_status: Option<Bzm2SavedOperatingPointStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub saved_operating_point_reasons: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub planner_notes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub domains: Vec<Bzm2DomainTuningState>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub asics: Vec<Bzm2AsicTuningState>,
}

/// Validation status of a saved BZM2 operating point.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Bzm2SavedOperatingPointStatus {
    #[default]
    Pending,
    Validated,
    Invalidated,
}

/// How a BZM2 board reached its current operating point at startup.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Bzm2StartupPath {
    SavedReplay,
    LiveCalibration,
}

/// Per-domain live tuning measurement.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Bzm2DomainTuningState {
    pub domain_id: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rail_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_voltage_mv: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measured_voltage_mv: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measured_power_w: Option<f32>,
}

/// Per-PLL live tuning measurement.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Bzm2PllTuningState {
    pub pll_index: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_mhz: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub throughput_hs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pass_rate: Option<f32>,
}

/// Per-ASIC live tuning measurement.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Bzm2AsicTuningState {
    pub id: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_engine_count: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub throughput_hs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub average_pass_rate: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scheduler_share_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plls: Vec<Bzm2PllTuningState>,
}

/// Per-bus BZM2 chain layout summary.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Bzm2BusSummary {
    pub thread_index: usize,
    pub serial_path: String,
    pub asic_start: u16,
    pub asic_count: u16,
}

/// Current BZM2 chain summary for a live board.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Bzm2ChainSummaryResponse {
    pub total_asics: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup_path: Option<Bzm2StartupPath>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saved_operating_point_status: Option<Bzm2SavedOperatingPointStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub buses: Vec<Bzm2BusSummary>,
}

/// One PLL status block in a BZM2 clock report.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Bzm2PllClockStatus {
    pub enable_register: u32,
    pub misc_register: u32,
    pub enabled: bool,
    pub locked: bool,
}

/// One DLL status block in a BZM2 clock report.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Bzm2DllClockStatus {
    pub control2: u8,
    pub control5: u8,
    pub coarsecon: u8,
    pub fincon: u8,
    pub freeze_valid: bool,
    pub locked: bool,
    pub fincon_valid: bool,
}

/// Response body for a live BZM2 clock-report query.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Bzm2ClockReportResponse {
    pub asic: u8,
    pub pll0: Bzm2PllClockStatus,
    pub pll1: Bzm2PllClockStatus,
    pub dll0: Bzm2DllClockStatus,
    pub dll1: Bzm2DllClockStatus,
}

/// Writable fields for `PATCH /api/v0/miner`.
///
/// All fields are optional; only those present in the request body are
/// applied. Read-only fields like `uptime_secs` and `hashrate` are not
/// included and cannot be set.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct MinerPatchRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paused: Option<bool>,
}

/// Request body for setting a fan's target duty cycle.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct SetFanTargetRequest {
    /// Target duty cycle percentage (0--100), or null for automatic control.
    pub target_percent: Option<u8>,
}

/// Job source telemetry.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct SourceTelemetry {
    pub name: String,
    /// Connection URL (e.g. "stratum+tcp://pool:3333"), if applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Current share difficulty set by the source.
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_opt_f64_as_integer_when_whole"
    )]
    pub difficulty: Option<f64>,
}

/// Serialize an `Option<f64>` so that whole numbers appear without a
/// fractional part (e.g. `2328` instead of `2328.0`).
fn serialize_opt_f64_as_integer_when_whole<S: serde::Serializer>(
    value: &Option<f64>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match value {
        None => serializer.serialize_none(),
        Some(v) if v.fract() == 0.0 && v.is_finite() => serializer.serialize_i64(*v as i64),
        Some(v) => serializer.serialize_f64(*v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_difficulty_serializes_as_integer() {
        let source = SourceTelemetry {
            difficulty: Some(2048.0),
            ..Default::default()
        };
        let json: serde_json::Value = serde_json::to_value(&source).unwrap();
        assert!(
            json["difficulty"].is_u64(),
            "expected integer, got {}",
            json["difficulty"]
        );
    }

    #[test]
    fn fractional_difficulty_serializes_as_float() {
        let source = SourceTelemetry {
            difficulty: Some(2048.5),
            ..Default::default()
        };
        let json: serde_json::Value = serde_json::to_value(&source).unwrap();
        assert!(
            json["difficulty"].is_f64(),
            "expected float, got {}",
            json["difficulty"]
        );
    }
}
