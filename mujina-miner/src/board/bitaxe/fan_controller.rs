//! Closed-loop fan speed controller for Bitaxe boards.
//!
//! Controls fan duty cycle to maintain a target ASIC temperature using a
//! proportional-integral (PI) control loop.

use std::time::Duration;

use crate::{peripheral::emc2101::Percent, tracing::prelude::*};

#[derive(Debug, Clone)]
pub struct FanControllerConfig {
    /// Target ASIC temperature in degrees Celsius. The PI controller
    /// adjusts fan speed to maintain this setpoint.
    pub target_temperature_c: f32,

    /// Kp - Tuned to react quickly to overheating while avoiding oscillation.
    pub pi_proportional_gain: f32,

    /// Ki - Kept modest so steady-state error is corrected gradually.
    pub pi_integral_gain: f32,

    /// Minimum allowed fan speed as a percent.
    pub fan_speed_min_pct: u8,

    /// Maximum allowed fan speed as a percent.
    pub fan_speed_max_pct: u8,

    /// Integral accumulator lower bound.
    pub integral_min: f32,

    /// Integral accumulator upper bound.
    pub integral_max: f32,

    /// EMA filter alpha for temperature input. Lower values = more smoothing.
    /// 0.2 means 20% new value, 80% previous filtered value.
    pub ema_alpha: f32,
}

impl Default for FanControllerConfig {
    fn default() -> Self {
        Self {
            target_temperature_c: 70.0,
            pi_proportional_gain: 3.0,
            pi_integral_gain: 0.15,
            fan_speed_min_pct: 25,
            fan_speed_max_pct: 100,
            integral_min: 0.0,
            integral_max: 100.0,
            ema_alpha: 0.2,
        }
    }
}

/// Fan speed controller for Bitaxe boards.
///
/// Drives fan PWM duty cycle to maintain a target ASIC temperature
/// using PI control.
///
/// # PI Control
///
/// The controller uses a proportional-integral (PI) control:
///
/// - **P term** reacts to the current temperature error.
/// - **I term** accumulates past errors over time to eliminate steady-state
///   offset.
///
/// The derivative term (D) is intentionally omitted. Temperature signals from
/// thermal diodes can be noisy, and a D-term tends to amplify this noise,
/// causing unnecessary fan speed oscillation.
///
/// # EMA Filtering
///
/// Temperature readings are smoothed using an exponential moving average (EMA)
/// to reduce sensor noise. The `ema_alpha` config value controls the filter
/// responsiveness: lower values provide more smoothing.
pub struct FanController {
    /// Accumulated integral term.
    integral: f32,
    /// Configuration (target temperature, speed limits, PI gains).
    config: FanControllerConfig,
    /// Filtered temperature using EMA to reduce sensor noise.
    ema_temperature: Option<f32>,
}

impl FanController {
    pub fn new(config: FanControllerConfig) -> Self {
        debug!(
            kp = config.pi_proportional_gain,
            ki = config.pi_integral_gain,
            "Initializing PI Controller"
        );
        Self {
            integral: 0.0,
            config,
            ema_temperature: None,
        }
    }

    /// Compute fan speed for the given temperature reading and time delta.
    ///
    /// `dt` is the time since the last call. The integral term
    /// needs reasonably stable intervals, but that's a given
    /// since the caller runs on a `time::interval`.
    ///
    /// Returns the fan speed as a percentage.
    pub fn update_speed(&mut self, temp_c: f32, dt: Duration) -> Percent {
        // Apply EMA filter to reduce sensor noise.
        // alpha = 0.2 means 20% new value, 80% previous filtered value.
        let filtered_temp = match self.ema_temperature {
            None => temp_c,
            Some(prev) => {
                let alpha = self.config.ema_alpha;
                (alpha * temp_c) + ((1.0 - alpha) * prev)
            }
        };

        self.ema_temperature = Some(filtered_temp);

        let error = filtered_temp - self.config.target_temperature_c;
        let dt_s = dt.as_secs_f32();

        let p_term = self.config.pi_proportional_gain * error;
        let i_term = self.config.pi_integral_gain * error * dt_s;

        let new_integral = self.integral + i_term;

        self.integral = if new_integral > self.config.integral_max && error > 0.0 {
            trace!("Integral saturated, keeping the current value");
            self.integral
        } else if new_integral < self.config.integral_min && error < 0.0 {
            self.config.integral_min
        } else {
            new_integral
        };

        let output = p_term + self.integral;

        trace!(
            error,
            p_term,
            i_term,
            integral = self.integral,
            output,
            "Fan PI state"
        );

        let speed = (output.round() as u8)
            .clamp(self.config.fan_speed_min_pct, self.config.fan_speed_max_pct);

        Percent::new_clamped(speed)
    }
}

#[cfg(test)]
mod fan_controller_tests {
    use super::*;

    #[test]
    fn proportional_term_applies_gain_to_error() {
        // Kp = 2.0, error = 3°C → output = 6
        let mut ctrl = FanController::new(FanControllerConfig {
            target_temperature_c: 70.0,
            pi_proportional_gain: 2.0,
            pi_integral_gain: 0.0,
            fan_speed_min_pct: 0,
            fan_speed_max_pct: 100,
            integral_min: 0.0,
            integral_max: 100.0,
            ema_alpha: 1.0,
        });

        let output = ctrl.update_speed(73.0, Duration::from_secs(1));

        assert_eq!(u8::from(output), 6);
    }

