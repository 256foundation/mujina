//! Chain enumeration, calibration planner I/O, and operating-point persistence for the BZM2 board.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::api_client::types::{Bzm2SavedOperatingPointStatus, Bzm2StartupPath};
use crate::asic::bzm2::{Bzm2DiscoveredEngineMap, Bzm2TdmControl, Bzm2UartController};
use crate::tracing::prelude::*;
use crate::tuning::calibration_planner::{
    Bzm2AsicMeasurement, Bzm2AsicTopology, Bzm2BoardCalibrationInput, Bzm2CalibrationConstraints,
    Bzm2CalibrationPlanner, Bzm2DomainMeasurement, Bzm2SavedEngineCoordinate,
    Bzm2SavedEngineTopology, Bzm2SavedOperatingPoint, Bzm2VoltageDomain,
};
use crate::tuning::thermal::ThermalCharacterisation;

use super::config::{
    Bzm2CalibrationConfig, DEFAULT_CALIBRATION_SITE_TEMP_C, DEFAULT_ENUMERATION_MAX_ASICS_PER_BUS,
    average_u32, operating_class_name, performance_mode_name,
};
use super::operating_point::{Bzm2OperatingPointRow, Bzm2OperatingPointTable};
use super::profile::{self, Bzm2ProfileIdentity};
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

    /// The ASIC ids that appear **on this bus's wire**.
    ///
    /// `asic_start` is a global index accumulated across buses, and exists so
    /// board-level bookkeeping can name every device on the machine uniquely.
    /// It is not what the chain says. Each bus is an independent chain
    /// addressed from `start_id`, so bus 1 reports 0..n whatever its global
    /// offset is.
    ///
    /// Handing the global range to a thread that compares it against wire ids
    /// disabled over-temperature shutdown on every bus after the first, and
    /// discarded their results, while a single-bus machine worked perfectly.
    pub(super) fn wire_asic_ids(&self, start_id: u8) -> Vec<u8> {
        (0..self.asic_count)
            .filter_map(|offset| u8::try_from(u16::from(start_id) + offset).ok())
            .collect()
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
    /// What hardware and firmware this profile describes.
    #[serde(default)]
    pub(super) identity: Bzm2ProfileIdentity,
    /// Unix epoch seconds at which the profile was written.
    #[serde(default)]
    pub(super) written_at_epoch_s: Option<u64>,
    /// Ambient temperature observed when the profile was written, against which
    /// the load-time reading is compared.
    #[serde(default)]
    pub(super) written_at_ambient_c: Option<f32>,
    /// SHA-256 over the profile with this field cleared.
    #[serde(default)]
    pub(super) checksum: Option<String>,
    #[serde(default)]
    pub(super) saved_operating_point_status: Bzm2SavedOperatingPointStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) saved_operating_point_reasons: Vec<String>,
    /// Operating points indexed by the die temperature they were learned at.
    #[serde(default)]
    pub(super) operating_points: Bzm2OperatingPointTable,
    /// Measured thermal resistance of this mechanical build, if characterised.
    ///
    /// Survives recalibration — changing voltage does not change the heatsink —
    /// and dies with the profile, which is the only invalidation software can
    /// see. A re-paste is electrically invisible and requires deleting the
    /// profile by hand.
    #[serde(default)]
    pub(super) thermal: Option<ThermalCharacterisation>,
    #[serde(alias = "calibration")]
    pub(super) saved_state: Bzm2SavedOperatingPoint,
}

impl Bzm2PersistedCalibrationProfile {
    /// Bumped from 1 when identity binding, checksums and the ambient gate were
    /// added. A version-1 profile carries none of them, so it cannot be
    /// validated and is discarded rather than trusted.
    const SCHEMA_VERSION: u32 = 2;

    /// Every reason this profile may not be replayed, or an empty list to use it.
    ///
    /// Reasons accumulate rather than short-circuiting: the list reaches the
    /// operator through `saved_operating_point_reasons`, and "firmware changed"
    /// alone suggests a rollback while "firmware changed and the engine map
    /// changed" says the board was swapped as well.
    fn refusal_reasons(&self, context: &Bzm2ProfileValidationContext<'_>) -> Vec<String> {
        let mut reasons = Vec::new();

        if self.schema_version != Self::SCHEMA_VERSION {
            reasons.push(format!(
                "profile schema {} is not the current {}",
                self.schema_version,
                Self::SCHEMA_VERSION
            ));
            // Nothing below can be trusted from an unknown schema, and the
            // fields the other checks read may not even be present.
            return reasons;
        }
        if let Some(reason) = self.checksum_mismatch() {
            reasons.push(reason);
            return reasons;
        }
        if self.saved_operating_point_status == Bzm2SavedOperatingPointStatus::Invalidated {
            reasons.push("profile was previously invalidated".into());
        }

        reasons.extend(self.identity.mismatches(context.identity));

        if self.operating_class != operating_class_name(context.calibration.operating_class) {
            reasons.push(format!(
                "operating class changed from {} to {}",
                self.operating_class,
                operating_class_name(context.calibration.operating_class)
            ));
        }
        if self.performance_mode != performance_mode_name(context.calibration.performance_mode) {
            reasons.push(format!(
                "performance mode changed from {} to {}",
                self.performance_mode,
                performance_mode_name(context.calibration.performance_mode)
            ));
        }
        if self.pll_post1_divider != context.calibration.pll_post1_divider {
            reasons.push(format!(
                "PLL post-1 divider changed from {} to {}",
                self.pll_post1_divider, context.calibration.pll_post1_divider
            ));
        }
        let expected_asics = context
            .bus_layouts
            .iter()
            .map(|bus| bus.asic_count as usize)
            .sum::<usize>();
        if self.saved_state.per_asic_pll_mhz.len() != expected_asics {
            reasons.push(format!(
                "profile holds {} ASIC frequency sets but the board has {} ASICs",
                self.saved_state.per_asic_pll_mhz.len(),
                expected_asics
            ));
        }
        if let Some(reason) =
            profile::ambient_mismatch(self.written_at_ambient_c, context.ambient_c)
        {
            reasons.push(reason);
        }

        reasons
    }

