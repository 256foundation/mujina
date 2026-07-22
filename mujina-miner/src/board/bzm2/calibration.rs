//! Chain enumeration, calibration planner I/O, and operating-point persistence for the BZM2 board.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::api_client::types::{Bzm2SavedOperatingPointStatus, Bzm2StartupPath};
use crate::asic::bzm2::{Bzm2DiscoveredEngineMap, Bzm2UartController};
use crate::tracing::prelude::*;
use crate::transport::SerialStream;
use crate::tuning::blockscale::{
    Bzm2AsicMeasurement, Bzm2AsicTopology, Bzm2BoardCalibrationInput, Bzm2CalibrationConstraints,
    Bzm2CalibrationPlanner, Bzm2DomainMeasurement, Bzm2SavedEngineCoordinate,
    Bzm2SavedEngineTopology, Bzm2SavedOperatingPoint, Bzm2VoltageDomain,
};

use super::config::{
    Bzm2CalibrationConfig, DEFAULT_CALIBRATION_SITE_TEMP_C, DEFAULT_ENUMERATION_MAX_ASICS_PER_BUS,
    average_u32, operating_class_name, performance_mode_name,
};
use super::telemetry::{
    publish_discovered_engine_map, publish_saved_engine_topology, snapshot_input_power,
    snapshot_temperature,
};
use super::{BoardError, Bzm2Board};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Bzm2BusLayout {
    pub(super) serial_path: String,
    pub(super) asic_start: u16,
    pub(super) asic_count: u16,
}

impl Bzm2BusLayout {
    pub(super) fn contains(&self, global_asic_id: u16) -> bool {
        global_asic_id >= self.asic_start && global_asic_id < self.asic_start + self.asic_count
    }

    pub(super) fn global_asic_id(&self, local_asic_id: u8) -> Option<u16> {
        (u16::from(local_asic_id) < self.asic_count)
            .then_some(self.asic_start + u16::from(local_asic_id))
    }

    pub(super) fn local_asic_id(&self, global_asic_id: u16) -> Option<u8> {
        self.contains(global_asic_id)
            .then_some((global_asic_id - self.asic_start) as u8)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(super) struct Bzm2PersistedCalibrationProfile {
    pub(super) schema_version: u32,
    #[serde(alias = "board_bin")]
    pub(super) operating_class: String,
    #[serde(alias = "strategy")]
    pub(super) performance_mode: String,
    pub(super) asics_per_bus: Vec<u16>,
    pub(super) pll_post1_divider: u8,
    #[serde(default)]
    pub(super) saved_operating_point_status: Bzm2SavedOperatingPointStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) saved_operating_point_reasons: Vec<String>,
    #[serde(alias = "calibration")]
    pub(super) saved_state: Bzm2SavedOperatingPoint,
}

impl Bzm2PersistedCalibrationProfile {
    const SCHEMA_VERSION: u32 = 1;

    fn is_compatible(
        &self,
        calibration: &Bzm2CalibrationConfig,
        bus_layouts: &[Bzm2BusLayout],
    ) -> bool {
        self.schema_version == Self::SCHEMA_VERSION
            && self.saved_operating_point_status != Bzm2SavedOperatingPointStatus::Invalidated
            && self.operating_class == operating_class_name(calibration.operating_class)
            && self.performance_mode == performance_mode_name(calibration.performance_mode)
            && self.pll_post1_divider == calibration.pll_post1_divider
            && self.asics_per_bus
                == bus_layouts
                    .iter()
                    .map(|bus| bus.asic_count)
                    .collect::<Vec<_>>()
            && self.saved_state.per_asic_pll_mhz.len()
                == bus_layouts
                    .iter()
                    .map(|bus| bus.asic_count as usize)
                    .sum::<usize>()
    }
}

#[derive(Debug, Clone)]
pub(super) struct Bzm2LoadedCalibrationProfile {
    pub(super) persisted: Option<Bzm2PersistedCalibrationProfile>,
    pub(super) saved_state: Bzm2SavedOperatingPoint,
}

#[derive(Debug, Clone, Default)]
pub(super) struct Bzm2AppliedOperatingState {
    pub(super) per_domain_voltage_mv: BTreeMap<u16, u32>,
    pub(super) per_asic_pll_mhz: BTreeMap<u16, [f32; 2]>,
    pub(super) saved_operating_point: Option<Bzm2SavedOperatingPoint>,
    pub(super) startup_path: Option<Bzm2StartupPath>,
    pub(super) saved_operating_point_status: Option<Bzm2SavedOperatingPointStatus>,
    pub(super) saved_operating_point_reasons: Vec<String>,
}

impl Bzm2Board {
    pub(super) async fn resolve_bus_layouts(&self) -> Result<Vec<Bzm2BusLayout>, BoardError> {
        let configured = build_bus_layouts(
            &self.config.serial_paths,
            &self.config.calibration.asics_per_bus,
        );
        if !self.config.enumeration.enabled {
            return Ok(configured);
        }

        let discovered = self.enumerate_bus_layouts().await?;
        if should_fallback_to_configured_bus_layouts(&discovered, &configured) {
            warn!(
                board = %self.config.device_id(),
                "BZM2 startup enumeration found no ASICs on the default id; falling back to configured bus topology"
            );
            return Ok(configured);
        }

        Ok(discovered)
    }

