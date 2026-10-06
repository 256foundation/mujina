//! API data transfer objects.
//!
//! These types define the API contract shared between the server and
//! clients (CLI, TUI). See `docs/api.md` (at the repository root)
//! for the full API contract documentation, including conventions
//! for null values and units.

use std::time::Instant;

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
    /// Windowed J/TH, one entry per power domain the board can measure.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub efficiency: Vec<EfficiencyReport>,
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
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct TemperatureSensor {
    pub name: String,
    #[serde(rename = "temperature_c")]
    #[schema(value_type = Option<f32>)]
    pub temperature: Option<Temperature>,
    /// When this reading was taken, on the monotonic clock of the process that
    /// observed it.
    ///
    /// WITHOUT THIS, A SENSOR THAT FROZE AN HOUR AGO COUNTS EXACTLY LIKE ONE
    /// ANSWERING NOW. The reasoning was already written down for
    /// [`AsicState::observed_at`] and simply never applied here, and the
    /// consequence is worse for temperatures than for fault bits: board state
    /// keeps every row it has ever seen -- nothing prunes -- so a die whose
    /// chain stopped publishing holds its last value for the life of the
    /// process, and a ceiling evaluated against it is evaluated against a
    /// number that stopped being a measurement.
    ///
    /// `None` is "unknown", and a reader deciding safety must treat unknown as
    /// stale rather than as fresh. Monotonic rather than wall time: wall time
    /// can step, and an age measured across a step is not a measurement. Never
    /// serialised, because an `Instant` names an instant only inside the
    /// process that took it.
    #[serde(skip)]
    #[schema(ignore)]
    pub observed_at: Option<Instant>,
}

/// Voltage, current, and power from a single measurement point.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct PowerMeasurement {
    pub name: String,
    pub voltage_v: Option<f32>,
    pub current_a: Option<f32>,
    pub power_w: Option<f32>,
}

/// Energy efficiency for one power domain, over several lookback windows.
///
/// A miner has more than one J/TH and the differences between them are the
/// diagnostics: `board` minus `asic` is the vampire load, `wall` minus `board`
/// is what the power supply costs. Quoting a single figure without saying where
/// it was measured overstates the miner.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct EfficiencyReport {
    /// Where in the delivery chain the power was measured: `asic`, `board`, or
    /// `wall`.
    pub domain: String,
    pub windows: Vec<EfficiencyWindow>,
}

/// Efficiency over one lookback window.
///
/// A window appears only once the miner has run long enough to fill it, so a
/// freshly started board reports the short windows and grows into the long
/// ones. An absent window means "not yet", never "zero".
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct EfficiencyWindow {
    pub window_secs: u64,
    pub joules_per_terahash: f32,
    /// `measured` if every power sample in the window came from a meter,
    /// `estimated` if any was modelled or apportioned. A control loop must not
    /// act on `estimated`.
    pub provenance: String,
    /// Average hashrate over the same window, in hashes per second. The
    /// denominator the J/TH figure was computed against.
    pub hashrate: Option<u64>,
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
///
/// One row per ASIC, so everything known about that ASIC has one home: the
/// engines it was discovered with, the fault bits it last reported, and when
/// it last said anything. Splitting "what it reported" from "when it
/// reported" across two structures would make them two facts to keep in
/// step, and the pair is only meaningful together -- a fault bit with no
/// arrival time cannot be told from one asserted an hour ago.
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
    /// The fault bits this ASIC last reported, or null where none can be
    /// believed.
    ///
    /// Null is "not available", NOT "none asserted": a chain that has not
    /// spoken, a generation whose frames carry no fault bits, and an ASIC
    /// whose last frame was mis-parsed must none of them read as a healthy
    /// one. All-false is the measured claim that none were asserted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub faults: Option<AsicFaultBits>,
    /// When this row's telemetry last arrived, on the monotonic clock of the
    /// process that observed it.
    ///
    /// Monotonic rather than wall time: wall time can step (NTP, an RTC
    /// catching up at boot) and an age measured across a step is not a
    /// measurement. Never serialised, because an `Instant` names an instant
    /// only inside the process that took it; what crosses the wire is the
    /// AGE computed from this at request time, by the reader that holds the
    /// same clock. A wall-clock stamp here would be a second, steppable copy
    /// of a fact this field already holds.
    #[serde(skip)]
    #[schema(ignore)]
    pub observed_at: Option<Instant>,
}

