//! Per-chip-model configuration for the BM13xx family.
//!
//! `ChipConfig` carries the defaults that vary by chip model: identity,
//! frequency range, IO driver strength, and the PLL search parameters.
//! Factory functions [`bm1362`] and [`bm1370`] return the values for
//! each supported model, validated against serial captures of an
//! S19 J Pro (BM1362) and an S21 Pro and Bitaxe Gamma (BM1370).

use std::ops::RangeInclusive;

use super::register::{
    AnalogMux, ChipModel, CoreCommand, CoreRegister, HashCountingNumber, MiscControl, PllDivider,
    SoftResetControl,
};
use crate::types::{Frequency, HashRate};

/// Per-chip-model configuration.
///
/// Build via [`bm1362`] or [`bm1370`] and adjust fields as needed for
/// a specific board:
///
/// ```
/// use mujina_miner::asic::bm13xx::chip_config::{self, ChipConfig};
/// use mujina_miner::types::Frequency;
///
/// // A board whose cooling limits the chip below its silicon maximum
/// let config = ChipConfig {
///     freq_range: Frequency::from_mhz(56.25)..=Frequency::from_mhz(490.0),
///     ..chip_config::bm1370()
/// };
/// ```
#[derive(Debug, Clone)]
pub struct ChipConfig {
    /// Chip model identity. Verified during enumeration.
    pub model: ChipModel,

    /// Frequencies supported on this chip. The lower bound is the
    /// chip's frequency out of reset, where the bring-up ramp
    /// starts.
    pub freq_range: RangeInclusive<Frequency>,

    /// Default hash frequency.
    pub default_freq: Frequency,

    /// Frequency increase per ramp write.
    pub ramp_step: Frequency,

    /// Nonce sweep length, the factory value observed in captures at
    /// the default frequency.
    pub hash_counting_number: HashCountingNumber,

    /// Nameplate hashrate of one chip: a production machine's
    /// specified hashrate over its chip count, rounded.
    pub nameplate: HashRate,

    /// Soft reset value written broadcast before configuration.
    pub soft_reset_defaults: SoftResetControl,

    /// Soft reset value that resets each chip's cores.
    pub core_reset: SoftResetControl,

    /// Misc control value with nonce reporting enabled.
    pub misc_control: MiscControl,

    /// Analog mux routing written during bring-up.
    pub analog_mux: AnalogMux,

    /// Clock-select core command; register and value vary by model.
    pub clock_select: CoreCommand,

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
    pub fn calculate_pll(&self, freq: Frequency) -> Option<PllDivider> {
        if !self.freq_range.contains(&freq) {
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
                        best_config = Some(PllDivider::new(fb_div, ref_div, post_div));
                    }
                }
            }
        }

        best_config
    }
}

/// BM1362 defaults (EmberOne00, S19 J Pro). The frequency range and
/// ramp step follow the S19 J Pro capture's ramp; the PLL bounds
/// follow the model's row in REFERENCE.md.
pub fn bm1362() -> ChipConfig {
    ChipConfig {
        model: ChipModel::BM1362,
        freq_range: Frequency::from_mhz(56.25)..=Frequency::from_mhz(525.0),
        default_freq: Frequency::from_mhz(525.0),
        ramp_step: Frequency::from_mhz(6.25),
        hash_counting_number: HashCountingNumber::from(0x1381),
        // S19j Pro: 104 TH/s over 378 chips
        nameplate: HashRate::from_gigahashes(275.0),
        soft_reset_defaults: SoftResetControl::defaults(ChipModel::BM1362),
        core_reset: SoftResetControl::core_reset(ChipModel::BM1362),
        misc_control: MiscControl::reporting_enabled(ChipModel::BM1362),
        analog_mux: AnalogMux::bring_up(ChipModel::BM1362),
        // Register offset and value from the S19j Pro capture
        clock_select: CoreCommand::write_all(CoreRegister::ClockSelectBM1362, 0x40),
        pll_params: PllParams {
            fb_div_min: 0x10,
            fb_div_max: 0xfa,
            vco_min_mhz: 2000.0,
            vco_max_mhz: 3200.0,
        },
    }
}

/// BM1370 defaults (Bitaxe Gamma, S21 Pro). The frequency range and
/// ramp step follow the S21 Pro capture's ramp; the PLL bounds
/// follow the model's row in REFERENCE.md.
pub fn bm1370() -> ChipConfig {
    ChipConfig {
        model: ChipModel::BM1370,
        freq_range: Frequency::from_mhz(56.25)..=Frequency::from_mhz(600.0),
        default_freq: Frequency::from_mhz(525.0),
        ramp_step: Frequency::from_mhz(6.25),
        hash_counting_number: HashCountingNumber::from(0x1EB5),
        // S21 Pro: 234 TH/s over 195 chips
        nameplate: HashRate::from_terahashes(1.2),
        soft_reset_defaults: SoftResetControl::defaults(ChipModel::BM1370),
        core_reset: SoftResetControl::core_reset(ChipModel::BM1370),
        misc_control: MiscControl::reporting_enabled(ChipModel::BM1370),
        analog_mux: AnalogMux::bring_up(ChipModel::BM1370),
        // Register offset and value from the Bitaxe capture
        clock_select: CoreCommand::write_all(CoreRegister::ClockSelectBM1368, 0x00),
        pll_params: PllParams {
            fb_div_min: 0xa0,
            fb_div_max: 0xef,
            vco_min_mhz: 1600.0,
            vco_max_mhz: 3200.0,
        },
    }
}

