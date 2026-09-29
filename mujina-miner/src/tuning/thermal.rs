//! Thermal model and the runaway precondition on a voltage grant.
//!
//! Leakage rises exponentially with temperature and dissipated power raises
//! temperature, so above some operating point the loop closes on itself: hotter
//! silicon leaks more, which makes it hotter. Past that point there is no
//! temperature the die settles at, and it climbs until something else stops it.
//!
//! This module answers one question before every voltage or frequency increase:
//! *does a stable operating temperature exist at the proposed point?* If not,
//! the grant is refused. The test is cheap enough to run on every control tick,
//! which matters because the safe dynamic-power budget **shrinks as voltage
//! rises** — a point that was safe at the last grant is not necessarily safe at
//! this one.
//!
//! ## Units
//!
//! The leakage term takes **absolute temperature**, and everything else in the
//! codebase is in Celsius. That mismatch is a documented source of real defects
//! in reference firmware, so the public interface here speaks [`Temperature`]
//! and converts to Kelvin internally. Nothing in this module takes a bare
//! degree value.
//!
//! ## What this model is not
//!
//! [`ThermalResistance`] is a **single lumped** °C/W. A real assembly has at
//! least two thermal time constants — die to TIM in seconds, heatsink to air in
//! minutes — so a measurement only recovers the θ that its own step duration
//! was long enough to expose. A short step measures the fast, small θ and will
//! badly under-report steady-state rise. Runaway is a slow phenomenon, so the
//! value this test needs is the **steady-state** one; see
//! [`ThermalResistance::from_step`].
//!
//! This is deliberately not a predictive thermal model. Predicting θ from
//! materials, geometry and airflow is a hard problem that resists far larger
//! efforts than this one. Measuring it in-system sidesteps the question
//! entirely: the model never has to explain why θ is what it is.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::types::Temperature;

/// Absolute zero, for the Celsius/Kelvin conversion the leakage term needs.
const KELVIN_OFFSET: f32 = 273.15;

/// Bisection steps used to locate the settling temperature.
///
/// Fixed rather than convergence-driven so the cost of the test is a constant
/// the caller can reason about. Forty halvings of a 200 °C bracket resolve to
/// well under a microdegree, far past anything the sensor can distinguish.
const BISECTION_STEPS: u32 = 40;

/// Step duration below which a thermal-resistance measurement is treated as
/// having caught only the fast thermal path.
///
/// The die-to-TIM constant is seconds and the heatsink-to-air constant is
/// minutes, so a step that ends inside the first has measured the wrong θ —
/// small, optimistic, and useless for predicting runaway.
const MIN_STEADY_STATE_STEP: Duration = Duration::from_secs(120);

/// Junction-to-ambient thermal resistance, in °C per watt.
///
/// On a board where the user chooses the heatsink, the TIM and the airflow this
/// cannot be assumed: bondline thickness alone moves junction temperature by
/// tens of degrees. It is measured per build, and it is invalidated by anything
/// that disturbs the mechanical stack — including a re-paste that changes
/// nothing electrical.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct ThermalResistance {
    degrees_c_per_watt: f32,
    /// Whether the measurement ran long enough to reach steady state.
    settled: bool,
}

impl ThermalResistance {
    /// θ from an observed temperature rise under a known power step.
    ///
    /// `held_for` is load-bearing, not bookkeeping. A step shorter than the
    /// slow thermal constant measures only the fast path and yields a θ that is
    /// too small — which would make every runaway check optimistic in the
    /// direction that matters. Such a measurement is still returned, because a
    /// fast-path θ is useful for short-horizon control, but it reports
    /// [`is_settled`][Self::is_settled] as false and
    /// [`RunawayCheck`] refuses to use it.
    ///
    /// A constant sensor offset cancels in the difference, so an uncalibrated
    /// temperature sensor does not corrupt this measurement — only gain error
    /// and noise do.
    pub fn from_step(
        delta_temperature: Temperature,
        delta_power_w: f32,
        held_for: Duration,
    ) -> Option<Self> {
        let rise_c = delta_temperature.as_degrees_c();
        if !rise_c.is_finite()
            || !delta_power_w.is_finite()
            || delta_power_w <= 0.0
            || rise_c <= 0.0
        {
            return None;
        }
        Some(Self {
            degrees_c_per_watt: rise_c / delta_power_w,
            settled: held_for >= MIN_STEADY_STATE_STEP,
        })
    }

    /// A θ from a datasheet or a prior characterisation, assumed steady-state.
    pub fn from_degrees_c_per_watt(degrees_c_per_watt: f32) -> Option<Self> {
        (degrees_c_per_watt.is_finite() && degrees_c_per_watt > 0.0).then_some(Self {
            degrees_c_per_watt,
            settled: true,
        })
    }

