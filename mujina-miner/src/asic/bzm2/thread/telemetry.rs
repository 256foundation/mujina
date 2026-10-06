use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::api_client::types::AsicFaultBits;
use crate::asic::hash_thread::{
    HashThreadAsicObservation, HashThreadEvent, HashThreadPowerReading, HashThreadStatus,
    HashThreadTelemetryUpdate, HashThreadTemperatureReading, TelemetryCoalescer,
};
use crate::types::LogVerdict;

use super::super::protocol::{self, TdmDtsVsFrame};
use super::interlock::*;
use super::*;

/// The band a silicon die can physically be in. Wider than any operating
/// point by a long way, deliberately: this rejects impossible readings, it
/// does not second-guess hot ones. About 30% of the sensor's code space falls
/// inside it, so it discards most mis-parses without touching real data.
const PLAUSIBLE_DIE_MIN_C: f32 = -40.0;
const PLAUSIBLE_DIE_MAX_C: f32 = 150.0;

/// State of the per-ASIC DTS/VS sensor stream on the shared TDM path.
///
/// The stream and synchronous UART diagnostics are mutually exclusive: sensor
/// frames interleave into the TDM path and corrupt a request/response exchange.
/// Previously that conflict was resolved by *refusing* diagnostics whenever
/// streaming was on, which — once streaming is enabled by default — would mean
/// refusing them forever. It is resolved instead by suspending around the
/// diagnostic, which needs somewhere to remember what to restore.
#[derive(Debug, Default)]
pub(super) struct DtsVsStream {
    /// Whether sensor frames are currently on the TDM path.
    ///
    /// Suspending writes the TX control register's documented reset value and
    /// resuming writes the enable value, so nothing about the part's prior
    /// state has to be captured or guessed.
    pub(super) enabled: bool,
}

