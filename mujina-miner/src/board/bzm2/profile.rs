//! Identity binding and validation for a persisted BZM2 operating point.
//!
//! A stored operating point is only meaningful for the board it was learned on,
//! under conditions close to the ones it was learned under. Applying one blind
//! is how a profile from a different board, a different firmware, or a
//! twenty-degree-warmer room gets replayed into silicon as though it were
//! measured — the failure is silent, because the numbers are all plausible.
//!
//! Every check here answers the same question: is this profile still describing
//! the machine in front of us? A failure is never fatal. It forces a full
//! calibration, which is slower and always correct.

use std::collections::BTreeMap;

use bitcoin::hashes::{Hash, HashEngine, sha256};
use serde::{Deserialize, Serialize};

use crate::tuning::calibration_planner::Bzm2SavedEngineTopology;

/// Firmware version a profile was written by.
///
/// A tuning change between versions can move what a given (V, f) pair actually
/// does, so a profile does not survive an upgrade.
pub(super) const FIRMWARE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Largest ambient change that still permits reusing a stored profile.
///
/// The functional voltage window slides with temperature, so a profile learned
/// in a cold room is a different operating point in a warm one. Five degrees is
/// roughly a tenth of the usable window.
pub(super) const AMBIENT_TOLERANCE_C: f32 = 5.0;

/// The tolerance above [`AMBIENT_WARM_THRESHOLD_C`].
///
/// Tightened because the headroom that absorbs an ambient error is itself
/// smaller when the board is already warm: the same five-degree slip costs more
/// of the remaining thermal budget at 30 °C than at 15 °C.
pub(super) const AMBIENT_TOLERANCE_WARM_C: f32 = 2.0;

/// Ambient above which the tighter tolerance applies, in either reading.
pub(super) const AMBIENT_WARM_THRESHOLD_C: f32 = 25.0;

/// What a stored profile is bound to.
///
/// None of these fields is a true board serial, because the part exposes no
/// unique identifier we can read. This is a composite fingerprint, and it is
/// worth being precise about what each component actually catches:
///
/// | field | catches | misses |
/// |---|---|---|
/// | `device_id` | the profile being carried to another host or port | a board swapped into the same port |
/// | `firmware_version` | a tuning change that redefines an operating point | nothing |
/// | `asics_per_bus` | a different chain topology | a like-for-like board swap |
/// | `engine_fingerprint` | a board swap, whenever the two boards differ in engine yield | a swap between two boards with identical engine maps |
///
/// The engine fingerprint is the load-bearing one and it is a heuristic, not a
/// guarantee. It digests the per-ASIC missing-engine map, which is a
/// manufacturing artefact and differs between boards in the ordinary case — but
/// two boards that both came out perfect are indistinguishable by it. A real
/// serial needs a hardware read the silicon does not offer, so this is the
/// strongest binding available rather than a sound one, and it is why an ambient
/// check runs regardless of whether the identity matched.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub(super) struct Bzm2ProfileIdentity {
    #[serde(default)]
    pub(super) device_id: String,
    #[serde(default)]
    pub(super) firmware_version: String,
    #[serde(default)]
    pub(super) asics_per_bus: Vec<u16>,
    /// `None` when engine discovery is disabled, in which case a board swap
    /// between two boards of the same shape cannot be detected at all --
    /// unless [`board_serials`] answers, which is why that field exists.
    ///
    /// [`board_serials`]: Self::board_serials
    #[serde(default)]
    pub(super) engine_fingerprint: Option<String>,
    /// Each driven board's serial number, read from its MCU's EEPROM.
    ///
    /// THE ONLY FIELD HERE THAT NAMES A PHYSICAL BOARD. `device_id` is the
    /// configured identity of the machine, `asics_per_bus` is a shape, and
    /// `engine_fingerprint` is `None` on any unit where engine discovery has
    /// not produced a usable record -- which is every run we have taken on
    /// this hardware. Without a serial, two boards of the same shape swapped
    /// between slots are indistinguishable, and a per-ASIC operating point is
    /// one board's silicon lottery: replaying board A's frequencies and
    /// voltages onto board B is replaying numbers measured from different
    /// parts.
    ///
    /// Slot position is not identity. Boards are field-replaceable.
    ///
    /// `None` per entry where the MCU did not answer or the EEPROM field is
    /// blank, which is what an unprogrammed board looks like.
    #[serde(default)]
    pub(super) board_serials: Vec<Option<String>>,
}

