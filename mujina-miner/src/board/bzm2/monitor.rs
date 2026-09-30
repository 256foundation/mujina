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
