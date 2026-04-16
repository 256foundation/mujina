//! Per-LED color calibration wrapper.
//!
//! Wraps an [`RgbLed`] and applies per-channel scaling, gamma
//! correction, and brightness limiting before forwarding to the
//! inner LED.
//! Each board can supply its own [`ColorProfile`] to compensate for
//! differences in LED color balance.

use async_trait::async_trait;

use crate::hw_trait::rgb_led::{RgbColor, RgbLed};

/// LED wrapper that applies a [`ColorProfile`].
pub struct CalibratedLed {
    inner: Box<dyn RgbLed>,
    profile: ColorProfile,
}

impl CalibratedLed {
    pub fn new(inner: Box<dyn RgbLed>, profile: ColorProfile) -> Self {
        Self { inner, profile }
    }
}

#[async_trait]
impl RgbLed for CalibratedLed {
    async fn set(&mut self, color: RgbColor, brightness: f32) -> crate::hw_trait::Result<()> {
        let p = &self.profile;
        let brightness = brightness.clamp(0.0, 1.0) * p.brightness;

        let final_color = RgbColor {
            r: channel(color.r, p.scale_r, brightness, p.gamma),
            g: channel(color.g, p.scale_g, brightness, p.gamma),
            b: channel(color.b, p.scale_b, brightness, p.gamma),
        };
        // Brightness is already baked into the channel values.
        self.inner.set(final_color, 1.0).await
    }
}

/// Compute a final channel value with scaling, gamma, and
/// brightness baked in.
fn channel(value: u8, scale: f32, brightness: f32, gamma: f32) -> u8 {
    let linear = (value as f32 / 255.0) * scale * brightness;
    let corrected = linear.clamp(0.0, 1.0).powf(gamma);
    (corrected * 255.0).round() as u8
}

/// Per-LED color and brightness calibration.
///
/// Corrections are applied in order:
///
/// 1. **Per-channel scale** (linear) -- compensates for uneven
///    channel intensities in the LED hardware. Because scaling is
///    linear, channel ratios are preserved at all brightness levels.
/// 2. **Gamma** -- perceptual correction applied after scaling
///    to compensate for the nonlinear brightness perception of
///    the human eye.
/// 3. **Brightness** -- overall brightness cap.
#[derive(Debug, Clone)]
pub struct ColorProfile {
    /// Maximum brightness (0.0..=1.0).
    pub brightness: f32,
    /// Gamma exponent applied after scaling.
    ///
    /// Compensates for the nonlinear brightness perception of the
    /// human eye. 1.0 is linear; 2.0-3.0 is typical for LEDs.
    pub gamma: f32,
    /// Linear scale factor for red (0.0..=1.0).
    pub scale_r: f32,
    /// Linear scale factor for green (0.0..=1.0).
    pub scale_g: f32,
    /// Linear scale factor for blue (0.0..=1.0).
    pub scale_b: f32,
}

impl ColorProfile {
    /// Profile for SK6812 RGB LEDs.
    ///
    /// Per-channel scale factors from FastLED's TypicalSMD5050
    /// color correction (0xFF, 0xB0, 0xF0), normalized to the
    /// strongest channel (red). Gamma 2.5 is the FastLED default
    /// for perceptual brightness correction on linear-PWM LEDs.
    pub const SK6812: Self = Self {
        brightness: 0.10,
        gamma: 2.5,
        scale_r: 1.0,
        scale_g: 0.69,
        scale_b: 0.94,
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_identity() {
        assert_eq!(channel(0, 1.0, 1.0, 1.0), 0);
        assert_eq!(channel(255, 1.0, 1.0, 1.0), 255);
        assert_eq!(channel(128, 1.0, 1.0, 1.0), 128);
    }

    #[test]
    fn channel_scales_linearly() {
        // With gamma 1.0, scaling is linear
        assert_eq!(channel(255, 0.5, 1.0, 1.0), 128);
        assert_eq!(channel(128, 0.5, 1.0, 1.0), 64);
    }

    #[test]
    fn channel_applies_brightness() {
        assert_eq!(channel(255, 1.0, 0.5, 1.0), 128);
        assert_eq!(channel(255, 0.5, 0.5, 1.0), 64);
    }

    #[test]
    fn gamma_compresses_output() {
        // Gamma > 1.0 darkens intermediate values
        assert!(channel(128, 1.0, 1.0, 2.5) < 128);
        // But preserves 0 and 255
        assert_eq!(channel(0, 1.0, 1.0, 2.5), 0);
        assert_eq!(channel(255, 1.0, 1.0, 2.5), 255);
    }
}