    /// Recompute the digest over everything but the digest itself.
    fn checksum_mismatch(&self) -> Option<String> {
        let Some(stored) = self.checksum.as_deref() else {
            return Some("profile carries no checksum".into());
        };
        let computed = self.compute_checksum().ok()?;
        (computed != stored).then(|| "profile checksum does not match its contents".into())
    }

    fn compute_checksum(&self) -> Result<String, serde_json::Error> {
        let mut bare = self.clone();
        bare.checksum = None;
        Ok(profile::checksum_of(&serde_json::to_string(&bare)?))
    }

    /// Stamp the digest in place. Called immediately before writing.
    fn seal(&mut self) -> Result<(), String> {
        self.checksum = None;
        self.checksum = Some(
            self.compute_checksum()
                .map_err(|err| format!("Failed to checksum calibration profile: {err}"))?,
        );
        Ok(())
    }
}

/// What a stored profile is checked against at load time.
pub(super) struct Bzm2ProfileValidationContext<'a> {
    pub(super) calibration: &'a Bzm2CalibrationConfig,
    pub(super) bus_layouts: &'a [Bzm2BusLayout],
    pub(super) identity: &'a Bzm2ProfileIdentity,
    /// Ambient **as read now**, not a value cached at process start. The whole
    /// question is whether the room has moved since the profile was written, and
    /// a boot-time constant cannot answer it.
    pub(super) ambient_c: Option<f32>,
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

            let stream = crate::transport::serial::open_with_platform_cflag(
                serial_path,
                self.config.baud_rate,
            )
            .map_err(|err| {
                BoardError::InitializationFailed(format!(
                    "Failed to open BZM2 enumeration transport {}: {}",
                    serial_path, err
                ))
            })?;
            let (reader, writer, _control) = super::config::split_chain_port(stream);
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

        // Read the telemetry before deciding, because the ambient check needs a
        // reading taken now rather than one carried from process start.
        let telemetry = self.config.telemetry.snapshot();
        let ambient_c = calibration
            .site_temp_c
            .or_else(|| snapshot_temperature(&telemetry, "board"))
            .or_else(|| snapshot_temperature(&telemetry, "asic"));

        // Discovery runs before the profile decision, not after it. The engine
        // map is how we tell one board from another, and a fast path that skips
        // finding out what hardware it is on is not a fast path but a guess.
        // This costs a read pass over the chain, not the calibration settle the
        // fast path exists to avoid.
        let discovered_topology = if calibration.discover_engine_topology {
            Some(
                self.discover_engine_topology_for_calibration(bus_layouts)
                    .await
                    .into_iter()
                    .map(|(asic_id, discovery)| {
                        (asic_id, saved_engine_topology_from_discovery(&discovery))
                    })
                    .collect::<BTreeMap<_, _>>(),
            )
        } else {
            None
        };
        let identity = Bzm2ProfileIdentity::new(
            &self.config.device_id(),
            bus_layouts.iter().map(|bus| bus.asic_count).collect(),
            discovered_topology.as_ref(),
            self.board_serials.clone(),
        );

        let mut refused_profile = false;
        if calibration.apply_saved_operating_point
            && !calibration.force_retune
            && let Some(profile) = loaded_profile
                .as_ref()
                .and_then(|loaded| loaded.persisted.as_ref())
        {
            let mut reasons = profile.refusal_reasons(&Bzm2ProfileValidationContext {
                calibration,
                bus_layouts,
                identity: &identity,
                ambient_c,
            });
            // SAY HOW MUCH OF THE IDENTITY CHECK ACTUALLY RAN. A slot whose
            // serial is unrecorded on either side is neither a match nor a
            // mismatch, and silence here would let "no reasons to refuse" be
            // read as "confirmed to be the right board". Reported before the
            // decision, so it is visible whether the profile is accepted or
            // not.
            let unverifiable = profile.identity.unverifiable_slots(&identity);
            if !unverifiable.is_empty() {
                warn!(
                    board = %self.config.device_id(),
                    slots = ?unverifiable,
                    "Board identity UNVERIFIED for these slots: no serial recorded on one \
                     side or the other, so this calibration cannot be shown to belong to \
                     the board in the slot -- nor shown not to. A profile written before \
                     serials were recorded is in this state."
                );
            }
            let die_temp_c = snapshot_temperature(&telemetry, "asic").or(ambient_c);
            match resolve_replay_point(profile, die_temp_c) {
                Ok(point) if reasons.is_empty() => {
                    self.apply_saved_operating_point(
                        bus_layouts,
                        &point,
                        profile.saved_operating_point_status,
                        &profile.saved_operating_point_reasons,
                    )
                    .await?;
                    info!(
                        board = %self.config.device_id(),
                        asic_count = point.per_asic_pll_mhz.len(),
                        die_temp_c,
                        rows = profile.operating_points.rows().len(),
                        "BZM2 replayed saved operating point profile"
                    );
                    return Ok(());
                }
                Ok(_) => {}
                Err(reason) => reasons.push(reason),
            }
            warn!(
                board = %self.config.device_id(),
                reasons = ?reasons,
                "BZM2 refused saved operating point profile; recalibrating live"
            );
            // A REFUSED PROFILE IS REFUSED FOR EVERY PURPOSE.
            //
            // Execution used to fall through here and read `loaded_profile`
            // again a few lines below, seeding the live calibration's engine
            // topology and per-ASIC throughput from the profile it had just
            // rejected. Not as an operating point -- that path returns above --
            // but the refusal reasons include checksum mismatch and IDENTITY
            // mismatch, and if this data belongs to a different board then its
            // topology and its throughput belong to that board too. Refusing
            // the voltages while keeping the shape they were measured at is
            // half a refusal.
            //
            // Recalibrating live means live: from what this board reports now,
            // seeded by nothing.
            refused_profile = true;
        }

        let site_temp_c = ambient_c.unwrap_or(DEFAULT_CALIBRATION_SITE_TEMP_C);
        let saved_operating_point =
            seed_for_live_calibration(refused_profile, loaded_profile.as_ref());
        let engine_topology = self
            .resolve_engine_topology_for_calibration(
                bus_layouts,
                saved_operating_point.as_ref(),
                discovered_topology,
            )
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

        let mut per_asic_pll_mhz = plan
            .asic_plans
            .iter()
            .map(|plan| (plan.asic_id, plan.pll_frequencies_mhz))
            .collect::<BTreeMap<_, _>>();

        // THE TUNING THE UNIT ALREADY HOLDS, when it is asked for.
        //
        // The planner's per-ASIC branches are gated on pass rates this path
        // hard-codes to None, so every ASIC receives the same frequency and the
        // board runs at a rate its weakest device can hold. On one measured
        // board that costs 5.95% of the mean frequency -- the devices there
        // spread 1093.75 to 1275 MHz around a mean of 1162.94, and a single
        // uniform value has to clear the floor.
        //
        // The stored file holds 200 per-PLL values a board, and the apply path
        // below has always been able to take them: what was missing was the
        // wire between the two. This is that wire, and NOTHING ELSE -- it does
        // not search, it does not measure, it reuses a measurement somebody
        // else already paid for.
        //
        // OFF BY DEFAULT. This writes frequencies to silicon, which is the
        // highest-consequence thing this driver does, and the map's device
        // ORDER is confirmed against the wire only for the identity case: the
        // file's own documentation says to treat a chain programmed from it as
        // tuned-but-unconfirmed until a per-device read-back agrees. So it is
        // opt-in, it logs which source won, and it refuses out-of-context
        // tuning rather than silently applying it.
        if let Some(stored) = self.config.stored_calibration.as_ref() {
            let ctx_ok = stored.context_matches(
                site_temp_c,
                plan.desired_voltage_mv,
                super::config::STORED_CALIBRATION_AMBIENT_TOLERANCE_C,
            );
            if ctx_ok {
                let from_file =
                    stored.per_asic_pll_mhz(u16::from(self.config.enumeration.start_id));
                let n = from_file.len();
                per_asic_pll_mhz.extend(from_file);
                info!(
                    board = %self.config.device_id(),
                    devices = n,
                    mean_mhz = stored.mean_mhz(),
                    min_mhz = stored.min_mhz(),
                    max_mhz = stored.max_mhz(),
                    "BZM2 applying the per-device tuning this unit already held, \
                     in place of a uniform frequency (tuned-but-unconfirmed: the \
                     device order is not read back)"
                );
            } else {
                warn!(
                    board = %self.config.device_id(),
                    file_ambient_c = stored.ambient_c,
                    now_ambient_c = site_temp_c,
                    file_rail_mv = stored.rail_mv,
                    now_rail_mv = plan.desired_voltage_mv,
                    "BZM2 stored tuning is out of context and was NOT applied: \
                     tuning is only valid near the conditions it was derived \
                     under, and using it elsewhere measures the context"
                );
            }
        }

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
            // Carry forward whatever the board has already learned at other
            // temperatures and add a row for this one. A fresh calibration is
            // new knowledge about one temperature, not a reason to forget the
            // rest of the curve.
            let previous = loaded_profile
                .as_ref()
                .and_then(|loaded| loaded.persisted.as_ref());
            let mut operating_points = previous
                .map(|previous| previous.operating_points.clone())
                .unwrap_or_default();
            // Thermal resistance describes the heatsink, not the operating
            // point, so recalibrating must not discard it. It is expensive to
            // obtain — a deliberate power step and minutes of settling — and
            // nothing about moving voltage invalidates it.
            let carried_thermal = previous.and_then(|previous| previous.thermal);
            if let Some(die_temp_c) = shared_temp.or(ambient_c) {
                operating_points.observe(Bzm2OperatingPointRow::observed(
                    die_temp_c,
                    &current_saved_operating_point,
                    // Nothing has been measured yet at this brand-new point;
                    // the monitor fills these in once the board has run here.
                    None,
                    None,
                ));
            }
            let profile = Bzm2PersistedCalibrationProfile {
                schema_version: Bzm2PersistedCalibrationProfile::SCHEMA_VERSION,
                operating_class: operating_class_name(calibration.operating_class).into(),
                performance_mode: performance_mode_name(calibration.performance_mode).into(),
                asics_per_bus: bus_layouts.iter().map(|bus| bus.asic_count).collect(),
                pll_post1_divider: calibration.pll_post1_divider,
                identity,
                written_at_epoch_s: now_epoch_s(),
                written_at_ambient_c: ambient_c,
                checksum: None,
                saved_operating_point_status: Bzm2SavedOperatingPointStatus::Pending,
                saved_operating_point_reasons: Vec::new(),
                operating_points,
                thermal: carried_thermal,
                saved_state: current_saved_operating_point,
            };
            store_calibration_profile(profile_path, &profile)
                .map_err(BoardError::InitializationFailed)?;
        }

        info!(board = %self.config.device_id(), reuse_saved_operating_point = plan.reuse_saved_operating_point, needs_retune = plan.needs_retune, initial_frequency_mhz = plan.initial_frequency_mhz, asic_count = plan.asic_plans.len(), "BZM2 live calibration completed");
        Ok(())
    }

    /// Merge the saved topology, what discovery actually found, and defaults.
    ///
    /// `discovered` is passed in rather than probed here: it is needed earlier,
    /// to fingerprint the board before deciding whether a stored profile still
    /// applies, and probing twice would double the enumeration cost.
    async fn resolve_engine_topology_for_calibration(
        &self,
        bus_layouts: &[Bzm2BusLayout],
        saved_operating_point: Option<&Bzm2SavedOperatingPoint>,
        discovered: Option<BTreeMap<u16, Bzm2SavedEngineTopology>>,
    ) -> BTreeMap<u16, Bzm2SavedEngineTopology> {
        let mut topology = saved_operating_point
            .map(|saved| saved.per_asic_engine_topology.clone())
            .unwrap_or_default();

        for (asic_id, entry) in discovered.unwrap_or_default() {
            topology.insert(asic_id, entry);
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
            let stream = match crate::transport::serial::open_with_platform_cflag(
                &bus.serial_path,
                self.config.baud_rate,
            ) {
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
            let (reader, writer, _control) = super::config::split_chain_port(stream);
            let mut uart = Bzm2UartController::new(reader, writer);
            let operating =
                Bzm2TdmControl::operating(&bus.wire_asic_ids(self.config.enumeration.start_id));

            for local_asic in 0..bus.asic_count {
                let global_asic = bus.asic_start + local_asic;
                match uart
                    .discover_engine_map(
                        local_asic as u8,
                        self.config.calibration.engine_discovery_tdm_prediv_raw,
                        self.config.calibration.engine_discovery_tdm_counter,
                        operating,
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

/// Update a stored profile's lifecycle status in place.
///
/// The existing file is read first and only the status, reasons and operating
/// point are replaced. Rebuilding the profile from the config instead would
/// silently drop the identity, ambient and timestamp recorded when it was
/// calibrated — and a profile that loses its binding every time the monitor
/// marks it Validated is a profile with no binding at all.
pub(super) fn store_saved_operating_point_status(
    path: &Path,
    calibration: &Bzm2CalibrationConfig,
    bus_layouts: &[Bzm2BusLayout],
    saved_state: &Bzm2SavedOperatingPoint,
    status: Bzm2SavedOperatingPointStatus,
    reasons: &[String],
) -> Result<(), String> {
    let existing = load_saved_operating_point_profile(Some(path))
        .ok()
        .flatten()
        .and_then(|loaded| loaded.persisted);

    let profile = match existing {
        Some(mut profile) => {
            profile.saved_operating_point_status = status;
            profile.saved_operating_point_reasons = reasons.to_vec();
            profile.saved_state = saved_state.clone();
            profile
        }
        None => Bzm2PersistedCalibrationProfile {
            schema_version: Bzm2PersistedCalibrationProfile::SCHEMA_VERSION,
            operating_class: operating_class_name(calibration.operating_class).into(),
            performance_mode: performance_mode_name(calibration.performance_mode).into(),
            asics_per_bus: bus_layouts.iter().map(|bus| bus.asic_count).collect(),
            pll_post1_divider: calibration.pll_post1_divider,
            identity: Bzm2ProfileIdentity::default(),
            written_at_epoch_s: now_epoch_s(),
            written_at_ambient_c: None,
            checksum: None,
            saved_operating_point_status: status,
            saved_operating_point_reasons: reasons.to_vec(),
            operating_points: Bzm2OperatingPointTable::default(),
            thermal: None,
            saved_state: saved_state.clone(),
        },
    };
    store_calibration_profile(path, &profile)
}

fn estimate_planned_hashrate(
    plan: &crate::tuning::calibration_planner::Bzm2CalibrationPlan,
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

/// What may seed a live calibration, given whether the profile was refused.
///
/// One home for the rule, and it is a rule worth naming: **a profile refused
/// for its voltages is refused for its shape too.** The refusal reasons
/// include checksum mismatch and identity mismatch, and data that belongs to a
/// different board carries that board's engine topology and throughput as
/// surely as it carries its voltages. Recalibrating live means seeded by
/// nothing.
fn seed_for_live_calibration(
    refused: bool,
    profile: Option<&Bzm2LoadedCalibrationProfile>,
) -> Option<Bzm2SavedOperatingPoint> {
    if refused {
        return None;
    }
    profile.and_then(saved_operating_point_from_loaded_profile)
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

/// The operating point to replay at `die_temp_c`, or why there is not one.
///
/// Three cases, and the third is the one worth being deliberate about:
///
/// - The table has a row for this temperature: replay it. This is the warm
///   restart the table exists for — the search has already been run here.
/// - The table is empty: replay the flat stored point. Nothing has been indexed
///   yet, and the ambient gate has already established the room has not moved,
///   so the stored point is the best available.
/// - The table has rows but none within reach of this temperature: **refuse**.
///   A populated table that cannot answer is saying this temperature has never
///   been characterised, and replaying a point learned twenty degrees away is
///   precisely the failure indexing by temperature was introduced to stop.
fn resolve_replay_point(
    profile: &Bzm2PersistedCalibrationProfile,
    die_temp_c: Option<f32>,
) -> Result<Bzm2SavedOperatingPoint, String> {
    if profile.operating_points.is_empty() {
        return Ok(profile.saved_state.clone());
    }
    let Some(die_temp_c) = die_temp_c else {
        return Err("die temperature unknown, cannot select an operating point".into());
    };
    profile
        .operating_points
        .lookup(die_temp_c)
        .map(|row| row.to_saved_operating_point(&profile.saved_state))
        .ok_or_else(|| {
            format!(
                "no operating point learned near {die_temp_c:.1}C \
                 (table covers {})",
                describe_coverage(&profile.operating_points)
            )
        })
}

fn describe_coverage(table: &Bzm2OperatingPointTable) -> String {
    match (table.rows().first(), table.rows().last()) {
        (Some(first), Some(last)) => {
            format!("{:.1}C to {:.1}C", first.die_temp_c, last.die_temp_c)
        }
        _ => "nothing".into(),
    }
}

/// Seconds since the Unix epoch, or `None` if the clock is before it.
fn now_epoch_s() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|since| since.as_secs())
}

fn store_calibration_profile(
    path: &Path,
    profile: &Bzm2PersistedCalibrationProfile,
) -> Result<(), String> {
    // Sealing here rather than at each construction site means no caller can
    // write an unchecksummed profile by forgetting to.
    let mut profile = profile.clone();
    profile.seal()?;
    let profile = &profile;

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

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A PROFILE REFUSED FOR ITS VOLTAGES IS REFUSED FOR ITS SHAPE TOO.
    ///
    /// The refusal branch logged "recalibrating live" and then fell through to
    /// read the SAME profile again, seeding the live calibration's engine
    /// topology and per-ASIC throughput from data it had just rejected. The
    /// refusal reasons include checksum mismatch and IDENTITY mismatch — and a
    /// profile that belongs to a different board carries that board's topology
    /// and throughput as surely as it carries its voltages.
    ///
    /// Passing `None` here is not the interesting half. The interesting half
    /// is that a profile which WOULD have seeded must stop seeding once it is
    /// refused, which is what the first assertion pins.
    #[test]
    fn a_refused_profile_seeds_nothing() {
        let profile = Bzm2LoadedCalibrationProfile {
            // No persisted record, so `saved_operating_point_from_loaded_profile`
            // takes its `_` arm and returns the saved state -- i.e. this
            // profile WOULD seed. That is what makes the refusal assertion
            // below meaningful rather than vacuous.
            persisted: None,
            saved_state: Bzm2SavedOperatingPoint {
                board_voltage_mv: 17_500,
                board_throughput_ths: 35.0,
                per_domain_voltage_mv: BTreeMap::new(),
                per_asic_engine_topology: BTreeMap::new(),
                per_asic_pll_mhz: BTreeMap::new(),
            },
        };
        assert!(
            seed_for_live_calibration(false, Some(&profile)).is_some(),
            "an accepted profile seeds the live calibration; if this is None the \
             second assertion proves nothing"
        );
        assert!(
            seed_for_live_calibration(true, Some(&profile)).is_none(),
            "a refused profile must seed nothing -- not its topology, not its throughput"
        );
    }

    #[test]
    fn no_profile_seeds_nothing_either_way() {
        assert!(seed_for_live_calibration(false, None).is_none());
        assert!(seed_for_live_calibration(true, None).is_none());
    }

    /// The bug this exists to prevent: a second bus whose global ids and wire
    /// ids differ. Every other test in this suite uses one bus starting at
    /// zero, which is exactly the configuration where the two conventions
    /// coincide and the defect cannot appear.
    #[test]
    fn wire_ids_are_local_to_the_bus_not_global_to_the_machine() {
        let paths = vec!["/dev/ttyA".to_string(), "/dev/ttyB".to_string()];
        let layouts = build_bus_layouts(&paths, &[100, 100]);
        assert_eq!(layouts.len(), 2);

        // Global numbering is machine-wide and continues across buses.
        assert_eq!(layouts[0].asic_start, 0);
        assert_eq!(layouts[1].asic_start, 100);

        // Wire numbering restarts on every bus, because every bus is its own
        // chain. The second bus reports 0..99, not 100..199.
        let wire0 = layouts[0].wire_asic_ids(0);
        let wire1 = layouts[1].wire_asic_ids(0);
        assert_eq!(wire0.first(), Some(&0));
        assert_eq!(wire1.first(), Some(&0));
        assert_eq!(wire1.last(), Some(&99));
        assert_eq!(wire0, wire1, "both chains address from the same start");

        // And the thing that actually broke: a frame from the second bus
        // carries a wire id that is NOT in that bus's global range.
        let frame_from_second_bus: u8 = 7;
        assert!(
            wire1.contains(&frame_from_second_bus),
            "a real device on bus 1 must be recognised by its wire id"
        );
        assert!(
            !layouts[1].contains(u16::from(frame_from_second_bus)),
            "and must NOT be found in the global range -- this mismatch is the \
             defect, so if this assertion ever fails the conventions have been \
             unified and this test should be rewritten rather than deleted"
        );
    }

    /// A non-zero enumeration start shifts the wire ids and nothing else.
    #[test]
    fn wire_ids_follow_the_enumeration_start_id() {
        let paths = vec!["/dev/ttyA".to_string(), "/dev/ttyB".to_string()];
        let layouts = build_bus_layouts(&paths, &[4, 4]);
        assert_eq!(layouts[1].wire_asic_ids(0), vec![0, 1, 2, 3]);
        assert_eq!(layouts[1].wire_asic_ids(16), vec![16, 17, 18, 19]);
        // Still nothing to do with where the bus sits globally.
        assert_eq!(layouts[1].asic_start, 4);
    }

    use super::super::bringup::Bzm2BringupConfig;
    use super::super::config::{
        Bzm2EnumerationConfig, Bzm2RuntimeConfig, DEFAULT_BAUD_RATE,
        DEFAULT_CALIBRATION_POST1_DIVIDER, TEST_NOMINAL_HASHRATE_THS,
    };
    use super::super::telemetry::Bzm2TelemetryConfig;
    use super::super::test_support::spawn_chain_emulator;

    use crate::api_client::types::BoardTelemetry;
    use crate::asic::bzm2::protocol::{OPCODE_UART_NOOP, encode_noop};
    use crate::tuning::calibration_planner::{Bzm2OperatingClass, Bzm2PerformanceMode};
    use crate::types::Temperature;
    use nix::pty::openpty;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use tokio::sync::{mpsc, watch};

    /// A profile freshly written for this board, in this room, by this build.
    fn valid_profile() -> Bzm2PersistedCalibrationProfile {
        let mut profile = Bzm2PersistedCalibrationProfile {
            schema_version: Bzm2PersistedCalibrationProfile::SCHEMA_VERSION,
            operating_class: operating_class_name(Bzm2OperatingClass::Generic).into(),
            performance_mode: performance_mode_name(Bzm2PerformanceMode::Standard).into(),
            asics_per_bus: vec![2],
            pll_post1_divider: DEFAULT_CALIBRATION_POST1_DIVIDER,
            identity: Bzm2ProfileIdentity::new("bzm2-ttyUSB0", vec![2], None, Vec::new()),
            written_at_epoch_s: now_epoch_s(),
            written_at_ambient_c: Some(21.0),
            checksum: None,
            saved_operating_point_status: Bzm2SavedOperatingPointStatus::Validated,
            saved_operating_point_reasons: Vec::new(),
            operating_points: Bzm2OperatingPointTable::default(),
            thermal: None,
            saved_state: Bzm2SavedOperatingPoint {
                board_voltage_mv: 17_500,
                board_throughput_ths: 80.0,
                per_domain_voltage_mv: BTreeMap::from([(0, 17_450)]),
                per_asic_engine_topology: BTreeMap::new(),
                per_asic_pll_mhz: BTreeMap::from([
                    (0, [1_100.0, 1_125.0]),
                    (1, [1_150.0, 1_175.0]),
                ]),
            },
        };
        profile.seal().unwrap();
        profile
    }

    fn validation_bus_layouts() -> Vec<Bzm2BusLayout> {
        vec![Bzm2BusLayout {
            serial_path: "/dev/ttyUSB0".into(),
            asic_start: 0,
            asic_count: 2,
        }]
    }

    fn refusals(
        profile: &Bzm2PersistedCalibrationProfile,
        identity: &Bzm2ProfileIdentity,
        ambient_c: Option<f32>,
    ) -> Vec<String> {
        let calibration = Bzm2CalibrationConfig {
            asics_per_bus: vec![2],
            ..Default::default()
        };
        profile.refusal_reasons(&Bzm2ProfileValidationContext {
            calibration: &calibration,
            bus_layouts: &validation_bus_layouts(),
            identity,
            ambient_c,
        })
    }

    #[test]
    fn an_unchanged_board_in_an_unchanged_room_replays() {
        let profile = valid_profile();
        let identity = profile.identity.clone();
        assert!(
            refusals(&profile, &identity, Some(21.0)).is_empty(),
            "{:?}",
            refusals(&profile, &identity, Some(21.0))
        );
        // And a small ambient drift is still fine.
        assert!(refusals(&profile, &identity, Some(24.0)).is_empty());
    }

    #[test]
    fn a_board_swap_is_refused() {
        let mut profile = valid_profile();
        profile.identity.engine_fingerprint = Some("a".repeat(64));
        profile.seal().unwrap();

        let mut identity = profile.identity.clone();
        identity.engine_fingerprint = Some("b".repeat(64));

        let reasons = refusals(&profile, &identity, Some(21.0));
        assert!(
            reasons.iter().any(|reason| reason.contains("swapped")),
            "{reasons:?}"
        );
    }

    #[test]
    fn a_firmware_change_is_refused() {
        let mut profile = valid_profile();
        profile.identity.firmware_version = "0.0.1-ancient".into();
        profile.seal().unwrap();
        let identity = Bzm2ProfileIdentity::new("bzm2-ttyUSB0", vec![2], None, Vec::new());

        let reasons = refusals(&profile, &identity, Some(21.0));
        assert!(
            reasons.iter().any(|reason| reason.contains("firmware")),
            "{reasons:?}"
        );
    }

    #[test]
    fn a_stale_ambient_is_refused() {
        let profile = valid_profile();
        let identity = profile.identity.clone();

        // Written at 21 C, loaded in a room ten degrees warmer.
        let reasons = refusals(&profile, &identity, Some(31.0));
        assert!(
            reasons
                .iter()
                .any(|reason| reason.contains("ambient moved")),
            "{reasons:?}"
        );

        // And an unmeasurable room is refused rather than assumed unchanged.
        assert!(!refusals(&profile, &identity, None).is_empty());
    }

    #[test]
    fn an_edited_profile_is_refused_before_anything_else_is_checked() {
        let mut profile = valid_profile();
        // Someone hand-edits the voltage upward without recomputing the digest.
        profile.saved_state.board_voltage_mv = 19_000;
        let identity = profile.identity.clone();

        let reasons = refusals(&profile, &identity, Some(21.0));
        assert_eq!(
            reasons.len(),
            1,
            "a failed checksum makes every other field untrustworthy, so it \
             should stop the walk: {reasons:?}"
        );
        assert!(reasons[0].contains("checksum"), "{reasons:?}");
    }

    #[test]
    fn a_previous_schema_is_discarded_rather_than_reinterpreted() {
        let mut profile = valid_profile();
        profile.schema_version = 1;
        profile.seal().unwrap();
        let identity = profile.identity.clone();

        let reasons = refusals(&profile, &identity, Some(21.0));
        assert_eq!(reasons.len(), 1, "{reasons:?}");
        assert!(reasons[0].contains("schema"), "{reasons:?}");
    }

    #[test]
    fn an_empty_table_replays_the_flat_stored_point() {
        // First run after the upgrade that introduced the table: nothing is
        // indexed yet, and the ambient gate has already established the room
        // has not moved.
        let profile = valid_profile();
        let point = resolve_replay_point(&profile, Some(55.0)).unwrap();
        assert_eq!(point.board_voltage_mv, 17_500);
    }

    #[test]
    fn a_warm_restart_takes_the_row_for_its_die_temperature() {
        let mut profile = valid_profile();
        let cold = Bzm2SavedOperatingPoint {
            board_voltage_mv: 17_400,
            ..profile.saved_state.clone()
        };
        let hot = Bzm2SavedOperatingPoint {
            board_voltage_mv: 17_440,
            ..profile.saved_state.clone()
        };
        profile
            .operating_points
            .observe(Bzm2OperatingPointRow::observed(40.0, &cold, None, None));
        profile
            .operating_points
            .observe(Bzm2OperatingPointRow::observed(80.0, &hot, None, None));

        // Restarting onto dies still at 80 C picks the point learned there,
        // not the one learned on a cold bench.
        let point = resolve_replay_point(&profile, Some(80.0)).unwrap();
        assert_eq!(point.board_voltage_mv, 17_440);

        // And a cold start picks the cold row from the same profile.
        let point = resolve_replay_point(&profile, Some(40.0)).unwrap();
        assert_eq!(point.board_voltage_mv, 17_400);
    }

    #[test]
    fn a_populated_table_refuses_a_temperature_it_has_never_seen() {
        let mut profile = valid_profile();
        let saved_state = profile.saved_state.clone();
        profile
            .operating_points
            .observe(Bzm2OperatingPointRow::observed(
                60.0,
                &saved_state,
                None,
                None,
            ));

        // Twenty degrees below anything learned. Replaying the 60 C point here
        // is exactly what indexing by temperature was introduced to stop, so
        // this must fall through to a live calibration rather than guess.
        let reason = resolve_replay_point(&profile, Some(40.0)).unwrap_err();
        assert!(reason.contains("no operating point learned"), "{reason}");
        assert!(reason.contains("60.0C"), "{reason}");

        // An unreadable die temperature is likewise a refusal, not a default.
        assert!(resolve_replay_point(&profile, None).is_err());
    }

    #[test]
    fn a_measured_row_supersedes_the_planned_throughput_baseline() {
        let mut profile = valid_profile();
        let saved_state = profile.saved_state.clone();
        profile
            .operating_points
            .observe(Bzm2OperatingPointRow::observed(
                60.0,
                &saved_state,
                Some(74.5),
                Some(21.0),
            ));

        // The flat profile carries a modelled throughput. Once a figure has
        // actually been observed at this temperature, validation should be
        // comparing against that instead of against a number never seen.
        assert_eq!(profile.saved_state.board_throughput_ths, 80.0);
        let point = resolve_replay_point(&profile, Some(60.0)).unwrap();
        assert!((point.board_throughput_ths - 74.5).abs() < 0.01);
    }

    fn characterisation() -> ThermalCharacterisation {
        ThermalCharacterisation {
            theta: crate::tuning::thermal::ThermalResistance::from_step(
                Temperature::from_celsius(18.0),
                40.0,
                Duration::from_secs(300),
            )
            .unwrap(),
            measured_at_ambient_c: 21.0,
            step_power_w: 40.0,
            held_for_secs: 300,
            measured_at_epoch_s: Some(1_700_000_000),
        }
    }

    #[test]
    fn a_characterised_heatsink_survives_a_status_update() {
        // theta costs a power step and minutes of settling. Losing it every
        // time the monitor marks a profile Validated would mean re-measuring
        // the heatsink to record an unrelated fact about the voltage.
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("bzm2-thermal-{unique}.json"));

        let mut profile = valid_profile();
        profile.thermal = Some(characterisation());
        store_calibration_profile(&path, &profile).unwrap();

        store_saved_operating_point_status(
            &path,
            &Bzm2CalibrationConfig {
                asics_per_bus: vec![2],
                ..Default::default()
            },
            &validation_bus_layouts(),
            &profile.saved_state,
            Bzm2SavedOperatingPointStatus::Validated,
            &[],
        )
        .unwrap();

        let reloaded = load_saved_operating_point_profile(Some(&path))
            .unwrap()
            .unwrap()
            .persisted
            .unwrap();
        let thermal = reloaded.thermal.expect("theta must survive the rewrite");
        assert_eq!(thermal, characterisation());
        assert!(
            (thermal.theta.degrees_c_per_watt() - 0.45).abs() < 1e-6,
            "18 C of rise on a 40 W step is 0.45 C/W"
        );
        assert!(thermal.theta.is_settled());

        let _ = fs::remove_file(path);
    }

    #[test]
    fn an_edited_theta_breaks_the_checksum() {
        // theta gates every voltage grant through the runaway precondition, so
        // a hand-edited one is a way to talk the miner into an unsafe point.
        // It has to be inside the digest, not merely stored beside it.
        let mut profile = valid_profile();
        profile.thermal = Some(characterisation());
        profile.seal().unwrap();
        assert!(profile.checksum_mismatch().is_none());

        let mut tampered = profile.clone();
        tampered.thermal = Some(ThermalCharacterisation {
            theta: crate::tuning::thermal::ThermalResistance::from_degrees_c_per_watt(0.05)
                .unwrap(),
            ..characterisation()
        });
        assert!(
            tampered.checksum_mismatch().is_some(),
            "a doctored heatsink figure must not pass validation"
        );
    }

    #[test]
    fn status_updates_preserve_the_binding() {
        // The monitor marks a profile Validated long after calibration wrote
        // it. If that rewrite rebuilt the profile from config, the identity and
        // the ambient it was learned at would be lost, and the next restart
        // would have nothing to validate against.
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("bzm2-binding-{unique}.json"));
        store_calibration_profile(&path, &valid_profile()).unwrap();

        let calibration = Bzm2CalibrationConfig {
            asics_per_bus: vec![2],
            ..Default::default()
        };
        let saved_state = valid_profile().saved_state.clone();
        store_saved_operating_point_status(
            &path,
            &calibration,
            &validation_bus_layouts(),
            &saved_state,
            Bzm2SavedOperatingPointStatus::Validated,
            &[],
        )
        .unwrap();

        let reloaded = load_saved_operating_point_profile(Some(&path))
            .unwrap()
            .unwrap()
            .persisted
            .unwrap();
        assert_eq!(reloaded.identity, valid_profile().identity);
        assert_eq!(reloaded.written_at_ambient_c, Some(21.0));
        assert!(
            reloaded.checksum_mismatch().is_none(),
            "the rewrite must reseal, not carry the stale digest"
        );

        let _ = fs::remove_file(path);
    }

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
            // Opt-in, so a test that does not ask for stored tuning does not get it.
            stored_calibration: None,
            heartbeat: Default::default(),
            serial_paths: vec![serial_path],
            baud_rate: DEFAULT_BAUD_RATE,
            timestamp_count: crate::asic::bzm2::protocol::DEFAULT_TIMESTAMP_COUNT,
            nonce_gap: crate::asic::bzm2::protocol::DEFAULT_NONCE_GAP,
            result_min_difficulty: None,
            dispatch_interval: Duration::from_millis(50),
            nominal_hashrate_ths: TEST_NOMINAL_HASHRATE_THS,
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
            nominal_hashrate_ths: TEST_NOMINAL_HASHRATE_THS,
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
                // The ambient gate refuses a profile it cannot compare against,
                // so the replay path needs a reading on both sides.
                site_temp_c: Some(22.0),
                ..Default::default()
            },
        };

        let persisted = Bzm2PersistedCalibrationProfile {
            schema_version: Bzm2PersistedCalibrationProfile::SCHEMA_VERSION,
            operating_class: operating_class_name(Bzm2OperatingClass::Generic).into(),
            performance_mode: performance_mode_name(Bzm2PerformanceMode::Standard).into(),
            asics_per_bus: vec![2],
            pll_post1_divider: DEFAULT_CALIBRATION_POST1_DIVIDER,
            identity: Bzm2ProfileIdentity::new(&config.device_id(), vec![2], None, Vec::new()),
            written_at_epoch_s: now_epoch_s(),
            written_at_ambient_c: Some(22.0),
            checksum: None,
            saved_operating_point_status: Bzm2SavedOperatingPointStatus::Validated,
            saved_operating_point_reasons: Vec::new(),
            operating_points: Bzm2OperatingPointTable::default(),
            thermal: None,
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
        // Written through the same path production uses, so the checksum is
        // sealed the same way rather than by the test reimplementing it.
        store_calibration_profile(&profile_path, &persisted).unwrap();
        let original = fs::read_to_string(&profile_path).unwrap();

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
            // Opt-in, so a test that does not ask for stored tuning does not get it.
            stored_calibration: None,
            heartbeat: Default::default(),
            serial_paths: vec![serial_path],
            baud_rate: DEFAULT_BAUD_RATE,
            timestamp_count: crate::asic::bzm2::protocol::DEFAULT_TIMESTAMP_COUNT,
            nonce_gap: crate::asic::bzm2::protocol::DEFAULT_NONCE_GAP,
            result_min_difficulty: None,
            dispatch_interval: Duration::from_millis(50),
            nominal_hashrate_ths: TEST_NOMINAL_HASHRATE_THS,
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
            // Opt-in, so a test that does not ask for stored tuning does not get it.
            stored_calibration: None,
            heartbeat: Default::default(),
            serial_paths: vec![serial_path],
            baud_rate: DEFAULT_BAUD_RATE,
            timestamp_count: crate::asic::bzm2::protocol::DEFAULT_TIMESTAMP_COUNT,
            nonce_gap: crate::asic::bzm2::protocol::DEFAULT_NONCE_GAP,
            result_min_difficulty: None,
            dispatch_interval: Duration::from_millis(50),
            nominal_hashrate_ths: TEST_NOMINAL_HASHRATE_THS,
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
