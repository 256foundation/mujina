//! Difficulty type with lossless 256-bit representation.

use crate::u256::U256;
use bitcoin::hash_types::BlockHash;
use bitcoin::hashes::Hash;
use bitcoin::pow::Target;
use std::cmp::Ordering;
use std::fmt;

/// Mining difficulty.
///
/// Internally stores the corresponding target value for lossless 256-bit
/// precision. Difficulty and target have an inverse relationship:
/// ```text
/// target = MAX_TARGET / difficulty
/// difficulty = MAX_TARGET / target
/// ```
///
/// Used for:
/// - Stratum protocol (pools communicate difficulty as integers)
/// - Logging and display (human-readable values)
/// - Share validation (via `to_target()`)
/// - Forced low-difficulty testing (sub-1.0 values)
///
/// In Bitcoin's proof-of-work, a hash is valid if it's numerically less than
/// or equal to a target value:
/// - Difficulty 1: target = MAX_TARGET (largest valid target, easiest)
/// - Difficulty 1000: target = MAX_TARGET / 1000 (smaller target, harder)
/// - Difficulty 0.001: target = MAX_TARGET * 1000 (larger than MAX, very easy)
///
/// Higher difficulty produces a smaller target, meaning fewer hash values
/// qualify as valid, requiring more hashing attempts on average.
#[derive(Debug, Clone, Copy)]
pub struct Difficulty(Target);

impl Difficulty {
    /// Maximum difficulty (target of zero---no hash can satisfy it).
    pub const MAX: Self = Self(Target::ZERO);

    /// Create from f64 for sub-1.0 difficulties (testing only).
    ///
    /// Most code should use `Difficulty::from(u64)` instead. This exists
    /// for forced-rate testing where fractional difficulties are needed.
    /// The conversion is necessarily lossy.
    pub fn from_f64(value: f64) -> Self {
        if value <= 0.0 || !value.is_finite() {
            return Self(Target::MAX);
        }

        let max_target = U256::from(Target::MAX);

        if value >= 1.0 {
            // target = MAX_TARGET / difficulty
            // Use integer division (lossy for non-integer difficulties)
            let target = max_target / (value as u64).max(1);
            Self(Target::from(target))
        } else {
            // Sub-1.0 difficulty: target > MAX_TARGET
            // target = MAX_TARGET / difficulty = MAX_TARGET * (1/difficulty)
            let multiplier = (1.0 / value) as u64;
            let target = max_target * multiplier;
            Self(Target::from(target))
        }
    }

    /// Get difficulty as f64 (lossy for very large values).
    ///
    /// Uses rust-bitcoin's `difficulty_float()` for the conversion.
    pub fn as_f64(self) -> f64 {
        self.0.difficulty_float()
    }

    /// Convert to u64, saturating at u64::MAX.
    ///
    /// Useful for Stratum protocol which uses integer difficulties.
    pub fn as_u64(self) -> u64 {
        let f = self.as_f64();
        if f >= u64::MAX as f64 {
            u64::MAX
        } else if f <= 0.0 {
            0
        } else {
            f as u64
        }
    }

    /// Create difficulty from a target (lossless).
    pub fn from_target(target: Target) -> Self {
        Self(target)
    }

    /// Get the underlying target (lossless).
    ///
    /// Use this for actual share validation (comparing against block hashes).
    pub fn to_target(self) -> Target {
        self.0
    }

    /// Calculate difficulty from a block hash.
    ///
    /// The hash value directly represents the target that was met, so this
    /// conversion is lossless. Useful for determining what difficulty a
    /// found share represents.
    pub fn from_hash(hash: &BlockHash) -> Self {
        let hash_u256 = U256::from_le_bytes(*hash.as_byte_array());
        if hash_u256 == U256::ZERO {
            return Self::MAX;
        }
        // The hash IS the target that was met
        Self(Target::from(hash_u256))
    }
}

