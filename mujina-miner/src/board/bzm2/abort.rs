//! What counts as an abort condition, evaluated from telemetry we actually
//! have.
//!
//! Separate from [`super::scram`], which decides what to DO about a condition.
//! This file decides whether there is one, and it is pure so that every
//! abort scenario can be exercised without a board.
//!
//! **A trip is a limit AND a sensor.** Either one alone is not protection, and
//! believing otherwise is not hypothetical here: the only limit this driver
//! armed by default was bound to a sysfs path that does not exist on this
//! platform, so it was permanently blind, and every run tripped
//! itself at 45 s while the real per-ASIC die temperatures streamed past
//! unread on the UART. Seven runs, seven identical self-trips. The limit was
//! right, the sensor was never connected to it, and nothing checked the pair.
//!
//! So conditions here are computed from `BoardTelemetry` -- the state the
//! threads publish into and the monitor already holds -- rather than from a
//! second set of sysfs reads that may name nothing.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::api_client::types::{
    AsicState, BoardTelemetry, Fan, PowerMeasurement, TemperatureSensor,
};

/// How hard to respond. The ladder in [`super::scram`] consumes this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AbortSeverity {
    /// Stop dispatch, keep watching, escalate only if it persists or recurs.
    Arm,
    /// Go straight to de-energising. No arm rung.
    ///
    /// Reserved for conditions that stopping work cannot fix, because the
    /// cause is the rail rather than the load. A part reporting a voltage
    /// fault is not made safer by being given less to do.
    Scram,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AbortCondition {
    pub(super) severity: AbortSeverity,
    pub(super) reason: String,
}

impl AbortCondition {
    pub(super) fn arm(reason: impl Into<String>) -> Self {
        Self {
            severity: AbortSeverity::Arm,
            reason: reason.into(),
        }
    }

    pub(super) fn scram(reason: impl Into<String>) -> Self {
        Self {
            severity: AbortSeverity::Scram,
            reason: reason.into(),
        }
    }
}

/// The limits an operator configured, paired here with nothing: whether a
/// sensor exists to measure each is decided at the point of evaluation, from
/// the telemetry itself.
#[derive(Debug, Clone, Default)]
pub(super) struct AbortLimits {
    /// Per-ASIC die ceiling, in degrees C.
    pub(super) max_die_c: Option<f32>,
    /// A tachometer below this, with the fan not reading zero, is a fan that
    /// is failing rather than one that is stopped.
    pub(super) min_fan_rpm: Option<u32>,
    /// Regulator output ceiling, in volts.
    pub(super) max_rail_v: Option<f32>,
    /// Board inlet/outlet ceiling, in degrees C.
    ///
    /// Its sensor is the board MCU's platform-thermal channels, published by
    /// the heartbeat -- NOT a sysfs path. `telemetry.rs` evaluated this against
    /// `MUJINA_BZM2_BOARD_TEMP_PATH`, which nothing in the tree sets, so the
    /// limit was reported permanently unarmed while the MCU answered the
    /// question every fifth beat and our own harness recorded the answer.
    pub(super) max_board_c: Option<f32>,
}

/// The strongest condition this telemetry supports, or `None`.
///
/// Severity order, not list order: a scram-severity condition is returned even
/// if an arm-severity one was found first, because reporting the milder of two
/// simultaneous faults would pick the wrong rung of the ladder.
pub(super) fn evaluate(
    telemetry: &BoardTelemetry,
    limits: &AbortLimits,
    now: Instant,
    die_max_age: Duration,
) -> Option<AbortCondition> {
    let mut found: Vec<AbortCondition> = Vec::new();
    found.extend(silicon_faults(&telemetry.asics));
    found.extend(die_over_temperature(
        &telemetry.temperatures,
        limits.max_die_c,
        now,
        die_max_age,
    ));
    found.extend(board_over_temperature(
        &telemetry.temperatures,
        limits.max_board_c,
        now,
        die_max_age,
    ));
    found.extend(fan_failure(&telemetry.fans, limits.min_fan_rpm));
    found.extend(rail_over_voltage(&telemetry.powers, limits.max_rail_v));

    found
        .iter()
        .find(|c| c.severity == AbortSeverity::Scram)
        .or_else(|| found.first())
        .cloned()
}

/// Faults the silicon reports about ITSELF.
///
/// These need no limit of ours to be armed and no budget to elapse: the part
/// has already applied its own threshold and is telling us the answer. They
/// were decoded, published to the API and acted on by nothing.
///
/// `faults: None` is "not available", never "none asserted" -- a chain that
/// has not spoken and a healthy one must not read alike -- so it is skipped
/// here rather than counted as clean.
fn silicon_faults(asics: &[AsicState]) -> Option<AbortCondition> {
    let mut thermal: Vec<u8> = Vec::new();
    let mut voltage: Vec<u8> = Vec::new();
    for asic in asics {
        let Some(faults) = asic.faults else { continue };
        if faults.thermal_trip || faults.thermal_fault {
            thermal.push(asic.id);
        }
        if faults.voltage_fault || faults.voltage_shutdown {
            voltage.push(asic.id);
        }
    }
    // Voltage outranks thermal: it is the one stopping work cannot fix.
    if !voltage.is_empty() {
        return Some(AbortCondition::scram(format!(
            "{} ASIC(s) report a voltage fault or shutdown ({}). The part has applied its own \
             threshold; giving it less work does not change its rail.",
            voltage.len(),
            id_list(&voltage),
        )));
    }
    if !thermal.is_empty() {
        return Some(AbortCondition::arm(format!(
            "{} ASIC(s) assert their own thermal trip or fault ({}). This is the silicon's \
             threshold, not ours.",
            thermal.len(),
            id_list(&thermal),
        )));
    }
    None
}

/// The hottest die on the board, against the configured ceiling.
///
/// Reads the per-ASIC DTS rows the hash threads publish -- `-dts` suffixed --
/// and NOT a board-level sysfs sensor. HOTTEST, not mean: a board averages
/// safe with one part cooking, and the part that fails is the hot one.
fn die_over_temperature(
    temperatures: &[TemperatureSensor],
    ceiling_c: Option<f32>,
    now: Instant,
    max_age: Duration,
) -> Option<AbortCondition> {
    let ceiling_c = ceiling_c?;
    let (name, hottest) = hottest_die_at(temperatures, now, max_age)?;
    (hottest > ceiling_c).then(|| {
        AbortCondition::arm(format!(
            "die {name} at {hottest:.1}C exceeded the {ceiling_c:.1}C ceiling"
        ))
    })
}

/// The hottest per-ASIC die reading that is still a MEASUREMENT.
///
/// A row older than `max_age` is rejected, and so is one with no stamp at all.
/// Board state never prunes -- `merge_temperature_readings` only overwrites or
/// appends -- so a die whose chain stopped publishing keeps its last value for
/// the life of the process. Without this filter a ceiling is evaluated against
/// a number that stopped changing an hour ago, and a frozen cool reading is
/// indistinguishable from a healthy one.
///
/// Rejecting makes that case fall through to the blindness path, which is
/// already budgeted and already escalates: there is nothing to see, which is
/// the truth, rather than a stale value dressed as a reading.
///
/// **Unstamped counts as stale.** A reader deciding safety must treat unknown
/// as unusable, never as fresh -- the same rule the interlock one layer down
/// applies to a missing temperature.
pub(super) fn hottest_die_at(
    temperatures: &[TemperatureSensor],
    now: Instant,
    max_age: Duration,
) -> Option<(&str, f32)> {
    temperatures
        .iter()
        .filter(|sensor| sensor.name.ends_with("-dts"))
        .filter(|sensor| {
            sensor
                .observed_at
                .is_some_and(|at| now.saturating_duration_since(at) <= max_age)
        })
        .filter_map(|sensor| {
            sensor
                .temperature
                .map(|t| (sensor.name.as_str(), t.as_degrees_c()))
        })
        .max_by(|a, b| a.1.total_cmp(&b.1))
}

