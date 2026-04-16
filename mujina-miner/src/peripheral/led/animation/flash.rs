//! One-shot flash animation.

use std::time::Duration;

use crate::tracing::prelude::*;
use tokio::sync::oneshot;
use tokio::time::Instant;

use super::{AnimationHandle, Resume, StopMode};
use crate::hw_trait::rgb_led::{RgbColor, RgbLed};

/// Flash the LED with a color for a fixed duration.
///
/// Spawns a task that sets the color and sleeps for the duration.
/// The handle can be awaited via [`AnimationHandle::finish`] to wait
/// for the flash to end, or cancelled early via
/// [`AnimationHandle::cancel`].
pub fn flash(led: Box<dyn RgbLed>, color: RgbColor, duration: Duration) -> AnimationHandle {
    spawn_flash(led, color, duration)
}

fn spawn_flash(mut led: Box<dyn RgbLed>, color: RgbColor, remaining: Duration) -> AnimationHandle {
    let (cancel_tx, mut cancel_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        if let Err(e) = led.set(color, 1.0).await {
            warn!(error = %e, "LED flash write failed");
        }
        let start = Instant::now();
        let sleep = tokio::time::sleep(remaining);
        tokio::pin!(sleep);

        let remaining = tokio::select! {
            _ = &mut sleep => Duration::ZERO,
            Ok(mode) = &mut cancel_rx => match mode {
                StopMode::Immediate => remaining.saturating_sub(start.elapsed()),
                StopMode::FinishCycle => {
                    sleep.await;
                    Duration::ZERO
                }
            },
        };
        let state = FlashState { color, remaining };
        (led as Box<dyn RgbLed>, Box::new(state) as Box<dyn Resume>)
    });
    AnimationHandle::new(cancel_tx, task)
}

/// State for resuming a flash with its remaining duration.
struct FlashState {
    color: RgbColor,
    remaining: Duration,
}

impl Resume for FlashState {
    fn resume(self: Box<Self>, led: Box<dyn RgbLed>) -> AnimationHandle {
        if self.remaining.is_zero() {
            super::off(led)
        } else {
            spawn_flash(led, self.color, self.remaining)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use super::super::StopMode;
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

    #[tokio::test(start_paused = true)]
    async fn resume_completes_remaining_duration() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let mock = MockLed {
            writes: writes.clone(),
        };

        let handle = flash(Box::new(mock), RgbColor::ORANGE, Duration::from_secs(1));

        // Let 300ms of the flash elapse
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Cancel mid-flash; state captures ~700ms remaining
        let (led, state) = handle.stop(StopMode::Immediate).await;

        // Resume and wait for natural completion
        let start = Instant::now();
        let _led = state.resume(led).finish().await;
        let elapsed = start.elapsed();

        // Should finish in ~700ms, not the full 1s
        assert!(
            elapsed >= Duration::from_millis(600),
            "resumed too fast: {elapsed:?}",
        );
        assert!(
            elapsed <= Duration::from_millis(800),
            "resumed too slow: {elapsed:?}",
        );
    }
}
