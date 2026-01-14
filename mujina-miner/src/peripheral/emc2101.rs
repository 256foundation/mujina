//! EMC2101 PWM fan controller and temperature sensor driver.
//!
//! The EMC2101 is an I2C fan controller with integrated temperature sensing.
//! It can monitor external temperature via a diode-connected transistor and
//! control fan speed using PWM output.
//!
//! Datasheet: <https://www.microchip.com/en-us/product/emc2101>

use crate::{
    hw_trait::{i2c::I2c, HwError, Result},
    tracing::prelude::*,
};

/// Default I2C address for EMC2101
pub const DEFAULT_ADDRESS: u8 = 0x4C;

/// Fan speed percentage (0-100)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Percent(u8);

impl Percent {
    /// Creates a new percentage, clamping to 0-100 range
    pub const fn new_clamped(value: u8) -> Self {
        if value > 100 {
            Self(100)
        } else {
            Self(value)
        }
    }

    /// Creates a new percentage if value is in range
    pub const fn new(value: u8) -> Option<Self> {
        if value <= 100 {
            Some(Self(value))
        } else {
            None
        }
    }

    /// Zero speed (0%)
    pub const ZERO: Self = Self(0);

    /// Full speed (100%)
    pub const FULL: Self = Self(100);

    /// Calculate this percentage of a value
    pub const fn of(self, value: u8) -> u8 {
        ((self.0 as u16 * value as u16) / 100) as u8
    }
}

impl From<Percent> for u8 {
    fn from(p: Percent) -> u8 {
        p.0
    }
}

impl TryFrom<u8> for Percent {
    type Error = HwError;

    fn try_from(value: u8) -> Result<Self> {
        Self::new(value).ok_or_else(|| {
            HwError::InvalidParameter(format!("Percent value {} out of range 0-100", value))
        })
    }
}

/// Protocol dissection utilities for EMC2101
pub mod protocol {
    /// Default I2C address for EMC2101
    pub const DEFAULT_ADDRESS: u8 = 0x4C;

    /// Expected manufacturer ID
    pub const EXPECTED_MFG_ID: u8 = 0x5D;

    /// EMC2101 register addresses
    pub mod regs {
        /// Internal temperature reading
        pub const INTERNAL_TEMP: u8 = 0x00;
        /// External temperature reading high byte
        pub const EXTERNAL_TEMP_HIGH: u8 = 0x01;
        /// External temperature reading low byte
        pub const EXTERNAL_TEMP_LOW: u8 = 0x10;
        /// Configuration register
        pub const CONFIG: u8 = 0x03;
        /// Conversion rate register
        pub const CONVERSION_RATE: u8 = 0x04;
        /// Internal temperature high limit
        pub const INTERNAL_TEMP_LIMIT: u8 = 0x05;
        /// External temperature high limit high byte
        pub const EXTERNAL_TEMP_LIMIT_HIGH: u8 = 0x07;
        /// External temperature high limit low byte
        pub const EXTERNAL_TEMP_LIMIT_LOW: u8 = 0x13;
        /// Fan configuration register
        pub const FAN_CONFIG: u8 = 0x4A;
        /// Fan spin-up configuration
        pub const FAN_SPINUP: u8 = 0x4B;
        /// Fan setting register (PWM duty cycle)
        pub const FAN_SETTING: u8 = 0x4C;
        /// PWM frequency register
        pub const PWM_FREQ: u8 = 0x4D;
        /// PWM frequency divide register
        pub const PWM_DIV: u8 = 0x4E;
        /// Fan minimum drive register
        pub const FAN_MIN_DRIVE: u8 = 0x55;
        /// Fan valid TACH count
        pub const FAN_VALID_TACH: u8 = 0x58;
        /// Fan drive fail band low byte
        pub const FAN_FAIL_BAND_LOW: u8 = 0x5A;
        /// Fan drive fail band high byte
        pub const FAN_FAIL_BAND_HIGH: u8 = 0x5B;
        /// TACH reading low byte (LSB)
        pub const TACH_LOW: u8 = 0x46;
        /// TACH reading high byte (MSB)
        pub const TACH_HIGH: u8 = 0x47;
        /// TACH limit high byte
        pub const TACH_LIMIT_HIGH: u8 = 0x48;
        /// TACH limit low byte
        pub const TACH_LIMIT_LOW: u8 = 0x49;
        /// Product ID register
        pub const PRODUCT_ID: u8 = 0xFD;
        /// Manufacturer ID register
        pub const MFG_ID: u8 = 0xFE;
        /// Revision register
        pub const REVISION: u8 = 0xFF;
    }

