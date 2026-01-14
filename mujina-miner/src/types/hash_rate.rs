//! Hashrate measurement type.

use std::time::Duration;

/// Hashrate measurement.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct HashRate(pub u64); // hashes per second

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
    pub fn hashes_in(&self, duration: Duration) -> u128 {
        self.0 as u128 * duration.as_nanos() / 1_000_000_000
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

impl From<HashRate> for f64 {
    fn from(rate: HashRate) -> Self {
        rate.0 as f64
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
}