/// The hottest board thermal against its ceiling.
///
/// Board inlet and outlet, from the MCU. Hottest of the two for the same
/// reason the die ceiling takes the hottest part: outlet leads inlet under
/// load, and an average would hide the one that matters.
fn board_over_temperature(
    temperatures: &[TemperatureSensor],
    ceiling_c: Option<f32>,
    now: Instant,
    max_age: Duration,
) -> Option<AbortCondition> {
    let ceiling_c = ceiling_c?;
    let (name, hottest) = temperatures
        .iter()
        .filter(|s| s.name.contains("-inlet") || s.name.contains("-outlet"))
        .filter(|s| {
            s.observed_at
                .is_some_and(|at| now.saturating_duration_since(at) <= max_age)
        })
        .filter_map(|s| s.temperature.map(|t| (s.name.as_str(), t.as_degrees_c())))
        .max_by(|a, b| a.1.total_cmp(&b.1))?;
    (hottest > ceiling_c).then(|| {
        AbortCondition::arm(format!(
            "board thermal {name} at {hottest:.1}C exceeded the {ceiling_c:.1}C ceiling"
        ))
    })
}

/// Is the fan floor armed with no tachometer answering it?
///
/// A trip is a limit AND a sensor, and `min_fan_rpm` is armed by default. The
/// floor cannot fire if nothing reports rpm, and a fan list where every tacho
/// reads `None` is indistinguishable -- from the floor's point of view -- from
/// a rack of healthy fans.
///
/// Returns true only when there are fans to read and NONE of them answered.
/// An empty fan list is a different fact (this board publishes no fans at all)
/// and is not this function's to report.
pub(super) fn fans_blind(fans: &[Fan], min_rpm: Option<u32>) -> bool {
    min_rpm.is_some() && !fans.is_empty() && fans.iter().all(|fan| fan.rpm.is_none())
}

/// Has a chain stopped returning results while we are still giving it work?
///
/// THE ONE FAILURE THE THERMAL LADDER CANNOT SEE. Everything else in this file
/// watches whether we can SEE the chain or whether it is HOT. A chain that has
/// stopped producing results while still streaming DTS/VS frames is neither —
/// and it is worse than neutral, because **the dies read cool precisely
/// because nothing is hashing**. Every thermal check passes, the interlock
/// keeps permitting dispatch, the ladder never arms, and the machine sits at
/// its operating point consuming power and producing nothing.
///
/// The stock stack powers the system off in this state. We had nothing.
///
/// **`decoded`, not `accepted`.** `accepted` counts results that met the
/// target, so it is luck-dependent and a quiet run is indistinguishable from a
/// dead one over short windows. `decoded` counts every result frame the chain
/// returned at all, which advances steadily while the chain is working and
/// stops dead when it is not. Liveness is about whether the chain is talking
/// back about work, not about whether the work was any good.
#[derive(Debug, Default)]
pub(super) struct StallWatch {
    /// Per chain: the last `decoded` value seen, and when it last CHANGED --
    /// not when it was last read. A counter read a thousand times without
    /// moving has not been alive a thousand times.
    per_thread: BTreeMap<usize, (u64, Instant)>,
}

impl StallWatch {
    /// Fold one poll's counters in.
    ///
    /// `dispatching` is what makes this honest: a thread that is not being
    /// given work is not expected to return results, and observing mode
    /// dispatches nothing at all. A stall condition that fired whenever a
    /// board was idle would fire on every observation run we have ever done.
    pub(super) fn observe(&mut self, thread: usize, decoded: u64, dispatching: bool, now: Instant) {
        if !dispatching {
            self.per_thread.remove(&thread);
            return;
        }
        match self.per_thread.get_mut(&thread) {
            Some(entry) if entry.0 != decoded => *entry = (decoded, now),
            Some(_) => {}
            None => {
                self.per_thread.insert(thread, (decoded, now));
            }
        }
    }

    /// Chains whose result counter has not moved for longer than `budget`.
    pub(super) fn stalled(&self, now: Instant, budget: Duration) -> Vec<(usize, Duration)> {
        self.per_thread
            .iter()
            .map(|(t, (_, at))| (*t, now.saturating_duration_since(*at)))
            .filter(|(_, age)| *age >= budget)
            .collect()
    }
}

/// A chain that is being given work and has stopped answering.
///
/// Arms rather than scrams. The first response to a chain that stopped
/// producing is to stop giving it work, which is cheap and removes the load;
/// if it does not come back, the ladder escalates on its own clock like
/// everything else.
pub(super) fn stall_condition(
    stalled: &[(usize, Duration)],
    budget: Duration,
) -> Option<AbortCondition> {
    if stalled.is_empty() {
        return None;
    }
    let worst = stalled.iter().map(|(_, age)| *age).max().unwrap_or(budget);
    Some(AbortCondition::arm(format!(
        "{} chain(s) have returned no result for up to {:.0}s, past the {:.0}s budget, while \
         still being given work ({}). A chain that stopped producing reads COOL because \
         nothing is hashing, so no thermal check will ever notice it.",
        stalled.len(),
        worst.as_secs_f32(),
        budget.as_secs_f32(),
        stalled
            .iter()
            .map(|(t, age)| format!("chain {t}: {:.0}s", age.as_secs_f32()))
            .collect::<Vec<_>>()
            .join(", "),
    )))
}

/// What this board can see, and what it is acting on.
///
/// DERIVED, NEVER TYPED. A hand-maintained inventory document is a
/// hand-maintained answer to this question, and it drifted exactly as a second
/// copy of a fact does: it recorded board inlet and outlet as covered because
/// the *harness* wrote them to disk, while the miner had never read them and
/// the board ceiling could not fire. It recorded per-ASIC die temperature as
/// uncaptured while the miner was publishing a hundred rows of it.
///
/// So the miner answers for itself, from the telemetry it actually holds and
/// the limits it actually armed, and the run record captures that answer. A
/// document cannot disagree with a machine that reports its own coverage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Coverage {
    /// A limit is armed and something fresh is answering it.
    Protected { rows: usize, fresh: usize },
    /// A limit is armed and NOTHING can make it fire. The dangerous one: from
    /// outside it is indistinguishable from a board within limits.
    ArmedButBlind { rows: usize, fresh: usize },
    /// Readings are present and no limit is set, so nothing will act on them.
    ObservedNotArmed { rows: usize, fresh: usize },
    /// No limit and no readings.
    Absent,
}

impl Coverage {
    fn of(armed: bool, rows: usize, fresh: usize) -> Self {
        match (armed, fresh > 0, rows > 0) {
            (true, true, _) => Coverage::Protected { rows, fresh },
            (true, false, _) => Coverage::ArmedButBlind { rows, fresh },
            (false, _, true) => Coverage::ObservedNotArmed { rows, fresh },
            (false, _, false) => Coverage::Absent,
        }
    }

    pub(super) fn is_armed_but_blind(&self) -> bool {
        matches!(self, Coverage::ArmedButBlind { .. })
    }

