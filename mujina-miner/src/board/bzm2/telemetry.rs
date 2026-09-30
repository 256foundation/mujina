//! Sensor polling and board telemetry publishing for the BZM2 board.

use tokio::sync::watch;

use crate::api_client::types::{AsicState, BoardTelemetry, PowerMeasurement, TemperatureSensor};
use crate::asic::hash_thread::{
    HashThreadAsicObservation, HashThreadStatus, HashThreadTelemetryUpdate,
};
use crate::types::Temperature;

pub(super) fn publish_thread_status(
    telemetry_tx: &watch::Sender<BoardTelemetry>,
    thread_index: usize,
    status: &HashThreadStatus,
) {
    telemetry_tx.send_modify(|state| {
        if let Some(thread) = state.threads.get_mut(thread_index) {
            thread.hashrate = status.hashrate.0;
            thread.is_active = status.is_active;
        }
    });
}

/// Merge one hash thread's readings into board state, and record that the
/// ASIC they came from spoke.
///
/// `thread_index` is not decoration: ASIC ids are local to a bus, so the row
/// this update belongs to is keyed by the pair, and the thread that produced
/// the update does not know where it sits on the board.
pub(super) fn publish_thread_telemetry(
    telemetry_tx: &watch::Sender<BoardTelemetry>,
    thread_index: usize,
    update: &HashThreadTelemetryUpdate,
) {
    // ONE STAMP PER UPDATE, taken from the ASIC observation when there is one
    // so a die's temperature row and its fault row cannot disagree about when
    // they were seen. `Instant::now()` only when the update carries no
    // observation to borrow from.
    let observed_at = Some(
        update
            .asic
            .as_ref()
            .map(|asic| asic.observed_at)
            .unwrap_or_else(std::time::Instant::now),
    );
    telemetry_tx.send_modify(|state| {
        // Merged straight from the thread's readings instead of through an
        // intermediate vector of API rows. This runs once per DTS/VS frame --
        // measured at 13,622 frames a second across a full chain, on a
        // Cortex-A9 -- and the intermediate cost a name clone per reading
        // (four on a gen2 frame) plus two vector allocations, all of them
        // freed again before the next frame arrived. A name is cloned now
        // only when its row is created, which happens once per ASIC per
        // sensor and never again.
        for reading in &update.temperatures {
            let temperature = reading.temperature_c.map(Temperature::from_celsius);
            match state
                .temperatures
                .iter_mut()
                .find(|sensor| sensor.name == reading.name)
            {
                Some(sensor) => {
                    sensor.temperature = temperature;
                    sensor.observed_at = observed_at;
                }
                None => state.temperatures.push(TemperatureSensor {
                    name: reading.name.clone(),
                    temperature,
                    observed_at,
                }),
            }
        }
        for reading in &update.powers {
            match state
                .powers
                .iter_mut()
                .find(|power| power.name == reading.name)
            {
                Some(power) => {
                    power.voltage_v = reading.voltage_v;
                    power.current_a = reading.current_a;
                    power.power_w = reading.power_w;
                }
                None => state.powers.push(PowerMeasurement {
                    name: reading.name.clone(),
                    voltage_v: reading.voltage_v,
                    current_a: reading.current_a,
                    power_w: reading.power_w,
                }),
            }
        }
        if let Some(observation) = &update.asic {
            record_asic_observation(state, thread_index, observation);
        }
    });
}

/// Record when one ASIC last spoke, and what it reported alongside its
/// readings.
///
/// Kept on the ASIC's own row rather than on each reading: the arrival time
/// and the fault bits are facts about the ASIC, and one frame delivers all
/// of them together. A time per reading would be four copies of one arrival
/// for a gen2 ASIC, and four copies of one fact are four chances to
/// disagree about when it last answered.
///
/// Recorded whatever the value gates upstream decided. Those gates judge
/// what a reading is worth; they do not judge whether the device spoke, and
/// an ASIC answering with a suppressed value is a different fault from one
/// that has gone silent. This field is what separates them: without it a
/// sensor that froze an hour ago counts exactly like one answering now.
///
/// `faults` is assigned, not merged: the latest frame is what this ASIC now
/// reports, and carrying an older frame's bits forward under a newer
/// observation time would date a fault to a frame that never carried it.
/// `None` from a generation that sends no fault bits therefore stays `None`
/// -- unavailable, which is not the same as none asserted.
fn record_asic_observation(
    state: &mut BoardTelemetry,
    thread_index: usize,
    observation: &HashThreadAsicObservation,
) {
    match asic_row_index(&state.asics, thread_index, observation.asic_id) {
        Some(index) => {
            let row = &mut state.asics[index];
            row.observed_at = Some(observation.observed_at);
            row.faults = observation.faults;
        }
        None => {
            state.asics.push(AsicState {
                id: observation.asic_id,
                thread_index: Some(thread_index),
                faults: observation.faults,
                observed_at: Some(observation.observed_at),
                ..Default::default()
            });
            sort_asic_rows(state);
        }
    }
}

