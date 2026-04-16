//! TMP451 remote and local temperature sensor driver.
//!
//! Driver for the Texas Instruments TMP451, a dual-channel 12-bit I2C
//! temperature sensor with a local channel (die temperature) and a
//! remote channel (external diode). Both channels have 0.0625 C
//! resolution.
//!
//! Datasheet: <https://www.ti.com/lit/ds/symlink/tmp451.pdf>

use crate::hw_trait::{HwError, i2c::I2c};

/// Expected manufacturer ID (register 0xFE).
pub const MANUFACTURER_ID: u8 = 0x55;

/// TMP451 register addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Register {
    /// Local temperature, high byte (integer, read-only)
    LocalTempHigh = 0x00,
    /// Remote temperature, high byte (integer, read-only)
    RemoteTempHigh = 0x01,
    /// Status (read-only)
    Status = 0x02,
    /// Configuration
    Config = 0x03,
    /// Remote temperature, low byte (fractional, read-only)
    RemoteTempLow = 0x10,
    /// Remote temperature offset, high byte (1 C/LSB, read/write)
    RemoteOffsetHigh = 0x11,
    /// Local temperature, low byte (fractional, read-only)
    LocalTempLow = 0x15,
    /// N-factor correction (two's complement, read/write)
    NFactorCorrection = 0x23,
    /// Manufacturer ID (read-only)
    MfgId = 0xFE,
}

/// Driver error.
#[derive(Debug, thiserror::Error)]
pub enum Error<E> {
    /// I2C bus error
    #[error("I2C: {0}")]
    I2c(E),

    /// Manufacturer ID register returned an unexpected value
    #[error("unexpected manufacturer ID: 0x{0:02X}")]
    UnexpectedMfgId(u8),

    /// Remote diode is open or faulted
    #[error("remote diode open")]
    RemoteDiodeOpen,
}

/// A raw temperature reading from the TMP451.
///
/// Wraps a 12-bit value assembled from the high byte (integer part)
/// and the upper 4 bits of the low byte (fractional part). In
/// standard binary mode, values below 0 C read as zero. Each LSB
/// represents 0.0625 C.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reading(i16);

impl Reading {
    const DEGREES_PER_LSB: f32 = 0.0625;

    /// Assemble from separate high and low register bytes.
    ///
    /// High byte is the integer part (unsigned in standard mode).
    /// Low byte upper 4 bits are the fractional part.
    pub fn from_high_low(high: u8, low: u8) -> Self {
        let raw = ((high as i16) << 4) | ((low >> 4) as i16);
        Self(raw)
    }

    /// Temperature in degrees Celsius.
    pub fn as_degrees_c(self) -> f32 {
        self.0 as f32 * Self::DEGREES_PER_LSB
    }
}

type Result<T> = std::result::Result<T, Error<HwError>>;

/// TMP451 driver, generic over I2C implementation.
pub struct Tmp451<I> {
    i2c: I,
    address: u8,
}

impl<I: I2c> Tmp451<I> {
    /// Create a new driver instance.
    pub fn new(i2c: I, address: u8) -> Self {
        Self { i2c, address }
    }

    /// Verify the manufacturer ID register.
    pub async fn init(&mut self) -> Result<()> {
        let id = self.read_byte(Register::MfgId).await?;
        if id != MANUFACTURER_ID {
            return Err(Error::UnexpectedMfgId(id));
        }
        Ok(())
    }

    /// Read the local (on-die) temperature.
    pub async fn read_local(&mut self) -> Result<Reading> {
        let high = self.read_byte(Register::LocalTempHigh).await?;
        let low = self.read_byte(Register::LocalTempLow).await?;
        Ok(Reading::from_high_low(high, low))
    }

    /// Read the remote (external diode) temperature.
    ///
    /// Returns `Error::RemoteDiodeOpen` if the diode is disconnected
    /// or faulted.
    pub async fn read_remote(&mut self) -> Result<Reading> {
        const REMOTE_OPEN: u8 = 0x04;
        let s = self.read_byte(Register::Status).await?;
        if s & REMOTE_OPEN != 0 {
            return Err(Error::RemoteDiodeOpen);
        }
        let high = self.read_byte(Register::RemoteTempHigh).await?;
        let low = self.read_byte(Register::RemoteTempLow).await?;
        Ok(Reading::from_high_low(high, low))
    }

    /// Set the remote temperature offset (high byte, 1 C/LSB).
    ///
    /// The offset is added to every remote ADC conversion result.
    /// Use a negative value to reduce a reading that is too high.
    pub async fn set_remote_offset(&mut self, degrees: i8) -> Result<()> {
        self.write_byte(Register::RemoteOffsetHigh, degrees as u8)
            .await
    }

    async fn read_byte(&mut self, reg: Register) -> Result<u8> {
        let mut buf = [0u8; 1];
        self.i2c
            .write_read(self.address, &[reg as u8], &mut buf)
            .await
            .map_err(Error::I2c)?;
        Ok(buf[0])
    }

    async fn write_byte(&mut self, reg: Register, value: u8) -> Result<()> {
        self.i2c
            .write(self.address, &[reg as u8, value])
            .await
            .map_err(Error::I2c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reading_from_high_low() {
        // Datasheet Table 7-1 / 7-2 values
        let r = Reading::from_high_low(0x00, 0x00);
        assert!((r.as_degrees_c() - 0.0).abs() < 1e-4);

        // Fractional bits: 25 + one LSB
        let r = Reading::from_high_low(0x19, 0x10);
        assert!((r.as_degrees_c() - 25.0625).abs() < 1e-4);

        // Max: 127.9375 C
        let r = Reading::from_high_low(0x7F, 0xF0);
        assert!((r.as_degrees_c() - 127.9375).abs() < 1e-4);
    }

    #[test]
    fn low_byte_bottom_nibble_ignored() {
        // Lower 4 bits of low byte are unused
        let a = Reading::from_high_low(0x19, 0x80);
        let b = Reading::from_high_low(0x19, 0x8F);
        assert_eq!(a, b);
    }
}
