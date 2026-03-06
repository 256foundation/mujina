//! API data transfer objects.
//!
//! These types define the API contract shared between the server and
//! clients (CLI, TUI). See `docs/api.md` (at the repository root)
//! for the full API contract documentation, including conventions
//! for null values and units.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Full miner state snapshot.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct MinerState {
    pub uptime_secs: u64,
    /// Aggregate hashrate in hashes per second.
    pub hashrate: u64,
    pub shares_submitted: u64,
    pub paused: bool,
    pub boards: Vec<BoardState>,
    pub sources: Vec<SourceState>,
}

/// Board status.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct BoardState {
    /// URL-friendly identifier (e.g. "bitaxe-e2f56f9b").
    pub name: String,
    pub model: String,
    pub serial: Option<String>,
    pub fans: Vec<Fan>,
    pub temperatures: Vec<TemperatureSensor>,
    pub powers: Vec<PowerMeasurement>,
    pub threads: Vec<ThreadState>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub asics: Vec<AsicState>,
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
    pub temperature_c: Option<f32>,
}

/// Voltage, current, and power from a single measurement point.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct PowerMeasurement {
    pub name: String,
    pub voltage_v: Option<f32>,
    pub current_a: Option<f32>,
    pub power_w: Option<f32>,
}

/// Per-thread runtime status.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct ThreadState {
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

/// Request body for an explicit BZM2 ASIC DTS/VS query.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Bzm2DtsVsQueryRequest {
    /// Index of the BZM2 UART thread/bus to query.
    pub thread_index: usize,
    /// ASIC id on that UART bus.
    pub asic: u8,
}

/// Request body for an explicit BZM2 ASIC engine-discovery scan.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Bzm2EngineDiscoveryRequest {
    /// Index of the BZM2 UART thread/bus to query.
    pub thread_index: usize,
    /// ASIC id on that UART bus.
    pub asic: u8,
    /// Raw TDM pre-divider value written into `LOCAL_REG_UART_TDM_CTL`.
    pub tdm_prediv_raw: u32,
    /// TDM counter value written into `LOCAL_REG_UART_TDM_CTL`.
    pub tdm_counter: u8,
    /// Optional per-engine probe timeout in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u32>,
}

/// Job source status.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct SourceState {
    pub name: String,
    /// Connection URL (e.g. "stratum+tcp://pool:3333"), if applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Current share difficulty set by the source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub difficulty: Option<f64>,
}