impl From<u64> for Difficulty {
    fn from(diff: u64) -> Self {
        if diff == 0 {
            return Self(Target::MAX);
        }
        // target = MAX_TARGET / difficulty
        let max_target = U256::from(Target::MAX);
        let target = max_target / diff;
        Self(Target::from(target))
    }
}

impl PartialEq for Difficulty {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for Difficulty {}

impl PartialOrd for Difficulty {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Difficulty {
    fn cmp(&self, other: &Self) -> Ordering {
        // Invert comparison: smaller target = higher difficulty
        // So if self.target < other.target, self is GREATER difficulty
        other.0.cmp(&self.0)
    }
}

impl fmt::Display for Difficulty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = self.as_f64();

        // Handle sub-1.0 difficulties with adaptive precision
        if value < 1.0 {
            let s = format!("{:.6}", value);
            let trimmed = s.trim_end_matches('0').trim_end_matches('.');
            return write!(f, "{}", trimmed);
        }

        // Format with SI suffixes (K, M, G, T, P)
        let (scaled, suffix) = if value >= 1e15 {
            (value / 1e15, "P")
        } else if value >= 1e12 {
            (value / 1e12, "T")
        } else if value >= 1e9 {
            (value / 1e9, "G")
        } else if value >= 1e6 {
            (value / 1e6, "M")
        } else if value >= 1e3 {
            (value / 1e3, "K")
        } else {
            (value, "")
        };

