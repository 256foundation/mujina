//! Per-chip-model configuration for the BM13xx family.
//!
//! `ChipConfig` carries the defaults that vary by chip model: identity,
//! frequency range, IO driver strength, and the PLL search parameters.
//! Factory functions [`bm1362`] and [`bm1370`] return the values for
//! each supported model, validated against serial captures of an
//! S19 J Pro (BM1362) and an S21 Pro and Bitaxe Gamma (BM1370).

use super::protocol::{ChipModel, PllConfig};
use crate::types::Frequency;

/// Per-chip-model configuration.
///
/// Build via [`bm1362`] or [`bm1370`] and adjust fields as needed for
/// a specific board.
#[derive(Debug, Clone)]
pub struct ChipConfig {
    /// Chip model identity. Verified during enumeration.
    pub model: ChipModel,
    /// Lowest frequency supported on this chip.
    pub min_freq: Frequency,
    /// Highest frequency supported on this chip.
    pub max_freq: Frequency,
    /// PLL search bounds for this chip model.
    pub pll_params: PllParams,
}

impl ChipConfig {
    /// Returns true if `model` matches this chip's model.
    pub fn verify_model(&self, model: ChipModel) -> bool {
        self.model == model
    }

    /// Calculates the optimal PLL configuration for `freq`.
    ///
    /// Searches the space defined by `pll_params` for dividers that
    /// produce the target frequency with minimum error. Among
    /// equal-error configurations, prefers the one with the lowest VCO
    /// frequency to keep VCO within its optimal operating range.
    /// Returns `None` when `freq` lies outside the chip's frequency
    /// range or no divider configuration reaches it.
    pub fn calculate_pll(&self, freq: Frequency) -> Option<PllConfig> {
        if freq < self.min_freq || freq > self.max_freq {
            return None;
        }

        let target_mhz = freq.mhz();
        let params = &self.pll_params;

        let mut best_config = None;
        let mut min_error = f32::MAX;
        let mut best_vco = f32::MAX;

        for ref_div in [2u8, 1u8] {
            for post_div1 in (1..=7).rev() {
                for post_div2 in (1..=post_div1).rev() {
                    let fb_div_f =
                        (post_div1 * post_div2) as f32 * target_mhz * ref_div as f32 / CRYSTAL_MHZ;
                    let fb_div = fb_div_f.round() as u8;

                    if fb_div < params.fb_div_min || fb_div > params.fb_div_max {
                        continue;
                    }

                    let actual_mhz = CRYSTAL_MHZ * fb_div as f32
                        / (ref_div as f32 * post_div1 as f32 * post_div2 as f32);
                    let error = (target_mhz - actual_mhz).abs();
                    let vco = CRYSTAL_MHZ * fb_div as f32 / ref_div as f32;

                    if vco < params.vco_min_mhz || vco > params.vco_max_mhz {
                        continue;
                    }

                    if error < 1.0 && (error < min_error || (error == min_error && vco < best_vco))
                    {
                        min_error = error;
                        best_vco = vco;
                        let post_div = ((post_div1 - 1) << 4) | (post_div2 - 1);
                        best_config = Some(PllConfig::new(fb_div, ref_div, post_div));
                    }
                }
            }
        }

        best_config
    }
}

/// BM1362 defaults (EmberOne00, S19 J Pro). Frequency range and PLL
/// bounds derived from S19 J Pro serial captures.
pub fn bm1362() -> ChipConfig {
    ChipConfig {
        model: ChipModel::BM1362,
        min_freq: Frequency::from_mhz(50.0),
        max_freq: Frequency::from_mhz(525.0),
        pll_params: PllParams {
            fb_div_min: 0xa0,
            fb_div_max: 0xef,
            vco_min_mhz: 1600.0,
            vco_max_mhz: 3200.0,
        },
    }
}

