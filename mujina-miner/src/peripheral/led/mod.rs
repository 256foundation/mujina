//! LED animations and board status indication.

pub mod animation;
pub mod calibrated;
pub mod status_led;

pub use calibrated::{CalibratedLed, ColorProfile};
pub use status_led::{Status, StatusLed};