impl Bzm2ProfileIdentity {
    pub(super) fn new(
        device_id: &str,
        asics_per_bus: Vec<u16>,
        discovered_topology: Option<&BTreeMap<u16, Bzm2SavedEngineTopology>>,
        board_serials: Vec<Option<String>>,
    ) -> Self {
        Self {
            device_id: device_id.to_owned(),
            firmware_version: FIRMWARE_VERSION.to_owned(),
            asics_per_bus,
            engine_fingerprint: discovered_topology.map(engine_fingerprint),
            board_serials,
        }
    }

    /// Why this identity does not describe `current`, one reason per mismatch.
    ///
    /// Every mismatch is collected rather than returning on the first, because
    /// the reasons are surfaced to an operator who is trying to work out what
    /// changed. "Firmware differs" alone invites an upgrade rollback; "firmware
    /// differs and the engine map differs" says the board was swapped too.
    pub(super) fn mismatches(&self, current: &Self) -> Vec<String> {
        let mut reasons = Vec::new();
        if self.device_id != current.device_id {
            reasons.push(format!(
                "profile was written for device {} but this is {}",
                self.device_id, current.device_id
            ));
        }
        if self.firmware_version != current.firmware_version {
            reasons.push(format!(
                "profile was written by firmware {} but this is {}",
                self.firmware_version, current.firmware_version
            ));
        }
        if self.asics_per_bus != current.asics_per_bus {
            reasons.push(format!(
                "profile expects {:?} ASICs per bus but this board has {:?}",
                self.asics_per_bus, current.asics_per_bus
            ));
        }
        // Compared only when both sides were fingerprinted. A profile written
        // with discovery disabled has nothing to compare against, and inventing
        // a mismatch there would refuse every profile on such a board; the cost
        // is that this configuration cannot detect a like-for-like swap.
        if let (Some(stored), Some(current)) =
            (&self.engine_fingerprint, &current.engine_fingerprint)
            && stored != current
        {
            reasons
                .push("engine map differs from the profile's; board may have been swapped".into());
        }
        // PER SLOT, AND ONLY WHERE BOTH SIDES KNOW. A profile written before
        // serials were recorded carries none, and inventing a mismatch there
        // would refuse every existing profile on every board -- the same trap
        // the fingerprint comparison above avoids, for the same reason.
        //
        // So there are three states and they are not two: the serials match,
        // they differ, or provenance is unknown. Only the middle one is a
        // refusal. The third is reported by `unverifiable_slots` rather than
        // being silently treated as a match.
        for (slot, (stored, current)) in self
            .board_serials
            .iter()
            .zip(current.board_serials.iter())
            .enumerate()
        {
            if let (Some(stored), Some(current)) = (stored, current)
                && stored != current
            {
                reasons.push(format!(
                    "slot {slot} holds board {current} but the profile was written for \
                     board {stored}: this is a different physical board, and a per-ASIC \
                     operating point belongs to the silicon it was measured on"
                ));
            }
        }
        reasons
    }

    /// Slots whose board identity could not be confirmed either way.
    ///
    /// NOT a refusal and NOT a match. A profile with no recorded serial cannot
    /// be shown to belong to the board in the slot, and cannot be shown not
    /// to. Every profile written before serials were recorded is in this
    /// state, so refusing on it would refuse everything we have; treating it
    /// as a match is the fail-open this field exists to close. It is reported
    /// so an operator can see how much of the identity check actually ran.
    pub(super) fn unverifiable_slots(&self, current: &Self) -> Vec<usize> {
        let slots = self.board_serials.len().max(current.board_serials.len());
        (0..slots)
            .filter(|slot| {
                self.board_serials
                    .get(*slot)
                    .and_then(Option::as_ref)
                    .is_none()
                    || current
                        .board_serials
                        .get(*slot)
                        .and_then(Option::as_ref)
                        .is_none()
            })
            .collect()
    }
}

/// Whether a stored ambient is close enough to the current one to reuse.
///
/// Returns the reason it is not, or `None` if it is. An absent reading on
/// either side is a refusal: an unknown ambient is not a matching one, and
/// treating it as one is how a profile gets replayed into a room nobody
/// measured.
pub(super) fn ambient_mismatch(stored_c: Option<f32>, current_c: Option<f32>) -> Option<String> {
    let (Some(stored_c), Some(current_c)) = (stored_c, current_c) else {
        return Some("ambient temperature unknown at either write or load".into());
    };
    let tolerance = ambient_tolerance_c(stored_c, current_c);
    let delta = (stored_c - current_c).abs();
    (delta > tolerance).then(|| {
        format!(
            "ambient moved {delta:.1}C since the profile was written \
             ({stored_c:.1}C -> {current_c:.1}C), tolerance {tolerance:.1}C"
        )
    })
}

