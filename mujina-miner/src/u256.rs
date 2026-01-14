//! 256-bit unsigned integer arithmetic.
//!
//! Wraps `ruint::aliases::U256` to provide a stable interface. This adapter
//! module exists so we can swap the underlying library or implement our own
//! arithmetic without changing callers.

use ruint::aliases::U256 as Ruint256;
use std::ops::{AddAssign, Div, Mul};

/// A 256-bit unsigned integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct U256(Ruint256);

impl U256 {
    /// Zero constant.
    pub const ZERO: Self = Self(Ruint256::ZERO);

    /// Maximum value (2^256 - 1).
    pub const MAX: Self = Self(Ruint256::MAX);

    /// Create from little-endian bytes.
    pub fn from_le_bytes(bytes: [u8; 32]) -> Self {
        Self(Ruint256::from_le_bytes(bytes))
    }

    /// Convert to little-endian bytes.
    pub fn to_le_bytes(self) -> [u8; 32] {
        self.0.to_le_bytes()
    }

    /// Convert to u64, saturating at u64::MAX.
    pub fn saturating_to_u64(self) -> u64 {
        self.0.saturating_to()
    }

    /// Convert to f64, losing precision for large values.
    ///
    /// For values larger than f64 can precisely represent (~2^53), this
    /// returns an approximation by extracting the high bits and scaling.
    pub fn to_f64_approx(self) -> f64 {
        let bytes = self.to_le_bytes();

        // Find highest non-zero byte to determine magnitude
        let mut highest_byte = 0;
        for (i, &b) in bytes.iter().enumerate().rev() {
            if b != 0 {
                highest_byte = i;
                break;
            }
        }

        // If zero or fits in u64, use direct conversion
        if highest_byte < 8 {
            return self.saturating_to_u64() as f64;
        }

        // Extract 8 bytes starting from highest_byte-7 (or 0 if less)
        let start = highest_byte.saturating_sub(7);
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&bytes[start..start + 8]);
        let mantissa = u64::from_le_bytes(buf) as f64;

        // Scale by 2^(start*8) to account for position
        mantissa * (2.0_f64).powi((start * 8) as i32)
    }
}

impl Div for U256 {
    type Output = Self;

    fn div(self, rhs: Self) -> Self::Output {
        Self(self.0 / rhs.0)
    }
}

impl Div<u64> for U256 {
    type Output = Self;

    fn div(self, rhs: u64) -> Self::Output {
        Self(self.0 / Ruint256::from(rhs))
    }
}

impl Div<u128> for U256 {
    type Output = Self;

    fn div(self, rhs: u128) -> Self::Output {
        Self(self.0 / Ruint256::from(rhs))
    }
}

impl Mul<u64> for U256 {
    type Output = Self;

    fn mul(self, rhs: u64) -> Self::Output {
        Self(self.0 * Ruint256::from(rhs))
    }
}

impl AddAssign for U256 {
    fn add_assign(&mut self, rhs: Self) {
        self.0 += rhs.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_division_u256() {
        let a = U256::from_le_bytes({
            let mut bytes = [0u8; 32];
            bytes[0] = 100;
            bytes
        });
        let b = U256::from_le_bytes({
            let mut bytes = [0u8; 32];
            bytes[0] = 10;
            bytes
        });
        let expected = U256::from_le_bytes({
            let mut bytes = [0u8; 32];
            bytes[0] = 10;
            bytes
        });
        assert_eq!(a / b, expected);
    }

    #[test]
    fn test_division_u64() {
        let a = U256::from_le_bytes({
            let mut bytes = [0u8; 32];
            bytes[0] = 100;
            bytes
        });
        let expected = U256::from_le_bytes({
            let mut bytes = [0u8; 32];
            bytes[0] = 10;
            bytes
        });
        assert_eq!(a / 10u64, expected);
    }

    #[test]
    fn test_large_division() {
        // Large value / 1 = same value
        let large = U256::from_le_bytes([0xff; 32]);
        let one = U256::from_le_bytes({
            let mut bytes = [0u8; 32];
            bytes[0] = 1;
            bytes
        });
        assert_eq!(large / one, large);
    }
}
