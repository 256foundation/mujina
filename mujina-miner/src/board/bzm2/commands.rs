//! BoardCommand dispatch loop for the BZM2 board.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::api::commands::BoardCommand;
use crate::api_client::types::{
    AsicFaultBits, AsicState, BoardTelemetry, Bzm2AsicAggregate, Bzm2AsicCoverage, Bzm2AsicExtreme,
    Bzm2AsicSummaryResponse, Bzm2BusAsicSummary, Bzm2BusSummary, Bzm2ChainSummaryResponse,
    Bzm2DtsVsGeneration, Bzm2FaultBit, Bzm2FaultBitCount, Bzm2FaultSummary,
    Bzm2MeasurementAvailability, Bzm2MeasurementNote, Bzm2ReadingAge, Bzm2ReadingStats,
    Bzm2VoltageChannelStats,
};
use crate::asic::bzm2::protocol::DtsVsGeneration;
use crate::asic::bzm2::thread::telemetry::sensor_prefix;
use crate::tracing::prelude::*;
use crate::types::Temperature;

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
        // The chain is addressed from this id on every bus, and the sensor
        // names the per-ASIC summary reads back are spelled with those wire
        // ids -- not with the board-wide ones.
        let enumeration_start_id = self.config.enumeration.start_id;
        let dts_vs_generation = self.config.dts_vs_generation;
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
                            BoardCommand::QueryBzm2AsicSummary { reply } => {
                                // Served entirely from state this process
                                // already holds: the telemetry watch value
                                // and the resolved bus layout. Nothing here
                                // touches a thread handle or a serial port,
                                // and that is the requirement rather than an
                                // optimisation. This channel is mpsc(16),
                                // served one command at a time, and only the
                                // reply is under a timeout -- the send is
                                // not. A summary that cost a chain
                                // round-trip would queue behind every
                                // diagnostic on the board and hang its
                                // caller within seconds of being polled.
                                let bus_layouts =
                                    bus_layouts.lock().unwrap_or_else(|e| e.into_inner()).clone();
                                let state = telemetry_tx.borrow().clone();
                                let summary = build_asic_summary(
                                    &state,
                                    &bus_layouts,
                                    &configured_asics_per_bus,
                                    enumeration_enabled,
                                    enumeration_start_id,
                                    dts_vs_generation,
                                    ASIC_READING_MAX_AGE,
                                );
                                debug!(
                                    board = %board_name,
                                    configured_asics = summary.configured_asics,
                                    discovered_asics = ?summary.discovered_asics,
                                    asics_in_layout = summary.board.coverage.asics_configured,
                                    asics_reporting = summary.board.coverage.asics_reporting,
                                    asics_value_suppressed =
                                        summary.board.coverage.asics_value_suppressed,
                                    asics_never_seen = summary.board.coverage.asics_never_seen,
                                    "BZM2 per-ASIC summary requested"
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

/// How old a per-ASIC reading may be and still count toward an aggregate.
///
/// The DTS/VS stream sweeps the chain tens of times a second, so a reading
/// this old is not a slow sensor -- it is a sensor that has stopped, and
/// averaging it in would report a stack's last known temperature as its
/// current one.
const ASIC_READING_MAX_AGE: Duration = Duration::from_secs(30);

/// How many ASIC ids one asserted fault bit names before it stops listing
/// and starts counting. A fault on three ASICs must name them; a fault on
/// all three hundred is a chain-wide event, and the three hundredth id adds
/// nothing the count has not already said.
const FAULT_ASIC_ID_REPORT_CAP: usize = 16;

/// Why `asics_stale` is null rather than zero.
const READING_AGE_NOT_RETAINED_NOTE: &str = "no ASIC that reported a value also carried an \
     observation time, so nothing could be excluded for age and nothing is claimed fresh. A \
     per-ASIC reading records its arrival time on that ASIC's row as it is published, so this is \
     what a board nothing has published to yet looks like -- or one whose values reached board \
     state by a path that records no time. Either way a sensor that froze an hour ago would be \
     counted here exactly like one answering now, so nothing is counted.";

/// Why the fault bits are null rather than a row of zeroes.
const FAULT_BITS_NOT_RETAINED_NOTE: &str = "no ASIC that reported a value also carried fault bits. \
     A gen2 DTS/VS frame carries thermal_trip, thermal_fault, voltage_fault and voltage_shutdown, \
     and those are recorded on the ASIC's row as the frame is published; a gen1 frame carries none \
     at all, so a gen1 chain can never report them here. Reporting zero asserted faults would be a \
     claim nothing measured. The fault-adjacent signal this response can always measure is \
     asics_value_suppressed.";

/// Build the per-ASIC thermal/voltage summary from state this process
/// already holds.
///
/// Every figure is computed here, at request time, from the per-ASIC rows
/// below it; none of it is stored anywhere between requests. A cached
/// maximum would be a second copy of a fact that the readings already hold,
/// and it would drift from them the first time a poll was missed.
fn build_asic_summary(
    state: &BoardTelemetry,
    bus_layouts: &[Bzm2BusLayout],
    configured_asics_per_bus: &[u16],
    enumeration_enabled: bool,
    start_id: u8,
    generation: DtsVsGeneration,
    max_age: Duration,
) -> Bzm2AsicSummaryResponse {
    let buses = collect_bus_observations(state, bus_layouts, start_id, generation);
    let channels = voltage_channel_count(generation);
    let every_asic: Vec<&Bzm2AsicObservation> =
        buses.iter().flat_map(|bus| bus.asics.iter()).collect();
    // Decided ONCE, over every ASIC on the board, and handed to every block
    // below -- never re-decided per bus.
    //
    // Deciding per bus produced a response that contradicted itself: a bus
    // whose ASICs happened to be timestamped excluded a frozen 120 C reading
    // and reported a mean of 60, while the board block -- unable to
    // age-filter, because another bus carried no times -- included that same
    // reading, reported 80, and named the dead die as the hottest on the
    // board. One ASIC, two answers, in one response, with nothing to say
    // which was which. Whatever the board cannot measure, no bus inside it
    // may claim to have measured either.
    let ages_measured = ages_are_measured(&every_asic);
    let faults_measured = faults_are_measured(&every_asic);
    let (configured_asics, discovered_asics) =
        chain_asic_counts(configured_asics_per_bus, enumeration_enabled, bus_layouts);

    Bzm2AsicSummaryResponse {
        dts_vs_generation: match generation {
            DtsVsGeneration::Gen1 => Bzm2DtsVsGeneration::Gen1,
            DtsVsGeneration::Gen2 => Bzm2DtsVsGeneration::Gen2,
        },
        // What the board was told to expect, beside what it resolved. The
        // coverage blocks below are all denominated in the resolved chain,
        // so this pair is the only place a chain that enumerated short is
        // visible at all: 40 reporting of 40 resolved is a healthy-looking
        // chain, and `configured_asics: 100` is what says otherwise.
        configured_asics,
        discovered_asics,
        // Computed from every ASIC on the board, NOT by combining the bus
        // aggregates below it. Averaging bus means weights a one-ASIC bus
        // like a hundred-ASIC one, and the two answers disagree whenever the
        // buses differ in length -- exactly when a board is part-way through
        // losing a chain and the number matters most.
        board: aggregate_asics(
            &every_asic,
            channels,
            max_age,
            ages_measured,
            faults_measured,
        ),
        buses: buses
            .iter()
            .map(|bus| Bzm2BusAsicSummary {
                thread_index: bus.thread_index,
                serial_path: bus.serial_path.clone(),
                asic_start: bus.asic_start,
                thread_active: bus.thread_active,
                aggregate: aggregate_asics(
                    &bus.asics.iter().collect::<Vec<_>>(),
                    channels,
                    max_age,
                    ages_measured,
                    faults_measured,
                ),
            })
            .collect(),
        per_asic_current: Bzm2MeasurementNote {
            availability: Bzm2MeasurementAvailability::UnavailableOnPlatform,
            note: "no per-ASIC or per-board current sensor exists on this platform; the only \
                   current the machine can report at all is machine-wide, over CAN, and is not \
                   implemented. This is absent, not zero, and polling harder will not change it."
                .into(),
        },
        per_asic_power: Bzm2MeasurementNote {
            availability: Bzm2MeasurementAvailability::UnavailableOnPlatform,
            note: "per-ASIC power is this ASIC's rail voltage times its own current, and there is \
                   no per-ASIC current to multiply by. A voltage alone is not a power, and \
                   apportioning the board's input power across ASICs would be a model, not a \
                   measurement."
                .into(),
        },
    }
}

/// Read the per-ASIC rows the board holds into one observation per
/// configured ASIC.
///
/// The chain layout decides which ASICs are asked about, so an ASIC that has
/// never said anything still appears here -- as an observation with nothing
/// in it. Building the list from the readings instead would make a dark
/// chain and a healthy one produce the same shape of answer, one just
/// shorter.
fn collect_bus_observations(
    state: &BoardTelemetry,
    bus_layouts: &[Bzm2BusLayout],
    start_id: u8,
    generation: DtsVsGeneration,
) -> Vec<Bzm2BusObservation> {
    // Indexed once per request rather than scanned once per ASIC: three
    // hashboards are 300 temperature rows and 900 voltage rows, and this
    // endpoint exists to be polled.
    let temperatures: HashMap<&str, Option<f32>> = state
        .temperatures
        .iter()
        .map(|sensor| {
            (
                sensor.name.as_str(),
                sensor.temperature.map(Temperature::as_degrees_c),
            )
        })
        .collect();
    let voltages: HashMap<&str, Option<f32>> = state
        .powers
        .iter()
        .map(|power| (power.name.as_str(), power.voltage_v))
        .collect();
    // Each ASIC's own row, which is where its arrival time and its fault
    // bits live. Keyed by the (thread index, wire id) PAIR because ids are
    // local to a bus: keyed by id alone, bus 1's ASIC 7 would answer for bus
    // 0's, and a frozen sensor on one chain would read as a fresh one.
    let asic_rows: HashMap<(usize, u8), &AsicState> = state
        .asics
        .iter()
        .filter_map(|asic| asic.thread_index.map(|index| ((index, asic.id), asic)))
        .collect();
    // ONE clock read for the whole response. Reading it per ASIC would date
    // the first row of a snapshot and the last against different instants --
    // a gap too small to see and impossible to reconcile, in a response
    // whose whole purpose is that its figures describe one moment.
    let now = Instant::now();

    bus_layouts
        .iter()
        .enumerate()
        .map(|(thread_index, layout)| {
            // Names are spelled by the publisher and read back through that
            // same `sensor_prefix`, so the convention has one home rather
            // than two that can drift. Two copies of it disagreed once
            // already -- on the replacement character for a udev `by-id`
            // path -- and per-ASIC temperature lookup silently found
            // nothing.
            let prefix = sensor_prefix(&layout.serial_path);
            let asics = layout
                .wire_asic_ids(start_id)
                .into_iter()
                .enumerate()
                .map(|(offset, wire_asic_id)| {
                    // The values are looked up by sensor name; when the ASIC
                    // last spoke, and what it reported alongside those
                    // values, come from its own row. Those two are facts
                    // about the device rather than about any one of its
                    // sensors, and one frame delivers all of them together.
                    //
                    // An ASIC that has never answered has no row, so both
                    // stay `None` -- "not measured", never a zero. A fault
                    // count of zero that nothing measured is the most
                    // dangerous number this endpoint could print.
                    let row = asic_rows.get(&(thread_index, wire_asic_id));
                    Bzm2AsicObservation {
                        thread_index,
                        asic_id: layout
                            .asic_start
                            .saturating_add(u16::try_from(offset).unwrap_or(u16::MAX)),
                        wire_asic_id,
                        die_temp_c: read_slot(
                            &temperatures,
                            &format!("{prefix}-asic-{wire_asic_id}-dts"),
                        ),
                        channel_voltages: voltage_channel_names(&prefix, wire_asic_id, generation)
                            .iter()
                            .map(|name| read_slot(&voltages, name))
                            .collect(),
                        // Monotonic at both ends: the row's stamp and `now`
                        // come from the same clock, so this is an elapsed
                        // time and not a difference between two wall-clock
                        // readings that may have stepped between them.
                        age: row
                            .and_then(|row| row.observed_at)
                            .map(|observed_at| now.saturating_duration_since(observed_at)),
                        faults: row.and_then(|row| row.faults),
                    }
                })
                .collect();

            Bzm2BusObservation {
                thread_index,
                serial_path: layout.serial_path.clone(),
                asic_start: layout.asic_start,
                thread_active: state
                    .threads
                    .get(thread_index)
                    .map(|thread| thread.is_active),
                asics,
            }
        })
        .collect()
}

/// Compute one aggregate over a set of ASICs.
///
/// Classification first, arithmetic second: every ASIC lands in exactly one
/// of reporting / stale / value-suppressed / never-seen, and only the
/// reporting ones reach the statistics. The counts and the figures therefore
/// describe the same set of rows by construction, rather than by two
/// independent passes that could disagree.
fn aggregate_asics(
    asics: &[&Bzm2AsicObservation],
    channels: usize,
    max_age: Duration,
    board_ages_measured: bool,
    board_faults_measured: bool,
) -> Bzm2AsicAggregate {
    // Board-wide verdict first, then this set's own opportunity count. A set
    // with nothing in it to age has not looked and found nothing fresh; it
    // has not looked. Both conditions must hold before this block claims to
    // have applied the cutoff.
    let ages_measured = board_ages_measured && asics.iter().any(|asic| asic.has_value());

    let mut asics_stale = 0u16;
    let mut asics_value_suppressed = 0u16;
    let mut asics_never_seen = 0u16;
    let mut included: Vec<&Bzm2AsicObservation> = Vec::with_capacity(asics.len());
    for asic in asics {
        if !asic.has_reading() {
            asics_never_seen = asics_never_seen.saturating_add(1);
        } else if !asic.has_value() {
            asics_value_suppressed = asics_value_suppressed.saturating_add(1);
        } else if ages_measured && asic.age.is_some_and(|age| age > max_age) {
            asics_stale = asics_stale.saturating_add(1);
        } else {
            included.push(asic);
        }
    }

    let (hottest, coldest) = temperature_extremes(&included);
    Bzm2AsicAggregate {
        coverage: Bzm2AsicCoverage {
            asics_configured: u16::try_from(asics.len()).unwrap_or(u16::MAX),
            asics_reporting: u16::try_from(included.len()).unwrap_or(u16::MAX),
            // Some(0) means "measured, and none were stale". None means "not
            // measurable", which is a different statement and must not be
            // spelled the same way.
            asics_stale: ages_measured.then_some(asics_stale),
            asics_value_suppressed,
            asics_never_seen,
            reading_age: Bzm2ReadingAge {
                availability: if ages_measured {
                    Bzm2MeasurementAvailability::Measured
                } else {
                    Bzm2MeasurementAvailability::NotRetained
                },
                max_age_secs: max_age.as_secs_f32(),
                oldest_included_secs: ages_measured
                    .then(|| included.iter().filter_map(|asic| asic.age).max())
                    .flatten()
                    .map(|age| age.as_secs_f32()),
                note: (!ages_measured).then(|| READING_AGE_NOT_RETAINED_NOTE.to_owned()),
            },
        },
        // The slots are handed over whole, not pre-filtered to their values:
        // a figure has to be able to say how many of the ASICs behind it had
        // this one sensor gated, and `Option<f32>` at this boundary has
        // already thrown that away.
        die_temperature_c: reading_stats(included.iter().map(|asic| asic.die_temp_c)),
        hottest,
        coldest,
        voltage_channels: (0..channels)
            .map(|channel| Bzm2VoltageChannelStats {
                channel: u8::try_from(channel).unwrap_or(u8::MAX),
                stats: reading_stats(included.iter().map(|asic| {
                    asic.channel_voltages
                        .get(channel)
                        .copied()
                        .unwrap_or(SensorSlot::Absent)
                })),
            })
            .collect(),
        faults: summarise_faults(&included, board_faults_measured),
    }
}

/// Did every ASIC that contributed a value also carry an observation time?
///
/// All or nothing, deliberately, and asked once for the whole board. A
/// staleness count over whichever subset happened to be timestamped is the
/// same defect as a mean over 40 of 100 ASICs: it looks like an answer about
/// the chain. Where the opportunity to measure is zero the answer is
/// "unmeasured", never "none are stale".
fn ages_are_measured(asics: &[&Bzm2AsicObservation]) -> bool {
    let with_value = asics.iter().filter(|asic| asic.has_value()).count();
    with_value > 0
        && asics
            .iter()
            .filter(|asic| asic.has_value() && asic.age.is_some())
            .count()
            == with_value
}

/// Did every ASIC that contributed a value also carry its fault bits?
///
/// The same all-or-nothing question as [`ages_are_measured`], asked once for
/// the whole board for the same reason: a fault count that holds on one bus
/// and not on another is a count a consumer will sum across buses and
/// believe.
fn faults_are_measured(asics: &[&Bzm2AsicObservation]) -> bool {
    let with_value = asics.iter().filter(|asic| asic.has_value()).count();
    with_value > 0
        && asics
            .iter()
            .filter(|asic| asic.has_value() && asic.faults.is_some())
            .count()
            == with_value
}

/// The hottest and coldest reporting die.
///
/// Ties go to the lower board-wide id, so polling an unchanged chain twice
/// names the same ASIC twice. A tie-break by iteration order would make the
/// answer depend on how the buses happened to be walked.
fn temperature_extremes(
    included: &[&Bzm2AsicObservation],
) -> (Option<Bzm2AsicExtreme>, Option<Bzm2AsicExtreme>) {
    let mut hottest: Option<Bzm2AsicExtreme> = None;
    let mut coldest: Option<Bzm2AsicExtreme> = None;

    for asic in included {
        let Some(temperature_c) = asic.die_temp_c.value() else {
            continue;
        };
        let candidate = Bzm2AsicExtreme {
            asic_id: asic.asic_id,
            wire_asic_id: asic.wire_asic_id,
            thread_index: asic.thread_index,
            temperature_c,
        };
        if hottest.as_ref().is_none_or(|best| {
            candidate.temperature_c > best.temperature_c
                || (candidate.temperature_c == best.temperature_c
                    && candidate.asic_id < best.asic_id)
        }) {
            hottest = Some(candidate.clone());
        }
        if coldest.as_ref().is_none_or(|best| {
            candidate.temperature_c < best.temperature_c
                || (candidate.temperature_c == best.temperature_c
                    && candidate.asic_id < best.asic_id)
        }) {
            coldest = Some(candidate);
        }
    }

    (hottest, coldest)
}

/// Minimum, maximum, mean and spread over the slots handed in, or `None`
/// when none of them carried a value.
///
/// `None` rather than a zeroed block: a mean of 0.0 C over an empty chain is
/// a number a dashboard will happily plot.
///
/// Takes slots rather than values so that the figure can carry its own
/// denominator. An ASIC that answers with its rails while its die sensor is
/// gated is a reporting ASIC -- per-ASIC coverage is right to count it as
/// one -- but it is not behind this mean, and nothing outside this block
/// knows that.
fn reading_stats(slots: impl Iterator<Item = SensorSlot>) -> Option<Bzm2ReadingStats> {
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    let mut total = 0.0f64;
    let mut samples = 0u16;
    let mut suppressed = 0u16;

    for slot in slots {
        match slot {
            SensorSlot::Value(value) => {
                min = min.min(value);
                max = max.max(value);
                // Accumulated in f64 so that a three-hundred-sample mean does
                // not depend on the order the buses were walked in.
                total += f64::from(value);
                samples = samples.saturating_add(1);
            }
            // A row that arrived carrying nothing: this sensor was gated on
            // an ASIC that is otherwise answering.
            SensorSlot::Suppressed => suppressed = suppressed.saturating_add(1),
            // No row at all -- an ASIC that publishes no such sensor, such as
            // a gen1 part and a die temperature. Not a gated reading.
            SensorSlot::Absent => {}
        }
    }

    (samples > 0).then(|| Bzm2ReadingStats {
        min,
        max,
        mean: (total / f64::from(samples)) as f32,
        // Computed, not left to the caller to subtract: on a series stack
        // this is the number that matters, and a figure every consumer
        // derives for itself is a figure they will derive differently.
        spread: max - min,
        samples,
        asics_value_suppressed: suppressed,
    })
}

/// One fault bit, paired with the way to read it off an ASIC's bits.
type FaultBitExtractor = (Bzm2FaultBit, fn(&AsicFaultBits) -> bool);

/// Count the fault bits asserted across the ASICs whose readings counted.
///
/// A stale ASIC's bits are not "currently asserted", so this runs over the
/// included set only. Partial fault coverage is reported as unavailable for
/// the same reason partial age coverage is: a count over some of the chain
/// reads exactly like a count over all of it.
fn summarise_faults(
    included: &[&Bzm2AsicObservation],
    board_faults_measured: bool,
) -> Bzm2FaultSummary {
    let reporting = included.iter().filter(|asic| asic.faults.is_some()).count();
    // Board-wide verdict first, so a bus cannot report counts the board has
    // already said it cannot measure; then this set's own opportunity count,
    // because a bit asserted by none of nobody is not a measured zero.
    if !board_faults_measured || reporting != included.len() || included.is_empty() {
        return Bzm2FaultSummary {
            availability: Bzm2MeasurementAvailability::NotRetained,
            bits: None,
            note: Some(if !board_faults_measured || reporting == 0 {
                FAULT_BITS_NOT_RETAINED_NOTE.to_owned()
            } else {
                format!(
                    "fault bits reached board state for only {reporting} of {} reporting ASICs; \
                     a count over that subset would read as this chain's count",
                    included.len()
                )
            }),
        };
    }

    let extractors: [FaultBitExtractor; 4] = [
        (Bzm2FaultBit::ThermalTrip, |bits| bits.thermal_trip),
        (Bzm2FaultBit::ThermalFault, |bits| bits.thermal_fault),
        (Bzm2FaultBit::VoltageFault, |bits| bits.voltage_fault),
        (Bzm2FaultBit::VoltageShutdown, |bits| bits.voltage_shutdown),
    ];

    let bits = extractors
        .into_iter()
        .map(|(bit, asserted)| {
            let ids: Vec<u16> = included
                .iter()
                .filter(|asic| asic.faults.is_some_and(|faults| asserted(&faults)))
                .map(|asic| asic.asic_id)
                .collect();
            let omitted = ids.len().saturating_sub(FAULT_ASIC_ID_REPORT_CAP);
            Bzm2FaultBitCount {
                // The full count, whatever the cap did to the id list.
                asics_asserting: u16::try_from(ids.len()).unwrap_or(u16::MAX),
                asic_ids: ids.into_iter().take(FAULT_ASIC_ID_REPORT_CAP).collect(),
                asic_ids_omitted: u16::try_from(omitted).unwrap_or(u16::MAX),
                bit,
            }
        })
        .collect();

    Bzm2FaultSummary {
        availability: Bzm2MeasurementAvailability::Measured,
        bits: Some(bits),
        note: None,
    }
}

/// The sensor names one ASIC's voltage channels are published under.
///
/// Gen1 publishes a single rail reading with no channel index on the wire;
/// gen2 publishes one per channel. The count comes from
/// [`voltage_channel_count`] rather than a literal here, so the names and
/// the per-channel blocks in the response cannot disagree about how many
/// channels a generation has.
fn voltage_channel_names(
    prefix: &str,
    wire_asic_id: u8,
    generation: DtsVsGeneration,
) -> Vec<String> {
    match generation {
        DtsVsGeneration::Gen1 => vec![format!("{prefix}-asic-{wire_asic_id}-vs")],
        DtsVsGeneration::Gen2 => (0..voltage_channel_count(generation))
            .map(|channel| format!("{prefix}-asic-{wire_asic_id}-vs-ch{channel}"))
            .collect(),
    }
}

/// How many voltage channels an ASIC publishes on this generation.
fn voltage_channel_count(generation: DtsVsGeneration) -> usize {
    match generation {
        DtsVsGeneration::Gen1 => 1,
        DtsVsGeneration::Gen2 => 3,
    }
}

/// Look one reading up by name, keeping the three outcomes apart.
fn read_slot(readings: &HashMap<&str, Option<f32>>, name: &str) -> SensorSlot {
    match readings.get(name) {
        None => SensorSlot::Absent,
        Some(None) => SensorSlot::Suppressed,
        Some(Some(value)) if value.is_finite() => SensorSlot::Value(*value),
        // A NaN or an infinity is not a reading. Caught at the boundary so
        // that no aggregate downstream has to defend against one: a single
        // NaN in a sum makes the whole mean NaN, and a NaN compares false
        // against everything, which would quietly disqualify it from being
        // the maximum it actually is.
        Some(Some(_)) => SensorSlot::Suppressed,
    }
}

/// One bus's per-ASIC observations, as the board holds them right now.
#[derive(Debug, Clone)]
struct Bzm2BusObservation {
    thread_index: usize,
    serial_path: String,
    asic_start: u16,
    thread_active: Option<bool>,
    asics: Vec<Bzm2AsicObservation>,
}

/// What board state holds about one configured ASIC at request time.
#[derive(Debug, Clone)]
struct Bzm2AsicObservation {
    thread_index: usize,
    /// Board-wide id: unique across buses.
    asic_id: u16,
    /// The id this ASIC answers to on its own chain, which is what the
    /// sensor names are spelled with.
    wire_asic_id: u8,
    die_temp_c: SensorSlot,
    /// One slot per channel this generation publishes.
    channel_voltages: Vec<SensorSlot>,
    /// How long ago these readings arrived, when that is known at all.
    age: Option<Duration>,
    /// The fault bits this ASIC last reported, when they are known at all.
    faults: Option<AsicFaultBits>,
}

impl Bzm2AsicObservation {
    /// Has this ASIC published anything at all?
    fn has_reading(&self) -> bool {
        !self.die_temp_c.is_absent() || self.channel_voltages.iter().any(|slot| !slot.is_absent())
    }

    /// Did any of it come with a number?
    fn has_value(&self) -> bool {
        self.die_temp_c.value().is_some()
            || self
                .channel_voltages
                .iter()
                .any(|slot| slot.value().is_some())
    }
}

/// One reading as board state holds it.
///
/// The distinction between the first two variants is most of the point of
/// this endpoint. "No row" means an ASIC that has never answered. "A row
/// with no value" means one that is answering while its sensor is disabled,
/// its frame says the reading is invalid, or the value was one no die can
/// physically be at -- the publish-side gate having fired. Those are
/// different faults with different fixes, and an `Option<f32>` alone cannot
/// tell them apart.
#[derive(Debug, Clone, Copy, PartialEq)]
enum SensorSlot {
    Absent,
    Suppressed,
    Value(f32),
}

impl SensorSlot {
    fn value(self) -> Option<f32> {
        match self {
            Self::Value(value) => Some(value),
            Self::Absent | Self::Suppressed => None,
        }
    }

    fn is_absent(self) -> bool {
        matches!(self, Self::Absent)
    }
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

    // --- per-ASIC summary ------------------------------------------------
    //
    // The figures are checked against values worked out by hand in each
    // case, never against the expression the code evaluates. A test that
    // recomputes `max - min` the way the code does would pass whatever both
    // of them did.

    use crate::api_client::types::{PowerMeasurement, TemperatureSensor, ThreadTelemetry};
    use crate::asic::hash_thread::{
        HashThreadAsicObservation, HashThreadPowerReading, HashThreadTelemetryUpdate,
        HashThreadTemperatureReading,
    };
    use crate::types::Temperature;

    use super::super::bringup::Bzm2BringupConfig;
    use super::super::config::{
        Bzm2CalibrationConfig, Bzm2EnumerationConfig, DEFAULT_BAUD_RATE, TEST_NOMINAL_HASHRATE_THS,
    };
    use super::super::telemetry::Bzm2TelemetryConfig;
    use super::super::{Bzm2Board, Bzm2RuntimeConfig};

    const MAX_AGE: Duration = Duration::from_secs(30);
    const GEN2_CHANNELS: usize = 3;

    #[test]
    fn a_full_chain_reports_every_asic_and_the_figures_they_make() {
        // Four dice at 60/70/80/90 C: mean 75, spread 30. Channel 0 at
        // 0.70/0.72/0.74/0.76 V: mean 0.73, spread 0.06.
        let asics: Vec<_> = [(60.0, 0.70), (70.0, 0.72), (80.0, 0.74), (90.0, 0.76)]
            .into_iter()
            .enumerate()
            .map(|(index, (temp, volts))| {
                observation(
                    0,
                    index as u16,
                    index as u8,
                    SensorSlot::Value(temp),
                    &[
                        SensorSlot::Value(volts),
                        SensorSlot::Value(volts),
                        SensorSlot::Value(volts),
                    ],
                )
            })
            .collect();

        let aggregate = aggregate_of(&asics, GEN2_CHANNELS, MAX_AGE);

        assert_eq!(aggregate.coverage.asics_configured, 4);
        assert_eq!(aggregate.coverage.asics_reporting, 4);
        assert_eq!(aggregate.coverage.asics_never_seen, 0);
        assert_eq!(aggregate.coverage.asics_value_suppressed, 0);
        assert_stats(
            aggregate.die_temperature_c.as_ref().unwrap(),
            60.0,
            90.0,
            75.0,
            30.0,
            4,
        );
        assert_eq!(aggregate.voltage_channels.len(), GEN2_CHANNELS);
        for channel in &aggregate.voltage_channels {
            assert_stats(channel.stats.as_ref().unwrap(), 0.70, 0.76, 0.73, 0.06, 4);
        }
        assert_eq!(aggregate.hottest.as_ref().unwrap().asic_id, 3);
        assert_eq!(aggregate.coldest.as_ref().unwrap().asic_id, 0);
    }

    #[test]
    fn a_partial_chain_averages_only_the_asics_that_answered() {
        // Three of six answer, at 60/70/80: the mean is 70. Divided by the
        // six that were configured it would be 35 -- a plausible-looking
        // number that no die is at.
        let mut asics: Vec<_> = [60.0f32, 70.0, 80.0]
            .into_iter()
            .enumerate()
            .map(|(index, temp)| {
                observation(0, index as u16, index as u8, SensorSlot::Value(temp), &[])
            })
            .collect();
        asics.extend((3..6u16).map(|id| observation(0, id, id as u8, SensorSlot::Absent, &[])));

        let aggregate = aggregate_of(&asics, GEN2_CHANNELS, MAX_AGE);

        assert_eq!(aggregate.coverage.asics_configured, 6);
        assert_eq!(aggregate.coverage.asics_reporting, 3);
        assert_eq!(aggregate.coverage.asics_never_seen, 3);
        let stats = aggregate.die_temperature_c.as_ref().unwrap();
        assert_stats(stats, 60.0, 80.0, 70.0, 20.0, 3);
        assert_ne!(
            stats.mean, 35.0,
            "the mean must be over the ASICs that reported, not over the chain length"
        );
    }

    #[test]
    fn an_all_stale_chain_reports_no_figures_rather_than_zeroes() {
        let asics: Vec<_> = [60.0f32, 70.0, 80.0]
            .into_iter()
            .enumerate()
            .map(|(index, temp)| {
                with_age(
                    observation(
                        0,
                        index as u16,
                        index as u8,
                        SensorSlot::Value(temp),
                        &[SensorSlot::Value(0.72)],
                    ),
                    Duration::from_secs(60),
                )
            })
            .collect();

        let aggregate = aggregate_of(&asics, 1, MAX_AGE);

        assert_eq!(aggregate.coverage.asics_reporting, 0);
        assert_eq!(aggregate.coverage.asics_stale, Some(3));
        assert!(
            aggregate.die_temperature_c.is_none(),
            "an aggregate over an excluded chain must be absent, not 0.0"
        );
        assert!(aggregate.voltage_channels[0].stats.is_none());
        assert!(aggregate.hottest.is_none() && aggregate.coldest.is_none());
        assert_eq!(
            aggregate.coverage.reading_age.availability,
            Bzm2MeasurementAvailability::Measured
        );
        assert!(
            aggregate
                .coverage
                .reading_age
                .oldest_included_secs
                .is_none()
        );
    }

    #[test]
    fn an_empty_chain_reports_null_aggregates_and_no_nan() {
        let aggregate = aggregate_of(&[], GEN2_CHANNELS, MAX_AGE);

        assert_eq!(aggregate.coverage.asics_configured, 0);
        assert_eq!(aggregate.coverage.asics_reporting, 0);
        assert!(aggregate.die_temperature_c.is_none());
        assert!(aggregate.hottest.is_none());
        for channel in &aggregate.voltage_channels {
            assert!(channel.stats.is_none());
        }
        // Zero ASICs is zero opportunities to observe a stale one, which is
        // not the same as having looked and found none.
        assert_eq!(aggregate.coverage.asics_stale, None);
        assert_eq!(
            aggregate.coverage.reading_age.availability,
            Bzm2MeasurementAvailability::NotRetained
        );
    }

    #[test]
    fn a_single_asic_is_both_extremes_and_has_no_spread() {
        let asics = vec![observation(
            2,
            7,
            5,
            SensorSlot::Value(64.5),
            &[SensorSlot::Value(0.81)],
        )];

        let aggregate = aggregate_of(&asics, 1, MAX_AGE);

        assert_stats(
            aggregate.die_temperature_c.as_ref().unwrap(),
            64.5,
            64.5,
            64.5,
            0.0,
            1,
        );
        let hottest = aggregate.hottest.as_ref().unwrap();
        let coldest = aggregate.coldest.as_ref().unwrap();
        assert_eq!((hottest.asic_id, hottest.wire_asic_id), (7, 5));
        assert_eq!(hottest.thread_index, 2);
        assert_eq!(coldest.asic_id, hottest.asic_id);
        assert_close(hottest.temperature_c, 64.5, "hottest temperature");
    }

    #[test]
    fn hottest_and_coldest_break_ties_on_the_lower_asic_id() {
        // Two dice at 90 and two at 50: whichever order they are walked in,
        // the ids named must be the lower of each tied pair.
        let hot_then_cold = vec![
            observation(0, 1, 1, SensorSlot::Value(90.0), &[]),
            observation(0, 4, 4, SensorSlot::Value(90.0), &[]),
            observation(0, 2, 2, SensorSlot::Value(50.0), &[]),
            observation(0, 9, 9, SensorSlot::Value(50.0), &[]),
        ];
        let reversed: Vec<_> = hot_then_cold.iter().rev().cloned().collect();

        for (name, asics) in [("ascending", hot_then_cold), ("reversed", reversed)] {
            let aggregate = aggregate_of(&asics, 0, MAX_AGE);
            assert_eq!(
                aggregate.hottest.as_ref().unwrap().asic_id,
                1,
                "{name}: hottest tie must name the lower id"
            );
            assert_eq!(
                aggregate.coldest.as_ref().unwrap().asic_id,
                2,
                "{name}: coldest tie must name the lower id"
            );
        }
    }

    #[test]
    fn coverage_counts_partition_the_configured_chain() {
        let reporting = observation(0, 0, 0, SensorSlot::Value(70.0), &[SensorSlot::Value(0.7)]);
        let suppressed = observation(0, 1, 1, SensorSlot::Suppressed, &[SensorSlot::Suppressed]);
        let never_seen = observation(0, 2, 2, SensorSlot::Absent, &[SensorSlot::Absent]);
        let stale = with_age(
            observation(0, 3, 3, SensorSlot::Value(70.0), &[SensorSlot::Value(0.7)]),
            Duration::from_secs(120),
        );
        let fresh = with_age(
            observation(0, 4, 4, SensorSlot::Value(72.0), &[SensorSlot::Value(0.7)]),
            Duration::from_secs(1),
        );

        // name, chain, reporting, stale, suppressed, never seen
        type CoverageCase = (
            &'static str,
            Vec<Bzm2AsicObservation>,
            u16,
            Option<u16>,
            u16,
            u16,
        );
        let cases: Vec<CoverageCase> = vec![
            ("all reporting", vec![reporting.clone()], 1, None, 0, 0),
            (
                "one of each, untimed",
                vec![reporting.clone(), suppressed.clone(), never_seen.clone()],
                1,
                None,
                1,
                1,
            ),
            (
                "timed, one stale",
                vec![fresh.clone(), stale.clone()],
                1,
                Some(1),
                0,
                0,
            ),
            (
                "timed, with a dark and a mute ASIC",
                vec![
                    fresh.clone(),
                    stale.clone(),
                    suppressed.clone(),
                    never_seen.clone(),
                ],
                1,
                Some(1),
                1,
                1,
            ),
            ("nothing configured", Vec::new(), 0, None, 0, 0),
        ];

        for (name, asics, expect_reporting, expect_stale, expect_suppressed, expect_never) in cases
        {
            let coverage = aggregate_of(&asics, 1, MAX_AGE).coverage;
            assert_eq!(
                coverage.asics_reporting, expect_reporting,
                "{name}: reporting"
            );
            assert_eq!(coverage.asics_stale, expect_stale, "{name}: stale");
            assert_eq!(
                coverage.asics_value_suppressed, expect_suppressed,
                "{name}: suppressed"
            );
            assert_eq!(
                coverage.asics_never_seen, expect_never,
                "{name}: never seen"
            );
            assert_eq!(
                coverage.asics_reporting
                    + coverage.asics_stale.unwrap_or(0)
                    + coverage.asics_value_suppressed
                    + coverage.asics_never_seen,
                coverage.asics_configured,
                "{name}: every configured ASIC must land in exactly one bucket"
            );
        }
    }

    #[test]
    fn a_reading_with_no_value_is_not_a_missing_one() {
        // An ASIC that is talking while its sensor says nothing usable is a
        // different fault from one that has gone silent, and it must not be
        // averaged in as if it had answered.
        let asics = vec![
            observation(0, 0, 0, SensorSlot::Value(80.0), &[SensorSlot::Suppressed]),
            observation(0, 1, 1, SensorSlot::Suppressed, &[SensorSlot::Suppressed]),
        ];

        let aggregate = aggregate_of(&asics, 1, MAX_AGE);

        assert_eq!(aggregate.coverage.asics_reporting, 1);
        assert_eq!(aggregate.coverage.asics_value_suppressed, 1);
        assert_eq!(aggregate.coverage.asics_never_seen, 0);
        assert_stats(
            aggregate.die_temperature_c.as_ref().unwrap(),
            80.0,
            80.0,
            80.0,
            0.0,
            1,
        );
        assert!(
            aggregate.voltage_channels[0].stats.is_none(),
            "a channel nobody reported has no statistics"
        );
    }

    #[test]
    fn partial_age_coverage_reports_unmeasured_rather_than_a_partial_count() {
        let asics = vec![
            with_age(
                observation(0, 0, 0, SensorSlot::Value(70.0), &[]),
                Duration::from_secs(1),
            ),
            observation(0, 1, 1, SensorSlot::Value(72.0), &[]),
        ];

        let coverage = aggregate_of(&asics, 0, MAX_AGE).coverage;

        assert_eq!(
            coverage.asics_stale, None,
            "a staleness count over the timed half would read as the chain's count"
        );
        assert_eq!(
            coverage.reading_age.availability,
            Bzm2MeasurementAvailability::NotRetained
        );
        assert!(coverage.reading_age.note.is_some());
        assert_eq!(coverage.asics_reporting, 2);
    }

    #[test]
    fn a_stale_reading_is_excluded_and_the_oldest_included_one_is_reported() {
        let asics = vec![
            with_age(
                observation(0, 0, 0, SensorSlot::Value(60.0), &[]),
                Duration::from_secs(2),
            ),
            with_age(
                observation(0, 1, 1, SensorSlot::Value(80.0), &[]),
                Duration::from_secs(20),
            ),
            with_age(
                observation(0, 2, 2, SensorSlot::Value(120.0), &[]),
                Duration::from_secs(31),
            ),
        ];

        let aggregate = aggregate_of(&asics, 0, MAX_AGE);

        assert_eq!(aggregate.coverage.asics_stale, Some(1));
        assert_stats(
            aggregate.die_temperature_c.as_ref().unwrap(),
            60.0,
            80.0,
            70.0,
            20.0,
            2,
        );
        assert_eq!(
            aggregate.hottest.as_ref().unwrap().asic_id,
            1,
            "the excluded ASIC must not be the hottest either"
        );
        assert_close(
            aggregate.coverage.reading_age.oldest_included_secs.unwrap(),
            20.0,
            "oldest included age",
        );
        assert_close(aggregate.coverage.reading_age.max_age_secs, 30.0, "cutoff");
    }

    #[test]
    fn asserted_fault_bits_name_the_asics_asserting_them() {
        let mut asics: Vec<_> = (0..5u16)
            .map(|id| {
                with_faults(
                    observation(0, id, id as u8, SensorSlot::Value(70.0), &[]),
                    AsicFaultBits::default(),
                )
            })
            .collect();
        asics[1].faults = Some(AsicFaultBits {
            thermal_trip: true,
            ..Default::default()
        });
        asics[3].faults = Some(AsicFaultBits {
            thermal_trip: true,
            voltage_shutdown: true,
            ..Default::default()
        });

        let faults = aggregate_of(&asics, 0, MAX_AGE).faults;

        assert_eq!(faults.availability, Bzm2MeasurementAvailability::Measured);
        let bits = faults.bits.as_ref().unwrap();
        assert_eq!(bits.len(), 4, "every bit is reported, asserted or not");
        let bit = |wanted: Bzm2FaultBit| bits.iter().find(|entry| entry.bit == wanted).unwrap();
        assert_eq!(bit(Bzm2FaultBit::ThermalTrip).asics_asserting, 2);
        assert_eq!(bit(Bzm2FaultBit::ThermalTrip).asic_ids, vec![1, 3]);
        assert_eq!(bit(Bzm2FaultBit::VoltageShutdown).asic_ids, vec![3]);
        assert_eq!(bit(Bzm2FaultBit::ThermalFault).asics_asserting, 0);
        assert!(bit(Bzm2FaultBit::ThermalFault).asic_ids.is_empty());
    }

    #[test]
    fn a_chain_wide_fault_counts_every_asic_and_names_a_capped_number_of_them() {
        let asics: Vec<_> = (0..20u16)
            .map(|id| {
                with_faults(
                    observation(0, id, id as u8, SensorSlot::Value(95.0), &[]),
                    AsicFaultBits {
                        thermal_fault: true,
                        ..Default::default()
                    },
                )
            })
            .collect();

        let faults = aggregate_of(&asics, 0, MAX_AGE).faults;
        let bits = faults.bits.as_ref().unwrap();
        let thermal = bits
            .iter()
            .find(|entry| entry.bit == Bzm2FaultBit::ThermalFault)
            .unwrap();

        assert_eq!(
            thermal.asics_asserting, 20,
            "the count is the whole chain, whatever the cap did to the ids"
        );
        assert_eq!(thermal.asic_ids.len(), FAULT_ASIC_ID_REPORT_CAP);
        assert_eq!(
            usize::from(thermal.asic_ids_omitted) + thermal.asic_ids.len(),
            usize::from(thermal.asics_asserting),
            "listed plus omitted must account for every asserting ASIC"
        );
    }

    #[test]
    fn fault_bits_nothing_measured_are_absent_rather_than_zero() {
        // What the live board produces today: readings, no fault bits.
        let asics = vec![observation(0, 0, 0, SensorSlot::Value(70.0), &[])];

        let faults = aggregate_of(&asics, 0, MAX_AGE).faults;

        assert_eq!(
            faults.availability,
            Bzm2MeasurementAvailability::NotRetained
        );
        assert!(
            faults.bits.is_none(),
            "a list of zeroed counts would read as a healthy chain"
        );
        assert!(faults.note.is_some());
    }

    #[test]
    fn partial_fault_coverage_is_reported_as_unavailable() {
        let asics = vec![
            with_faults(
                observation(0, 0, 0, SensorSlot::Value(70.0), &[]),
                AsicFaultBits::default(),
            ),
            observation(0, 1, 1, SensorSlot::Value(70.0), &[]),
        ];

        let faults = aggregate_of(&asics, 0, MAX_AGE).faults;

        assert_eq!(
            faults.availability,
            Bzm2MeasurementAvailability::NotRetained
        );
        assert!(faults.bits.is_none());
    }

    #[test]
    fn each_generation_publishes_the_channels_it_names() {
        // The spellings are written out here rather than built from the
        // helper the code uses, so a change to the naming convention fails
        // this test instead of travelling through it.
        let gen1 = voltage_channel_names("ttyUSB0", 4, DtsVsGeneration::Gen1);
        assert_eq!(gen1, vec!["ttyUSB0-asic-4-vs".to_string()]);
        assert_eq!(gen1.len(), voltage_channel_count(DtsVsGeneration::Gen1));

        let gen2 = voltage_channel_names("ttyUSB0", 4, DtsVsGeneration::Gen2);
        assert_eq!(
            gen2,
            vec![
                "ttyUSB0-asic-4-vs-ch0".to_string(),
                "ttyUSB0-asic-4-vs-ch1".to_string(),
                "ttyUSB0-asic-4-vs-ch2".to_string(),
            ]
        );
        assert_eq!(gen2.len(), voltage_channel_count(DtsVsGeneration::Gen2));
    }

    #[test]
    fn a_gen1_chain_summarises_its_one_channel_and_claims_no_temperature() {
        let state = telemetry_state(
            &[],
            &[
                ("ttyUSB0-asic-0-vs", Some(0.80)),
                ("ttyUSB0-asic-1-vs", Some(0.84)),
            ],
            &[],
        );
        let layouts = vec![bus("/dev/ttyUSB0", 0, 2)];

        let summary = summary_of(&state, &layouts, 0, DtsVsGeneration::Gen1);

        assert_eq!(summary.dts_vs_generation, Bzm2DtsVsGeneration::Gen1);
        assert_eq!(summary.board.voltage_channels.len(), 1);
        assert_stats(
            summary.board.voltage_channels[0].stats.as_ref().unwrap(),
            0.80,
            0.84,
            0.82,
            0.04,
            2,
        );
        assert!(
            summary.board.die_temperature_c.is_none(),
            "a gen1 chain publishes no die temperature at all"
        );
        assert_eq!(
            summary.board.coverage.asics_reporting, 2,
            "an ASIC that reported only a voltage is still reporting"
        );
    }

    #[test]
    fn board_figures_are_computed_from_the_asics_not_from_the_bus_aggregates() {
        // One ASIC at 100 C on bus 0, three at 60 C on bus 1. Over the four
        // dice the mean is 70. Averaging the two bus means would give 80,
        // and a 100 C die would be reported as a board sitting ten degrees
        // cooler than it is.
        let state = telemetry_state(
            &[
                ("ttyUSB0-asic-0-dts", Some(100.0)),
                ("ttyUSB1-asic-0-dts", Some(60.0)),
                ("ttyUSB1-asic-1-dts", Some(60.0)),
                ("ttyUSB1-asic-2-dts", Some(60.0)),
            ],
            &[],
            &[true, true],
        );
        let layouts = vec![bus("/dev/ttyUSB0", 0, 1), bus("/dev/ttyUSB1", 1, 3)];

        let summary = summary_of(&state, &layouts, 0, DtsVsGeneration::Gen2);

        let board = summary.board.die_temperature_c.as_ref().unwrap();
        assert_stats(board, 60.0, 100.0, 70.0, 40.0, 4);
        assert_ne!(
            board.mean, 80.0,
            "the board mean must not be the mean of the bus means"
        );
        assert_eq!(summary.buses.len(), 2);
        assert_close(
            summary.buses[0]
                .aggregate
                .die_temperature_c
                .as_ref()
                .unwrap()
                .mean,
            100.0,
            "bus 0 mean",
        );
        assert_close(
            summary.buses[1]
                .aggregate
                .die_temperature_c
                .as_ref()
                .unwrap()
                .mean,
            60.0,
            "bus 1 mean",
        );
        assert_eq!(summary.board.hottest.as_ref().unwrap().asic_id, 0);
        assert_eq!(
            summary.board.coldest.as_ref().unwrap().asic_id,
            1,
            "the coldest of three tied dice is the lowest board-wide id"
        );
        assert_eq!(summary.buses[0].thread_active, Some(true));
    }

    #[test]
    fn readings_are_found_under_the_wire_ids_the_chain_answers_to() {
        // Bus 1 is addressed from wire id 2 while its ASICs are known
        // board-wide as 10 and 11. Reading the sensor rows under the
        // board-wide ids would find nothing at all.
        let state = telemetry_state(
            &[
                ("ttyUSB1-asic-2-dts", Some(71.0)),
                ("ttyUSB1-asic-3-dts", Some(73.0)),
                ("ttyUSB1-asic-10-dts", Some(999.0)),
            ],
            &[],
            &[],
        );
        let layouts = vec![bus("/dev/ttyUSB1", 10, 2)];

        let summary = summary_of(&state, &layouts, 2, DtsVsGeneration::Gen2);

        assert_stats(
            summary.board.die_temperature_c.as_ref().unwrap(),
            71.0,
            73.0,
            72.0,
            2.0,
            2,
        );
        let hottest = summary.board.hottest.as_ref().unwrap();
        assert_eq!(
            (hottest.asic_id, hottest.wire_asic_id),
            (11, 3),
            "both ids are reported, and they are not the same number"
        );
    }

    #[test]
    fn a_dark_chain_reports_its_length_and_no_figures() {
        let state = telemetry_state(&[], &[], &[false]);
        let layouts = vec![bus("/dev/ttyUSB0", 0, 100)];

        let summary = summary_of(&state, &layouts, 0, DtsVsGeneration::Gen2);

        assert_eq!(summary.board.coverage.asics_configured, 100);
        assert_eq!(summary.board.coverage.asics_never_seen, 100);
        assert_eq!(summary.board.coverage.asics_reporting, 0);
        assert!(summary.board.die_temperature_c.is_none());
        assert_eq!(summary.buses[0].thread_active, Some(false));
    }

    #[test]
    fn a_non_finite_reading_is_not_a_measurement() {
        let state = telemetry_state(
            &[
                ("ttyUSB0-asic-0-dts", Some(70.0)),
                ("ttyUSB0-asic-1-dts", Some(f32::NAN)),
            ],
            &[],
            &[],
        );
        let layouts = vec![bus("/dev/ttyUSB0", 0, 2)];

        let summary = summary_of(&state, &layouts, 0, DtsVsGeneration::Gen2);

        let stats = summary.board.die_temperature_c.as_ref().unwrap();
        assert!(
            stats.mean.is_finite() && stats.max.is_finite(),
            "one NaN must not poison the whole aggregate"
        );
        assert_stats(stats, 70.0, 70.0, 70.0, 0.0, 1);
        assert_eq!(summary.board.coverage.asics_value_suppressed, 1);
    }

    #[test]
    fn per_asic_current_and_power_are_reported_as_unavailable_on_this_platform() {
        let summary = summary_of(
            &telemetry_state(&[("ttyUSB0-asic-0-dts", Some(70.0))], &[], &[]),
            &[bus("/dev/ttyUSB0", 0, 1)],
            0,
            DtsVsGeneration::Gen2,
        );

        // Distinct from "not measured this cycle": no sensor exists to
        // measure it, so this can never become a number by polling again.
        assert_eq!(
            summary.per_asic_current.availability,
            Bzm2MeasurementAvailability::UnavailableOnPlatform
        );
        assert_eq!(
            summary.per_asic_power.availability,
            Bzm2MeasurementAvailability::UnavailableOnPlatform
        );
        // What the board does not retain is a different absence again.
        assert_eq!(
            summary.board.faults.availability,
            Bzm2MeasurementAvailability::NotRetained
        );
    }

    #[tokio::test]
    async fn the_asic_summary_is_served_without_a_thread_or_a_port() {
        // The board is built with no hash threads and no serial ports at
        // all: there is nothing here that could reach the wire. The summary
        // answers anyway, while the command that does need the chain fails
        // on the same board -- so this is not passing by accident.
        let config = Bzm2RuntimeConfig {
            // Opt-in, so a test that does not ask for stored tuning does not get it.
            stored_calibration: None,
            heartbeat: Default::default(),
            serial_paths: Vec::new(),
            baud_rate: DEFAULT_BAUD_RATE,
            timestamp_count: crate::asic::bzm2::protocol::DEFAULT_TIMESTAMP_COUNT,
            nonce_gap: crate::asic::bzm2::protocol::DEFAULT_NONCE_GAP,
            result_min_difficulty: None,
            dispatch_interval: Duration::from_millis(50),
            nominal_hashrate_ths: TEST_NOMINAL_HASHRATE_THS,
            dts_vs_generation: DtsVsGeneration::Gen2,
            telemetry: Bzm2TelemetryConfig::default(),
            calibration: Bzm2CalibrationConfig::default(),
            enumeration: Bzm2EnumerationConfig::default(),
            bringup: Bzm2BringupConfig::default(),
        };
        let (telemetry_tx, _telemetry_rx) = watch::channel(telemetry_state(
            &[
                ("ttyUSB0-asic-0-dts", Some(64.0)),
                ("ttyUSB0-asic-1-dts", Some(66.0)),
            ],
            &[("ttyUSB0-asic-0-vs-ch0", Some(0.80))],
            &[true],
        ));
        let (command_tx, command_rx) = tokio::sync::mpsc::channel(16);
        let mut board = Bzm2Board::new(config, telemetry_tx, command_rx);
        *board.bus_layouts.lock().unwrap() = vec![bus("/dev/ttyUSB0", 0, 2)];
        assert!(
            board.serial_controls.is_empty() && board.shutdown_handles.is_empty(),
            "the board under test holds no port and no thread"
        );
        board.spawn_command_loop();

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        command_tx
            .send(BoardCommand::QueryBzm2AsicSummary { reply: reply_tx })
            .await
            .unwrap();
        let summary = tokio::time::timeout(Duration::from_secs(5), reply_rx)
            .await
            .expect("the summary must not wait on anything")
            .unwrap()
            .unwrap();

        assert_eq!(summary.board.coverage.asics_configured, 2);
        assert_eq!(summary.board.coverage.asics_reporting, 2);
        assert_stats(
            summary.board.die_temperature_c.as_ref().unwrap(),
            64.0,
            66.0,
            65.0,
            2.0,
            2,
        );
        // The cutoff the handler actually applies, written out rather than
        // read back off the constant the handler passed in -- a test that
        // asks the code what it did agrees with whatever it did.
        assert_close(
            summary.board.coverage.reading_age.max_age_secs,
            30.0,
            "the age cutoff the handler applies",
        );

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        command_tx
            .send(BoardCommand::QueryBzm2DtsVs {
                thread_index: 0,
                asic: 0,
                reply: reply_tx,
            })
            .await
            .unwrap();
        let wire_query = tokio::time::timeout(Duration::from_secs(5), reply_rx)
            .await
            .unwrap()
            .unwrap();
        assert!(
            wire_query.is_err(),
            "a command that needs the chain must fail on a board with no thread"
        );
    }

    #[test]
    fn an_unmeasurable_field_is_null_on_the_wire_rather_than_missing() {
        // The difference this response is built around has to survive
        // serialisation: an absent key reads as "not applicable to this
        // board", a null reads as "we looked and there is nothing there".
        // Only the second is true of these fields, so neither may be
        // skipped.
        let summary = summary_of(
            &telemetry_state(&[("ttyUSB0-asic-0-dts", Some(70.0))], &[], &[true]),
            &[bus("/dev/ttyUSB0", 0, 1)],
            0,
            DtsVsGeneration::Gen2,
        );
        let json = serde_json::to_string(&summary).unwrap();

        for expected in [
            r#""asics_stale":null"#,
            r#""bits":null"#,
            r#""oldest_included_secs":null"#,
            r#""availability":"not_retained""#,
            r#""availability":"unavailable_on_platform""#,
        ] {
            assert!(json.contains(expected), "{expected} missing from {json}");
        }
        // And a channel nobody reported is a present channel with null
        // statistics, not a channel that vanished from the list.
        assert_eq!(
            json.matches(r#""stats":null"#).count(),
            // three channels on gen2, times the board block and one bus
            6
        );
    }

    #[test]
    fn a_bus_never_excludes_a_reading_the_board_block_averages_in() {
        // Bus 0's readings carry observation times, bus 1's do not -- the
        // shape the staleness path will have on the first day it is half
        // wired up. Deciding availability per bus made the response
        // contradict itself: bus 0 dropped a ten-minute-old 120 C reading
        // and reported 60 C, while the board block, unable to age-filter
        // because bus 1 carried no times, averaged that same dead die back
        // in at 80 C and named it the hottest on the board. One ASIC, two
        // answers, nothing in the response to say which.
        let bus0 = vec![
            with_age(
                observation(0, 0, 0, SensorSlot::Value(120.0), &[]),
                Duration::from_secs(600),
            ),
            with_age(
                observation(0, 1, 1, SensorSlot::Value(60.0), &[]),
                Duration::from_secs(1),
            ),
        ];
        let bus1 = vec![observation(1, 2, 0, SensorSlot::Value(60.0), &[])];
        let mut every = bus0.clone();
        every.extend(bus1.clone());

        let all: Vec<&Bzm2AsicObservation> = every.iter().collect();
        let ages = ages_are_measured(&all);
        let faults = faults_are_measured(&all);
        assert!(
            !ages,
            "a board one of whose buses carries no observation times cannot age-filter"
        );

        let board = aggregate_asics(&all, 0, MAX_AGE, ages, faults);
        let bus0_block =
            aggregate_asics(&bus0.iter().collect::<Vec<_>>(), 0, MAX_AGE, ages, faults);
        let bus1_block =
            aggregate_asics(&bus1.iter().collect::<Vec<_>>(), 0, MAX_AGE, ages, faults);

        for (name, block) in [
            ("board", &board),
            ("bus 0", &bus0_block),
            ("bus 1", &bus1_block),
        ] {
            assert_eq!(
                block.coverage.asics_stale, None,
                "{name}: no block may claim a staleness count the board could not take"
            );
            assert_eq!(
                block.coverage.reading_age.availability,
                Bzm2MeasurementAvailability::NotRetained,
                "{name}: availability"
            );
        }
        assert_eq!(
            board.coverage.asics_reporting,
            bus0_block.coverage.asics_reporting + bus1_block.coverage.asics_reporting,
            "every ASIC the board counted must be counted by exactly one bus"
        );
        // 120 C is in or out of both blocks, never one.
        assert_close(
            bus0_block.die_temperature_c.as_ref().unwrap().max,
            120.0,
            "bus 0 max",
        );
        assert_close(
            board.die_temperature_c.as_ref().unwrap().max,
            120.0,
            "board max",
        );
        assert_eq!(board.hottest.as_ref().unwrap().asic_id, 0);
    }

    #[test]
    fn a_bus_never_counts_faults_the_board_block_cannot() {
        // The same contradiction in the other retained-state gap: bus 0's
        // ASICs carry fault bits and bus 1's do not. A per-bus verdict let
        // bus 0 publish "one thermal trip" while the board published null,
        // and a consumer summing the buses would have had a count the board
        // says does not exist.
        let bus0 = vec![with_faults(
            observation(0, 0, 0, SensorSlot::Value(70.0), &[]),
            AsicFaultBits {
                thermal_trip: true,
                ..Default::default()
            },
        )];
        let bus1 = vec![observation(1, 1, 0, SensorSlot::Value(70.0), &[])];
        let mut every = bus0.clone();
        every.extend(bus1.clone());

        let all: Vec<&Bzm2AsicObservation> = every.iter().collect();
        let ages = ages_are_measured(&all);
        let faults = faults_are_measured(&all);
        assert!(!faults, "half a board's fault bits is not the board's");

        for (name, set) in [("board", &every), ("bus 0", &bus0)] {
            let block = aggregate_asics(&set.iter().collect::<Vec<_>>(), 0, MAX_AGE, ages, faults);
            assert_eq!(
                block.faults.availability,
                Bzm2MeasurementAvailability::NotRetained,
                "{name}: availability"
            );
            assert!(
                block.faults.bits.is_none(),
                "{name}: no block may publish counts the board could not take"
            );
        }
    }

    #[test]
    fn a_chain_that_enumerated_short_says_so_beside_its_coverage() {
        // 100 ASICs configured; startup enumeration found 40 and the layout
        // -- and therefore every coverage block in the response -- is
        // denominated in those 40. Coverage alone reads 40 reporting of 40
        // configured: a full, healthy chain, with sixty ASICs missing.
        let temperatures: Vec<(String, Option<f32>)> = (0..40u8)
            .map(|id| (format!("ttyUSB0-asic-{id}-dts"), Some(70.0)))
            .collect();
        let borrowed: Vec<(&str, Option<f32>)> = temperatures
            .iter()
            .map(|(name, value)| (name.as_str(), *value))
            .collect();
        let state = telemetry_state(&borrowed, &[], &[true]);
        let layouts = vec![bus("/dev/ttyUSB0", 0, 40)];

        let summary = build_asic_summary(
            &state,
            &layouts,
            &[100],
            true,
            0,
            DtsVsGeneration::Gen2,
            MAX_AGE,
        );

        assert_eq!(
            summary.board.coverage.asics_configured, 40,
            "coverage is denominated in the chain the threads were handed"
        );
        assert_eq!(
            summary.board.coverage.asics_reporting, 40,
            "and by that denominator the chain looks whole"
        );
        assert_eq!(
            summary.configured_asics, 100,
            "the configured count is what says otherwise, and it is not a measurement"
        );
        assert_eq!(
            summary.discovered_asics,
            Some(40),
            "enumeration ran, so what it found is reportable"
        );
    }

    #[test]
    fn nobody_asked_the_hardware_is_not_the_same_as_nothing_was_missing() {
        let state = telemetry_state(&[], &[], &[true]);
        let layouts = vec![bus("/dev/ttyUSB0", 0, 4)];

        let summary = build_asic_summary(
            &state,
            &layouts,
            &[4],
            false,
            0,
            DtsVsGeneration::Gen2,
            MAX_AGE,
        );

        assert_eq!(summary.configured_asics, 4);
        assert_eq!(
            summary.discovered_asics, None,
            "enumeration disabled means nobody counted, not that the count agreed"
        );
    }

    #[test]
    fn every_coverage_block_is_denominated_in_the_resolved_layout() {
        // The relationship between the three ASIC counts in this response,
        // asserted rather than described. If the layout ever stops being the
        // denominator -- or the buses stop summing to the board -- this
        // fails here instead of being discovered on a dashboard.
        type LayoutCase = (&'static str, bool, Vec<u16>, Vec<Bzm2BusLayout>);
        let cases: Vec<LayoutCase> = vec![
            (
                "enumeration off",
                false,
                vec![4, 6],
                vec![bus("/dev/ttyUSB0", 0, 4), bus("/dev/ttyUSB1", 4, 6)],
            ),
            (
                "enumeration found everything",
                true,
                vec![4, 6],
                vec![bus("/dev/ttyUSB0", 0, 4), bus("/dev/ttyUSB1", 4, 6)],
            ),
            (
                "enumeration came up short",
                true,
                vec![100, 100],
                vec![bus("/dev/ttyUSB0", 0, 40), bus("/dev/ttyUSB1", 40, 100)],
            ),
        ];

        for (name, enumeration_enabled, configured, layouts) in cases {
            let summary = build_asic_summary(
                &telemetry_state(&[], &[], &[]),
                &layouts,
                &configured,
                enumeration_enabled,
                0,
                DtsVsGeneration::Gen2,
                MAX_AGE,
            );

            let bus_total: u16 = summary
                .buses
                .iter()
                .map(|entry| entry.aggregate.coverage.asics_configured)
                .sum();
            assert_eq!(
                bus_total, summary.board.coverage.asics_configured,
                "{name}: the board's chain is the sum of its buses' chains"
            );
            assert_eq!(
                summary.board.coverage.asics_configured,
                summary.discovered_asics.unwrap_or(summary.configured_asics),
                "{name}: coverage is denominated in the resolved layout"
            );
        }
    }

    #[test]
    fn a_board_that_resolved_no_bus_is_not_a_board_with_nothing_wrong() {
        // No layout at all: every coverage count is zero, the partition
        // 0+0+0+0 == 0 holds, and the whole aggregate block is null. Read on
        // its own that is a clean bill of health for a board configured with
        // eight ASICs. `configured_asics` is the only field that says a
        // chain was expected here at all, and `buses: []` is the only field
        // that says none was found -- so neither may be omitted from the
        // wire when empty.
        let summary = build_asic_summary(
            &telemetry_state(&[], &[], &[]),
            &[],
            &[8],
            false,
            0,
            DtsVsGeneration::Gen2,
            MAX_AGE,
        );

        assert_eq!(summary.board.coverage.asics_configured, 0);
        assert_eq!(summary.board.coverage.asics_reporting, 0);
        assert_eq!(
            summary.configured_asics, 8,
            "the board was configured for a chain it did not resolve"
        );
        let json = serde_json::to_string(&summary).unwrap();
        assert!(
            json.contains(r#""buses":[]"#),
            "buses must survive as []: {json}"
        );
        assert!(json.contains(r#""configured_asics":8"#), "{json}");
    }

    #[test]
    fn a_figure_says_how_many_of_its_sensors_were_gated() {
        // Four gen2 ASICs, all four answering on all three rails; the die
        // temperature gate fired on two of them. Per-ASIC coverage is right
        // that all four are reporting -- they are talking, and their rails
        // are measured -- so coverage alone reads as a whole, healthy chain
        // while the board's die temperature rests on half of it.
        let asics: Vec<_> = (0..4u16)
            .map(|id| {
                observation(
                    0,
                    id,
                    id as u8,
                    if id < 2 {
                        SensorSlot::Value(70.0)
                    } else {
                        SensorSlot::Suppressed
                    },
                    &[
                        SensorSlot::Value(0.80),
                        SensorSlot::Value(0.80),
                        SensorSlot::Value(0.80),
                    ],
                )
            })
            .collect();

        let aggregate = aggregate_of(&asics, GEN2_CHANNELS, MAX_AGE);

        assert_eq!(aggregate.coverage.asics_reporting, 4);
        assert_eq!(
            aggregate.coverage.asics_value_suppressed, 0,
            "an ASIC still answering on its rails has not gone quiet"
        );
        let die = aggregate.die_temperature_c.as_ref().unwrap();
        assert_eq!(die.samples, 2);
        assert_eq!(
            die.asics_value_suppressed, 2,
            "the figure must carry its own denominator: 70 C is the mean of \
             two of the four ASICs coverage calls healthy"
        );
        for channel in &aggregate.voltage_channels {
            let stats = channel.stats.as_ref().unwrap();
            assert_eq!(stats.samples, 4);
            assert_eq!(
                stats.asics_value_suppressed, 0,
                "a measured zero: every rail answered"
            );
        }
        // No figure may claim more ASICs behind it than the coverage block
        // admits are reporting at all.
        for (name, stats) in [("die temperature", die)].into_iter().chain(
            aggregate
                .voltage_channels
                .iter()
                .map(|channel| ("rail", channel.stats.as_ref().unwrap())),
        ) {
            assert!(
                u32::from(stats.samples) + u32::from(stats.asics_value_suppressed)
                    <= u32::from(aggregate.coverage.asics_reporting),
                "{name}: a figure cannot rest on more ASICs than are reporting"
            );
        }
    }

    #[test]
    fn a_figure_with_no_sensor_at_all_is_not_a_gated_one() {
        // gen1 publishes no die temperature: those ASICs have no row, which
        // is a different absence from a row that arrived empty. The one
        // voltage channel they do publish is gated on one of the two.
        let state = telemetry_state(
            &[],
            &[
                ("ttyUSB0-asic-0-vs", Some(0.80)),
                ("ttyUSB0-asic-1-vs", None),
            ],
            &[true],
        );
        let layouts = vec![bus("/dev/ttyUSB0", 0, 2)];

        let summary = summary_of(&state, &layouts, 0, DtsVsGeneration::Gen1);

        assert!(
            summary.board.die_temperature_c.is_none(),
            "no row is no figure, not a figure over gated sensors"
        );
        let rail = summary.board.voltage_channels[0].stats.as_ref().unwrap();
        assert_eq!(rail.samples, 1);
        assert_eq!(
            rail.asics_value_suppressed, 0,
            "the two suppression counts do not double-count: this ASIC \
             published nothing at all, so it never reached the reporting set \
             this figure is denominated in"
        );
        assert_eq!(
            summary.board.coverage.asics_value_suppressed, 1,
            "it is counted once, at the ASIC level, where it belongs"
        );
    }

    // --- the two gaps, closed end to end ---------------------------------

    #[test]
    fn readings_published_the_way_the_chain_publishes_them_are_age_and_fault_measured() {
        // Everything above this point hands the aggregates observations it
        // built itself. This one goes through the real publish path -- the
        // same function the hash threads' telemetry events call -- and then
        // reads the summary back, because that is the join that was broken:
        // the values arrived, and nothing about WHEN or WHAT ELSE did.
        let (telemetry_tx, telemetry_rx) = watch::channel(BoardTelemetry {
            name: "bzm2-test".into(),
            model: "BZM2".into(),
            ..Default::default()
        });

        // Three ASICs on one bus: one quiet, one asserting a thermal trip,
        // and one talking with every value gated.
        publish(
            &telemetry_tx,
            0,
            0,
            Some(60.0),
            Some(0.70),
            AsicFaultBits::default(),
        );
        publish(
            &telemetry_tx,
            0,
            1,
            Some(90.0),
            Some(0.72),
            AsicFaultBits {
                thermal_trip: true,
                ..Default::default()
            },
        );
        publish(
            &telemetry_tx,
            0,
            2,
            None,
            None,
            AsicFaultBits {
                voltage_fault: true,
                ..Default::default()
            },
        );

        // Real elapsed time, on the same monotonic clock the summary reads:
        // an age that is always zero would satisfy "measured" while
        // measuring nothing.
        let slept = Duration::from_millis(20);
        std::thread::sleep(slept);

        let layouts = vec![bus("/dev/ttyUSB0", 0, 3)];
        let summary = summary_of(
            &telemetry_rx.borrow().clone(),
            &layouts,
            0,
            DtsVsGeneration::Gen2,
        );
        let coverage = &summary.board.coverage;

        assert_eq!(
            coverage.reading_age.availability,
            Bzm2MeasurementAvailability::Measured,
            "the ages are real now, and the response must stop saying they are not"
        );
        assert_eq!(
            coverage.asics_stale,
            Some(0),
            "measured, and none of them older than the cutoff -- which is a different \
             statement from null"
        );
        let oldest = coverage
            .reading_age
            .oldest_included_secs
            .expect("an included reading has an age");
        assert!(
            oldest >= slept.as_secs_f32(),
            "the age must be measured from when the frame arrived, not from when the summary \
             read it back: expected at least {}s, got {oldest}s",
            slept.as_secs_f32()
        );
        assert!(
            oldest < coverage.reading_age.max_age_secs,
            "and these readings are fresh, so none of them may be excluded"
        );

        assert_eq!(coverage.asics_reporting, 2);
        assert_eq!(
            coverage.asics_value_suppressed, 1,
            "the ASIC that talked with its values gated is neither reporting nor unseen"
        );
        assert_eq!(coverage.asics_never_seen, 0);

        let faults = &summary.board.faults;
        assert_eq!(
            faults.availability,
            Bzm2MeasurementAvailability::Measured,
            "the fault bits reach board state now"
        );
        let bits = faults.bits.as_ref().expect("measured means a row per bit");
        let trip = bits
            .iter()
            .find(|count| count.bit == Bzm2FaultBit::ThermalTrip)
            .expect("thermal_trip is one of the four");
        assert_eq!(trip.asics_asserting, 1);
        assert_eq!(
            trip.asic_ids,
            vec![1],
            "named, not just counted: an operator has to know which die to look at"
        );
        let voltage = bits
            .iter()
            .find(|count| count.bit == Bzm2FaultBit::VoltageFault)
            .expect("voltage_fault is one of the four");
        assert_eq!(
            voltage.asics_asserting, 0,
            "the suppressed ASIC's bits are not counted as currently asserted -- its readings are \
             not in the reporting set these figures are denominated in. Its suppression is what \
             this response says about it, and it is counted above."
        );
    }

    #[test]
    fn a_chain_that_has_published_nothing_still_refuses_to_claim_zero() {
        // The gap-closing must not turn "nothing has spoken" into a clean
        // bill of health: a fault count of zero that nothing measured is the
        // most dangerous number this endpoint can print.
        let state = BoardTelemetry {
            name: "bzm2-test".into(),
            model: "BZM2".into(),
            ..Default::default()
        };
        let layouts = vec![bus("/dev/ttyUSB0", 0, 3)];

        let summary = summary_of(&state, &layouts, 0, DtsVsGeneration::Gen2);

        assert_eq!(
            summary.board.coverage.reading_age.availability,
            Bzm2MeasurementAvailability::NotRetained
        );
        assert_eq!(summary.board.coverage.asics_stale, None);
        assert_eq!(
            summary.board.faults.availability,
            Bzm2MeasurementAvailability::NotRetained
        );
        assert!(summary.board.faults.bits.is_none());
        assert_eq!(summary.board.coverage.asics_never_seen, 3);
    }

    /// Publish one ASIC's frame the way a hash thread publishes it: the die
    /// temperature and all three rails under the names the publisher spells,
    /// plus the observation that says when they arrived and what the device
    /// reported alongside them.
    fn publish(
        telemetry_tx: &watch::Sender<BoardTelemetry>,
        thread_index: usize,
        asic_id: u8,
        temperature_c: Option<f32>,
        voltage_v: Option<f32>,
        faults: AsicFaultBits,
    ) {
        publish_thread_telemetry(
            telemetry_tx,
            thread_index,
            &HashThreadTelemetryUpdate {
                temperatures: vec![HashThreadTemperatureReading {
                    name: format!("ttyUSB{thread_index}-asic-{asic_id}-dts"),
                    temperature_c,
                }],
                powers: (0..3)
                    .map(|channel| HashThreadPowerReading {
                        name: format!("ttyUSB{thread_index}-asic-{asic_id}-vs-ch{channel}"),
                        voltage_v,
                        current_a: None,
                        power_w: None,
                    })
                    .collect(),
                asic: Some(HashThreadAsicObservation {
                    asic_id,
                    observed_at: Instant::now(),
                    faults: Some(faults),
                }),
            },
        );
    }

    // --- helpers ---------------------------------------------------------

    fn observation(
        thread_index: usize,
        asic_id: u16,
        wire_asic_id: u8,
        die_temp_c: SensorSlot,
        channel_voltages: &[SensorSlot],
    ) -> Bzm2AsicObservation {
        Bzm2AsicObservation {
            thread_index,
            asic_id,
            wire_asic_id,
            die_temp_c,
            channel_voltages: channel_voltages.to_vec(),
            age: None,
            faults: None,
        }
    }

    fn with_age(mut observation: Bzm2AsicObservation, age: Duration) -> Bzm2AsicObservation {
        observation.age = Some(age);
        observation
    }

    fn with_faults(
        mut observation: Bzm2AsicObservation,
        faults: AsicFaultBits,
    ) -> Bzm2AsicObservation {
        observation.faults = Some(faults);
        observation
    }

    /// Aggregate one set as if it were the whole board: the availability
    /// verdicts are taken over the same ASICs handed in, exactly as
    /// `build_asic_summary` takes them over every ASIC on the board.
    fn aggregate_of(
        asics: &[Bzm2AsicObservation],
        channels: usize,
        max_age: Duration,
    ) -> Bzm2AsicAggregate {
        let refs: Vec<&Bzm2AsicObservation> = asics.iter().collect();
        aggregate_asics(
            &refs,
            channels,
            max_age,
            ages_are_measured(&refs),
            faults_are_measured(&refs),
        )
    }

    /// Build a whole summary for a board whose configured topology is the
    /// one it resolved -- startup enumeration off, which is the default and
    /// the case every test that does not say otherwise is about.
    fn summary_of(
        state: &BoardTelemetry,
        layouts: &[Bzm2BusLayout],
        start_id: u8,
        generation: DtsVsGeneration,
    ) -> Bzm2AsicSummaryResponse {
        let configured: Vec<u16> = layouts.iter().map(|layout| layout.asic_count).collect();
        build_asic_summary(
            state,
            layouts,
            &configured,
            false,
            start_id,
            generation,
            MAX_AGE,
        )
    }

    fn telemetry_state(
        temperatures: &[(&str, Option<f32>)],
        voltages: &[(&str, Option<f32>)],
        threads_active: &[bool],
    ) -> BoardTelemetry {
        BoardTelemetry {
            name: "bzm2-test".into(),
            model: "BZM2".into(),
            temperatures: temperatures
                .iter()
                .map(|(name, value)| TemperatureSensor {
                    name: (*name).to_owned(),
                    temperature: value.map(Temperature::from_celsius),
                    observed_at: Some(std::time::Instant::now()),
                })
                .collect(),
            powers: voltages
                .iter()
                .map(|(name, value)| PowerMeasurement {
                    name: (*name).to_owned(),
                    voltage_v: *value,
                    current_a: None,
                    power_w: None,
                })
                .collect(),
            threads: threads_active
                .iter()
                .enumerate()
                .map(|(index, is_active)| ThreadTelemetry {
                    name: format!("BZM2 UART {index}"),
                    hashrate: 0,
                    is_active: *is_active,
                })
                .collect(),
            ..Default::default()
        }
    }

    #[track_caller]
    fn assert_stats(
        stats: &Bzm2ReadingStats,
        min: f32,
        max: f32,
        mean: f32,
        spread: f32,
        samples: u16,
    ) {
        assert_close(stats.min, min, "min");
        assert_close(stats.max, max, "max");
        assert_close(stats.mean, mean, "mean");
        assert_close(stats.spread, spread, "spread");
        assert_eq!(stats.samples, samples, "samples");
    }

    #[track_caller]
    fn assert_close(actual: f32, expected: f32, what: &str) {
        assert!(
            (actual - expected).abs() < 1e-4,
            "{what}: expected {expected}, got {actual}"
        );
    }
}