    pub(super) fn label(&self) -> &'static str {
        match self {
            Coverage::Protected { .. } => "protected",
            Coverage::ArmedButBlind { .. } => "ARMED-BUT-BLIND",
            Coverage::ObservedNotArmed { .. } => "observed, not armed",
            Coverage::Absent => "absent",
        }
    }

    pub(super) fn counts(&self) -> (usize, usize) {
        match *self {
            Coverage::Protected { rows, fresh }
            | Coverage::ArmedButBlind { rows, fresh }
            | Coverage::ObservedNotArmed { rows, fresh } => (rows, fresh),
            Coverage::Absent => (0, 0),
        }
    }
}

/// One row per signal class this board could protect itself with.
pub(super) fn coverage(
    telemetry: &BoardTelemetry,
    limits: &AbortLimits,
    now: Instant,
    max_age: Duration,
) -> Vec<(&'static str, Coverage)> {
    let fresh = |s: &TemperatureSensor| {
        s.observed_at
            .is_some_and(|at| now.saturating_duration_since(at) <= max_age)
    };
    let count = |pred: &dyn Fn(&TemperatureSensor) -> bool| {
        let rows = telemetry.temperatures.iter().filter(|s| pred(s)).count();
        let f = telemetry
            .temperatures
            .iter()
            .filter(|s| pred(s) && fresh(s) && s.temperature.is_some())
            .count();
        (rows, f)
    };

    let (die_rows, die_fresh) = count(&|s: &TemperatureSensor| s.name.ends_with("-dts"));
    let (board_rows, board_fresh) =
        count(&|s: &TemperatureSensor| s.name.contains("-inlet") || s.name.contains("-outlet"));

    let fan_rows = telemetry.fans.len();
    let fan_fresh = telemetry.fans.iter().filter(|f| f.rpm.is_some()).count();

    let rail_rows = telemetry
        .powers
        .iter()
        .filter(|p| p.name.ends_with("-output"))
        .count();
    let rail_fresh = telemetry
        .powers
        .iter()
        .filter(|p| p.name.ends_with("-output") && p.voltage_v.is_some())
        .count();

    // The silicon's own fault bits need no limit of ours, so they are armed
    // whenever any ASIC is reporting them at all -- and aged like every other
    // reading. Counting `faults.is_some()` alone read 100/100 PROTECTED on
    // hardware 37 s after the stream carrying the bits had stopped:
    // held is not the same as still being sent.
    let fault_rows = telemetry.asics.len();
    let fault_fresh = telemetry
        .asics
        .iter()
        .filter(|a| {
            a.faults.is_some()
                && a.observed_at
                    .is_some_and(|at| now.saturating_duration_since(at) <= max_age)
        })
        .count();

    vec![
        (
            "asic die temperature",
            Coverage::of(limits.max_die_c.is_some(), die_rows, die_fresh),
        ),
        (
            "silicon self-reported faults",
            Coverage::of(true, fault_rows, fault_fresh),
        ),
        (
            "board inlet/outlet",
            Coverage::of(limits.max_board_c.is_some(), board_rows, board_fresh),
        ),
        (
            "fan tachometers",
            Coverage::of(limits.min_fan_rpm.is_some(), fan_rows, fan_fresh),
        ),
        (
            "rail output voltage",
            Coverage::of(limits.max_rail_v.is_some(), rail_rows, rail_fresh),
        ),
    ]
}

/// How many `-dts` rows exist at all, fresh or not.
///
/// Reported beside the fresh count so an operator can tell "this board has no
/// die sensors" from "this board's die sensors stopped answering" -- which are
/// different faults with the same effect on the ceiling.
pub(super) fn die_row_count(temperatures: &[TemperatureSensor]) -> usize {
    temperatures
        .iter()
        .filter(|s| s.name.ends_with("-dts"))
        .count()
}

/// A stopped or failing fan.
///
/// Zero and "below the floor" are reported differently because they are
/// different findings: zero is a fan that is not turning, a low number is one
/// that is. `rpm: None` is neither -- it is a tachometer that did not answer,
/// which is blindness and is budgeted elsewhere rather than treated as death.
/// Counting an unread tacho as a dead fan would stop the machine every time a
/// sysfs read hiccuped.
///
/// This arms rather than scrams. A stopped fan on a loaded board is a thermal
/// fault that has not arrived yet, and the arm rung removes the heat source
/// immediately; if the fan does not come back the heat does not leave either,
/// and the escalation rung is what covers that.
fn fan_failure(fans: &[Fan], min_rpm: Option<u32>) -> Option<AbortCondition> {
    let stopped: Vec<&str> = fans
        .iter()
        .filter(|fan| fan.rpm == Some(0))
        .map(|fan| fan.name.as_str())
        .collect();
    if !stopped.is_empty() {
        return Some(AbortCondition::arm(format!(
            "fan(s) {} report 0 rpm: not turning",
            stopped.join(", ")
        )));
    }
    let floor = min_rpm?;
    let slow: Vec<String> = fans
        .iter()
        .filter_map(|fan| {
            fan.rpm
                .filter(|rpm| *rpm < floor)
                .map(|rpm| format!("{} at {rpm} rpm", fan.name))
        })
        .collect();
    (!slow.is_empty()).then(|| {
        AbortCondition::arm(format!(
            "fan(s) below the {floor} rpm floor: {}",
            slow.join(", ")
        ))
    })
}

/// A regulator output above its ceiling.
///
/// Scram, not arm, for the same reason as a silicon voltage fault: the load is
/// not the cause and reducing it is not the remedy.
fn rail_over_voltage(
    powers: &[PowerMeasurement],
    ceiling_v: Option<f32>,
) -> Option<AbortCondition> {
    let ceiling_v = ceiling_v?;
    powers
        .iter()
        .filter(|p| p.name.ends_with("-output"))
        .find_map(|p| {
            p.voltage_v.filter(|v| *v > ceiling_v).map(|v| {
                AbortCondition::scram(format!(
                    "{} at {v:.3}V is above the {ceiling_v:.3}V ceiling",
                    p.name
                ))
            })
        })
}

/// Render ids compactly, and say when the list was cut rather than cutting it
/// silently -- a truncated list that looks complete under-reports a fault.
/// What asking a chain's thread for its counters returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ThreadAnswer {
    Answered,
    /// The command channel is closed: the thread's actor has exited. Nothing
    /// will ever answer on it again, so this needs no budget.
    Closed,
    /// No answer inside the bound, or an error that is not a closed channel.
    /// A busy actor can miss one; one that keeps missing is lost.
    Unanswered,
}

/// A CHAIN THAT HAS GONE AWAY WHILE ITS BOARD STAYS POWERED.
///
/// Every other blindness here is board-wide: the die ceiling counts as blind
/// only when EVERY die row is stale, so a board with three chains stays
/// "measured" while one of them is dark. Measured on hardware:
/// thread 1 stopped itself, and for five minutes the
/// monitor logged `die_rows_fresh=200` of 300 with nothing armed. Observing,
/// harmless. Mining, it is a chain that stops hashing unnoticed -- or one
/// whose dies nobody is reading while its board is still fed.
///
/// Two independent signs, either of which is enough:
///
/// - **every die row of the chain is stale**, or the chain has none, for the
///   blindness budget (the chain's name is its rows' prefix, the serial
///   port's file name, so a chain the board configured and never heard from
///   is found too);
/// - **its thread does not answer**: a closed command channel at once (the
///   actor has exited, and that cannot heal), or no answer for the budget.
///
/// Both ARM. Stopping dispatch is right for a lost chain, and a chain that
/// cannot come back in this process escalates to a scram on the ladder's own
/// clock.
///
/// **After any Arm, every chain reads lost.** The arm's `stop_dispatch` shuts
/// every thread down, so each channel is closed from then on and this stays
/// true until the scram. That changes no outcome: the ladder disarms only
/// after 300 s clear, and with the threads stopped the die rows go stale in
/// 15 s and board-wide die blindness returns 30 s later, so an arm that
/// stopped the threads never reached disarm before this existed either. A
/// board nobody can measure is not one to leave powered.
#[derive(Debug, Default)]
pub(super) struct ChainWatch {
    dies_blind_since: BTreeMap<String, Instant>,
    unanswered_since: BTreeMap<usize, Instant>,
    closed: std::collections::BTreeSet<usize>,
}

