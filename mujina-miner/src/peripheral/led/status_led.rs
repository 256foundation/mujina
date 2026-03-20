//! Board status indication via RGB LED.
//!
//! Translates high-level board status into LED colors and animations.
//! The board owns a [`StatusLed`] and calls its methods directly
//! as status changes occur.

use std::time::Duration;

use crate::tracing::prelude::*;

use super::animation::{self, AnimationHandle};
use crate::hw_trait::rgb_led::{RgbColor, RgbLed};

/// High-level board status for LED indication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Initializing,
    Idle,
    Hashing,
    Fault,
    Identify,
}

/// Indicates board status using an RGB LED.
///
/// The board owns this directly and calls methods as status changes.
/// All states are represented as animations (including static colors
/// via [`animation::hold`]), providing a uniform interface for
/// interruption and resumption.
pub struct StatusLed {
    handle: Option<AnimationHandle>,
    status: Status,
}

impl StatusLed {
    /// Create a new status indicator with the given initial status
    pub fn new(led: Box<dyn RgbLed>, status: Status) -> Self {
        let mut this = Self {
            handle: None,
            status,
        };
        this.apply_status(status, led);
        this
    }

    /// Update the LED to reflect a new board status.
    pub async fn set(&mut self, status: Status) {
        trace!(?status, "LED status change");
        let led = self.take_led().await;
        self.apply_status(status, led);
    }

    /// Turn the LED off, stopping any animation.
    pub async fn off(&mut self) {
        let led = self.take_led().await;
        self.handle = Some(animation::off(led));
    }

    /// Flash the LED briefly for a found share.
    pub fn flash_share(&mut self) {
        const FLASH_DURATION: Duration = Duration::from_millis(150);

        let handle = self.handle.take().expect("handle taken");
        self.handle =
            Some(handle.interrupt(|led| animation::flash(led, RgbColor::ORANGE, FLASH_DURATION)));
    }

    /// Cancel the current animation and return the LED.
    async fn take_led(&mut self) -> Box<dyn RgbLed> {
        self.handle.take().expect("handle taken").cancel().await
    }

    /// Start the animation for the given status.
    fn apply_status(&mut self, status: Status, led: Box<dyn RgbLed>) {
        const FAULT_BLINK_PHASE: Duration = Duration::from_millis(500);
        const IDENTIFY_BLINK_PHASE: Duration = Duration::from_millis(125);

        self.status = status;

        self.handle = Some(match status {
            Status::Initializing => animation::breathe(led, RgbColor::ORANGE),
            Status::Hashing => animation::breathe(led, RgbColor::WHITE),
            Status::Identify => animation::blink(led, RgbColor::BLUE, IDENTIFY_BLINK_PHASE),
            Status::Idle => animation::hold(led, RgbColor::WHITE, 1.0),
            Status::Fault => animation::blink(led, RgbColor::RED, FAULT_BLINK_PHASE),
        });
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

    #[tokio::test(start_paused = true)]
    async fn static_states_set_led_directly() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let mock = MockLed {
            writes: writes.clone(),
        };

        let mut status_led = StatusLed::new(Box::new(mock), Status::Idle);

        // Let the hold task set the color
        tokio::task::yield_now().await;
        // Idle is solid white
        let last = writes.lock().unwrap().last().copied().unwrap();
        assert_eq!(last, (RgbColor::WHITE, 1.0));

        status_led.off().await;
    }

    #[tokio::test(start_paused = true)]
    async fn flash_share_during_static() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let mock = MockLed {
            writes: writes.clone(),
        };

        let mut status_led = StatusLed::new(Box::new(mock), Status::Idle);

        // Let the hold task set the color
        tokio::task::yield_now().await;
        let before_count = writes.lock().unwrap().len();
        status_led.flash_share();

        // Let the flash run and hold resume
        tokio::time::sleep(Duration::from_secs(1)).await;

        let writes = writes.lock().unwrap();
        // Should have flash color followed by restored idle color
        let flash_write = writes[before_count..]
            .iter()
            .find(|(c, _)| *c == RgbColor::ORANGE);
        assert!(flash_write.is_some(), "expected a flash write");

        let last = writes.last().unwrap();
        assert_eq!(*last, (RgbColor::WHITE, 1.0), "should restore idle white");
    }

    #[tokio::test(start_paused = true)]
    async fn flash_share_during_breathing() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let mock = MockLed {
            writes: writes.clone(),
        };

        let mut status_led = StatusLed::new(Box::new(mock), Status::Hashing);

        // Let it breathe for a bit
        tokio::time::sleep(Duration::from_secs(1)).await;

        status_led.flash_share();

        // Let it breathe some more after flash
        tokio::time::sleep(Duration::from_secs(1)).await;

        let writes = writes.lock().unwrap();

        // Should see white (breathing), then orange (flash), then white again (resumed)
        let flash_idx = writes
            .iter()
            .position(|(c, _)| *c == RgbColor::ORANGE)
            .expect("expected an orange flash write");

        assert!(
            writes[flash_idx + 1..]
                .iter()
                .any(|(c, _)| *c == RgbColor::WHITE),
            "expected white writes after flash (breathing resumed)",
        );

        drop(writes);
        status_led.off().await;
    }
}
