//! Runtime monitor loop and tuning evaluation for the BZM2 board.

use tokio::sync::watch;

use crate::tracing::prelude::*;

use super::Bzm2Board;
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
        let serial_controls = self.serial_controls.clone();
        let board_name = self.config.device_id();
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        self.monitor_shutdown = Some(shutdown_tx);

        self.monitor_task = Some(tokio::spawn(async move {
            let mut interval = tokio::time::interval(telemetry.poll_interval);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = interval.tick() => {
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
