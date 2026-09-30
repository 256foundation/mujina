//! Runtime monitor loop and tuning evaluation for the BZM2 board.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::watch;

use crate::api_client::types::{
    AsicState, Bzm2AsicTuningState, Bzm2DomainTuningState, Bzm2PllTuningState, Bzm2ResultCounts,
    Bzm2SavedOperatingPointStatus, Bzm2TuningState, EfficiencyReport, EfficiencyWindow,
    EngineCoordinate, TemperatureSensor,
};
use crate::asic::bzm2::thread::ShutdownAsk;
use crate::asic::bzm2::thread::telemetry::sensor_prefix;
use crate::asic::bzm2::{Bzm2ThreadHandle, Bzm2ThreadRuntimeMetrics};
use crate::asic::hash_thread::HashThreadError;
use crate::tracing::prelude::*;
use crate::tuning::calibration_planner::{
    Bzm2AsicMeasurement, Bzm2BoardCalibrationInput, Bzm2CalibrationConstraints,
    Bzm2CalibrationPlanner, Bzm2DomainMeasurement, Bzm2SavedEngineCoordinate,
    Bzm2SavedEngineTopology,
};
use crate::types::{EfficiencyTracker, HashRate, PowerDomain, PowerReading, Temperature};

use super::Bzm2Board;
use super::abort::{self, AbortCondition, AbortLimits, AbortSeverity};
use super::bringup::Bzm2BringupConfig;
use super::calibration::{
    Bzm2AppliedOperatingState, Bzm2BusLayout, build_topology, build_voltage_domains,
    default_saved_engine_topology, store_saved_operating_point_status,
};
use super::config::{Bzm2CalibrationConfig, DEFAULT_CALIBRATION_SITE_TEMP_C};
use super::scram::{ScramAction, ScramLadder};
use super::telemetry::{
    Bzm2TelemetrySnapshot, merge_power_readings, merge_temperature_readings, snapshot_input_power,
};

#[derive(Debug, Clone, Default)]
pub(super) struct Bzm2RuntimeMeasurementCache {
    domain_measurements: BTreeMap<u16, Bzm2DomainMeasurement>,
    asic_measurements: BTreeMap<u16, Bzm2AsicMeasurement>,
}

#[derive(Debug, Clone, Default)]
struct Bzm2RetuneTriggerTracker {
    throughput_regression_polls: u8,
    thermal_drift_polls: u8,
    voltage_imbalance_polls: u8,
}

