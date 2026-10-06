//! Reading the per-ASIC calibration a unit already holds.
//!
//! The vendor daemon does not necessarily calibrate when it starts. It stores
//! per-device tuning to disk and replays it, and on a unit that has run before,
//! replay is the normal path -- observed taking 39 seconds where a fresh
//! derivation takes up to fifteen minutes.
//!
//! That tuning is the board's own silicon lottery. Measured on one board: a
//! uniform 800 MHz at power-on becomes 1163 MHz average, with individual
//! devices spread from 1093.75 to 1275 MHz. **That spread is the calibration.**
//!
//! Two reasons to read it rather than ignore it:
//!
//! 1. **Comparison.** Running against a stack that replays stored tuning, while
//!    we run generic, compares calibration and reports it as firmware.
//! 2. **Fairness to the hardware.** Every unit in the field holds this file. A
//!    miner that cannot use the tuning already on the machine is slower for a
//!    reason that has nothing to do with its quality.
//!
//! # Format
//!
//! Established by reading a real file and cross-checking the derived mean, min
//! and max against the vendor daemon's own log line for the same board -- all
//! three agreed, so the layout below is confirmed rather than assumed.
//!
//! ```text
//! line 1        capture timestamp
//! line 2        ambient temperature, degrees C
//! line 3        rail voltage, mV
//! lines 4..203  100 devices x 2 entries each, adjacent, frequency in MHz
//! line 204      trailer, purpose not established
//! ```
//!
//! Every frequency observed is a multiple of 6.25 MHz, which is the PLL step,
//! and in the sample every device's two entries were equal. Neither is assumed
//! here: the parser accepts differing entries, because a format that permits it
//! may use it, and rejects values off the PLL grid, because those cannot be
//! programmed.

use std::io::BufRead;

/// The calibration a unit holds for one board.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredCalibration {
    /// Verbatim, unparsed. The vendor writes a non-padded local time with no
    /// zone, so anything we decoded it into would be a guess wearing a type.
    pub captured_at: String,
    /// Ambient temperature when the tuning was derived.
    ///
    /// Load-bearing, not decorative: tuning derived at one ambient need not hold
    /// at another, and a caller comparing two runs should know whether it is
    /// comparing like with like.
    pub ambient_c: f32,
    /// Rail voltage the tuning was derived at, mV. Same caveat as ambient.
    pub rail_mv: u32,
    /// Per device, the two entries in file order.
    pub devices: Vec<(f32, f32)>,
    /// The trailing value. Kept because discarding a field we cannot explain
    /// would make a rewrite lossy, and named honestly rather than guessed at.
    pub trailer: f32,
}

/// PLL step. A frequency off this grid cannot be programmed, so a file
/// containing one is not describing this part.
pub const FREQ_STEP_MHZ: f32 = 6.25;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum StoredCalibrationError {
    #[error("calibration file has {0} lines; too short to hold a header")]
    TooShort(usize),
    #[error("line {line}: expected a number, found {value:?}")]
    NotANumber { line: usize, value: String },
    #[error(
        "line {line}: {mhz} MHz is not a multiple of {FREQ_STEP_MHZ} MHz, so it cannot be programmed"
    )]
    OffPllGrid { line: usize, mhz: f32 },
    #[error("file holds {0} frequency entries, which is not two per device")]
    OddEntryCount(usize),
    #[error("device count {0} is implausible for a board")]
    ImplausibleDeviceCount(usize),
}

impl StoredCalibration {
    /// Parse a stored calibration.
    ///
    /// Strict on the things that would be dangerous to get wrong -- values off
    /// the PLL grid, an entry count that is not two per device -- and tolerant
    /// of the things we do not fully understand, which are carried through
    /// rather than dropped.
    pub fn parse<R: BufRead>(reader: R) -> Result<Self, StoredCalibrationError> {
        let lines: Vec<String> = reader
            .lines()
            .map_while(Result::ok)
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        if lines.len() < 5 {
            return Err(StoredCalibrationError::TooShort(lines.len()));
        }

        let num = |idx: usize| -> Result<f32, StoredCalibrationError> {
            lines[idx]
                .parse::<f32>()
                .map_err(|_| StoredCalibrationError::NotANumber {
                    line: idx + 1,
                    value: lines[idx].clone(),
                })
        };

        let ambient_c = num(1)?;
        let rail_mv = num(2)? as u32;
        let trailer = num(lines.len() - 1)?;

        let body = &lines[3..lines.len() - 1];
        if !body.len().is_multiple_of(2) {
            return Err(StoredCalibrationError::OddEntryCount(body.len()));
        }
        let mut freqs = Vec::with_capacity(body.len());
        for (offset, raw) in body.iter().enumerate() {
            let line = offset + 4;
            let mhz = raw
                .parse::<f32>()
                .map_err(|_| StoredCalibrationError::NotANumber {
                    line,
                    value: raw.clone(),
                })?;
            let steps = mhz / FREQ_STEP_MHZ;
            if (steps - steps.round()).abs() > 1e-3 {
                return Err(StoredCalibrationError::OffPllGrid { line, mhz });
            }
            freqs.push(mhz);
        }

        let devices: Vec<(f32, f32)> = freqs.chunks_exact(2).map(|c| (c[0], c[1])).collect();
        if devices.is_empty() || devices.len() > 1024 {
            return Err(StoredCalibrationError::ImplausibleDeviceCount(
                devices.len(),
            ));
        }

        Ok(Self {
            captured_at: lines[0].clone(),
            ambient_c,
            rail_mv,
            devices,
            trailer,
        })
    }