    pub fn degrees_c_per_watt(self) -> f32 {
        self.degrees_c_per_watt
    }

    /// Whether the measurement reached the slow thermal path.
    pub fn is_settled(self) -> bool {
        self.settled
    }
}

/// A stored θ measurement, with the conditions that produced it.
///
/// θ is expensive to obtain — it costs a deliberate power step and minutes of
/// settling — so it is persisted and carried across recalibration rather than
/// re-measured whenever the operating point moves. Changing voltage does not
/// change the heatsink.
///
/// **What invalidates it, and what cannot detect that.** θ describes the
/// mechanical build: heatsink, airflow, and above all TIM bondline. A board
/// swap or a firmware change refuses the whole profile and takes θ with it. But
/// a re-paste, a re-torqued heatsink or a changed fan alters θ completely while
/// being *electrically invisible* — nothing in software can observe it. So a
/// mechanical change means deleting the profile, and that is an operator
/// responsibility this type documents rather than one it can enforce.
///
/// The conditions are stored alongside so a stored θ can be argued with later:
/// a number with no record of how it was obtained cannot be audited, only
/// believed.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct ThermalCharacterisation {
    pub theta: ThermalResistance,
    /// Ambient during the measurement. Not used to correct θ — θ is a ratio and
    /// does not depend on ambient — but a measurement taken in a hot room had
    /// less headroom for the step, which is worth knowing when the number looks
    /// surprising.
    pub measured_at_ambient_c: f32,
    /// Power step applied, in watts.
    pub step_power_w: f32,
    /// How long the step was held. The reason θ can be trusted as steady state,
    /// or the reason it cannot.
    pub held_for_secs: u64,
    /// Unix epoch seconds of the measurement.
    pub measured_at_epoch_s: Option<u64>,
}

/// Coefficients of the power model, fitted per board.
///
/// `P(V, f, T) = C·V²·f  +  V·Ig  +  V·k1·T²·exp(k2/T)`
///
/// The three terms are switching power, static gate leakage, and subthreshold
/// leakage. Only the last depends on temperature, and it is the only reason
/// runaway is possible at all.
///
/// `k1` and `k2` are recovered from two or three clock-gated leakage
/// measurements at different plate temperatures — two parameters, so two points
/// suffice and a third checks the fit. Unlike θ, this fit **is** sensitive to a
/// temperature offset: the exponential takes absolute temperature, so an
/// uncalibrated sensor biases the coefficients nonlinearly and the bias does not
/// cancel when the result is evaluated somewhere else.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PowerModel {
    /// Switching capacitance term, watts per volt² per MHz.
    pub dynamic_coefficient: f32,
    /// Static gate leakage current, amps.
    pub gate_leakage_a: f32,
    /// Subthreshold leakage scale.
    pub leakage_k1: f32,
    /// Subthreshold leakage exponent, in kelvin. Negative: leakage rises with
    /// temperature.
    pub leakage_k2: f32,
}

impl PowerModel {
    /// Total dissipation at an operating point.
    pub fn power_w(&self, volts: f32, megahertz: f32, temperature: Temperature) -> f32 {
        let kelvin = to_kelvin(temperature);
        self.dynamic_coefficient * volts * volts * megahertz
            + volts * self.gate_leakage_a
            + volts * self.subthreshold_leakage_w_per_volt(kelvin)
    }

    /// `dP/dT` at an operating point, in watts per °C.
    ///
    /// Only the subthreshold term contributes. A degree and a kelvin are the
    /// same size, so no scaling is needed on the derivative itself.
    pub fn power_slope_w_per_c(&self, volts: f32, temperature: Temperature) -> f32 {
        let kelvin = to_kelvin(temperature);
        // d/dT [ T² exp(k2/T) ] = exp(k2/T) · (2T − k2)
        volts
            * self.leakage_k1
            * (self.leakage_k2 / kelvin).exp()
            * (2.0 * kelvin - self.leakage_k2)
    }

    fn subthreshold_leakage_w_per_volt(&self, kelvin: f32) -> f32 {
        self.leakage_k1 * kelvin * kelvin * (self.leakage_k2 / kelvin).exp()
    }
}

/// The proposed operating point and the envelope it must fit inside.
#[derive(Debug, Clone, Copy)]
pub struct RunawayCheck {
    pub volts: f32,
    pub megahertz: f32,
    pub ambient: Temperature,
    pub max_junction: Temperature,
}

