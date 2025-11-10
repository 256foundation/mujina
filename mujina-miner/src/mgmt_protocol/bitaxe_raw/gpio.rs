//! GPIO implementation using bitaxe-raw control protocol.

use async_trait::async_trait;

use super::channel::ControlChannel;
use super::Packet;
use crate::hw_trait::gpio::{Gpio, GpioPin, PinMode, PinValue};
use crate::hw_trait::{HwError, Result};

/// GPIO controller using bitaxe-raw control protocol.
///
/// The controller provides access to individual GPIO pins. For shared access
/// to specific pins (e.g., between board and thread), create pin handles and
/// clone them rather than sharing the controller.
#[derive(Clone)]
pub struct BitaxeRawGpioController {
    channel: ControlChannel,
}

impl BitaxeRawGpioController {
    /// Create a new GPIO controller using the given control channel.
    pub fn new(channel: ControlChannel) -> Self {
        Self { channel }
    }
}

#[async_trait]
impl Gpio for BitaxeRawGpioController {
    type Pin = BitaxeRawGpioPin;

    async fn pin(&mut self, number: u8) -> Result<Self::Pin> {
        Ok(BitaxeRawGpioPin {
            channel: self.channel.clone(),
            number,
        })
    }
}

/// GPIO pin handle using bitaxe-raw control protocol.
///
/// Pin handles are stateless and Clone-able - they wrap a shared ControlChannel
/// and a pin number. Multiple handles to the same pin can coexist safely since
/// operations coordinate through the underlying synchronized channel.
#[derive(Clone)]
pub struct BitaxeRawGpioPin {
    channel: ControlChannel,
    number: u8,
}

#[async_trait]
impl GpioPin for BitaxeRawGpioPin {
    async fn set_mode(&mut self, mode: PinMode) -> Result<()> {
        // The bitaxe-raw protocol doesn't support setting pin modes
        // GPIO pins are assumed to be correctly configured by firmware
        // The bitaxe-raw protocol doesn't support setting pin modes
        // GPIO pins are assumed to be correctly configured by firmware
        let _ = mode;
        Ok(())
    }

    async fn write(&mut self, value: PinValue) -> Result<()> {
        let packet = Packet::gpio_write(0, self.number, value.into());
        self.channel.send_packet(packet).await?;
        Ok(())
    }

    async fn read(&mut self) -> Result<PinValue> {
        let packet = Packet::gpio_read(0, self.number);
        let response = self.channel.send_packet(packet).await?;

        // Response should contain one byte
        if response.data.len() != 1 {
            return Err(HwError::InvalidParameter(format!(
                "Expected 1 byte in GPIO read response, got {}",
                response.data.len()
            )));
        }

        Ok(if response.data[0] != 0 {
            PinValue::High
        } else {
            PinValue::Low
        })
    }
}
