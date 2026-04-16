//! Breathing LED animation.
//!
//! Brightness is derived from wall-clock time relative to a fixed
//! epoch rather than an internal counter. When an animation is
//! interrupted and later resumed, the epoch carries over, so the
//! animation picks up at the correct phase.

use std::f32::consts::{E, PI};
use std::time::Duration;

use crate::tracing::prelude::*;
use tokio::time::{self, Instant, MissedTickBehavior};

use tokio::sync::oneshot;

use super::{AnimationHandle, Resume, StopMode};
use crate::hw_trait::rgb_led::{RgbColor, RgbLed};

/// Start a breathe animation with the default period.
///
/// Takes ownership of the LED and fades it in a sinusoidal cycle.
/// Cancel or interrupt via the returned handle.
pub fn breathe(led: Box<dyn RgbLed>, color: RgbColor) -> AnimationHandle {
    const BREATHE_PERIOD: Duration = Duration::from_secs(3);

    spawn_breathe(led, color, BREATHE_PERIOD, Instant::now())
}

fn spawn_breathe(
    mut led: Box<dyn RgbLed>,
    color: RgbColor,
    period: Duration,
    epoch: Instant,
) -> AnimationHandle {
    const TICK_INTERVAL: Duration = Duration::from_millis(50);

    let (cancel_tx, mut cancel_rx) = oneshot::channel();
    let period_ms = period.as_millis() as u64;

    let task = tokio::spawn(async move {
        let mut interval = time::interval(TICK_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

        let mut stop: Option<StopMode> = None;

        loop {
            // Derive brightness from wall-clock time so the phase
            // is preserved across interrupt/resume cycles.
            let elapsed_ms = Instant::now().duration_since(epoch).as_millis() as u64;
            let cycle_fraction = (elapsed_ms % period_ms) as f32 / period_ms as f32;
            let brightness = breathe_brightness(cycle_fraction);
            if let Err(e) = led.set(color, brightness).await {
                warn!(error = %e, "LED breathe write failed");
            }

            interval.tick().await;

            if let Ok(mode) = cancel_rx.try_recv() {
                stop = Some(mode);
            }

            match stop {
                Some(StopMode::Immediate) => break,
                Some(StopMode::FinishCycle) => {
                    let elapsed_ms = Instant::now().duration_since(epoch).as_millis() as u64;
                    let phase_ms = elapsed_ms % period_ms;
                    if phase_ms < TICK_INTERVAL.as_millis() as u64 * 2 {
                        break;
                    }
                }
                _ => {}
            }
        }

        let state = BreatheState {
            color,
            period,
            epoch,
        };
        (led as Box<dyn RgbLed>, Box::new(state) as Box<dyn Resume>)
    });

    AnimationHandle::new(cancel_tx, task)
}

/// Compute breathing brightness for a position in the cycle.
///
/// `cycle_fraction` ranges from 0.0 (start) to 1.0 (end). Values
/// outside this range wrap via modulo. Returns a brightness in
/// 0.0..=1.0 using an `e^sin(t)` curve that naturally lingers
/// near off and rounds softly at the peak.
pub fn breathe_brightness(cycle_fraction: f32) -> f32 {
    let t = cycle_fraction.rem_euclid(1.0);
    let inv_e = 1.0 / E;
    let raw = (2.0 * PI * t - PI / 2.0).sin().exp();
    (raw - inv_e) / (E - inv_e)
}

/// State needed to resume a breathe animation.
///
/// Captures the wall-clock epoch so a resumed animation continues
/// at the phase the clock dictates, skipping over any interruption.
#[derive(Debug, Clone)]
struct BreatheState {
    color: RgbColor,
    period: Duration,
    epoch: Instant,
}

impl Resume for BreatheState {
    fn resume(self: Box<Self>, led: Box<dyn RgbLed>) -> AnimationHandle {
        spawn_breathe(led, self.color, self.period, self.epoch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use crate::hw_trait;

    struct MockLed {
        writes: Arc<Mutex<Vec<(RgbColor, f32)>>>,
    }

    #[async_trait::async_trait]
    impl RgbLed for MockLed {
        async fn set(&mut self, color: RgbColor, brightness: f32) -> hw_trait::Result<()> {
            self.writes.lock().unwrap().push((color, brightness));
            Ok(())
        }
    }

    #[test]
    fn breathe_curve_at_known_points() {
        // Start of cycle (sin = -1): brightness should be 0
        let b = breathe_brightness(0.0);
        assert!((b - 0.0).abs() < 0.01, "at 0.0: {b}");

        // Mid-cycle (sin = 1): brightness should be 1
        let b = breathe_brightness(0.5);
        assert!((b - 1.0).abs() < 0.01, "at 0.5: {b}");

        // Full cycle: back to 0
        let b = breathe_brightness(1.0);
        assert!((b - 0.0).abs() < 0.01, "at 1.0: {b}");
    }

    #[test]
    fn breathe_brightness_wraps_out_of_range() {
        // Values > 1.0 should wrap
        let b = breathe_brightness(1.5);
        let expected = breathe_brightness(0.5);
        assert!((b - expected).abs() < 0.01, "1.5 should equal 0.5: {b}");

        // Negative values should wrap
        let b = breathe_brightness(-0.25);
        let expected = breathe_brightness(0.75);
        assert!((b - expected).abs() < 0.01, "-0.25 should equal 0.75: {b}");
    }

    #[tokio::test(start_paused = true)]
    async fn finish_cycle_completes_full_cycle() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let mock = MockLed {
            writes: writes.clone(),
        };

        let color = RgbColor::BLUE;
        let handle = breathe(Box::new(mock), color);

        // Let it run partway through a cycle
        tokio::time::sleep(Duration::from_secs(1)).await;

        // finish_cycle should run until the end of the current cycle
        let led = handle.finish().await;

        // All writes should be the requested color
        let writes = writes.lock().unwrap();
        assert!(!writes.is_empty());
        for (c, _) in writes.iter() {
            assert_eq!(*c, color);
        }

        drop(led);
    }
}