/// The fault bits one ASIC reported alongside its readings.
///
/// Four independent bits rather than one "faulted" flag: a thermal trip and
/// a voltage shutdown demand different responses, and a summary that could
/// only say "something is wrong" would send an operator back to the raw
/// stream to find out which.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, ToSchema, PartialEq, Eq)]
pub struct AsicFaultBits {
    pub thermal_trip: bool,
    pub thermal_fault: bool,
    pub voltage_fault: bool,
    pub voltage_shutdown: bool,
}

impl AsicFaultBits {
    /// Is any bit asserted?
    pub fn any(&self) -> bool {
        self.thermal_trip || self.thermal_fault || self.voltage_fault || self.voltage_shutdown
    }
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

/// Outcome tally for the result frames one ASIC has returned.
///
/// `decoded` is the number of frames that reconstructed to a block header,
/// accepted or not; the remaining fields partition every frame by why it
/// was, or was not, forwarded as a share.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, ToSchema, PartialEq, Eq)]
pub struct Bzm2ResultCounts {
    pub decoded: u64,
    pub accepted: u64,
    pub invalid_nonce: u64,
    pub unknown_engine: u64,
    pub no_dispatch: u64,
    pub stale_sequence: u64,
    pub below_target: u64,
}

impl From<crate::asic::bzm2::Bzm2ResultCounters> for Bzm2ResultCounts {
    fn from(counters: crate::asic::bzm2::Bzm2ResultCounters) -> Self {
        Self {
            decoded: counters.decoded,
            accepted: counters.accepted,
            invalid_nonce: counters.invalid_nonce,
            unknown_engine: counters.unknown_engine,
            no_dispatch: counters.no_dispatch,
            stale_sequence: counters.stale_sequence,
            below_target: counters.below_target,
        }
    }
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub results: Option<Bzm2ResultCounts>,
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

/// Per-ASIC thermal and voltage summary for a live BZM2 board.
///
/// Every figure here is computed, at the moment of the request, from the
/// per-ASIC readings the board already holds. Nothing in this response is
/// accumulated, cached or carried forward, which has two visible
/// consequences:
///
/// - The board-wide block is computed from **every per-ASIC reading**, not
///   from the per-bus blocks beside it. A mean of three bus means weights a
///   one-ASIC bus like a hundred-ASIC one, and the two answers differ
///   whenever the buses differ in size.
/// - An aggregate over an empty set is `null`, never `0` and never `NaN`.
///   Zero is a measurement; absence is not.
///
/// Read [`Bzm2AsicCoverage`] before reading any mean. A mean over 40 of 100
/// ASICs is not the board's mean, and the coverage counts are the only way
/// to tell a full chain from a partial one without a second request.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Bzm2AsicSummaryResponse {
    /// Which DTS/VS stream these figures were read from. `gen1` publishes one
    /// voltage channel per ASIC and no die temperature at all; `gen2`
    /// publishes three channels and a temperature.
    pub dts_vs_generation: Bzm2DtsVsGeneration,
    /// ASICs this board is **configured** for: `MUJINA_BZM2_ASICS_PER_BUS`
    /// summed, reflected straight back. Never a measurement, always present.
    ///
    /// Compare it with `board.coverage.asics_configured`, which is the chain
    /// the hash threads were actually handed. The two are the same number
    /// only when startup enumeration was off or found everything. When
    /// enumeration ran and found 40 of 100, the coverage block below counts
    /// 40 of 40 -- a full, healthy chain -- and this field is the only thing
    /// in the response that still says 100.
    pub configured_asics: u16,
    /// ASICs startup enumeration found, summed over the buses.
    ///
    /// `None` means nobody asked the hardware (enumeration disabled, the
    /// default) -- **not** "the same as configured". `Some(n)` with `n <
    /// configured_asics` is a chain that came up short, and every aggregate
    /// below is over that shorter chain.
    pub discovered_asics: Option<u16>,
    /// Board-wide figures, computed directly from every per-ASIC reading.
    pub board: Bzm2AsicAggregate,
    /// One block per bus, in thread order.
    ///
    /// Serialised even when empty: `[]` is "this board resolved no bus at
    /// all", which is a thing worth seeing, and an absent key reads as a
    /// field that does not apply.
    #[serde(default)]
    pub buses: Vec<Bzm2BusAsicSummary>,
    /// Per-ASIC current draw.
    pub per_asic_current: Bzm2MeasurementNote,
    /// Per-ASIC power.
    pub per_asic_power: Bzm2MeasurementNote,
}

/// One bus's share of a [`Bzm2AsicSummaryResponse`].
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Bzm2BusAsicSummary {
    pub thread_index: usize,
    pub serial_path: String,
    /// Board-wide id of this bus's first ASIC; the ids in `aggregate` are
    /// numbered from here.
    pub asic_start: u16,
    /// Whether the hash thread driving this bus is working. Null when the
    /// board holds no thread slot for it. A dark bus with `thread_active:
    /// false` is a stopped thread; a dark bus with `true` is a chain that
    /// has gone quiet underneath a running one.
    pub thread_active: Option<bool>,
    pub aggregate: Bzm2AsicAggregate,
}

/// Computed per-ASIC figures over one set of ASICs (one bus, or the board).
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Bzm2AsicAggregate {
    /// How much of the chain these figures actually rest on. Read first.
    pub coverage: Bzm2AsicCoverage,
    /// Die temperature across the ASICs that reported one. Null when none
    /// did -- including every gen1 chain, which publishes no die
    /// temperature.
    pub die_temperature_c: Option<Bzm2ReadingStats>,
    /// The hottest and coldest reporting die. Null when no ASIC reported a
    /// temperature. On a tie the lower board-wide `asic_id` is named, so
    /// repeated polls of an unchanged chain return the same ASIC.
    pub hottest: Option<Bzm2AsicExtreme>,
    pub coldest: Option<Bzm2AsicExtreme>,
    /// One entry per voltage channel this generation publishes: three on
    /// gen2, one on gen1. An entry is always present; its `stats` is null
    /// when no ASIC reported that channel. Serialised even when empty, for
    /// the same reason `buses` is: every generation has at least one
    /// channel, so an empty list is a defect and must be visible as one.
    #[serde(default)]
    pub voltage_channels: Vec<Bzm2VoltageChannelStats>,
    /// Fault bits asserted by the ASICs whose readings counted.
    pub faults: Bzm2FaultSummary,
}

/// How much of the configured chain an aggregate was computed from.
///
/// `asics_reporting + asics_stale + asics_value_suppressed +
/// asics_never_seen == asics_configured`, always: every configured ASIC
/// lands in exactly one of these. When `asics_stale` is null nothing was
/// excluded for age and the other three account for the whole chain.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Bzm2AsicCoverage {
    /// ASICs this board addresses on this bus (or in total), from the
    /// resolved chain layout -- the same denominator the hash threads were
    /// handed.
    ///
    /// The resolved layout is **not** always the configured one. Summed over
    /// the buses this equals
    /// `Bzm2AsicSummaryResponse::discovered_asics.unwrap_or(configured_asics)`,
    /// which is asserted by a test rather than left to drift: when startup
    /// enumeration ran and came up short, this is the short count, and a
    /// chain missing sixty ASICs reads as a full one from this field alone.
    pub asics_configured: u16,
    /// ASICs that contributed at least one value to the figures above.
    pub asics_reporting: u16,
    /// ASICs excluded because their reading was older than
    /// `reading_age.max_age_secs`.
    ///
    /// Null, never `0`, when reading ages are not available: nothing was
    /// excluded because nothing could be. See `reading_age`.
    pub asics_stale: Option<u16>,
    /// ASICs that published a reading carrying no value.
    ///
    /// This is the publish-side gate having fired: a die temperature is
    /// published only when the sensor is enabled, the frame says the
    /// reading is valid, and the value is one a die can physically be at.
    /// An ASIC counted here is talking but not measuring, which is a
    /// different fault from one that has gone silent.
    pub asics_value_suppressed: u16,
    /// ASICs the board holds no reading of any kind for.
    pub asics_never_seen: u16,
    /// Freshness of the readings behind the figures above.
    pub reading_age: Bzm2ReadingAge,
}