impl ChainWatch {
    /// Fold one poll's die rows in, for the chains the board configured.
    pub(super) fn observe_dies(
        &mut self,
        chains: &[String],
        temperatures: &[TemperatureSensor],
        now: Instant,
        max_age: Duration,
    ) {
        for chain in chains {
            let prefix = format!("{chain}-asic-");
            let fresh = temperatures.iter().any(|t| {
                t.name.starts_with(&prefix)
                    && t.name.ends_with("-dts")
                    && t.temperature.is_some()
                    && t.observed_at
                        .is_some_and(|at| now.saturating_duration_since(at) <= max_age)
            });
            if fresh {
                self.dies_blind_since.remove(chain);
            } else {
                self.dies_blind_since.entry(chain.clone()).or_insert(now);
            }
        }
    }

    /// Fold in what one thread's metrics query returned.
    pub(super) fn observe_thread(&mut self, thread: usize, answer: ThreadAnswer, now: Instant) {
        match answer {
            ThreadAnswer::Answered => {
                self.unanswered_since.remove(&thread);
                self.closed.remove(&thread);
            }
            ThreadAnswer::Closed => {
                self.closed.insert(thread);
            }
            ThreadAnswer::Unanswered => {
                self.unanswered_since.entry(thread).or_insert(now);
            }
        }
    }

    /// The condition, if any chain is lost. `die_armed` is whether the die
    /// ceiling is configured: blind dies are a lost limit only when a limit
    /// reads them, but a thread that is gone is a lost chain regardless.
    pub(super) fn condition(
        &self,
        now: Instant,
        budget: Duration,
        die_armed: bool,
    ) -> Option<AbortCondition> {
        let mut lost: Vec<String> = Vec::new();
        for thread in &self.closed {
            lost.push(format!("chain {thread}: its thread has exited"));
        }
        for (thread, since) in &self.unanswered_since {
            let age = now.saturating_duration_since(*since);
            if age >= budget && !self.closed.contains(thread) {
                lost.push(format!(
                    "chain {thread}: its thread has not answered for {:.0}s",
                    age.as_secs_f32()
                ));
            }
        }
        if die_armed {
            for (chain, since) in &self.dies_blind_since {
                let age = now.saturating_duration_since(*since);
                if age >= budget {
                    lost.push(format!(
                        "{chain}: no fresh die reading for {:.0}s",
                        age.as_secs_f32()
                    ));
                }
            }
        }
        if lost.is_empty() {
            return None;
        }
        Some(AbortCondition::arm(format!(
            "a chain is lost while its board stays powered ({}). The other chains are still \
             measured, so no board-wide check sees this; the {:.0}s budget is the blindness one.",
            lost.join("; "),
            budget.as_secs_f32(),
        )))
    }
}