// Each argument is a separate piece of the actor's state the frame updates;
// bundling them would only move the list into a struct the actor unpacks.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_dts_vs_frame(
    frame: &TdmDtsVsFrame,
    config: &Bzm2ThreadConfig,
    status: &Arc<RwLock<HashThreadStatus>>,
    event_tx: &mpsc::Sender<HashThreadEvent>,
    // Optional so the frame-handling tests can exercise parsing without
    // standing up an interlock. In the actor it is always present.
    interlock: Option<&mut ThermalInterlock>,
    corroborator: &mut FaultCorroborator,
    diagnostics: &mut DtsVsDiagnostics,
    coalescer: &mut TelemetryCoalescer,
) -> bool {
    if let Some(update) = build_dts_vs_telemetry_update(frame, config) {
        // THE INTERLOCK SEES EVERY FRAME, BEFORE ANY COALESCING.
        //
        // This is the protection path and it does not go through the
        // coalescer, so a spike cannot be hidden inside a publication window.
        // It touches no shared lock and no channel -- `interlock` is a `&mut`
        // to thread-local state -- so observing it per frame costs nothing
        // that the API can contend with.
        // KEYED BY DEVICE. The interlock holds the latest reading per ASIC and
        // judges the hottest, so a cool neighbour arriving a microsecond later
        // cannot displace the part that needs work stopped. Without an id
        // there is nothing to key on, and an unattributable reading must not
        // silently become "the" temperature -- so it is skipped, and the
        // staleness window is what notices if they all stop arriving.
        if let (Some(reading), Some(observation)) =
            (update.temperatures.first(), update.asic.as_ref())
            && let (Some(t), Some(lock)) = (reading.temperature_c, interlock)
        {
            lock.observe(observation.asic_id, t);
        }

        // PUBLICATION IS BOUNDED BY TIME, NOT BY WIRE RATE.
        //
        // This previously took a blocking RwLock write on the shared status
        // and awaited a send into a 64-slot channel, once per frame. Measured
        // 2026-09-18: a write-enabled handover left the sensor stream running
        // at 13,850 frames/s, and the API on the two-core control board
        // answered NOTHING for the whole 29-second window -- no HTTP status
        // line at all, with the listener bound and the daemon healthy. The
        // same build at 454 frames/s under a dry-run policy served most
        // requests, which is why this read as an intermittent API fault.
        // Evidence: measured on hardware, one capture against another.
        let now = Instant::now();
        coalescer.observe(update, now);
        if coalescer.due(now) {
            let (batch, hottest) = coalescer.drain(now);
            // One lock acquisition per window instead of one per frame, and
            // the hottest reading in the window rather than whichever device
            // happened to report last.
            set_temperature(status, hottest);
            for update in batch {
                let _ = event_tx
                    .send(HashThreadEvent::TelemetryUpdate(update))
                    .await;
            }
        }
    }

    match frame {
        TdmDtsVsFrame::Gen1(frame) => {
            // Budgeted: one record per telemetry frame is 421,643 records in a
            // thirty-second window on a 100-device chain. See LogBudget.
            match diagnostics.telemetry_frame.admit(Instant::now()) {
                LogVerdict::Emit => trace!(
                    path = %config.serial_path,
                    asic = frame.asic,
                    voltage = frame.voltage,
                    voltage_enabled = frame.voltage_enabled,
                    thermal_tune_code = frame.thermal_tune_code,
                    thermal_validity = frame.thermal_validity,
                    thermal_enabled = frame.thermal_enabled,
                    "BZM2 DTS/VS telemetry frame"
                ),
                LogVerdict::Summarise {
                    folded,
                    span,
                    total,
                } => trace!(
                    path = %config.serial_path,
                    folded,
                    span_ms = span.as_millis(),
                    total,
                    "BZM2 DTS/VS telemetry frames; individual records folded"
                ),
                LogVerdict::Count => {}
            }
            false
        }
        TdmDtsVsFrame::Gen2(frame) => {
            // Budgeted, same reason as the Gen1 arm above.
            match diagnostics.telemetry_frame.admit(Instant::now()) {
                LogVerdict::Emit => trace!(
                    path = %config.serial_path,
                    asic = frame.asic,
                    thermal_trip = frame.thermal_trip_status,
                    thermal_fault = frame.thermal_fault,
                    voltage_fault = frame.voltage_fault,
                    voltage_shutdown = frame.voltage_shutdown_status,
                    thermal_tune_code = frame.thermal_tune_code,
                    ch0_voltage = frame.ch0_voltage,
                    ch1_voltage = frame.ch1_voltage,
                    ch2_voltage = frame.ch2_voltage,
                    "BZM2 DTS/VS gen2 telemetry frame"
                ),
                LogVerdict::Summarise {
                    folded,
                    span,
                    total,
                } => trace!(
                    path = %config.serial_path,
                    folded,
                    span_ms = span.as_millis(),
                    total,
                    "BZM2 DTS/VS gen2 telemetry frames; individual records folded"
                ),
                LogVerdict::Count => {}
            }

            // Validity gate: a fault/trip bit is only authoritative when the
            // sensor that produced it is enabled. All of the trip / fault /
            // validity / enabled bits live in a single payload byte, so a
            // stray or mis-framed frame can set one at random; acting on that
            // unconditionally lets line noise on a shared bus force a permanent,
            // unrecoverable shutdown (DoS-by-noise). The sensor-enable bits
            // reflect host configuration (see `configure_dts_vs_stream`) and are
            // set on every genuine frame, so a real over-temp still stops
            // immediately -- no debounce, no delayed protection.
            //
            // Residual risk: the DTS/VS frame carries no checksum, so a single
            // noise frame that happens to set BOTH the enable bit and a fault
            // bit in the same byte can still trip. This gate removes the far
            // more probable single-bit case; closing it fully would need a
            // protocol-level integrity field the silicon does not provide.
            // Address gate, ahead of the payload gates above.
            //
            // Membership in the configured chain, NOT an index against
            // asic_count(): that helper floors at one so the rate maths cannot
            // divide by zero, which makes it useless as a bound, and the
            // protocol reference warns that ids need not be contiguous. The
            // question is whether we have this device, not how many we have.
            //
            // A frame attributed to a device the chain does not contain did not
            // come from the chain. Measured on hardware: readings for asic 213
            // on a chain of 100 devices numbered 0-99, produced by joining a
            // stream mid-frame after a handover, alongside a die temperature
            // below ambient and a fault that never happened.
            //
            // This is a stronger gate than the enable-bit reasoning above,
            // because it needs no cooperation from the payload: the address has
            // to name a device we actually have. It does not replace those
            // checks, it precedes them.
            //
            // NO MEMBERSHIP GATE ON THE FAULT PATH. This is deliberate and it
            // is the second correction to this code in one day.
            //
            // The first version discarded out-of-chain frames outright, which
            // suppressed real trips from devices we were not configured for.
            // The second escalated instead of shutting down, which looked
            // right and was worse: `config.asic_ids` carries GLOBAL ids
            // accumulated across buses (board/bzm2/mod.rs builds it from
            // `asic_start`), while the wire reports ids LOCAL to each bus. On
            // any bus after the first, every genuine frame is "out of chain",
            // so over-temp shutdown was disabled for two of three boards and
            // the only symptom was a log line.
            //
            // Between a spurious stop and a missing stop, a spurious stop is
            // the safe failure: it is visible, recoverable, and costs a
            // restart. A missing stop costs silicon. So an asserted fault is
            // acted on wherever it claims to come from, and an address we do
            // not recognise is reported as evidence that the line or the
            // configuration is wrong -- not used as a reason to do nothing.
            //
            // The membership gate stays on the TELEMETRY path, where its worst
            // case is a dropped reading rather than a dropped trip.
            //
            // The addressing mismatch this gate used to trip over is fixed:
            // `board/bzm2/mod.rs` now builds `asic_ids` with
            // `wire_asic_ids(start_id)`, so the thread compares wire addresses
            // against wire addresses on every bus. Before that this warning
            // fired on every genuine frame on two boards of three -- which is
            // why it is budgeted: a diagnostic on the per-frame path is one
            // configuration error away from being a denial of service against
            // its own daemon.
            if !config.asic_ids.contains(&frame.asic) {
                match diagnostics.out_of_chain.admit(Instant::now()) {
                    LogVerdict::Emit => warn!(
                        path = %config.serial_path,
                        asic = frame.asic,
                        "DTS/VS frame from an address outside the configured chain. Either the \
                         line is out of sync, or the chain is misconfigured. Fault bits below \
                         are still acted on: an unrecognised address is a reason to distrust \
                         the telemetry, never a reason to skip a shutdown."
                    ),
                    LogVerdict::Summarise {
                        folded,
                        span,
                        total,
                    } => warn!(
                        path = %config.serial_path,
                        folded,
                        span_ms = span.as_millis(),
                        total,
                        "DTS/VS frames from addresses outside the configured chain continue; \
                         further individual reports are folded."
                    ),
                    LogVerdict::Count => {}
                }
            }

            // PLAUSIBILITY, before the flags are believed.
            //
            // The thermal flags share a payload byte with the top nibble of
            // the temperature code, so a byte that is garbage makes both
            // garbage together. Only about 30% of the 12-bit code space maps
            // to a temperature a die can physically be at, so demanding a
            // plausible reading discards most mis-parses on their own terms
            // rather than on a guess about framing.
            //
            // A frame whose temperature is impossible is not evidence of
            // anything, including a fault. Not tripping on it is safe because
            // the protection does not depend on it: the thermal interlock
            // refuses to dispatch when readings are absent or stale, and a
            // stream producing impossible codes is feeding it nothing. So a
            // chain that has desynced stops being given work within the
            // interlock's staleness window whether or not its fault bits are
            // believed. Two mechanisms, different failure modes, and the slow
            // one does not depend on the fast one being right.
            let plausible = gen2_temperature_is_plausible(frame);

            // Read off the same bits the telemetry update records, rather
            // than re-deriving them here: one frame must not be able to
            // produce one verdict for the shutdown path and another for
            // anything watching from outside. The enable gate is unchanged,
            // only distributed -- `enabled && (a || b)` is `(enabled && a)
            // || (enabled && b)`.
            //
            // These are the ENABLE-gated bits, before the plausibility gate
            // below, because the diagnostic depends on seeing an assertion
            // that is about to be disbelieved. What reaches board state is
            // `credible_fault_bits`, which is these bits when the frame is
            // plausible and nothing at all when it is not.
            let asserted = enabled_fault_bits(frame);
            let thermal_asserted = asserted.thermal_trip || asserted.thermal_fault;
            let voltage_asserted = asserted.voltage_fault || asserted.voltage_shutdown;

            // THE MIDPOINT, WHICH THE SILICON WILL NOT ACT ON.
            //
            // ch0 and ch1 are each a differential across their OWN stack, near
            // 353 mV, and both feed the part's own shutdown. ch2 is the
            // MIDPOINT ERROR -- top-stack VSS against bottom-stack VDD -- and
            // the voltage-sensor control register has no threshold field for
            // it. So the part takes no action on a midpoint fault however large
            // it gets, which makes a host response the WHOLE protection here
            // rather than a backstop.
            //
            // Bounded by ABSOLUTE MAGNITUDE, because a healthy part reads near
            // zero rather than near its neighbours. The obvious bound --
            // "about 353 mV like ch0 and ch1" -- is exactly backwards.
            let midpoint_asserted = midpoint_fault(frame);
            if midpoint_asserted {
                warn!(
                    path = %config.serial_path,
                    asic = frame.asic,
                    ch2_code = frame.ch2_voltage,
                    ch0_code = frame.ch0_voltage,
                    ch1_code = frame.ch1_voltage,
                    "Stack MIDPOINT imbalance. The ASIC has no threshold for this channel and \
                     will not act on it, so this is the only protection there is."
                );
            }
            let voltage_asserted = voltage_asserted || midpoint_asserted;

            if (thermal_asserted || voltage_asserted) && !plausible {
                // Budgeted. This is the record that wrote 38 MB to the control
                // board's flash in thirty seconds while the decode was mirrored,
                // and starved the HTTP API the case was being measured through.
                // The verdict below does not depend on whether we print.
                match diagnostics.implausible_fault.admit(Instant::now()) {
                    LogVerdict::Emit => warn!(
                        path = %config.serial_path,
                        asic = frame.asic,
                        tune_code = frame.thermal_tune_code,
                        "DTS/VS frame asserts a fault but reports a temperature no die can be \
                         at. Treating the frame as mis-parsed rather than tripping on it; the \
                         thermal interlock stops dispatch on its own if real readings have \
                         stopped."
                    ),
                    LogVerdict::Summarise {
                        folded,
                        span,
                        total,
                    } => warn!(
                        path = %config.serial_path,
                        folded,
                        span_ms = span.as_millis(),
                        total,
                        "DTS/VS frames continue to assert faults at impossible temperatures. \
                         The chain is very likely desynced or configured for the wrong sensor \
                         generation; individual reports are folded to keep the log from \
                         starving the daemon."
                    ),
                    LogVerdict::Count => {}
                }
                return false;
            }

            // Everything corroborates, including a reading that is already
            // over the ceiling.
            //
            // An earlier version let a plausible over-ceiling temperature trip
            // immediately, on the reasoning that the magnitude corroborates
            // the flag. Measurement killed it: noise decodes to a plausible
            // hot reading often enough to have been the only remaining path by
            // which junk stopped the miner. And the argument for the exception
            // was weak anyway — three sweeps is on the order of seventy
            // milliseconds, while the thermal time constant of a die and its
            // heatsink is seconds. Nothing is saved by hurrying, and a whole
            // class of spurious stop is removed by not.
            let corroborated =
                corroborator.observe(frame.asic, thermal_asserted || voltage_asserted);

            if (thermal_asserted || voltage_asserted) && !corroborated {
                // Budgeted: in a desync storm every frame asserts and every
                // one of them is held, so this is a per-frame record too.
                match diagnostics.corroboration_hold.admit(Instant::now()) {
                    LogVerdict::Emit => debug!(
                        path = %config.serial_path,
                        asic = frame.asic,
                        "DTS/VS frame asserts a fault; holding for corroboration"
                    ),
                    LogVerdict::Summarise {
                        folded,
                        span,
                        total,
                    } => debug!(
                        path = %config.serial_path,
                        folded,
                        span_ms = span.as_millis(),
                        total,
                        "DTS/VS fault assertions continue to be held for \
                         corroboration; individual records folded"
                    ),
                    LogVerdict::Count => {}
                }
                return false;
            }

            let thermal_shutdown = thermal_asserted;
            let voltage_shutdown = voltage_asserted;
            if thermal_shutdown || voltage_shutdown {
                // DELIBERATELY UNBUDGETED, and the only per-frame record here
                // that is. It fires at most once per thread: the next thing
                // that happens is the shutdown. Budgeting it could only ever
                // withhold the one record that explains why the miner stopped,
                // which is the opposite of what a budget is for.
                warn!(
                    path = %config.serial_path,
                    asic = frame.asic,
                    thermal_trip = frame.thermal_trip_status,
                    thermal_fault = frame.thermal_fault,
                    voltage_fault = frame.voltage_fault,
                    voltage_shutdown = frame.voltage_shutdown_status,
                    "BZM2 hardware fault reported by DTS/VS frame"
                );
                record_hardware_error(status);
                return true;
            }
            false
        }
    }
}

