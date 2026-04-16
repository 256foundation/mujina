//! Hashrate measurement type.
//!
//! Represents a per-miner hashrate in hashes per second, stored as
//! u64. This covers up to ~18.4 EH/s, which is well beyond any
//! single miner's capability. Not intended for representing
//! aggregate network hashrate.
//!
//! Arithmetic follows Rust's default overflow behavior: panics in
//! debug builds, wraps in release builds.

use std::iter::Sum;
use std::ops::Add;
use std::time::Duration;

/// Per-miner hashrate in hashes per second.
///
/// Stored as u64, covering up to ~18.4 EH/s. Constructors from
/// f64 (e.g., `from_terahashes`) saturate to `u64::MAX` for
/// out-of-range values.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct HashRate(pub u64);

impl HashRate {
    /// Create from megahashes per second
    pub fn from_megahashes(mh: f64) -> Self {
        Self((mh * 1_000_000.0) as u64)
    }

    /// Create from gigahashes per second
    pub fn from_gigahashes(gh: f64) -> Self {
        Self((gh * 1_000_000_000.0) as u64)
    }

    /// Create from terahashes per second
    pub fn from_terahashes(th: f64) -> Self {
        Self((th * 1_000_000_000_000.0) as u64)
    }

    /// Get value as megahashes per second
    pub fn as_megahashes(&self) -> f64 {
        self.0 as f64 / 1_000_000.0
    }

    /// Get value as gigahashes per second
    pub fn as_gigahashes(&self) -> f64 {
        self.0 as f64 / 1_000_000_000.0
    }

    /// Get value as terahashes per second
    pub fn as_terahashes(&self) -> f64 {
        self.0 as f64 / 1_000_000_000_000.0
    }

    /// Returns true if the hashrate is zero.
    pub fn is_zero(&self) -> bool {
        self.0 == 0
    }

    /// Expected number of hashes in the given duration.
    ///
    /// Returns a fractional count because this is a statistical
    /// expectation (e.g., 0.5 hashes in 100 ms at 5 H/s). Exact for
    /// hashrates up to ~9 PH/s (2^53 H/s); above that, rounding
    /// errors are bounded by ~10^-16 relative.
    pub fn hashes_in(&self, duration: Duration) -> f64 {
        self.0 as f64 * duration.as_secs_f64()
    }

    /// Format as human-readable string with appropriate units
    pub fn to_human_readable(&self) -> String {
        if self.0 >= 1_000_000_000_000 {
            format!("{:.2} TH/s", self.as_terahashes())
        } else if self.0 >= 1_000_000_000 {
            format!("{:.2} GH/s", self.as_gigahashes())
        } else if self.0 >= 1_000_000 {
            format!("{:.2} MH/s", self.as_megahashes())
        } else {
            format!("{} H/s", self.0)
        }
    }
}

impl From<u64> for HashRate {
    fn from(hashes_per_second: u64) -> Self {
        Self(hashes_per_second)
    }
}

impl From<HashRate> for u64 {
    fn from(rate: HashRate) -> Self {
        rate.0
    }
}

impl From<HashRate> for f64 {
    fn from(rate: HashRate) -> Self {
        rate.0 as f64
    }
}

impl Add for HashRate {
    type Output = Self;

    fn add(self, rhs: Self) -> Self {
        Self(self.0 + rhs.0)
    }
}

impl Sum for HashRate {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(HashRate(0), |acc, x| acc + x)
    }
}

impl std::fmt::Display for HashRate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_human_readable())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hashrate_conversions() {
        let rate = HashRate::from_terahashes(100.0);
        assert_eq!(rate.as_terahashes(), 100.0);
        assert_eq!(rate.as_gigahashes(), 100_000.0);
        assert_eq!(rate.to_human_readable(), "100.00 TH/s");

        let rate = HashRate::from_gigahashes(500.0);
        assert_eq!(rate.as_gigahashes(), 500.0);
        assert_eq!(rate.to_human_readable(), "500.00 GH/s");
    }

    #[test]
    fn test_hashrate_to_f64() {
        let rate = HashRate::from_gigahashes(1.5);
        let expected = 1_500_000_000.0;
        assert_eq!(f64::from(rate), expected);
    }

    #[test]
    fn add() {
        let a = HashRate::from_gigahashes(1.0);
        let b = HashRate::from_gigahashes(2.0);
        assert_eq!(a + b, HashRate::from_gigahashes(3.0));
    }

    #[test]
    fn sum_over_iterator() {
        let rates = vec![
            HashRate::from_megahashes(100.0),
            HashRate::from_megahashes(200.0),
            HashRate::from_megahashes(300.0),
        ];
        let total: HashRate = rates.into_iter().sum();
        assert_eq!(total, HashRate::from_megahashes(600.0));
    }

    #[test]
    fn sum_empty_iterator() {
        let total: HashRate = std::iter::empty().sum();
        assert_eq!(total, HashRate::from(0));
    }
}