    async fn enumerate_bus_layouts(&self) -> Result<Vec<Bzm2BusLayout>, BoardError> {
        let mut counts = Vec::with_capacity(self.config.serial_paths.len());

        for (index, serial_path) in self.config.serial_paths.iter().enumerate() {
            let max_asics = *self
                .config
                .enumeration
                .max_asics_per_bus
                .get(index)
                .or_else(|| self.config.enumeration.max_asics_per_bus.last())
                .unwrap_or(&DEFAULT_ENUMERATION_MAX_ASICS_PER_BUS);
            let max_asics = max_asics.min(u8::MAX as u16) as u8;

            let stream = SerialStream::new(serial_path, self.config.baud_rate).map_err(|err| {
                BoardError::InitializationFailed(format!(
                    "Failed to open BZM2 enumeration transport {}: {}",
                    serial_path, err
                ))
            })?;
            let (reader, writer, _control) = stream.split();
            let mut uart = Bzm2UartController::new(reader, writer);
            let assigned = uart
                .enumerate_chain(max_asics, self.config.enumeration.start_id)
                .await
                .map_err(|err| {
                    BoardError::InitializationFailed(format!(
                        "BZM2 startup enumeration failed on {}: {}",
                        serial_path, err
                    ))
                })?;
            counts.push(assigned.len() as u16);
            info!(
                board = %self.config.device_id(),
                serial_path,
                asic_count = assigned.len(),
                "BZM2 startup enumeration completed"
            );
        }

        Ok(build_discovered_bus_layouts(
            &self.config.serial_paths,
            &counts,
        ))
    }