/// How often per-ASIC telemetry is published to the shared status and the
/// event channel, regardless of how fast frames arrive.
///
/// Two hundred milliseconds is far faster than anything reads it -- the API is
/// polled at about 1 Hz -- and slow enough that a 13.8 kHz sensor stream costs
/// five lock acquisitions a second instead of thirteen thousand.
pub(super) const TELEMETRY_PUBLISH_INTERVAL: Duration = Duration::from_millis(200);

pub(super) fn build_dts_vs_telemetry_update(
    frame: &TdmDtsVsFrame,
    config: &Bzm2ThreadConfig,
) -> Option<HashThreadTelemetryUpdate> {
    // Same address gate as the fault path. Publishing a reading for a device
    // outside the chain turns a framing fault into a plausible-looking
    // measurement, which is worse than publishing nothing: it reaches the API,
    // the dashboard and any comparison built on them, carrying no hint that it
    // is fiction.
    let asic = match frame {
        TdmDtsVsFrame::Gen1(f) => f.asic,
        TdmDtsVsFrame::Gen2(f) => f.asic,
    };
    if !config.asic_ids.contains(&asic) {
        return None;
    }

    // Taken ONCE per frame, before any of the value gates below run, and
    // carried whatever they decide. An ASIC that is talking while its sensor
    // is disabled, or is reporting a temperature no die can be at, is a
    // different fault from one that has gone quiet -- and only an arrival
    // time can tell the two apart downstream. Gating the time on the value
    // would make a chain that has desynced into a chain that has stopped.
    //
    // Monotonic: this is only ever read as "how long ago", and a wall clock
    // that steps backwards would make that negative.
    let observed_at = Instant::now();
    let prefix = sensor_prefix(&config.serial_path);
    match frame {
        TdmDtsVsFrame::Gen1(frame) => Some(HashThreadTelemetryUpdate {
            asic: Some(HashThreadAsicObservation {
                asic_id: frame.asic,
                observed_at,
                // Gen1 frames carry no fault bits at all. `None` says that,
                // where an all-false set would claim this ASIC reported four
                // healthy bits it never sent.
                faults: None,
            }),
            temperatures: Vec::new(),
            powers: vec![HashThreadPowerReading {
                name: format!("{prefix}-asic-{}-vs", frame.asic),
                voltage_v: frame
                    .voltage_enabled
                    .then(|| legacy_tune_code_to_voltage_v(frame.voltage)),
                current_a: None,
                power_w: None,
            }],
        }),
        TdmDtsVsFrame::Gen2(frame) => Some(HashThreadTelemetryUpdate {
            asic: Some(HashThreadAsicObservation {
                asic_id: frame.asic,
                observed_at,
                // The same bits the fault path acts on, through the same
                // two gates, so what a watchdog reads and what the miner did
                // about it cannot disagree. `None` where the frame was
                // judged mis-parsed: unavailable, not none asserted.
                faults: credible_fault_bits(frame),
            }),
            temperatures: vec![HashThreadTemperatureReading {
                name: format!("{prefix}-asic-{}-dts", frame.asic),
                // Publish nothing rather than something impossible. A number
                // that cannot be true is worse than no number: it reaches the
                // API, the dashboard, and every comparison built on them,
                // carrying no hint that it is fiction. We published a die
                // temperature below ambient once, from a mis-framed read.
                temperature_c: (frame.thermal_enabled && gen2_temperature_is_plausible(frame))
                    .then(|| legacy_tune_code_to_temperature_c(frame.thermal_tune_code)),
            }],
            powers: vec![
                HashThreadPowerReading {
                    name: format!("{prefix}-asic-{}-vs-ch0", frame.asic),
                    voltage_v: frame
                        .voltage_enabled
                        .then(|| legacy_tune_code_to_voltage_v(frame.ch0_voltage)),
                    current_a: None,
                    power_w: None,
                },
                HashThreadPowerReading {
                    name: format!("{prefix}-asic-{}-vs-ch1", frame.asic),
                    voltage_v: frame
                        .voltage_enabled
                        .then(|| legacy_tune_code_to_voltage_v(frame.ch1_voltage)),
                    current_a: None,
                    power_w: None,
                },
                HashThreadPowerReading {
                    name: format!("{prefix}-asic-{}-vs-ch2", frame.asic),
                    voltage_v: frame
                        .voltage_enabled
                        .then(|| legacy_tune_code_to_voltage_v(frame.ch2_voltage)),
                    current_a: None,
                    power_w: None,
                },
            ],
        }),
    }
}

