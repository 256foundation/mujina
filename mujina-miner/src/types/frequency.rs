//! Frequency type for representing clock rates and oscillation frequencies.
//!
//! Stores frequency internally as Hz (u64) for precision, following the
//! pattern of `std::time::Duration`. Convert on access via the unit-
//! specific methods.

/// Frequency in Hz.
///
/// A unit-aware frequency type. Stores Hz internally for precision;
/// convert on access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Frequency {
    hz: u64,
}

impl Frequency {
    /// Creates a frequency from Hz.
    pub fn from_hz(hz: u64) -> Self {
        Self { hz }
    }

    /// Creates a frequency from kHz.
    pub fn from_khz(khz: u32) -> Self {
        Self {
            hz: khz as u64 * 1_000,
        }
    }

    /// Creates a frequency from MHz.
    pub fn from_mhz(mhz: f32) -> Self {
        Self {
            hz: (mhz * 1_000_000.0) as u64,
        }
    }

    /// Returns the frequency in Hz.
    pub fn hz(&self) -> u64 {
        self.hz
    }

    /// Returns the frequency in kHz.
    pub fn khz(&self) -> u32 {
        (self.hz / 1_000) as u32
    }

    /// Returns the frequency in MHz.
    pub fn mhz(&self) -> f32 {
        self.hz as f32 / 1_000_000.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_hz() {
        let freq = Frequency::from_hz(25_000_000);
        assert_eq!(freq.hz(), 25_000_000);
        assert_eq!(freq.khz(), 25_000);
        assert_eq!(freq.mhz(), 25.0);
    }

    #[test]
    fn from_khz() {
        let freq = Frequency::from_khz(25_000);
        assert_eq!(freq.hz(), 25_000_000);
        assert_eq!(freq.khz(), 25_000);
        assert_eq!(freq.mhz(), 25.0);
    }

    #[test]
    fn from_mhz() {
        let freq = Frequency::from_mhz(400.0);
        assert_eq!(freq.mhz(), 400.0);
        assert_eq!(freq.khz(), 400_000);
        assert_eq!(freq.hz(), 400_000_000);
    }

    #[test]
    fn fractional_mhz() {
        let freq = Frequency::from_mhz(62.5);
        assert_eq!(freq.hz(), 62_500_000);
        assert_eq!(freq.mhz(), 62.5);
    }

    #[test]
    fn ordering() {
        let low = Frequency::from_mhz(50.0);
        let high = Frequency::from_mhz(600.0);
        assert!(low < high);
    }

    #[test]
    fn default_is_zero() {
        let freq = Frequency::default();
        assert_eq!(freq.hz(), 0);
    }
}