/// Why a grant was refused, or the temperature it would settle at.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RunawayVerdict {
    /// A stable operating temperature exists, below the junction limit, with
    /// loop gain under unity.
    Stable {
        settles_at: Temperature,
        /// `θ · dP/dT` at the settling point. Below 1 by definition here; the
        /// margin says how much headroom remains before the loop closes.
        loop_gain: f32,
    },
    /// No temperature satisfies `T = T_ambient + θ·P(T)` below the junction
    /// limit. The die would climb until the hardware trip stops it.
    NoEquilibrium,
    /// An equilibrium exists but above the junction limit.
    ExceedsJunctionLimit { settles_at: Temperature },
    /// The inputs cannot support a decision.
    ///
    /// Refusing is the only safe answer: an unmeasured thermal path is not a
    /// good one, and a θ from a step that never reached steady state is
    /// optimistic in exactly the direction that matters.
    Indeterminate(&'static str),
}

impl RunawayVerdict {
    /// Whether a grant may proceed. Only [`Stable`][Self::Stable] permits it.
    pub fn is_grantable(self) -> bool {
        matches!(self, Self::Stable { .. })
    }
}

/// Decide whether a proposed operating point has a stable temperature.
///
/// The die settles where `T = T_ambient + θ·P(V, f, T)`. Writing
/// `g(T) = T_ambient + θ·P(T) − T`, the equilibrium is a root of `g`.
///
/// `g` starts positive at ambient — the die always heats when powered — and
/// ends positive at high temperature, because the exponential leakage term
/// eventually outruns the linear one. So `g` either dips below zero somewhere
/// between, giving a stable root and an unstable one above it, or it never does
/// and there is no equilibrium at all. Finding the *first* crossing therefore
/// finds the stable point, and a bracket with no crossing is a runaway.
///
/// The loop-gain test is the same statement differentiated: at a stable point a
/// small temperature excursion must feed back less than itself, so `θ·dP/dT < 1`.
/// A root with gain at or above unity sits on the knife edge and is refused.
pub fn evaluate_runaway(
    model: &PowerModel,
    theta: ThermalResistance,
    check: RunawayCheck,
) -> RunawayVerdict {
    if !theta.is_settled() {
        return RunawayVerdict::Indeterminate(
            "thermal resistance was measured over too short a step to represent steady state",
        );
    }
    if !check.volts.is_finite()
        || !check.megahertz.is_finite()
        || check.volts <= 0.0
        || check.megahertz < 0.0
    {
        return RunawayVerdict::Indeterminate("proposed operating point is not a real point");
    }

    let ambient_c = check.ambient.as_degrees_c();
    let limit_c = check.max_junction.as_degrees_c();
    if !ambient_c.is_finite() || !limit_c.is_finite() || limit_c <= ambient_c {
        return RunawayVerdict::Indeterminate("junction limit is not above ambient");
    }

    // Search a little past the limit so an equilibrium just outside it is
    // reported as too hot rather than as an absent one. The two are different
    // faults: one is a point that needs backing off, the other is a point that
    // has no resting place at all.
    let search_ceiling_c = limit_c + (limit_c - ambient_c);
    let gap = |temperature_c: f32| {
        let temperature = Temperature::from_celsius(temperature_c);
        ambient_c
            + theta.degrees_c_per_watt() * model.power_w(check.volts, check.megahertz, temperature)
            - temperature_c
    };

    let Some(settles_at_c) = first_root(&gap, ambient_c, search_ceiling_c) else {
        return RunawayVerdict::NoEquilibrium;
    };
    let settles_at = Temperature::from_celsius(settles_at_c);
    if settles_at_c > limit_c {
        return RunawayVerdict::ExceedsJunctionLimit { settles_at };
    }

    let loop_gain = theta.degrees_c_per_watt() * model.power_slope_w_per_c(check.volts, settles_at);
    if !loop_gain.is_finite() {
        return RunawayVerdict::Indeterminate("loop gain could not be evaluated");
    }
    // At or above unity the root sits on the knife edge: a small excursion feeds
    // back at least as much as itself, so it is an equilibrium in name only.
    if loop_gain >= 1.0 {
        return RunawayVerdict::NoEquilibrium;
    }
    RunawayVerdict::Stable {
        settles_at,
        loop_gain,
    }
}