    /// Get register name from address
    pub fn register_name(addr: u8) -> String {
        match addr {
            regs::INTERNAL_TEMP => "INTERNAL_TEMP".to_string(),
            regs::EXTERNAL_TEMP_HIGH => "EXTERNAL_TEMP_HIGH".to_string(),
            regs::EXTERNAL_TEMP_LOW => "EXTERNAL_TEMP_LOW".to_string(),
            regs::CONFIG => "CONFIG".to_string(),
            regs::CONVERSION_RATE => "CONVERSION_RATE".to_string(),
            regs::INTERNAL_TEMP_LIMIT => "INTERNAL_TEMP_LIMIT".to_string(),
            regs::EXTERNAL_TEMP_LIMIT_HIGH => "EXTERNAL_TEMP_LIMIT_HIGH".to_string(),
            regs::EXTERNAL_TEMP_LIMIT_LOW => "EXTERNAL_TEMP_LIMIT_LOW".to_string(),
            regs::FAN_CONFIG => "FAN_CONFIG".to_string(),
            regs::FAN_SPINUP => "FAN_SPINUP".to_string(),
            regs::FAN_SETTING => "FAN_SETTING".to_string(),
            regs::PWM_FREQ => "PWM_FREQ".to_string(),
            regs::PWM_DIV => "PWM_DIV".to_string(),
            regs::FAN_MIN_DRIVE => "FAN_MIN_DRIVE".to_string(),
            regs::FAN_VALID_TACH => "FAN_VALID_TACH".to_string(),
            regs::FAN_FAIL_BAND_LOW => "FAN_FAIL_BAND_LOW".to_string(),
            regs::FAN_FAIL_BAND_HIGH => "FAN_FAIL_BAND_HIGH".to_string(),
            regs::TACH_LOW => "TACH_LOW".to_string(),
            regs::TACH_HIGH => "TACH_HIGH".to_string(),
            regs::TACH_LIMIT_HIGH => "TACH_LIMIT_HIGH".to_string(),
            regs::TACH_LIMIT_LOW => "TACH_LIMIT_LOW".to_string(),
            regs::PRODUCT_ID => "PRODUCT_ID".to_string(),
            regs::MFG_ID => "MFG_ID".to_string(),
            regs::REVISION => "REVISION".to_string(),
            _ => format!("UNKNOWN[0x{:02x}]", addr),
        }
    }

    /// Decode temperature value (signed 8-bit)
    pub fn decode_temperature(value: u8) -> String {
        let temp = value as i8;
        format!("{} degC", temp)
    }

    /// Decode PWM duty cycle to percentage
    pub fn decode_pwm_percent(value: u8) -> String {
        let percent = (value as f32 / 255.0 * 100.0) as u8;
        format!("{}% (0x{:02x})", percent, value)
    }

