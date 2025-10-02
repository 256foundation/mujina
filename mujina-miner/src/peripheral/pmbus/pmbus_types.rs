//! Type-safe PMBus Linear format implementations
//!
//! This module provides type-safe wrappers for PMBus Linear11 and Linear16 formats,
//! along with VOUT_MODE handling for proper voltage encoding/decoding.

use super::{extract_5bit_exponent, PMBusError};

/// PMBus VOUT_MODE format enumeration
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VoutModeFormat {
    Linear,
    Vid,
    Direct,
    Ieee754Half,
    Reserved(u8),
}

/// PMBus VOUT_MODE register (command 0x20)
/// Determines the data format for voltage-related commands
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VoutMode(pub u8);

impl VoutMode {
    pub fn new(raw: u8) -> Self {
        Self(raw)
    }

    /// Get the mode field (bits 7:5 for standard PMBus, 6:5 for TPS546)
    pub fn mode(&self) -> VoutModeFormat {
        if self.is_tps546_format() {
            // For TPS546, only bits 6:5 matter (always 00 = Linear)
            VoutModeFormat::Linear
        } else {
            // Standard PMBus uses bits 7:5
            let mode = (self.0 >> 5) & 0x07;
            match mode {
                0b000 => VoutModeFormat::Linear,
                0b001 => VoutModeFormat::Vid,
                0b010 => VoutModeFormat::Direct,
                0b011 => VoutModeFormat::Ieee754Half,
                _ => VoutModeFormat::Reserved(mode),
            }
        }
    }

    /// Get the 5-bit exponent (bits 4:0)
    pub fn exponent(&self) -> i8 {
        extract_5bit_exponent(self.0 & 0x1F)
    }

    /// Check if this is TPS546D24A format (REL bit + restricted mode)
    pub fn is_tps546_format(&self) -> bool {
        let mode_field = (self.0 >> 5) & 0x03; // 2-bit mode for TPS546
        let exp = self.exponent();
        mode_field == 0 && (-12..=-4).contains(&exp)
    }

    /// Get the REL field for TPS546 (bit 7)
    pub fn is_relative(&self) -> bool {
        self.0 & 0x80 != 0
    }

    /// Convert Linear16 value using this VOUT_MODE
    pub fn decode_linear16(&self, value: u16) -> f32 {
        value as f32 * 2.0_f32.powi(self.exponent() as i32)
    }

    /// Encode to Linear16 using this VOUT_MODE
    pub fn encode_linear16(&self, volts: f32) -> Result<u16, PMBusError> {
        let exponent = self.exponent() as i32;
        let mantissa = (volts / 2.0_f32.powi(exponent)).round();

        if mantissa < 0.0 || mantissa > u16::MAX as f32 {
            return Err(PMBusError::ValueOutOfRange);
        }

        Ok(mantissa as u16)
    }
}

/// Linear11 format (11-bit mantissa, 5-bit exponent)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Linear11(pub u16);

impl Linear11 {
    pub fn new(raw: u16) -> Self {
        Self(raw)
    }

    pub fn to_f32(self) -> f32 {
        let exp_raw = ((self.0 >> 11) & 0x1F) as u8;
        let exponent = extract_5bit_exponent(exp_raw) as i32;
        let mantissa = (self.0 & 0x07FF) as u32;
        mantissa as f32 * 2.0_f32.powi(exponent)
    }

    pub fn from_f32(value: f32) -> Result<Self, PMBusError> {
        if value == 0.0 {
            return Ok(Self(0));
        }

        // Find best exponent for unsigned mantissa
        let mut best_exp = 0i8;
        let mut best_error = f32::MAX;

        for exp in -16i8..=15 {
            let mantissa_f = value / 2.0_f32.powi(exp as i32);

            if (0.0..2048.0).contains(&mantissa_f) {
                let mantissa = mantissa_f.round() as u32;
                let reconstructed = mantissa as f32 * 2.0_f32.powi(exp as i32);
                let error = (reconstructed - value).abs();

                if error < best_error {
                    best_error = error;
                    best_exp = exp;
                }
            }
        }

        let mantissa = (value / 2.0_f32.powi(best_exp as i32)).round() as u32;
        if mantissa > 0x07FF {
            return Err(PMBusError::ValueOutOfRange);
        }

        let exp_bits = (best_exp as u16) & 0x1F;
        let mant_bits = (mantissa & 0x07FF) as u16;

        Ok(Self((exp_bits << 11) | mant_bits))
    }
}

/// Linear16 format (16-bit mantissa with separate VOUT_MODE)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Linear16 {
    pub value: u16,
    pub mode: VoutMode,
}

impl Linear16 {
    pub fn new(value: u16, mode: VoutMode) -> Self {
        Self { value, mode }
    }

    pub fn to_f32(self) -> f32 {
        self.mode.decode_linear16(self.value)
    }