fn id_list(ids: &[u8]) -> String {
    const SHOWN: usize = 8;
    if ids.len() <= SHOWN {
        return ids
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(",");
    }
    format!(
        "{}, and {} more",
        ids[..SHOWN]
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(","),
        ids.len() - SHOWN
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_client::types::AsicFaultBits;
    use crate::types::Temperature;

    fn telemetry() -> BoardTelemetry {
        BoardTelemetry {
            name: "bzm2".into(),
            model: "BZM2".into(),
            serial: None,
            fans: Vec::new(),
            temperatures: Vec::new(),
            powers: Vec::new(),
            threads: Vec::new(),
            efficiency: Vec::new(),
            asics: Vec::new(),
        }
    }

    const MAX_AGE: Duration = Duration::from_secs(15);

    /// A reading taken NOW. Every test that is not about staleness wants this.
    fn die(name: &str, c: f32) -> TemperatureSensor {
        die_aged(name, c, Duration::ZERO)
    }

    /// A reading taken `ago` in the past.
    fn die_aged(name: &str, c: f32, ago: Duration) -> TemperatureSensor {
        TemperatureSensor {
            name: name.into(),
            temperature: Some(Temperature::from_celsius(c)),
            observed_at: Instant::now().checked_sub(ago),
        }
    }

    /// Evaluate against a clock the test controls.
    fn evaluate(t: &BoardTelemetry, limits: &AbortLimits) -> Option<AbortCondition> {
        super::evaluate(t, limits, Instant::now(), MAX_AGE)
    }

    fn asic(id: u8, faults: Option<AsicFaultBits>) -> AsicState {
        AsicState {
            id,
            thread_index: None,
            serial_path: None,
            discovered_engine_count: None,
            missing_engines: Vec::new(),
            faults,
            observed_at: None,
        }
    }

    fn fan(name: &str, rpm: Option<u32>) -> Fan {
        Fan {
            name: name.into(),
            rpm,
            percent: Some(60),
            target_percent: None,
        }
    }

    /// THE BUG THIS FILE EXISTS FOR. The die ceiling must be measured against
    /// the per-ASIC DTS rows the threads publish. Before this, it was measured
    /// against an unbound sysfs path, so a board at 120C read as no condition
    /// at all while these exact rows sat in the same telemetry struct.
    #[test]
    fn the_die_ceiling_reads_the_per_asic_dts_rows() {
        let mut t = telemetry();
        t.temperatures = vec![
            die("tty9bit00-asic-1-dts", 70.0),
            die("tty9bit00-asic-2-dts", 120.0),
            die("tty9bit00-asic-3-dts", 68.0),
        ];
        let limits = AbortLimits {
            max_die_c: Some(105.0),
            ..Default::default()
        };
        let condition = evaluate(&t, &limits).expect("120C must be a condition");
        assert_eq!(condition.severity, AbortSeverity::Arm);
        assert!(condition.reason.contains("asic-2"), "{}", condition.reason);
        assert!(condition.reason.contains("120.0"), "{}", condition.reason);
    }

    /// THE DEFECT THIS FIX EXISTS FOR. Board state never prunes, so a
    /// die whose chain stopped publishing keeps its last value forever. Before
    /// the stamp, a reading frozen an hour ago was evaluated exactly like one
    /// taken this instant -- and a frozen COOL reading reported a safe board
    /// with nothing behind it.
    #[test]
    fn a_frozen_cool_reading_is_not_a_safe_board() {
        let mut t = telemetry();
        t.temperatures = vec![
            die_aged("tty9bit00-asic-1-dts", 60.0, Duration::from_secs(3600)),
            die_aged("tty9bit00-asic-2-dts", 62.0, Duration::from_secs(3600)),
        ];
        let limits = AbortLimits {
            max_die_c: Some(105.0),
            ..Default::default()
        };
        // It must not read as a die measurement at all...
        assert_eq!(
            hottest_die_at(&t.temperatures, Instant::now(), MAX_AGE),
            None,
            "an hour-old row is not a measurement"
        );
        // ...and so it produces no over-temperature condition either, because
        // there is nothing to compare. The rows still EXIST, which is what the
        // blindness path needs in order to say which kind of blindness it is.
        assert_eq!(evaluate(&t, &limits), None);
        assert_eq!(die_row_count(&t.temperatures), 2);
    }

    /// The boundary, from both sides, so the filter cannot be off by a whole
    /// max_age without a test noticing.
    #[test]
    fn a_reading_inside_the_age_limit_still_counts_and_one_outside_does_not() {
        let fresh = vec![die_aged(
            "c-asic-1-dts",
            70.0,
            MAX_AGE - Duration::from_secs(1),
        )];
        assert!(hottest_die_at(&fresh, Instant::now(), MAX_AGE).is_some());
        let stale = vec![die_aged(
            "c-asic-1-dts",
            70.0,
            MAX_AGE + Duration::from_secs(1),
        )];
        assert!(hottest_die_at(&stale, Instant::now(), MAX_AGE).is_none());
    }

    /// Unknown reads as unusable, never as fresh. A row with no stamp is a row
    /// nobody can date, and a reader deciding safety must not assume the
    /// generous answer.
    #[test]
    fn an_unstamped_reading_counts_as_stale() {
        let rows = vec![TemperatureSensor {
            name: "c-asic-1-dts".into(),
            temperature: Some(Temperature::from_celsius(70.0)),
            observed_at: None,
        }];
        assert_eq!(hottest_die_at(&rows, Instant::now(), MAX_AGE), None);
    }

    /// A stale row must not mask a live one. If one chain dies and another
    /// keeps talking, the ceiling is still answerable -- by the chain that is
    /// still answering.
    ///
    /// Still true, and still the ceiling's job: a frozen row is not a
    /// measurement and must not trip it. What this test does NOT show is the
    /// frozen chain being noticed at all -- nothing did, until `ChainWatch`
    /// (measured that gap on hardware; see the tests below).
    #[test]
    fn one_live_chain_still_answers_when_another_has_frozen() {
        let mut t = telemetry();
        t.temperatures = vec![
            die_aged("dead-asic-1-dts", 130.0, Duration::from_secs(3600)),
            die("live-asic-1-dts", 70.0),
        ];
        let hottest = hottest_die_at(&t.temperatures, Instant::now(), MAX_AGE);
        assert_eq!(hottest.map(|(n, _)| n), Some("live-asic-1-dts"));
        // And the frozen 130C row must NOT trip a 105C ceiling: it is not a
        // measurement, and acting on it would be acting on an hour-old number.
        let limits = AbortLimits {
            max_die_c: Some(105.0),
            ..Default::default()
        };
        assert_eq!(evaluate(&t, &limits), None);
    }

    fn chains() -> Vec<String> {
        vec!["tty9bit00".into(), "tty9bit10".into(), "tty9bit20".into()]
    }

    /// Three chains, one of them frozen: a shape measured on hardware.
    fn one_chain_frozen() -> Vec<TemperatureSensor> {
        let mut rows = Vec::new();
        for (chain, ago) in [
            ("tty9bit00", Duration::ZERO),
            ("tty9bit10", Duration::from_secs(300)),
            ("tty9bit20", Duration::ZERO),
        ] {
            for i in 0..100 {
                rows.push(die_aged(&format!("{chain}-asic-{i}-dts"), 40.0, ago));
            }
        }
        rows
    }

    #[test]
    fn a_chain_whose_dies_all_went_stale_is_lost_while_the_others_are_fresh() {
        let rows = one_chain_frozen();
        let now = Instant::now();
        let budget = Duration::from_secs(30);
        // What the board-wide checks say about exactly this telemetry: the
        // ceiling has a fresh hottest die, so nothing is blind and nothing is
        // over. This is why this class sat unarmed for five minutes.
        let mut t = telemetry();
        t.temperatures = rows.clone();
        assert!(hottest_die_at(&rows, now, MAX_AGE).is_some());
        let limits = AbortLimits {
            max_die_c: Some(90.0),
            ..Default::default()
        };
        assert_eq!(super::evaluate(&t, &limits, now, MAX_AGE), None);

        let mut watch = ChainWatch::default();
        watch.observe_dies(&chains(), &rows, now, MAX_AGE);
        assert_eq!(
            watch.condition(now, budget, true),
            None,
            "not before the budget"
        );
        let later = now + budget;
        watch.observe_dies(&chains(), &rows, later, MAX_AGE);
        let c = watch
            .condition(later, budget, true)
            .expect("lost after the budget");
        assert_eq!(c.severity, AbortSeverity::Arm);
        assert!(
            c.reason.contains("tty9bit10: no fresh die reading"),
            "{}",
            c.reason
        );
        assert!(!c.reason.contains("tty9bit00"), "{}", c.reason);
    }

    #[test]
    fn a_chain_the_board_configured_and_never_heard_from_is_lost() {
        let rows: Vec<_> = (0..100)
            .map(|i| die(&format!("tty9bit00-asic-{i}-dts"), 40.0))
            .collect();
        let now = Instant::now();
        let mut watch = ChainWatch::default();
        watch.observe_dies(&chains(), &rows, now, MAX_AGE);
        let c = watch
            .condition(now + Duration::from_secs(30), Duration::from_secs(30), true)
            .unwrap();
        assert!(
            c.reason.contains("tty9bit10") && c.reason.contains("tty9bit20"),
            "{}",
            c.reason
        );
    }

    #[test]
    fn a_chain_that_speaks_again_clears_its_clock() {
        let now = Instant::now();
        let budget = Duration::from_secs(30);
        let mut watch = ChainWatch::default();
        watch.observe_dies(&chains(), &one_chain_frozen(), now, MAX_AGE);
        let all_fresh: Vec<_> = chains()
            .iter()
            .flat_map(|c| (0..100).map(move |i| die(&format!("{c}-asic-{i}-dts"), 40.0)))
            .collect();
        watch.observe_dies(
            &chains(),
            &all_fresh,
            now + Duration::from_secs(10),
            MAX_AGE,
        );
        assert_eq!(watch.condition(now + budget * 2, budget, true), None);
    }

    #[test]
    fn a_thread_whose_actor_exited_is_lost_at_once() {
        let now = Instant::now();
        let mut watch = ChainWatch::default();
        watch.observe_thread(1, ThreadAnswer::Closed, now);
        let c = watch
            .condition(now, Duration::from_secs(30), false)
            .expect("no budget for a closed channel");
        assert_eq!(c.severity, AbortSeverity::Arm);
        assert!(
            c.reason.contains("chain 1: its thread has exited"),
            "{}",
            c.reason
        );
    }

    #[test]
    fn a_thread_that_misses_one_answer_is_not_lost_one_that_keeps_missing_is() {
        let now = Instant::now();
        let budget = Duration::from_secs(30);
        let mut watch = ChainWatch::default();
        watch.observe_thread(2, ThreadAnswer::Unanswered, now);
        assert_eq!(
            watch.condition(now + Duration::from_secs(5), budget, false),
            None
        );
        watch.observe_thread(2, ThreadAnswer::Answered, now + Duration::from_secs(5));
        watch.observe_thread(2, ThreadAnswer::Unanswered, now + Duration::from_secs(10));
        assert_eq!(
            watch.condition(now + Duration::from_secs(35), budget, false),
            None,
            "the clock restarts when it answers"
        );
        let c = watch
            .condition(now + Duration::from_secs(40), budget, false)
            .unwrap();
        assert!(
            c.reason
                .contains("chain 2: its thread has not answered for 30s"),
            "{}",
            c.reason
        );
    }

    #[test]
    fn blind_dies_need_an_armed_ceiling_a_gone_thread_does_not() {
        let now = Instant::now();
        let budget = Duration::from_secs(30);
        let mut watch = ChainWatch::default();
        watch.observe_dies(&chains(), &one_chain_frozen(), now, MAX_AGE);
        assert_eq!(watch.condition(now + budget, budget, false), None);
        watch.observe_thread(1, ThreadAnswer::Closed, now);
        assert!(watch.condition(now + budget, budget, false).is_some());
    }

    /// HOTTEST, not mean. Ninety-nine cool parts must not average away the
    /// one that is cooking.
    #[test]
    fn one_hot_die_among_many_cool_ones_still_trips() {
        let mut t = telemetry();
        t.temperatures = (0..100)
            .map(|i| die(&format!("tty9bit00-asic-{i}-dts"), 60.0))
            .collect();
        t.temperatures.push(die("tty9bit00-asic-100-dts", 110.0));
        let limits = AbortLimits {
            max_die_c: Some(105.0),
            ..Default::default()
        };
        assert!(evaluate(&t, &limits).is_some());
    }

    /// A board-level sensor named "asic" is not a die reading and must not be
    /// mistaken for one -- that conflation is how the mismap survived.
    #[test]
    fn a_board_level_asic_sensor_is_not_a_die_reading() {
        let mut t = telemetry();
        t.temperatures = vec![die("asic", 200.0), die("board", 200.0)];
        let limits = AbortLimits {
            max_die_c: Some(105.0),
            ..Default::default()
        };
        assert_eq!(evaluate(&t, &limits), None);
        assert_eq!(
            hottest_die_at(&t.temperatures, Instant::now(), MAX_AGE),
            None
        );
    }

    /// The silicon's own thermal bits need no limit of ours to be armed.
    #[test]
    fn a_silicon_thermal_trip_is_a_condition_with_no_limit_configured() {
        let mut t = telemetry();
        t.asics = vec![
            asic(0, Some(AsicFaultBits::default())),
            asic(
                7,
                Some(AsicFaultBits {
                    thermal_trip: true,
                    ..Default::default()
                }),
            ),
        ];
        let condition = evaluate(&t, &AbortLimits::default()).expect("a self-reported trip");
        assert_eq!(condition.severity, AbortSeverity::Arm);
        assert!(condition.reason.contains('7'), "{}", condition.reason);
    }

    /// A voltage fault goes straight to scram: it is not something stopping
    /// work can fix, because the load is not the cause.
    #[test]
    fn a_silicon_voltage_fault_scrams_without_arming() {
        let mut t = telemetry();
        t.asics = vec![asic(
            3,
            Some(AsicFaultBits {
                voltage_shutdown: true,
                ..Default::default()
            }),
        )];
        let condition = evaluate(&t, &AbortLimits::default()).unwrap();
        assert_eq!(condition.severity, AbortSeverity::Scram);
    }

    /// Severity order, not list order. A hot die is found before the fault
    /// scan reaches a voltage-faulted part further down; returning the arm
    /// would pick the wrong rung.
    #[test]
    fn a_scram_condition_outranks_a_simultaneous_arm_condition() {
        let mut t = telemetry();
        t.temperatures = vec![die("tty9bit00-asic-1-dts", 130.0)];
        t.asics = vec![asic(
            99,
            Some(AsicFaultBits {
                voltage_fault: true,
                ..Default::default()
            }),
        )];
        let limits = AbortLimits {
            max_die_c: Some(105.0),
            ..Default::default()
        };
        assert_eq!(
            evaluate(&t, &limits).unwrap().severity,
            AbortSeverity::Scram
        );
    }

    /// `faults: None` is "not available", never "none asserted".
    #[test]
    fn an_asic_that_has_not_spoken_is_not_a_healthy_one() {
        let mut t = telemetry();
        t.asics = vec![asic(0, None), asic(1, None)];
        assert_eq!(evaluate(&t, &AbortLimits::default()), None);
    }

    /// MY OWN COMMENT WAS THE BUG. `fan_failure` said an unread tachometer was
    /// "budgeted elsewhere"; nothing budgeted it, because `blind_limits` has
    /// only ever covered board temperature and input power. So `min_fan_rpm`
    /// -- armed by default -- sat armed against a sensor whose disappearance
    /// was tolerated silently and forever.
    #[test]
    fn every_tacho_silent_is_blindness_not_health() {
        let fans = vec![fan("fan0", None), fan("fan1", None), fan("fan2", None)];
        assert!(
            fans_blind(&fans, Some(300)),
            "a floor armed with no tacho answering is not a healthy rack"
        );
        // ...and it is still NOT a fan-death condition, which would stop the
        // machine on one dropped sysfs read.
        let mut t = telemetry();
        t.fans = fans;
        let limits = AbortLimits {
            min_fan_rpm: Some(300),
            ..Default::default()
        };
        assert_eq!(evaluate(&t, &limits), None);
    }

    #[test]
    fn one_answering_tacho_is_not_blindness() {
        let fans = vec![fan("fan0", None), fan("fan1", Some(4200))];
        assert!(!fans_blind(&fans, Some(300)));
    }

    /// No floor configured, nothing to be blind for.
    #[test]
    fn an_unarmed_floor_cannot_be_blind() {
        let fans = vec![fan("fan0", None)];
        assert!(!fans_blind(&fans, None));
    }

    /// A board that publishes no fans at all is a different fact from a board
    /// whose fans stopped answering, and must not be reported as the latter.
    #[test]
    fn a_board_with_no_fans_is_not_a_board_with_silent_fans() {
        assert!(!fans_blind(&[], Some(300)));
    }

    /// The board ceiling now reads the MCU's platform thermals, published by
    /// the heartbeat. It used to be evaluated against
    /// `MUJINA_BZM2_BOARD_TEMP_PATH` -- a sysfs path nothing in the tree sets
    /// -- so it was reported permanently unarmed while the MCU answered the
    /// question every fifth beat and our own harness wrote the answer to disk.
    #[test]
    fn the_board_ceiling_reads_the_mcu_thermal_rows() {
        let mut t = telemetry();
        t.temperatures = vec![
            die("board0-inlet", 32.0),
            die("board0-outlet", 71.0),
            die("tty9bit00-asic-1-dts", 60.0),
        ];
        let limits = AbortLimits {
            max_board_c: Some(65.0),
            ..Default::default()
        };
        let c = evaluate(&t, &limits).expect("71C over a 65C ceiling");
        assert_eq!(c.severity, AbortSeverity::Arm);
        assert!(c.reason.contains("board0-outlet"), "{}", c.reason);
    }

    /// Outlet leads inlet under load, so the HOTTER of the two decides -- an
    /// average would hide the one that matters, same as for the dies.
    #[test]
    fn the_board_ceiling_takes_the_hotter_of_inlet_and_outlet() {
        let mut t = telemetry();
        t.temperatures = vec![die("board0-inlet", 30.0), die("board0-outlet", 64.0)];
        let limits = AbortLimits {
            max_board_c: Some(65.0),
            ..Default::default()
        };
        assert_eq!(evaluate(&t, &limits), None, "64C is under a 65C ceiling");
    }

    /// A die row must not be mistaken for a board thermal, nor the reverse.
    #[test]
    fn a_die_row_does_not_satisfy_the_board_ceiling() {
        let mut t = telemetry();
        t.temperatures = vec![die("tty9bit00-asic-1-dts", 120.0)];
        let limits = AbortLimits {
            max_board_c: Some(65.0),
            ..Default::default()
        };
        assert_eq!(evaluate(&t, &limits), None);
    }

    /// Stale board thermals are no more usable than stale die rows. The MCU
    /// read is published only when it answered, so a row that stopped updating
    /// means the heartbeat stopped reading it -- not that the board cooled.
    #[test]
    fn a_stale_board_thermal_does_not_satisfy_the_ceiling() {
        let mut t = telemetry();
        t.temperatures = vec![die_aged("board0-outlet", 90.0, Duration::from_secs(3600))];
        let limits = AbortLimits {
            max_board_c: Some(65.0),
            ..Default::default()
        };
        assert_eq!(evaluate(&t, &limits), None);
    }

    /// THE GATE FOR THE WHOLE CLASS, AT THE OTHER END FROM THE CONFIG.
    ///
    /// `telemetry.rs` has a test that no *sysfs* limit is armed without a
    /// sensor bound. That one cannot see these: the die ceiling, the board
    /// ceiling and the fan floor are answered from published telemetry, not
    /// from files, so a config-side check finds nothing to inspect.
    ///
    /// This is the same question asked from the evaluator's end. For every
    /// limit the shipped configuration arms by default, build the telemetry
    /// that *should* trip it and assert that it does. A reader that is
    /// unwired, renamed, or filtered away by a new predicate fails here
    /// instead of on a rig -- which is how the die ceiling spent days armed
    /// against a sysfs path this platform does not have, and how the board
    /// ceiling spent longer armed against one nothing sets at all.
    #[test]
    fn every_limit_armed_by_default_has_a_working_reader() {
        let shipped = crate::board::bzm2::telemetry::Bzm2TelemetryConfig::from_env();
        let limits = AbortLimits {
            max_die_c: shipped.max_asic_temp_c,
            min_fan_rpm: shipped.min_fan_rpm,
            max_rail_v: shipped.max_rail_v,
            max_board_c: shipped.max_board_temp_c,
        };

        // (armed?, telemetry that must trip it, what it is)
        let cases: Vec<(bool, BoardTelemetry, &str)> = vec![
            (
                limits.max_die_c.is_some(),
                {
                    let mut t = telemetry();
                    let over = limits.max_die_c.unwrap_or(0.0) + 10.0;
                    t.temperatures = vec![die("tty9bit00-asic-7-dts", over)];
                    t
                },
                "die ceiling",
            ),
            (
                limits.max_board_c.is_some(),
                {
                    let mut t = telemetry();
                    let over = limits.max_board_c.unwrap_or(0.0) + 10.0;
                    t.temperatures = vec![die("board0-outlet", over)];
                    t
                },
                "board ceiling",
            ),
            (
                limits.min_fan_rpm.is_some(),
                {
                    let mut t = telemetry();
                    let under = limits.min_fan_rpm.unwrap_or(0).saturating_sub(1);
                    t.fans = vec![fan("fan0", Some(under))];
                    t
                },
                "fan floor",
            ),
            (
                limits.max_rail_v.is_some(),
                {
                    let mut t = telemetry();
                    let over = limits.max_rail_v.unwrap_or(0.0) + 1.0;
                    t.powers = vec![PowerMeasurement {
                        name: "rail0-output".into(),
                        voltage_v: Some(over),
                        current_a: None,
                        power_w: None,
                    }];
                    t
                },
                "rail ceiling",
            ),
        ];

        let mut armed = 0;
        for (is_armed, t, what) in cases {
            if !is_armed {
                continue;
            }
            armed += 1;
            assert!(
                evaluate(&t, &limits).is_some(),
                "the {what} is ARMED by the shipped configuration and nothing in published \
                 telemetry can make it fire. An armed limit with no reader is not protection."
            );
        }
        assert!(
            armed >= 2,
            "the shipped configuration should arm at least the die ceiling and the fan floor; \
             it armed {armed}. If a default was deliberately removed, change this number and \
             say why -- silently shipping with fewer trips is the thing to notice."
        );
    }

    /// COVERAGE MUST NAME THE DANGEROUS CASE DISTINCTLY. "Armed and nothing
    /// answering" is the state the die ceiling sat in for days and the board
    /// ceiling sat in for longer, and from outside it looked exactly like a
    /// board within limits. It must not collapse into "observed" or "absent".
    #[test]
    fn coverage_separates_armed_but_blind_from_every_other_state() {
        let now = Instant::now();
        let mut t = telemetry();

        // Armed die ceiling, no rows at all: the original defect.
        let limits = AbortLimits {
            max_die_c: Some(100.0),
            ..Default::default()
        };
        let rows = coverage(&t, &limits, now, MAX_AGE);
        let row = rows
            .iter()
            .find(|(n, _)| *n == "asic die temperature")
            .unwrap();
        assert!(row.1.is_armed_but_blind(), "got {:?}", row.1);
        assert_eq!(row.1.label(), "ARMED-BUT-BLIND");

        // Rows present but ALL STALE is still blind -- a frozen reading is not
        // an answer, which is the shape this fix closed.
        t.temperatures = vec![die_aged("c-asic-1-dts", 60.0, Duration::from_secs(3600))];
        let rows = coverage(&t, &limits, now, MAX_AGE);
        let row = rows
            .iter()
            .find(|(n, _)| *n == "asic die temperature")
            .unwrap();
        assert!(row.1.is_armed_but_blind(), "stale rows are not coverage");
        assert_eq!(row.1.counts(), (1, 0), "one row, none usable");

        // Fresh rows: protected.
        t.temperatures = vec![die("c-asic-1-dts", 60.0)];
        let rows = coverage(&t, &limits, now, MAX_AGE);
        let row = rows
            .iter()
            .find(|(n, _)| *n == "asic die temperature")
            .unwrap();
        assert_eq!(row.1, Coverage::Protected { rows: 1, fresh: 1 });

        // Readings with no limit are NOT protection and must say so.
        let rows = coverage(&t, &AbortLimits::default(), now, MAX_AGE);
        let row = rows
            .iter()
            .find(|(n, _)| *n == "asic die temperature")
            .unwrap();
        assert_eq!(row.1.label(), "observed, not armed");
        assert!(!row.1.is_armed_but_blind());
    }

    /// The exact divergence the inventory hid: the harness recording a sensor
    /// says nothing about whether the miner can act on it. Coverage is
    /// computed from the miner's own telemetry, so a board with no inlet rows
    /// reports blind however much the harness has written to disk.
    #[test]
    fn coverage_answers_for_the_miner_not_for_the_evidence() {
        let mut t = telemetry();
        t.temperatures = vec![die("c-asic-1-dts", 60.0)];
        let limits = AbortLimits {
            max_board_c: Some(65.0),
            ..Default::default()
        };
        let rows = coverage(&t, &limits, Instant::now(), MAX_AGE);
        let board = rows
            .iter()
            .find(|(n, _)| *n == "board inlet/outlet")
            .unwrap();
        assert!(
            board.1.is_armed_but_blind(),
            "a board ceiling with no board rows in the MINER's telemetry is blind, \
             whatever mcu.tsv contains"
        );
    }

    /// FAULT BITS AGE LIKE EVERY OTHER READING.
    ///
    /// Measured on hardware: 37 s after the
    /// stream carrying them stopped, this class read 100 rows, 100 fresh,
    /// PROTECTED. The bits were still held; the parts were no longer sending
    /// them. The die class caught the same blindness and scrammed, so nothing
    /// was unsafe -- but a coverage verdict that says protected while blind is
    /// the one thing this table exists to prevent.
    #[test]
    fn stale_fault_bits_are_not_coverage() {
        let now = Instant::now();
        let mut t = telemetry();
        let limits = AbortLimits::default();
        let faults = |t: &BoardTelemetry| {
            coverage(t, &limits, now, MAX_AGE)
                .into_iter()
                .find(|(n, _)| *n == "silicon self-reported faults")
                .unwrap()
                .1
        };

        let mut stale = asic(0, Some(AsicFaultBits::default()));
        stale.observed_at = Some(now - Duration::from_secs(37));
        t.asics = vec![stale];
        let blind = faults(&t);
        assert!(
            blind.is_armed_but_blind(),
            "37 s old fault bits read {blind:?}"
        );
        assert_eq!(blind.counts(), (1, 0), "one row, none usable");

        let mut fresh = asic(0, Some(AsicFaultBits::default()));
        fresh.observed_at = Some(now);
        t.asics = vec![fresh];
        assert_eq!(faults(&t), Coverage::Protected { rows: 1, fresh: 1 });

        // Bits with no arrival time cannot be told from ones an hour old.
        t.asics = vec![asic(0, Some(AsicFaultBits::default()))];
        assert!(
            faults(&t).is_armed_but_blind(),
            "an undated bit is not fresh"
        );
    }

    /// THE FAILURE NO THERMAL CHECK CAN SEE. A chain being given work that has
    /// stopped returning results, while its telemetry keeps flowing — the dies
    /// read cool BECAUSE nothing is hashing, so every thermal rule passes.
    #[test]
    fn a_chain_that_stops_returning_results_is_a_condition() {
        let t0 = Instant::now();
        let budget = Duration::from_secs(120);
        let mut w = StallWatch::default();
        w.observe(0, 5_000, true, t0);
        // The counter is read many times and never moves. Reading it is not
        // evidence of life; CHANGING is.
        for k in 1..=10 {
            w.observe(0, 5_000, true, t0 + Duration::from_secs(k * 20));
        }
        let stalled = w.stalled(t0 + Duration::from_secs(200), budget);
        assert_eq!(stalled.len(), 1, "one chain silent for 200 s");
        let c = stall_condition(&stalled, budget).expect("a stall is a condition");
        assert_eq!(
            c.severity,
            AbortSeverity::Arm,
            "stop the work first, then escalate"
        );
        assert!(c.reason.contains("COOL"), "{}", c.reason);
    }

    /// A chain that is still answering is not stalled, however slowly the
    /// counter climbs.
    #[test]
    fn a_chain_still_returning_results_is_not_stalled() {
        let t0 = Instant::now();
        let budget = Duration::from_secs(120);
        let mut w = StallWatch::default();
        for k in 0..20u64 {
            w.observe(0, 5_000 + k, true, t0 + Duration::from_secs(k * 30));
        }
        assert!(w.stalled(t0 + Duration::from_secs(600), budget).is_empty());
    }

    /// THE CASE THAT MAKES THIS HONEST. Observation runs dispatch nothing, so
    /// no chain returns results and every one of them would look stalled. A
    /// condition that fired on an idle board would have fired on every run we
    /// have ever done.
    #[test]
    fn a_chain_that_is_not_being_given_work_cannot_stall() {
        let t0 = Instant::now();
        let budget = Duration::from_secs(120);
        let mut w = StallWatch::default();
        w.observe(0, 0, false, t0);
        w.observe(0, 0, false, t0 + Duration::from_secs(600));
        assert!(
            w.stalled(t0 + Duration::from_secs(600), budget).is_empty(),
            "an idle board is idle, not stalled"
        );
    }

    /// And the clock starts when work starts, not when the process did: a
    /// chain that has just been given work has not been silent for an hour.
    #[test]
    fn the_stall_clock_starts_when_dispatch_does() {
        let t0 = Instant::now();
        let budget = Duration::from_secs(120);
        let mut w = StallWatch::default();
        w.observe(0, 0, false, t0);
        // ...an hour of observing...
        let t1 = t0 + Duration::from_secs(3600);
        w.observe(0, 0, true, t1);
        assert!(
            w.stalled(t1 + Duration::from_secs(60), budget).is_empty(),
            "60 s into dispatch is not past a 120 s budget"
        );
        assert_eq!(w.stalled(t1 + Duration::from_secs(130), budget).len(), 1);
    }

    /// Several chains stall independently and the reason names each.
    #[test]
    fn each_stalled_chain_is_named() {
        let t0 = Instant::now();
        let budget = Duration::from_secs(120);
        let mut w = StallWatch::default();
        w.observe(0, 1, true, t0);
        w.observe(2, 1, true, t0);
        w.observe(1, 1, true, t0);
        w.observe(1, 2, true, t0 + Duration::from_secs(150));
        let stalled = w.stalled(t0 + Duration::from_secs(200), budget);
        assert_eq!(stalled.len(), 2, "chains 0 and 2, not 1");
        let reason = stall_condition(&stalled, budget).unwrap().reason;
        assert!(reason.contains("chain 0"), "{reason}");
        assert!(reason.contains("chain 2"), "{reason}");
        assert!(!reason.contains("chain 1"), "{reason}");
    }

    #[test]
    fn a_stopped_fan_is_a_condition() {
        let mut t = telemetry();
        t.fans = vec![fan("fan0", Some(4200)), fan("fan1", Some(0))];
        let condition = evaluate(&t, &AbortLimits::default()).unwrap();
        assert_eq!(condition.severity, AbortSeverity::Arm);
        assert!(condition.reason.contains("fan1"), "{}", condition.reason);
    }

    /// An unread tachometer is blindness, not death. Treating it as death
    /// stops the machine every time a sysfs read hiccups.
    #[test]
    fn an_unread_tachometer_is_not_a_dead_fan() {
        let mut t = telemetry();
        t.fans = vec![fan("fan0", None), fan("fan1", None)];
        let limits = AbortLimits {
            min_fan_rpm: Some(500),
            ..Default::default()
        };
        assert_eq!(evaluate(&t, &limits), None);
    }

    #[test]
    fn a_fan_below_the_floor_is_reported_separately_from_a_stopped_one() {
        let mut t = telemetry();
        t.fans = vec![fan("fan0", Some(120))];
        let limits = AbortLimits {
            min_fan_rpm: Some(500),
            ..Default::default()
        };
        let reason = evaluate(&t, &limits).unwrap().reason;
        assert!(reason.contains("floor"), "{reason}");
        assert!(reason.contains("120"), "{reason}");
    }

    /// With no floor configured, a slow fan is not a condition -- but a
    /// stopped one still is, because zero needs no threshold to interpret.
    #[test]
    fn a_stopped_fan_needs_no_configured_floor() {
        let mut t = telemetry();
        t.fans = vec![fan("fan0", Some(120)), fan("fan1", Some(0))];
        let condition = evaluate(&t, &AbortLimits::default()).unwrap();
        assert!(condition.reason.contains("0 rpm"), "{}", condition.reason);
    }

    #[test]
    fn a_rail_above_its_ceiling_scrams() {
        let mut t = telemetry();
        t.powers = vec![PowerMeasurement {
            name: "rail0-output".into(),
            voltage_v: Some(0.42),
            current_a: None,
            power_w: None,
        }];
        let limits = AbortLimits {
            max_rail_v: Some(0.40),
            ..Default::default()
        };
        assert_eq!(
            evaluate(&t, &limits).unwrap().severity,
            AbortSeverity::Scram
        );
    }

    /// No limit, no trip -- and with no limit an out-of-range reading is not
    /// a condition, it is just a reading.
    #[test]
    fn nothing_trips_when_nothing_is_configured_and_nothing_is_faulted() {
        let mut t = telemetry();
        t.temperatures = vec![die("tty9bit00-asic-1-dts", 130.0)];
        t.fans = vec![fan("fan0", Some(120))];
        t.powers = vec![PowerMeasurement {
            name: "rail0-output".into(),
            voltage_v: Some(9.9),
            current_a: None,
            power_w: None,
        }];
        assert_eq!(evaluate(&t, &AbortLimits::default()), None);
    }

    #[test]
    fn a_long_fault_list_says_it_was_truncated() {
        let ids: Vec<u8> = (0..30).collect();
        let rendered = id_list(&ids);
        assert!(rendered.contains("and 22 more"), "{rendered}");
    }
}
