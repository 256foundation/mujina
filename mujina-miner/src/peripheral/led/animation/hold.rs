//! Static color hold.
//!
//! Wraps a static LED color as an [`AnimationHandle`] so that
//! callers can treat all LED states uniformly. This allows
//! operations like [`AnimationHandle::interrupt`] to work the same
//! regardless of whether the LED is animating or holding steady.

use crate::tracing::prelude::*;
use tokio::sync::oneshot;

use super::{AnimationHandle, Resume};
use crate::hw_trait::rgb_led::{RgbColor, RgbLed};

/// Hold the LED at a static color.
///
/// Spawns a brief task that sets the color and then completes.
/// Cancelling or finishing the returned handle returns immediately
/// since the task is already done.
pub fn hold(mut led: Box<dyn RgbLed>, color: RgbColor, brightness: f32) -> AnimationHandle {
    // Spawns a task because Resume::resume is not async, so this
    // function cannot be either. The task completes immediately.
    let (cancel_tx, _cancel_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        if let Err(e) = led.set(color, brightness).await {
            warn!(error = %e, "LED hold set failed");
        }
        let state = HoldState { color, brightness };
        (led as Box<dyn RgbLed>, Box::new(state) as Box<dyn Resume>)
    });
    AnimationHandle::new(cancel_tx, task)
}

/// State for resuming a held color.
struct HoldState {
    color: RgbColor,
    brightness: f32,
}

impl Resume for HoldState {
    fn resume(self: Box<Self>, led: Box<dyn RgbLed>) -> AnimationHandle {
        hold(led, self.color, self.brightness)
    }
}
