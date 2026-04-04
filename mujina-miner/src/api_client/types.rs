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

pub type MinerTelemetry = MinerState;

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bzm2_tuning: Option<Bzm2TuningState>,
}

pub type BoardTelemetry = BoardState;

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

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Bzm2SavedOperatingPointStatus {
    #[default]
    Pending,
    Validated,
    Invalidated,
}

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

/// Request body for a live BZM2 NOOP diagnostic query.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Bzm2NoopRequest {
    /// Index of the BZM2 UART thread/bus to query.
    pub thread_index: usize,
    /// ASIC id on that UART bus.
    pub asic: u8,
}

/// Response body for a live BZM2 NOOP diagnostic query.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Bzm2NoopResponse {
    /// Hex-encoded three-byte NOOP payload returned by the ASIC.
    pub payload_hex: String,
}

/// Request body for a live BZM2 loopback diagnostic query.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Bzm2LoopbackRequest {
    /// Index of the BZM2 UART thread/bus to query.
    pub thread_index: usize,
    /// ASIC id on that UART bus.
    pub asic: u8,
    /// Hex-encoded payload to round-trip through the ASIC loopback opcode.
    pub payload_hex: String,
}

/// Response body for a live BZM2 loopback diagnostic query.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Bzm2LoopbackResponse {
    /// Hex-encoded payload returned by the ASIC.
    pub payload_hex: String,
}

/// Request body for a live BZM2 register read.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Bzm2RegisterReadRequest {
    /// Index of the BZM2 UART thread/bus to query.
    pub thread_index: usize,
    /// ASIC id on that UART bus.
    pub asic: u8,
    /// Engine or local-register address.
    pub engine_address: u16,
    /// Register offset within the selected engine or local block.
    pub offset: u8,
    /// Number of bytes to read.
    pub count: u8,
}

/// Response body for a live BZM2 register read.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Bzm2RegisterReadResponse {
    /// Hex-encoded register payload.
    pub value_hex: String,
}

/// Request body for a live BZM2 register write.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Bzm2RegisterWriteRequest {
    /// Index of the BZM2 UART thread/bus to query.
    pub thread_index: usize,
    /// ASIC id on that UART bus.
    pub asic: u8,
    /// Engine or local-register address.
    pub engine_address: u16,
    /// Register offset within the selected engine or local block.
    pub offset: u8,
    /// Hex-encoded bytes to write.
    pub value_hex: String,
}

/// Response body for a live BZM2 register write.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Bzm2RegisterWriteResponse {
    /// Number of bytes written to the requested register.
    pub bytes_written: usize,
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

/// Request body for a live BZM2 clock-report query.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Bzm2ClockReportRequest {
    /// Index of the BZM2 UART thread/bus to query.
    pub thread_index: usize,
    /// ASIC id on that UART bus.
    pub asic: u8,
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

pub type SourceTelemetry = SourceState;
