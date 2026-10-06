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
use crate::asic::hash_thread::{
    HashThreadAsicObservation, HashThreadStatus, HashThreadTelemetryUpdate,
};
use crate::tuning::calibration_planner::Bzm2SavedEngineTopology;
use crate::types::Temperature;

use super::config::{
    ASSUMED_FAN_TACHO_RPM_SCALE, DEFAULT_ASIC_TEMP_SCALE, DEFAULT_BOARD_TEMP_SCALE,
    DEFAULT_CURRENT_SCALE, DEFAULT_FAN_PERCENT_PATHS, DEFAULT_FAN_PERCENT_SCALE,
    DEFAULT_FAN_RPM_PATHS, DEFAULT_MAX_ASIC_TEMP_C, DEFAULT_MIN_FAN_RPM, DEFAULT_POWER_SCALE,
    DEFAULT_TELEMETRY_INTERVAL_SECS, DEFAULT_VOLTAGE_SCALE, env_csv_strings_any, env_f32,
    parse_csv_numbers_any,
};

#[derive(Debug, Clone, Default)]
pub struct Bzm2TelemetryConfig {
    pub poll_interval: Duration,
    pub asic_temp: Option<SensorSpec>,
    pub board_temp: Option<SensorSpec>,
    /// One entry per physical fan (four on this platform's control board).
    /// Index `i` here and in `fan_percent` describe the same fan; they are
    /// read as parallel arrays rather than paired up front because a fan
    /// with a live tachometer and a dead duty read (or vice versa) is still
    /// worth reporting.
    pub fan_rpm: Vec<SensorSpec>,
    pub fan_percent: Vec<SensorSpec>,
    pub input_voltage: Option<SensorSpec>,
    pub input_current: Option<SensorSpec>,
    pub input_power: Option<SensorSpec>,
    pub max_asic_temp_c: Option<f32>,
    pub max_board_temp_c: Option<f32>,
    pub max_input_power_w: Option<f32>,
    /// A tachometer below this, and not at zero, is a fan that is failing
    /// rather than one that has stopped. Zero needs no threshold.
    pub min_fan_rpm: Option<u32>,
    /// Regulator output ceiling, in volts.
    pub max_rail_v: Option<f32>,
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
            // Plural: this control board has four fans, not one. The old
            // singular path/scale keys are kept as a fallback in the key
            // list so a one-fan override still works, but the platform
            // default is the real four-fan sysfs layout, not an empty list.
            fan_rpm: sensor_specs_from_env_or_default(
                &["MUJINA_BZM2_FAN_RPM_PATHS", "MUJINA_BZM2_FAN_RPM_PATH"],
                &["MUJINA_BZM2_FAN_RPM_SCALES", "MUJINA_BZM2_FAN_RPM_SCALE"],
                ASSUMED_FAN_TACHO_RPM_SCALE,
                &DEFAULT_FAN_RPM_PATHS,
            ),
            fan_percent: sensor_specs_from_env_or_default(
                &[
                    "MUJINA_BZM2_FAN_PERCENT_PATHS",
                    "MUJINA_BZM2_FAN_PERCENT_PATH",
                ],
                &[
                    "MUJINA_BZM2_FAN_PERCENT_SCALES",
                    "MUJINA_BZM2_FAN_PERCENT_SCALE",
                ],
                DEFAULT_FAN_PERCENT_SCALE,
                &DEFAULT_FAN_PERCENT_PATHS,
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
            // A DEFAULT, because a trip nobody configures is a trip that does
            // not exist. Arming the ASIC limit took an environment variable no
            // packaged unit file, config or script in the tree ever set, and
            // the single variable required to mine at all is the serial path --
            // so the ordinary way to run this driver was with no thermal
            // protection and nothing said about it.
            //
            // The value is not invented. It is the die maximum the platform
            // itself enforces, parsed from our own captures and recorded in
            // our own captured safety envelope (vendor_limit column,
            // asic_die_max_c). Defaulting to the limit the machine already
            // holds itself to cannot be more permissive than the machine.
            //
            // The other two stay unset on purpose and are REPORTED as unset
            // (see `unarmed_limits`): board temperature and input power are
            // properties of a chassis, and this file knows about a hashboard.
            // Defaulting them from a three-board system figure would be a
            // number with the wrong denominator, which is worse than none.
            max_asic_temp_c: env_f32("MUJINA_BZM2_MAX_ASIC_TEMP_C")
                .or(Some(DEFAULT_MAX_ASIC_TEMP_C)),
            max_board_temp_c: env_f32("MUJINA_BZM2_MAX_BOARD_TEMP_C"),
            max_input_power_w: env_f32("MUJINA_BZM2_MAX_INPUT_POWER_W"),
            // A DEFAULT, and a safe one to default: the lowest commanded fan
            // speed we have measured on this chassis is ~1,050 rpm at 25 %
            // duty, and a fan whose gate is shut free-runs at ~5,430 rpm
            // (both measured on hardware). 300 rpm is below every state a working
            // fan can be in, so this cannot fire on a healthy machine -- it
            // fires on one that is stalling. Zero rpm needs no threshold and
            // is caught without this.
            min_fan_rpm: env_f32("MUJINA_BZM2_MIN_FAN_RPM")
                .map(|v| v.max(0.0) as u32)
                .or(Some(DEFAULT_MIN_FAN_RPM)),
            // NOT defaulted, and reported as unarmed. We have no measured
            // ceiling for a regulator output, and a number chosen to look
            // reasonable is the same error as a board limit taken from a
            // three-board figure: it would fire on the wrong evidence.
            max_rail_v: env_f32("MUJINA_BZM2_MAX_RAIL_V"),
        }
    }

    /// Limits this board is NOT protected by, for the caller to say out loud.
    ///
    /// `is_enabled()` answers "will this publish telemetry", which is not the
    /// same question as "is anything protecting this board" and was being read
    /// as though it were. Since the fan list gained defaults, `is_enabled()` is
    /// unconditionally true -- so the monitor spawns, publishes fans, and
    /// evaluates trips that may all be unconfigured. Protection that looks
    /// present from the outside is the worst of the three states.
    pub(super) fn unarmed_limits(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.max_asic_temp_c.is_none() {
            out.push("ASIC temperature");
        }
        if self.max_board_temp_c.is_none() {
            out.push("board temperature");
        }
        if self.max_rail_v.is_none() {
            out.push("rail over-voltage");
        }
        // Board current has no entry here because it has no sensor to be
        // unarmed against: opcode 20 and opcode 42 both return rail
        // millivolts, not current, so per-board current is NOT READABLE
        // rather than unconfigured. Listing it as "unset" would imply setting
        // it would help.
        if self.max_input_power_w.is_none() {
            out.push("input power");
        }
        out
    }

    /// Does any trip exist at all? Distinct from [`Self::is_enabled`].
    pub(super) fn any_trip_armed(&self) -> bool {
        self.max_asic_temp_c.is_some()
            || self.max_board_temp_c.is_some()
            || self.max_input_power_w.is_some()
    }

    /// Whether telemetry will be PUBLISHED. Says nothing about protection --
    /// see [`Self::any_trip_armed`] and [`Self::unarmed_limits`].
    pub(super) fn is_enabled(&self) -> bool {
        self.asic_temp.is_some()
            || self.board_temp.is_some()
            || !self.fan_rpm.is_empty()
            || !self.fan_percent.is_empty()
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
        let fan_rpm: Vec<Option<u32>> = self
            .fan_rpm
            .iter()
            .map(|spec| spec.read().map(|v| v.round() as u32))
            .collect();
        let fan_percent: Vec<Option<u8>> = self
            .fan_percent
            .iter()
            .map(|spec| spec.read().map(|v| v.round().clamp(0.0, 100.0) as u8))
            .collect();
        let voltage_v = self.input_voltage.as_ref().and_then(SensorSpec::read);
        let current_a = self.input_current.as_ref().and_then(SensorSpec::read);
        let power_w = self
            .input_power
            .as_ref()
            .and_then(SensorSpec::read)
            .or_else(|| voltage_v.zip(current_a).map(|(v, c)| v * c));

        // One entry per configured fan slot, not per fan that actually
        // answered: a fan whose tachometer read failed still needs to show
        // up as "fanN: rpm unknown" rather than vanish from the list, which
        // is what let a single dead sensor silently look identical to "this
        // board only has one fan".
        let fan_count = fan_rpm.len().max(fan_percent.len());
        let fans = (0..fan_count)
            .map(|index| Fan {
                name: format!("fan{index}"),
                rpm: fan_rpm.get(index).copied().flatten(),
                percent: fan_percent.get(index).copied().flatten(),
                target_percent: None,
            })
            .collect::<Vec<_>>();

        let mut temperatures = Vec::new();
        if self.asic_temp.is_some() || asic_temp.is_some() {
            temperatures.push(TemperatureSensor {
                name: "asic".into(),
                temperature: asic_temp.map(Temperature::from_celsius),
                observed_at: Some(std::time::Instant::now()),
            });
        }
        if self.board_temp.is_some() || board_temp.is_some() {
            temperatures.push(TemperatureSensor {
                name: "board".into(),
                temperature: board_temp.map(Temperature::from_celsius),
                observed_at: Some(std::time::Instant::now()),
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

        let trip_reason = self.trip_reason(board_temp, power_w);
        let blind = self.blind_limits(board_temp, power_w);
        Bzm2TelemetrySnapshot {
            fans,
            temperatures,
            powers,
            trip_reason,
            blind,
        }
    }

    /// Limits that are configured but had no reading this cycle.
    ///
    /// THE TRIP USED TO FAIL OPEN, and this is the missing fact that let it.
    /// `trip_reason` is three `if let (Some(limit), Some(value))` arms over a
    /// bare `None` fall-through, and every value comes from a read that turns
    /// an I/O error, a vanished node, a permissions change or unparseable
    /// content into `None` with no log and no counter. The sole consumer reads
    /// `Some` as "shut everything down" and `None` as "keep mining", so a
    /// sensor that stopped answering and a sensor answering safely were the
    /// same value at the call site: a limit could be configured, its sensor
    /// could disappear, and the machine would mine on with the protection
    /// silently gone.
    ///
    /// Our own rule is the opposite of that -- no sample inside the window is a
    /// TRIP, not a pass -- and it is stated here as a separate fact rather than
    /// folded into `trip_reason` because the two need different policies. An
    /// over-limit reading is true now. Blindness has to persist before it means
    /// anything, or one dropped read halts a 3 kW machine; the monitor holds
    /// that policy because the monitor is what has a loop.
    ///
    /// The die ceiling is absent for the reason given on [`Self::trip_reason`]:
    /// its sensor is not a file, so its blindness is not measurable from here
    /// and the monitor computes it from the DTS rows instead.
    fn blind_limits(&self, _board_temp: Option<f32>, input_power_w: Option<f32>) -> Vec<String> {
        let mut out = Vec::new();
        for (limit, value, what) in [(self.max_input_power_w, input_power_w, "input power")] {
            if limit.is_some() && value.is_none() {
                out.push(what.to_string());
            }
        }
        out
    }

    /// Trips this file can evaluate: the ones whose sensor is a sysfs read.
    ///
    /// THE DIE CEILING IS NOT HERE, deliberately. `max_asic_temp_c` is still
    /// the config home for the number, but the sensor that answers it is the
    /// per-ASIC DTS stream the hash threads publish into `BoardTelemetry`, not
    /// a file. Evaluating it here meant evaluating it against
    /// `MUJINA_BZM2_ASIC_TEMP_PATH` -- a path nothing in this tree sets and
    /// which does not exist on this platform -- so the one limit armed by
    /// default was permanently blind, and every run tripped itself
    /// at 45 s on a board that was never measured. `board/bzm2/abort.rs` owns
    /// every condition computed from published telemetry; this owns the ones
    /// computed from sysfs. A limit lives in exactly one of the two.
    fn trip_reason(&self, _board_temp: Option<f32>, input_power_w: Option<f32>) -> Option<String> {
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
    /// Configured limits with no reading this cycle. Empty is the healthy case;
    /// a non-empty list means protection is ABSENT for those limits, which is
    /// not the same as protection reporting nothing to worry about.
    pub(super) blind: Vec<String>,
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

/// Merge one hash thread's readings into board state, and record that the
/// ASIC they came from spoke.
///
/// `thread_index` is not decoration: ASIC ids are local to a bus, so the row
/// this update belongs to is keyed by the pair, and the thread that produced
/// the update does not know where it sits on the board.
pub(super) fn publish_thread_telemetry(
    telemetry_tx: &watch::Sender<BoardTelemetry>,
    thread_index: usize,
    update: &HashThreadTelemetryUpdate,
) {
    // ONE STAMP PER UPDATE, taken from the ASIC observation when there is one
    // so a die's temperature row and its fault row cannot disagree about when
    // they were seen. `Instant::now()` only when the update carries no
    // observation to borrow from.
    let observed_at = Some(
        update
            .asic
            .as_ref()
            .map(|asic| asic.observed_at)
            .unwrap_or_else(std::time::Instant::now),
    );
    telemetry_tx.send_modify(|state| {
        // Merged straight from the thread's readings instead of through an
        // intermediate vector of API rows. This runs once per DTS/VS frame --
        // measured at 13,622 frames a second across a full chain, on a
        // Cortex-A9 -- and the intermediate cost a name clone per reading
        // (four on a gen2 frame) plus two vector allocations, all of them
        // freed again before the next frame arrived. A name is cloned now
        // only when its row is created, which happens once per ASIC per
        // sensor and never again.
        for reading in &update.temperatures {
            let temperature = reading.temperature_c.map(Temperature::from_celsius);
            match state
                .temperatures
                .iter_mut()
                .find(|sensor| sensor.name == reading.name)
            {
                Some(sensor) => {
                    sensor.temperature = temperature;
                    sensor.observed_at = observed_at;
                }
                None => state.temperatures.push(TemperatureSensor {
                    name: reading.name.clone(),
                    temperature,
                    observed_at,
                }),
            }
        }
        for reading in &update.powers {
            match state
                .powers
                .iter_mut()
                .find(|power| power.name == reading.name)
            {
                Some(power) => {
                    power.voltage_v = reading.voltage_v;
                    power.current_a = reading.current_a;
                    power.power_w = reading.power_w;
                }
                None => state.powers.push(PowerMeasurement {
                    name: reading.name.clone(),
                    voltage_v: reading.voltage_v,
                    current_a: reading.current_a,
                    power_w: reading.power_w,
                }),
            }
        }
        if let Some(observation) = &update.asic {
            record_asic_observation(state, thread_index, observation);
        }
    });
}

/// Record when one ASIC last spoke, and what it reported alongside its
/// readings.
///
/// Kept on the ASIC's own row rather than on each reading: the arrival time
/// and the fault bits are facts about the ASIC, and one frame delivers all
/// of them together. A time per reading would be four copies of one arrival
/// for a gen2 ASIC, and four copies of one fact are four chances to
/// disagree about when it last answered.
///
/// Recorded whatever the value gates upstream decided. Those gates judge
/// what a reading is worth; they do not judge whether the device spoke, and
/// an ASIC answering with a suppressed value is a different fault from one
/// that has gone silent. This field is what separates them: without it a
/// sensor that froze an hour ago counts exactly like one answering now.
///
/// `faults` is assigned, not merged: the latest frame is what this ASIC now
/// reports, and carrying an older frame's bits forward under a newer
/// observation time would date a fault to a frame that never carried it.
/// `None` from a generation that sends no fault bits therefore stays `None`
/// -- unavailable, which is not the same as none asserted.
fn record_asic_observation(
    state: &mut BoardTelemetry,
    thread_index: usize,
    observation: &HashThreadAsicObservation,
) {
    match asic_row_index(&state.asics, thread_index, observation.asic_id) {
        Some(index) => {
            let row = &mut state.asics[index];
            row.observed_at = Some(observation.observed_at);
            row.faults = observation.faults;
        }
        None => {
            state.asics.push(AsicState {
                id: observation.asic_id,
                thread_index: Some(thread_index),
                faults: observation.faults,
                observed_at: Some(observation.observed_at),
                ..Default::default()
            });
            sort_asic_rows(state);
        }
    }
}

/// Where one ASIC's row sits in board state, if it has one yet.
///
/// The key is the (thread index, wire id) PAIR. Ids are local to a bus, so
/// the same id on two buses names two different devices; keying on the id
/// alone would have every bus after the first overwrite the first bus's
/// rows. Both writers here and the summary reader spell the key through this
/// one function, so they cannot come to spell it differently.
fn asic_row_index(asics: &[AsicState], thread_index: usize, asic_id: u8) -> Option<usize> {
    asics
        .iter()
        .position(|asic| asic.thread_index == Some(thread_index) && asic.id == asic_id)
}

/// Keep the rows in bus-then-id order, so a reader sees the chain in the
/// order it is wired.
///
/// Called only where a row is ADDED. An update in place cannot change the
/// order, and this runs on the per-frame telemetry path: re-sorting three
/// hundred rows thousands of times a second to preserve an order that
/// already holds is the kind of cost this platform cannot absorb.
fn sort_asic_rows(state: &mut BoardTelemetry) {
    state
        .asics
        .sort_by_key(|asic| (asic.thread_index.unwrap_or(usize::MAX), asic.id));
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
            // The stamp travels with the reading. Updating the value and
            // leaving the old time would make a fresh reading look stale and,
            // worse, a stale one look fresh the moment anything touched it.
            sensor.observed_at = update.observed_at;
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

pub(super) fn sensor_specs_from_env(
    paths_keys: &[&str],
    scales_keys: &[&str],
    default_scale: f32,
) -> Vec<SensorSpec> {
    build_sensor_specs(env_csv_strings_any(paths_keys), scales_keys, default_scale)
}

/// Like [`sensor_specs_from_env`], but for a fixed set of instances this
/// platform always has (e.g. the four control-board fans): when the paths
/// keys are unset, `default_paths` is used instead of an empty list, so the
/// sensor is live with zero configuration. An explicit path list always
/// replaces the defaults outright rather than merging with them.
pub(super) fn sensor_specs_from_env_or_default(
    paths_keys: &[&str],
    scales_keys: &[&str],
    default_scale: f32,
    default_paths: &[&str],
) -> Vec<SensorSpec> {
    let paths = env_csv_strings_any(paths_keys);
    let paths = if paths.is_empty() {
        default_paths
            .iter()
            .map(|path| (*path).to_owned())
            .collect()
    } else {
        paths
    };
    build_sensor_specs(paths, scales_keys, default_scale)
}

fn build_sensor_specs(
    paths: Vec<String>,
    scales_keys: &[&str],
    default_scale: f32,
) -> Vec<SensorSpec> {
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

/// Parse a number out of a sysfs attribute. ONE HOME, used by every reader.
///
/// THE FAN TACHOMETER DRIVER ON THIS PLATFORM TERMINATES WITH NUL, NOT
/// NEWLINE. Captured from `/sys/class/hwmon/hwmon0/speed` on hardware,
/// 2026-09-22: bytes `[50, 50, 53, 0]` -- "225\0". `cat` prints `225` and a
/// terminal swallows the NUL, so every shell read ever taken looked clean.
/// `str::trim` strips whitespace and NUL is not whitespace, so every parse
/// failed.
///
/// The consequence was total and silent: Mujina has NEVER read a fan speed on
/// this hardware. Every capture of its own API shows `"rpm": null` for all
/// four fans in every run, while the harness -- reading the same nodes from a
/// shell -- recorded 5,640 rpm. So the fan-death and under-speed conditions
/// could never fire, and the fan-blindness condition added on 2026-09-22
/// would have tripped the ladder and de-energised a healthy board on its first
/// energised run.
///
/// Diagnosed late, and worth recording why: the padding hypothesis was
/// considered and wrongly dismissed because `wc -c` returned 4, read as
/// "225\n". "225\0" is also 4 bytes. The check could not tell the two cases
/// apart, so it falsified nothing. What settled it was making the reader
/// report the raw bytes it saw.
pub(super) fn parse_sysfs_number<T: std::str::FromStr>(raw: &str) -> Option<T> {
    let trimmed = raw.trim_matches(|c: char| c.is_whitespace() || c == '\0');
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse().ok()
}

fn parse_scaled_sensor_value(raw: &str, scale: f32) -> Option<f32> {
    parse_sysfs_number::<f32>(raw).map(|value| value * scale)
}

fn upsert_asic_state(
    telemetry_tx: &watch::Sender<BoardTelemetry>,
    thread_index: usize,
    serial_path: &str,
    asic_id: u8,
    active_engine_count: u16,
    missing_engines: Vec<EngineCoordinate>,
) {
    telemetry_tx.send_modify(
        |state| match asic_row_index(&state.asics, thread_index, asic_id) {
            Some(index) => {
                let asic = &mut state.asics[index];
                asic.serial_path = Some(serial_path.to_owned());
                asic.discovered_engine_count = Some(active_engine_count);
                asic.missing_engines = missing_engines.clone();
            }
            None => {
                state.asics.push(AsicState {
                    id: asic_id,
                    thread_index: Some(thread_index),
                    serial_path: Some(serial_path.to_owned()),
                    discovered_engine_count: Some(active_engine_count),
                    missing_engines: missing_engines.clone(),
                    ..Default::default()
                });
                sort_asic_rows(state);
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::api_client::types::{AsicFaultBits, ThreadTelemetry};

    /// THE EXACT BYTES CAPTURED FROM THE RIG. Before this, every fan speed
    /// Mujina ever tried to read on this hardware parsed to None.
    #[test]
    fn a_nul_terminated_sysfs_value_parses() {
        let captured = std::str::from_utf8(&[50, 50, 53, 0]).unwrap();
        assert_eq!(parse_sysfs_number::<u32>(captured), Some(225));
        assert_eq!(parse_scaled_sensor_value(captured, 30.0), Some(6750.0));
        // Both terminators, and neither, still parse; garbage still does not.
        assert_eq!(parse_sysfs_number::<u32>("225\n"), Some(225));
        assert_eq!(parse_sysfs_number::<u32>("225"), Some(225));
        assert_eq!(parse_sysfs_number::<u32>("225\n\0\0"), Some(225));
        assert_eq!(parse_sysfs_number::<u32>("\0"), None);
        assert_eq!(parse_sysfs_number::<u32>("22x5"), None);
    }

    #[test]
    fn parse_scaled_sensor_value_applies_scale() {
        let parsed = parse_scaled_sensor_value("42500\n", 0.001).unwrap();
        assert!((parsed - 42.5).abs() < 0.001);
        assert_eq!(parse_scaled_sensor_value("", 0.001), None);
        assert_eq!(parse_scaled_sensor_value("nope", 1.0), None);
    }

    /// The usable duty range this hardware documents is 11000..=40000 raw
    /// against a fixed period of 40000; these are the two endpoints of that
    /// range, computed from the raw sysfs value through the same scaling
    /// path production reads through (`parse_scaled_sensor_value` with the
    /// real `DEFAULT_FAN_PERCENT_SCALE`), not restated from the scale
    /// constant itself.
    #[test]
    fn fan_duty_scale_converts_range_endpoints_to_percent() {
        let low = parse_scaled_sensor_value("11000", DEFAULT_FAN_PERCENT_SCALE).unwrap();
        assert!(
            (low - 27.5).abs() < 0.01,
            "11000 raw should be 27.5%, got {low}"
        );

        let high = parse_scaled_sensor_value("40000", DEFAULT_FAN_PERCENT_SCALE).unwrap();
        assert!(
            (high - 100.0).abs() < 0.01,
            "40000 raw should be 100%, got {high}"
        );
    }

    /// From the task's own worked example: a captured raw tach count of 177
    /// scaled to RPM should land at 5310, the same figure that motivated
    /// choosing this scale in the first place -- computed here via the raw
    /// count and the constant, not asserted as a bare number.
    #[test]
    fn fan_tacho_scale_applies_assumed_pulses_per_rev() {
        let rpm = parse_scaled_sensor_value("177", ASSUMED_FAN_TACHO_RPM_SCALE).unwrap();
        assert!(
            (rpm - 5310.0).abs() < 0.01,
            "177 raw should be 5310 RPM, got {rpm}"
        );
    }

    #[test]
    fn sensor_specs_from_env_or_default_uses_defaults_when_unset() {
        // Keys deliberately unused anywhere else, so this cannot race a
        // concurrently-running test over real MUJINA_BZM2_* env state.
        let specs = sensor_specs_from_env_or_default(
            &["MUJINA_BZM2_TEST_UNUSED_FAN_PATHS_KEY"],
            &["MUJINA_BZM2_TEST_UNUSED_FAN_SCALES_KEY"],
            2.5,
            &["/sensors/a", "/sensors/b", "/sensors/c"],
        );
        assert_eq!(specs.len(), 3);
        for (spec, expected_path) in specs.iter().zip(["/sensors/a", "/sensors/b", "/sensors/c"]) {
            assert_eq!(spec.path, expected_path);
            assert_eq!(spec.scale, 2.5);
        }
    }

    /// Reads four independently-valued fans through the real
    /// `Bzm2TelemetryConfig::snapshot` path -- distinct raw values per fan,
    /// so a bug that aliased every slot to the first sensor (or to a fixed
    /// "fan" name) would fail this rather than pass by coincidence.
    #[test]
    fn snapshot_reports_one_named_entry_per_configured_fan() {
        let unique = std::process::id();
        let rpm_raw = [100i64, 110, 120, 130]; // -> 3000/3300/3600/3900 RPM
        let duty_raw = [12000i64, 16000, 20000, 24000]; // -> 30/40/50/60 %

        let rpm_paths: Vec<_> = rpm_raw
            .iter()
            .enumerate()
            .map(|(index, raw)| {
                let path = env::temp_dir().join(format!("bzm2-fan-rpm-{unique}-{index}.txt"));
                fs::write(&path, raw.to_string()).unwrap();
                path
            })
            .collect();
        let duty_paths: Vec<_> = duty_raw
            .iter()
            .enumerate()
            .map(|(index, raw)| {
                let path = env::temp_dir().join(format!("bzm2-fan-duty-{unique}-{index}.txt"));
                fs::write(&path, raw.to_string()).unwrap();
                path
            })
            .collect();

        let telemetry = Bzm2TelemetryConfig {
            fan_rpm: rpm_paths
                .iter()
                .map(|path| SensorSpec {
                    path: path.to_string_lossy().into_owned(),
                    scale: ASSUMED_FAN_TACHO_RPM_SCALE,
                })
                .collect(),
            fan_percent: duty_paths
                .iter()
                .map(|path| SensorSpec {
                    path: path.to_string_lossy().into_owned(),
                    scale: DEFAULT_FAN_PERCENT_SCALE,
                })
                .collect(),
            ..Default::default()
        };

        let snapshot = telemetry.snapshot();
        assert_eq!(snapshot.fans.len(), 4);
        for (index, fan) in snapshot.fans.iter().enumerate() {
            assert_eq!(fan.name, format!("fan{index}"));
            assert_eq!(fan.rpm, Some((rpm_raw[index] * 30) as u32));
            assert_eq!(fan.percent, Some((duty_raw[index] * 100 / 40_000) as u8));
        }

        for path in rpm_paths.into_iter().chain(duty_paths) {
            let _ = fs::remove_file(path);
        }
    }

    #[test]
    fn telemetry_trip_detects_thresholds() {
        let telemetry = Bzm2TelemetryConfig {
            max_input_power_w: Some(1200.0),
            ..Default::default()
        };
        assert!(
            telemetry
                .trip_reason(None, Some(1250.0))
                .unwrap()
                .contains("Input power")
        );
        assert!(telemetry.trip_reason(None, Some(1100.0)).is_none());
    }

    /// THE DIE CEILING IS NOT EVALUATED HERE, and that is the fix rather than
    /// an omission. Its sensor is the DTS stream, not a file, so a config
    /// carrying the limit and no sysfs reading must report NOTHING from this
    /// file -- neither a trip nor a blindness. Reporting blindness is exactly
    /// what tripped every run at 45 s.
    #[test]
    fn the_die_ceiling_is_not_a_sysfs_limit_and_is_never_blind_here() {
        let telemetry = Bzm2TelemetryConfig {
            max_asic_temp_c: Some(80.0),
            ..Default::default()
        };
        assert!(telemetry.trip_reason(None, None).is_none());
        assert!(
            telemetry.blind_limits(None, None).is_empty(),
            "an armed die ceiling must not read as a blind SYSFS limit"
        );
    }

    /// A configured limit whose sensor says nothing is NOT a limit within range.
    ///
    /// The old test passed `None` against a configured limit and asserted
    /// nothing about it, so the fail-open behaviour had a test walk straight
    /// past it. These assert the distinction that was missing: `trip_reason`
    /// still reports only genuine over-limits, and blindness is reported
    /// separately so the monitor can require it to persist.
    #[test]
    fn a_configured_limit_with_no_reading_is_reported_blind() {
        let telemetry = Bzm2TelemetryConfig {
            max_input_power_w: Some(1200.0),
            ..Default::default()
        };
        // Configured, not readable: blind.
        let blind = telemetry.blind_limits(None, None);
        assert_eq!(blind.len(), 1, "got {blind:?}");
        assert!(blind.iter().any(|b| b.contains("input power")));
        // And it is NOT a trip on its own -- one dropped read must not halt a
        // 3 kW machine. The monitor requires it to persist.
        assert!(telemetry.trip_reason(None, None).is_none());
        // Readable and within limits: nothing blind, nothing tripped.
        assert!(telemetry.blind_limits(None, Some(900.0)).is_empty());
        // A limit that is NOT configured cannot be blind: there is no
        // protection for a missing reading to disarm.
        let unconfigured = Bzm2TelemetryConfig::default();
        assert!(unconfigured.blind_limits(None, None).is_empty());
        // One readable, one not: only the unreadable one is blind.
        let partial = telemetry.blind_limits(None, None);
        assert_eq!(partial.len(), 1, "got {partial:?}");
        assert!(partial[0].contains("input power"));
    }

    /// A TRIP IS A LIMIT AND A SENSOR. This is the gate for the class.
    ///
    /// The defect this exists to prevent shipped and ran: `max_asic_temp_c`
    /// was armed by default, its sensor was bound to a sysfs path nothing in
    /// this tree sets, and the pair was never checked together. The limit
    /// could not be measured, blindness is a trip, and every run
    /// stopped itself at 45 s on a board that was never over temperature.
    ///
    /// The old test asserted `max_asic_temp_c.is_some()` and passed the whole
    /// time -- it checked the half that was easy to check. This checks the
    /// pair, for every limit this file evaluates. The die ceiling is absent
    /// because this file no longer evaluates it; `abort.rs` does, from the
    /// DTS rows, and its own tests hold that end.
    #[test]
    fn no_sysfs_limit_is_armed_without_a_sensor_to_answer_it() {
        let cfg = Bzm2TelemetryConfig::from_env();
        let power_readable = cfg.input_power.is_some()
            || (cfg.input_voltage.is_some() && cfg.input_current.is_some());
        // Board temperature is absent for the same reason the die ceiling is:
        // its sensor is the board MCU's platform-thermal channels, published by
        // the heartbeat, not a file. `abort.rs` owns it.
        for (armed, readable, what) in [(
            cfg.max_input_power_w.is_some(),
            power_readable,
            "input power",
        )] {
            assert!(
                !armed || readable,
                "{what} is armed with no sensor bound to answer it. An armed limit \
                 nothing can measure is not protection -- it is a guaranteed \
                 blindness trip. Bind the sensor or do not arm the limit."
            );
        }
    }

    /// The ordinary way to run this driver must not be unprotected.
    ///
    /// The only variable required to mine is the serial path. Before this, that
    /// binary had no thermal trip, no over-power trip, and said nothing about
    /// either -- and once the fan list gained defaults it also spawned a
    /// monitor, so it looked protected from outside.
    #[test]
    fn a_default_config_has_a_thermal_trip() {
        let telemetry = Bzm2TelemetryConfig::from_env();
        assert!(
            telemetry.max_asic_temp_c.is_some(),
            "a default build must carry a die temperature limit"
        );
        assert!(
            telemetry.any_trip_armed(),
            "a default build must have at least one trip armed"
        );
        // And the limits that are genuinely unknowable from here are REPORTED
        // rather than silently absent: they are chassis properties and this
        // file knows about a hashboard.
        let unarmed = telemetry.unarmed_limits();
        assert!(unarmed.contains(&"board temperature"), "got {unarmed:?}");
        assert!(unarmed.contains(&"input power"), "got {unarmed:?}");
        assert!(!unarmed.contains(&"ASIC temperature"), "got {unarmed:?}");
    }

    /// Publishing telemetry is not the same question as being protected, and
    /// reading one for the other is how a monitor that cannot fire looked fine.
    #[test]
    fn publishing_is_not_protection() {
        let publishes_nothing_armed = Bzm2TelemetryConfig {
            max_asic_temp_c: None,
            max_board_temp_c: None,
            max_input_power_w: None,
            ..Default::default()
        };
        assert!(!publishes_nothing_armed.any_trip_armed());
        let unarmed = publishes_nothing_armed.unarmed_limits();
        assert_eq!(unarmed.len(), 4, "got {unarmed:?}");
        assert!(unarmed.contains(&"rail over-voltage"), "got {unarmed:?}");
    }

    /// The snapshot must carry it, or the monitor cannot act on it.
    #[test]
    fn the_snapshot_carries_blindness() {
        // Input power: the last limit whose sensor really is a file.
        let telemetry = Bzm2TelemetryConfig {
            max_input_power_w: Some(1200.0),
            ..Default::default()
        };
        // No sensor paths are configured, so every read returns None -- which
        // is exactly the shape of a node that has vanished.
        let snap = telemetry.snapshot();
        assert!(
            !snap.blind.is_empty(),
            "a configured limit with no sensor must reach the monitor as blind"
        );
        assert!(
            snap.trip_reason.is_none(),
            "blindness is not itself an over-limit"
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
                observed_at: Some(std::time::Instant::now()),
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
            0,
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
                asic: None,
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
        assert!(
            state.asics.is_empty(),
            "an update that names no ASIC must not invent a row for one"
        );
    }

    // --- per-ASIC observation --------------------------------------------
    //
    // Two absences closed here: WHEN a reading arrived, and WHAT fault bits
    // came with it. Both are recorded on the ASIC's own row, because one
    // frame delivers them together and a fault bit with no arrival time
    // cannot be told from one asserted an hour ago.

    #[test]
    fn a_published_reading_records_when_its_asic_spoke_and_what_it_reported() {
        let (telemetry_tx, telemetry_rx) = watch::channel(board_state());
        let observed_at = Instant::now();

        publish_thread_telemetry(
            &telemetry_tx,
            1,
            &asic_update(
                "ttyUSB1-asic-3",
                Some(64.5),
                Some(0.78),
                HashThreadAsicObservation {
                    asic_id: 3,
                    observed_at,
                    faults: Some(AsicFaultBits {
                        thermal_trip: true,
                        ..Default::default()
                    }),
                },
            ),
        );

        let state = telemetry_rx.borrow().clone();
        let row = asic_row(&state, 1, 3).expect("the ASIC that spoke must have a row");
        assert_eq!(
            row.observed_at,
            Some(observed_at),
            "the row must carry the instant the frame was observed, not the instant it was read              back: an age measured from the read is always zero"
        );
        assert_eq!(
            row.faults,
            Some(AsicFaultBits {
                thermal_trip: true,
                thermal_fault: false,
                voltage_fault: false,
                voltage_shutdown: false,
            }),
            "all four bits as reported, so a watchdog can tell which fault fired"
        );
        assert!(row.faults.is_some_and(|faults| faults.any()));
    }

    #[test]
    fn an_asic_talking_with_its_values_gated_still_records_that_it_spoke() {
        // The publish-side gates suppress a VALUE -- a sensor disabled, a
        // frame marked invalid, a temperature no die can be at. None of them
        // is evidence the device went quiet, and an ASIC answering with
        // nothing usable is a different fault from one that has stopped
        // answering. Without an arrival time on a value-less row the two are
        // indistinguishable, and the fault bits that say WHICH would be lost
        // exactly when they matter.
        let (telemetry_tx, telemetry_rx) = watch::channel(board_state());
        let observed_at = Instant::now();
        let suppressed = AsicFaultBits {
            thermal_trip: true,
            thermal_fault: true,
            voltage_fault: false,
            voltage_shutdown: false,
        };

        publish_thread_telemetry(
            &telemetry_tx,
            0,
            &asic_update(
                "ttyUSB0-asic-7",
                None,
                None,
                HashThreadAsicObservation {
                    asic_id: 7,
                    observed_at,
                    faults: Some(suppressed),
                },
            ),
        );

        let state = telemetry_rx.borrow().clone();
        assert!(
            state
                .temperatures
                .iter()
                .any(|sensor| sensor.name == "ttyUSB0-asic-7-dts" && sensor.temperature.is_none()),
            "the row still arrives, carrying no value -- that is what a gated reading is"
        );
        let row = asic_row(&state, 0, 7).expect("a suppressed value is still an ASIC talking");
        assert_eq!(row.observed_at, Some(observed_at));
        assert_eq!(row.faults, Some(suppressed));
    }

    #[test]
    fn a_later_frame_replaces_the_row_rather_than_accumulating_rows() {
        let (telemetry_tx, telemetry_rx) = watch::channel(board_state());
        let first = Instant::now();
        let second = first + Duration::from_secs(5);

        publish_thread_telemetry(
            &telemetry_tx,
            0,
            &asic_update(
                "ttyUSB0-asic-2",
                Some(60.0),
                Some(0.70),
                HashThreadAsicObservation {
                    asic_id: 2,
                    observed_at: first,
                    faults: Some(AsicFaultBits {
                        thermal_trip: true,
                        ..Default::default()
                    }),
                },
            ),
        );
        publish_thread_telemetry(
            &telemetry_tx,
            0,
            &asic_update(
                "ttyUSB0-asic-2",
                Some(61.0),
                Some(0.71),
                HashThreadAsicObservation {
                    asic_id: 2,
                    observed_at: second,
                    faults: Some(AsicFaultBits::default()),
                },
            ),
        );

        let state = telemetry_rx.borrow().clone();
        assert_eq!(state.asics.len(), 1, "one ASIC, one row");
        let row = &state.asics[0];
        assert_eq!(row.observed_at, Some(second), "the newest arrival wins");
        assert_eq!(
            row.faults,
            Some(AsicFaultBits::default()),
            "the bits are this frame's, not every frame's: carrying a cleared fault forward would              date it to a frame that never asserted it"
        );
        assert_eq!(state.temperatures.len(), 1, "and one row per sensor name");
    }

    #[test]
    fn the_same_wire_id_on_two_buses_is_two_asics() {
        // ASIC ids are local to a chain: every bus addresses from the same
        // start id. Keyed by id alone, the second bus would overwrite the
        // first, and a chain that had gone quiet would read as fresh because
        // its neighbour was still talking.
        let (telemetry_tx, telemetry_rx) = watch::channel(board_state());
        let bus0_at = Instant::now();
        let bus1_at = bus0_at + Duration::from_secs(3);

        for (thread_index, prefix, observed_at, trip) in [
            (0usize, "ttyUSB0-asic-3", bus0_at, true),
            (1usize, "ttyUSB1-asic-3", bus1_at, false),
        ] {
            publish_thread_telemetry(
                &telemetry_tx,
                thread_index,
                &asic_update(
                    prefix,
                    Some(70.0),
                    Some(0.75),
                    HashThreadAsicObservation {
                        asic_id: 3,
                        observed_at,
                        faults: Some(AsicFaultBits {
                            thermal_trip: trip,
                            ..Default::default()
                        }),
                    },
                ),
            );
        }

        let state = telemetry_rx.borrow().clone();
        assert_eq!(state.asics.len(), 2, "two devices, two rows");
        assert_eq!(asic_row(&state, 0, 3).unwrap().observed_at, Some(bus0_at));
        assert_eq!(asic_row(&state, 1, 3).unwrap().observed_at, Some(bus1_at));
        assert!(asic_row(&state, 0, 3).unwrap().faults.unwrap().thermal_trip);
        assert!(!asic_row(&state, 1, 3).unwrap().faults.unwrap().thermal_trip);
    }

    #[test]
    fn an_observation_and_a_discovery_share_one_row() {
        // Two writers, one row per ASIC. If they kept separate rows, a
        // reader joining "what this ASIC is" to "when it last spoke" would
        // find two answers for one device.
        let (telemetry_tx, telemetry_rx) = watch::channel(board_state());
        let observed_at = Instant::now();

        publish_discovered_engine_map(
            &telemetry_tx,
            0,
            "/dev/ttyUSB0",
            &Bzm2DiscoveredEngineMap {
                asic: 5,
                present: vec![crate::asic::bzm2::Bzm2EngineCoordinate::new(0, 0)],
                missing: Vec::new(),
            },
        );
        publish_thread_telemetry(
            &telemetry_tx,
            0,
            &asic_update(
                "ttyUSB0-asic-5",
                Some(66.0),
                Some(0.72),
                HashThreadAsicObservation {
                    asic_id: 5,
                    observed_at,
                    faults: Some(AsicFaultBits::default()),
                },
            ),
        );

        let state = telemetry_rx.borrow().clone();
        assert_eq!(state.asics.len(), 1);
        let row = &state.asics[0];
        assert_eq!(row.discovered_engine_count, Some(1), "discovery's field");
        assert_eq!(row.serial_path.as_deref(), Some("/dev/ttyUSB0"));
        assert_eq!(row.observed_at, Some(observed_at), "telemetry's field");
        assert_eq!(row.faults, Some(AsicFaultBits::default()));
    }

    #[test]
    fn rows_stay_in_bus_then_id_order_however_the_frames_arrive() {
        // The order is maintained where rows are ADDED, not on every frame:
        // an update in place cannot change it. This is the test that would
        // fail if that reasoning were wrong.
        let (telemetry_tx, telemetry_rx) = watch::channel(board_state());
        let observed_at = Instant::now();

        for (thread_index, asic_id) in [(1usize, 9u8), (0, 4), (1, 2), (0, 11)] {
            publish_thread_telemetry(
                &telemetry_tx,
                thread_index,
                &asic_update(
                    &format!("ttyUSB{thread_index}-asic-{asic_id}"),
                    Some(60.0),
                    Some(0.70),
                    HashThreadAsicObservation {
                        asic_id,
                        observed_at,
                        faults: None,
                    },
                ),
            );
        }

        let state = telemetry_rx.borrow().clone();
        let order: Vec<(usize, u8)> = state
            .asics
            .iter()
            .map(|asic| (asic.thread_index.unwrap(), asic.id))
            .collect();
        assert_eq!(order, vec![(0, 4), (0, 11), (1, 2), (1, 9)]);
    }

    /// A board with nothing published to it yet.
    fn board_state() -> BoardTelemetry {
        BoardTelemetry {
            name: "bzm2-test".into(),
            model: "BZM2".into(),
            serial: Some("bzm2-test".into()),
            ..Default::default()
        }
    }

    /// One ASIC's frame as the hash thread hands it over: a die temperature
    /// and one rail, under the names the publisher spells, plus the
    /// observation that says when they arrived.
    fn asic_update(
        sensor_prefix: &str,
        temperature_c: Option<f32>,
        voltage_v: Option<f32>,
        observation: HashThreadAsicObservation,
    ) -> HashThreadTelemetryUpdate {
        HashThreadTelemetryUpdate {
            temperatures: vec![crate::asic::hash_thread::HashThreadTemperatureReading {
                name: format!("{sensor_prefix}-dts"),
                temperature_c,
            }],
            powers: vec![crate::asic::hash_thread::HashThreadPowerReading {
                name: format!("{sensor_prefix}-vs-ch0"),
                voltage_v,
                current_a: None,
                power_w: None,
            }],
            asic: Some(observation),
        }
    }

    fn asic_row(state: &BoardTelemetry, thread_index: usize, asic_id: u8) -> Option<&AsicState> {
        asic_row_index(&state.asics, thread_index, asic_id).map(|index| &state.asics[index])
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
