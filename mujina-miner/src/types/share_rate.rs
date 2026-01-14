//! Share rate type for expressing share submission limits.

use std::time::Duration;

/// Share submission rate (shares per unit time).
///
/// Used to express rate limits for share submission to pools. The scheduler
/// uses this to compute appropriate share target values that limit average
/// share generation rate while allowing natural bursts from luck variance.
///
/// Internally stores the interval between shares as a Duration, which
/// guarantees the rate is always positive.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShareRate(Duration);

impl ShareRate {
    /// Create a rate of N shares per second.
    ///
    /// # Panics
    /// Panics if `shares` is not positive.
    pub fn per_second(shares: f64) -> Self {
        assert!(shares > 0.0, "share rate must be positive");
        Self(Duration::from_secs_f64(1.0 / shares))
    }

    /// Create a rate of N shares per minute.
    ///
    /// # Panics
    /// Panics if `shares` is not positive.
    pub fn per_minute(shares: f64) -> Self {
        assert!(shares > 0.0, "share rate must be positive");
        Self(Duration::from_secs_f64(60.0 / shares))
    }

    /// Create a rate from target average interval between shares.
    ///
    /// For example, `ShareRate::from_interval(Duration::from_secs(10))` creates
    /// a rate targeting one share per 10 seconds on average.
    ///
    /// # Panics
    /// Panics if `interval` is zero.
    pub const fn from_interval(interval: Duration) -> Self {
        assert!(!interval.is_zero(), "interval must be non-zero");
        Self(interval)
    }

    /// Get the rate as shares per second.
    pub fn as_per_second(&self) -> f64 {
        1.0 / self.0.as_secs_f64()
    }

    /// Get the rate as shares per minute.
    pub fn as_per_minute(&self) -> f64 {
        60.0 / self.0.as_secs_f64()
    }

    /// Get the average interval between shares.
    pub fn as_interval(&self) -> Duration {
        self.0
    }
}

impl std::fmt::Display for ShareRate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let per_min = self.as_per_minute();
        if per_min >= 1.0 {
            write!(f, "{:.1} shares/min", per_min)
        } else {
            write!(f, "{:.3} shares/sec", self.as_per_second())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_per_minute_conversion() {
        let rate = ShareRate::per_minute(6.0);
        assert!((rate.as_per_second() - 0.1).abs() < 1e-9);
        assert!((rate.as_per_minute() - 6.0).abs() < 1e-9);
    }

    #[test]
    fn test_per_second_conversion() {
        let rate = ShareRate::per_second(0.5);
        assert!((rate.as_per_minute() - 30.0).abs() < 1e-9);
    }

    #[test]
    fn test_from_interval() {
        let rate = ShareRate::from_interval(Duration::from_secs(10));
        assert!((rate.as_per_second() - 0.1).abs() < 1e-9);
        assert!((rate.as_per_minute() - 6.0).abs() < 1e-9);
    }

    #[test]
    fn test_as_interval() {
        let rate = ShareRate::per_minute(6.0); // 1 share per 10 seconds
        let interval = rate.as_interval();
        assert!((interval.as_secs_f64() - 10.0).abs() < 1e-9);
    }

    #[test]
    fn test_from_interval_roundtrip() {
        let original = Duration::from_secs(15);
        let rate = ShareRate::from_interval(original);
        let recovered = rate.as_interval();
        assert!((recovered.as_secs_f64() - 15.0).abs() < 1e-9);
    }

    #[test]
    fn test_display() {
        // >= 1 share/min: display as shares/min
        assert_eq!(ShareRate::per_minute(6.0).to_string(), "6.0 shares/min");
        assert_eq!(ShareRate::per_minute(1.0).to_string(), "1.0 shares/min");
        // < 1 share/min: display as shares/sec
        assert_eq!(ShareRate::per_second(0.01).to_string(), "0.010 shares/sec");
        assert_eq!(ShareRate::per_second(0.001).to_string(), "0.001 shares/sec");
    }
}
