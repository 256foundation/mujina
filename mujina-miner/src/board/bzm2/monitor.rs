//! Runtime monitor loop and tuning evaluation for the BZM2 board.

use std::time::Instant;

use tokio::sync::watch;

use crate::tracing::prelude::*;

use crate::asic::bzm2::Bzm2ThreadHandle;
use crate::asic::bzm2::thread::ShutdownAsk;

use super::Bzm2Board;
use super::abort::{self, AbortCondition, AbortLimits, AbortSeverity};
use super::scram::{ScramAction, ScramLadder};
use super::telemetry::{merge_power_readings, merge_temperature_readings};

impl Bzm2Board {
    pub(super) fn spawn_monitor(&mut self) {
        if (!self.config.telemetry.is_enabled() && !self.config.bringup.has_telemetry())
            || self.monitor_task.is_some()
        {
            return;
        }

        let telemetry = self.config.telemetry.clone();
        let rail_telemetry = self.config.bringup.clone();
        let telemetry_tx = self.telemetry_tx.clone();
        let shutdown_handles = self.shutdown_handles.clone();
        let serial_controls = self.serial_controls.clone();
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
                        });
                        trace!(
                            board = %board_name,
                            bytes_read = total_stats.0,
                            bytes_written = total_stats.1,
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

                        let published = telemetry_tx.borrow().clone();
                        let condition = abort::evaluate(&published, &abort_limits, now, die_max_age)
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
#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::super::bringup::Bzm2BringupConfig;
    #[cfg(unix)]
    use super::super::config::{
        Bzm2CalibrationConfig, Bzm2EnumerationConfig, Bzm2RuntimeConfig, DEFAULT_BAUD_RATE,
        DEFAULT_NOMINAL_HASHRATE_THS,
    };
    #[cfg(unix)]
    use super::super::telemetry::{Bzm2TelemetryConfig, SensorSpec};
    use super::*;
    #[cfg(unix)]
    use crate::api_client::types::BoardTelemetry;
    #[cfg(unix)]
    use nix::pty::openpty;
    use std::fs;
    #[cfg(unix)]
    use std::os::fd::AsRawFd;
    #[cfg(unix)]
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};
    #[cfg(unix)]
    use tokio::sync::watch;

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
        let mut board = Bzm2Board::new(config, telemetry_tx);

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
}
