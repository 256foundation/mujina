//! Ratio type for dimensionless factors of a reference value.
//!
//! Stores the ratio internally as parts per million (i64),
//! following the pattern of `std::time::Duration`. Exact for
//! percent-step values; not a general rational number. Convert on
//! access via the unit-specific methods.

/// A dimensionless ratio in parts per million.
///
/// A ratio type with named scales. Stores parts per million
/// internally; convert on access. Exact for percent-step values,
/// not a general rational number. Signed: negative ratios are
/// valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Ratio {
    ppm: i64,
}

impl Ratio {
    /// Creates a ratio from parts per million.
    pub const fn from_ppm(ppm: i64) -> Self {
        Self { ppm }
    }

    /// Creates a ratio from a percentage (100.0 is the whole).
    pub const fn from_percent(percent: f32) -> Self {
        Self {
            ppm: round_to_i64(percent * 10_000.0),
        }
    }

    /// Creates a ratio from a plain factor (1.0 is the whole).
    pub const fn from_factor(factor: f32) -> Self {
        Self {
            ppm: round_to_i64(factor * 1_000_000.0),
        }
    }

    /// Returns the ratio in parts per million.
    pub const fn ppm(&self) -> i64 {
        self.ppm
    }

    /// Returns the ratio as a percentage.
    pub const fn percent(&self) -> f32 {
        self.ppm as f32 / 10_000.0
    }

    /// Returns the ratio as a plain factor.
    pub const fn factor(&self) -> f32 {
        self.ppm as f32 / 1_000_000.0
    }
}

/// Rounds to the nearest integer, so a factor that f32 stores
/// slightly low, such as 1.16, still maps to its exact ppm value.
const fn round_to_i64(x: f32) -> i64 {
    if x >= 0.0 {
        (x + 0.5) as i64
    } else {
        (x - 0.5) as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_factor() {
        let r = Ratio::from_factor(1.25);
        assert_eq!(r.ppm(), 1_250_000);
        assert_eq!(r.percent(), 125.0);
        assert_eq!(r.factor(), 1.25);
    }

    #[test]
    fn from_percent() {
        let r = Ratio::from_percent(75.0);
        assert_eq!(r.ppm(), 750_000);
        assert_eq!(r.factor(), 0.75);
    }

    #[test]
    fn from_ppm() {
        let r = Ratio::from_ppm(10_000);
        assert_eq!(r.percent(), 1.0);
    }

    #[test]
    fn percent_steps_are_exact() {
        // 1.16 has no exact f32 representation; the rounding
        // constructor must still hit the intended step.
        assert_eq!(Ratio::from_factor(1.16).ppm(), 1_160_000);
        assert_eq!(Ratio::from_percent(116.0).ppm(), 1_160_000);
    }

    #[test]
    fn ordering() {
        assert!(Ratio::from_factor(0.75) < Ratio::from_factor(1.25));
        assert!(Ratio::from_percent(-5.0) < Ratio::default());
    }

    #[test]
    fn default_is_zero() {
        assert_eq!(Ratio::default().ppm(), 0);
    }
}
