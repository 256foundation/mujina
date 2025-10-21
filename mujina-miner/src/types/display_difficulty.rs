//! Display-only difficulty formatting with SI prefixes.

use bitcoin::{BlockHash, Target};
use std::fmt;

/// Difficulty value for display purposes only.
///
/// Never use this for validation or comparison - use Target types instead.
/// This type exists solely for human-readable output with SI prefixes.
///
/// # Examples
///
/// ```
/// use mujina_miner::types::DisplayDifficulty;
/// use bitcoin::Target;
///
/// let diff = DisplayDifficulty::from_target(&Target::MAX);
/// println!("Difficulty: {}", diff);  // "1.00" (difficulty 1)
/// ```
#[derive(Debug, Clone, Copy)]
pub struct DisplayDifficulty(f64);

impl DisplayDifficulty {
    /// Calculate from block hash
    pub fn from_hash(hash: &BlockHash) -> Self {
        // Use rust-bitcoin's Target::MAX as difficulty-1 target
        // difficulty = max_target / hash
        // We need to convert hash to target first, then use difficulty_float()

        // For now, use the established constant from cgminer/esp-miner
        // TODO: Switch to rust-bitcoin's approach when we figure out the conversion
        let hash_f64 = Self::hash_to_f64(hash);
        if hash_f64 == 0.0 {
            return Self(f64::MAX);
        }

        const DIFFICULTY_1_TARGET_F64: f64 =
            26959535291011309493156476344723991336010898738574164086137773096960.0;
        let difficulty = DIFFICULTY_1_TARGET_F64 / hash_f64;
        Self(difficulty)
    }

    /// Calculate from target using rust-bitcoin's built-in method
    pub fn from_target(target: &Target) -> Self {
        Self(target.difficulty_float())
    }

    /// Get raw f64 value (use sparingly)
    pub fn as_f64(&self) -> f64 {
        self.0
    }

    /// Convert 256-bit hash to f64 for difficulty calculation
    /// Follows cgminer's le256todouble implementation
    fn hash_to_f64(hash: &BlockHash) -> f64 {
        use bitcoin::hashes::Hash;

        const BITS_64: f64 = 18446744073709551616.0;
        const BITS_128: f64 = 340282366920938463463374607431768211456.0;
        const BITS_192: f64 = 6277101735386680763835789423207666416102355444464034512896.0;

        let bytes = hash.as_byte_array();

        // Process in 64-bit chunks (little-endian)
        let mut result = 0.0;
        result += u64::from_le_bytes(bytes[0..8].try_into().unwrap()) as f64;
        result += u64::from_le_bytes(bytes[8..16].try_into().unwrap()) as f64 * BITS_64;
        result += u64::from_le_bytes(bytes[16..24].try_into().unwrap()) as f64 * BITS_128;
        result += u64::from_le_bytes(bytes[24..32].try_into().unwrap()) as f64 * BITS_192;

        result
    }
}

impl fmt::Display for DisplayDifficulty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = self.0;

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

        // Round to appropriate precision
        if scaled >= 100.0 {
            write!(f, "{:.0}{}", scaled, suffix) // "112T"
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

    #[test]
    fn test_display_formatting() {
        // High difficulty (petahash range)
        let diff = DisplayDifficulty(1.5e15);
        assert_eq!(diff.to_string(), "1.50P");

        // Terahash range
        let diff = DisplayDifficulty(112.7e12);
        assert_eq!(diff.to_string(), "113T"); // Rounded

        let diff = DisplayDifficulty(11.2e12);
        assert_eq!(diff.to_string(), "11.2T");

        let diff = DisplayDifficulty(1.12e12);
        assert_eq!(diff.to_string(), "1.12T");

        // Gigahash range
        let diff = DisplayDifficulty(500.0e9);
        assert_eq!(diff.to_string(), "500G");

        // Megahash range
        let diff = DisplayDifficulty(1.5e6);
        assert_eq!(diff.to_string(), "1.50M");

        // Small values
        let diff = DisplayDifficulty(500.0);
        assert_eq!(diff.to_string(), "500");
    }

    #[test]
    fn test_from_target_uses_rust_bitcoin() {
        // Target::MAX is difficulty 1
        let diff = DisplayDifficulty::from_target(&Target::MAX);
        assert!((diff.as_f64() - 1.0).abs() < 0.01);
    }
}
