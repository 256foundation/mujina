//! LED implementation using bitaxe-raw control protocol.

use crate::tracing::prelude::*;
use async_trait::async_trait;

use super::channel::ControlChannel;
use super::{LEDCommand, Packet, Page};
use crate::hw_trait::Result;
use crate::hw_trait::rgb_led::{RgbColor, RgbLed};

/// RGB LED controller using bitaxe-raw control protocol.
///
/// Wraps a [`ControlChannel`] and sends SetRGB packets to control an
/// SK6812 LED. Brightness scaling is applied before transmission.
#[derive(Clone)]
pub struct BitaxeRawLed {
    channel: ControlChannel,
}

impl BitaxeRawLed {
    /// Create a new LED controller using the given control channel.
    pub fn new(channel: ControlChannel) -> Self {
        Self { channel }
    }
}

/// Scale a single color channel by brightness.
fn scale(value: u8, brightness: f32) -> u8 {
    (value as f32 * brightness).round() as u8
}

#[async_trait]
impl RgbLed for BitaxeRawLed {
    async fn set(&mut self, color: RgbColor, brightness: f32) -> Result<()> {
        let brightness = brightness.clamp(0.0, 1.0);
        let r = scale(color.r, brightness);
        let g = scale(color.g, brightness);
        let b = scale(color.b, brightness);
        trace!(r, g, b, "LED set");
        let packet = Packet::new(Page::LED, LEDCommand::SetRGB as u8, vec![r, g, b]);
        self.channel.send_packet(packet).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn led_set_rgb_packet_encoding() {
        let packet = Packet::new(Page::LED, LEDCommand::SetRGB as u8, vec![0xff, 0x80, 0x00]);
        let encoded = packet.encode();

        assert_eq!(encoded[0], 0x09); // length low: 6 header + 3 data
        assert_eq!(encoded[1], 0x00); // length high
        assert_eq!(encoded[3], 0x00); // bus
        assert_eq!(encoded[4], 0x08); // LED page
        assert_eq!(encoded[5], 0x10); // SetRGB command
        assert_eq!(encoded[6], 0xff); // r
        assert_eq!(encoded[7], 0x80); // g
        assert_eq!(encoded[8], 0x00); // b
    }

    #[test]
    fn brightness_scaling() {
        assert_eq!(scale(255, 0.5), 128);
        assert_eq!(scale(128, 0.5), 64);
        assert_eq!(scale(0, 0.5), 0);
        assert_eq!(scale(255, 1.0), 255);
        assert_eq!(scale(255, 0.0), 0);
    }
}
