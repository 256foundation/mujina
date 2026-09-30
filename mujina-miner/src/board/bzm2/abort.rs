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