/// Freshness of the readings an aggregate was computed from.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Bzm2ReadingAge {
    /// `measured` when every reporting ASIC's reading carried an observation
    /// time and the cutoff below was applied. `not_retained` when it did
    /// not: the aggregate then could not be age-filtered, `asics_stale` is
    /// null rather than `0`, and a frozen sensor is indistinguishable from a
    /// live one in these figures.
    pub availability: Bzm2MeasurementAvailability,
    /// The cutoff that would exclude a reading, in seconds. This describes
    /// the rule, not a measurement, so it is reported whether or not it
    /// could be applied.
    pub max_age_secs: f32,
    /// Age of the oldest reading that WAS included. Null when no reading
    /// carried an observation time, or when nothing was included.
    pub oldest_included_secs: Option<f32>,
    /// Why ages are unavailable, when they are.
    pub note: Option<String>,
}

/// Minimum, maximum, mean and spread over a set of readings.
///
/// `spread` is `max - min`. On a series stack it is the number worth
/// watching: the mean can sit exactly on target while one die runs twenty
/// degrees hotter than its neighbour.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Bzm2ReadingStats {
    pub min: f32,
    pub max: f32,
    pub mean: f32,
    pub spread: f32,
    /// How many ASICs this was computed from. Never zero -- a stats block
    /// over nothing is null instead.
    pub samples: u16,
    /// Reporting ASICs whose row for **this** figure carried no value: the
    /// publish-side gate fired on this one sensor while the ASIC went on
    /// answering with its others.
    ///
    /// [`Bzm2AsicCoverage`] counts an ASIC as suppressed only when nothing
    /// it published carried a value, so an ASIC whose die-temperature gate
    /// fired while its rails kept reporting counts there as a healthy,
    /// reporting ASIC -- and it is. This is the figure-level count that says
    /// the mean above rests on `samples` of `samples + this`. A gen2 chain
    /// with the thermal gate firing on sixty of a hundred dice shows `4 of 4
    /// reporting` in coverage and `samples: 40` here.
    ///
    /// Zero here is measured. When every ASIC's sensor was gated there is no
    /// stats block at all and the figure is null, which coverage's
    /// `asics_reporting` then contradicts loudly enough to see.
    ///
    /// This and [`Bzm2AsicCoverage::asics_value_suppressed`] never
    /// double-count: an ASIC that published nothing at all is counted there
    /// and never reaches the reporting set this figure is denominated in.
    /// `samples + asics_value_suppressed <= coverage.asics_reporting`, the
    /// remainder being reporting ASICs that publish no such sensor at all.
    pub asics_value_suppressed: u16,
}

