//! BoardCommand dispatch loop for the BZM2 board.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::api::commands::BoardCommand;
use crate::api_client::types::{Bzm2BusSummary, Bzm2ChainSummaryResponse};
use crate::tracing::prelude::*;

use super::calibration::Bzm2BusLayout;
use super::config::DEFAULT_ENGINE_DISCOVERY_TIMEOUT_MS;
use super::telemetry::{map_clock_report, publish_discovered_engine_map, publish_thread_telemetry};
use super::{BoardError, Bzm2Board};

impl Bzm2Board {
    pub(super) fn spawn_command_loop(&mut self) {
        if self.command_task.is_some() {
            return;
        }
        let Some(mut command_rx) = self.command_rx.take() else {
            return;
        };

        let telemetry_tx = self.telemetry_tx.clone();
        let shutdown_handles = self.shutdown_handles.clone();
        let serial_paths = self.config.serial_paths.clone();
        let bus_layouts = Arc::clone(&self.bus_layouts);
        let applied_operating_state = Arc::clone(&self.applied_operating_state);
        let board_name = self.config.device_id();
        let configured_asics_per_bus = self.config.calibration.asics_per_bus.clone();
        let enumeration_enabled = self.config.enumeration.enabled;
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        self.command_shutdown = Some(shutdown_tx);

        self.command_task = Some(tokio::spawn(async move {
            loop {
                tokio::select! {
                    command = command_rx.recv() => {
                        let Some(command) = command else {
                            break;
                        };
                        match command {
                            BoardCommand::QueryBzm2DtsVs { thread_index, asic, reply } => {
                                let result: Result<_, BoardError> = async {
                                    let handle = shutdown_handles.get(thread_index).ok_or_else(|| {
                                        BoardError::HardwareControl(format!(
                                            "invalid BZM2 thread index {thread_index} for board {board_name}"
                                        ))
                                    })?;
                                    let update = handle
                                        .query_dts_vs(asic)
                                        .await
                                        .map_err(|err| BoardError::HardwareControl(err.to_string()))?;
                                    publish_thread_telemetry(&telemetry_tx, thread_index, &update);
                                    Ok(())
                                }
                                .await;
                                let _ = reply.send(result.map_err(anyhow::Error::from));
                            }
                            BoardCommand::QueryBzm2Noop { thread_index, asic, reply } => {
                                let result: Result<_, BoardError> = async {
                                    let handle = shutdown_handles.get(thread_index).ok_or_else(|| {
                                        BoardError::HardwareControl(format!(
                                            "invalid BZM2 thread index {thread_index} for board {board_name}"
                                        ))
                                    })?;
                                    handle
                                        .noop(asic)
                                        .await
                                        .map_err(|err| BoardError::HardwareControl(err.to_string()))
                                }
                                .await;
                                let _ = reply.send(result.map_err(anyhow::Error::from));
                            }
                            BoardCommand::QueryBzm2ChainSummary { reply } => {
                                let bus_layouts =
                                    bus_layouts.lock().unwrap_or_else(|e| e.into_inner()).clone();
                                let applied = applied_operating_state
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .clone();
                                // `total_asics` below is `bus_layouts` summed, and
                                // `bus_layouts` is EITHER the configured topology
                                // (MUJINA_BZM2_ASICS_PER_BUS, enumeration disabled --
                                // the default) OR the discovered one (enumeration
                                // succeeded); which one it is gets decided once at
                                // startup in `resolve_bus_layouts` (calibration.rs)
                                // and is not recorded afterwards. A completely dark
                                // chain with enumeration off reports the same
                                // `total_asics` as a healthy one -- a consumer cannot
                                // tell configuration from measurement from this field
                                // alone. `Bzm2ChainSummaryResponse` (api_client/types.rs,
                                // not owned by this module) needs a machine-readable
                                // split to fix that:
                                //
                                //   pub configured_asics: u16,         // MUJINA_BZM2_ASICS_PER_BUS, always present
                                //   pub discovered_asics: Option<u16>, // Some(..) only when enumeration ran
                                //
                                // Until that lands, the same split is computed below
                                // and at least visible in the trace event.
                                let (configured_asics, discovered_asics) = chain_asic_counts(
                                    &configured_asics_per_bus,
                                    enumeration_enabled,
                                    &bus_layouts,
                                );
                                let summary = Bzm2ChainSummaryResponse {
                                    total_asics: bus_layouts
                                        .iter()
                                        .map(|bus| bus.asic_count)
                                        .sum::<u16>(),
                                    startup_path: applied.startup_path,
                                    saved_operating_point_status: applied.saved_operating_point_status,
                                    buses: bus_layouts
                                        .iter()
                                        .enumerate()
                                        .map(|(thread_index, bus)| Bzm2BusSummary {
                                            thread_index,
                                            serial_path: bus.serial_path.clone(),
                                            asic_start: bus.asic_start,
                                            asic_count: bus.asic_count,
                                        })
                                        .collect(),
                                };
                                debug!(
                                    board = %board_name,
                                    total_asics = summary.total_asics,
                                    configured_asics,
                                    discovered_asics = ?discovered_asics,
                                    "BZM2 chain summary requested"
                                );
                                let _ = reply.send(Ok(summary));
                            }
                            BoardCommand::QueryBzm2ClockReport {
                                thread_index,
                                asic,
                                reply,
                            } => {
                                let result: Result<_, BoardError> = async {
                                    let handle = shutdown_handles.get(thread_index).ok_or_else(|| {
                                        BoardError::HardwareControl(format!(
                                            "invalid BZM2 thread index {thread_index} for board {board_name}"
                                        ))
                                    })?;
                                    let report = handle
                                        .clock_report(asic)
                                        .await
                                        .map_err(|err| BoardError::HardwareControl(err.to_string()))?;
                                    Ok(map_clock_report(report))
                                }
                                .await;
                                let _ = reply.send(result.map_err(anyhow::Error::from));
                            }
                            BoardCommand::QueryBzm2Loopback {
                                thread_index,
                                asic,
                                payload,
                                reply,
                            } => {
                                let result: Result<_, BoardError> = async {
                                    let handle = shutdown_handles.get(thread_index).ok_or_else(|| {
                                        BoardError::HardwareControl(format!(
                                            "invalid BZM2 thread index {thread_index} for board {board_name}"
                                        ))
                                    })?;
                                    handle
                                        .loopback(asic, payload)
                                        .await
                                        .map_err(|err| BoardError::HardwareControl(err.to_string()))
                                }
                                .await;
                                let _ = reply.send(result.map_err(anyhow::Error::from));
                            }
                            BoardCommand::ReadBzm2Register {
                                thread_index,
                                asic,
                                engine_address,
                                offset,
                                count,
                                reply,
                            } => {
                                let result: Result<_, BoardError> = async {
                                    let handle = shutdown_handles.get(thread_index).ok_or_else(|| {
                                        BoardError::HardwareControl(format!(
                                            "invalid BZM2 thread index {thread_index} for board {board_name}"
                                        ))
                                    })?;
                                    handle
                                        .read_register(asic, engine_address, offset, count)
                                        .await
                                        .map_err(|err| BoardError::HardwareControl(err.to_string()))
                                }
                                .await;
                                let _ = reply.send(result.map_err(anyhow::Error::from));
                            }
                            BoardCommand::WriteBzm2Register {
                                thread_index,
                                asic,
                                engine_address,
                                offset,
                                value,
                                reply,
                            } => {
                                let result: Result<_, BoardError> = async {
                                    let handle = shutdown_handles.get(thread_index).ok_or_else(|| {
                                        BoardError::HardwareControl(format!(
                                            "invalid BZM2 thread index {thread_index} for board {board_name}"
                                        ))
                                    })?;
                                    handle
                                        .write_register(asic, engine_address, offset, value)
                                        .await
                                        .map_err(|err| BoardError::HardwareControl(err.to_string()))
                                }
                                .await;
                                let _ = reply.send(result.map_err(anyhow::Error::from));
                            }
                            BoardCommand::DiscoverBzm2Engines {
                                thread_index,
                                asic,
                                tdm_prediv_raw,
                                tdm_counter,
                                timeout_ms,
                                reply,
                            } => {
                                let result: Result<_, BoardError> = async {
                                    let handle = shutdown_handles.get(thread_index).ok_or_else(|| {
                                        BoardError::HardwareControl(format!(
                                            "invalid BZM2 thread index {thread_index} for board {board_name}"
                                        ))
                                    })?;
                                    let serial_path = serial_paths.get(thread_index).ok_or_else(|| {
                                        BoardError::HardwareControl(format!(
                                            "missing serial path for BZM2 thread index {thread_index} on board {board_name}"
                                        ))
                                    })?;
                                    let discovery = handle
                                        .discover_engine_map(
                                            asic,
                                            tdm_prediv_raw,
                                            tdm_counter,
                                            Duration::from_millis(u64::from(
                                                timeout_ms.unwrap_or(
                                                    DEFAULT_ENGINE_DISCOVERY_TIMEOUT_MS as u32,
                                                ),
                                            )),
                                        )
                                        .await
                                        .map_err(|err| BoardError::HardwareControl(err.to_string()))?;
                                    publish_discovered_engine_map(
                                        &telemetry_tx,
                                        thread_index,
                                        serial_path,
                                        &discovery,
                                    );
                                    Ok(())
                                }
                                .await;
                                let _ = reply.send(result.map_err(anyhow::Error::from));
                            }
                            BoardCommand::SetFanTarget {
                                fan, percent, reply, ..
                            } => {
                                // This used to refuse with "BZM2 board has no
                                // controllable fans". That was false: the
                                // chassis fans were driven across their whole
                                // range on 2026-09-21, 1,050 to 6,800 rpm.
                                //
                                // COMMANDING IS NOT THE WHOLE JOB. The gate is
                                // write-only and the duty node echoes whatever
                                // was written whether or not it drives
                                // anything, so this waits and reports what the
                                // TACHO says. A caller asking for 80% is told
                                // the rpm that resulted, not that a write
                                // succeeded.
                                let fans = super::fans::Bzm2Fans::new(super::platform::DEFAULT);
                                let result = async {
                                    let Some(pct) = percent else {
                                        anyhow::bail!(
                                            "automatic fan control is the thermal loop's job;                                              set an explicit percent to override it"
                                        );
                                    };
                                    let index: usize = fan
                                        .trim_start_matches("fan")
                                        .parse()
                                        .map_err(|_| anyhow::anyhow!("no fan named {fan}"))?;
                                    // ONE fan. This used to call the
                                    // all-fans helper, so a request for fan2
                                    // at 20% set every fan to 20% and then
                                    // reported fan2 -- and nothing in the
                                    // reply said the other three had moved.
                                    let outcome = fans.command_and_measure(index, pct).await?;
                                    let Some(rpm) = outcome.measured_rpm else {
                                        // Unreadable is not success. Saying so
                                        // is the whole point of measuring.
                                        anyhow::bail!(
                                            "fan{index} was commanded {pct}% and its tacho could \
                                             not be read, so whether it is turning is UNMEASURED"
                                        );
                                    };
                                    // A NUMBER IS NOT A CONFIRMATION. This
                                    // used to accept any rpm the tacho gave,
                                    // zero included, so a dead fan commanded
                                    // to 80% replied Ok. `confirmed` is the
                                    // check that existed for exactly this and
                                    // had no caller.
                                    //
                                    // Only when something was asked for: 0% is
                                    // a legitimate command and 0 rpm is its
                                    // correct outcome.
                                    if pct > 0 && !outcome.confirmed(super::config::DEFAULT_MIN_FAN_RPM) {
                                        anyhow::bail!(
                                            "fan{index} was commanded {pct}% and measured {rpm} \
                                             rpm, below the {} rpm floor. It is not turning as \
                                             asked; treat it as a mechanical or control fault.",
                                            super::config::DEFAULT_MIN_FAN_RPM
                                        );
                                    }
                                    // Report the PAIR, not just success. The
                                    // reply channel carries only Result<()>,
                                    // so the record is where the numbers land.
                                    tracing::info!(
                                        fan = index,
                                        commanded_pct = pct,
                                        measured_rpm = rpm,
                                        "fan commanded and confirmed by tacho"
                                    );
                                    Ok(format!(
                                        "fan{index} commanded {pct}%, measured {rpm} rpm"
                                    ))
                                }
                                .await;
                                let _ = reply.send(result.map(|_| ()));
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

/// Configured-vs-discovered ASIC counts for one chain summary. See the
/// comment above the `QueryBzm2ChainSummary` handler: this distinction
/// belongs in `Bzm2ChainSummaryResponse` itself and is computed here only
/// because that type is not owned by this module.
///
/// `configured_asics` is `MUJINA_BZM2_ASICS_PER_BUS` reflected back,
/// independent of whatever `bus_layouts` currently holds -- it is never a
/// measurement, and is always present. `discovered_asics` is `Some` only
/// when startup enumeration ran; `None` here means "nobody asked the
/// hardware", not "same as configured".
fn chain_asic_counts(
    configured_asics_per_bus: &[u16],
    enumeration_enabled: bool,
    bus_layouts: &[Bzm2BusLayout],
) -> (u16, Option<u16>) {
    let configured_asics = configured_asics_per_bus.iter().sum();
    let discovered_asics =
        enumeration_enabled.then(|| bus_layouts.iter().map(|bus| bus.asic_count).sum());
    (configured_asics, discovered_asics)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bus(serial_path: &str, asic_start: u16, asic_count: u16) -> Bzm2BusLayout {
        Bzm2BusLayout {
            serial_path: serial_path.into(),
            asic_start,
            asic_count,
        }
    }

    #[test]
    fn chain_asic_counts_configured_is_independent_of_bus_layouts() {
        // configured_asics_per_bus deliberately disagrees with bus_layouts:
        // configured_asics must reflect the CONFIG, never whatever
        // bus_layouts happens to hold right now.
        let (configured, discovered) =
            chain_asic_counts(&[100, 100, 100], false, &[bus("/dev/ttyUSB0", 0, 1)]);
        assert_eq!(configured, 300);
        assert_eq!(discovered, None);
    }

    #[test]
    fn chain_asic_counts_discovered_present_only_when_enumeration_ran() {
        let bus_layouts = [bus("/dev/ttyUSB0", 0, 2), bus("/dev/ttyUSB1", 2, 4)];

        let (_configured, discovered) = chain_asic_counts(&[100, 100], false, &bus_layouts);
        assert_eq!(
            discovered, None,
            "enumeration disabled must report no measurement, not the configured count"
        );

        let (_configured, discovered) = chain_asic_counts(&[100, 100], true, &bus_layouts);
        assert_eq!(
            discovered,
            Some(6),
            "enumeration enabled must report what it found"
        );
    }
}
