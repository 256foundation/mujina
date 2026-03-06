use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const CALI_VOLTAGE_MV: u32 = 50;
const CALI_FREQ_MHZ: f32 = 25.0;
const CALI_PASS_RATE_STEP: f32 = 0.0025;
const TARGET_VOLTAGE_MAX_MV: u32 = 21_000;
const TARGET_VOLTAGE_MIN_MV: u32 = 16_950;
const TARGET_FREQ_MAX_MHZ: f32 = 2_000.0;
const TARGET_FREQ_MIN_MHZ: f32 = 800.0;
const TARGET_FREQ_HIGH_PLUS_MHZ: f32 = 1_312.5;
const TARGET_FREQ_HIGH_MHZ: f32 = 1_200.0;
const TARGET_FREQ_BALANCED_MHZ: f32 = 1_150.0;
const TARGET_FREQ_LOW_MHZ: f32 = 1_000.0;
const FREQ_RANGE_MHZ: f32 = 100.0;
const MAX_FREQ_RANGE_MHZ: f32 = 150.0;
const ACCEPT_RATIO_BAND_MAX_THROUGHPUT: f32 = 0.02;
const ACCEPT_RATIO_BAND_STANDARD: f32 = 0.02;
const ACCEPT_RATIO_BAND_EFFICIENCY: f32 = 0.02;
const MIN_ACCEPT_RATIO: f32 = 0.90;
const DESIRED_ACCEPT_RATIO_MAX_THROUGHPUT: f32 = 0.975;
const DESIRED_ACCEPT_RATIO_STANDARD: f32 = 0.975;
const DESIRED_ACCEPT_RATIO_EFFICIENCY: f32 = 0.975;
const STARTUP_VOLTAGE_BIAS_MV: i32 = 50;
const SITE_TEMP_COLD_SOAK_C: f32 = -2.5;
const SITE_TEMP_COOL_C: f32 = 7.5;
const SITE_TEMP_NOMINAL_C: f32 = 17.5;
const SITE_TEMP_WARM_C: f32 = 27.5;
const DEFAULT_THERMAL_THRESHOLD_C: f32 = 100.0;
const DEFAULT_AVG_THERMAL_THRESHOLD_C: f32 = 85.0;
const DEFAULT_CURRENT_THRESHOLD_A: f32 = 260.0;
const DEFAULT_POWER_THRESHOLD_W: f32 = 4_900.0;
const DEFAULT_FREQ_INCREASE_RATIO_HIGH: f32 = 0.28;
const DEFAULT_FREQ_INCREASE_RATIO_LOW: f32 = 0.24;
const DEFAULT_RECALIBRATE_THROUGHPUT_RATIO: f32 = 0.80;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Bzm2PerformanceMode {
    MaxThroughput,
    Standard,
    Efficiency,
}