    /// Format an EMC2101 I2C transaction
    pub fn format_transaction(reg: u8, data: Option<&[u8]>, is_read: bool) -> String {
        let reg_name = register_name(reg);

        if is_read {
            if let Some(data) = data {
                if data.len() == 1 {
                    let decoded = match reg {
                        regs::INTERNAL_TEMP | regs::EXTERNAL_TEMP_HIGH => {
                            decode_temperature(data[0])
                        }
                        regs::FAN_SETTING | regs::FAN_MIN_DRIVE => decode_pwm_percent(data[0]),
                        regs::MFG_ID => format!(
                            "0x{:02x} ({})",
                            data[0],
                            if data[0] == EXPECTED_MFG_ID {
                                "SMSC/Microchip"
                            } else {
                                "Unknown"
                            }
                        ),
                        regs::CONVERSION_RATE => {
                            let rate_desc = match data[0] {
                                0x00 => "1/16 Hz",
                                0x01 => "1/8 Hz",
                                0x02 => "1/4 Hz",
                                0x03 => "1/2 Hz",
                                0x04 => "1 Hz",
                                0x05 => "2 Hz",
                                0x06 => "4 Hz",
                                0x07 => "8 Hz",
                                0x08 => "16 Hz",
                                0x09 => "32 Hz",
                                _ => "unknown",
                            };
                            format!("0x{:02x} ({})", data[0], rate_desc)
                        }
                        regs::INTERNAL_TEMP_LIMIT => {
                            format!("0x{:02x} ({} degC)", data[0], data[0] as i8)
                        }
                        _ => format!("0x{:02x}", data[0]),
                    };
                    format!("-> READ {}={}", reg_name, decoded)
                } else {
                    format!("-> READ {}={:02x?}", reg_name, data)
                }
            } else {
                format!("-> READ {}", reg_name)
            }
        } else if let Some(data) = data {
            if data.len() == 1 {
                let decoded = match reg {
                    regs::FAN_SETTING | regs::FAN_MIN_DRIVE => decode_pwm_percent(data[0]),
                    _ => format!("0x{:02x}", data[0]),
                };
                format!("<- WRITE {}={}", reg_name, decoded)
            } else {
                format!("<- WRITE {}={:02x?}", reg_name, data)
            }
        } else {
            format!("<- WRITE REG[0x{:02x}]", reg)
        }
    }
}

// Re-export register constants for driver compatibility
use protocol::regs;

/// EMC2101 driver
pub struct Emc2101<I: I2c> {
    i2c: I,
    address: u8,
}

impl<I: I2c> Emc2101<I> {
    /// EMC2101 uses 6-bit PWM duty cycle (0-63 = 0-100%)
    const PWM_MAX: u8 = 63;

    /// Create a new EMC2101 driver with default address
    pub fn new(i2c: I) -> Self {
        Self {
            i2c,
            address: DEFAULT_ADDRESS,
        }
    }

    /// Create a new EMC2101 driver with custom address
    pub fn new_with_address(i2c: I, address: u8) -> Self {
        Self { i2c, address }
    }

    /// Initialize the EMC2101 for basic operation
    pub async fn init(&mut self) -> Result<()> {
        // Verify chip ID
        let mfg_id = self.read_register(regs::MFG_ID).await?;
        let product_id = self.read_register(regs::PRODUCT_ID).await?;
        let revision = self.read_register(regs::REVISION).await?;

        // Expected manufacturer ID for SMSC/Microchip
        const EXPECTED_MFG_ID: u8 = 0x5D;
        if mfg_id != EXPECTED_MFG_ID {
            return Err(HwError::InvalidParameter(format!(
                "Wrong manufacturer ID: 0x{:02X}, expected 0x{:02X}",
                mfg_id, EXPECTED_MFG_ID
            )));
        }

        // Valid product IDs for EMC2101
        const PRODUCT_ID_EMC2101_1: u8 = 0x16;
        const PRODUCT_ID_EMC2101_2: u8 = 0x28;
        if product_id != PRODUCT_ID_EMC2101_1 && product_id != PRODUCT_ID_EMC2101_2 {
            return Err(HwError::InvalidParameter(format!(
                "Wrong product ID: 0x{:02X}, expected 0x{:02X} or 0x{:02X}",
                product_id, PRODUCT_ID_EMC2101_1, PRODUCT_ID_EMC2101_2
            )));
        }

        debug!(
            mfg_id = format!("{:#04x}", mfg_id),
            product_id = format!("{:#04x}", product_id),
            revision = format!("{:#04x}", revision),
            "Detected EMC2101 variant"
        );

        // Read current CONFIG register to preserve other bits
        let mut config = self.read_register(regs::CONFIG).await?;
        trace!("Current CONFIG register: 0x{:02X}", config);

        // Enable TACH input in CONFIG register
        // Bit 2 = 1: Enable TACH input
        const CONFIG_TACH_ENABLE_BIT: u8 = 0x04;
        config |= CONFIG_TACH_ENABLE_BIT;
        self.write_register(regs::CONFIG, config).await?;
        trace!("Updated CONFIG register to: 0x{:02X}", config);

        // Small delay after enabling TACH for it to stabilize
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Configure for PWM control mode
        // Enable PWM, disable RPM mode
        // Bits 1:0 control PWM frequency: 11 = ~22.5kHz (as used by esp-miner)
        const FAN_CONFIG_PWM_MODE: u8 = 0x23;
        self.write_register(regs::FAN_CONFIG, FAN_CONFIG_PWM_MODE)
            .await?;

        Ok(())
    }

