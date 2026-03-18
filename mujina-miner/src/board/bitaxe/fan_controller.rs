//! Closed-loop fan speed controller for Bitaxe boards.
//!
//! Controls fan duty cycle to maintain a target ASIC temperature using a
//! proportional-integral (PI) control loop.
//!
//! The derivative term is intentionally omitted. Temperature signals from
//! thermal diodes can be noisy, and a D-term tends to amplify this noise,
//! causing unnecessary fan speed oscillation.

use std::collections::VecDeque;
use std::time::Duration;

use crate::{peripheral::emc2101::Percent, tracing::prelude::*};

#[derive(Debug, Clone)]
pub struct FanControllerConfig {
    /// Target ASIC temperature in degrees Celsius. The PI controller
    /// adjusts fan speed to maintain this setpoint.
    pub target_temperature_c: f32,

    /// Moving average window for temperature filter.
    pub temperature_moving_avg_window: u8,

    /// Temperature noise threshold in Celsius.
    pub temperature_noise_threshold_c: f32,

    /// Kp - Tuned to react quickly to overheating while avoiding oscillation.
    pub pi_proportional_gain: f32,

    /// Ki - Kept modest so steady-state error is corrected gradually.
    pub pi_integral_gain: f32,

    /// Minimum allowed fan speed as a percent.
    pub fan_speed_min_pct: f32,

    /// Maximum allowed fan speed as a percent.
    pub fan_speed_max_pct: f32,
}

impl Default for FanControllerConfig {
    fn default() -> Self {
        Self {
            target_temperature_c: 70.0,
            temperature_moving_avg_window: 5,
            temperature_noise_threshold_c: 15.0,
            pi_proportional_gain: 3.0,
            pi_integral_gain: 0.15,
            fan_speed_min_pct: 25.0,
            fan_speed_max_pct: 100.0,
        }
    }
}

pub struct FanController {
    pub filter: TemperatureFilter,
    pub pi: FanPIController,
    pub config: FanControllerConfig,
}

impl FanController {
    pub fn new(config: FanControllerConfig) -> Self {
        let FanControllerConfig {
            target_temperature_c: _target_temperature_c,
            temperature_moving_avg_window,
            temperature_noise_threshold_c,
            pi_proportional_gain,
            pi_integral_gain,
            fan_speed_min_pct,
            fan_speed_max_pct,
        } = config;

        let filter =
            TemperatureFilter::new(temperature_moving_avg_window, temperature_noise_threshold_c);
        let pi = FanPIController::new(
            pi_proportional_gain,
            pi_integral_gain,
            fan_speed_max_pct,
            (fan_speed_min_pct, fan_speed_max_pct),
        );

        Self { filter, pi, config }
    }
    /// Feed a raw sensor reading; get back a fan speed if the
    /// reading passes the noise filter.
    /// `dt` is the time since the last call. The integral term
    /// needs reasonably stable intervals, but that's a given
    /// since the caller runs on a `time::interval`.
    pub fn update(&mut self, reading: f32, dt: Duration) -> Option<Percent> {
        if let Some(temp) = self.filter.consider(reading) {
            info!(temp_c = %temp, "Temperature reading");

            let error = temp - self.config.target_temperature_c;
            let output = self.pi.update(error, dt);
            let speed = output.clamp(self.config.fan_speed_min_pct, self.config.fan_speed_max_pct);

            Some(Percent::new_clamped(speed as u8))
        } else {
            info!("Skipping noisy temperature reading");

            None
        }
    }
}

/// Proportional-integral controller for fan speed.
///
/// Omits the derivative term to avoid amplifying thermal diode noise.
/// See module-level documentation for rationale.
pub struct FanPIController {
    kp: f32,
    ki: f32,
    integral: f32,
    integral_bounds: (f32, f32),
}

impl FanPIController {
    pub fn new(kp: f32, ki: f32, integral: f32, integral_bounds: (f32, f32)) -> Self {
        debug!(kp, ki, integral, ?integral_bounds, "Initializing PI Controller");
        Self {
            kp,
            ki,
            integral,
            integral_bounds,
        }
    }

    pub fn update(&mut self, error: f32, dt: Duration) -> f32 {
        let dt_s = dt.as_secs_f32();

        let p_term = self.kp * error;
        let i_term = self.ki * error * dt_s;

        let new_integral = self.integral + i_term;

        self.integral = if new_integral > self.integral_bounds.1 && error > 0.0 {
            debug!("Integral saturated, keeping the current value");
            self.integral
        } else if new_integral < self.integral_bounds.0 && error < 0.0 {
            self.integral_bounds.0
        } else {
            new_integral
        };

        let output = p_term + self.integral;

        debug!(
            error,
            p_term,
            i_term,
            integral = self.integral,
            output,
            "Fan PI state"
        );

        output
    }
}

#[cfg(test)]
mod fan_pi_controller_tests {
    use super::*;

    #[test]
    fn should_apply_proportional_term() {
        let mut pi = FanPIController::new(2.0, 0.0, 0.0, (0.0, 100.0));

        let output = pi.update(3.0, Duration::from_secs(1));

        assert_eq!(output, 6.0);
    }

    #[test]
    fn should_accumulate_integral_term() {
        let mut pi = FanPIController::new(0.0, 0.5, 10.0, (0.0, 100.0));

        let output = pi.update(4.0, Duration::from_secs(2));

        assert_eq!(output, 14.0);
    }

    #[test]
    fn should_keep_integral_when_upper_bound_is_exceeded() {
        let mut pi = FanPIController::new(0.0, 1.0, 10.0, (0.0, 10.0));

        let output = pi.update(5.0, Duration::from_secs(1));

        assert_eq!(output, 10.0);
    }