    pub fn device_count(&self) -> usize {
        self.devices.len()
    }

    /// Mean across every entry, both halves of every device.
    ///
    /// Computed the same way the vendor daemon reports it, which is how the
    /// format was confirmed: our mean, min and max all matched its log line for
    /// the same board.
    pub fn mean_mhz(&self) -> f32 {
        let n = (self.devices.len() * 2) as f32;
        self.devices.iter().map(|(a, b)| a + b).sum::<f32>() / n
    }

    pub fn min_mhz(&self) -> f32 {
        self.devices
            .iter()
            .flat_map(|(a, b)| [*a, *b])
            .fold(f32::INFINITY, f32::min)
    }

    pub fn max_mhz(&self) -> f32 {
        self.devices
            .iter()
            .flat_map(|(a, b)| [*a, *b])
            .fold(f32::NEG_INFINITY, f32::max)
    }

    /// The per-ASIC frequency map, keyed for `apply_frequency_map`.
    ///
    /// The shapes already agree: this file holds two entries per device, and the
    /// operating-point model holds `[f32; 2]` per ASIC. Nothing is averaged or
    /// flattened on the way across, because the spread is the entire content.
    ///
    /// # The assumption, stated because it is not yet verified
    ///
    /// **File order is assumed to map to ascending ASIC id from `start_id`.**
    /// That is the obvious reading and it is only a reading: the vendor could
    /// equally be writing in stack or column order, which would produce a
    /// file that parses perfectly and programs every device with its
    /// neighbour's tuning. The result would not be an error. It would be a
    /// slightly worse-performing chain, which is exactly the kind of defect
    /// that survives for months.
    ///
    /// **The experiment that settles it** needs no rig time beyond a powered
    /// chain: apply this map, read the per-ASIC PLL registers back, and compare
    /// against the file. Agreement confirms the order; disagreement shows the
    /// permutation directly. Until that has been run, treat a chain programmed
    /// from this map as tuned-but-unconfirmed rather than calibrated.
    pub fn per_asic_pll_mhz(&self, start_id: u16) -> std::collections::BTreeMap<u16, [f32; 2]> {
        self.devices
            .iter()
            .enumerate()
            .map(|(index, (a, b))| (start_id + index as u16, [*a, *b]))
            .collect()
    }

    /// Is this tuning being used near the conditions it was derived under?
    ///
    /// Tuning is only valid in context: the file records the ambient and rail it
    /// was taken at, and applying it far from either is not the same operation.
    /// This does not refuse anything -- it reports, so a caller can decide and a
    /// run can record which it was. A comparison between two firmwares where one
    /// ran on out-of-context tuning is measuring the context.
    pub fn context_matches(&self, ambient_c: f32, rail_mv: u32, ambient_tolerance_c: f32) -> bool {
        (self.ambient_c - ambient_c).abs() <= ambient_tolerance_c && self.rail_mv == rail_mv
    }

