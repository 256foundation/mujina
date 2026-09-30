//! Chain enumeration, calibration planner I/O, and operating-point persistence for the BZM2 board.

use std::collections::BTreeMap;

use crate::asic::bzm2::{Bzm2DiscoveredEngineMap, Bzm2TdmControl, Bzm2UartController};
use crate::tracing::prelude::*;
use crate::tuning::calibration_planner::{
    Bzm2AsicMeasurement, Bzm2AsicTopology, Bzm2BoardCalibrationInput, Bzm2CalibrationConstraints,
    Bzm2CalibrationPlanner, Bzm2DomainMeasurement, Bzm2SavedEngineCoordinate,
    Bzm2SavedEngineTopology, Bzm2VoltageDomain,
};

use super::config::{DEFAULT_CALIBRATION_SITE_TEMP_C, DEFAULT_ENUMERATION_MAX_ASICS_PER_BUS};
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

        let site_temp_c = ambient_c.unwrap_or(DEFAULT_CALIBRATION_SITE_TEMP_C);
        let engine_topology = self
            .resolve_engine_topology_for_calibration(bus_layouts, discovered_topology)
            .await;
        let (voltage_domains, domain_lookup) = build_voltage_domains(
            total_asics as u16,
            &calibration.asics_per_domain,
            &calibration.domain_voltage_offsets_mv,
        );
        let asics = build_topology(bus_layouts, &domain_lookup, &engine_topology);
        let shared_temp = snapshot_temperature(&telemetry, "asic")
            .or_else(|| snapshot_temperature(&telemetry, "board"));
        let asic_measurements = asics
            .iter()
            .map(|asic| Bzm2AsicMeasurement {
                asic_id: asic.asic_id,
                temperature_c: shared_temp,
                average_pass_rate: None,
                pll_pass_rates: [None, None],
                throughput_ths: None,
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
            domain_measurements,
            asic_measurements,
            constraints: Bzm2CalibrationConstraints::default(),
            force_retune: calibration.force_retune,
            saved_operating_point: None,
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
        discovered: Option<BTreeMap<u16, Bzm2SavedEngineTopology>>,
    ) -> BTreeMap<u16, Bzm2SavedEngineTopology> {
        let mut topology = BTreeMap::new();

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
        Bzm2CalibrationConfig, Bzm2EnumerationConfig, Bzm2RuntimeConfig, DEFAULT_BAUD_RATE,
        DEFAULT_NOMINAL_HASHRATE_THS,
    };
    use super::super::telemetry::Bzm2TelemetryConfig;
    use super::super::test_support::spawn_chain_emulator;

    use crate::api_client::types::BoardTelemetry;
    use crate::asic::bzm2::protocol::{OPCODE_UART_NOOP, encode_noop};

    use nix::pty::openpty;
    use std::fs;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::time::Duration;
    use tokio::sync::watch;

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
        let board = Bzm2Board::new(config, telemetry_tx);

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
        let board = Bzm2Board::new(config, telemetry_tx);

        let layouts = board.resolve_bus_layouts().await.unwrap();
        assert_eq!(layouts.len(), 1);
        assert_eq!(layouts[0].asic_count, 3);

        emulator.join().unwrap();
    }
}
