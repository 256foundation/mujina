//! Runtime monitor loop and tuning evaluation for the BZM2 board.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use tokio::sync::watch;

use crate::api_client::types::{
    AsicState, Bzm2AsicTuningState, Bzm2DomainTuningState, Bzm2PllTuningState,
    Bzm2SavedOperatingPointStatus, Bzm2TuningState, EngineCoordinate, TemperatureSensor,
};
use crate::asic::bzm2::{Bzm2ThreadHandle, Bzm2ThreadRuntimeMetrics};
use crate::tracing::prelude::*;
use crate::tuning::blockscale::{
    Bzm2AsicMeasurement, Bzm2BoardCalibrationInput, Bzm2CalibrationConstraints,
    Bzm2CalibrationPlanner, Bzm2DomainMeasurement, Bzm2SavedEngineCoordinate,
    Bzm2SavedEngineTopology,
};
use crate::types::Temperature;

use super::Bzm2Board;
use super::bringup::Bzm2BringupConfig;
use super::calibration::{
    Bzm2AppliedOperatingState, Bzm2BusLayout, build_topology, build_voltage_domains,
    default_saved_engine_topology, store_saved_operating_point_status,
};
use super::config::{Bzm2CalibrationConfig, DEFAULT_CALIBRATION_SITE_TEMP_C};
use super::telemetry::{Bzm2TelemetrySnapshot, merge_power_readings, merge_temperature_readings};

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
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        self.monitor_shutdown = Some(shutdown_tx);

        self.monitor_task = Some(tokio::spawn(async move {
            let mut interval = tokio::time::interval(telemetry.poll_interval);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut retune_tracker = Bzm2RetuneTriggerTracker::default();
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let snapshot = telemetry.snapshot();
                        let rail_snapshot = rail_telemetry.snapshot_telemetry();
                        let thread_metrics = collect_thread_runtime_metrics(&shutdown_handles).await;
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
                        });
                        trace!(
                            board = %board_name,
                            bytes_read = total_stats.0,
                            bytes_written = total_stats.1,
                            runtime_domain_count,
                            runtime_asic_count,
                            "BZM2 board telemetry updated"
                        );
                        if let Some(reason) = snapshot.trip_reason.clone() {
                            warn!(board = %board_name, reason = %reason, "BZM2 safety trip triggered");
                            for handle in &shutdown_handles {
                                handle.shutdown();
                            }
                            telemetry_tx.send_modify(|state| {
                                for thread in &mut state.threads {
                                    thread.is_active = false;
                                    thread.hashrate = 0;
                                }
                            });
                            break;
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

async fn collect_thread_runtime_metrics(
    handles: &[Bzm2ThreadHandle],
) -> BTreeMap<usize, Bzm2ThreadRuntimeMetrics> {
    let mut metrics = BTreeMap::new();
    for (thread_index, handle) in handles.iter().enumerate() {
        match handle.runtime_metrics().await {
            Ok(snapshot) => {
                metrics.insert(thread_index, snapshot);
            }
            Err(err) => {
                warn!(thread_index, error = %err, "Failed to query BZM2 runtime metrics");
            }
        }
    }
    metrics
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
        let frequencies = applied_operating_state
            .per_asic_pll_mhz
            .get(&global_asic_id)
            .copied();
        let mut pll_states = Vec::with_capacity(2);
        let mut pll_pass_rates = [None, None];

        for pll_index in 0..2usize {
            let throughput_hs = runtime_asic.and_then(|asic| asic.plls[pll_index].throughput_hs);
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
                frequency_mhz,
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
    plan: Option<crate::tuning::blockscale::Bzm2CalibrationPlan>,
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
        guard.saved_operating_point.as_ref().map(|_| {
            (
                Bzm2SavedOperatingPointStatus::Pending,
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
) -> Option<crate::tuning::blockscale::Bzm2CalibrationPlan> {
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
                                            sensor_prefix_from_serial(
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

fn sensor_prefix_from_serial(serial_path: &str) -> String {
    Path::new(serial_path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(serial_path)
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect()
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
    let prefix = Path::new(serial_path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(serial_path)
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect::<String>();
    let name = format!("{prefix}-asic-{asic}-dts");
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
    use crate::tuning::blockscale::Bzm2SavedOperatingPoint;
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

    #[cfg(unix)]
    #[tokio::test]
    async fn board_safety_trip_closes_scheduler_event_stream() {
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
            serial_paths: vec![serial_path],
            baud_rate: DEFAULT_BAUD_RATE,
            timestamp_count: crate::asic::bzm2::protocol::DEFAULT_TIMESTAMP_COUNT,
            nonce_gap: crate::asic::bzm2::protocol::DEFAULT_NONCE_GAP,
            dispatch_interval: Duration::from_millis(50),
            nominal_hashrate_ths: DEFAULT_NOMINAL_HASHRATE_THS,
            dts_vs_generation: crate::asic::bzm2::protocol::DtsVsGeneration::Gen2,
            telemetry: Bzm2TelemetryConfig {
                poll_interval: Duration::from_millis(20),
                asic_temp: Some(SensorSpec {
                    path: sensor_path.to_string_lossy().into_owned(),
                    scale: 1.0,
                }),
                max_asic_temp_c: Some(80.0),
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

        let closed = tokio::time::timeout(Duration::from_secs(1), async {
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
                if snapshot.temperatures.iter().any(|sensor| {
                    sensor.name == "asic"
                        && sensor.temperature.map(Temperature::as_degrees_c) == Some(90.0)
                }) {
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
        }];
        let temperatures = vec![TemperatureSensor {
            name: "ttyUSB0-asic-0-dts".into(),
            temperature: Some(Temperature::from_celsius(67.0)),
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
            startup_path: None,
            saved_operating_point_status: None,
            saved_operating_point_reasons: Vec::new(),
        };
        let thread_metrics = BTreeMap::from([(
            0usize,
            Bzm2ThreadRuntimeMetrics {
                throughput_hs: Some(358_720_000_000),
                asics: vec![crate::asic::bzm2::Bzm2AsicRuntimeMetrics {
                    asic: 0,
                    throughput_hs: Some(358_720_000_000),
                    scheduler_share_count: 12,
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

    #[test]
    fn evaluate_runtime_tuning_plan_flags_underperforming_saved_point() {
        let asics = vec![AsicState {
            id: 0,
            thread_index: Some(0),
            serial_path: Some("/dev/ttyUSB0".into()),
            discovered_engine_count: Some(236),
            missing_engines: Vec::new(),
        }];
        let temperatures = vec![TemperatureSensor {
            name: "ttyUSB0-asic-0-dts".into(),
            temperature: Some(Temperature::from_celsius(72.0)),
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
            Some(Bzm2SavedOperatingPointStatus::Pending)
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
            Some(Bzm2SavedOperatingPointStatus::Pending)
        );

        let stored = load_saved_operating_point_profile(Some(&profile_path))
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.persisted.unwrap().saved_operating_point_status,
            Bzm2SavedOperatingPointStatus::Pending
        );

        let _ = fs::remove_file(profile_path);
    }
}