/// PLL search bounds for a BM13xx chip model.
///
/// fb_div and VCO bounds vary by model; the per-model table under
/// PLL_DIVIDER in REFERENCE.md gives each model's values and their
/// provenance. Other search parameters (ref_div range, postdiv
/// ordering) are shared across the family and hardcoded in the
/// search loop.
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
pub(super) const CRYSTAL_MHZ: f32 = 25.0;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asic::bm13xx::register::VcoSel;

    /// A captured ramp write: PLL enabled, VCO in the low range.
    const fn low(fb_div: u8, ref_div: u8, post_div: u8) -> PllDivider {
        PllDivider {
            locked: false,
            pll_en: true,
            bypass: false,
            vco_sel: VcoSel::Low,
            fb_div,
            ref_div,
            post_div,
        }
    }

    /// A captured ramp write: PLL enabled, VCO at or above 2400 MHz.
    const fn high(fb_div: u8, ref_div: u8, post_div: u8) -> PllDivider {
        PllDivider {
            vco_sel: VcoSel::High,
            ..low(fb_div, ref_div, post_div)
        }
    }

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
    fn bm1362_and_bm1370_pll_identical_in_shared_vco_range() {
        // Within the VCO range both models accept (BM1362 [2000, 3200]
        // intersected with BM1370 [1600, 3200]), the PLL search yields
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
    const S19J_PRO_RAMP: &[PllDivider] = &[
        low(0xa2, 0x02, 0x55),
        low(0xaf, 0x02, 0x64),
        low(0xa5, 0x02, 0x54),
        low(0xa8, 0x02, 0x63),
        low(0xb6, 0x02, 0x63),
        low(0xa8, 0x02, 0x53),
        low(0xb4, 0x02, 0x53),
        low(0xa8, 0x02, 0x62),
        low(0xaa, 0x02, 0x43),
        low(0xa2, 0x02, 0x52),
        low(0xab, 0x02, 0x52),
        low(0xb4, 0x02, 0x52),
        low(0xbd, 0x02, 0x52),
        low(0xa5, 0x02, 0x42),
        low(0xa1, 0x02, 0x61),
        low(0xa8, 0x02, 0x61),
        low(0xaf, 0x02, 0x61),
        low(0xb6, 0x02, 0x61),
        low(0xa2, 0x02, 0x51),
        low(0xa8, 0x02, 0x51),
        low(0xae, 0x02, 0x51),
        low(0xb4, 0x02, 0x51),
        low(0xba, 0x02, 0x51),
        low(0xa0, 0x02, 0x41),
        low(0xa5, 0x02, 0x41),
        low(0xaa, 0x02, 0x41),
        low(0xaf, 0x02, 0x41),
        low(0xb4, 0x02, 0x41),
        low(0xb9, 0x02, 0x41),
        low(0xbe, 0x02, 0x41),
        high(0xc3, 0x02, 0x41),
        low(0xa0, 0x02, 0x31),
        low(0xa4, 0x02, 0x31),
        low(0xa8, 0x02, 0x31),
        low(0xac, 0x02, 0x31),
        low(0xb0, 0x02, 0x31),
        low(0xb4, 0x02, 0x31),
        low(0xa1, 0x02, 0x60),
        low(0xbc, 0x02, 0x31),
        low(0xa8, 0x02, 0x60),
        high(0xc4, 0x02, 0x31),
        low(0xaf, 0x02, 0x60),
        high(0xcc, 0x02, 0x31),
        low(0xb6, 0x02, 0x60),
        high(0xd4, 0x02, 0x31),
        low(0xa2, 0x02, 0x50),
        low(0xa5, 0x02, 0x50),
        low(0xa8, 0x02, 0x50),
        low(0xab, 0x02, 0x50),
        low(0xae, 0x02, 0x50),
        low(0xb1, 0x02, 0x50),
        low(0xb4, 0x02, 0x50),
        low(0xb7, 0x02, 0x50),
        low(0xba, 0x02, 0x50),
        low(0xbd, 0x02, 0x50),
        low(0xa0, 0x02, 0x40),
        high(0xc3, 0x02, 0x50),
        low(0xa5, 0x02, 0x40),
        high(0xc9, 0x02, 0x50),
        low(0xaa, 0x02, 0x40),
        high(0xcf, 0x02, 0x50),
        low(0xaf, 0x02, 0x40),
        high(0xd5, 0x02, 0x50),
        low(0xb4, 0x02, 0x40),
        high(0xdb, 0x02, 0x50),
        low(0xb9, 0x02, 0x40),
        high(0xe1, 0x02, 0x50),
        low(0xbe, 0x02, 0x40),
        high(0xe7, 0x02, 0x50),
        high(0xc3, 0x02, 0x40),
        high(0xed, 0x02, 0x50),
        low(0xa0, 0x02, 0x30),
        low(0xa2, 0x02, 0x30),
        low(0xa4, 0x02, 0x30),
        low(0xa6, 0x02, 0x30),
        low(0xa8, 0x02, 0x30),
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