/// BM1370 defaults (Bitaxe Gamma, S21 Pro). Frequency range and PLL
/// bounds derived from S21 Pro serial captures and Bitaxe Gamma logic
/// analyzer captures.
pub fn bm1370() -> ChipConfig {
    ChipConfig {
        model: ChipModel::BM1370,
        min_freq: Frequency::from_mhz(50.0),
        max_freq: Frequency::from_mhz(600.0),
        pll_params: PllParams {
            fb_div_min: 0xa0,
            fb_div_max: 0xef,
            vco_min_mhz: 2000.0,
            vco_max_mhz: 3000.0,
        },
    }
}

/// PLL search bounds for a BM13xx chip model.
///
/// fb_div and VCO bounds vary by model (e.g., BM1362/BM1370 use
/// `fb_div in [0xa0, 0xef]`, while BM1366/BM1368 use `[0x90, 0xeb]`).
/// Other search parameters (ref_div range, postdiv ordering) are
/// shared across the family and hardcoded in the search loop.
#[derive(Debug, Clone, Copy)]
pub struct PllParams {
    /// Minimum feedback divider considered during search.
    pub fb_div_min: u8,
    /// Maximum feedback divider considered during search.
    pub fb_div_max: u8,
    /// Minimum valid VCO frequency (MHz).
    pub vco_min_mhz: f32,
    /// Maximum valid VCO frequency (MHz).
    pub vco_max_mhz: f32,
}