    pub(super) async fn execute_live_calibration(
        &self,
        bus_layouts: &[Bzm2BusLayout],
    ) -> Result<(), BoardError> {
        let calibration = &self.config.calibration;
        if !calibration.enabled {
            return Ok(());
        }

        let total_asics = bus_layouts
            .iter()
            .map(|layout| layout.asic_count as usize)
            .sum::<usize>();
        if total_asics == 0 {
            return Ok(());
        }

        let loaded_profile =
            load_saved_operating_point_profile(calibration.profile_path.as_deref())
                .map_err(BoardError::InitializationFailed)?;
        if calibration.apply_saved_operating_point
            && !calibration.force_retune
            && let Some(profile) = loaded_profile
                .as_ref()
                .and_then(|loaded| loaded.persisted.as_ref())
                .filter(|profile| profile.is_compatible(calibration, bus_layouts))
        {
            self.apply_saved_operating_point(bus_layouts, profile)
                .await?;
            info!(
                board = %self.config.device_id(),
                asic_count = profile.saved_state.per_asic_pll_mhz.len(),
                "BZM2 replayed saved operating point profile"
            );
            return Ok(());
        }

        let telemetry = self.config.telemetry.snapshot();
        let site_temp_c = calibration
            .site_temp_c
            .or_else(|| snapshot_temperature(&telemetry, "board"))
            .or_else(|| snapshot_temperature(&telemetry, "asic"))
            .unwrap_or(DEFAULT_CALIBRATION_SITE_TEMP_C);
        let saved_operating_point = loaded_profile
            .as_ref()
            .and_then(saved_operating_point_from_loaded_profile);
        let engine_topology = self
            .resolve_engine_topology_for_calibration(bus_layouts, saved_operating_point.as_ref())
            .await;
        let (voltage_domains, domain_lookup) = build_voltage_domains(
            total_asics as u16,
            &calibration.asics_per_domain,
            &calibration.domain_voltage_offsets_mv,
        );
        let asics = build_topology(bus_layouts, &domain_lookup, &engine_topology);
        let per_asic_throughput = saved_operating_point
            .as_ref()
            .map(|stored| distribute_saved_throughput(stored.board_throughput_ths, &asics));
        let shared_temp = snapshot_temperature(&telemetry, "asic")
            .or_else(|| snapshot_temperature(&telemetry, "board"));
        let asic_measurements = asics
            .iter()
            .map(|asic| Bzm2AsicMeasurement {
                asic_id: asic.asic_id,
                temperature_c: shared_temp,
                throughput_ths: per_asic_throughput
                    .as_ref()
                    .and_then(|throughput| throughput.get(&asic.asic_id).copied()),
                average_pass_rate: None,
                pll_pass_rates: [None, None],
            })
            .collect::<Vec<_>>();
        let shared_domain_power = snapshot_input_power(&telemetry).map(|power| {
            if voltage_domains.is_empty() {
                power
            } else {
                power / voltage_domains.len() as f32
            }
        });
        let domain_measurements = voltage_domains
            .iter()
            .map(|domain| Bzm2DomainMeasurement {
                domain_id: domain.domain_id,
                measured_voltage_mv: None,
                measured_power_w: shared_domain_power,
            })
            .collect::<Vec<_>>();

        let planner = Bzm2CalibrationPlanner;
        let plan = planner.plan(&Bzm2BoardCalibrationInput {
            operating_class: calibration.operating_class,
            site_temp_c,
            target_mode: calibration.performance_mode,
            mode: calibration.mode,
            per_stack_clocking: calibration.per_stack_clocking,
            voltage_domains: voltage_domains.clone(),
            asics: asics.clone(),
            saved_operating_point,
            domain_measurements,
            asic_measurements,
            constraints: Bzm2CalibrationConstraints::default(),
            force_retune: calibration.force_retune,
        });
        let per_domain_voltage_mv = plan
            .domain_plans
            .iter()
            .map(|domain| (domain.domain_id, domain.voltage_mv))
            .collect::<BTreeMap<_, _>>();
        self.apply_domain_voltage_map(&per_domain_voltage_mv)
            .await?;

        let per_asic_pll_mhz = plan
            .asic_plans
            .iter()
            .map(|plan| (plan.asic_id, plan.pll_frequencies_mhz))
            .collect::<BTreeMap<_, _>>();
        self.apply_frequency_map(
            bus_layouts,
            [plan.initial_frequency_mhz; 2],
            &per_asic_pll_mhz,
        )
        .await?;
        let current_saved_operating_point = Bzm2SavedOperatingPoint {
            board_voltage_mv: average_u32(plan.domain_plans.iter().map(|domain| domain.voltage_mv))
                .unwrap_or(plan.desired_voltage_mv),
            board_throughput_ths: estimate_planned_hashrate(
                &plan,
                self.config.nominal_hashrate_ths as f32,
                &asics,
            ),
            per_domain_voltage_mv: per_domain_voltage_mv.clone(),
            per_asic_engine_topology: engine_topology.clone(),
            per_asic_pll_mhz: per_asic_pll_mhz.clone(),
        };
        store_applied_operating_state(
            &self.applied_operating_state,
            &per_domain_voltage_mv,
            &per_asic_pll_mhz,
            Some(current_saved_operating_point.clone()),
            Some(Bzm2StartupPath::LiveCalibration),
            Some(Bzm2SavedOperatingPointStatus::Pending),
            &[],
        );

        if let Some(profile_path) = calibration.profile_path.as_deref() {
            let profile = Bzm2PersistedCalibrationProfile {
                schema_version: Bzm2PersistedCalibrationProfile::SCHEMA_VERSION,
                operating_class: operating_class_name(calibration.operating_class).into(),
                performance_mode: performance_mode_name(calibration.performance_mode).into(),
                asics_per_bus: bus_layouts.iter().map(|bus| bus.asic_count).collect(),
                pll_post1_divider: calibration.pll_post1_divider,
                saved_operating_point_status: Bzm2SavedOperatingPointStatus::Pending,
                saved_operating_point_reasons: Vec::new(),
                saved_state: current_saved_operating_point,
            };
            store_calibration_profile(profile_path, &profile)
                .map_err(BoardError::InitializationFailed)?;
        }

        info!(board = %self.config.device_id(), reuse_saved_operating_point = plan.reuse_saved_operating_point, needs_retune = plan.needs_retune, initial_frequency_mhz = plan.initial_frequency_mhz, asic_count = plan.asic_plans.len(), "BZM2 live calibration completed");
        Ok(())
    }

    async fn resolve_engine_topology_for_calibration(
        &self,
        bus_layouts: &[Bzm2BusLayout],
        saved_operating_point: Option<&Bzm2SavedOperatingPoint>,
    ) -> BTreeMap<u16, Bzm2SavedEngineTopology> {
        let mut topology = saved_operating_point
            .map(|saved| saved.per_asic_engine_topology.clone())
            .unwrap_or_default();

        if self.config.calibration.discover_engine_topology {
            for (asic_id, discovery) in self
                .discover_engine_topology_for_calibration(bus_layouts)
                .await
            {
                topology.insert(asic_id, saved_engine_topology_from_discovery(&discovery));
            }
        }

        for (thread_index, bus) in bus_layouts.iter().enumerate() {
            for asic_id in bus.asic_start..bus.asic_start + bus.asic_count {
                let saved = topology
                    .entry(asic_id)
                    .or_insert_with(default_saved_engine_topology)
                    .clone();
                if let Some(local_asic) = bus.local_asic_id(asic_id) {
                    publish_saved_engine_topology(
                        &self.telemetry_tx,
                        thread_index,
                        &bus.serial_path,
                        local_asic,
                        &saved,
                    );
                }
            }
        }

        topology
    }