/// Does this frame's die temperature decode to something a die can be at?
///
/// The one home for the plausibility band. Three readers need the same
/// answer -- the published temperature value, the shutdown path, and the
/// fault bits that reach board state -- and a band spelled out three times
/// is a band that will be widened twice.
fn gen2_temperature_is_plausible(frame: &protocol::TdmDtsVsGen2Frame) -> bool {
    frame.thermal_validity
        && (PLAUSIBLE_DIE_MIN_C..=PLAUSIBLE_DIE_MAX_C)
            .contains(&legacy_tune_code_to_temperature_c(frame.thermal_tune_code))
}

/// The fault bits a gen2 frame asserts, under the sensor-enable gate.
///
/// All of the trip / fault / validity / enable bits share one payload byte,
/// so a stray bit can assert a fault at random, and a fault bit is only
/// authoritative when the sensor that produced it is enabled. The reasoning
/// in full is at the shutdown site in `handle_dts_vs_frame`.
fn enabled_fault_bits(frame: &protocol::TdmDtsVsGen2Frame) -> AsicFaultBits {
    AsicFaultBits {
        thermal_trip: frame.thermal_enabled && frame.thermal_trip_status,
        thermal_fault: frame.thermal_enabled && frame.thermal_fault,
        voltage_fault: frame.voltage_enabled && frame.voltage_fault,
        voltage_shutdown: frame.voltage_enabled && frame.voltage_shutdown_status,
    }
}

/// The fault bits a gen2 frame credibly reports, or `None` where the frame
/// is not evidence of anything.
///
/// This is what reaches board state, and it answers the same question the
/// shutdown path answers, through the same two gates in the same order:
/// enable first, then plausibility. Recording bits this miner declined to
/// act on would tell a watchdog that a fault fired and nothing was done
/// about it -- and the watchdog would escalate, on noise, exactly the
/// outcome the plausibility gate exists to prevent.
///
/// `None` rather than a row of falses when the frame is mis-parsed: a frame
/// whose temperature is impossible is not evidence of a fault, and it is not
/// evidence of the absence of one either. An ASIC whose last frame was
/// garbage has no fault bits available, which is a different statement from
/// an ASIC reporting none asserted.
fn credible_fault_bits(frame: &protocol::TdmDtsVsGen2Frame) -> Option<AsicFaultBits> {
    gen2_temperature_is_plausible(frame).then(|| enabled_fault_bits(frame))
}

/// Canonical sensor-name prefix for a serial path.
///
/// This is the single source of truth for the naming convention: this module
/// *emits* the sensor names, so any consumer that looks one up must derive the
/// prefix the same way. Two independent copies previously disagreed on the
/// replacement character (`-` here, `_` in the board monitor), which silently
/// broke per-ASIC temperature lookup for udev `by-id` paths — those contain
/// non-alphanumerics, so the two spellings diverged, while a plain `ttyUSB0`
/// is all-alphanumeric and masked the bug entirely.
pub(crate) fn sensor_prefix(serial_path: &str) -> String {
    Path::new(serial_path)
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or(serial_path)
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}

fn legacy_tune_code_to_temperature_c(tune_code: u16) -> f32 {
    let resolution_power = 4096.0_f32;
    -293.8 + 631.8 * ((tune_code as f32) - (2048.0 / resolution_power)) / 4096.0
}

/// Largest stack-midpoint error tolerated before it is treated as a fault.
///
/// DERIVED FROM MEASUREMENT, not from a nominal. Over 542,600 real readings
/// from 100 devices, measured on hardware, the largest healthy `|ch2|`
/// was 3.5 mV and the 99.999th percentile was the same. Fifty millivolts is
/// therefore fourteen times the worst healthy value seen and about seven times
/// BELOW a single stack differential of 353 mV -- far enough above the
/// distribution never to fire on a good part, far enough below an imbalance to
/// catch one.
///
/// The nominal would have been zero, and the interesting number is the spread.
const MIDPOINT_FAULT_V: f32 = 0.050;

/// Whether this frame shows a stack-midpoint imbalance worth acting on.
///
/// Three gates, and each one is load-bearing:
///   * the voltage sensor must be ENABLED, or the reading is not a measurement;
///   * a tune code of zero is the SENTINEL for "no reading", and decodes to
///     -0.283 V -- treating it as a measurement would fire this on every
///     unarmed part, which is how a protection becomes noise;
///   * and then the magnitude.
fn midpoint_fault(frame: &protocol::TdmDtsVsGen2Frame) -> bool {
    if !frame.voltage_enabled || frame.ch2_voltage == 0 {
        return false;
    }
    legacy_tune_code_to_voltage_v(frame.ch2_voltage).abs() > MIDPOINT_FAULT_V
}

