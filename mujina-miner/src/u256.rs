//! 256-bit unsigned integer type for Bitcoin mining difficulty calculations.
//!
//! This provides the minimal functionality needed for difficulty-to-target
//! conversions without pulling in a full bignum library.

use std::ops::{Div, Mul};

/// A 256-bit unsigned integer stored as two 128-bit limbs (little-endian).
///
/// Layout: [low 128 bits, high 128 bits]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct U256 {
    /// Low 128 bits
    pub low: u128,
    /// High 128 bits
    pub high: u128,
}

impl U256 {
    /// Create from little-endian bytes
    pub fn from_le_bytes(bytes: [u8; 32]) -> Self {
        let low = u128::from_le_bytes(bytes[0..16].try_into().unwrap());
        let high = u128::from_le_bytes(bytes[16..32].try_into().unwrap());
        Self { low, high }
    }

    /// Convert to little-endian bytes
    pub fn to_le_bytes(self) -> [u8; 32] {
        let mut bytes = [0u8; 32];
        bytes[0..16].copy_from_slice(&self.low.to_le_bytes());
        bytes[16..32].copy_from_slice(&self.high.to_le_bytes());
        bytes
    }

    /// Divide by a u64 value
    ///
    /// This implements long division for a 256-bit dividend and 64-bit divisor.
    pub fn div_u64(self, divisor: u64) -> Self {
        if divisor == 0 {
            panic!("Division by zero");
        }

        if divisor == 1 {
            return self;
        }

        // If the dividend fits in 128 bits, use simple division
        if self.high == 0 {
            return Self {
                low: self.low / divisor as u128,
                high: 0,
            };
        }

        // Full 256-bit division by 64-bit divisor
        // We process the number in 64-bit chunks from high to low
        let divisor_u128 = divisor as u128;

        // Split into four 64-bit limbs
        let limb3 = (self.high >> 64) as u64;
        let limb2 = self.high as u64;
        let limb1 = (self.low >> 64) as u64;
        let limb0 = self.low as u64;

        let mut remainder: u128 = 0;
        let mut result = [0u64; 4];

        // Process from most significant to least significant
        for (i, &limb) in [limb3, limb2, limb1, limb0].iter().enumerate() {
            remainder = (remainder << 64) | (limb as u128);
            result[i] = (remainder / divisor_u128) as u64;
            remainder %= divisor_u128;
        }

        // Reconstruct the result
        let result_high = ((result[0] as u128) << 64) | (result[1] as u128);
        let result_low = ((result[2] as u128) << 64) | (result[3] as u128);

        Self {
            low: result_low,
            high: result_high,
        }
    }
}

impl Div<u64> for U256 {
    type Output = Self;

    fn div(self, rhs: u64) -> Self::Output {
        self.div_u64(rhs)
    }
}

impl Mul<u64> for U256 {
    type Output = Self;

    fn mul(self, rhs: u64) -> Self::Output {
        // Multiply by splitting into 64-bit chunks to avoid overflow
        // self = (high_128 << 128) + low_128
        // result = self * rhs = (high_128 * rhs << 128) + (low_128 * rhs)

        let rhs_u128 = rhs as u128;

        // Multiply low 128 bits
        // Split into high and low 64-bit parts to handle carry
        let low_low = (self.low as u64) as u128 * rhs_u128;
        let low_high = (self.low >> 64) * rhs_u128;

        // Add them together with proper shifting
        let low_result = low_low + (low_high << 64);
        let low_carry = (low_low >> 64) + low_high;

        // Multiply high 128 bits and add carry from low
        let high_low = (self.high as u64) as u128 * rhs_u128;
        let high_high = (self.high >> 64) * rhs_u128;

        let high_result = high_low + (high_high << 64) + (low_carry >> 64);

        // Note: We ignore overflow beyond 256 bits
        debug_assert!((high_high >> 64) == 0, "U256 multiplication overflow");

        Self {
            low: low_result,
            high: high_result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_to_bytes() {
        let bytes = [0x12u8; 32];
        let u = U256::from_le_bytes(bytes);
        assert_eq!(u.to_le_bytes(), bytes);
    }

    #[test]
    fn test_div_simple() {
        let u = U256 { low: 100, high: 0 };
        let result = u / 10;
        assert_eq!(result, U256 { low: 10, high: 0 });
    }

    #[test]
    fn test_div_with_high_bits() {
        // Create a number with high bits set
        let u = U256 { low: 0, high: 100 };
        let result = u / 10;
        assert_eq!(result, U256 { low: 0, high: 10 });
    }

    #[test]
    fn test_mul_simple() {
        let u = U256 { low: 10, high: 0 };
        let result = u * 10;
        assert_eq!(result, U256 { low: 100, high: 0 });
    }

    #[test]
    fn test_div_by_one() {
        let u = U256 {
            low: 12345,
            high: 67890,
        };
        let result = u / 1;
        assert_eq!(result, u);
    }
}