    #[test]
    fn integral_term_accumulates_over_successive_readings() {
        // Ki accumulates the error over time. Two identical readings build up integral.
        let mut ctrl = FanController::new(FanControllerConfig {
            target_temperature_c: 70.0,
            pi_proportional_gain: 0.0,
            pi_integral_gain: 0.5,
            fan_speed_min_pct: 0,
            fan_speed_max_pct: 100,
            integral_min: 0.0,
            integral_max: 100.0,
            ema_alpha: 1.0,
        });

        let first = ctrl.update_speed(74.0, Duration::from_secs(2));
        let second = ctrl.update_speed(74.0, Duration::from_secs(2));

        assert_eq!(u8::from(first), 4);
        assert_eq!(u8::from(second), 8);
    }

    #[test]
    fn integral_windup_is_prevented_when_upper_bound_is_exceeded() {
        // Saturate the integral at its upper bound, then push it further.
        // Anti-windup must prevent the integral from growing past the limit.
        let mut ctrl = FanController::new(FanControllerConfig {
            target_temperature_c: 70.0,
            pi_proportional_gain: 0.0,
            pi_integral_gain: 1.0,
            fan_speed_min_pct: 0,
            fan_speed_max_pct: 100,
            integral_min: 0.0,
            integral_max: 10.0,
            ema_alpha: 1.0,
        });

        // Build integral up to its maximum (10 calls × error=1°C × dt=1s).
        for _ in 0..10 {
            ctrl.update_speed(71.0, Duration::from_secs(1));
        }

        // Next call would push integral to 15; anti-windup clamps it at 10.
        let output = ctrl.update_speed(75.0, Duration::from_secs(1));
        assert_eq!(u8::from(output), 10);
    }

    #[test]
    fn integral_windup_is_prevented_when_lower_bound_is_exceeded() {
        // Saturate the integral near zero, then drive it negative.
        // Anti-windup must prevent the integral from going below the limit.
        let mut ctrl = FanController::new(FanControllerConfig {
            target_temperature_c: 70.0,
            pi_proportional_gain: 0.0,
            pi_integral_gain: 1.0,
            fan_speed_min_pct: 0,
            fan_speed_max_pct: 100,
            integral_min: 0.0,
            integral_max: 10.0,
            ema_alpha: 1.0,
        });

        // Build integral up to 2.
        ctrl.update_speed(71.0, Duration::from_secs(1));
        ctrl.update_speed(71.0, Duration::from_secs(1));

        // Drive integral below zero; anti-windup clamps it at 0.
        let output = ctrl.update_speed(65.0, Duration::from_secs(1));
        assert_eq!(u8::from(output), 0);
    }

    #[test]
    fn output_is_clamped_to_minimum_when_below_target() {
        // Kp = 10, Ki = 0 — output = P term exactly, no integral accumulation.
        let mut ctrl = FanController::new(FanControllerConfig {
            target_temperature_c: 70.0,
            pi_proportional_gain: 10.0,
            pi_integral_gain: 0.0,
            fan_speed_min_pct: 25,
            fan_speed_max_pct: 100,
            integral_min: 0.0,
            integral_max: 100.0,
            ema_alpha: 1.0,
        });

        let speed = ctrl.update_speed(60.0, Duration::from_secs(1));
        assert_eq!(u8::from(speed), 25);
    }

    #[test]
    fn output_is_clamped_to_maximum_when_above_target() {
        // Kp = 10, Ki = 0 — output = P term exactly, no integral accumulation.
        let mut ctrl = FanController::new(FanControllerConfig {
            target_temperature_c: 70.0,
            pi_proportional_gain: 10.0,
            pi_integral_gain: 0.0,
            fan_speed_min_pct: 0,
            fan_speed_max_pct: 100,
            integral_min: 0.0,
            integral_max: 100.0,
            ema_alpha: 1.0,
        });

        let speed = ctrl.update_speed(90.0, Duration::from_secs(1));
        assert_eq!(u8::from(speed), 100);
    }

    #[test]
    fn ema_filter_smooths_temperature_jitter() {
        let mut ctrl = FanController::new(FanControllerConfig {
            target_temperature_c: 70.0,
            pi_proportional_gain: 2.0,
            pi_integral_gain: 0.0,
            fan_speed_min_pct: 0,
            fan_speed_max_pct: 100,
            integral_min: 0.0,
            integral_max: 100.0,
            ema_alpha: 0.2,
        });

        // First call initializes the filter (no smoothing on first reading).
        let first = ctrl.update_speed(80.0, Duration::from_secs(1));
        assert_eq!(u8::from(first), 20);

        // Second call with same raw temperature still gives same result.
        let second = ctrl.update_speed(80.0, Duration::from_secs(1));
        assert_eq!(u8::from(second), 20);

        // Third call with 60°C (big drop). Without EMA: error=-10 => output=-20 => clamped to 0.
        // With EMA: filtered = 0.2*60 + 0.8*80 = 76°C, error=6 => output=12 => 12%.
        let third = ctrl.update_speed(60.0, Duration::from_secs(1));
        assert_eq!(u8::from(third), 12);
    }
}