/// The lowest root of `gap` in `[low, high]`, if it crosses zero at all.
///
/// `gap` is positive at `low`. Coarse scanning finds the first interval where it
/// turns non-positive, then bisection resolves it. The scan resolution bounds
/// how narrow a dip can be missed; at one degree, a thermal equilibrium narrower
/// than that is not one any real control loop could hold anyway.
fn first_root(gap: &dyn Fn(f32) -> f32, low: f32, high: f32) -> Option<f32> {
    if gap(low) <= 0.0 {
        return Some(low);
    }
    let steps = ((high - low).ceil() as usize).max(1);
    let width = (high - low) / steps as f32;

    let mut left = low;
    for step in 1..=steps {
        let right = low + width * step as f32;
        if gap(right) <= 0.0 {
            return Some(bisect(gap, left, right));
        }
        left = right;
    }
    None
}

/// Bisect for the crossing, given `gap(low) > 0 >= gap(high)`.
fn bisect(gap: &dyn Fn(f32) -> f32, mut low: f32, mut high: f32) -> f32 {
    for _ in 0..BISECTION_STEPS {
        let mid = 0.5 * (low + high);
        if gap(mid) > 0.0 {
            low = mid;
        } else {
            high = mid;
        }
    }
    0.5 * (low + high)
}