    #[test]
    fn should_clamp_integral_to_lower_bound() {
        let mut pi = FanPIController::new(0.0, 1.0, 2.0, (0.0, 10.0));

        let output = pi.update(-5.0, Duration::from_secs(1));

        assert_eq!(output, 0.0);
    }
}

#[cfg(test)]
mod fan_controller_tests {
    use super::*;

    fn controller_for_clamp_tests() -> FanController {
        FanController::new(FanControllerConfig {
            target_temperature_c: 70.0,
            temperature_moving_avg_window: 1,
            temperature_noise_threshold_c: 1_000.0, // effectively disables filter rejection
            pi_proportional_gain: 10.0,
            pi_integral_gain: 0.0, // avoids accumulating integral so anti-windup won't kick in
            fan_speed_min_pct: 25.0,
            fan_speed_max_pct: 100.0,
        })
    }

    #[test]
    fn should_clamp_fan_speed_to_minimum() {
        let mut controller = controller_for_clamp_tests();

        let speed = controller
            .update(60.0, Duration::from_secs(1))
            .expect("reading should be accepted");

        assert_eq!(u8::from(speed), 25);
    }

    #[test]
    fn should_clamp_fan_speed_to_maximum() {
        let mut controller = controller_for_clamp_tests();

        let speed = controller
            .update(90.0, Duration::from_secs(1))
            .expect("reading should be accepted");

        assert_eq!(u8::from(speed), 100);
    }
}

/// Sliding-window noise filter for temperature readings.
///
/// Maintains a moving average of recent readings and rejects readings that
/// deviate too much from this average, which helps filter out sensor noise
/// while still responding to genuine temperature changes.
#[derive(Debug, Clone)]
pub struct TemperatureFilter {
    window: VecDeque<f32>,
    window_size: u8,
    max_deviation_c: f32,
}

impl TemperatureFilter {
    /// Creates a new temperature filter.
    ///
    /// # Arguments
    /// * `window_size` - Number of recent readings in the sliding window
    /// * `max_deviation_c` - Maximum allowed deviation from the moving
    ///   average (in degrees C)
    pub fn new(window_size: u8, max_deviation_c: f32) -> Self {
        Self {
            window: VecDeque::with_capacity(window_size as usize),
            window_size,
            max_deviation_c,
        }
    }

    /// Considers a new reading; returns `Some(temp)` if accepted, `None`
    /// if rejected as noise.
    ///
    /// Readings are rejected if:
    /// - They are outside the valid range (-20 C to 100 C)
    /// - They deviate more than `max_deviation_c` from the current moving
    ///   average
    pub fn consider(&mut self, temp: f32) -> Option<f32> {
        if !(-20.0..=100.0).contains(&temp) {
            return None;
        }

        if !self.window.is_empty() {
            let avg = self.window.iter().sum::<f32>() / self.window.len() as f32;
            let deviation = (temp - avg).abs();
            if deviation > self.max_deviation_c {
                return None;
            }
        }

        if self.window.len() == self.window_size as usize {
            self.window.pop_front();
        }

        self.window.push_back(temp);

        Some(temp)
    }
}

#[cfg(test)]
mod temperature_filter_tests {
    use super::*;

    #[test]
    fn should_accept_valid_reading_when_window_is_empty() {
        let mut filter = TemperatureFilter::new(5, 5.0);

        let result = filter.consider(45.0);

        assert_eq!(result, Some(45.0));
        assert_eq!(filter.window.len(), 1);
    }

    #[test]
    fn should_reject_reading_below_valid_range() {
        let mut filter = TemperatureFilter::new(5, 5.0);

        assert_eq!(filter.consider(-21.0), None);
        assert_eq!(filter.window.len(), 0);
    }

    #[test]
    fn should_reject_reading_above_valid_range() {
        let mut filter = TemperatureFilter::new(5, 5.0);

        assert_eq!(filter.consider(101.0), None);
        assert_eq!(filter.window.len(), 0);
    }

    #[test]
    fn should_accept_readings_at_boundaries() {
        let mut filter = TemperatureFilter::new(5, 5.0);

        assert_eq!(filter.consider(-20.0), Some(-20.0));
        assert_eq!(filter.consider(100.0), None); // deviates 120 C from -20
    }

    #[test]
    fn should_reject_reading_that_exceeds_max_deviation() {
        let mut filter = TemperatureFilter::new(5, 5.0);

        filter.consider(50.0);
        filter.consider(51.0);
        filter.consider(52.0);

        assert_eq!(filter.consider(65.0), None);
    }

    #[test]
    fn should_accept_reading_within_max_deviation() {
        let mut filter = TemperatureFilter::new(5, 5.0);

        filter.consider(50.0);
        filter.consider(51.0);
        filter.consider(52.0);

        assert_eq!(filter.consider(54.0), Some(54.0));
    }

    #[test]
    fn should_maintain_sliding_window_size() {
        let mut filter = TemperatureFilter::new(3, 5.0);

        filter.consider(50.0);
        filter.consider(51.0);
        filter.consider(52.0);
        filter.consider(53.0);

        assert_eq!(filter.window.len(), 3);
        assert_eq!(*filter.window.front().unwrap(), 51.0);
    }

    #[test]
    fn should_accept_readings_after_noise_rejection() {
        let mut filter = TemperatureFilter::new(5, 5.0);

        filter.consider(50.0);
        filter.consider(51.0);
        filter.consider(52.0);
        filter.consider(65.0); // Rejected

        assert_eq!(filter.consider(53.0), Some(53.0));
    }
}