fn legacy_tune_code_to_voltage_v(tune_code: u16) -> f32 {
    let resolution_power = 16384.0_f32;
    0.4 * 0.7067 * (6.0 * (tune_code as f32) / 16384.0 - 3.0 / resolution_power - 1.0)
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::super::protocol::{TdmFrame, TdmFrameParser};
    use super::*;

    use tokio::sync::mpsc as tokio_mpsc;
    #[test]
    fn the_midpoint_bound_sits_between_healthy_and_a_stack_differential() {
        // Derived from 542,600 real readings across 100 devices: the largest
        // healthy |ch2| was 3.5 mV, and one stack differential is about 353 mV.
        // The bound must clear the first and stay well under the second, or it
        // either cries wolf on good parts or cannot see a real imbalance.
        assert!(
            MIDPOINT_FAULT_V > 0.0035 * 10.0,
            "bound must sit well above the worst healthy reading measured"
        );
        assert!(
            MIDPOINT_FAULT_V < 0.353 / 3.0,
            "bound must sit well below a single stack differential"
        );
    }

    #[test]
    fn a_zero_code_is_no_reading_and_must_not_fire_the_midpoint_fault() {
        // THE TRAP. Tune code 0 decodes to -0.283 V, which is five times the
        // bound. If it counted as a measurement this would assert on every
        // part whose voltage sensor has not been armed -- turning the one
        // protection ch2 has into noise nobody reads.
        assert!(legacy_tune_code_to_voltage_v(0).abs() > MIDPOINT_FAULT_V);

        let mut frame = healthy_gen2_frame(0);
        frame.voltage_enabled = true;
        frame.ch2_voltage = 0;
        assert!(
            !midpoint_fault(&frame),
            "a zero code is absence, not a fault"
        );
    }

    #[test]
    fn an_unarmed_voltage_sensor_cannot_report_a_midpoint_fault() {
        // A reading from a sensor that is off is not a reading.
        let mut frame = healthy_gen2_frame(0);
        frame.voltage_enabled = false;
        frame.ch2_voltage = 16_000; // a large, obviously-out-of-band code
        assert!(!midpoint_fault(&frame));
    }

    #[test]
    fn a_healthy_midpoint_passes_and_a_real_imbalance_does_not() {
        let mut frame = healthy_gen2_frame(0);
        frame.voltage_enabled = true;

        // The median healthy reading from the capture.
        let healthy = (0..u16::MAX)
            .find(|&c| c != 0 && legacy_tune_code_to_voltage_v(c).abs() < 0.0012)
            .expect("a code near the healthy median exists");
        frame.ch2_voltage = healthy;
        assert!(!midpoint_fault(&frame), "a healthy part must not trip this");

        // An imbalance approaching a stack differential.
        let bad = (0..u16::MAX)
            .find(|&c| legacy_tune_code_to_voltage_v(c) > 0.20)
            .expect("a code above 0.2 V exists");
        frame.ch2_voltage = bad;
        assert!(midpoint_fault(&frame), "a real imbalance must trip this");
    }

    /// Random bytes must rarely be able to stop a miner.
    ///
    /// This is a property rather than an example, because the failure mode has
    /// no particular shape: any byte pattern that happens to set an enable bit
    /// and a fault bit can trip a thread, and there is no specific sequence to
    /// write a test against. So the test asserts a RATE over a stream of
    /// noise, which is the thing that actually matters operationally.
    ///
    /// Why a rate and not zero: a spurious stop is the safe failure and we
    /// deliberately chose it over a missed one. But a spurious stop is only
    /// safe ONCE. If noise stops the miner regularly, somebody turns the
    /// protection off, and then the next real fault is unprotected. Resilience
    /// is a safety property here, not a comfort.
    #[tokio::test]
    async fn noise_rarely_trips_a_shutdown() {
        let mut config = Bzm2ThreadConfig::new("/dev/ttyUSB0".into(), 5_000_000, 55.0);
        config.asic_ids = (0..=255).collect(); // worst case: every address in chain
        let status = Arc::new(RwLock::new(HashThreadStatus::default()));
        let (event_tx, _rx) = tokio_mpsc::channel(4096);

        // Deterministic noise, so a failure is reproducible.
        let mut seed: u64 = 0x5eed_1234_dead_beef;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };

        let mut parser = protocol::TdmFrameParser::new(protocol::DtsVsGeneration::Gen2);
        let mut frames_seen = 0usize;
        let mut trips = 0usize;
        for _ in 0..400 {
            let chunk: Vec<u8> = (0..64).map(|_| (next() & 0xff) as u8).collect();
            for f in parser.push(&chunk) {
                if let protocol::TdmFrame::DtsVs(frame) = f {
                    frames_seen += 1;
                    let mut corro = FaultCorroborator::default();
                    if handle_dts_vs_frame(
                        &frame,
                        &config,
                        &status,
                        &event_tx,
                        None,
                        &mut corro,
                        &mut DtsVsDiagnostics::default(),
                        &mut TelemetryCoalescer::new(Duration::ZERO),
                    )
                    .await
                    {
                        trips += 1;
                    }
                }
            }
        }

        // Measured per BYTE of noise, not per surviving frame. How many
        // frames noise manages to produce is itself a moving target -- the
        // framing check already suppresses most of them -- and the operator
        // question is "how often does junk on the wire stop my miner", which
        // is a rate against wire traffic.
        let noise_bytes = 400 * 64;
        assert!(
            trips <= 1,
            "noise produced {trips} shutdowns and {frames_seen} frames from {noise_bytes} bytes. \
                 A spurious stop is the safe failure and we chose it deliberately, but it is only \
                 safe ONCE: if junk stops the miner regularly somebody disables the protection, and \
                 the next real fault is unprotected. Resilience is a safety property here."
        );
        println!("noise: {noise_bytes} bytes -> {frames_seen} frames -> {trips} shutdowns");
    }

    /// A frame from asic 7 with plausible readings, asserting a voltage fault
    /// or nothing.
    fn gen2_frame(asserting: bool) -> TdmDtsVsFrame {
        TdmDtsVsFrame::Gen2(protocol::TdmDtsVsGen2Frame {
            asic: 7,
            ch0_voltage: 0,
            ch1_voltage: 0,
            ch2_voltage: 0,
            voltage_shutdown_status: false,
            voltage_enabled: true,
            thermal_tune_code: 2600,
            thermal_trip_status: false,
            thermal_fault: false,
            thermal_validity: true,
            thermal_enabled: true,
            voltage_fault: asserting,
            dll0_lock: false,
            dll1_lock: false,
            pll_lock: false,
        })
    }

    /// Feed frames through the real handler with one corroborator, `gap`
    /// apart, and return whether any of them stopped the thread.
    async fn feed(frames: &[bool], gap: Duration) -> Vec<bool> {
        let mut config = Bzm2ThreadConfig::new("/dev/ttyUSB0".into(), 5_000_000, 55.0);
        config.asic_ids = (0..100).collect();
        let status = Arc::new(RwLock::new(HashThreadStatus::default()));
        let (event_tx, _rx) = tokio_mpsc::channel(4096);
        let mut corro = FaultCorroborator::default();
        let mut out = Vec::new();
        for (i, asserting) in frames.iter().enumerate() {
            if i > 0 {
                std::thread::sleep(gap);
            }
            out.push(
                handle_dts_vs_frame(
                    &gen2_frame(*asserting),
                    &config,
                    &status,
                    &event_tx,
                    None,
                    &mut corro,
                    &mut DtsVsDiagnostics::default(),
                    &mut TelemetryCoalescer::new(Duration::ZERO),
                )
                .await,
            );
        }
        out
    }

    /// Measured on hardware: two healthy frames and one bad one from the same
    /// device, close together, stopped the chain. One fault frame is one.
    #[tokio::test]
    async fn one_fault_frame_after_healthy_ones_is_not_believed() {
        let stops = feed(&[false, false, true], Duration::from_millis(20)).await;
        assert_eq!(
            stops,
            vec![false, false, false],
            "one bad frame stopped the chain"
        );
    }

    /// A device that asserts on every frame, at the per-device period a full
    /// chain actually runs at (~151 ms), must stop the thread. Under a 250 ms
    /// window three such frames never fit, and it never did.
    #[tokio::test]
    async fn a_steady_fault_at_the_measured_frame_period_is_believed() {
        let stops = feed(&[true, true, true], Duration::from_millis(151)).await;
        assert_eq!(
            stops,
            vec![false, false, true],
            "a real, steady fault never stopped the chain"
        );
    }

    #[tokio::test]
    async fn a_healthy_frame_between_faults_starts_the_count_again() {
        let stops = feed(&[true, true, false, true, true], Duration::from_millis(5)).await;
        assert!(!stops.iter().any(|s| *s), "{stops:?}");
    }

    /// Nothing implausible may reach the telemetry surface.
    ///
    /// A number that cannot be true is worse than no number: it reaches the
    /// API, the dashboard and any comparison built on them, carrying no hint
    /// that it is fiction. We published a die temperature below ambient once.
    #[tokio::test]
    async fn noise_never_publishes_an_impossible_temperature() {
        let mut config = Bzm2ThreadConfig::new("/dev/ttyUSB0".into(), 5_000_000, 55.0);
        config.asic_ids = (0..=255).collect();
        let status = Arc::new(RwLock::new(HashThreadStatus::default()));
        let (event_tx, mut rx) = tokio_mpsc::channel(4096);

        let mut seed: u64 = 0xf00d_0bad_c0de_1111;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut parser = protocol::TdmFrameParser::new(protocol::DtsVsGeneration::Gen2);
        for _ in 0..200 {
            let chunk: Vec<u8> = (0..64).map(|_| (next() & 0xff) as u8).collect();
            for f in parser.push(&chunk) {
                if let protocol::TdmFrame::DtsVs(frame) = f {
                    let mut corro = FaultCorroborator::default();
                    let _ = handle_dts_vs_frame(
                        &frame,
                        &config,
                        &status,
                        &event_tx,
                        None,
                        &mut corro,
                        &mut DtsVsDiagnostics::default(),
                        &mut TelemetryCoalescer::new(Duration::ZERO),
                    )
                    .await;
                }
            }
        }
        drop(event_tx);

        let mut checked = 0usize;
        while let Some(ev) = rx.recv().await {
            if let HashThreadEvent::TelemetryUpdate(u) = ev {
                for t in u.temperatures {
                    if let Some(c) = t.temperature_c {
                        checked += 1;
                        assert!(
                            (PLAUSIBLE_DIE_MIN_C..=PLAUSIBLE_DIE_MAX_C).contains(&c),
                            "published {c} C for {}, which no die can be at",
                            t.name
                        );
                    }
                }
            }
        }
        assert!(checked > 0, "the test must actually examine some readings");
    }

    #[tokio::test]
    async fn fault_from_unrecognised_address_still_stops_the_thread() {
        // The configured chain carries GLOBAL ids while the wire carries ids
        // local to each bus, so on any bus after the first EVERY genuine frame
        // looks unrecognised. A membership gate on this path therefore disables
        // over-temp shutdown for real hardware. Acting on the fault regardless
        // risks a spurious stop from noise; not acting risks no stop at all.
        // The spurious stop is visible and costs a restart; the missing one
        // costs silicon.
        let mut config = Bzm2ThreadConfig::new("/dev/ttyUSB0".into(), 5_000_000, 55.0);
        config.asic_ids = vec![0, 1, 2, 3];
        let status = Arc::new(RwLock::new(HashThreadStatus::default()));
        let (event_tx, _event_rx) = tokio_mpsc::channel(4);

        // Every fault bit set, from an address the chain does not contain.
        let frame = TdmDtsVsFrame::Gen2(protocol::TdmDtsVsGen2Frame {
            asic: 213,
            ch0_voltage: 0,
            ch1_voltage: 0,
            ch2_voltage: 0,
            voltage_shutdown_status: true,
            voltage_enabled: true,
            // A plausible die temperature. Code 0 decodes to about -294 C,
            // which the plausibility gate now rejects as mis-parsed -- and
            // rightly, but it would make this test assert the wrong thing.
            thermal_tune_code: 2600,
            thermal_trip_status: true,
            thermal_fault: true,
            thermal_validity: true,
            thermal_enabled: true,
            voltage_fault: true,
            dll0_lock: false,
            dll1_lock: false,
            pll_lock: false,
        });

        let mut corro = FaultCorroborator::default();
        // These cases assert what a SUSTAINED fault does, which is the real
        // property: a device asserting on every sweep stops the thread. The
        // debounce that makes the first assertions inert has its own test.
        let mut should_shutdown = false;
        for _ in 0..FAULT_CORROBORATION_COUNT {
            should_shutdown = handle_dts_vs_frame(
                &frame,
                &config,
                &status,
                &event_tx,
                None,
                &mut corro,
                &mut DtsVsDiagnostics::default(),
                &mut TelemetryCoalescer::new(Duration::ZERO),
            )
            .await;
        }
        assert!(
            should_shutdown,
            "a fully-asserted fault must stop the thread even from an unrecognised address: \
                 between a spurious stop and a missing stop, the spurious one is the safe failure"
        );
        assert_eq!(
            status.read().unwrap().hardware_errors,
            1,
            "and it must be counted, not merely logged"
        );
    }

    #[tokio::test]
    async fn gen2_dts_vs_emits_api_telemetry() {
        // A chain that actually contains the device the frame claims to come
        // from. The default config is a single chip at id 0, and a frame from
        // id 2 on that chain is by definition not ours -- which is what the
        // address gate in handle_dts_vs_frame now rejects.
        let mut config = Bzm2ThreadConfig::new("/dev/ttyUSB0".into(), 5_000_000, 55.0);
        config.asic_ids = (0..=3).collect();
        let status = Arc::new(RwLock::new(HashThreadStatus::default()));
        let (event_tx, mut event_rx) = tokio_mpsc::channel(4);
        let frame = TdmDtsVsFrame::Gen2(protocol::TdmDtsVsGen2Frame {
            asic: 2,
            ch0_voltage: 0x1645,
            ch1_voltage: 0x04B4,
            // Near zero, as a healthy midpoint reads. This was 0x16AC, which
            // decodes to +318 mV -- a stack imbalance, not a healthy part --
            // because the fixture predated the correction that ch2 is the
            // midpoint ERROR rather than a third rail like its neighbours.
            ch2_voltage: 0x0ab6,
            voltage_shutdown_status: false,
            voltage_enabled: true,
            thermal_tune_code: 0x07A9,
            thermal_trip_status: false,
            thermal_fault: false,
            thermal_validity: true,
            thermal_enabled: true,
            voltage_fault: false,
            dll0_lock: false,
            dll1_lock: true,
            pll_lock: true,
        });

        let mut corro = FaultCorroborator::default();
        // These cases assert what a SUSTAINED fault does, which is the real
        // property: a device asserting on every sweep stops the thread. The
        // debounce that makes the first assertions inert has its own test.
        let mut should_shutdown = false;
        for _ in 0..FAULT_CORROBORATION_COUNT {
            should_shutdown = handle_dts_vs_frame(
                &frame,
                &config,
                &status,
                &event_tx,
                None,
                &mut corro,
                &mut DtsVsDiagnostics::default(),
                &mut TelemetryCoalescer::new(Duration::ZERO),
            )
            .await;
        }
        assert!(!should_shutdown);

        let update = event_rx.recv().await.unwrap();
        match update {
            HashThreadEvent::TelemetryUpdate(update) => {
                assert_eq!(update.temperatures.len(), 1);
                assert_eq!(update.temperatures[0].name, "ttyUSB0-asic-2-dts");
                let temp = update.temperatures[0].temperature_c.unwrap();
                assert!((temp - legacy_tune_code_to_temperature_c(0x07A9)).abs() < 0.01);

                assert_eq!(update.powers.len(), 3);
                assert_eq!(update.powers[0].name, "ttyUSB0-asic-2-vs-ch0");
                assert!(
                    (update.powers[0].voltage_v.unwrap() - legacy_tune_code_to_voltage_v(0x1645))
                        .abs()
                        < 0.0001
                );
                assert_eq!(update.powers[1].name, "ttyUSB0-asic-2-vs-ch1");
                assert_eq!(update.powers[2].name, "ttyUSB0-asic-2-vs-ch2");
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let snapshot = status.read().unwrap().clone();
        assert!(
            (snapshot.temperature_c.unwrap() - legacy_tune_code_to_temperature_c(0x07A9)).abs()
                < 0.01
        );
    }

    // --- what a published frame carries besides its values ---------------
    //
    // A value alone cannot be judged. These cases are about the two facts
    // that let something downstream judge it: WHEN the frame arrived, and
    // WHICH fault bits came with it.

    /// A gen2 frame with nothing wrong with it: every sensor enabled, a
    /// valid and plausible die temperature (code 0x07A9 decodes to about
    /// 8.6 C), no fault bit asserted.
    fn healthy_gen2_frame(asic: u8) -> protocol::TdmDtsVsGen2Frame {
        protocol::TdmDtsVsGen2Frame {
            asic,
            ch0_voltage: 0x1645,
            ch1_voltage: 0x1645,
            // ch2 IS NOT LIKE ITS NEIGHBOURS, and this fixture used to say it
            // was. ch0 and ch1 are each a differential across their OWN stack,
            // near 353 mV; ch2 is the MIDPOINT ERROR and reads near ZERO on a
            // healthy part. Setting all three the same encoded a model that
            // has since been corrected, and it described a device with a
            // 307 mV stack imbalance as healthy.
            //
            // 0xab6 decodes to +1.12 mV, the median of 542,600 real readings
            // from 100 devices, measured on hardware.
            ch2_voltage: 0x0ab6,
            voltage_shutdown_status: false,
            voltage_enabled: true,
            thermal_tune_code: 0x07A9,
            thermal_trip_status: false,
            thermal_fault: false,
            thermal_validity: true,
            thermal_enabled: true,
            voltage_fault: false,
            dll0_lock: true,
            dll1_lock: true,
            pll_lock: true,
        }
    }

    fn chain_of_four() -> Bzm2ThreadConfig {
        let mut config = Bzm2ThreadConfig::new("/dev/ttyUSB0".into(), 5_000_000, 55.0);
        config.asic_ids = (0..=3).collect();
        config
    }

    #[test]
    fn a_published_frame_carries_when_it_was_observed_and_what_it_reported() {
        let config = chain_of_four();
        let mut frame = healthy_gen2_frame(2);
        frame.thermal_fault = true;

        let before = Instant::now();
        let update = build_dts_vs_telemetry_update(&TdmDtsVsFrame::Gen2(frame), &config)
            .expect("a frame from a device on this chain publishes");
        let after = Instant::now();

        let observation = update
            .asic
            .expect("a per-ASIC frame must say which ASIC it came from, and when");
        assert_eq!(
            observation.asic_id, 2,
            "the WIRE id, which is the id the reading names are spelled with"
        );
        assert!(
            (before..=after).contains(&observation.observed_at),
            "the stamp must be taken as the frame is decoded: one taken when the summary reads \
                 the row back would make every reading look current"
        );
        assert_eq!(
            observation.faults,
            Some(AsicFaultBits {
                thermal_trip: false,
                thermal_fault: true,
                voltage_fault: false,
                voltage_shutdown: false,
            }),
            "each bit on its own: which fault fired decides what to do about it"
        );
    }

    #[test]
    fn an_asic_whose_values_are_gated_still_reports_that_it_spoke() {
        // The publish-side gates decide what a READING is worth. Neither of
        // them is evidence the device went quiet, and an ASIC talking while
        // it measures nothing is a different fault -- with a different fix --
        // from one that has stopped talking. Only the arrival time separates
        // them, so it must survive both gates.
        let config = chain_of_four();

        let mut sensor_off = healthy_gen2_frame(1);
        sensor_off.thermal_enabled = false;
        sensor_off.voltage_enabled = false;

        // Code 0 decodes to about -294 C, which no die can be at.
        let mut impossible = healthy_gen2_frame(1);
        impossible.thermal_tune_code = 0;

        // The two gates differ in what they can still say about the fault
        // bits, and the difference is the point: a disabled sensor is a
        // frame we believe, reporting nothing; an impossible code is a frame
        // we do not believe at all.
        for (name, frame, expected_faults) in [
            (
                "sensor disabled",
                sensor_off,
                Some(AsicFaultBits::default()),
            ),
            ("impossible code", impossible, None),
        ] {
            let update = build_dts_vs_telemetry_update(&TdmDtsVsFrame::Gen2(frame), &config)
                .expect("the device is on this chain whatever its readings say");
            assert_eq!(
                update.temperatures.len(),
                1,
                "{name}: the row still arrives"
            );
            assert!(
                update.temperatures[0].temperature_c.is_none(),
                "{name}: carrying no value, which is what a gated reading is"
            );
            let observation = update
                .asic
                .expect("the ASIC is still recorded as having spoken");
            assert_eq!(
                observation.faults, expected_faults,
                "{name}: a mis-parsed frame reports no bits at all, where a frame whose sensors \
                     are simply off reports none asserted"
            );
        }
    }

    #[tokio::test]
    async fn the_fault_bits_recorded_are_the_bits_the_shutdown_path_acts_on() {
        // The expected bits are written out here by hand; the second half of
        // each case asks the SHUTDOWN path -- which reaches its answer by its
        // own route, through the plausibility gate and the corroborator --
        // whether it agrees. Recording bits the miner did not act on, or
        // acting on bits it did not record, would give a watchdog a picture
        // of a machine that behaved differently.
        let config = chain_of_four();

        let mut thermal_trip = healthy_gen2_frame(0);
        thermal_trip.thermal_trip_status = true;

        let mut thermal_noise = healthy_gen2_frame(0);
        thermal_noise.thermal_trip_status = true;
        thermal_noise.thermal_enabled = false;

        let mut voltage_shutdown = healthy_gen2_frame(0);
        voltage_shutdown.voltage_shutdown_status = true;

        let mut voltage_noise = healthy_gen2_frame(0);
        voltage_noise.voltage_fault = true;
        voltage_noise.voltage_enabled = false;

        // Every bit asserted, on a frame that also claims a temperature no
        // die can be at: mis-parsed, and not evidence of anything.
        let mut mis_parsed = healthy_gen2_frame(0);
        mis_parsed.thermal_trip_status = true;
        mis_parsed.thermal_fault = true;
        mis_parsed.voltage_fault = true;
        mis_parsed.voltage_shutdown_status = true;
        mis_parsed.thermal_tune_code = 0;

        let cases = [
            (
                "nothing asserted",
                healthy_gen2_frame(0),
                Some(AsicFaultBits::default()),
                false,
            ),
            (
                "trip bit on an enabled thermal sensor",
                thermal_trip,
                Some(AsicFaultBits {
                    thermal_trip: true,
                    thermal_fault: false,
                    voltage_fault: false,
                    voltage_shutdown: false,
                }),
                true,
            ),
            (
                "trip bit with the thermal sensor disabled",
                thermal_noise,
                Some(AsicFaultBits::default()),
                false,
            ),
            (
                "shutdown bit on an enabled rail",
                voltage_shutdown,
                Some(AsicFaultBits {
                    thermal_trip: false,
                    thermal_fault: false,
                    voltage_fault: false,
                    voltage_shutdown: true,
                }),
                true,
            ),
            (
                "fault bit with the rail disabled",
                voltage_noise,
                Some(AsicFaultBits::default()),
                false,
            ),
            (
                "every bit set on a mis-parsed frame",
                mis_parsed,
                None,
                false,
            ),
        ];

        for (name, frame, expected_bits, expect_stop) in cases {
            let frame = TdmDtsVsFrame::Gen2(frame);
            let update = build_dts_vs_telemetry_update(&frame, &config)
                .expect("every one of these frames is from a device on this chain");
            assert_eq!(
                update.asic.expect("per-ASIC frame").faults,
                expected_bits,
                "{name}: recorded bits"
            );

            let status = Arc::new(RwLock::new(HashThreadStatus::default()));
            let (event_tx, _event_rx) = tokio_mpsc::channel(64);
            let mut corroborator = FaultCorroborator::default();
            let mut dts_vs_diagnostics = DtsVsDiagnostics::default();
            // A sustained fault, because a single assertion is deliberately
            // inert: the debounce is what makes line noise survivable.
            let mut stopped = false;
            for _ in 0..FAULT_CORROBORATION_COUNT {
                stopped = handle_dts_vs_frame(
                    &frame,
                    &config,
                    &status,
                    &event_tx,
                    None,
                    &mut corroborator,
                    &mut dts_vs_diagnostics,
                    &mut TelemetryCoalescer::new(Duration::ZERO),
                )
                .await;
            }
            assert_eq!(
                stopped, expect_stop,
                "{name}: what the shutdown path did with the same frame"
            );
            assert_eq!(
                expected_bits.is_some_and(|bits| bits.any()),
                expect_stop,
                "{name}: a recorded fault and a stopped thread are one event seen twice; if this \
                     case ever needs them to differ, the two mechanisms have parted company"
            );
        }
    }

    #[test]
    fn a_gen1_frame_reports_no_fault_bits_rather_than_none_asserted() {
        // Gen1 frames carry no fault bits at all. Publishing four false ones
        // would let a chain that cannot report a fault read exactly like a
        // chain reporting none -- the one number this must never print.
        let config = chain_of_four();
        let frame = TdmDtsVsFrame::Gen1(protocol::TdmDtsVsGen1Frame {
            asic: 1,
            voltage: 0x1645,
            voltage_enabled: true,
            thermal_tune_code: 0x7A,
            thermal_validity: true,
            thermal_enabled: true,
        });

        let update = build_dts_vs_telemetry_update(&frame, &config).expect("gen1 publishes a rail");
        let observation = update.asic.expect("and says which ASIC, and when");
        assert_eq!(observation.asic_id, 1);
        assert!(
            observation.faults.is_none(),
            "unavailable, not none asserted"
        );
    }

    #[test]
    fn a_frame_from_outside_the_chain_publishes_no_observation_either() {
        // The membership gate on the telemetry path is unchanged: an address
        // we do not have did not come from a device we have. A fresh
        // observation for a phantom ASIC would make a framing fault look
        // like a healthy chain, which is the opposite of what an arrival
        // time is for.
        let config = chain_of_four();
        let frame = TdmDtsVsFrame::Gen2(healthy_gen2_frame(213));
        assert!(build_dts_vs_telemetry_update(&frame, &config).is_none());
    }

    #[tokio::test]
    async fn gen2_dts_vs_unvalidated_trip_bit_does_not_shut_down() {
        // A single noisy DTS/VS frame whose only meaningful content is a set
        // thermal-trip bit, with the thermal sensor NOT enabled. This is line
        // noise, not a credible over-temp, and must not take the thread
        // permanently offline.
        let config = Bzm2ThreadConfig::new("/dev/null".into(), 5_000_000, 55.0);
        let status = Arc::new(RwLock::new(HashThreadStatus::default()));
        let (event_tx, _event_rx) = tokio_mpsc::channel(4);

        let mut parser = TdmFrameParser::new(protocol::DtsVsGeneration::Gen2);
        let frame = match parser
            // Trip bit in payload byte 0, wire order.
            .push(&[
                0x00,
                protocol::OPCODE_UART_DTS_VS,
                0x10,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
            ])
            .into_iter()
            .next()
        {
            Some(TdmFrame::DtsVs(frame)) => frame,
            other => panic!("expected a DTS/VS frame, got {other:?}"),
        };

        // Premise: the trip bit decoded true, but the sensor is not enabled.
        match frame {
            TdmDtsVsFrame::Gen2(gen2) => {
                assert!(gen2.thermal_trip_status);
                assert!(!gen2.thermal_enabled);
            }
            other => panic!("expected a gen2 frame, got {other:?}"),
        }

        let mut corro = FaultCorroborator::default();
        // These cases assert what a SUSTAINED fault does, which is the real
        // property: a device asserting on every sweep stops the thread. The
        // debounce that makes the first assertions inert has its own test.
        let mut should_shutdown = false;
        for _ in 0..FAULT_CORROBORATION_COUNT {
            should_shutdown = handle_dts_vs_frame(
                &frame,
                &config,
                &status,
                &event_tx,
                None,
                &mut corro,
                &mut DtsVsDiagnostics::default(),
                &mut TelemetryCoalescer::new(Duration::ZERO),
            )
            .await;
        }
        assert!(
            !should_shutdown,
            "an unvalidated trip bit from a disabled sensor must not shut the thread down"
        );
        assert_eq!(status.read().unwrap().hardware_errors, 0);
    }
}