        // Round to appropriate precision; omit decimals for whole numbers
        if scaled >= 100.0 || scaled.fract() == 0.0 {
            write!(f, "{:.0}{}", scaled, suffix) // "112T" or "1"
        } else if scaled >= 10.0 {
            write!(f, "{:.1}{}", scaled, suffix) // "11.2T"
        } else {
            write!(f, "{:.2}{}", scaled, suffix) // "1.12T"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::hashes::Hash;

    #[test]
    fn test_difficulty_as_u64() {
        let diff = Difficulty::from(1024_u64);
        assert_eq!(diff.as_u64(), 1024);

        // Sub-1.0 truncates to 0
        let diff = Difficulty::from_f64(0.5);
        assert_eq!(diff.as_u64(), 0);
    }

    #[test]
    fn test_difficulty_to_target() {
        // Difficulty 1 should equal MAX target
        let diff = Difficulty::from(1_u64);
        assert_eq!(diff.to_target(), Target::MAX);

        // Difficulty 0 treated as 1 (edge case)
        let diff = Difficulty::from(0_u64);
        assert_eq!(diff.to_target(), Target::MAX);

        // Higher difficulty should produce smaller target
        let diff_low = Difficulty::from(100_u64);
        let diff_high = Difficulty::from(1000_u64);
        assert!(diff_high.to_target() < diff_low.to_target());
    }

    #[test]
    fn test_difficulty_from_target() {
        // Target::MAX gives difficulty 1
        let diff = Difficulty::from_target(Target::MAX);
        assert!((diff.as_f64() - 1.0).abs() < 0.001);

        // Round-trip: difficulty -> target -> difficulty is exact
        let original = Difficulty::from(1024_u64);
        let recovered = Difficulty::from_target(original.to_target());
        assert_eq!(original, recovered);

        // Larger difficulty round-trip
        let original = Difficulty::from(1_000_000_u64);
        let recovered = Difficulty::from_target(original.to_target());
        assert_eq!(original, recovered);
    }

    #[test]
    fn test_difficulty_ordering() {
        let diff_low = Difficulty::from(100_u64);
        let diff_high = Difficulty::from(1000_u64);

        // Higher difficulty value should compare greater
        assert!(diff_high > diff_low);
        assert!(diff_low < diff_high);

        // Equal difficulties
        let diff_a = Difficulty::from(500_u64);
        let diff_b = Difficulty::from(500_u64);
        assert_eq!(diff_a, diff_b);
        assert!(!(diff_a > diff_b));
        assert!(!(diff_a < diff_b));
    }

    #[test]
    fn test_difficulty_display() {
        // High difficulty (petahash range)
        let diff = Difficulty::from(1_500_000_000_000_000_u64);
        assert_eq!(diff.to_string(), "1.50P");

        // Terahash range
        let diff = Difficulty::from(112_700_000_000_000_u64);
        assert_eq!(diff.to_string(), "113T");

        let diff = Difficulty::from(11_200_000_000_000_u64);
        assert_eq!(diff.to_string(), "11.2T");

        let diff = Difficulty::from(1_120_000_000_000_u64);
        assert_eq!(diff.to_string(), "1.12T");

        // Gigahash range
        let diff = Difficulty::from(500_000_000_000_u64);
        assert_eq!(diff.to_string(), "500G");

        // Megahash range
        let diff = Difficulty::from(1_500_000_u64);
        assert_eq!(diff.to_string(), "1.50M");

        // Small values
        let diff = Difficulty::from(500_u64);
        assert_eq!(diff.to_string(), "500");

        // Difficulty 1 displays without decimals
        let diff = Difficulty::from(1_u64);
        assert_eq!(diff.to_string(), "1");

        // Sub-1.0 values display with adaptive precision (no trailing zeros)
        let diff = Difficulty::from_f64(0.5);
        assert_eq!(diff.to_string(), "0.5");

        let diff = Difficulty::from_f64(0.000048);
        assert_eq!(diff.to_string(), "0.000048");
    }

    #[test]
    fn test_difficulty_from_hash() {
        // Target::MAX gives difficulty 1
        let hash = BlockHash::from_byte_array(Target::MAX.to_le_bytes());
        let diff = Difficulty::from_hash(&hash);
        assert!((diff.as_f64() - 1.0).abs() < 0.001);

        // Half of Target::MAX gives difficulty 2
        let mut bytes = Target::MAX.to_le_bytes();
        // Shift right by 1 bit (divide by 2)
        let mut carry = 0u8;
        for byte in bytes.iter_mut().rev() {
            let new_carry = *byte & 1;
            *byte = (*byte >> 1) | (carry << 7);
            carry = new_carry;
        }
        let hash = BlockHash::from_byte_array(bytes);
        let diff = Difficulty::from_hash(&hash);
        assert!((diff.as_f64() - 2.0).abs() < 0.01);

        // Very small hash gives high difficulty
        let mut bytes = [0u8; 32];
        bytes[0] = 1; // Smallest non-zero LE value
        let hash = BlockHash::from_byte_array(bytes);
        assert!(Difficulty::from_hash(&hash).as_f64() > 1_000_000.0);

        // Zero hash saturates to MAX
        let hash = BlockHash::from_byte_array([0u8; 32]);
        assert_eq!(Difficulty::from_hash(&hash), Difficulty::MAX);
    }

    #[test]
    fn test_sub_1_difficulty_target() {
        // Sub-1.0 difficulty should produce target > MAX_TARGET
        let diff = Difficulty::from_f64(0.5);
        let target = diff.to_target();

        // Target should be larger than MAX (easier)
        assert!(target > Target::MAX);

        // Difficulty 0.5 means target = MAX_TARGET * 2
        let max_u256 = U256::from(Target::MAX);
        let expected_target = max_u256 * 2;
        assert_eq!(U256::from(target), expected_target);
    }

    #[test]
    fn test_lossless_roundtrip() {
        // Any u64 difficulty should round-trip exactly
        for &diff_val in &[1_u64, 2, 100, 1000, 1_000_000, u64::MAX / 2] {
            let diff = Difficulty::from(diff_val);
            let target = diff.to_target();
            let recovered = Difficulty::from_target(target);
            assert_eq!(diff, recovered, "Round-trip failed for {}", diff_val);
        }
    }
}
