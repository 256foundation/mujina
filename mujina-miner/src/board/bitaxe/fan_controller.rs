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

    /// EMA weight of each new reading. Its smoothing depends on the
    /// update interval; 0.7 at 2 s absorbs jitter without lagging real
    /// temperature rises.
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
            ema_alpha: 0.7,
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
            // Match the fan's full speed at init, so a hot restart eases
            // the fan down instead of dropping it to the floor.
            integral: f32::from(config.fan_speed_max_pct),
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
        // EMA filter to reduce sensor noise.
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

        // The integral shares the fan's range so it rests at the floor
        // below the target and the fan reacts as soon as the target is
        // passed. Back-calculation prevents windup.
        let out_min = f32::from(self.config.fan_speed_min_pct);
        let out_max = f32::from(self.config.fan_speed_max_pct);

        self.integral = (self.integral + i_term).clamp(out_min, out_max);
        let mut output = p_term + self.integral;
        if output > out_max {
            self.integral -= output - out_max;
            output = out_max;
        } else if output < out_min {
            self.integral += out_min - output;
            output = out_min;
        }

        trace!(
            error,
            p_term,
            i_term,
            integral = self.integral,
            output,
            "Fan PI state"
        );

        Percent::new_clamped(output.round() as u8)
    }
}

#[cfg(test)]
mod fan_controller_tests {
    use super::*;

    const TICK: Duration = Duration::from_secs(2);

    /// Smoothing off, so each reading reaches the controller unchanged.
    fn unsmoothed() -> FanControllerConfig {
        FanControllerConfig {
            ema_alpha: 1.0,
            ..Default::default()
        }
    }

    fn hold(ctrl: &mut FanController, temp_c: f32, ticks: usize) -> u8 {
        let mut speed = 0;
        for _ in 0..ticks {
            speed = u8::from(ctrl.update_speed(temp_c, TICK));
        }
        speed
    }

    fn settled_at_floor(config: FanControllerConfig) -> FanController {
        let mut ctrl = FanController::new(config);
        assert_eq!(hold(&mut ctrl, 69.0, 500), 25);
        ctrl
    }

    #[test]
    fn fan_speed_stays_within_configured_limits() {
        let mut ctrl = FanController::new(FanControllerConfig {
            fan_speed_min_pct: 40,
            fan_speed_max_pct: 80,
            ..unsmoothed()
        });

        let mut speeds = Vec::new();
        for temp_c in [50.0, 95.0, 50.0] {
            for _ in 0..200 {
                speeds.push(u8::from(ctrl.update_speed(temp_c, TICK)));
            }
        }

        assert!(speeds.iter().all(|s| (40..=80).contains(s)));
        assert!(speeds.contains(&40));
        assert!(speeds.contains(&80));
    }

    #[test]
    fn hot_restart_does_not_collapse_to_floor() {
        let mut ctrl = FanController::new(FanControllerConfig::default());

        assert!(u8::from(ctrl.update_speed(75.0, TICK)) > 50);
    }

    #[test]
    fn fan_speeds_up_as_soon_as_temperature_passes_target() {
        let mut ctrl = settled_at_floor(unsmoothed());

        assert!(u8::from(ctrl.update_speed(71.0, TICK)) > 25);
    }

    #[test]
    fn hotter_reading_gives_faster_fan() {
        let mut slightly_hot = settled_at_floor(unsmoothed());
        let mut very_hot = settled_at_floor(unsmoothed());

        let slightly_hot_speed = u8::from(slightly_hot.update_speed(71.0, TICK));
        let very_hot_speed = u8::from(very_hot.update_speed(74.0, TICK));

        assert!(very_hot_speed > slightly_hot_speed);
    }

    #[test]
    fn sustained_error_keeps_raising_fan_speed() {
        // The integral term removes steady-state offset.
        let mut ctrl = settled_at_floor(unsmoothed());

        let speeds: Vec<u8> = (0..10)
            .map(|_| u8::from(ctrl.update_speed(72.0, TICK)))
            .collect();

        assert!(speeds.windows(2).all(|w| w[1] >= w[0]));
        assert!(speeds.last() > speeds.first());
    }

    #[test]
    fn fan_releases_from_full_once_overheat_ends() {
        // Guards against integral windup during a long overheat.
        let mut ctrl = FanController::new(unsmoothed());
        assert_eq!(hold(&mut ctrl, 90.0, 200), 100);

        assert!(u8::from(ctrl.update_speed(70.0, TICK)) < 100);
    }

    #[test]
    fn smoothing_damps_single_reading_spike() {
        let mut smoothed = settled_at_floor(FanControllerConfig::default());
        let mut unfiltered = settled_at_floor(unsmoothed());

        let smoothed_speed = u8::from(smoothed.update_speed(75.0, TICK));
        let unfiltered_speed = u8::from(unfiltered.update_speed(75.0, TICK));

        assert!(smoothed_speed < unfiltered_speed);
        assert!(smoothed_speed > 25);
    }

    #[test]
    fn smoothing_follows_sustained_rise_within_two_ticks() {
        // Smoothing must not noticeably delay the response to a real rise.
        let mut smoothed = FanController::new(FanControllerConfig::default());
        let mut unfiltered = FanController::new(unsmoothed());
        hold(&mut smoothed, 60.0, 500);
        hold(&mut unfiltered, 60.0, 500);

        let smoothed_speed = u32::from(hold(&mut smoothed, 72.0, 2));
        let unfiltered_speed = u32::from(hold(&mut unfiltered, 72.0, 2));

        assert!(smoothed_speed * 10 >= unfiltered_speed * 9);
    }
}
