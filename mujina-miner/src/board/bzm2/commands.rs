//! BoardCommand dispatch loop for the BZM2 board.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::api::commands::BoardCommand;
use crate::api_client::types::{Bzm2BusSummary, Bzm2ChainSummaryResponse};

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
                                    publish_thread_telemetry(&telemetry_tx, &update);
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
                            BoardCommand::SetFanTarget { reply, .. } => {
                                let _ = reply
                                    .send(Err(anyhow::anyhow!("BZM2 board has no controllable fans")));
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