    pub fn from_f32(volts: f32, mode: VoutMode) -> Result<Self, PMBusError> {
        Ok(Self {
            value: mode.encode_linear16(volts)?,
            mode,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_5bit_exponent_positive() {
        assert_eq!(extract_5bit_exponent(0x00), 0);
        assert_eq!(extract_5bit_exponent(0x0F), 15);
        assert_eq!(extract_5bit_exponent(0x07), 7);
    }

    #[test]
    fn test_extract_5bit_exponent_negative() {
        assert_eq!(extract_5bit_exponent(0x10), -16);
        assert_eq!(extract_5bit_exponent(0x1F), -1);
        assert_eq!(extract_5bit_exponent(0x17), -9); // TPS546 default
    }

    #[test]
    fn test_extract_5bit_exponent_boundary() {
        assert_eq!(extract_5bit_exponent(0x0F), 15); // Max positive
        assert_eq!(extract_5bit_exponent(0x10), -16); // Max negative
    }

    #[test]
    fn test_vout_mode_tps546_default() {
        let mode = VoutMode::new(0x97); // TPS546 default
        assert_eq!(mode.exponent(), -9);
        assert!(mode.is_relative());
        assert!(mode.is_tps546_format());
        assert_eq!(mode.mode(), VoutModeFormat::Linear);
    }

    #[test]
    fn test_vout_mode_decode_encode_roundtrip() {
        let mode = VoutMode::new(0x97);

        // Test 1.2V
        let encoded = mode.encode_linear16(1.2).unwrap();
        let decoded = mode.decode_linear16(encoded);
        assert!((decoded - 1.2).abs() < 0.001);

        // Test 0.75V
        let encoded = mode.encode_linear16(0.75).unwrap();
        let decoded = mode.decode_linear16(encoded);
        assert!((decoded - 0.75).abs() < 0.001);
    }

    #[test]
    fn test_vout_mode_standard_pmbus() {
        let mode = VoutMode::new(0x17); // Standard PMBus, exp=-9
        assert_eq!(mode.exponent(), -9);
        assert!(!mode.is_relative());
        assert_eq!(mode.mode(), VoutModeFormat::Linear);
    }

    #[test]
    fn test_vout_mode_encode_out_of_range() {
        let mode = VoutMode::new(0x97);

        // Too large
        assert!(mode.encode_linear16(40000.0).is_err());

        // Negative
        assert!(mode.encode_linear16(-1.0).is_err());
    }

    #[test]
    fn test_linear11_known_values() {
        // Test some known Linear11 encodings
        let l = Linear11::new(0xD2E6); // exp=-6, mantissa=0x2E6=742
        assert!((l.to_f32() - 11.59375).abs() < 0.001);

        let l = Linear11::new(0x0064); // exp=0, mantissa=0x64=100
        assert_eq!(l.to_f32(), 100.0);
    }

    #[test]
    fn test_linear11_roundtrip() {
        let values = [1.0, 5.5, 12.0, 48.0, 0.75, 3.3];

        for &val in &values {
            let encoded = Linear11::from_f32(val).unwrap();
            let decoded = encoded.to_f32();
            let error = (decoded - val).abs() / val;
            assert!(error < 0.01, "Value {} error too large: {}", val, error);
        }
    }

    #[test]
    fn test_linear11_zero() {
        let encoded = Linear11::from_f32(0.0).unwrap();
        assert_eq!(encoded.0, 0);
        assert_eq!(encoded.to_f32(), 0.0);
    }

    #[test]
    fn test_linear11_max_mantissa() {
        // Max unsigned mantissa is 0x7FF (2047) with exp=0
        let l = Linear11::new(0x07FF); // exp=0, mant=2047
        assert_eq!(l.to_f32(), 2047.0);

        // Test with positive exponent
        let l = Linear11::new(0x7801); // exp=15, mant=1
        assert_eq!(l.to_f32(), 32768.0); // 1 * 2^15
    }

    #[test]
    fn test_linear16_tps546_values() {
        let mode = VoutMode::new(0x97); // TPS546 default

        // Test VOUT_COMMAND=1.199V from capture [66 02]
        let l = Linear16::new(0x0266, mode);
        assert!((l.to_f32() - 1.199).abs() < 0.001);

        // Test VOUT_MAX=2.000V from capture [00 04]
        let l = Linear16::new(0x0400, mode);
        assert!((l.to_f32() - 2.000).abs() < 0.001);
    }

    #[test]
    fn test_linear16_roundtrip() {
        let mode = VoutMode::new(0x97);
        let voltages = [0.5, 0.75, 1.0, 1.2, 1.8, 3.3, 5.0];

        for &v in &voltages {
            let encoded = Linear16::from_f32(v, mode).unwrap();
            let decoded = encoded.to_f32();
            let error = (decoded - v).abs();
            assert!(error < 0.001, "Voltage {} error: {}", v, error);
        }
    }

    #[test]
    fn test_linear16_different_exponents() {
        // Test with different VOUT_MODE exponents
        let test_cases = [
            (0x14, 1.0, 4096), // exp=-12, 1V needs 4096 counts
            (0x17, 1.0, 512),  // exp=-9, 1V needs 512 counts
            (0x1C, 1.0, 16),   // exp=-4, 1V needs 16 counts
        ];

        for (mode_byte, voltage, expected_raw) in test_cases {
            let mode = VoutMode::new(mode_byte);
            let encoded = Linear16::from_f32(voltage, mode).unwrap();
            assert_eq!(encoded.value, expected_raw);
        }
    }

    #[test]
    fn test_sign_extension_consistency() {
        // Ensure all functions use same sign extension
        let test_exp = 0x17u8; // -9 in 5-bit two's complement

        // Via extract_5bit_exponent
        let exp1 = extract_5bit_exponent(test_exp);

        // Via VoutMode
        let mode = VoutMode::new(test_exp);
        let exp2 = mode.exponent();

        assert_eq!(exp1, -9);
        assert_eq!(exp2, -9);
    }
}