    async fn discover_engine_topology_for_calibration(
        &self,
        bus_layouts: &[Bzm2BusLayout],
    ) -> BTreeMap<u16, Bzm2DiscoveredEngineMap> {
        let mut topology = BTreeMap::new();

        for (thread_index, bus) in bus_layouts.iter().enumerate() {
            if bus.asic_count == 0 {
                continue;
            }
            let stream = match SerialStream::new(&bus.serial_path, self.config.baud_rate) {
                Ok(stream) => stream,
                Err(err) => {
                    warn!(
                        board = %self.config.device_id(),
                        path = %bus.serial_path,
                        error = %err,
                        "Failed to open BZM2 calibration discovery transport"
                    );
                    continue;
                }
            };
            let (reader, writer, _control) = stream.split();
            let mut uart = Bzm2UartController::new(reader, writer);

            for local_asic in 0..bus.asic_count {
                let global_asic = bus.asic_start + local_asic;
                match uart
                    .discover_engine_map(
                        local_asic as u8,
                        self.config.calibration.engine_discovery_tdm_prediv_raw,
                        self.config.calibration.engine_discovery_tdm_counter,
                        self.config.calibration.engine_discovery_timeout,
                    )
                    .await
                {
                    Ok(discovery) => {
                        publish_discovered_engine_map(
                            &self.telemetry_tx,
                            thread_index,
                            &bus.serial_path,
                            &discovery,
                        );
                        topology.insert(global_asic, discovery);
                    }
                    Err(err) => {
                        warn!(
                            board = %self.config.device_id(),
                            path = %bus.serial_path,
                            asic = local_asic,
                            error = %err,
                            "BZM2 calibration engine discovery failed; falling back to saved or default topology"
                        );
                    }
                }
            }
        }

        topology
    }
}

fn build_bus_layouts(serial_paths: &[String], asics_per_bus: &[u16]) -> Vec<Bzm2BusLayout> {
    build_bus_layouts_with_minimum(serial_paths, asics_per_bus, 1)
}

fn build_discovered_bus_layouts(
    serial_paths: &[String],
    asics_per_bus: &[u16],
) -> Vec<Bzm2BusLayout> {
    build_bus_layouts_with_minimum(serial_paths, asics_per_bus, 0)
}

fn build_bus_layouts_with_minimum(
    serial_paths: &[String],
    asics_per_bus: &[u16],
    minimum_asic_count: u16,
) -> Vec<Bzm2BusLayout> {
    let mut next_asic = 0u16;
    serial_paths
        .iter()
        .enumerate()
        .map(|(index, path)| {
            let asic_count = *asics_per_bus
                .get(index)
                .or_else(|| asics_per_bus.last())
                .unwrap_or(&1)
                .max(&minimum_asic_count);
            let layout = Bzm2BusLayout {
                serial_path: path.clone(),
                asic_start: next_asic,
                asic_count,
            };
            next_asic = next_asic.saturating_add(asic_count);
            layout
        })
        .collect()
}

fn should_fallback_to_configured_bus_layouts(
    discovered: &[Bzm2BusLayout],
    configured: &[Bzm2BusLayout],
) -> bool {
    let discovered_total = discovered
        .iter()
        .map(|layout| layout.asic_count as usize)
        .sum::<usize>();
    let configured_total = configured
        .iter()
        .map(|layout| layout.asic_count as usize)
        .sum::<usize>();
    discovered_total == 0 && configured_total > 0
}

pub(super) fn build_voltage_domains(
    total_asics: u16,
    asics_per_domain: &[u16],
    domain_voltage_offsets_mv: &[i32],
) -> (Vec<Bzm2VoltageDomain>, BTreeMap<u16, u16>) {
    let mut domains = Vec::new();
    let mut lookup = BTreeMap::new();
    let mut domain_id = 0u16;
    let mut asic_start = 0u16;
    while asic_start < total_asics {
        let requested = *asics_per_domain
            .get(domain_id as usize)
            .or_else(|| asics_per_domain.last())
            .unwrap_or(&total_asics)
            .max(&1);
        let asic_end = (asic_start.saturating_add(requested)).min(total_asics);
        let asic_ids = (asic_start..asic_end).collect::<Vec<_>>();
        for asic_id in &asic_ids {
            lookup.insert(*asic_id, domain_id);
        }
        domains.push(Bzm2VoltageDomain {
            domain_id,
            asic_ids,
            voltage_offset_mv: *domain_voltage_offsets_mv
                .get(domain_id as usize)
                .or_else(|| domain_voltage_offsets_mv.last())
                .unwrap_or(&0),
            max_power_w: None,
        });
        domain_id = domain_id.saturating_add(1);
        asic_start = asic_end;
    }
    (domains, lookup)
}

pub(super) fn build_topology(
    bus_layouts: &[Bzm2BusLayout],
    domain_lookup: &BTreeMap<u16, u16>,
    engine_topology: &BTreeMap<u16, Bzm2SavedEngineTopology>,
) -> Vec<Bzm2AsicTopology> {
    let mut asics = Vec::new();
    for layout in bus_layouts {
        for asic_id in layout.asic_start..layout.asic_start + layout.asic_count {
            let saved_topology = engine_topology
                .get(&asic_id)
                .cloned()
                .unwrap_or_else(default_saved_engine_topology);
            asics.push(Bzm2AsicTopology {
                asic_id,
                domain_id: *domain_lookup.get(&asic_id).unwrap_or(&0),
                pll_count: 2,
                alive: true,
                active_engine_count: saved_topology.active_engine_count,
                missing_engines: saved_topology.missing_engines,
            });
        }
    }
    asics
}

pub(super) fn default_saved_engine_topology() -> Bzm2SavedEngineTopology {
    Bzm2SavedEngineTopology {
        active_engine_count: crate::asic::bzm2::protocol::default_engine_coordinates().len() as u16,
        missing_engines: crate::asic::bzm2::protocol::default_excluded_engines()
            .into_iter()
            .map(|(row, col)| Bzm2SavedEngineCoordinate { row, col })
            .collect(),
    }
}

fn saved_engine_topology_from_discovery(
    discovery: &Bzm2DiscoveredEngineMap,
) -> Bzm2SavedEngineTopology {
    Bzm2SavedEngineTopology {
        active_engine_count: discovery.present_count() as u16,
        missing_engines: discovery
            .missing
            .iter()
            .map(|coord| Bzm2SavedEngineCoordinate {
                row: coord.row,
                col: coord.col,
            })
            .collect(),
    }
}

fn distribute_saved_throughput(
    total_throughput_ths: f32,
    asics: &[Bzm2AsicTopology],
) -> BTreeMap<u16, f32> {
    let total_active = asics
        .iter()
        .filter(|asic| asic.alive)
        .map(|asic| asic.active_engine_count.max(1) as f32)
        .sum::<f32>()
        .max(1.0);

    asics
        .iter()
        .filter(|asic| asic.alive)
        .map(|asic| {
            (
                asic.asic_id,
                total_throughput_ths * (asic.active_engine_count.max(1) as f32 / total_active),
            )
        })
        .collect()
}

pub(super) fn store_applied_operating_state(
    state: &Arc<Mutex<Bzm2AppliedOperatingState>>,
    per_domain_voltage_mv: &BTreeMap<u16, u32>,
    per_asic_pll_mhz: &BTreeMap<u16, [f32; 2]>,
    saved_operating_point: Option<Bzm2SavedOperatingPoint>,
    startup_path: Option<Bzm2StartupPath>,
    saved_operating_point_status: Option<Bzm2SavedOperatingPointStatus>,
    saved_operating_point_reasons: &[String],
) {
    let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
    guard.per_domain_voltage_mv = per_domain_voltage_mv.clone();
    guard.per_asic_pll_mhz = per_asic_pll_mhz.clone();
    guard.saved_operating_point = saved_operating_point;
    guard.startup_path = startup_path;
    guard.saved_operating_point_status = saved_operating_point_status;
    guard.saved_operating_point_reasons = saved_operating_point_reasons.to_vec();
}

pub(super) fn load_saved_operating_point_profile(
    path: Option<&Path>,
) -> Result<Option<Bzm2LoadedCalibrationProfile>, String> {
    let Some(path) = path else {
        return Ok(None);
    };
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path).map_err(|err| {
        format!(
            "Failed to read calibration profile {}: {}",
            path.display(),
            err
        )
    })?;

