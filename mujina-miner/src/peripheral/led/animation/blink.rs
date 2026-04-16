//! Blink LED animation.
//!
//! Like breathe, phase is derived from wall-clock time relative to
//! a fixed epoch, so the animation resumes at the correct phase
//! after an interrupt.

use std::time::Duration;

use crate::tracing::prelude::*;
use tokio::sync::oneshot;
use tokio::time::{self, Instant, MissedTickBehavior};

use super::{AnimationHandle, Resume, StopMode};
use crate::hw_trait::rgb_led::{RgbColor, RgbLed};

/// Start a blink animation with the given phase duration.
///
/// The LED toggles on and off, spending `phase` in each state.
/// The full cycle is `2 * phase`.
pub fn blink(led: Box<dyn RgbLed>, color: RgbColor, phase: Duration) -> AnimationHandle {
    spawn_blink(led, color, phase, Instant::now())
}

fn spawn_blink(
    mut led: Box<dyn RgbLed>,
    color: RgbColor,
    phase: Duration,
    epoch: Instant,
) -> AnimationHandle {
    const TICK_INTERVAL: Duration = Duration::from_millis(50);

    let (cancel_tx, mut cancel_rx) = oneshot::channel();
    let full_cycle = phase * 2;
    let full_cycle_ms = full_cycle.as_millis() as u64;
    let phase_ms_threshold = phase.as_millis() as u64;

    let task = tokio::spawn(async move {
        let mut interval = time::interval(TICK_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

        let mut stop: Option<StopMode> = None;

        loop {
            // Derive on/off state from wall-clock time so the phase
            // is preserved across interrupt/resume cycles.
            let elapsed_ms = Instant::now().duration_since(epoch).as_millis() as u64;
            let phase_ms = elapsed_ms % full_cycle_ms;
            let on = phase_ms < phase_ms_threshold;
            let brightness = if on { 1.0 } else { 0.0 };
            if let Err(e) = led.set(color, brightness).await {
                warn!(error = %e, "LED blink write failed");
            }

            interval.tick().await;

            if let Ok(mode) = cancel_rx.try_recv() {
                stop = Some(mode);
            }

            match stop {
                Some(StopMode::Immediate) => break,
                Some(StopMode::FinishCycle) => {
                    let elapsed_ms = Instant::now().duration_since(epoch).as_millis() as u64;
                    let phase_ms = elapsed_ms % full_cycle_ms;
                    if phase_ms < TICK_INTERVAL.as_millis() as u64 * 2 {
                        break;
                    }
                }
                _ => {}
            }
        }

        let state = BlinkState {
            color,
            phase,
            epoch,
        };
        (led as Box<dyn RgbLed>, Box::new(state) as Box<dyn Resume>)
    });

    AnimationHandle::new(cancel_tx, task)
}

/// State needed to resume a blink animation.
///
/// Captures the wall-clock epoch so a resumed animation continues
/// at the phase the clock dictates, skipping over any interruption.
#[derive(Debug, Clone)]
struct BlinkState {
    color: RgbColor,
    phase: Duration,
    epoch: Instant,
}

impl Resume for BlinkState {
    fn resume(self: Box<Self>, led: Box<dyn RgbLed>) -> AnimationHandle {
        spawn_blink(led, self.color, self.phase, self.epoch)
    }
}