/// Where one ASIC's row sits in board state, if it has one yet.
///
/// The key is the (thread index, wire id) PAIR. Ids are local to a bus, so
/// the same id on two buses names two different devices; keying on the id
/// alone would have every bus after the first overwrite the first bus's
/// rows. Both writers here and the summary reader spell the key through this
/// one function, so they cannot come to spell it differently.
fn asic_row_index(asics: &[AsicState], thread_index: usize, asic_id: u8) -> Option<usize> {
    asics
        .iter()
        .position(|asic| asic.thread_index == Some(thread_index) && asic.id == asic_id)
}

/// Keep the rows in bus-then-id order, so a reader sees the chain in the
/// order it is wired.
///
/// Called only where a row is ADDED. An update in place cannot change the
/// order, and this runs on the per-frame telemetry path: re-sorting three
/// hundred rows thousands of times a second to preserve an order that
/// already holds is the kind of cost this platform cannot absorb.
fn sort_asic_rows(state: &mut BoardTelemetry) {
    state
        .asics
        .sort_by_key(|asic| (asic.thread_index.unwrap_or(usize::MAX), asic.id));
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::api_client::types::{AsicFaultBits, ThreadTelemetry};

    #[test]
    fn publish_thread_telemetry_updates_board_state() {
        let (telemetry_tx, telemetry_rx) = watch::channel(BoardTelemetry {
            name: "bzm2-test".into(),
            model: "BZM2".into(),
            serial: Some("bzm2-test".into()),
            temperatures: vec![TemperatureSensor {
                name: "host-board-temp".into(),
                temperature: Some(Temperature::from_celsius(52.0)),
                observed_at: Some(std::time::Instant::now()),
            }],
            powers: vec![PowerMeasurement {
                name: "host-input".into(),
                voltage_v: Some(12.0),
                current_a: Some(10.0),
                power_w: Some(120.0),
            }],
            ..Default::default()
        });

        publish_thread_telemetry(
            &telemetry_tx,
            0,
            &HashThreadTelemetryUpdate {
                temperatures: vec![crate::asic::hash_thread::HashThreadTemperatureReading {
                    name: "ttyUSB0-asic-2-dts".into(),
                    temperature_c: Some(64.5),
                }],
                powers: vec![crate::asic::hash_thread::HashThreadPowerReading {
                    name: "ttyUSB0-asic-2-vs-ch0".into(),
                    voltage_v: Some(0.78),
                    current_a: None,
                    power_w: None,
                }],
                asic: None,
            },
        );

        let state = telemetry_rx.borrow().clone();
        assert_eq!(state.temperatures.len(), 2);
        assert!(
            state
                .temperatures
                .iter()
                .any(|sensor| sensor.name == "host-board-temp"
                    && sensor.temperature.map(Temperature::as_degrees_c) == Some(52.0))
        );
        assert!(
            state
                .temperatures
                .iter()
                .any(|sensor| sensor.name == "ttyUSB0-asic-2-dts"
                    && sensor.temperature.map(Temperature::as_degrees_c) == Some(64.5))
        );
        assert_eq!(state.powers.len(), 2);
        assert!(
            state
                .powers
                .iter()
                .any(|sensor| sensor.name == "host-input" && sensor.voltage_v == Some(12.0))
        );
        assert!(
            state
                .powers
                .iter()
                .any(|sensor| sensor.name == "ttyUSB0-asic-2-vs-ch0"
                    && sensor.voltage_v == Some(0.78))
        );
        assert!(
            state.asics.is_empty(),
            "an update that names no ASIC must not invent a row for one"
        );
    }

    // --- per-ASIC observation --------------------------------------------
    //
    // Two absences closed here: WHEN a reading arrived, and WHAT fault bits
    // came with it. Both are recorded on the ASIC's own row, because one
    // frame delivers them together and a fault bit with no arrival time
    // cannot be told from one asserted an hour ago.

    #[test]
    fn a_published_reading_records_when_its_asic_spoke_and_what_it_reported() {
        let (telemetry_tx, telemetry_rx) = watch::channel(board_state());
        let observed_at = Instant::now();

        publish_thread_telemetry(
            &telemetry_tx,
            1,
            &asic_update(
                "ttyUSB1-asic-3",
                Some(64.5),
                Some(0.78),
                HashThreadAsicObservation {
                    asic_id: 3,
                    observed_at,
                    faults: Some(AsicFaultBits {
                        thermal_trip: true,
                        ..Default::default()
                    }),
                },
            ),
        );

        let state = telemetry_rx.borrow().clone();
        let row = asic_row(&state, 1, 3).expect("the ASIC that spoke must have a row");
        assert_eq!(
            row.observed_at,
            Some(observed_at),
            "the row must carry the instant the frame was observed, not the instant it was read              back: an age measured from the read is always zero"
        );
        assert_eq!(
            row.faults,
            Some(AsicFaultBits {
                thermal_trip: true,
                thermal_fault: false,
                voltage_fault: false,
                voltage_shutdown: false,
            }),
            "all four bits as reported, so a watchdog can tell which fault fired"
        );
        assert!(row.faults.is_some_and(|faults| faults.any()));
    }

    #[test]
    fn an_asic_talking_with_its_values_gated_still_records_that_it_spoke() {
        // The publish-side gates suppress a VALUE -- a sensor disabled, a
        // frame marked invalid, a temperature no die can be at. None of them
        // is evidence the device went quiet, and an ASIC answering with
        // nothing usable is a different fault from one that has stopped
        // answering. Without an arrival time on a value-less row the two are
        // indistinguishable, and the fault bits that say WHICH would be lost
        // exactly when they matter.
        let (telemetry_tx, telemetry_rx) = watch::channel(board_state());
        let observed_at = Instant::now();
        let suppressed = AsicFaultBits {
            thermal_trip: true,
            thermal_fault: true,
            voltage_fault: false,
            voltage_shutdown: false,
        };

        publish_thread_telemetry(
            &telemetry_tx,
            0,
            &asic_update(
                "ttyUSB0-asic-7",
                None,
                None,
                HashThreadAsicObservation {
                    asic_id: 7,
                    observed_at,
                    faults: Some(suppressed),
                },
            ),
        );

        let state = telemetry_rx.borrow().clone();
        assert!(
            state
                .temperatures
                .iter()
                .any(|sensor| sensor.name == "ttyUSB0-asic-7-dts" && sensor.temperature.is_none()),
            "the row still arrives, carrying no value -- that is what a gated reading is"
        );
        let row = asic_row(&state, 0, 7).expect("a suppressed value is still an ASIC talking");
        assert_eq!(row.observed_at, Some(observed_at));
        assert_eq!(row.faults, Some(suppressed));
    }

    #[test]
    fn a_later_frame_replaces_the_row_rather_than_accumulating_rows() {
        let (telemetry_tx, telemetry_rx) = watch::channel(board_state());
        let first = Instant::now();
        let second = first + Duration::from_secs(5);

        publish_thread_telemetry(
            &telemetry_tx,
            0,
            &asic_update(
                "ttyUSB0-asic-2",
                Some(60.0),
                Some(0.70),
                HashThreadAsicObservation {
                    asic_id: 2,
                    observed_at: first,
                    faults: Some(AsicFaultBits {
                        thermal_trip: true,
                        ..Default::default()
                    }),
                },
            ),
        );
        publish_thread_telemetry(
            &telemetry_tx,
            0,
            &asic_update(
                "ttyUSB0-asic-2",
                Some(61.0),
                Some(0.71),
                HashThreadAsicObservation {
                    asic_id: 2,
                    observed_at: second,
                    faults: Some(AsicFaultBits::default()),
                },
            ),
        );

        let state = telemetry_rx.borrow().clone();
        assert_eq!(state.asics.len(), 1, "one ASIC, one row");
        let row = &state.asics[0];
        assert_eq!(row.observed_at, Some(second), "the newest arrival wins");
        assert_eq!(
            row.faults,
            Some(AsicFaultBits::default()),
            "the bits are this frame's, not every frame's: carrying a cleared fault forward would              date it to a frame that never asserted it"
        );
        assert_eq!(state.temperatures.len(), 1, "and one row per sensor name");
    }

    #[test]
    fn the_same_wire_id_on_two_buses_is_two_asics() {
        // ASIC ids are local to a chain: every bus addresses from the same
        // start id. Keyed by id alone, the second bus would overwrite the
        // first, and a chain that had gone quiet would read as fresh because
        // its neighbour was still talking.
        let (telemetry_tx, telemetry_rx) = watch::channel(board_state());
        let bus0_at = Instant::now();
        let bus1_at = bus0_at + Duration::from_secs(3);

        for (thread_index, prefix, observed_at, trip) in [
            (0usize, "ttyUSB0-asic-3", bus0_at, true),
            (1usize, "ttyUSB1-asic-3", bus1_at, false),
        ] {
            publish_thread_telemetry(
                &telemetry_tx,
                thread_index,
                &asic_update(
                    prefix,
                    Some(70.0),
                    Some(0.75),
                    HashThreadAsicObservation {
                        asic_id: 3,
                        observed_at,
                        faults: Some(AsicFaultBits {
                            thermal_trip: trip,
                            ..Default::default()
                        }),
                    },
                ),
            );
        }

        let state = telemetry_rx.borrow().clone();
        assert_eq!(state.asics.len(), 2, "two devices, two rows");
        assert_eq!(asic_row(&state, 0, 3).unwrap().observed_at, Some(bus0_at));
        assert_eq!(asic_row(&state, 1, 3).unwrap().observed_at, Some(bus1_at));
        assert!(asic_row(&state, 0, 3).unwrap().faults.unwrap().thermal_trip);
        assert!(!asic_row(&state, 1, 3).unwrap().faults.unwrap().thermal_trip);
    }

    #[test]
    fn rows_stay_in_bus_then_id_order_however_the_frames_arrive() {
        // The order is maintained where rows are ADDED, not on every frame:
        // an update in place cannot change it. This is the test that would
        // fail if that reasoning were wrong.
        let (telemetry_tx, telemetry_rx) = watch::channel(board_state());
        let observed_at = Instant::now();

        for (thread_index, asic_id) in [(1usize, 9u8), (0, 4), (1, 2), (0, 11)] {
            publish_thread_telemetry(
                &telemetry_tx,
                thread_index,
                &asic_update(
                    &format!("ttyUSB{thread_index}-asic-{asic_id}"),
                    Some(60.0),
                    Some(0.70),
                    HashThreadAsicObservation {
                        asic_id,
                        observed_at,
                        faults: None,
                    },
                ),
            );
        }

        let state = telemetry_rx.borrow().clone();
        let order: Vec<(usize, u8)> = state
            .asics
            .iter()
            .map(|asic| (asic.thread_index.unwrap(), asic.id))
            .collect();
        assert_eq!(order, vec![(0, 4), (0, 11), (1, 2), (1, 9)]);
    }

    /// A board with nothing published to it yet.
    fn board_state() -> BoardTelemetry {
        BoardTelemetry {
            name: "bzm2-test".into(),
            model: "BZM2".into(),
            serial: Some("bzm2-test".into()),
            ..Default::default()
        }
    }

    /// One ASIC's frame as the hash thread hands it over: a die temperature
    /// and one rail, under the names the publisher spells, plus the
    /// observation that says when they arrived.
    fn asic_update(
        sensor_prefix: &str,
        temperature_c: Option<f32>,
        voltage_v: Option<f32>,
        observation: HashThreadAsicObservation,
    ) -> HashThreadTelemetryUpdate {
        HashThreadTelemetryUpdate {
            temperatures: vec![crate::asic::hash_thread::HashThreadTemperatureReading {
                name: format!("{sensor_prefix}-dts"),
                temperature_c,
            }],
            powers: vec![crate::asic::hash_thread::HashThreadPowerReading {
                name: format!("{sensor_prefix}-vs-ch0"),
                voltage_v,
                current_a: None,
                power_w: None,
            }],
            asic: Some(observation),
        }
    }

    fn asic_row(state: &BoardTelemetry, thread_index: usize, asic_id: u8) -> Option<&AsicState> {
        asic_row_index(&state.asics, thread_index, asic_id).map(|index| &state.asics[index])
    }

    #[test]
    fn publish_thread_status_updates_state_slot() {
        let (telemetry_tx, telemetry_rx) = watch::channel(BoardTelemetry {
            name: "bzm2-test".into(),
            model: "BZM2".into(),
            serial: Some("bzm2-test".into()),
            threads: vec![ThreadTelemetry {
                name: "BZM2 UART 0".into(),
                hashrate: 0,
                is_active: false,
            }],
            ..Default::default()
        });

        let status = HashThreadStatus {
            hashrate: crate::types::HashRate::from_terahashes(42.0),
            is_active: true,
            ..Default::default()
        };

        publish_thread_status(&telemetry_tx, 0, &status);

        let state = telemetry_rx.borrow().clone();
        assert_eq!(
            state.threads[0].hashrate,
            crate::types::HashRate::from_terahashes(42.0).0
        );
        assert!(state.threads[0].is_active);
    }
}