impl Bzm2PerformanceMode {
    fn pass_rate_range(self) -> f32 {
        match self {
            Self::MaxThroughput => ACCEPT_RATIO_BAND_MAX_THROUGHPUT,
            Self::Standard => ACCEPT_RATIO_BAND_STANDARD,
            Self::Efficiency => ACCEPT_RATIO_BAND_EFFICIENCY,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bzm2OperatingClass {
    Generic,
    EarlyValidation,
    ProductionValidation,
    StackTunedA,
    StackTunedB,
    ExtendedHeadroom,
    ExtendedHeadroomB,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Bzm2CalibrationMode {
    pub sweep_strategy: bool,
    pub sweep_voltage: bool,
    pub sweep_frequency: bool,
    pub sweep_pass_rate: bool,
}

#[derive(Debug, Clone)]
pub struct Bzm2CalibrationConstraints {
    pub max_power_w: f32,
    pub max_current_a: f32,
    pub max_thermal_c: f32,
    pub max_avg_thermal_c: f32,
    pub freq_range_mhz: f32,
    pub max_freq_range_mhz: f32,
    pub recalibrate_throughput_ratio: f32,
}

impl Default for Bzm2CalibrationConstraints {
    fn default() -> Self {
        Self {
            max_power_w: DEFAULT_POWER_THRESHOLD_W,
            max_current_a: DEFAULT_CURRENT_THRESHOLD_A,
            max_thermal_c: DEFAULT_THERMAL_THRESHOLD_C,
            max_avg_thermal_c: DEFAULT_AVG_THERMAL_THRESHOLD_C,
            freq_range_mhz: FREQ_RANGE_MHZ,
            max_freq_range_mhz: MAX_FREQ_RANGE_MHZ,
            recalibrate_throughput_ratio: DEFAULT_RECALIBRATE_THROUGHPUT_RATIO,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Bzm2CalibrationSweepRequest {
    pub operating_class: Bzm2OperatingClass,
    pub target_mode: Bzm2PerformanceMode,
    pub mode: Bzm2CalibrationMode,
    pub voltage_steps: u8,
    pub frequency_steps: u8,
    pub pass_rate_steps: u8,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Bzm2SavedOperatingPoint {
    pub board_voltage_mv: u32,
    pub board_throughput_ths: f32,
    #[serde(default)]
    pub per_domain_voltage_mv: BTreeMap<u16, u32>,
    pub per_asic_pll_mhz: BTreeMap<u16, [f32; 2]>,
}

#[derive(Debug, Clone)]
pub struct Bzm2AsicTopology {
    pub asic_id: u16,
    pub domain_id: u16,
    pub pll_count: usize,
    pub alive: bool,
}

#[derive(Debug, Clone)]
pub struct Bzm2VoltageDomain {
    pub domain_id: u16,
    pub asic_ids: Vec<u16>,
    pub voltage_offset_mv: i32,
    pub max_power_w: Option<f32>,
}

#[derive(Debug, Clone, Default)]
pub struct Bzm2DomainMeasurement {
    pub domain_id: u16,
    pub measured_voltage_mv: Option<u32>,
    pub measured_power_w: Option<f32>,
}

#[derive(Debug, Clone, Default)]
pub struct Bzm2AsicMeasurement {
    pub asic_id: u16,
    pub temperature_c: Option<f32>,
    pub throughput_ths: Option<f32>,
    pub average_pass_rate: Option<f32>,
    pub pll_pass_rates: [Option<f32>; 2],
}

#[derive(Debug, Clone)]
pub struct Bzm2BoardCalibrationInput {
    pub operating_class: Bzm2OperatingClass,
    pub site_temp_c: f32,
    pub target_mode: Bzm2PerformanceMode,
    pub mode: Bzm2CalibrationMode,
    pub per_stack_clocking: bool,
    pub voltage_domains: Vec<Bzm2VoltageDomain>,
    pub asics: Vec<Bzm2AsicTopology>,
    pub saved_operating_point: Option<Bzm2SavedOperatingPoint>,
    pub domain_measurements: Vec<Bzm2DomainMeasurement>,
    pub asic_measurements: Vec<Bzm2AsicMeasurement>,
    pub constraints: Bzm2CalibrationConstraints,
    pub force_retune: bool,
}

#[derive(Debug, Clone)]
pub struct Bzm2DomainPlan {
    pub domain_id: u16,
    pub voltage_mv: u32,
    pub average_frequency_mhz: f32,
    pub guarded: bool,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Bzm2AsicPlan {
    pub asic_id: u16,
    pub domain_id: u16,
    pub pll_frequencies_mhz: [f32; 2],
    pub notes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Bzm2CalibrationPlan {
    pub reuse_saved_operating_point: bool,
    pub needs_retune: bool,
    pub desired_voltage_mv: u32,
    pub desired_clock_mhz: f32,
    pub desired_accept_ratio: f32,
    pub initial_voltage_mv: u32,
    pub initial_frequency_mhz: f32,
    pub freq_increase_threshold_mhz: f32,
    pub search_space: Vec<Bzm2ParameterSet>,
    pub domain_plans: Vec<Bzm2DomainPlan>,
    pub asic_plans: Vec<Bzm2AsicPlan>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Bzm2ParameterSet {
    pub mode: Bzm2PerformanceMode,
    pub desired_voltage_mv: u32,
    pub desired_clock_mhz: f32,
    pub desired_accept_ratio: f32,
}

#[derive(Debug, Default)]
pub struct Bzm2CalibrationPlanner;

impl Bzm2CalibrationPlanner {
    pub fn build_search_space(
        &self,
        request: &Bzm2CalibrationSweepRequest,
    ) -> Vec<Bzm2ParameterSet> {
        let modes = if request.mode.sweep_strategy {
            vec![
                Bzm2PerformanceMode::MaxThroughput,
                Bzm2PerformanceMode::Standard,
                Bzm2PerformanceMode::Efficiency,
            ]
        } else {
            vec![request.target_mode]
        };

        let mut parameters = Vec::new();
        for mode in modes {
            let target = operating_targets(request.operating_class, mode);
            let voltage_offsets = build_offsets(request.mode.sweep_voltage, request.voltage_steps);
            let frequency_offsets =
                build_frequency_offsets(request.mode.sweep_frequency, request.frequency_steps);
            let pass_rate_offsets =
                build_pass_rate_offsets(request.mode.sweep_pass_rate, request.pass_rate_steps);

            for voltage_offset in &voltage_offsets {
                for frequency_offset in &frequency_offsets {
                    for pass_rate_offset in &pass_rate_offsets {
                        parameters.push(Bzm2ParameterSet {
                            mode,
                            desired_voltage_mv: clamp_voltage(apply_i32(
                                target.voltage_mv,
                                *voltage_offset,
                            )),
                            desired_clock_mhz: clamp_frequency(
                                target.frequency_mhz + *frequency_offset,
                            ),
                            desired_accept_ratio: clamp_pass_rate(
                                target.pass_rate + *pass_rate_offset,
                            ),
                        });
                    }
                }
            }
        }

        parameters.sort_by(|a, b| {
            a.mode
                .cmp(&b.mode)
                .then(a.desired_voltage_mv.cmp(&b.desired_voltage_mv))
                .then_with(|| a.desired_clock_mhz.total_cmp(&b.desired_clock_mhz))
                .then_with(|| a.desired_accept_ratio.total_cmp(&b.desired_accept_ratio))
        });
        parameters.dedup_by(|a, b| {
            a.mode == b.mode
                && a.desired_voltage_mv == b.desired_voltage_mv
                && (a.desired_clock_mhz - b.desired_clock_mhz).abs() < f32::EPSILON
                && (a.desired_accept_ratio - b.desired_accept_ratio).abs() < f32::EPSILON
        });
        parameters
    }

    pub fn plan(&self, input: &Bzm2BoardCalibrationInput) -> Bzm2CalibrationPlan {
        let target = operating_targets(input.operating_class, input.target_mode);
        let search_space = self.build_search_space(&Bzm2CalibrationSweepRequest {
            operating_class: input.operating_class,
            target_mode: input.target_mode,
            mode: input.mode,
            voltage_steps: 4,
            frequency_steps: 4,
            pass_rate_steps: 2,
        });

        let current_throughput = input
            .asic_measurements
            .iter()
            .filter_map(|asic| asic.throughput_ths)
            .sum::<f32>();
        let has_live_throughput = input
            .asic_measurements
            .iter()
            .any(|asic| asic.throughput_ths.is_some());
        let reuse_saved_operating_point =
            input.saved_operating_point.as_ref().is_some_and(|stored| {
                !input.force_retune
                    && stored.per_asic_pll_mhz.len()
                        == input.asics.iter().filter(|asic| asic.alive).count()
                    && (!has_live_throughput
                        || current_throughput
                            >= stored.board_throughput_ths
                                * input.constraints.recalibrate_throughput_ratio)
            });
        let needs_retune = input.force_retune
            || (has_live_throughput
                && input.saved_operating_point.as_ref().is_some_and(|stored| {
                    current_throughput
                        < stored.board_throughput_ths
                            * input.constraints.recalibrate_throughput_ratio
                }));

        let (initial_voltage_mv, freq_increase_threshold_mhz) = initial_voltage_and_threshold(
            target.voltage_mv,
            input.site_temp_c,
            input.mode.sweep_frequency,
        );
        let initial_frequency_mhz = clamp_frequency(
            (target.frequency_mhz - input.constraints.freq_range_mhz).max(TARGET_FREQ_MIN_MHZ),
        );

        let domain_measurements: BTreeMap<u16, &Bzm2DomainMeasurement> = input
            .domain_measurements
            .iter()
            .map(|measurement| (measurement.domain_id, measurement))
            .collect();
        let asic_measurements: BTreeMap<u16, &Bzm2AsicMeasurement> = input
            .asic_measurements
            .iter()
            .map(|measurement| (measurement.asic_id, measurement))
            .collect();

        let mut domain_plans = Vec::new();
        let mut asic_plans = Vec::new();
        let mut notes = Vec::new();

        for domain in &input.voltage_domains {
            let domain_target_voltage =
                clamp_voltage(apply_i32(initial_voltage_mv, domain.voltage_offset_mv));
            let domain_power = domain_measurements
                .get(&domain.domain_id)
                .and_then(|measurement| measurement.measured_power_w)
                .unwrap_or_default();
            let domain_guarded = domain.max_power_w.is_some_and(|limit| domain_power > limit)
                || domain_power > input.constraints.max_power_w;

            let domain_asics: Vec<&Bzm2AsicTopology> = input
                .asics
                .iter()
                .filter(|asic| asic.alive && asic.domain_id == domain.domain_id)
                .collect();
            let domain_avg_temp = average(
                domain_asics
                    .iter()
                    .filter_map(|asic| asic_measurements.get(&asic.asic_id))
                    .filter_map(|measurement| measurement.temperature_c),
            );
            let domain_avg_pass_rate = average(
                domain_asics
                    .iter()
                    .filter_map(|asic| asic_measurements.get(&asic.asic_id))
                    .filter_map(|measurement| measurement.average_pass_rate),
            );

            let mut domain_frequency = initial_frequency_mhz;
            let mut domain_notes = Vec::new();
            if let Some(pass_rate) = domain_avg_pass_rate {
                if pass_rate >= target.pass_rate && !domain_guarded {
                    domain_frequency = clamp_frequency(
                        target
                            .frequency_mhz
                            .min(initial_frequency_mhz + input.constraints.max_freq_range_mhz),
                    );
                    domain_notes.push(format!(
                        "domain average pass rate {:.2}% supports target frequency",
                        pass_rate * 100.0
                    ));
                } else {
                    domain_notes.push(format!(
                        "domain average pass rate {:.2}% below target {:.2}%",
                        pass_rate * 100.0,
                        target.pass_rate * 100.0
                    ));
                }
            }
            if let Some(temp) = domain_avg_temp {
                if temp >= input.constraints.max_avg_thermal_c {
                    domain_frequency = clamp_frequency(domain_frequency - CALI_FREQ_MHZ);
                    domain_notes.push(format!(
                        "domain average temperature {:.1}C triggered thermal guard",
                        temp
                    ));
                }
            }
            if domain_guarded {
                domain_frequency = clamp_frequency(domain_frequency - CALI_FREQ_MHZ);
                domain_notes.push("domain power guard active".into());
            }

            domain_plans.push(Bzm2DomainPlan {
                domain_id: domain.domain_id,
                voltage_mv: domain_target_voltage,
                average_frequency_mhz: domain_frequency,
                guarded: domain_guarded,
                notes: domain_notes.clone(),
            });

            for asic in domain_asics {
                let measurement = asic_measurements.get(&asic.asic_id).copied();
                let mut pll_frequencies = [domain_frequency; 2];
                let mut asic_notes = Vec::new();

                if reuse_saved_operating_point {
                    if let Some(stored) = input
                        .saved_operating_point
                        .as_ref()
                        .and_then(|stored| stored.per_asic_pll_mhz.get(&asic.asic_id))
                    {
                        pll_frequencies = *stored;
                        asic_notes.push("reusing stored per-ASIC calibration".into());
                    }
                } else if let Some(measurement) = measurement {
                    if let Some(temp) = measurement.temperature_c {
                        if temp >= input.constraints.max_thermal_c {
                            pll_frequencies =
                                [clamp_frequency(domain_frequency - CALI_FREQ_MHZ); 2];
                            asic_notes.push(format!(
                                "ASIC temperature {:.1}C exceeded thermal threshold",
                                temp
                            ));
                        }
                    }

                    if input.per_stack_clocking {
                        for (pll_index, pass_rate) in measurement.pll_pass_rates.iter().enumerate()
                        {
                            if let Some(pass_rate) = pass_rate {
                                let low = target.pass_rate - input.target_mode.pass_rate_range();
                                let high = target.pass_rate + input.target_mode.pass_rate_range();
                                if *pass_rate < low {
                                    pll_frequencies[pll_index] =
                                        clamp_frequency(pll_frequencies[pll_index] - CALI_FREQ_MHZ);
                                    asic_notes.push(format!(
                                        "PLL {} pass rate {:.2}% below window",
                                        pll_index,
                                        pass_rate * 100.0
                                    ));
                                } else if *pass_rate > high && !domain_guarded {
                                    pll_frequencies[pll_index] = clamp_frequency(
                                        pll_frequencies[pll_index] + CALI_FREQ_MHZ / 2.0,
                                    );
                                    asic_notes.push(format!(
                                        "PLL {} pass rate {:.2}% above window",
                                        pll_index,
                                        pass_rate * 100.0
                                    ));
                                }
                            }
                        }
                    } else if let Some(pass_rate) = measurement.average_pass_rate {
                        if pass_rate < target.pass_rate - input.target_mode.pass_rate_range() {
                            pll_frequencies =
                                [clamp_frequency(domain_frequency - CALI_FREQ_MHZ); 2];
                            asic_notes.push(format!(
                                "ASIC pass rate {:.2}% below target window",
                                pass_rate * 100.0
                            ));
                        }
                    }
                }

                if domain_guarded {
                    asic_notes.push("bounded by domain power guard".into());
                }

                asic_plans.push(Bzm2AsicPlan {
                    asic_id: asic.asic_id,
                    domain_id: asic.domain_id,
                    pll_frequencies_mhz: pll_frequencies,
                    notes: asic_notes,
                });
            }
        }

        if reuse_saved_operating_point {
            notes.push("saved operating point is consistent with current throughput".into());
        } else if needs_retune {
            notes.push(
                "saved operating point is missing or underperforming; full retune required".into(),
            );
        } else {
            notes.push("building fresh domain-aware calibration plan".into());
        }

        if input.voltage_domains.len() > 1 {
            notes.push("domain-first planning enabled for multi-domain hardware".into());
        }
        if input.asics.len() >= 100 {
            notes.push("planner uses one domain aggregation pass and one ASIC tuning pass".into());
        }

        Bzm2CalibrationPlan {
            reuse_saved_operating_point,
            needs_retune,
            desired_voltage_mv: target.voltage_mv,
            desired_clock_mhz: target.frequency_mhz,
            desired_accept_ratio: target.pass_rate,
            initial_voltage_mv,
            initial_frequency_mhz,
            freq_increase_threshold_mhz,
            search_space,
            domain_plans,
            asic_plans,
            notes,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct OperatingTarget {
    voltage_mv: u32,
    frequency_mhz: f32,
    pass_rate: f32,
}

fn operating_targets(
    operating_class: Bzm2OperatingClass,
    mode: Bzm2PerformanceMode,
) -> OperatingTarget {
    let (high_voltage, balanced_voltage, low_voltage, high_freq) = match operating_class {
        Bzm2OperatingClass::Generic => (17_600, 17_500, 17_150, TARGET_FREQ_HIGH_MHZ),
        Bzm2OperatingClass::EarlyValidation => (17_800, 17_700, 17_350, TARGET_FREQ_HIGH_MHZ),
        Bzm2OperatingClass::ProductionValidation => (17_550, 17_450, 17_100, TARGET_FREQ_HIGH_MHZ),
        Bzm2OperatingClass::StackTunedA => (17_300, 17_200, 16_850, TARGET_FREQ_HIGH_MHZ),
        Bzm2OperatingClass::StackTunedB => (17_600, 17_500, 17_150, TARGET_FREQ_HIGH_MHZ),
        Bzm2OperatingClass::ExtendedHeadroom => (17_900, 17_450, 17_100, TARGET_FREQ_HIGH_PLUS_MHZ),
        Bzm2OperatingClass::ExtendedHeadroomB => {
            (18_050, 17_550, 17_150, TARGET_FREQ_HIGH_PLUS_MHZ)
        }
    };

    match mode {
        Bzm2PerformanceMode::MaxThroughput => OperatingTarget {
            voltage_mv: high_voltage,
            frequency_mhz: high_freq,
            pass_rate: DESIRED_ACCEPT_RATIO_MAX_THROUGHPUT,
        },
        Bzm2PerformanceMode::Standard => OperatingTarget {
            voltage_mv: balanced_voltage,
            frequency_mhz: TARGET_FREQ_BALANCED_MHZ,
            pass_rate: DESIRED_ACCEPT_RATIO_STANDARD,
        },
        Bzm2PerformanceMode::Efficiency => OperatingTarget {
            voltage_mv: low_voltage,
            frequency_mhz: TARGET_FREQ_LOW_MHZ,
            pass_rate: DESIRED_ACCEPT_RATIO_EFFICIENCY,
        },
    }
}

fn build_offsets(enabled: bool, steps: u8) -> Vec<i32> {
    if !enabled {
        return vec![0];
    }

    let steps = steps.min(20) as i32;
    (-steps..=steps)
        .map(|step| step * CALI_VOLTAGE_MV as i32)
        .collect()
}

fn build_frequency_offsets(enabled: bool, steps: u8) -> Vec<f32> {
    if !enabled {
        return vec![0.0];
    }

    let steps = steps.min(16) as i32;
    (-steps..=steps)
        .map(|step| step as f32 * CALI_FREQ_MHZ)
        .collect()
}

fn build_pass_rate_offsets(enabled: bool, steps: u8) -> Vec<f32> {
    if !enabled {
        return vec![0.0];
    }

    let steps = steps.min(4) as i32;
    (-steps..=steps)
        .map(|step| step as f32 * CALI_PASS_RATE_STEP)
        .collect()
}

fn initial_voltage_and_threshold(
    desired_voltage_mv: u32,
    site_temp_c: f32,
    frequency_mode: bool,
) -> (u32, f32) {
    let (offset, ratio) = if site_temp_c < SITE_TEMP_COLD_SOAK_C {
        (STARTUP_VOLTAGE_BIAS_MV * 2, DEFAULT_FREQ_INCREASE_RATIO_LOW)
    } else if site_temp_c < SITE_TEMP_COOL_C {
        (STARTUP_VOLTAGE_BIAS_MV, DEFAULT_FREQ_INCREASE_RATIO_HIGH)
    } else if site_temp_c < SITE_TEMP_NOMINAL_C {
        (0, DEFAULT_FREQ_INCREASE_RATIO_HIGH)
    } else if site_temp_c < SITE_TEMP_WARM_C {
        (-STARTUP_VOLTAGE_BIAS_MV, DEFAULT_FREQ_INCREASE_RATIO_HIGH)
    } else {
        (
            -STARTUP_VOLTAGE_BIAS_MV * 2,
            DEFAULT_FREQ_INCREASE_RATIO_LOW,
        )
    };

    let threshold = if frequency_mode {
        CALI_FREQ_MHZ * DEFAULT_FREQ_INCREASE_RATIO_LOW
    } else {
        CALI_FREQ_MHZ * ratio
    };

    (
        clamp_voltage(apply_i32(desired_voltage_mv, offset)),
        threshold,
    )
}

fn clamp_voltage(voltage_mv: u32) -> u32 {
    voltage_mv.clamp(TARGET_VOLTAGE_MIN_MV, TARGET_VOLTAGE_MAX_MV)
}

fn clamp_frequency(frequency_mhz: f32) -> f32 {
    frequency_mhz.clamp(TARGET_FREQ_MIN_MHZ, TARGET_FREQ_MAX_MHZ)
}

fn clamp_pass_rate(pass_rate: f32) -> f32 {
    pass_rate.clamp(MIN_ACCEPT_RATIO, DESIRED_ACCEPT_RATIO_MAX_THROUGHPUT)
}

fn apply_i32(value: u32, offset: i32) -> u32 {
    if offset >= 0 {
        value.saturating_add(offset as u32)
    } else {
        value.saturating_sub(offset.unsigned_abs())
    }
}

fn average(values: impl Iterator<Item = f32>) -> Option<f32> {
    let mut total = 0.0;
    let mut count = 0usize;
    for value in values {
        total += value;
        count += 1;
    }
    (count > 0).then_some(total / count as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_space_expands_requested_axes() {
        let planner = Bzm2CalibrationPlanner;
        let parameters = planner.build_search_space(&Bzm2CalibrationSweepRequest {
            operating_class: Bzm2OperatingClass::Generic,
            target_mode: Bzm2PerformanceMode::Standard,
            mode: Bzm2CalibrationMode {
                sweep_strategy: true,
                sweep_voltage: true,
                sweep_frequency: true,
                sweep_pass_rate: true,
            },
            voltage_steps: 1,
            frequency_steps: 1,
            pass_rate_steps: 1,
        });

        assert!(parameters.len() > 20);
        assert!(
            parameters
                .iter()
                .any(|p| p.mode == Bzm2PerformanceMode::MaxThroughput)
        );
        assert!(parameters.iter().any(|p| p.desired_voltage_mv < 17_500));
        assert!(
            parameters
                .iter()
                .any(|p| p.desired_clock_mhz > TARGET_FREQ_BALANCED_MHZ)
        );
    }

    #[test]
    fn single_asic_plan_prefers_saved_operating_point_when_consistent() {
        let planner = Bzm2CalibrationPlanner;
        let mut stored = BTreeMap::new();
        stored.insert(0, [1_075.0, 1_075.0]);
        let plan = planner.plan(&Bzm2BoardCalibrationInput {
            operating_class: Bzm2OperatingClass::Generic,
            site_temp_c: 15.0,
            target_mode: Bzm2PerformanceMode::Standard,
            mode: Bzm2CalibrationMode::default(),
            per_stack_clocking: false,
            voltage_domains: vec![Bzm2VoltageDomain {
                domain_id: 0,
                asic_ids: vec![0],
                voltage_offset_mv: 0,
                max_power_w: None,
            }],
            asics: vec![Bzm2AsicTopology {
                asic_id: 0,
                domain_id: 0,
                pll_count: 2,
                alive: true,
            }],
            saved_operating_point: Some(Bzm2SavedOperatingPoint {
                board_voltage_mv: 17_500,
                board_throughput_ths: 42.0,
                per_domain_voltage_mv: BTreeMap::new(),
                per_asic_pll_mhz: stored,
            }),
            domain_measurements: vec![Bzm2DomainMeasurement {
                domain_id: 0,
                measured_voltage_mv: Some(17_480),
                measured_power_w: Some(320.0),
            }],
            asic_measurements: vec![Bzm2AsicMeasurement {
                asic_id: 0,
                temperature_c: Some(72.0),
                throughput_ths: Some(40.0),
                average_pass_rate: Some(0.98),
                pll_pass_rates: [Some(0.98), Some(0.98)],
            }],
            constraints: Bzm2CalibrationConstraints::default(),
            force_retune: false,
        });

        assert!(plan.reuse_saved_operating_point);
        assert!(!plan.needs_retune);
        assert_eq!(plan.asic_plans[0].pll_frequencies_mhz, [1_075.0, 1_075.0]);
    }

    #[test]
    fn planner_requests_retune_when_throughput_drops() {
        let planner = Bzm2CalibrationPlanner;
        let mut stored = BTreeMap::new();
        stored.insert(0, [1_150.0, 1_150.0]);
        let plan = planner.plan(&Bzm2BoardCalibrationInput {
            operating_class: Bzm2OperatingClass::Generic,
            site_temp_c: 20.0,
            target_mode: Bzm2PerformanceMode::Standard,
            mode: Bzm2CalibrationMode::default(),
            per_stack_clocking: false,
            voltage_domains: vec![Bzm2VoltageDomain {
                domain_id: 0,
                asic_ids: vec![0],
                voltage_offset_mv: 0,
                max_power_w: None,
            }],
            asics: vec![Bzm2AsicTopology {
                asic_id: 0,
                domain_id: 0,
                pll_count: 2,
                alive: true,
            }],
            saved_operating_point: Some(Bzm2SavedOperatingPoint {
                board_voltage_mv: 17_500,
                board_throughput_ths: 50.0,
                per_domain_voltage_mv: BTreeMap::new(),
                per_asic_pll_mhz: stored,
            }),
            domain_measurements: vec![],
            asic_measurements: vec![Bzm2AsicMeasurement {
                asic_id: 0,
                temperature_c: Some(74.0),
                throughput_ths: Some(20.0),
                average_pass_rate: Some(0.94),
                pll_pass_rates: [Some(0.94), Some(0.94)],
            }],
            constraints: Bzm2CalibrationConstraints::default(),
            force_retune: false,
        });

        assert!(!plan.reuse_saved_operating_point);
        assert!(plan.needs_retune);
    }

    #[test]
    fn multi_domain_plan_scales_to_large_topology() {
        let planner = Bzm2CalibrationPlanner;
        let domains: Vec<Bzm2VoltageDomain> = (0..25)
            .map(|domain_id| Bzm2VoltageDomain {
                domain_id,
                asic_ids: (0..4).map(|offset| domain_id * 4 + offset).collect(),
                voltage_offset_mv: if domain_id % 2 == 0 { 0 } else { 25 },
                max_power_w: Some(450.0),
            })
            .collect();
        let asics: Vec<Bzm2AsicTopology> = (0..100)
            .map(|asic_id| Bzm2AsicTopology {
                asic_id,
                domain_id: asic_id / 4,
                pll_count: 2,
                alive: true,
            })
            .collect();
        let domain_measurements: Vec<Bzm2DomainMeasurement> = (0..25)
            .map(|domain_id| Bzm2DomainMeasurement {
                domain_id,
                measured_voltage_mv: Some(17_450),
                measured_power_w: Some(if domain_id == 3 { 500.0 } else { 300.0 }),
            })
            .collect();
        let asic_measurements: Vec<Bzm2AsicMeasurement> = (0..100)
            .map(|asic_id| Bzm2AsicMeasurement {
                asic_id,
                temperature_c: Some(if asic_id == 13 { 101.0 } else { 74.0 }),
                throughput_ths: Some(0.4),
                average_pass_rate: Some(if asic_id % 9 == 0 { 0.93 } else { 0.98 }),
                pll_pass_rates: [Some(0.97), Some(0.98)],
            })
            .collect();

        let plan = planner.plan(&Bzm2BoardCalibrationInput {
            operating_class: Bzm2OperatingClass::ExtendedHeadroom,
            site_temp_c: 10.0,
            target_mode: Bzm2PerformanceMode::Standard,
            mode: Bzm2CalibrationMode::default(),
            per_stack_clocking: true,
            voltage_domains: domains,
            asics,
            saved_operating_point: None,
            domain_measurements,
            asic_measurements,
            constraints: Bzm2CalibrationConstraints::default(),
            force_retune: false,
        });

        assert_eq!(plan.domain_plans.len(), 25);
        assert_eq!(plan.asic_plans.len(), 100);
        assert!(
            plan.domain_plans
                .iter()
                .find(|domain| domain.domain_id == 3)
                .unwrap()
                .guarded
        );
        assert!(
            plan.asic_plans
                .iter()
                .find(|asic| asic.asic_id == 13)
                .unwrap()
                .pll_frequencies_mhz[0]
                < plan.initial_frequency_mhz + 1.0
        );
        assert!(plan.notes.iter().any(|note| note.contains("domain-first")));
    }
}