    if let Ok(profile) = serde_json::from_str::<Bzm2PersistedCalibrationProfile>(&raw) {
        return Ok(Some(Bzm2LoadedCalibrationProfile {
            saved_state: profile.saved_state.clone(),
            persisted: Some(profile),
        }));
    }

    serde_json::from_str::<Bzm2SavedOperatingPoint>(&raw)
        .map(|saved_state| {
            Some(Bzm2LoadedCalibrationProfile {
                persisted: None,
                saved_state,
            })
        })
        .map_err(|err| {
            format!(
                "Failed to parse calibration profile {}: {}",
                path.display(),
                err
            )
        })
}

fn saved_operating_point_from_loaded_profile(
    profile: &Bzm2LoadedCalibrationProfile,
) -> Option<Bzm2SavedOperatingPoint> {
    match profile.persisted.as_ref() {
        Some(persisted)
            if persisted.saved_operating_point_status
                == Bzm2SavedOperatingPointStatus::Invalidated =>
        {
            None
        }
        _ => Some(profile.saved_state.clone()),
    }
}

fn store_calibration_profile(
    path: &Path,
    profile: &Bzm2PersistedCalibrationProfile,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            format!(
                "Failed to create calibration profile directory {}: {}",
                parent.display(),
                err
            )
        })?;
    }
    let raw = serde_json::to_string_pretty(profile)
        .map_err(|err| format!("Failed to serialize calibration profile: {}", err))?;
    fs::write(path, raw).map_err(|err| {
        format!(
            "Failed to write calibration profile {}: {}",
            path.display(),
            err
        )
    })
}

pub(super) fn store_saved_operating_point_status(
    path: &Path,
    calibration: &Bzm2CalibrationConfig,
    bus_layouts: &[Bzm2BusLayout],
    saved_state: &Bzm2SavedOperatingPoint,
    status: Bzm2SavedOperatingPointStatus,
    reasons: &[String],
) -> Result<(), String> {
    store_calibration_profile(
        path,
        &Bzm2PersistedCalibrationProfile {
            schema_version: Bzm2PersistedCalibrationProfile::SCHEMA_VERSION,
            operating_class: operating_class_name(calibration.operating_class).into(),
            performance_mode: performance_mode_name(calibration.performance_mode).into(),
            asics_per_bus: bus_layouts.iter().map(|bus| bus.asic_count).collect(),
            pll_post1_divider: calibration.pll_post1_divider,
            saved_operating_point_status: status,
            saved_operating_point_reasons: reasons.to_vec(),
            saved_state: saved_state.clone(),
        },
    )
}