/// One voltage channel's figures across a set of ASICs.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Bzm2VoltageChannelStats {
    /// Channel index as published: `0..3` on gen2, `0` alone on gen1, whose
    /// single channel carries no index on the wire.
    pub channel: u8,
    /// Null when no ASIC in this set reported this channel.
    pub stats: Option<Bzm2ReadingStats>,
}

/// One end of the temperature range, and which ASIC is there.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Bzm2AsicExtreme {
    /// Board-wide ASIC id: unique across buses.
    pub asic_id: u16,
    /// The id this ASIC answers to on its own chain. Every bus is addressed
    /// from the same start id, so a three-bus machine has three "ASIC 7";
    /// `thread_index` plus `wire_asic_id` is what names one physically.
    pub wire_asic_id: u8,
    pub thread_index: usize,
    pub temperature_c: f32,
}

/// Fault bits asserted across a set of ASICs.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Bzm2FaultSummary {
    pub availability: Bzm2MeasurementAvailability,
    /// One entry per fault bit, each with a count that may legitimately be
    /// zero -- a measured "no ASIC is asserting this".
    ///
    /// Null, not `[]`, when the bits could not be read: an empty list would
    /// be indistinguishable from a healthy chain.
    pub bits: Option<Vec<Bzm2FaultBitCount>>,
    /// Why the bits are unavailable, when they are.
    pub note: Option<String>,
}

/// How many ASICs are asserting one fault bit, and which.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Bzm2FaultBitCount {
    pub bit: Bzm2FaultBit,
    pub asics_asserting: u16,
    /// Board-wide ids of the asserting ASICs, capped. A fault on three ASICs
    /// must name them; a fault on all three hundred is a chain-wide event
    /// and further ids add nothing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub asic_ids: Vec<u16>,
    /// How many asserting ids the cap left out. `asics_asserting` is always
    /// the full count.
    pub asic_ids_omitted: u16,
}

/// A fault bit a DTS/VS frame carries.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Bzm2FaultBit {
    ThermalTrip,
    ThermalFault,
    VoltageFault,
    VoltageShutdown,
}

/// Whether a figure beside this flag is a measurement, and if not, why not.
///
/// Three absences that a bare `null` would flatten into one, and that a `0`
/// would hide entirely.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Bzm2MeasurementAvailability {
    /// Computed from readings the board holds.
    Measured,
    /// The hardware reports it, but it is consumed where it arrives and is
    /// not kept in board state, so a query served from memory cannot see it.
    /// A sensor exists; this response cannot reach it.
    NotRetained,
    /// No sensor on this platform produces it at all. No amount of polling
    /// will turn this into a number.
    UnavailableOnPlatform,
}

/// A measurement class this response does not summarise, and why.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Bzm2MeasurementNote {
    pub availability: Bzm2MeasurementAvailability,
    pub note: String,
}

/// Which DTS/VS telemetry generation a chain speaks.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Bzm2DtsVsGeneration {
    Gen1,
    Gen2,
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

/// Request body for a live BZM2 clock-report query.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Bzm2ClockReportRequest {
    /// Index of the BZM2 UART thread/bus to query.
    pub thread_index: usize,
    /// ASIC id on that UART bus.
    pub asic: u8,
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
