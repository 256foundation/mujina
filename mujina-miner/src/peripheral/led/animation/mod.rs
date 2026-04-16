//! LED animation primitives.
//!
//! Provides composable async animations for RGB LEDs. Animations come
//! in two flavors:
//!
//! - Looping (breathe, blink, hold): run indefinitely until cancelled.
//! - One-shot (flash): run for a bounded duration then complete on their own.
//!
//! Both return an [`AnimationHandle`] with a uniform API. An
//! animation can be temporarily interrupted by a one-shot via
//! [`AnimationHandle::interrupt`], which resumes the original
//! animation after the one-shot completes.

pub mod blink;
pub mod breathe;
pub mod flash;
pub mod hold;
pub mod off;

use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::hw_trait::rgb_led::RgbLed;

pub use blink::blink;
pub use breathe::{breathe, breathe_brightness};
pub use flash::flash;
pub use hold::hold;
pub use off::off;

/// Handle to a running or completed LED animation.
///
/// Provides a uniform interface regardless of whether the underlying
/// animation is a looping task, a one-shot task, or a hold whose
/// task has already completed.
pub struct AnimationHandle {
    cancel_tx: Option<oneshot::Sender<StopMode>>,
    task: JoinHandle<(Box<dyn RgbLed>, Box<dyn Resume>)>,
}

impl AnimationHandle {
    fn new(
        cancel_tx: oneshot::Sender<StopMode>,
        task: JoinHandle<(Box<dyn RgbLed>, Box<dyn Resume>)>,
    ) -> Self {
        Self {
            cancel_tx: Some(cancel_tx),
            task,
        }
    }

    /// Cancel the animation and return the LED.
    ///
    /// Stops a running task immediately. Returns at once if the
    /// task has already completed.
    pub async fn cancel(self) -> Box<dyn RgbLed> {
        let (led, _) = self.stop(StopMode::Immediate).await;
        led
    }

    /// Wait for the animation to reach a natural stopping point.
    ///
    /// For looping animations, waits until the end of the current
    /// cycle. For one-shot animations, waits for completion. For
    /// holds (already completed), returns immediately.
    pub async fn finish(self) -> Box<dyn RgbLed> {
        let (led, _) = self.stop(StopMode::FinishCycle).await;
        led
    }

    /// Temporarily interrupt this animation with a one-shot.
    ///
    /// Returns immediately. Spawns a coordinator task that cancels
    /// the current animation, runs the one-shot produced by `f`,
    /// waits for it to finish, then resumes the original animation.
    pub fn interrupt<F>(self, f: F) -> AnimationHandle
    where
        F: FnOnce(Box<dyn RgbLed>) -> AnimationHandle + Send + 'static,
    {
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let join = tokio::spawn(async move {
            let (led, state) = self.stop(StopMode::Immediate).await;
            let led = f(led).finish().await;
            let resumed = state.resume(led);
            let mode = cancel_rx.await.unwrap_or(StopMode::Immediate);
            resumed.stop(mode).await
        });
        AnimationHandle::new(cancel_tx, join)
    }

    async fn stop(mut self, mode: StopMode) -> (Box<dyn RgbLed>, Box<dyn Resume>) {
        if let Some(tx) = self.cancel_tx.take() {
            let _ = tx.send(mode);
        }
        self.task.await.expect("animation task panicked")
    }
}

/// How the animation should stop when cancelled.
enum StopMode {
    /// Exit at the next tick.
    Immediate,
    /// Finish the current cycle before exiting.
    FinishCycle,
}

/// Animation state that knows how to restart itself on a given LED.
trait Resume: Send {
    fn resume(self: Box<Self>, led: Box<dyn RgbLed>) -> AnimationHandle;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use crate::hw_trait;
    use crate::hw_trait::rgb_led::RgbColor;

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
    async fn hold_sets_color() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let mock = MockLed {
            writes: writes.clone(),
        };

        let handle = hold(Box::new(mock), RgbColor::WHITE, 1.0);
        let _led = handle.cancel().await;

        let w = writes.lock().unwrap();
        assert_eq!(w.len(), 1);
        assert_eq!(w[0], (RgbColor::WHITE, 1.0));
    }

    #[tokio::test(start_paused = true)]
    async fn flash_completes_after_duration() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let mock = MockLed {
            writes: writes.clone(),
        };

        let color = RgbColor::ORANGE;
        let handle = flash(Box::new(mock), color, Duration::from_millis(150));
        let _led = handle.finish().await;

        let writes = writes.lock().unwrap();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0], (color, 1.0));
    }

    #[tokio::test(start_paused = true)]
    async fn flash_can_be_cancelled_early() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let mock = MockLed {
            writes: writes.clone(),
        };

        let handle = flash(Box::new(mock), RgbColor::ORANGE, Duration::from_secs(10));
        let _led = handle.cancel().await;

        let writes = writes.lock().unwrap();
        assert_eq!(writes.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn interrupt_breathe_with_flash() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let mock = MockLed {
            writes: writes.clone(),
        };

        let blue = RgbColor::BLUE;
        let orange = RgbColor::ORANGE;
        let handle = breathe(Box::new(mock), blue);

        // Let it breathe for a bit
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Interrupt with a flash (non-blocking)
        let handle = handle.interrupt(move |led| flash(led, orange, Duration::from_millis(150)));

        // Let it breathe some more after flash resumes
        tokio::time::sleep(Duration::from_secs(1)).await;

        let led = handle.finish().await;

        let writes = writes.lock().unwrap();

        // Should see blue writes, then an orange flash, then blue again
        let flash_idx = writes
            .iter()
            .position(|(c, _)| *c == orange)
            .expect("expected an orange flash write");

        assert!(flash_idx > 0, "flash should not be the first write");
        assert!(
            writes[flash_idx + 1..].iter().any(|(c, _)| *c == blue),
            "expected blue writes after flash",
        );

        drop(led);
    }

    #[tokio::test(start_paused = true)]
    async fn interrupt_hold_with_flash() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let mock = MockLed {
            writes: writes.clone(),
        };

        let handle = hold(Box::new(mock), RgbColor::WHITE, 1.0);

        // Interrupt with a flash (non-blocking)
        let handle =
            handle.interrupt(|led| flash(led, RgbColor::ORANGE, Duration::from_millis(150)));

        // Wait for flash to finish and hold to resume
        tokio::time::sleep(Duration::from_secs(1)).await;

        let _led = handle.cancel().await;

        let writes = writes.lock().unwrap();

        // Should see: white (hold), orange (flash), white (resumed hold)
        let flash_idx = writes
            .iter()
            .position(|(c, _)| *c == RgbColor::ORANGE)
            .expect("expected an orange flash write");

        assert!(flash_idx > 0, "flash should not be the first write");
        assert!(
            writes[flash_idx + 1..]
                .iter()
                .any(|(c, b)| *c == RgbColor::WHITE && *b == 1.0),
            "expected white hold restored after flash",
        );
    }
}