fn to_kelvin(temperature: Temperature) -> f32 {
    temperature.as_degrees_c() + KELVIN_OFFSET
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A model in roughly the right region for one BZM2 die: about 13 W of
    /// switching at 0.8 V and 1 GHz, leakage negligible when cold and doubling
    /// every ten degrees or so, which is what makes runaway reachable at all.
    ///
    /// These are plausible numbers, not measured ones. Fitting them for real is
    /// the bench half of this issue.
    fn model() -> PowerModel {
        PowerModel {
            dynamic_coefficient: 0.02,
            gate_leakage_a: 1.0,
            leakage_k1: 1.7e5,
            leakage_k2: -8.0e3,
        }
    }

    fn settled(theta: f32) -> ThermalResistance {
        ThermalResistance::from_degrees_c_per_watt(theta).unwrap()
    }

    fn check(volts: f32, megahertz: f32, theta_ambient_c: f32) -> RunawayCheck {
        RunawayCheck {
            volts,
            megahertz,
            ambient: Temperature::from_celsius(theta_ambient_c),
            max_junction: Temperature::from_celsius(110.0),
        }
    }

    #[test]
    fn a_modest_point_on_a_good_heatsink_settles() {
        let verdict = evaluate_runaway(&model(), settled(1.5), check(0.8, 1_000.0, 25.0));
        let RunawayVerdict::Stable {
            settles_at,
            loop_gain,
        } = verdict
        else {
            panic!("expected a stable point, got {verdict:?}");
        };
        assert!(settles_at.as_degrees_c() > 25.0, "the die must heat at all");
        assert!(settles_at.as_degrees_c() < 110.0);
        assert!(
            loop_gain < 1.0,
            "a stable point has sub-unity loop gain by definition"
        );
        assert!(verdict.is_grantable());
    }

    #[test]
    fn a_bad_thermal_build_turns_the_same_point_into_a_runaway() {
        // Identical silicon and identical operating point. The only thing that
        // changed is the mechanical build — which is exactly why theta cannot
        // be assumed from a datasheet.
        let point = check(0.8, 1_000.0, 25.0);
        assert!(evaluate_runaway(&model(), settled(1.5), point).is_grantable());
        assert!(!evaluate_runaway(&model(), settled(4.0), point).is_grantable());
    }

    #[test]
    fn the_budget_shrinks_as_voltage_rises() {
        // The reason this is a precondition on every grant rather than a
        // one-time check: a point that was safe at the last grant is not
        // necessarily safe at this one.
        let theta = settled(2.5);
        let mut last_grantable = 0.0;
        for step in 0..40 {
            let volts = 0.6 + step as f32 * 0.02;
            if evaluate_runaway(&model(), theta, check(volts, 1_000.0, 25.0)).is_grantable() {
                last_grantable = volts;
            } else {
                assert!(
                    last_grantable > 0.0,
                    "the sweep should grant something before refusing"
                );
                // Once refused it must stay refused: the boundary is a ceiling,
                // not a notch.
                for higher in step..40 {
                    let volts = 0.6 + higher as f32 * 0.02;
                    assert!(
                        !evaluate_runaway(&model(), theta, check(volts, 1_000.0, 25.0))
                            .is_grantable(),
                        "granted {volts} V after refusing a lower voltage"
                    );
                }
                return;
            }
        }
        panic!("expected the sweep to reach a refusal");
    }

    #[test]
    fn a_hotter_room_lowers_the_ceiling() {
        let theta = settled(2.5);
        let cold = (0..40).filter(|&step| {
            evaluate_runaway(
                &model(),
                theta,
                check(0.6 + step as f32 * 0.02, 1_000.0, 15.0),
            )
            .is_grantable()
        });
        let warm = (0..40).filter(|&step| {
            evaluate_runaway(
                &model(),
                theta,
                check(0.6 + step as f32 * 0.02, 1_000.0, 45.0),
            )
            .is_grantable()
        });
        assert!(
            warm.count() < cold.count(),
            "a warmer room must permit strictly less, or ambient is not being used"
        );
    }

    #[test]
    fn an_equilibrium_above_the_limit_is_distinct_from_no_equilibrium() {
        // These are different faults and deserve different handling: one point
        // needs backing off, the other has no resting place at all.
        let mut saw_too_hot = false;
        let mut saw_runaway = false;
        for step in 0..60 {
            let theta = settled(0.5 + step as f32 * 0.05);
            let point = RunawayCheck {
                max_junction: Temperature::from_celsius(70.0),
                ..check(0.85, 1_000.0, 25.0)
            };
            match evaluate_runaway(&model(), theta, point) {
                RunawayVerdict::ExceedsJunctionLimit { settles_at } => {
                    assert!(settles_at.as_degrees_c() > 70.0);
                    saw_too_hot = true;
                }
                RunawayVerdict::NoEquilibrium => saw_runaway = true,
                _ => {}
            }
        }
        assert!(saw_too_hot, "expected some build to settle above the limit");
        assert!(saw_runaway, "expected some build to have no equilibrium");
    }

    #[test]
    fn an_unsettled_theta_is_refused_rather_than_used() {
        // A step that ended inside the fast thermal path measures a small,
        // optimistic theta. Using it would make every check wrong in the
        // dangerous direction.
        let fast = ThermalResistance::from_step(
            Temperature::from_celsius(4.0),
            40.0,
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(!fast.is_settled());
        assert!(matches!(
            evaluate_runaway(&model(), fast, check(0.8, 1_000.0, 25.0)),
            RunawayVerdict::Indeterminate(_)
        ));

        let slow = ThermalResistance::from_step(
            Temperature::from_celsius(4.0),
            40.0,
            Duration::from_secs(300),
        )
        .unwrap();
        assert!(slow.is_settled());
        assert!((slow.degrees_c_per_watt() - 0.1).abs() < 1e-6);
    }

    #[test]
    fn a_sensor_offset_cancels_out_of_a_theta_measurement() {
        // The measurement is a difference, so a constant offset drops out. This
        // is why theta characterisation is not blocked on absolute temperature
        // accuracy, while the leakage fit is.
        let true_rise = ThermalResistance::from_step(
            Temperature::from_celsius(12.0),
            40.0,
            Duration::from_secs(300),
        )
        .unwrap();
        let offset_rise = ThermalResistance::from_step(
            Temperature::from_celsius((62.0 + 5.0) - (50.0 + 5.0)),
            40.0,
            Duration::from_secs(300),
        )
        .unwrap();
        assert_eq!(
            true_rise.degrees_c_per_watt(),
            offset_rise.degrees_c_per_watt()
        );
    }

    #[test]
    fn nonsense_inputs_refuse_rather_than_guess() {
        let theta = settled(0.5);
        for bad in [
            RunawayCheck {
                volts: f32::NAN,
                ..check(0.8, 1_000.0, 25.0)
            },
            RunawayCheck {
                volts: -0.8,
                ..check(0.8, 1_000.0, 25.0)
            },
            RunawayCheck {
                max_junction: Temperature::from_celsius(10.0),
                ..check(0.8, 1_000.0, 25.0)
            },
        ] {
            assert!(
                matches!(
                    evaluate_runaway(&model(), theta, bad),
                    RunawayVerdict::Indeterminate(_)
                ),
                "expected a refusal for {bad:?}"
            );
        }
        assert!(ThermalResistance::from_degrees_c_per_watt(0.0).is_none());
        assert!(
            ThermalResistance::from_step(
                Temperature::from_celsius(5.0),
                0.0,
                Duration::from_secs(300)
            )
            .is_none()
        );
    }

    #[test]
    fn the_check_is_cheap_enough_for_every_control_tick() {
        // The cost is a constant: a bounded scan plus a fixed number of
        // bisection steps. Timed generously so this cannot flake — the point is
        // the order of magnitude, not the number.
        let theta = settled(2.5);
        let started = std::time::Instant::now();
        let iterations = 10_000;
        for step in 0..iterations {
            let volts = 0.7 + (step % 100) as f32 * 0.001;
            std::hint::black_box(evaluate_runaway(
                &model(),
                theta,
                check(volts, 1_000.0, 25.0),
            ));
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "{iterations} evaluations took {elapsed:?}; expected well under a \
             microsecond each on any plausible control host"
        );
    }
}