impl Bzm2Board {
    pub(super) fn spawn_monitor(&mut self) {
        if (!self.config.telemetry.is_enabled() && !self.config.bringup.has_telemetry())
            || self.monitor_task.is_some()
        {
            return;
        }

        let telemetry = self.config.telemetry.clone();
        let rail_telemetry = self.config.bringup.clone();
        let calibration = self.config.calibration.clone();
        let telemetry_tx = self.telemetry_tx.clone();
        let shutdown_handles = self.shutdown_handles.clone();
        let serial_controls = self.serial_controls.clone();
        let bus_layouts = Arc::clone(&self.bus_layouts);
        let applied_operating_state = Arc::clone(&self.applied_operating_state);
        let runtime_measurements = Arc::clone(&self.runtime_measurements);
        let board_name = self.config.device_id();
        // THE SCRAM HANDLER'S TWO LEVERS, captured here because the monitor is
        // a detached task and cannot reach `&mut self` when it needs them.
        //
        // The heartbeat sender is the more important of the two. Stopping the
        // beat does not de-energise anything by itself -- it re-arms the MCU's
        // own shed, measured on this hardware at 74-80 s, which is the only
        // part of this that still works if the I2C actuation below fails or
        // this task dies mid-scram.
        let heartbeat_shutdown = self.heartbeat_shutdown.clone();
        let scram_boards = self.driven_board_indices();
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        self.monitor_shutdown = Some(shutdown_tx);

        self.monitor_task = Some(tokio::spawn(async move {
            // How long a configured limit may go unmeasurable before it is a
            // trip. Eight poll intervals, floored at thirty seconds: long
            // enough that a transient read failure costs nothing, short enough
            // that nobody mines for minutes with a limit unwatched.
            let blind_budget = std::cmp::max(
                telemetry.poll_interval.saturating_mul(8),
                std::time::Duration::from_secs(30),
            );
            let mut blind_since: Option<tokio::time::Instant> = None;
            // SAY WHAT IS PROTECTING THIS BOARD, AND WHAT IS NOT.
            //
            // The monitor spawns whenever telemetry PUBLISHES, which since the
            // fan list gained defaults is always -- so it starts, reports fans,
            // and may evaluate trips that are all unconfigured. From outside
            // that is indistinguishable from a protected machine. An operator
            // who is told nothing will reasonably assume the monitor running is
            // the monitor guarding.
            let unarmed = telemetry.unarmed_limits();
            if !telemetry.any_trip_armed() {
                warn!(
                    board = %board_name,
                    "BZM2 board monitor running with NO trip armed: telemetry is \
                     published and nothing will stop this board. Unarmed: {}",
                    unarmed.join(", ")
                );
            } else if !unarmed.is_empty() {
                info!(
                    board = %board_name,
                    "BZM2 board monitor armed, but these limits are unset and \
                     cannot trip: {}",
                    unarmed.join(", ")
                );
            }
            let mut interval = tokio::time::interval(telemetry.poll_interval);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut retune_tracker = Bzm2RetuneTriggerTracker::default();
            let mut efficiency = EfficiencyTracker::new();
            let mut ladder = ScramLadder::new();
            // The die ceiling's number lives in the telemetry config; the
            // sensor that answers it lives in published telemetry. This is
            // where the two are put together, and putting them together is
            // the whole fix.
            let abort_limits = AbortLimits {
                max_die_c: telemetry.max_asic_temp_c,
                min_fan_rpm: telemetry.min_fan_rpm,
                max_rail_v: telemetry.max_rail_v,
                max_board_c: telemetry.max_board_temp_c,
            };
            let mut die_blind_since: Option<Instant> = None;
            // Budgeted separately from the die rows and from the sysfs limits,
            // because all three can go blind independently and a shared clock
            // lets one mask another's recovery.
            let mut fan_blind_since: Option<Instant> = None;
            let mut stall_watch = abort::StallWatch::default();
            // Per chain, not per board: see `abort::ChainWatch`.
            let mut chain_watch = abort::ChainWatch::default();
            // HOW LONG MAY A CHAIN BE SILENT AND STILL BE WORKING?
            //
            // NOT MEASURED. We have never dispatched work to this hardware, so
            // nothing in this tree knows the rate at which a healthy chain
            // returns result frames. This number is therefore deliberately
            // generous and deliberately labelled: it is a placeholder that
            // refuses to halt a slow-but-live chain, not a limit derived from
            // anything.
            //
            // A real dispatch is where it gets a real value. The first run that
            // dispatches work will show the actual inter-result interval, and
            // that measurement replaces this -- the same way the die ceiling
            // and the thermal scale were settled.
            let stall_budget = std::time::Duration::from_secs(
                std::env::var("MUJINA_BZM2_STALL_BUDGET_S")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(120),
            );
            // HOW OLD MAY A DIE READING BE AND STILL BE A MEASUREMENT?
            //
            // Derived from the poll interval rather than fixed, so it stays
            // correct if the rate is changed: a constant that is generous at
            // 5 s is nonsense at 30 s. Three polls, floored at 15 s -- which
            // is 75x the 200 ms rate at which the threads publish, so no
            // healthy jitter or brief stall can reach it, while a stream that
            // has actually stopped is caught well inside the blindness budget.
            let die_max_age = std::cmp::max(
                telemetry.poll_interval.saturating_mul(3),
                std::time::Duration::from_secs(15),
            );
            // SAY PERIODICALLY THAT THIS IS STILL RUNNING, AND WHAT IT SEES.
            //
            // The monitor used to emit one line -- at startup -- and nothing
            // ever again. From the log, a monitor polling happily and a
            // monitor that exited three minutes ago are the same thing: no
            // lines. One capture could not measure whether the loop survived
            // its hold for exactly this reason, and "is my protection running"
            // is not a question evidence should be unable to answer.
            //
            // Budgeted by TIME rather than by poll count, so the rate does not
            // change with the poll interval, and carrying the reading rather
            // than just a pulse: a liveness line that does not say what the
            // monitor can currently see proves the task is alive without
            // proving it is doing anything.
            const LIVENESS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
            let mut last_liveness: Option<Instant> = None;
            let mut polls_since_liveness: u64 = 0;
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let now = Instant::now();
                        let snapshot = telemetry.snapshot();
                        let rail_snapshot = rail_telemetry.snapshot_telemetry();
                        let (thread_metrics, thread_answers) =
                            collect_thread_runtime_metrics(&shutdown_handles).await;
                        let bus_layouts = bus_layouts.lock().unwrap_or_else(|e| e.into_inner()).clone();
                        let applied_operating_snapshot = applied_operating_state.lock().unwrap_or_else(|e| e.into_inner()).clone();
                        let current_state = telemetry_tx.borrow().clone();
                        let (tuning_state, measurement_cache) = build_runtime_tuning_state(
                            &current_state.asics,
                            &current_state.temperatures,
                            &bus_layouts,
                            &calibration,
                            &rail_telemetry,
                            &rail_snapshot,
                            &applied_operating_snapshot,
                            &thread_metrics,
                        );
                        let tuning_state = apply_runtime_tuning_plan(
                            tuning_state,
                            evaluate_runtime_tuning_plan(
                                &current_state.asics,
                                &current_state.temperatures,
                                &bus_layouts,
                                &calibration,
                                &applied_operating_snapshot,
                                &measurement_cache,
                            ),
                        );
                        let tuning_state = apply_runtime_retune_triggers(
                            tuning_state,
                            &calibration,
                            &measurement_cache,
                            &mut retune_tracker,
                        );
                        let tuning_state = reconcile_saved_operating_point_status(
                            tuning_state,
                            &calibration,
                            &bus_layouts,
                            &applied_operating_state,
                        );
                        let runtime_domain_count = measurement_cache.domain_measurements.len();
                        let runtime_asic_count = measurement_cache.asic_measurements.len();
                        record_efficiency_samples(
                            &mut efficiency,
                            now,
                            &snapshot,
                            &measurement_cache,
                            tuning_state.board_throughput_hs,
                        );
                        let efficiency_report = build_efficiency_report(&efficiency, now);
                        *runtime_measurements.lock().unwrap_or_else(|e| e.into_inner()) = measurement_cache;
                        let total_stats = serial_controls.iter().fold((0u64, 0u64), |acc, control| {
                            let stats = control.stats();
                            (acc.0 + stats.bytes_read, acc.1 + stats.bytes_written)
                        });
                        telemetry_tx.send_modify(|state| {
                            state.fans = snapshot.fans.clone();
                            merge_temperature_readings(&mut state.temperatures, &snapshot.temperatures);
                            merge_power_readings(&mut state.powers, &snapshot.powers);
                            merge_temperature_readings(&mut state.temperatures, &rail_snapshot.temperatures);
                            merge_power_readings(&mut state.powers, &rail_snapshot.powers);
                            state.bzm2_tuning =
                                (!tuning_state.asics.is_empty() || !tuning_state.domains.is_empty()
                                    || tuning_state.board_throughput_hs.is_some())
                                    .then_some(tuning_state.clone());
                            state.efficiency = efficiency_report.clone();
                        });
                        trace!(
                            board = %board_name,
                            bytes_read = total_stats.0,
                            bytes_written = total_stats.1,
                            runtime_domain_count,
                            runtime_asic_count,
                            "BZM2 board telemetry updated"
                        );
                        // BLINDNESS THAT PERSISTS IS A TRIP.
                        //
                        // A configured limit whose sensor stops answering used
                        // to read exactly like a sensor answering safely, so a
                        // node could vanish and the machine would mine on with
                        // that protection silently gone. It is tolerated for a
                        // bounded time -- one dropped read must not halt 3 kW --
                        // and then it is a trip, because a limit nobody can
                        // measure is not protection.
                        //
                        // The budget is a DURATION, converted by the poll
                        // interval rather than counted in cycles: a cycle count
                        // means a different amount of blindness at every poll
                        // rate, and the thing that matters is how long the rig
                        // ran unwatched.
                        // Blindness is budgeted; an over-limit reading is
                        // not. A configured limit whose sensor stops answering
                        // is tolerated for a bounded time -- one dropped read
                        // must not stop 3 kW -- and then it IS the condition,
                        // because a limit nobody can measure is not protection.
                        //
                        // The budget is a DURATION converted by the poll
                        // interval, not a cycle count: a cycle count means a
                        // different amount of blindness at every poll rate, and
                        // what matters is how long the rig ran unwatched.
                        let blind_condition = if snapshot.blind.is_empty() {
                            blind_since = None;
                            None
                        } else {
                            let tnow = tokio::time::Instant::now();
                            let started = *blind_since.get_or_insert(tnow);
                            let blind_for = tnow.duration_since(started);
                            (blind_for >= blind_budget).then(|| {
                                format!(
                                    "no reading for {} for {:.0}s, past the {:.0}s \
                                     blindness budget: the limit is configured and \
                                     cannot be measured, which is not the same as \
                                     being within it",
                                    snapshot.blind.join(", "),
                                    blind_for.as_secs_f32(),
                                    blind_budget.as_secs_f32(),
                                )
                            })
                        };
                        // CONDITIONS COMPUTED FROM PUBLISHED TELEMETRY: the
                        // per-ASIC die temperatures, the silicon's own fault
                        // bits, the fan tachometers and the regulator outputs.
                        // These are read from the same `BoardTelemetry` the
                        // tuning pass above already consumes -- the state that
                        // was sitting unread while the one armed limit was
                        // bound to a sysfs path this platform does not have.
                        // A CHAIN THAT STOPPED PRODUCING IS NOT VISIBLE TO ANY
                        // THERMAL CHECK. Folded in here, from the counters the
                        // metrics pass above already collected, so there is no
                        // second poll and no second source for the same fact.
                        for (index, metrics) in &thread_metrics {
                            // `is_active` is the board's own statement that
                            // this thread is being given work; an observation
                            // run dispatches nothing and must never stall.
                            let dispatching = current_state
                                .threads
                                .get(*index)
                                .map(|t| t.is_active)
                                .unwrap_or(false);
                            stall_watch.observe(
                                *index,
                                metrics.results.decoded,
                                dispatching,
                                now,
                            );
                        }
                        let stalled = stall_watch.stalled(now, stall_budget);

                        let published = telemetry_tx.borrow().clone();
                        // Chains by the name their die rows carry: the serial
                        // port's file name, the same prefix the threads publish.
                        let chain_names = chain_names(&bus_layouts);
                        chain_watch.observe_dies(&chain_names, &published.temperatures, now, die_max_age);
                        for (index, answer) in &thread_answers {
                            chain_watch.observe_thread(*index, *answer, now);
                        }
                        let condition = abort::evaluate(&published, &abort_limits, now, die_max_age)
                            .or_else(|| abort::stall_condition(&stalled, stall_budget))
                            .or_else(|| {
                                chain_watch.condition(now, blind_budget, abort_limits.max_die_c.is_some())
                            })
                            // A sysfs trip is true now and needs no budget.
                            .or_else(|| {
                                snapshot
                                    .trip_reason
                                    .clone()
                                    .map(|reason| AbortCondition {
                                        severity: AbortSeverity::Arm,
                                        reason,
                                    })
                            })
                            // Blindness last: it is the weakest claim of the
                            // three. It says only that we cannot see, which
                            // matters, but not as much as something we can.
                            .or_else(|| {
                                die_blind_condition(&published, &abort_limits, blind_budget, now, die_max_age, &mut die_blind_since)
                                    .or_else(|| {
                                        fan_blind_condition(
                                            &published,
                                            &abort_limits,
                                            blind_budget,
                                            now,
                                            &mut fan_blind_since,
                                        )
                                    })
                                    .or(blind_condition)
                                    .map(|reason| AbortCondition {
                                        severity: AbortSeverity::Arm,
                                        reason,
                                    })
                            });

                        polls_since_liveness += 1;
                        let due = last_liveness
                            .map(|at| now.saturating_duration_since(at) >= LIVENESS_INTERVAL)
                            .unwrap_or(true);
                        if due {
                            last_liveness = Some(now);
                            let hottest =
                                abort::hottest_die_at(&published.temperatures, now, die_max_age);
                            // WHAT THIS BOARD CAN SEE, AND WHAT IT IS ACTING ON.
                            //
                            // Derived from the telemetry actually held and the
                            // limits actually armed, then written into the run
                            // record -- so nobody has to consult a document
                            // that can disagree with the machine. The document
                            // did disagree, in both directions, on the two
                            // sensors that mattered most.
                            for (signal, cover) in
                                abort::coverage(&published, &abort_limits, now, die_max_age)
                            {
                                let (rows, fresh) = cover.counts();
                                if cover.is_armed_but_blind() {
                                    warn!(
                                        board = %board_name, signal, rows, fresh,
                                        verdict = cover.label(),
                                        "COVERAGE: a limit is armed and nothing can make it fire"
                                    );
                                } else {
                                    info!(
                                        board = %board_name, signal, rows, fresh,
                                        verdict = cover.label(),
                                        "COVERAGE"
                                    );
                                }
                            }
                            info!(
                                board = %board_name,
                                polls = polls_since_liveness,
                                armed = ladder.is_armed(),
                                hottest_die = hottest.map(|(name, _)| name),
                                hottest_c = hottest.map(|(_, c)| c),
                                die_rows = abort::die_row_count(&published.temperatures),
                                die_rows_fresh = published
                                    .temperatures
                                    .iter()
                                    .filter(|s| s.name.ends_with("-dts"))
                                    .filter(|s| s.observed_at.is_some_and(|at| {
                                        now.saturating_duration_since(at) <= die_max_age
                                    }))
                                    .count(),
                                condition = condition.as_ref().map(|c| c.reason.as_str()),
                                "BZM2 board monitor alive"
                            );
                            polls_since_liveness = 0;
                        }

                        match ladder.observe(now, condition.as_ref()) {
                            ScramAction::Nothing | ScramAction::Hold => {}
                            ScramAction::Arm { reason } => {
                                warn!(
                                    board = %board_name,
                                    reason = %reason,
                                    "BZM2 SAFETY TRIP: stopping work on this board. The rails \
                                     stay up and the monitor keeps watching -- if this does not \
                                     clear, or it comes back, the board will be de-energised."
                                );
                                stop_dispatch(&board_name, &shutdown_handles, &telemetry_tx).await;
                            }
                            ScramAction::Disarm { armed_for } => {
                                // Say plainly what did and did not come back.
                                // Dispatch is gone for the life of this process
                                // -- the threads were stopped and nothing here
                                // can restart them -- and claiming otherwise
                                // would be the same class of error as claiming
                                // a board is off because the write succeeded.
                                warn!(
                                    board = %board_name,
                                    armed_for_s = armed_for.as_secs_f32(),
                                    "BZM2 safety trip cleared and stayed clear. The rails stay \
                                     up: this board is stopped, cooling and reading within every \
                                     armed limit. Work does NOT resume; restart the miner."
                                );
                            }
                            ScramAction::Scram { reason } => {
                                error!(
                                    board = %board_name,
                                    reason = %reason,
                                    "BZM2 SCRAM: de-energising this board."
                                );
                                stop_dispatch(&board_name, &shutdown_handles, &telemetry_tx).await;
                                scram(&board_name, &heartbeat_shutdown, &scram_boards).await;
                                break;
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
}

/// Is the die ceiling armed with nothing answering it?
///
/// The counterpart to `Bzm2TelemetryConfig::blind_limits`, which cannot see
/// this: the die sensor is not a file, it is the DTS stream, and a chain that
/// stops speaking produces no rows at all rather than a failed read.
///
/// Budgeted like every other blindness, and for the same reason -- a single
/// poll that catches the publish between updates must not stop 3 kW. But it is
/// budgeted separately from the sysfs limits, because they can go blind
/// independently and a shared clock would let one mask the other's recovery.
fn die_blind_condition(
    published: &crate::api_client::types::BoardTelemetry,
    limits: &AbortLimits,
    budget: std::time::Duration,
    now: Instant,
    max_age: std::time::Duration,
    since: &mut Option<Instant>,
) -> Option<String> {
    if limits.max_die_c.is_none()
        || abort::hottest_die_at(&published.temperatures, now, max_age).is_some()
    {
        *since = None;
        return None;
    }
    // SAY WHICH KIND OF BLINDNESS THIS IS. No rows at all and rows that all
    // went stale are different faults -- a chain that never attached versus
    // one that stopped talking -- and an operator acts differently on each.
    let rows = abort::die_row_count(&published.temperatures);
    let started = *since.get_or_insert(now);
    let blind_for = now.saturating_duration_since(started);
    (blind_for >= budget).then(|| {
        if rows == 0 {
            format!(
                "no per-ASIC die temperature row exists at all, for {:.0}s, past the {:.0}s \
                 blindness budget. The die ceiling is armed and NOTHING is publishing DTS rows.",
                blind_for.as_secs_f32(),
                budget.as_secs_f32(),
            )
        } else {
            format!(
                "all {rows} per-ASIC die rows are older than {:.0}s, and have been unusable for \
                 {:.0}s -- past the {:.0}s blindness budget. The rows still hold their last \
                 values, which is not the same as the dies still being at them.",
                max_age.as_secs_f32(),
                blind_for.as_secs_f32(),
                budget.as_secs_f32(),
            )
        }
    })
}

/// Is the fan floor armed with no tachometer answering it?
///
/// `min_fan_rpm` is armed by default, and its sensor was never covered by any
/// blindness budget: `blind_limits` has only ever known about board
/// temperature and input power. So a tachometer that stopped answering was
/// tolerated silently and forever, while the floor sat armed against nothing
/// -- the same shape as the die ceiling bound to a path that does not exist.
///
/// Budgeted, like every other blindness, because one dropped sysfs read must
/// not stop a 3 kW machine.
fn fan_blind_condition(
    published: &crate::api_client::types::BoardTelemetry,
    limits: &AbortLimits,
    budget: std::time::Duration,
    now: Instant,
    since: &mut Option<Instant>,
) -> Option<String> {
    if !abort::fans_blind(&published.fans, limits.min_fan_rpm) {
        *since = None;
        return None;
    }
    let started = *since.get_or_insert(now);
    let blind_for = now.saturating_duration_since(started);
    (blind_for >= budget).then(|| {
        format!(
            "no fan tachometer has answered for {:.0}s, past the {:.0}s blindness budget. All \
             {} fans are configured and none reports an rpm: the {} rpm floor is armed against \
             nothing, and a stopped fan would read exactly like this.",
            blind_for.as_secs_f32(),
            budget.as_secs_f32(),
            published.fans.len(),
            limits.min_fan_rpm.unwrap_or(0),
        )
    })
}

/// Stop every hash thread and say how many actually accepted the stop.
///
/// COUNT WHAT WAS ACCEPTED. A stop the channel refused is a thread that was
/// never asked, and on a safety trip that is the difference between a board
/// that stops making heat and one that does not. A full channel means a busy
/// thread -- precisely the thread this is trying to stop.
async fn stop_dispatch(
    board_name: &str,
    handles: &[Bzm2ThreadHandle],
    telemetry_tx: &watch::Sender<crate::api_client::types::BoardTelemetry>,
) {
    let asked = handles.len();
    // CLOSED IS NOT REFUSED. A closed channel means the thread has already
    // stopped -- which is what every ask after the first one looks like, and
    // it is the outcome we wanted. Only a FULL channel is a thread that is
    // alive and not listening, and only that is worth shouting about.
    //
    // Measured on hardware: the ladder armed, stopped the threads, and
    // escalated 95 s later. The scram's ask found closed channels and this
    // line claimed every one of them was "still dispatching work" -- the most
    // alarming possible message, at the most critical moment, about a
    // condition that was not happening.
    let mut still_running = 0usize;
    let mut already_stopped = 0usize;
    for handle in handles {
        match handle.shutdown() {
            ShutdownAsk::Accepted => {}
            ShutdownAsk::AlreadyStopped => already_stopped += 1,
            ShutdownAsk::Refused => still_running += 1,
        }
    }
    if still_running > 0 {
        error!(
            board = %board_name,
            still_running,
            asked,
            "SAFETY TRIP COULD NOT BE DELIVERED to every thread. \
             The ones that refused are ALIVE and still dispatching work."
        );
    } else if already_stopped > 0 {
        debug!(
            board = %board_name,
            already_stopped,
            asked,
            "stop asked of threads that had already stopped; nothing left to do"
        );
    }
    telemetry_tx.send_modify(|state| {
        for thread in &mut state.threads {
            thread.is_active = false;
            thread.hashrate = 0;
        }
    });
}

/// De-energise the board. The last rung of the ladder.
///
/// Two levers, in this order, and the order is the whole design:
///
/// 1. **Stop beating the MCU.** This removes nothing by itself; it re-arms the
///    board's own shed. Measured on this hardware at 74-80 s. It is the only
///    part of a scram that still fires if the I2C command below fails, if the
///    bus is wedged, or if this task dies between the two. Doing it first means
///    the backstop is live *before* the thing that might fail is attempted.
/// 2. **Command the MCU's ordered power-down, and verify the rail.** Opcode 14,
///    then a real ADC conversion rather than the MCU's own shadow of the
///    command it last obeyed. [`PowerDownOutcome::StillEnergised`] is the
///    outcome that exists because those two can disagree, and it is the one a
///    shadow read alone can never produce.
///
/// The sysfs rails are deliberately NOT touched here. They belong to the
/// bring-up plan, which `Bzm2Board::shutdown` may be driving concurrently, and
/// a second actor writing the same write-only enables is a worse failure than
/// the one this is fixing. The MCU command is the lever that has been proven on
/// this hardware and it is the one that verifies itself.
pub(super) async fn scram(
    board_name: &str,
    heartbeat_shutdown: &Option<watch::Sender<bool>>,
    board_indices: &[usize],
) {
    match heartbeat_shutdown {
        Some(tx) => {
            let _ = tx.send(true);
            info!(
                board = %board_name,
                "SCRAM: MCU heartbeat stopped. The board's own shed is now armed as a backstop."
            );
        }
        None => {
            // Not a failure -- it means nothing was ever feeding the MCU, so
            // the shed was never suppressed. Worth saying, because "no
            // heartbeat to stop" and "heartbeat stopped" look identical in a
            // log that only reports the happy path.
            info!(
                board = %board_name,
                "SCRAM: no MCU heartbeat was running, so nothing was suppressing the shed."
            );
        }
    }

    if board_indices.is_empty() {
        error!(
            board = %board_name,
            "SCRAM: no board index resolves to an I2C bus, so NOTHING here can command a \
             power-down. The MCU shed above is the only mechanism left."
        );
        return;
    }

    let platform = super::platform::DEFAULT;
    for &board_index in board_indices {
        let Some(bus_path) = platform.i2c_bus_path(board_index) else {
            error!(board = %board_name, board_index,
                   "SCRAM: no I2C bus path for this board; cannot command a power-down");
            continue;
        };
        let bus = match crate::hw_trait::i2c::linux::LinuxI2c::open(&bus_path) {
            Ok(bus) => bus,
            Err(err) => {
                error!(
                    board = %board_name, board_index, bus = %bus_path.display(), error = %err,
                    "SCRAM: cannot open this board's MCU bus; it will come down on the shed \
                     instead, which takes up to 80s"
                );
                continue;
            }
        };
        let mut power = super::board_power::Bzm2BoardPower::new(bus);
        match power.power_down().await {
            Ok(super::board_power::PowerDownOutcome::Down) => {
                warn!(board = %board_name, board_index,
                      "SCRAM: board de-energised and its rail reads dark.");
            }
            Ok(super::board_power::PowerDownOutcome::StillEnergised { status, rail_mv }) => {
                error!(
                    board = %board_name, board_index, rail_mv, ?status,
                    "SCRAM: the MCU says it complied AND THE RAIL IS STILL UP. Roughly a \
                     hundred series devices remain energised. The shed is the last mechanism."
                );
            }
            Ok(super::board_power::PowerDownOutcome::StillUp(status)) => {
                error!(
                    board = %board_name, board_index, ?status,
                    "SCRAM: power-down accepted and the board still reports power."
                );
            }
            Ok(super::board_power::PowerDownOutcome::Unverified) => {
                error!(
                    board = %board_name, board_index,
                    "SCRAM: power-down written but the verify could not be read. This is not \
                     proof of either state -- treat this board as energised."
                );
            }
            Err(err) => {
                error!(
                    board = %board_name, board_index, error = %err,
                    "SCRAM: power-down command FAILED. The shed is the last mechanism."
                );
            }
        }
    }
}

/// Each chain by the name its die rows carry: `sensor_prefix`, the same
/// function that names them, so a port whose file name has any character
/// other than a letter or digit is not a chain that is never fresh.
fn chain_names(bus_layouts: &[Bzm2BusLayout]) -> Vec<String> {
    bus_layouts
        .iter()
        .map(|bus| sensor_prefix(&bus.serial_path))
        .collect()
}

async fn collect_thread_runtime_metrics(
    handles: &[Bzm2ThreadHandle],
) -> (
    BTreeMap<usize, Bzm2ThreadRuntimeMetrics>,
    Vec<(usize, abort::ThreadAnswer)>,
) {
    collect_bounded(
        handles.len(),
        |i| handles[i].runtime_metrics(),
        THREAD_ANSWER_BOUND,
    )
    .await
}

/// How long one thread may take to answer the monitor.
///
/// The query was awaited unbounded, so one actor that stopped reading its
/// channel without closing it would stop the monitor's whole poll -- every
/// limit on every chain -- behind one silent thread. A second is generous for
/// a counter snapshot and short beside the poll interval.
const THREAD_ANSWER_BOUND: std::time::Duration = std::time::Duration::from_secs(1);

/// Ask each of `count` threads, each bounded, and say how each answered.
///
/// Generic over the query so the bound can be tested against a thread that
/// never answers without constructing one.
async fn collect_bounded<M, F, Fut>(
    count: usize,
    query: F,
    bound: std::time::Duration,
) -> (BTreeMap<usize, M>, Vec<(usize, abort::ThreadAnswer)>)
where
    F: Fn(usize) -> Fut,
    Fut: std::future::Future<Output = Result<M, HashThreadError>>,
{
    let mut metrics = BTreeMap::new();
    let mut answers = Vec::with_capacity(count);
    for thread_index in 0..count {
        let answer = match tokio::time::timeout(bound, query(thread_index)).await {
            Ok(Ok(snapshot)) => {
                metrics.insert(thread_index, snapshot);
                abort::ThreadAnswer::Answered
            }
            Ok(Err(HashThreadError::ChannelClosed(err))) => {
                warn!(thread_index, error = %err, "Failed to query BZM2 runtime metrics: the thread has exited");
                abort::ThreadAnswer::Closed
            }
            Ok(Err(err)) => {
                warn!(thread_index, error = %err, "Failed to query BZM2 runtime metrics");
                abort::ThreadAnswer::Unanswered
            }
            Err(_) => {
                warn!(
                    thread_index,
                    bound_ms = bound.as_millis() as u64,
                    "BZM2 thread did not answer the monitor inside the bound"
                );
                abort::ThreadAnswer::Unanswered
            }
        };
        answers.push((thread_index, answer));
    }
    (metrics, answers)
}

/// Fold this poll's power and work into the efficiency history.
///
/// Two domains are measurable on a BZM2 board, from different sensors:
///
/// - [`Board`][PowerDomain::Board] is the board's own input sensor: everything
///   it draws, ASICs and housekeeping alike.
/// - [`Asic`][PowerDomain::Asic] is the sum of the regulator *outputs* feeding
///   the voltage domains — what is delivered to the ASIC stacks. It excludes
///   the controller, fans and radio, but still includes the IR drop between
///   the regulator terminals and the die, so it is an upper bound on silicon
///   consumption rather than the silicon figure itself.
///
/// The difference between them is the board's vampire load.
///
/// The ASIC total is recorded only when *every* voltage domain reports. A
/// partial sum would under-count the numerator and report the board as more
/// efficient than it is, which is the wrong direction for a number a control
/// loop may later act on.
fn record_efficiency_samples(
    efficiency: &mut EfficiencyTracker,
    now: Instant,
    snapshot: &Bzm2TelemetrySnapshot,
    measurements: &Bzm2RuntimeMeasurementCache,
    board_throughput_hs: Option<u64>,
) {
    if let Some(watts) = snapshot_input_power(snapshot) {
        efficiency.record_power_at(now, PowerReading::measured(PowerDomain::Board, watts));
    }
    if let Some(watts) = total_asic_power_w(measurements) {
        efficiency.record_power_at(now, PowerReading::measured(PowerDomain::Asic, watts));
    }
    // Recorded after the power samples: the tracker credits work only to
    // domains that reported on this poll.
    if let Some(throughput_hs) = board_throughput_hs {
        efficiency.record_hashrate_at(now, HashRate::from(throughput_hs));
    }
}

fn build_efficiency_report(efficiency: &EfficiencyTracker, now: Instant) -> Vec<EfficiencyReport> {
    efficiency
        .report(now)
        .into_iter()
        .map(|domain| EfficiencyReport {
            domain: domain.domain.as_str().to_owned(),
            windows: domain
                .samples
                .into_iter()
                .map(|sample| EfficiencyWindow {
                    window_secs: sample.window.as_secs(),
                    joules_per_terahash: sample.efficiency.joules_per_terahash,
                    provenance: sample.efficiency.provenance.as_str().to_owned(),
                    hashrate: sample.hashrate.map(u64::from),
                })
                .collect(),
        })
        .collect()
}

/// Total power delivered to the ASIC stacks, or `None` if any domain is silent.
fn total_asic_power_w(measurements: &Bzm2RuntimeMeasurementCache) -> Option<f32> {
    let domains = &measurements.domain_measurements;
    if domains.is_empty() {
        return None;
    }
    domains
        .values()
        .map(|domain| domain.measured_power_w)
        .try_fold(0.0f32, |total, watts| Some(total + watts?))
}

// Aggregates the monitor's per-poll working set; a parameter struct would be
// built and torn down at the single call site for no clarity gain.
#[allow(clippy::too_many_arguments)]
fn build_runtime_tuning_state(
    asics: &[AsicState],
    temperatures: &[TemperatureSensor],
    bus_layouts: &[Bzm2BusLayout],
    calibration: &Bzm2CalibrationConfig,
    bringup: &Bzm2BringupConfig,
    rail_snapshot: &Bzm2TelemetrySnapshot,
    applied_operating_state: &Bzm2AppliedOperatingState,
    thread_metrics: &BTreeMap<usize, Bzm2ThreadRuntimeMetrics>,
) -> (Bzm2TuningState, Bzm2RuntimeMeasurementCache) {
    let total_asics = bus_layouts.iter().map(|bus| bus.asic_count).sum::<u16>();
    let (domains, _domain_lookup) = build_voltage_domains(
        total_asics,
        &calibration.asics_per_domain,
        &calibration.domain_voltage_offsets_mv,
    );

    let mut tuning_domains = Vec::new();
    let mut cache_domains = BTreeMap::new();
    for domain in domains {
        let rail_index = bringup.rail_index_for_domain(domain.domain_id);
        let rail_output_name = rail_index.map(|index| format!("rail{index}-output"));
        let measured_voltage_mv = rail_output_name
            .as_ref()
            .and_then(|name| {
                rail_snapshot
                    .powers
                    .iter()
                    .find(|power| power.name == *name)
            })
            .and_then(|power| power.voltage_v)
            .map(|voltage| (voltage * 1000.0).round() as u32);
        let measured_power_w = rail_output_name
            .as_ref()
            .and_then(|name| {
                rail_snapshot
                    .powers
                    .iter()
                    .find(|power| power.name == *name)
            })
            .and_then(|power| power.power_w);
        tuning_domains.push(Bzm2DomainTuningState {
            domain_id: domain.domain_id,
            rail_index,
            target_voltage_mv: applied_operating_state
                .per_domain_voltage_mv
                .get(&domain.domain_id)
                .copied(),
            measured_voltage_mv,
            measured_power_w,
        });
        cache_domains.insert(
            domain.domain_id,
            Bzm2DomainMeasurement {
                domain_id: domain.domain_id,
                measured_voltage_mv,
                measured_power_w,
            },
        );
    }
    tuning_domains.sort_by_key(|domain| domain.domain_id);

    let mut board_throughput_hs = 0u64;
    let mut board_has_throughput = false;
    let mut tuning_asics = Vec::new();
    let mut cache_asics = BTreeMap::new();

    for asic in asics {
        let Some(thread_index) = asic.thread_index else {
            continue;
        };
        let Some(bus) = bus_layouts.get(thread_index) else {
            continue;
        };
        let Some(global_asic_id) = bus.global_asic_id(asic.id) else {
            continue;
        };
        let runtime_asic = thread_metrics
            .get(&thread_index)
            .and_then(|metrics| metrics.asics.iter().find(|metrics| metrics.asic == asic.id));
        let missing_engines =
            if asic.missing_engines.is_empty() && asic.discovered_engine_count.is_none() {
                default_saved_engine_topology()
                    .missing_engines
                    .into_iter()
                    .map(|engine| EngineCoordinate {
                        row: engine.row,
                        col: engine.col,
                    })
                    .collect::<Vec<_>>()
            } else {
                asic.missing_engines.clone()
            };
        let (stack0_active, stack1_active) =
            split_active_engine_counts(asic.discovered_engine_count, &missing_engines);
        // `per_asic_pll_mhz` is what calibration last COMMANDED this PLL to
        // (see `Bzm2AppliedOperatingState` in calibration.rs); there is no
        // PLL frequency readback path on this hardware, so the value below
        // is never a measurement, however it is presented.
        let frequencies = applied_operating_state
            .per_asic_pll_mhz
            .get(&global_asic_id)
            .copied();
        let mut pll_states = Vec::with_capacity(2);
        let mut pll_pass_rates = [None, None];

        for pll_index in 0..2usize {
            let throughput_hs = runtime_asic.and_then(|asic| asic.plls[pll_index].throughput_hs);
            // Commanded, not measured -- see the comment above `frequencies`.
            // `Bzm2PllTuningState::frequency_mhz` (api_client/types.rs) is
            // published unlabeled next to genuinely measured fields like
            // `throughput_hs`; it should be renamed to
            // `commanded_frequency_mhz` there so a consumer cannot mistake
            // one for the other. That file is out of this module's
            // ownership, so the field keeps its current name here until
            // that rename lands.
            let frequency_mhz = frequencies.map(|freq| freq[pll_index]);
            let active_engines = if pll_index == 0 {
                stack0_active
            } else {
                stack1_active
            };
            let pass_rate =
                throughput_hs
                    .zip(frequency_mhz)
                    .and_then(|(throughput_hs, frequency_mhz)| {
                        expected_stack_throughput_hs(active_engines, frequency_mhz)
                            .map(|expected| throughput_hs as f32 / expected.max(1) as f32)
                    });
            pll_pass_rates[pll_index] = pass_rate;
            pll_states.push(Bzm2PllTuningState {
                pll_index: pll_index as u8,
                frequency_mhz, // commanded, not a readback -- see above
                throughput_hs,
                pass_rate,
            });
        }

        let average_pass_rate = weighted_average_pass_rate(&[
            (pll_pass_rates[0], stack0_active),
            (pll_pass_rates[1], stack1_active),
        ]);
        let throughput_hs = runtime_asic.and_then(|asic| asic.throughput_hs);
        if let Some(throughput_hs) = throughput_hs {
            board_throughput_hs = board_throughput_hs.saturating_add(throughput_hs);
            board_has_throughput = true;
        }

        tuning_asics.push(Bzm2AsicTuningState {
            id: asic.id,
            thread_index: asic.thread_index,
            active_engine_count: asic.discovered_engine_count,
            throughput_hs,
            average_pass_rate,
            scheduler_share_count: runtime_asic.map(|asic| asic.scheduler_share_count),
            results: runtime_asic.map(|asic| Bzm2ResultCounts::from(asic.results)),
            plls: pll_states,
        });

        cache_asics.insert(
            global_asic_id,
            Bzm2AsicMeasurement {
                asic_id: global_asic_id,
                temperature_c: asic_temperature_for_sensor(
                    temperatures,
                    bus.serial_path.as_str(),
                    asic.id,
                ),
                throughput_ths: throughput_hs
                    .map(|throughput| throughput as f32 / 1_000_000_000_000.0),
                average_pass_rate,
                pll_pass_rates,
            },
        );
    }
    tuning_asics.sort_by_key(|asic| (asic.thread_index.unwrap_or(usize::MAX), asic.id));

    (
        Bzm2TuningState {
            board_throughput_hs: board_has_throughput.then_some(board_throughput_hs),
            reuse_saved_operating_point: None,
            needs_retune: None,
            desired_voltage_mv: None,
            desired_clock_mhz: None,
            desired_accept_ratio: None,
            retune_pending: None,
            retune_reasons: Vec::new(),
            saved_operating_point_status: applied_operating_state.saved_operating_point_status,
            saved_operating_point_reasons: applied_operating_state
                .saved_operating_point_reasons
                .clone(),
            planner_notes: Vec::new(),
            domains: tuning_domains,
            asics: tuning_asics,
        },
        Bzm2RuntimeMeasurementCache {
            domain_measurements: cache_domains,
            asic_measurements: cache_asics,
        },
    )
}

fn apply_runtime_tuning_plan(
    mut tuning_state: Bzm2TuningState,
    plan: Option<crate::tuning::calibration_planner::Bzm2CalibrationPlan>,
) -> Bzm2TuningState {
    if let Some(plan) = plan {
        tuning_state.reuse_saved_operating_point = Some(plan.reuse_saved_operating_point);
        tuning_state.needs_retune = Some(plan.needs_retune);
        tuning_state.desired_voltage_mv = Some(plan.desired_voltage_mv);
        tuning_state.desired_clock_mhz = Some(plan.desired_clock_mhz);
        tuning_state.desired_accept_ratio = Some(plan.desired_accept_ratio);
        tuning_state.planner_notes = plan.notes;
    }
    tuning_state
}

fn apply_runtime_retune_triggers(
    mut tuning_state: Bzm2TuningState,
    calibration: &Bzm2CalibrationConfig,
    measurement_cache: &Bzm2RuntimeMeasurementCache,
    tracker: &mut Bzm2RetuneTriggerTracker,
) -> Bzm2TuningState {
    if !calibration.runtime_retune_enabled {
        tuning_state.retune_pending = Some(false);
        tuning_state.retune_reasons.clear();
        return tuning_state;
    }

    let persistence = calibration.runtime_retune_persistence_polls.max(1);
    let throughput_regression = tuning_state.needs_retune.unwrap_or(false);
    let thermal_drift = measurement_cache
        .asic_measurements
        .values()
        .filter_map(|asic| asic.temperature_c)
        .any(|temp| temp >= calibration.runtime_retune_thermal_c);
    let voltage_imbalance = tuning_state.domains.iter().any(|domain| {
        domain
            .target_voltage_mv
            .zip(domain.measured_voltage_mv)
            .is_some_and(|(target, measured)| {
                target.abs_diff(measured) >= calibration.runtime_retune_voltage_imbalance_mv
            })
    });

    let mut retune_reasons = Vec::new();
    if update_trigger_counter(
        &mut tracker.throughput_regression_polls,
        throughput_regression,
    ) >= persistence
    {
        retune_reasons.push("throughput regression".into());
    }
    if update_trigger_counter(&mut tracker.thermal_drift_polls, thermal_drift) >= persistence {
        retune_reasons.push("thermal drift".into());
    }
    if update_trigger_counter(&mut tracker.voltage_imbalance_polls, voltage_imbalance)
        >= persistence
    {
        retune_reasons.push("persistent voltage imbalance".into());
    }

    tuning_state.retune_pending = Some(!retune_reasons.is_empty());
    tuning_state.retune_reasons = retune_reasons;
    tuning_state
}

fn reconcile_saved_operating_point_status(
    mut tuning_state: Bzm2TuningState,
    calibration: &Bzm2CalibrationConfig,
    bus_layouts: &[Bzm2BusLayout],
    applied_operating_state: &Arc<Mutex<Bzm2AppliedOperatingState>>,
) -> Bzm2TuningState {
    let mut guard = applied_operating_state
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let desired = if tuning_state.retune_pending == Some(true) {
        // Retune triggers only set retune_pending once they persist past the
        // trigger tracker's threshold, so the operating point is known bad:
        // invalidate it so startup replay refuses it and falls back to live
        // calibration.
        guard.saved_operating_point.as_ref().map(|_| {
            (
                Bzm2SavedOperatingPointStatus::Invalidated,
                tuning_state.retune_reasons.clone(),
            )
        })
    } else if guard.saved_operating_point.is_some() {
        Some((Bzm2SavedOperatingPointStatus::Validated, Vec::new()))
    } else {
        guard
            .saved_operating_point_status
            .map(|status| (status, guard.saved_operating_point_reasons.clone()))
    };

    if let Some((status, reasons)) = desired.clone() {
        let status_changed = guard.saved_operating_point_status != Some(status)
            || guard.saved_operating_point_reasons != reasons;
        if status_changed {
            if let (Some(profile_path), Some(saved_state)) = (
                calibration.profile_path.as_deref(),
                guard.saved_operating_point.as_ref(),
            ) && let Err(err) = store_saved_operating_point_status(
                profile_path,
                calibration,
                bus_layouts,
                saved_state,
                status,
                &reasons,
            ) {
                warn!(
                    path = %profile_path.display(),
                    error = %err,
                    "Failed to persist BZM2 saved operating point status"
                );
            }
            guard.saved_operating_point_status = Some(status);
            guard.saved_operating_point_reasons = reasons.clone();
        }

        tuning_state.saved_operating_point_status = Some(status);
        tuning_state.saved_operating_point_reasons = reasons;
        if tuning_state.retune_pending == Some(true) {
            tuning_state.reuse_saved_operating_point = Some(false);
        }
    } else {
        tuning_state.saved_operating_point_status = None;
        tuning_state.saved_operating_point_reasons.clear();
    }

    tuning_state
}

fn update_trigger_counter(counter: &mut u8, active: bool) -> u8 {
    if active {
        *counter = counter.saturating_add(1);
    } else {
        *counter = 0;
    }
    *counter
}

fn evaluate_runtime_tuning_plan(
    asics: &[AsicState],
    temperatures: &[TemperatureSensor],
    bus_layouts: &[Bzm2BusLayout],
    calibration: &Bzm2CalibrationConfig,
    applied_operating_state: &Bzm2AppliedOperatingState,
    measurement_cache: &Bzm2RuntimeMeasurementCache,
) -> Option<crate::tuning::calibration_planner::Bzm2CalibrationPlan> {
    if bus_layouts.is_empty() {
        return None;
    }

    let total_asics = bus_layouts.iter().map(|bus| bus.asic_count).sum::<u16>();
    if total_asics == 0 {
        return None;
    }

    let (_voltage_domains, domain_lookup) = build_voltage_domains(
        total_asics,
        &calibration.asics_per_domain,
        &calibration.domain_voltage_offsets_mv,
    );
    let engine_topology = saved_engine_topology_from_state(asics, bus_layouts);
    let voltage_domains = build_voltage_domains(
        total_asics,
        &calibration.asics_per_domain,
        &calibration.domain_voltage_offsets_mv,
    )
    .0;
    let asic_topology = build_topology(bus_layouts, &domain_lookup, &engine_topology);
    let domain_measurements = voltage_domains
        .iter()
        .map(|domain| {
            measurement_cache
                .domain_measurements
                .get(&domain.domain_id)
                .cloned()
                .unwrap_or(Bzm2DomainMeasurement {
                    domain_id: domain.domain_id,
                    measured_voltage_mv: None,
                    measured_power_w: None,
                })
        })
        .collect::<Vec<_>>();
    let asic_measurements = asic_topology
        .iter()
        .map(|asic| {
            measurement_cache
                .asic_measurements
                .get(&asic.asic_id)
                .cloned()
                .unwrap_or(Bzm2AsicMeasurement {
                    asic_id: asic.asic_id,
                    temperature_c: temperatures
                        .iter()
                        .find(|sensor| {
                            bus_layouts
                                .iter()
                                .find(|layout| layout.contains(asic.asic_id))
                                .and_then(|layout| layout.local_asic_id(asic.asic_id))
                                .map(|local_asic| {
                                    sensor.name
                                        == format!(
                                            "{}-asic-{local_asic}-dts",
                                            sensor_prefix(
                                                bus_layouts
                                                    .iter()
                                                    .find(|layout| layout.contains(asic.asic_id))
                                                    .map(|layout| layout.serial_path.as_str())
                                                    .unwrap_or(""),
                                            )
                                        )
                                })
                                .unwrap_or(false)
                        })
                        .and_then(|sensor| sensor.temperature.map(Temperature::as_degrees_c)),
                    throughput_ths: None,
                    average_pass_rate: None,
                    pll_pass_rates: [None, None],
                })
        })
        .collect::<Vec<_>>();
    let site_temp_c = temperatures
        .iter()
        .find(|sensor| sensor.name == "board")
        .and_then(|sensor| sensor.temperature.map(Temperature::as_degrees_c))
        .or_else(|| {
            temperatures
                .iter()
                .find(|sensor| sensor.name == "asic")
                .and_then(|sensor| sensor.temperature.map(Temperature::as_degrees_c))
        })
        .or(calibration.site_temp_c)
        .unwrap_or(DEFAULT_CALIBRATION_SITE_TEMP_C);

    Some(Bzm2CalibrationPlanner.plan(&Bzm2BoardCalibrationInput {
        operating_class: calibration.operating_class,
        site_temp_c,
        target_mode: calibration.performance_mode,
        mode: calibration.mode,
        per_stack_clocking: calibration.per_stack_clocking,
        voltage_domains,
        asics: asic_topology,
        saved_operating_point: applied_operating_state.saved_operating_point.clone(),
        domain_measurements,
        asic_measurements,
        constraints: Bzm2CalibrationConstraints::default(),
        force_retune: calibration.force_retune,
    }))
}

fn saved_engine_topology_from_state(
    asics: &[AsicState],
    bus_layouts: &[Bzm2BusLayout],
) -> BTreeMap<u16, Bzm2SavedEngineTopology> {
    let mut topology = BTreeMap::new();
    for asic in asics {
        let Some(thread_index) = asic.thread_index else {
            continue;
        };
        let Some(bus) = bus_layouts.get(thread_index) else {
            continue;
        };
        let Some(global_asic_id) = bus.global_asic_id(asic.id) else {
            continue;
        };
        topology.insert(
            global_asic_id,
            Bzm2SavedEngineTopology {
                active_engine_count: asic
                    .discovered_engine_count
                    .unwrap_or_else(|| default_saved_engine_topology().active_engine_count),
                missing_engines: if asic.missing_engines.is_empty()
                    && asic.discovered_engine_count.is_none()
                {
                    default_saved_engine_topology().missing_engines
                } else {
                    asic.missing_engines
                        .iter()
                        .map(|engine| Bzm2SavedEngineCoordinate {
                            row: engine.row,
                            col: engine.col,
                        })
                        .collect()
                },
            },
        );
    }
    topology
}

fn split_active_engine_counts(
    active_engine_count: Option<u16>,
    missing_engines: &[EngineCoordinate],
) -> (u16, u16) {
    if missing_engines.is_empty()
        && let Some(active_engine_count) = active_engine_count
    {
        let lower = active_engine_count / 2;
        return (lower, active_engine_count.saturating_sub(lower));
    }
    let mut bottom_missing = 0u16;
    let mut top_missing = 0u16;
    for engine in missing_engines {
        if engine.row < 10 {
            bottom_missing = bottom_missing.saturating_add(1);
        } else {
            top_missing = top_missing.saturating_add(1);
        }
    }
    let engines_per_stack = 10u16 * 12u16;
    (
        engines_per_stack.saturating_sub(bottom_missing),
        engines_per_stack.saturating_sub(top_missing),
    )
}

fn expected_stack_throughput_hs(active_engines: u16, frequency_mhz: f32) -> Option<u64> {
    (frequency_mhz > 0.0).then(|| {
        let ghs = active_engines as f32 * 4.0 * (frequency_mhz / 1000.0) / 3.0;
        (ghs * 1_000_000_000.0).round() as u64
    })
}

fn weighted_average_pass_rate(samples: &[(Option<f32>, u16)]) -> Option<f32> {
    let mut weighted = 0.0f32;
    let mut total_weight = 0u32;
    for (pass_rate, weight) in samples {
        if let Some(pass_rate) = pass_rate {
            weighted += pass_rate * *weight as f32;
            total_weight += u32::from(*weight);
        }
    }
    (total_weight > 0).then_some(weighted / total_weight as f32)
}

fn asic_temperature_for_sensor(
    temperatures: &[TemperatureSensor],
    serial_path: &str,
    asic: u8,
) -> Option<f32> {
    let name = format!("{}-asic-{asic}-dts", sensor_prefix(serial_path));
    temperatures
        .iter()
        .find(|sensor| sensor.name == name)
        .and_then(|sensor| sensor.temperature.map(Temperature::as_degrees_c))
}

#[cfg(test)]
mod tests {
    use super::super::calibration::load_saved_operating_point_profile;
    #[cfg(unix)]
    use super::super::config::{
        Bzm2EnumerationConfig, Bzm2RuntimeConfig, DEFAULT_BAUD_RATE, DEFAULT_NOMINAL_HASHRATE_THS,
    };
    #[cfg(unix)]
    use super::super::telemetry::{Bzm2TelemetryConfig, SensorSpec};
    use super::*;
    #[cfg(unix)]
    use crate::api_client::types::BoardTelemetry;
    use crate::api_client::types::{Bzm2StartupPath, PowerMeasurement};
    use crate::tuning::calibration_planner::Bzm2SavedOperatingPoint;
    #[cfg(unix)]
    use nix::pty::openpty;
    use std::fs;
    #[cfg(unix)]
    use std::os::fd::AsRawFd;
    #[cfg(unix)]
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};
    #[cfg(unix)]
    use tokio::sync::{mpsc, watch};

    #[test]
    fn a_chain_is_named_as_its_die_rows_are() {
        let layouts = vec![
            Bzm2BusLayout {
                serial_path: "/dev/tty9bit10".into(),
                asic_start: 0,
                asic_count: 100,
            },
            Bzm2BusLayout {
                serial_path: "/dev/ttyS_1".into(),
                asic_start: 100,
                asic_count: 4,
            },
        ];
        assert_eq!(
            chain_names(&layouts),
            vec!["tty9bit10".to_string(), "ttyS-1".to_string()]
        );
        assert_eq!(
            sensor_prefix("/dev/ttyS_1"),
            "ttyS-1",
            "the rows' own prefix"
        );
    }

    /// A thread that stops reading its channel without closing it held the
    /// monitor's whole poll -- every limit, every chain -- for as long as it
    /// stayed silent. Bounded now, and the silence is named per thread.
    #[tokio::test(start_paused = true)]
    async fn a_thread_that_never_answers_cannot_hold_the_monitor() {
        // What an unbounded await of this query does: it does not return.
        let unbounded = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            std::future::pending::<Result<u8, HashThreadError>>(),
        )
        .await;
        assert!(
            unbounded.is_err(),
            "an unbounded await of a silent thread never returns"
        );

        let started = tokio::time::Instant::now();
        let (metrics, answers) = collect_bounded(
            3,
            |i| async move {
                match i {
                    0 => Ok(7u8),
                    1 => std::future::pending().await,
                    _ => Err(HashThreadError::ChannelClosed(
                        "command channel closed".into(),
                    )),
                }
            },
            THREAD_ANSWER_BOUND,
        )
        .await;
        assert_eq!(tokio::time::Instant::now() - started, THREAD_ANSWER_BOUND);
        assert_eq!(metrics.get(&0), Some(&7));
        assert_eq!(
            answers,
            vec![
                (0, abort::ThreadAnswer::Answered),
                (1, abort::ThreadAnswer::Unanswered),
                (2, abort::ThreadAnswer::Closed),
            ]
        );
    }

    fn domain_measurement(domain_id: u16, watts: Option<f32>) -> Bzm2DomainMeasurement {
        Bzm2DomainMeasurement {
            domain_id,
            measured_voltage_mv: Some(800),
            measured_power_w: watts,
        }
    }

    fn measurement_cache(domains: &[Bzm2DomainMeasurement]) -> Bzm2RuntimeMeasurementCache {
        Bzm2RuntimeMeasurementCache {
            domain_measurements: domains
                .iter()
                .map(|domain| (domain.domain_id, domain.clone()))
                .collect(),
            asic_measurements: BTreeMap::new(),
        }
    }

    fn input_power_snapshot(watts: f32) -> Bzm2TelemetrySnapshot {
        Bzm2TelemetrySnapshot {
            powers: vec![PowerMeasurement {
                name: "input".into(),
                voltage_v: Some(12.0),
                current_a: Some(watts / 12.0),
                power_w: Some(watts),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn asic_power_needs_every_domain_to_report() {
        let both = measurement_cache(&[
            domain_measurement(0, Some(30.0)),
            domain_measurement(1, Some(28.0)),
        ]);
        assert_eq!(total_asic_power_w(&both), Some(58.0));

        // One silent regulator must suppress the total, not shrink it. A
        // partial sum would report the board as more efficient than it is.
        let partial = measurement_cache(&[
            domain_measurement(0, Some(30.0)),
            domain_measurement(1, None),
        ]);
        assert_eq!(total_asic_power_w(&partial), None);

        assert_eq!(total_asic_power_w(&measurement_cache(&[])), None);
    }

    #[test]
    fn monitor_samples_produce_per_domain_efficiency() {
        // Synthetic round figures, not a measurement: 100 W at the board
        // input, 80 W delivered to the stacks, 10 TH/s.
        // Board: 100 / 10 = 10 J/TH. ASIC: 80 / 10 = 8 J/TH.
        let t0 = std::time::Instant::now();
        let mut efficiency = EfficiencyTracker::new();
        let snapshot = input_power_snapshot(100.0);
        let measurements = measurement_cache(&[
            domain_measurement(0, Some(40.0)),
            domain_measurement(1, Some(40.0)),
        ]);
        let throughput_hs = u64::from(HashRate::from_terahashes(10.0));

        for step in 0..=40 {
            let now = t0 + std::time::Duration::from_secs(step * 10);
            record_efficiency_samples(
                &mut efficiency,
                now,
                &snapshot,
                &measurements,
                Some(throughput_hs),
            );
        }

        let report = build_efficiency_report(&efficiency, t0 + std::time::Duration::from_secs(400));
        assert_eq!(report.len(), 2);

        let board = report
            .iter()
            .find(|report| report.domain == "board")
            .expect("board domain reported");
        let five_minutes = board
            .windows
            .iter()
            .find(|window| window.window_secs == 300)
            .expect("five-minute window earned after 400 s");
        assert!(
            (five_minutes.joules_per_terahash - 10.0).abs() < 0.2,
            "expected ~10 J/TH at the board, got {}",
            five_minutes.joules_per_terahash
        );
        assert_eq!(five_minutes.provenance, "measured");
        assert_eq!(five_minutes.hashrate, Some(throughput_hs));

        let asic = report
            .iter()
            .find(|report| report.domain == "asic")
            .expect("asic domain reported");
        let five_minutes = asic
            .windows
            .iter()
            .find(|window| window.window_secs == 300)
            .expect("five-minute window earned after 400 s");
        assert!(
            (five_minutes.joules_per_terahash - 8.0).abs() < 0.2,
            "expected ~8 J/TH at the silicon, got {}",
            five_minutes.joules_per_terahash
        );

        // The gap between the two is the vampire load, and it is the reason
        // both are reported rather than one.
        assert!(
            board.windows[0].joules_per_terahash > asic.windows[0].joules_per_terahash,
            "board efficiency must be worse than silicon efficiency"
        );
    }

    #[test]
    fn no_throughput_yields_no_efficiency() {
        let t0 = std::time::Instant::now();
        let mut efficiency = EfficiencyTracker::new();
        let snapshot = input_power_snapshot(72.0);
        let measurements = measurement_cache(&[domain_measurement(0, Some(60.0))]);

        for step in 0..=40 {
            let now = t0 + std::time::Duration::from_secs(step * 10);
            record_efficiency_samples(&mut efficiency, now, &snapshot, &measurements, None);
        }

        // Burning power while finding nothing is unmeasured, not infinitely
        // inefficient.
        assert!(
            build_efficiency_report(&efficiency, t0 + std::time::Duration::from_secs(400))
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn board_safety_trip_closes_scheduler_event_stream() {
        // THIS IS A BENCH, AND THE VARIANT HAS TO SAY SO.
        //
        // The POST judges against rds-dvt2's default variant, air-3b, which
        // declares four fans that must turn. A workstation has none, and the
        // whole point of post.rs is that absent-by-design and absent-by-fault
        // are indistinguishable from a probe -- so a test host declares
        // itself rather than being guessed at.
        unsafe { std::env::set_var("MUJINA_BZM2_VARIANT", "bench") };
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

        let config = Bzm2RuntimeConfig {
            // Opt-in, so a test that does not ask for stored tuning does not get it.
            stored_calibration: None,
            heartbeat: Default::default(),
            serial_paths: vec![serial_path],
            baud_rate: DEFAULT_BAUD_RATE,
            timestamp_count: crate::asic::bzm2::protocol::DEFAULT_TIMESTAMP_COUNT,
            nonce_gap: crate::asic::bzm2::protocol::DEFAULT_NONCE_GAP,
            result_min_difficulty: None,
            dispatch_interval: Duration::from_millis(50),
            nominal_hashrate_ths: DEFAULT_NOMINAL_HASHRATE_THS,
            dts_vs_generation: crate::asic::bzm2::protocol::DtsVsGeneration::Gen2,
            telemetry: Bzm2TelemetryConfig {
                poll_interval: Duration::from_millis(20),
                // INPUT POWER: the last limit this file still evaluates from a
                // file. The die ceiling reads the DTS stream and the board
                // ceiling reads the MCU, so a sysfs path standing in for
                // either would test nothing.
                input_power: Some(SensorSpec {
                    path: sensor_path.to_string_lossy().into_owned(),
                    scale: 1.0,
                }),
                max_input_power_w: Some(80.0),
                ..Default::default()
            },
            enumeration: Bzm2EnumerationConfig::default(),
            bringup: Bzm2BringupConfig::default(),
            calibration: Bzm2CalibrationConfig::default(),
        };
        let (telemetry_tx, mut telemetry_rx) = watch::channel(BoardTelemetry {
            name: "bzm2-test".into(),
            model: "BZM2".into(),
            serial: Some("bzm2-test".into()),
            ..Default::default()
        });
        let mut board = Bzm2Board::new(config, telemetry_tx, mpsc::channel(1).1);

        let mut threads = board.create_hash_threads().await.unwrap();
        let mut event_rx = threads[0].take_event_receiver().unwrap();

        // The trip can only act once attach reaches the command loop. On this
        // emulator, which answers nothing, attach runs each phase to its
        // timeout: about 1.17 s since the engine ungate read was added, which
        // is past the 1 s this used to allow (and was 80 ms under it before).
        let closed = tokio::time::timeout(Duration::from_secs(2), async {
            while event_rx.recv().await.is_some() {}
        })
        .await;
        assert!(
            closed.is_ok(),
            "event stream should close after safety trip"
        );

        let state = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let snapshot = telemetry_rx.borrow().clone();
                if snapshot
                    .powers
                    .iter()
                    .any(|sensor| sensor.name == "input" && sensor.power_w == Some(90.0))
                {
                    break snapshot;
                }
                telemetry_rx.changed().await.unwrap();
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
    fn build_runtime_tuning_state_maps_live_measurements() {
        let asics = vec![AsicState {
            id: 0,
            thread_index: Some(0),
            serial_path: Some("/dev/ttyUSB0".into()),
            discovered_engine_count: Some(236),
            missing_engines: Vec::new(),
            ..Default::default()
        }];
        let temperatures = vec![TemperatureSensor {
            name: "ttyUSB0-asic-0-dts".into(),
            temperature: Some(Temperature::from_celsius(67.0)),
            observed_at: Some(std::time::Instant::now()),
        }];
        let bus_layouts = vec![Bzm2BusLayout {
            serial_path: "/dev/ttyUSB0".into(),
            asic_start: 0,
            asic_count: 1,
        }];
        let calibration = Bzm2CalibrationConfig::default();
        let bringup = Bzm2BringupConfig {
            rail_set_paths: vec!["/tmp/rail0".into()],
            ..Default::default()
        };
        let rail_snapshot = Bzm2TelemetrySnapshot {
            powers: vec![PowerMeasurement {
                name: "rail0-output".into(),
                voltage_v: Some(0.9),
                current_a: Some(44.0),
                power_w: Some(40.0),
            }],
            ..Default::default()
        };
        let applied = Bzm2AppliedOperatingState {
            per_domain_voltage_mv: BTreeMap::from([(0, 18_500)]),
            per_asic_pll_mhz: BTreeMap::from([(0, [1_200.0, 1_200.0])]),
            saved_operating_point: None,
            saved_operating_point_status: None,
            saved_operating_point_reasons: Vec::new(),
            startup_path: None,
        };
        let thread_metrics = BTreeMap::from([(
            0usize,
            Bzm2ThreadRuntimeMetrics {
                throughput_hs: Some(358_720_000_000),
                results: Default::default(),
                asics: vec![crate::asic::bzm2::Bzm2AsicRuntimeMetrics {
                    asic: 0,
                    throughput_hs: Some(358_720_000_000),
                    scheduler_share_count: 12,
                    results: Default::default(),
                    plls: [
                        crate::asic::bzm2::Bzm2PllRuntimeMetrics {
                            throughput_hs: Some(179_360_000_000),
                            scheduler_share_count: 6,
                        },
                        crate::asic::bzm2::Bzm2PllRuntimeMetrics {
                            throughput_hs: Some(179_360_000_000),
                            scheduler_share_count: 6,
                        },
                    ],
                }],
            },
        )]);

        let (tuning, cache) = build_runtime_tuning_state(
            &asics,
            &temperatures,
            &bus_layouts,
            &calibration,
            &bringup,
            &rail_snapshot,
            &applied,
            &thread_metrics,
        );

        assert_eq!(tuning.board_throughput_hs, Some(358_720_000_000));
        assert_eq!(tuning.domains.len(), 1);
        assert_eq!(tuning.domains[0].target_voltage_mv, Some(18_500));
        assert_eq!(tuning.domains[0].measured_voltage_mv, Some(900));
        assert_eq!(tuning.domains[0].measured_power_w, Some(40.0));
        assert_eq!(tuning.asics.len(), 1);
        assert_eq!(tuning.asics[0].throughput_hs, Some(358_720_000_000));
        assert_eq!(tuning.asics[0].scheduler_share_count, Some(12));
        assert!(
            tuning.asics[0]
                .average_pass_rate
                .is_some_and(|pass_rate| (pass_rate - 0.95).abs() < 0.0001)
        );
        assert!(
            tuning.asics[0].plls[0]
                .pass_rate
                .is_some_and(|pass_rate| (pass_rate - 0.95).abs() < 0.0001)
        );
        assert!(
            cache.asic_measurements[&0]
                .temperature_c
                .is_some_and(|temp| (temp - 67.0).abs() < 0.0001)
        );
        assert!(
            cache.asic_measurements[&0]
                .throughput_ths
                .is_some_and(|throughput| (throughput - 0.35872).abs() < 0.0001)
        );
        assert_eq!(cache.domain_measurements[&0].measured_voltage_mv, Some(900));
        assert_eq!(cache.domain_measurements[&0].measured_power_w, Some(40.0));
    }

    /// `Bzm2PllTuningState::frequency_mhz` must be a pure passthrough of
    /// what calibration commanded (`per_asic_pll_mhz`), never something
    /// derived from -- or reconciled against -- a runtime measurement.
    /// Deliberately pairs an applied frequency with a throughput that is
    /// consistent with neither PLL's commanded value: if `frequency_mhz`
    /// were ever computed from throughput instead of passed through
    /// verbatim, this mismatch would surface as a value other than the two
    /// asserted below.
    #[test]
    fn asic_pll_frequency_mhz_reflects_commanded_value_not_a_measurement() {
        let asics = vec![AsicState {
            id: 0,
            thread_index: Some(0),
            serial_path: Some("/dev/ttyUSB0".into()),
            discovered_engine_count: Some(236),
            missing_engines: Vec::new(),
            ..Default::default()
        }];
        let bus_layouts = vec![Bzm2BusLayout {
            serial_path: "/dev/ttyUSB0".into(),
            asic_start: 0,
            asic_count: 1,
        }];
        let calibration = Bzm2CalibrationConfig::default();
        let applied = Bzm2AppliedOperatingState {
            per_domain_voltage_mv: BTreeMap::new(),
            per_asic_pll_mhz: BTreeMap::from([(0, [777.0, 888.0])]),
            saved_operating_point: None,
            saved_operating_point_status: None,
            saved_operating_point_reasons: Vec::new(),
            startup_path: None,
        };
        let thread_metrics = BTreeMap::from([(
            0usize,
            Bzm2ThreadRuntimeMetrics {
                throughput_hs: Some(1),
                results: Default::default(),
                asics: vec![crate::asic::bzm2::Bzm2AsicRuntimeMetrics {
                    asic: 0,
                    throughput_hs: Some(1),
                    scheduler_share_count: 1,
                    results: Default::default(),
                    plls: [
                        crate::asic::bzm2::Bzm2PllRuntimeMetrics {
                            throughput_hs: Some(1),
                            scheduler_share_count: 1,
                        },
                        crate::asic::bzm2::Bzm2PllRuntimeMetrics {
                            throughput_hs: Some(1),
                            scheduler_share_count: 1,
                        },
                    ],
                }],
            },
        )]);

        let (tuning, _cache) = build_runtime_tuning_state(
            &asics,
            &[],
            &bus_layouts,
            &calibration,
            &Bzm2BringupConfig::default(),
            &Bzm2TelemetrySnapshot::default(),
            &applied,
            &thread_metrics,
        );

        assert_eq!(tuning.asics[0].plls[0].frequency_mhz, Some(777.0));
        assert_eq!(tuning.asics[0].plls[1].frequency_mhz, Some(888.0));
    }

    #[test]
    fn evaluate_runtime_tuning_plan_flags_underperforming_saved_point() {
        let asics = vec![AsicState {
            id: 0,
            thread_index: Some(0),
            serial_path: Some("/dev/ttyUSB0".into()),
            discovered_engine_count: Some(236),
            missing_engines: Vec::new(),
            ..Default::default()
        }];
        let temperatures = vec![TemperatureSensor {
            name: "ttyUSB0-asic-0-dts".into(),
            temperature: Some(Temperature::from_celsius(72.0)),
            observed_at: Some(std::time::Instant::now()),
        }];
        let bus_layouts = vec![Bzm2BusLayout {
            serial_path: "/dev/ttyUSB0".into(),
            asic_start: 0,
            asic_count: 1,
        }];
        let calibration = Bzm2CalibrationConfig::default();
        let applied = Bzm2AppliedOperatingState {
            per_domain_voltage_mv: BTreeMap::from([(0, 18_500)]),
            per_asic_pll_mhz: BTreeMap::from([(0, [1_200.0, 1_200.0])]),
            saved_operating_point: Some(Bzm2SavedOperatingPoint {
                board_voltage_mv: 18_500,
                board_throughput_ths: 0.40,
                per_domain_voltage_mv: BTreeMap::from([(0, 18_500)]),
                per_asic_engine_topology: BTreeMap::new(),
                per_asic_pll_mhz: BTreeMap::from([(0, [1_200.0, 1_200.0])]),
            }),
            startup_path: Some(Bzm2StartupPath::SavedReplay),
            saved_operating_point_status: Some(Bzm2SavedOperatingPointStatus::Validated),
            saved_operating_point_reasons: Vec::new(),
        };
        let measurement_cache = Bzm2RuntimeMeasurementCache {
            domain_measurements: BTreeMap::from([(
                0,
                Bzm2DomainMeasurement {
                    domain_id: 0,
                    measured_voltage_mv: Some(18_300),
                    measured_power_w: Some(55.0),
                },
            )]),
            asic_measurements: BTreeMap::from([(
                0,
                Bzm2AsicMeasurement {
                    asic_id: 0,
                    temperature_c: Some(72.0),
                    throughput_ths: Some(0.20),
                    average_pass_rate: Some(0.94),
                    pll_pass_rates: [Some(0.94), Some(0.94)],
                },
            )]),
        };

        let plan = evaluate_runtime_tuning_plan(
            &asics,
            &temperatures,
            &bus_layouts,
            &calibration,
            &applied,
            &measurement_cache,
        )
        .unwrap();

        assert!(plan.needs_retune);
        assert!(!plan.reuse_saved_operating_point);
    }

    #[test]
    fn runtime_retune_triggers_require_persistence() {
        let mut calibration = Bzm2CalibrationConfig::default();
        calibration.runtime_retune_persistence_polls = 2;
        calibration.runtime_retune_thermal_c = 80.0;
        let measurement_cache = Bzm2RuntimeMeasurementCache {
            domain_measurements: BTreeMap::new(),
            asic_measurements: BTreeMap::from([(
                0,
                Bzm2AsicMeasurement {
                    asic_id: 0,
                    temperature_c: Some(82.0),
                    throughput_ths: Some(0.30),
                    average_pass_rate: Some(0.97),
                    pll_pass_rates: [Some(0.97), Some(0.97)],
                },
            )]),
        };
        let mut tracker = Bzm2RetuneTriggerTracker::default();
        let tuning = Bzm2TuningState {
            needs_retune: Some(true),
            domains: vec![Bzm2DomainTuningState {
                domain_id: 0,
                rail_index: Some(0),
                target_voltage_mv: Some(18_500),
                measured_voltage_mv: Some(18_650),
                measured_power_w: Some(40.0),
            }],
            ..Default::default()
        };

        let first = apply_runtime_retune_triggers(
            tuning.clone(),
            &calibration,
            &measurement_cache,
            &mut tracker,
        );
        assert_eq!(first.retune_pending, Some(false));
        assert!(first.retune_reasons.is_empty());

        let second =
            apply_runtime_retune_triggers(tuning, &calibration, &measurement_cache, &mut tracker);
        assert_eq!(second.retune_pending, Some(true));
        assert!(
            second
                .retune_reasons
                .iter()
                .any(|reason| reason == "throughput regression")
        );
        assert!(
            second
                .retune_reasons
                .iter()
                .any(|reason| reason == "thermal drift")
        );
        assert!(
            second
                .retune_reasons
                .iter()
                .any(|reason| reason == "persistent voltage imbalance")
        );
    }

    #[test]
    fn reconcile_saved_operating_point_status_validates_profile() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let profile_path = std::env::temp_dir().join(format!(
            "bzm2-validate-profile-{}-{}.json",
            std::process::id(),
            unique
        ));
        let mut calibration = Bzm2CalibrationConfig::default();
        calibration.profile_path = Some(profile_path.clone());
        let bus_layouts = vec![Bzm2BusLayout {
            serial_path: "/dev/ttyUSB0".into(),
            asic_start: 0,
            asic_count: 1,
        }];
        let saved_state = Bzm2SavedOperatingPoint {
            board_voltage_mv: 17_500,
            board_throughput_ths: 40.0,
            per_domain_voltage_mv: BTreeMap::from([(0, 17_500)]),
            per_asic_engine_topology: BTreeMap::new(),
            per_asic_pll_mhz: BTreeMap::from([(0, [1_100.0, 1_100.0])]),
        };
        let applied_state = Arc::new(Mutex::new(Bzm2AppliedOperatingState {
            per_domain_voltage_mv: saved_state.per_domain_voltage_mv.clone(),
            per_asic_pll_mhz: saved_state.per_asic_pll_mhz.clone(),
            saved_operating_point: Some(saved_state),
            startup_path: Some(Bzm2StartupPath::LiveCalibration),
            saved_operating_point_status: Some(Bzm2SavedOperatingPointStatus::Pending),
            saved_operating_point_reasons: vec!["awaiting runtime validation".into()],
        }));

        let tuning = reconcile_saved_operating_point_status(
            Bzm2TuningState::default(),
            &calibration,
            &bus_layouts,
            &applied_state,
        );
        assert_eq!(
            tuning.saved_operating_point_status,
            Some(Bzm2SavedOperatingPointStatus::Validated)
        );
        assert!(tuning.saved_operating_point_reasons.is_empty());

        let stored = load_saved_operating_point_profile(Some(&profile_path))
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.persisted.unwrap().saved_operating_point_status,
            Bzm2SavedOperatingPointStatus::Validated
        );

        let _ = fs::remove_file(profile_path);
    }

    #[test]
    fn reconcile_saved_operating_point_status_invalidates_profile() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let profile_path = std::env::temp_dir().join(format!(
            "bzm2-invalidate-profile-{}-{}.json",
            std::process::id(),
            unique
        ));
        let mut calibration = Bzm2CalibrationConfig::default();
        calibration.profile_path = Some(profile_path.clone());
        let bus_layouts = vec![Bzm2BusLayout {
            serial_path: "/dev/ttyUSB0".into(),
            asic_start: 0,
            asic_count: 1,
        }];
        let saved_state = Bzm2SavedOperatingPoint {
            board_voltage_mv: 17_500,
            board_throughput_ths: 40.0,
            per_domain_voltage_mv: BTreeMap::from([(0, 17_500)]),
            per_asic_engine_topology: BTreeMap::new(),
            per_asic_pll_mhz: BTreeMap::from([(0, [1_100.0, 1_100.0])]),
        };
        let applied_state = Arc::new(Mutex::new(Bzm2AppliedOperatingState {
            per_domain_voltage_mv: saved_state.per_domain_voltage_mv.clone(),
            per_asic_pll_mhz: saved_state.per_asic_pll_mhz.clone(),
            saved_operating_point: Some(saved_state),
            startup_path: Some(Bzm2StartupPath::SavedReplay),
            saved_operating_point_status: Some(Bzm2SavedOperatingPointStatus::Validated),
            saved_operating_point_reasons: Vec::new(),
        }));

        let tuning = reconcile_saved_operating_point_status(
            Bzm2TuningState {
                retune_pending: Some(true),
                retune_reasons: vec!["throughput regression".into()],
                ..Default::default()
            },
            &calibration,
            &bus_layouts,
            &applied_state,
        );
        assert_eq!(
            tuning.saved_operating_point_status,
            Some(Bzm2SavedOperatingPointStatus::Invalidated)
        );
        assert_eq!(
            tuning.saved_operating_point_reasons,
            vec!["throughput regression"]
        );
        assert_eq!(tuning.reuse_saved_operating_point, Some(false));

        let applied = applied_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        assert!(applied.saved_operating_point.is_some());
        assert_eq!(
            applied.saved_operating_point_status,
            Some(Bzm2SavedOperatingPointStatus::Invalidated)
        );

        let stored = load_saved_operating_point_profile(Some(&profile_path))
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.persisted.unwrap().saved_operating_point_status,
            Bzm2SavedOperatingPointStatus::Invalidated
        );

        let _ = fs::remove_file(profile_path);
    }

    #[test]
    fn persistent_retune_triggers_invalidate_saved_operating_point() {
        let mut calibration = Bzm2CalibrationConfig::default();
        calibration.runtime_retune_persistence_polls = 2;
        calibration.runtime_retune_thermal_c = 80.0;
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let profile_path = std::env::temp_dir().join(format!(
            "bzm2-persistent-invalidate-{}-{}.json",
            std::process::id(),
            unique
        ));
        calibration.profile_path = Some(profile_path.clone());
        let bus_layouts = vec![Bzm2BusLayout {
            serial_path: "/dev/ttyUSB0".into(),
            asic_start: 0,
            asic_count: 1,
        }];
        let saved_state = Bzm2SavedOperatingPoint {
            board_voltage_mv: 17_500,
            board_throughput_ths: 40.0,
            per_domain_voltage_mv: BTreeMap::from([(0, 17_500)]),
            per_asic_engine_topology: BTreeMap::new(),
            per_asic_pll_mhz: BTreeMap::from([(0, [1_100.0, 1_100.0])]),
        };
        let applied_state = Arc::new(Mutex::new(Bzm2AppliedOperatingState {
            per_domain_voltage_mv: saved_state.per_domain_voltage_mv.clone(),
            per_asic_pll_mhz: saved_state.per_asic_pll_mhz.clone(),
            saved_operating_point: Some(saved_state),
            startup_path: Some(Bzm2StartupPath::SavedReplay),
            saved_operating_point_status: Some(Bzm2SavedOperatingPointStatus::Validated),
            saved_operating_point_reasons: Vec::new(),
        }));
        let measurement_cache = Bzm2RuntimeMeasurementCache {
            domain_measurements: BTreeMap::new(),
            asic_measurements: BTreeMap::from([(
                0,
                Bzm2AsicMeasurement {
                    asic_id: 0,
                    temperature_c: Some(82.0),
                    throughput_ths: Some(0.30),
                    average_pass_rate: Some(0.97),
                    pll_pass_rates: [Some(0.97), Some(0.97)],
                },
            )]),
        };
        let mut tracker = Bzm2RetuneTriggerTracker::default();
        let tuning = Bzm2TuningState {
            needs_retune: Some(true),
            ..Default::default()
        };

        // First poll: the trigger fires but has not persisted; the saved
        // point keeps its validated status.
        let first = apply_runtime_retune_triggers(
            tuning.clone(),
            &calibration,
            &measurement_cache,
            &mut tracker,
        );
        assert_eq!(first.retune_pending, Some(false));
        let first = reconcile_saved_operating_point_status(
            first,
            &calibration,
            &bus_layouts,
            &applied_state,
        );
        assert_eq!(
            first.saved_operating_point_status,
            Some(Bzm2SavedOperatingPointStatus::Validated)
        );

        // Second poll: the trigger passes the persistence threshold; the
        // saved point is invalidated and the invalidation is persisted.
        let second =
            apply_runtime_retune_triggers(tuning, &calibration, &measurement_cache, &mut tracker);
        assert_eq!(second.retune_pending, Some(true));
        let second = reconcile_saved_operating_point_status(
            second,
            &calibration,
            &bus_layouts,
            &applied_state,
        );
        assert_eq!(
            second.saved_operating_point_status,
            Some(Bzm2SavedOperatingPointStatus::Invalidated)
        );

        let stored = load_saved_operating_point_profile(Some(&profile_path))
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.persisted.unwrap().saved_operating_point_status,
            Bzm2SavedOperatingPointStatus::Invalidated
        );

        let _ = fs::remove_file(profile_path);
    }

    /// The sensor name is produced in `asic::bzm2::thread` and consumed here, so
    /// both sides must derive the prefix identically. Two independent copies
    /// previously disagreed (`-` vs `_`), which silently broke this lookup for
    /// udev `by-id` paths. A plain `ttyUSB0` is all-alphanumeric and hides the
    /// bug, so the regression case has to be a path containing non-alphanumerics.
    #[test]
    fn asic_temperature_lookup_matches_emitted_name_for_by_id_paths() {
        for serial_path in [
            "/dev/serial/by-id/usb-FTDI_FT232R_USB_UART_A50285BI-if00-port0",
            "/dev/ttyUSB0",
        ] {
            // Exactly how `asic::bzm2::thread` names a DTS reading.
            let emitted = format!("{}-asic-{}-dts", sensor_prefix(serial_path), 2u8);
            let sensors = vec![TemperatureSensor {
                name: emitted.clone(),
                temperature: Some(Temperature::from_celsius(71.5)),
                observed_at: Some(std::time::Instant::now()),
            }];

            assert_eq!(
                asic_temperature_for_sensor(&sensors, serial_path, 2),
                Some(71.5),
                "lookup failed for {serial_path} (emitted name was {emitted})"
            );
        }
    }
}