/// Crystal oscillator frequency for BM13xx chips (25 MHz).
const CRYSTAL_MHZ: f32 = 25.0;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_model_matches() {
        let config = bm1362();
        assert!(config.verify_model(ChipModel::BM1362));
        assert!(!config.verify_model(ChipModel::BM1370));
    }

    #[test]
    fn pll_calculation_produces_valid_frequencies() {
        // Reference PLL values from esp-miner (first-match algorithm).
        // Format: (target_mhz, [fb_div, ref_div, post_div]).
        let test_cases = [
            (62.5, [0xd2, 0x02, 0x65]),
            (75.0, [0xd2, 0x02, 0x64]),
            (100.0, [0xe0, 0x02, 0x63]),
            (400.0, [0xe0, 0x02, 0x60]),
            (500.0, [0xa2, 0x02, 0x30]),
        ];

        let config = bm1370();
        for (target_mhz, esp_raw) in test_cases {
            let freq = Frequency::from_mhz(target_mhz);
            let pll = config.calculate_pll(freq).unwrap();

            let esp_post1 = ((esp_raw[2] >> 4) & 0xf) + 1;
            let esp_post2 = (esp_raw[2] & 0xf) + 1;
            let esp_actual =
                CRYSTAL_MHZ * esp_raw[0] as f32 / (esp_raw[1] * esp_post1 * esp_post2) as f32;

            let our_post1 = ((pll.post_div >> 4) & 0xf) + 1;
            let our_post2 = (pll.post_div & 0xf) + 1;
            let our_actual =
                CRYSTAL_MHZ * pll.fb_div as f32 / (pll.ref_div * our_post1 * our_post2) as f32;

            let esp_error = (target_mhz - esp_actual).abs();
            let our_error = (target_mhz - our_actual).abs();

            assert!(
                (0xa0..=0xef).contains(&pll.fb_div),
                "fb_div out of range: {:#04x} at {} MHz",
                pll.fb_div,
                target_mhz
            );
            assert!(
                pll.ref_div == 1 || pll.ref_div == 2,
                "ref_div invalid: {} at {} MHz",
                pll.ref_div,
                target_mhz
            );
            assert!(
                our_error < 1.0,
                "error {:.4} MHz too large for target {} MHz",
                our_error,
                target_mhz
            );
            assert!(
                our_error <= esp_error + 0.01,
                "worse than esp-miner at {} MHz (ours {:.4}, esp {:.4})",
                target_mhz,
                our_error,
                esp_error
            );
        }
    }

    #[test]
    fn rejects_out_of_range_frequencies() {
        // 700 MHz has an exact divider solution (fb_div 0xa8, ref_div 2,
        // post divs 3x1) within the search bounds, so only the frequency
        // range check can reject it.
        assert_eq!(bm1370().calculate_pll(Frequency::from_mhz(700.0)), None);
        assert_eq!(bm1362().calculate_pll(Frequency::from_mhz(600.0)), None);
        assert_eq!(bm1370().calculate_pll(Frequency::from_mhz(40.0)), None);
    }

    #[test]
    fn pll_vco_flag_set_correctly() {
        // Flag 0x50 for vco >= 2400 MHz, otherwise 0x40. Both families
        // use the same rule; VCO bounds differ but the threshold does not.
        for config in [bm1362(), bm1370()] {
            for freq_mhz in [100.0, 200.0, 300.0, 400.0, 500.0, 525.0] {
                let pll = config.calculate_pll(Frequency::from_mhz(freq_mhz)).unwrap();
                let vco = pll.fb_div as f32 * CRYSTAL_MHZ / pll.ref_div as f32;
                let expected = if vco >= 2400.0 { 0x50 } else { 0x40 };
                assert_eq!(
                    pll.flag, expected,
                    "{:?} {} MHz: VCO={:.1} should use flag=0x{:02X}, got 0x{:02X}",
                    config.model, freq_mhz, vco, expected, pll.flag
                );
            }
        }
    }

    #[test]
    fn bm1362_and_bm1370_pll_identical_in_shared_vco_range() {
        // Within the VCO range both models accept (BM1362 [1600, 3200]
        // intersected with BM1370 [2000, 3000]), the PLL search yields
        // identical configurations.
        let bm1362_config = bm1362();
        let bm1370_config = bm1370();
        for freq_mhz in [100.0, 200.0, 300.0, 400.0, 500.0] {
            let freq = Frequency::from_mhz(freq_mhz);
            assert_eq!(
                bm1362_config.calculate_pll(freq).unwrap(),
                bm1370_config.calculate_pll(freq).unwrap(),
                "BM1362 and BM1370 should produce identical PLL at {} MHz",
                freq_mhz
            );
        }
    }

    /// Every PLL write an S19 J Pro hashboard emits during its ramp,
    /// in the order emitted. Each entry is the payload of a
    /// `write_register(pll)` command captured on the serial bus.
    /// 76 steps total.
    #[rustfmt::skip]
    const S19J_PRO_RAMP: &[PllConfig] = &[
        PllConfig { flag: 0x40, fb_div: 0xa2, ref_div: 0x02, post_div: 0x55 },
        PllConfig { flag: 0x40, fb_div: 0xaf, ref_div: 0x02, post_div: 0x64 },
        PllConfig { flag: 0x40, fb_div: 0xa5, ref_div: 0x02, post_div: 0x54 },
        PllConfig { flag: 0x40, fb_div: 0xa8, ref_div: 0x02, post_div: 0x63 },
        PllConfig { flag: 0x40, fb_div: 0xb6, ref_div: 0x02, post_div: 0x63 },
        PllConfig { flag: 0x40, fb_div: 0xa8, ref_div: 0x02, post_div: 0x53 },
        PllConfig { flag: 0x40, fb_div: 0xb4, ref_div: 0x02, post_div: 0x53 },
        PllConfig { flag: 0x40, fb_div: 0xa8, ref_div: 0x02, post_div: 0x62 },
        PllConfig { flag: 0x40, fb_div: 0xaa, ref_div: 0x02, post_div: 0x43 },
        PllConfig { flag: 0x40, fb_div: 0xa2, ref_div: 0x02, post_div: 0x52 },
        PllConfig { flag: 0x40, fb_div: 0xab, ref_div: 0x02, post_div: 0x52 },
        PllConfig { flag: 0x40, fb_div: 0xb4, ref_div: 0x02, post_div: 0x52 },
        PllConfig { flag: 0x40, fb_div: 0xbd, ref_div: 0x02, post_div: 0x52 },
        PllConfig { flag: 0x40, fb_div: 0xa5, ref_div: 0x02, post_div: 0x42 },
        PllConfig { flag: 0x40, fb_div: 0xa1, ref_div: 0x02, post_div: 0x61 },
        PllConfig { flag: 0x40, fb_div: 0xa8, ref_div: 0x02, post_div: 0x61 },
        PllConfig { flag: 0x40, fb_div: 0xaf, ref_div: 0x02, post_div: 0x61 },
        PllConfig { flag: 0x40, fb_div: 0xb6, ref_div: 0x02, post_div: 0x61 },
        PllConfig { flag: 0x40, fb_div: 0xa2, ref_div: 0x02, post_div: 0x51 },
        PllConfig { flag: 0x40, fb_div: 0xa8, ref_div: 0x02, post_div: 0x51 },
        PllConfig { flag: 0x40, fb_div: 0xae, ref_div: 0x02, post_div: 0x51 },
        PllConfig { flag: 0x40, fb_div: 0xb4, ref_div: 0x02, post_div: 0x51 },
        PllConfig { flag: 0x40, fb_div: 0xba, ref_div: 0x02, post_div: 0x51 },
        PllConfig { flag: 0x40, fb_div: 0xa0, ref_div: 0x02, post_div: 0x41 },
        PllConfig { flag: 0x40, fb_div: 0xa5, ref_div: 0x02, post_div: 0x41 },
        PllConfig { flag: 0x40, fb_div: 0xaa, ref_div: 0x02, post_div: 0x41 },
        PllConfig { flag: 0x40, fb_div: 0xaf, ref_div: 0x02, post_div: 0x41 },
        PllConfig { flag: 0x40, fb_div: 0xb4, ref_div: 0x02, post_div: 0x41 },
        PllConfig { flag: 0x40, fb_div: 0xb9, ref_div: 0x02, post_div: 0x41 },
        PllConfig { flag: 0x40, fb_div: 0xbe, ref_div: 0x02, post_div: 0x41 },
        PllConfig { flag: 0x50, fb_div: 0xc3, ref_div: 0x02, post_div: 0x41 },
        PllConfig { flag: 0x40, fb_div: 0xa0, ref_div: 0x02, post_div: 0x31 },
        PllConfig { flag: 0x40, fb_div: 0xa4, ref_div: 0x02, post_div: 0x31 },
        PllConfig { flag: 0x40, fb_div: 0xa8, ref_div: 0x02, post_div: 0x31 },
        PllConfig { flag: 0x40, fb_div: 0xac, ref_div: 0x02, post_div: 0x31 },
        PllConfig { flag: 0x40, fb_div: 0xb0, ref_div: 0x02, post_div: 0x31 },
        PllConfig { flag: 0x40, fb_div: 0xb4, ref_div: 0x02, post_div: 0x31 },
        PllConfig { flag: 0x40, fb_div: 0xa1, ref_div: 0x02, post_div: 0x60 },
        PllConfig { flag: 0x40, fb_div: 0xbc, ref_div: 0x02, post_div: 0x31 },
        PllConfig { flag: 0x40, fb_div: 0xa8, ref_div: 0x02, post_div: 0x60 },
        PllConfig { flag: 0x50, fb_div: 0xc4, ref_div: 0x02, post_div: 0x31 },
        PllConfig { flag: 0x40, fb_div: 0xaf, ref_div: 0x02, post_div: 0x60 },
        PllConfig { flag: 0x50, fb_div: 0xcc, ref_div: 0x02, post_div: 0x31 },
        PllConfig { flag: 0x40, fb_div: 0xb6, ref_div: 0x02, post_div: 0x60 },
        PllConfig { flag: 0x50, fb_div: 0xd4, ref_div: 0x02, post_div: 0x31 },
        PllConfig { flag: 0x40, fb_div: 0xa2, ref_div: 0x02, post_div: 0x50 },
        PllConfig { flag: 0x40, fb_div: 0xa5, ref_div: 0x02, post_div: 0x50 },
        PllConfig { flag: 0x40, fb_div: 0xa8, ref_div: 0x02, post_div: 0x50 },
        PllConfig { flag: 0x40, fb_div: 0xab, ref_div: 0x02, post_div: 0x50 },
        PllConfig { flag: 0x40, fb_div: 0xae, ref_div: 0x02, post_div: 0x50 },
        PllConfig { flag: 0x40, fb_div: 0xb1, ref_div: 0x02, post_div: 0x50 },
        PllConfig { flag: 0x40, fb_div: 0xb4, ref_div: 0x02, post_div: 0x50 },
        PllConfig { flag: 0x40, fb_div: 0xb7, ref_div: 0x02, post_div: 0x50 },
        PllConfig { flag: 0x40, fb_div: 0xba, ref_div: 0x02, post_div: 0x50 },
        PllConfig { flag: 0x40, fb_div: 0xbd, ref_div: 0x02, post_div: 0x50 },
        PllConfig { flag: 0x40, fb_div: 0xa0, ref_div: 0x02, post_div: 0x40 },
        PllConfig { flag: 0x50, fb_div: 0xc3, ref_div: 0x02, post_div: 0x50 },
        PllConfig { flag: 0x40, fb_div: 0xa5, ref_div: 0x02, post_div: 0x40 },
        PllConfig { flag: 0x50, fb_div: 0xc9, ref_div: 0x02, post_div: 0x50 },
        PllConfig { flag: 0x40, fb_div: 0xaa, ref_div: 0x02, post_div: 0x40 },
        PllConfig { flag: 0x50, fb_div: 0xcf, ref_div: 0x02, post_div: 0x50 },
        PllConfig { flag: 0x40, fb_div: 0xaf, ref_div: 0x02, post_div: 0x40 },
        PllConfig { flag: 0x50, fb_div: 0xd5, ref_div: 0x02, post_div: 0x50 },
        PllConfig { flag: 0x40, fb_div: 0xb4, ref_div: 0x02, post_div: 0x40 },
        PllConfig { flag: 0x50, fb_div: 0xdb, ref_div: 0x02, post_div: 0x50 },
        PllConfig { flag: 0x40, fb_div: 0xb9, ref_div: 0x02, post_div: 0x40 },
        PllConfig { flag: 0x50, fb_div: 0xe1, ref_div: 0x02, post_div: 0x50 },
        PllConfig { flag: 0x40, fb_div: 0xbe, ref_div: 0x02, post_div: 0x40 },
        PllConfig { flag: 0x50, fb_div: 0xe7, ref_div: 0x02, post_div: 0x50 },
        PllConfig { flag: 0x50, fb_div: 0xc3, ref_div: 0x02, post_div: 0x40 },
        PllConfig { flag: 0x50, fb_div: 0xed, ref_div: 0x02, post_div: 0x50 },
        PllConfig { flag: 0x40, fb_div: 0xa0, ref_div: 0x02, post_div: 0x30 },
        PllConfig { flag: 0x40, fb_div: 0xa2, ref_div: 0x02, post_div: 0x30 },
        PllConfig { flag: 0x40, fb_div: 0xa4, ref_div: 0x02, post_div: 0x30 },
        PllConfig { flag: 0x40, fb_div: 0xa6, ref_div: 0x02, post_div: 0x30 },
        PllConfig { flag: 0x40, fb_div: 0xa8, ref_div: 0x02, post_div: 0x30 },
    ];

    /// Derives the target frequency of each captured PLL write in the
    /// S19 J Pro ramp, runs `calculate_pll` on it, and asserts our VCO
    /// is not higher than the firmware's at any step. Equality
    /// counts as success (our_vco == captured_vco), so exact matches
    /// pass trivially; mismatches must pick a lower VCO.
    #[test]
    fn pll_ramp_never_higher_vco_than_firmware() {
        let config = bm1362();

        for &captured in S19J_PRO_RAMP {
            let post1 = ((captured.post_div >> 4) & 0xf) + 1;
            let post2 = (captured.post_div & 0xf) + 1;
            let captured_mhz =
                CRYSTAL_MHZ * captured.fb_div as f32 / (captured.ref_div * post1 * post2) as f32;
            let captured_vco = CRYSTAL_MHZ * captured.fb_div as f32 / captured.ref_div as f32;

            let pll = config
                .calculate_pll(Frequency::from_mhz(captured_mhz))
                .unwrap();
            let our_vco = CRYSTAL_MHZ * pll.fb_div as f32 / pll.ref_div as f32;

            assert!(
                our_vco <= captured_vco,
                "at {:.4} MHz: captured VCO {:.1}, ours {:.1} (higher than firmware)",
                captured_mhz,
                captured_vco,
                our_vco
            );
        }
    }
}