    /// Set fan speed
    pub async fn set_fan_speed(&mut self, speed_percent: Percent) -> Result<()> {
        let duty = speed_percent.of(Self::PWM_MAX);
        self.write_register(regs::FAN_SETTING, duty).await
    }

    /// Get fan speed
    pub async fn get_fan_speed(&mut self) -> Result<Percent> {
        let duty = self.read_register(regs::FAN_SETTING).await?;
        let percent = ((duty as u16 * 100) / Self::PWM_MAX as u16) as u8;
        Ok(Percent::new_clamped(percent))
    }

    /// Read external temperature in Celsius
    /// This is typically connected to the ASIC's temperature diode
    pub async fn get_external_temperature(&mut self) -> Result<f32> {
        let high = self.read_register(regs::EXTERNAL_TEMP_HIGH).await?;
        let low = self.read_register(regs::EXTERNAL_TEMP_LOW).await?;

        // Temperature is in 11-bit format with 0.125 degC resolution
        // High byte is integer part, low byte bits 7-5 are fractional
        const FRACTION_BITS: u8 = 3;
        const FRACTION_SHIFT: u8 = 5;
        let raw = ((high as u16) << FRACTION_BITS) | ((low as u16) >> FRACTION_SHIFT);

        // Convert to Celsius
        const RESOLUTION: f32 = 0.125; // degC per LSB
        const SIGN_BIT: u16 = 0x400; // 11-bit sign bit
        const VALUE_MASK: u16 = 0x7FF; // 11-bit mask

        let temp = if raw & SIGN_BIT != 0 {
            // Negative temperature (11-bit two's complement)
            -(((!raw & VALUE_MASK) + 1) as f32) * RESOLUTION
        } else {
            (raw as f32) * RESOLUTION
        };

        Ok(temp)
    }

    /// Read internal temperature in Celsius
    pub async fn get_internal_temperature(&mut self) -> Result<f32> {
        let raw = self.read_register(regs::INTERNAL_TEMP).await?;

        // Internal temp is 8-bit signed with 1 degC resolution
        Ok(raw as i8 as f32)
    }

    /// Read TACH count (fan speed measurement)
    /// Returns raw TACH count - convert to RPM based on fan specs
    pub async fn get_tach_count(&mut self) -> Result<u16> {
        let high = self.read_register(regs::TACH_HIGH).await?;
        let low = self.read_register(regs::TACH_LOW).await?;

        let count = ((high as u16) << 8) | (low as u16);
        trace!(
            "TACH registers: HIGH=0x{:02X}, LOW=0x{:02X}, combined=0x{:04X}",
            high,
            low,
            count
        );

        Ok(count)
    }

    /// Get fan RPM
    /// Uses the simplified formula from esp-miner: RPM = 5400000 / TACH_count
    pub async fn get_rpm(&mut self) -> Result<u32> {
        let tach = self.get_tach_count().await?;

        const TACH_ERROR_VALUE: u16 = 0xFFFF; // Indicates fan stopped/error
        if tach == 0 || tach == TACH_ERROR_VALUE {
            return Ok(0); // Fan stopped or error
        }

        // EMC2101 constant for RPM calculation (from esp-miner)
        const EMC2101_FAN_RPM_NUMERATOR: u32 = 5_400_000;
        let rpm = EMC2101_FAN_RPM_NUMERATOR / (tach as u32);

        // esp-miner returns 0 if RPM is exactly 82 (not sure why)
        const INVALID_RPM: u32 = 82;
        if rpm == INVALID_RPM {
            return Ok(0);
        }

        Ok(rpm)
    }

    // Helper methods for register access

    async fn read_register(&mut self, reg: u8) -> Result<u8> {
        let mut buf = [0u8; 1];
        self.i2c.write_read(self.address, &[reg], &mut buf).await?;
        Ok(buf[0])
    }

    async fn write_register(&mut self, reg: u8, value: u8) -> Result<()> {
        self.i2c.write(self.address, &[reg, value]).await
    }
}
