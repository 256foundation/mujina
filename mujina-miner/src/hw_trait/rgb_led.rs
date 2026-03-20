//! RGB LED hardware abstraction trait.

use super::Result;
use async_trait::async_trait;

/// 24-bit RGB color value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl RgbColor {
    pub const BLACK: Self = Self { r: 0, g: 0, b: 0 };
    pub const WHITE: Self = Self {
        r: 255,
        g: 255,
        b: 255,
    };
    pub const RED: Self = Self { r: 255, g: 0, b: 0 };
    pub const GREEN: Self = Self { r: 0, g: 255, b: 0 };
    pub const BLUE: Self = Self { r: 0, g: 0, b: 255 };

    /// Saturated orange, distinct from white on status LEDs.
    pub const ORANGE: Self = Self {
        r: 255,
        g: 40,
        b: 0,
    };
}

/// RGB LED control abstraction.
///
/// Color and brightness are separate parameters. Implementations
/// scale RGB channels by brightness before sending to hardware.
#[async_trait]
pub trait RgbLed: Send + Sync {
    /// Set LED color and brightness.
    ///
    /// `color` specifies the base color. `brightness` (0.0..=1.0)
    /// scales each channel before writing to hardware.
    async fn set(&mut self, color: RgbColor, brightness: f32) -> Result<()>;

    /// Turn the LED off.
    async fn off(&mut self) -> Result<()> {
        self.set(RgbColor::BLACK, 0.0).await
    }
}