/// Digest of the payload a checksum covers, as lowercase hex.
///
/// The caller passes the profile serialised with its own checksum field
/// cleared, so the digest is over everything the profile asserts and not over
/// itself.
pub(super) fn checksum_of(payload: &str) -> String {
    let mut engine = sha256::HashEngine::default();
    engine.input(payload.as_bytes());
    sha256::Hash::from_engine(engine).to_string()
}

/// Digest of the per-ASIC engine map.
///
/// Built from a `BTreeMap` and sorted coordinates so the same physical board
/// always digests the same way regardless of discovery order.
fn engine_fingerprint(topology: &BTreeMap<u16, Bzm2SavedEngineTopology>) -> String {
    let mut engine = sha256::HashEngine::default();
    for (asic_id, entry) in topology {
        engine.input(&asic_id.to_le_bytes());
        engine.input(&entry.active_engine_count.to_le_bytes());
        let mut missing = entry
            .missing_engines
            .iter()
            .map(|coord| (coord.row, coord.col))
            .collect::<Vec<_>>();
        missing.sort_unstable();
        for (row, col) in missing {
            engine.input(&[row, col]);
        }
    }
    sha256::Hash::from_engine(engine).to_string()
}

fn ambient_tolerance_c(stored_c: f32, current_c: f32) -> f32 {
    if stored_c > AMBIENT_WARM_THRESHOLD_C || current_c > AMBIENT_WARM_THRESHOLD_C {
        AMBIENT_TOLERANCE_WARM_C
    } else {
        AMBIENT_TOLERANCE_C
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ident(serials: Vec<Option<&str>>) -> Bzm2ProfileIdentity {
        Bzm2ProfileIdentity {
            device_id: "bzm2".into(),
            firmware_version: "x".into(),
            asics_per_bus: vec![100, 100, 100],
            engine_fingerprint: None,
            board_serials: serials.into_iter().map(|s| s.map(str::to_string)).collect(),
        }
    }

    /// SLOT POSITION IS NOT IDENTITY. Boards are field-replaceable and a
    /// per-ASIC operating point is one board's silicon lottery, so replaying
    /// board A's frequencies onto board B is replaying numbers measured from
    /// different parts.
    ///
    /// Before this, the only field that could have caught a swap was
    /// `engine_fingerprint`, and it is `None` on every run we have taken --
    /// engine discovery has never produced a usable record on this hardware.
    /// So a like-for-like swap was undetectable in practice.
    #[test]
    fn a_swapped_board_is_a_refusal() {
        let stored = ident(vec![Some("SN-A"), Some("SN-B"), Some("SN-C")]);
        let current = ident(vec![Some("SN-A"), Some("SN-ZZ"), Some("SN-C")]);
        let reasons = stored.mismatches(&current);
        assert_eq!(reasons.len(), 1, "{reasons:?}");
        assert!(reasons[0].contains("slot 1"), "{}", reasons[0]);
        assert!(reasons[0].contains("SN-ZZ"), "{}", reasons[0]);
        assert!(reasons[0].contains("SN-B"), "{}", reasons[0]);
    }

    #[test]
    fn the_same_boards_in_the_same_slots_are_no_refusal() {
        let stored = ident(vec![Some("SN-A"), Some("SN-B")]);
        assert!(stored.mismatches(&stored).is_empty());
    }

    /// THREE STATES, NOT TWO. A profile written before serials were recorded
    /// has none. Refusing on that would refuse every profile we already hold;
    /// treating it as a match is the fail-open the field exists to close. So
    /// it is neither -- it is reported as unverifiable.
    #[test]
    fn an_unrecorded_serial_is_unverifiable_not_a_mismatch() {
        let stored = ident(vec![None, None]);
        let current = ident(vec![Some("SN-A"), Some("SN-B")]);
        assert!(
            stored.mismatches(&current).is_empty(),
            "a profile with no recorded serial must not refuse every board"
        );
        assert_eq!(
            stored.unverifiable_slots(&current),
            vec![0, 1],
            "but it must be REPORTED as unchecked, not counted as a match"
        );
    }

    /// A board whose MCU did not answer is unverifiable on that slot alone --
    /// the slots that did answer are still checked.
    #[test]
    fn one_silent_mcu_does_not_disable_the_whole_identity_check() {
        let stored = ident(vec![Some("SN-A"), Some("SN-B")]);
        let current = ident(vec![Some("SN-A"), None]);
        assert!(stored.mismatches(&current).is_empty());
        assert_eq!(stored.unverifiable_slots(&current), vec![1]);
        // ...and slot 0 still catches a swap.
        let swapped = ident(vec![Some("SN-X"), None]);
        assert_eq!(stored.mismatches(&swapped).len(), 1);
    }
    use crate::tuning::calibration_planner::Bzm2SavedEngineCoordinate;

    fn topology(missing: &[(u8, u8)]) -> BTreeMap<u16, Bzm2SavedEngineTopology> {
        BTreeMap::from([(
            0,
            Bzm2SavedEngineTopology {
                active_engine_count: 236 - missing.len() as u16,
                missing_engines: missing
                    .iter()
                    .map(|&(row, col)| Bzm2SavedEngineCoordinate { row, col })
                    .collect(),
            },
        )])
    }

    fn identity(device: &str, missing: &[(u8, u8)]) -> Bzm2ProfileIdentity {
        Bzm2ProfileIdentity::new(device, vec![4], Some(&topology(missing)), Vec::new())
    }

    #[test]
    fn identical_hardware_matches_itself() {
        let current = identity("bzm2-ttyUSB0", &[(3, 7), (5, 11)]);
        assert!(current.mismatches(&current).is_empty());
    }

    #[test]
    fn engine_fingerprint_ignores_discovery_order() {
        // The same board enumerated twice must digest the same way, or every
        // restart would look like a board swap.
        let forwards = identity("bzm2-ttyUSB0", &[(3, 7), (5, 11)]);
        let backwards = identity("bzm2-ttyUSB0", &[(5, 11), (3, 7)]);
        assert_eq!(forwards.engine_fingerprint, backwards.engine_fingerprint);
    }

    #[test]
    fn a_different_engine_map_reads_as_a_board_swap() {
        let stored = identity("bzm2-ttyUSB0", &[(3, 7), (5, 11)]);
        let current = identity("bzm2-ttyUSB0", &[(3, 7), (9, 2)]);
        let reasons = stored.mismatches(&current);
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].contains("swapped"), "got {reasons:?}");
    }

    #[test]
    fn every_mismatch_is_reported_not_just_the_first() {
        let mut stored = identity("bzm2-ttyUSB0", &[(3, 7)]);
        stored.firmware_version = "0.0.1-ancient".into();
        stored.asics_per_bus = vec![2];
        let current = identity("bzm2-ttyUSB1", &[(9, 2)]);
        assert_eq!(
            stored.mismatches(&current).len(),
            4,
            "an operator diagnosing a refusal needs the whole picture: {:?}",
            stored.mismatches(&current)
        );
    }

    #[test]
    fn an_unfingerprinted_side_cannot_report_a_swap() {
        // Discovery disabled on one side or the other: the check is skipped
        // rather than refusing every profile on such a board.
        let discovered = identity("bzm2-ttyUSB0", &[(3, 7)]);
        let blind = Bzm2ProfileIdentity::new("bzm2-ttyUSB0", vec![4], None, Vec::new());
        assert!(discovered.mismatches(&blind).is_empty());
        assert!(blind.mismatches(&discovered).is_empty());
    }

    #[test]
    fn ambient_tolerance_tightens_once_either_reading_is_warm() {
        // Cool room: four degrees of drift is fine.
        assert!(ambient_mismatch(Some(18.0), Some(22.0)).is_none());
        // Same drift with one reading above 25 C is not.
        assert!(ambient_mismatch(Some(24.0), Some(28.0)).is_some());
        // And the tighter band still passes when the drift is small.
        assert!(ambient_mismatch(Some(28.0), Some(29.5)).is_none());
        // Six degrees fails even cold.
        assert!(ambient_mismatch(Some(10.0), Some(16.5)).is_some());
    }

    #[test]
    fn an_unknown_ambient_is_a_refusal_not_a_pass() {
        assert!(ambient_mismatch(None, Some(22.0)).is_some());
        assert!(ambient_mismatch(Some(22.0), None).is_some());
        assert!(ambient_mismatch(None, None).is_some());
    }

    #[test]
    fn the_checksum_covers_the_payload() {
        assert_eq!(checksum_of("a"), checksum_of("a"));
        assert_ne!(checksum_of("a"), checksum_of("b"));
        assert_eq!(checksum_of("a").len(), 64, "sha-256 as hex");
    }
}