fn estimate_planned_hashrate(
    plan: &crate::tuning::blockscale::Bzm2CalibrationPlan,
    nominal_hashrate_ths: f32,
    asics: &[Bzm2AsicTopology],
) -> f32 {
    let nominal_board_hashrate =
        nominal_hashrate_ths * asics.iter().filter(|asic| asic.alive).count().max(1) as f32;
    let average_frequency_mhz = if plan.asic_plans.is_empty() {
        plan.desired_clock_mhz
    } else {
        plan.asic_plans
            .iter()
            .map(|asic| (asic.pll_frequencies_mhz[0] + asic.pll_frequencies_mhz[1]) / 2.0)
            .sum::<f32>()
            / plan.asic_plans.len() as f32
    };
    let ratio = if plan.desired_clock_mhz > 0.0 {
        average_frequency_mhz / plan.desired_clock_mhz
    } else {
        1.0
    };
    let active_engine_ratio = {
        let total_active = asics
            .iter()
            .filter(|asic| asic.alive)
            .map(|asic| asic.active_engine_count.max(1) as f32)
            .sum::<f32>()
            .max(1.0);
        let total_nominal = asics.iter().filter(|asic| asic.alive).count().max(1) as f32
            * default_saved_engine_topology().active_engine_count as f32;
        (total_active / total_nominal).max(0.1)
    };
    nominal_board_hashrate * ratio.max(0.1) * active_engine_ratio
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::bringup::Bzm2BringupConfig;
    use super::super::config::{
        Bzm2EnumerationConfig, Bzm2RuntimeConfig, DEFAULT_BAUD_RATE,
        DEFAULT_CALIBRATION_POST1_DIVIDER, DEFAULT_NOMINAL_HASHRATE_THS,
    };
    use super::super::telemetry::Bzm2TelemetryConfig;
    use super::super::test_support::spawn_chain_emulator;
    use super::*;
    use crate::api_client::types::BoardTelemetry;
    use crate::asic::bzm2::protocol::{OPCODE_UART_NOOP, encode_noop};
    use crate::tuning::blockscale::{Bzm2OperatingClass, Bzm2PerformanceMode};
    use nix::pty::openpty;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use tokio::sync::{mpsc, watch};

    #[tokio::test]
    async fn live_calibration_persists_profile() {
        let pty = openpty(None, None).unwrap();
        let serial_path = fs::read_link(format!("/proc/self/fd/{}", pty.slave.as_raw_fd()))
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let profile_path = std::env::temp_dir().join(format!(
            "bzm2-profile-{}-{}.json",
            std::process::id(),
            unique
        ));
        let rail0_path = std::env::temp_dir().join(format!("bzm2-domain-rail0-{unique}.txt"));
        let rail1_path = std::env::temp_dir().join(format!("bzm2-domain-rail1-{unique}.txt"));

        let config = Bzm2RuntimeConfig {
            serial_paths: vec![serial_path],
            baud_rate: DEFAULT_BAUD_RATE,
            timestamp_count: crate::asic::bzm2::protocol::DEFAULT_TIMESTAMP_COUNT,
            nonce_gap: crate::asic::bzm2::protocol::DEFAULT_NONCE_GAP,
            dispatch_interval: Duration::from_millis(50),
            nominal_hashrate_ths: DEFAULT_NOMINAL_HASHRATE_THS,
            dts_vs_generation: crate::asic::bzm2::protocol::DtsVsGeneration::Gen2,
            telemetry: Bzm2TelemetryConfig::default(),
            enumeration: Bzm2EnumerationConfig::default(),
            bringup: Bzm2BringupConfig {
                rail_set_paths: vec![
                    rail0_path.to_string_lossy().into_owned(),
                    rail1_path.to_string_lossy().into_owned(),
                ],
                rail_write_scales: vec![1000.0, 1000.0],
                ..Default::default()
            },
            calibration: Bzm2CalibrationConfig {
                enabled: true,
                asics_per_bus: vec![2],
                asics_per_domain: vec![1],
                domain_voltage_offsets_mv: vec![0, 100],
                profile_path: Some(profile_path.clone()),
                skip_lock_check: true,
                ..Default::default()
            },
        };
        let (telemetry_tx, _telemetry_rx) = watch::channel(BoardTelemetry {
            name: "bzm2-test".into(),
            model: "BZM2".into(),
            serial: Some("bzm2-test".into()),
            ..Default::default()
        });
        let board = Bzm2Board::new(config, telemetry_tx, mpsc::channel(1).1);
        let bus_layouts = board.resolve_bus_layouts().await.unwrap();

        board.execute_live_calibration(&bus_layouts).await.unwrap();

        let profile = load_saved_operating_point_profile(Some(&profile_path))
            .unwrap()
            .unwrap();
        assert_eq!(profile.saved_state.per_asic_pll_mhz.len(), 2);
        assert_eq!(profile.saved_state.per_domain_voltage_mv.len(), 2);
        assert_eq!(profile.saved_state.per_asic_engine_topology.len(), 2);
        assert_eq!(
            profile
                .saved_state
                .per_asic_engine_topology
                .get(&0)
                .unwrap()
                .active_engine_count,
            default_saved_engine_topology().active_engine_count
        );
        assert_eq!(
            fs::read_to_string(&rail0_path).unwrap().trim(),
            profile
                .saved_state
                .per_domain_voltage_mv
                .get(&0)
                .unwrap()
                .to_string()
        );
        assert_eq!(
            fs::read_to_string(&rail1_path).unwrap().trim(),
            profile
                .saved_state
                .per_domain_voltage_mv
                .get(&1)
                .unwrap()
                .to_string()
        );
        assert!(profile.persisted.is_some());

        let _ = fs::remove_file(profile_path);
        let _ = fs::remove_file(rail0_path);
        let _ = fs::remove_file(rail1_path);
        drop(pty);
    }

    #[tokio::test]
    async fn stored_profile_replays_on_restart_without_rewrite() {
        let pty = openpty(None, None).unwrap();
        let serial_path = fs::read_link(format!("/proc/self/fd/{}", pty.slave.as_raw_fd()))
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let profile_path = std::env::temp_dir().join(format!(
            "bzm2-replay-{}-{}.json",
            std::process::id(),
            unique
        ));
        let rail0_path = std::env::temp_dir().join(format!("bzm2-replay-rail0-{unique}.txt"));
        let rail1_path = std::env::temp_dir().join(format!("bzm2-replay-rail1-{unique}.txt"));
        let persisted = Bzm2PersistedCalibrationProfile {
            schema_version: Bzm2PersistedCalibrationProfile::SCHEMA_VERSION,
            operating_class: operating_class_name(Bzm2OperatingClass::Generic).into(),
            performance_mode: performance_mode_name(Bzm2PerformanceMode::Standard).into(),
            asics_per_bus: vec![2],
            pll_post1_divider: DEFAULT_CALIBRATION_POST1_DIVIDER,
            saved_operating_point_status: Bzm2SavedOperatingPointStatus::Validated,
            saved_operating_point_reasons: Vec::new(),
            saved_state: Bzm2SavedOperatingPoint {
                board_voltage_mv: 17_500,
                board_throughput_ths: 80.0,
                per_domain_voltage_mv: BTreeMap::from([(0, 17_450), (1, 17_600)]),
                per_asic_engine_topology: BTreeMap::new(),
                per_asic_pll_mhz: BTreeMap::from([
                    (0, [1_100.0, 1_125.0]),
                    (1, [1_150.0, 1_175.0]),
                ]),
            },
        };
        let original = serde_json::to_string_pretty(&persisted).unwrap();
        fs::write(&profile_path, &original).unwrap();

        let config = Bzm2RuntimeConfig {
            serial_paths: vec![serial_path],
            baud_rate: DEFAULT_BAUD_RATE,
            timestamp_count: crate::asic::bzm2::protocol::DEFAULT_TIMESTAMP_COUNT,
            nonce_gap: crate::asic::bzm2::protocol::DEFAULT_NONCE_GAP,
            dispatch_interval: Duration::from_millis(50),
            nominal_hashrate_ths: DEFAULT_NOMINAL_HASHRATE_THS,
            dts_vs_generation: crate::asic::bzm2::protocol::DtsVsGeneration::Gen2,
            telemetry: Bzm2TelemetryConfig::default(),
            enumeration: Bzm2EnumerationConfig::default(),
            bringup: Bzm2BringupConfig {
                rail_set_paths: vec![
                    rail0_path.to_string_lossy().into_owned(),
                    rail1_path.to_string_lossy().into_owned(),
                ],
                rail_write_scales: vec![1000.0, 1000.0],
                ..Default::default()
            },
            calibration: Bzm2CalibrationConfig {
                enabled: true,
                apply_saved_operating_point: true,
                asics_per_bus: vec![2],
                profile_path: Some(profile_path.clone()),
                skip_lock_check: true,
                ..Default::default()
            },
        };
        let (telemetry_tx, _telemetry_rx) = watch::channel(BoardTelemetry {
            name: "bzm2-test".into(),
            model: "BZM2".into(),
            serial: Some("bzm2-test".into()),
            ..Default::default()
        });
        let board = Bzm2Board::new(config, telemetry_tx, mpsc::channel(1).1);
        let bus_layouts = board.resolve_bus_layouts().await.unwrap();

        board.execute_live_calibration(&bus_layouts).await.unwrap();

        assert_eq!(fs::read_to_string(&profile_path).unwrap(), original);
        assert_eq!(fs::read_to_string(&rail0_path).unwrap().trim(), "17450");
        assert_eq!(fs::read_to_string(&rail1_path).unwrap().trim(), "17600");

        let _ = fs::remove_file(profile_path);
        let _ = fs::remove_file(rail0_path);
        let _ = fs::remove_file(rail1_path);
        drop(pty);
    }

    #[test]
    fn build_bus_layouts_assigns_global_ranges() {
        let layouts = build_bus_layouts(&["/dev/ttyUSB0".into(), "/dev/ttyUSB1".into()], &[4, 6]);
        assert_eq!(layouts[0].asic_start, 0);
        assert_eq!(layouts[0].asic_count, 4);
        assert_eq!(layouts[1].asic_start, 4);
        assert_eq!(layouts[1].asic_count, 6);
    }

    #[tokio::test]
    async fn resolve_bus_layouts_uses_startup_enumeration_counts() {
        let pty = openpty(None, None).unwrap();
        let master = pty.master;
        let slave = pty.slave;
        let serial_path = fs::read_link(format!("/proc/self/fd/{}", slave.as_raw_fd()))
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let emulator = spawn_chain_emulator(master, 2, 0);

        let config = Bzm2RuntimeConfig {
            serial_paths: vec![serial_path],
            baud_rate: DEFAULT_BAUD_RATE,
            timestamp_count: crate::asic::bzm2::protocol::DEFAULT_TIMESTAMP_COUNT,
            nonce_gap: crate::asic::bzm2::protocol::DEFAULT_NONCE_GAP,
            dispatch_interval: Duration::from_millis(50),
            nominal_hashrate_ths: DEFAULT_NOMINAL_HASHRATE_THS,
            dts_vs_generation: crate::asic::bzm2::protocol::DtsVsGeneration::Gen2,
            telemetry: Bzm2TelemetryConfig::default(),
            enumeration: Bzm2EnumerationConfig {
                enabled: true,
                start_id: 0,
                max_asics_per_bus: vec![4],
            },
            bringup: Bzm2BringupConfig::default(),
            calibration: Bzm2CalibrationConfig::default(),
        };
        let (telemetry_tx, _telemetry_rx) = watch::channel(BoardTelemetry {
            name: "bzm2-test".into(),
            model: "BZM2".into(),
            serial: Some("bzm2-test".into()),
            ..Default::default()
        });
        let board = Bzm2Board::new(config, telemetry_tx, mpsc::channel(1).1);

        let layouts = board.resolve_bus_layouts().await.unwrap();
        assert_eq!(layouts.len(), 1);
        assert_eq!(layouts[0].asic_count, 2);

        emulator.join().unwrap();
    }

    #[tokio::test]
    async fn resolve_bus_layouts_falls_back_to_configured_counts_when_default_id_is_silent() {
        let pty = openpty(None, None).unwrap();
        let master = pty.master;
        let slave = pty.slave;
        let serial_path = fs::read_link(format!("/proc/self/fd/{}", slave.as_raw_fd()))
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let emulator = std::thread::spawn(move || {
            let mut file = fs::File::from(master);
            let mut probe = vec![0u8; encode_noop(crate::asic::bzm2::DEFAULT_ASIC_ID).len()];
            file.read_exact(&mut probe).unwrap();
            assert_eq!(probe, encode_noop(crate::asic::bzm2::DEFAULT_ASIC_ID));
            file.write_all(&[
                crate::asic::bzm2::DEFAULT_ASIC_ID,
                OPCODE_UART_NOOP,
                b'N',
                b'O',
                b'P',
            ])
            .unwrap();
        });

        let config = Bzm2RuntimeConfig {
            serial_paths: vec![serial_path],
            baud_rate: DEFAULT_BAUD_RATE,
            timestamp_count: crate::asic::bzm2::protocol::DEFAULT_TIMESTAMP_COUNT,
            nonce_gap: crate::asic::bzm2::protocol::DEFAULT_NONCE_GAP,
            dispatch_interval: Duration::from_millis(50),
            nominal_hashrate_ths: DEFAULT_NOMINAL_HASHRATE_THS,
            dts_vs_generation: crate::asic::bzm2::protocol::DtsVsGeneration::Gen2,
            telemetry: Bzm2TelemetryConfig::default(),
            enumeration: Bzm2EnumerationConfig {
                enabled: true,
                start_id: 0,
                max_asics_per_bus: vec![4],
            },
            bringup: Bzm2BringupConfig::default(),
            calibration: Bzm2CalibrationConfig {
                asics_per_bus: vec![3],
                ..Default::default()
            },
        };
        let (telemetry_tx, _telemetry_rx) = watch::channel(BoardTelemetry {
            name: "bzm2-test".into(),
            model: "BZM2".into(),
            serial: Some("bzm2-test".into()),
            ..Default::default()
        });
        let board = Bzm2Board::new(config, telemetry_tx, mpsc::channel(1).1);

        let layouts = board.resolve_bus_layouts().await.unwrap();
        assert_eq!(layouts.len(), 1);
        assert_eq!(layouts[0].asic_count, 3);

        emulator.join().unwrap();
    }
}