    /// Devices whose two entries disagree.
    ///
    /// Empty in every sample seen so far. Exposed rather than assumed away: if
    /// it is ever non-empty, a caller programming one frequency per device is
    /// silently discarding half the tuning.
    #[cfg(test)]
    pub fn devices_with_split_halves(&self) -> Vec<usize> {
        self.devices
            .iter()
            .enumerate()
            .filter(|(_, (a, b))| a != b)
            .map(|(i, _)| i)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    /// Synthetic: the same shape and format the vendor daemon's own replay file
    /// uses (established by reading a real one and cross-checking the derived
    /// mean, min and max against that daemon's own log line, which is where the
    /// layout below comes from), but every value here is invented, not measured.
    /// A hundred devices, two equal entries each on the PLL grid, at a fixed
    /// synthetic spread: mean 1059.375 MHz, min 1000.00, max 1118.75.
    fn fixture() -> StoredCalibration {
        let bytes = include_bytes!("../../../tests/fixtures/bzm2/stored-calibration-board0.csv");
        StoredCalibration::parse(BufReader::new(&bytes[..])).expect("the fixture must parse")
    }

    /// The parser's derived statistics against the synthetic fixture's own
    /// known values -- proof the same three-statistic derivation the format
    /// note describes still runs correctly, without depending on a captured file.
    #[test]
    fn derived_statistics_match_the_synthetic_fixture() {
        let c = fixture();
        assert_eq!(c.device_count(), 100, "a board carries 100 devices");
        assert!(
            (c.mean_mhz() - 1059.375).abs() < 0.001,
            "mean {} should match the synthetic fixture's 1059.375",
            c.mean_mhz()
        );
        assert!((c.min_mhz() - 1000.00).abs() < 0.01);
        assert!((c.max_mhz() - 1118.75).abs() < 0.01);
    }

    /// The context the tuning is only valid in. Both are load-bearing: tuning
    /// derived at one ambient and rail need not hold at another, so a comparison
    /// that ignores them is not comparing like with like.
    #[test]
    fn carries_the_conditions_the_tuning_was_derived_under() {
        let c = fixture();
        assert!((c.ambient_c - 25.0).abs() < 0.01);
        assert_eq!(c.rail_mv, 18_000);
        assert!(!c.captured_at.is_empty());
    }

    /// The spread IS the calibration. A parser that returned a single number per
    /// board, or averaged the devices together, would parse this file successfully
    /// and throw away the only thing in it that matters.
    #[test]
    fn preserves_the_per_device_spread() {
        let c = fixture();
        let distinct: std::collections::BTreeSet<_> = c
            .devices
            .iter()
            .flat_map(|(a, b)| [a.to_bits(), b.to_bits()])
            .collect();
        assert!(
            distinct.len() > 10,
            "only {} distinct frequencies; the spread has been flattened",
            distinct.len()
        );
        assert!(c.max_mhz() - c.min_mhz() > 100.0);
    }

    /// Every device's halves agreed in this sample. Asserted as an observation
    /// about the sample, not a rule about the format -- the parser deliberately
    /// accepts differing halves, and this test documents which world we are in.
    #[test]
    fn halves_agree_in_this_sample_though_the_format_allows_otherwise() {
        assert!(fixture().devices_with_split_halves().is_empty());
    }

    /// A frequency off the PLL grid cannot be programmed, so a file containing one
    /// is not describing this part. Refused rather than rounded: rounding would
    /// silently run the chain at a frequency nobody chose.
    #[test]
    fn refuses_frequencies_that_cannot_be_programmed() {
        let bad = "2026-1-1 0:0:0\n21.00\n17550\n1150.00\n1151.00\n35.08\n";
        match StoredCalibration::parse(BufReader::new(bad.as_bytes())) {
            Err(StoredCalibrationError::OffPllGrid { mhz, .. }) => assert_eq!(mhz, 1151.0),
            other => panic!("expected an off-grid refusal, got {other:?}"),
        }
    }

    /// Two entries per device is structural. An odd count means the file is not
    /// what we think it is, and guessing which device lost an entry would put a
    /// wrong frequency on real silicon.
    #[test]
    fn refuses_an_entry_count_that_is_not_two_per_device() {
        let odd = "2026-1-1 0:0:0\n21.00\n17550\n1150.00\n1150.00\n1150.00\n35.08\n";
        assert!(matches!(
            StoredCalibration::parse(BufReader::new(odd.as_bytes())),
            Err(StoredCalibrationError::OddEntryCount(3))
        ));
    }

    /// The map keeps every device distinct. A conversion that averaged, or that
    /// collapsed the two entries into one, would compile, run, and silently
    /// discard the only thing the file contains.
    #[test]
    fn per_asic_map_preserves_every_device_and_both_entries() {
        let c = fixture();
        let map = c.per_asic_pll_mhz(0);
        assert_eq!(map.len(), 100);
        for (index, (a, b)) in c.devices.iter().enumerate() {
            assert_eq!(map[&(index as u16)], [*a, *b]);
        }
        let distinct: std::collections::BTreeSet<_> =
            map.values().map(|v| v[0].to_bits()).collect();
        assert!(
            distinct.len() > 10,
            "the spread was flattened in conversion"
        );
    }

    /// start_id offsets the keys; it does not reorder or drop anything.
    #[test]
    fn per_asic_map_honours_the_start_id() {
        let c = fixture();
        let base = c.per_asic_pll_mhz(0);
        let offset = c.per_asic_pll_mhz(16);
        assert_eq!(base.len(), offset.len());
        assert_eq!(base[&0], offset[&16]);
        assert_eq!(base[&99], offset[&115]);
    }

    /// Context is reported, never enforced. Tuning taken at one ambient and rail
    /// is not wrong elsewhere -- it is differently valid, and a comparison that
    /// does not record which it had is measuring the context rather than the
    /// firmware.
    #[test]
    fn context_match_is_reported_against_ambient_and_rail() {
        let c = fixture();
        assert!(c.context_matches(25.0, 18_000, 2.0));
        assert!(c.context_matches(26.5, 18_000, 2.0), "within tolerance");
        assert!(
            !c.context_matches(39.0, 18_000, 2.0),
            "far from derivation ambient"
        );
        assert!(
            !c.context_matches(25.0, 17_000, 2.0),
            "different rail entirely"
        );
    }
}
