//! Temperature measurement type.

mod conversions;

use serde::{Deserialize, Serialize};
use std::fmt;

/// Temperature in degrees Celsius.
///
/// A newtype preventing bare floats from being passed where a
/// temperature is expected. This catches two classes of mistakes
/// at compile time: using the wrong quantity (e.g., a voltage
/// where a temperature belongs) and using the wrong unit.
/// Constructors like from_celsius make the input unit explicit;
/// additional constructors (from_fahrenheit, etc.) can convert
/// on the way in.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Temperature(f32);

impl Temperature {
    /// Construct from a value in degrees Celsius.
    pub fn from_celsius(degrees: f32) -> Self {
        Self(degrees)
    }

    /// Return the temperature in degrees Celsius.
    pub fn as_degrees_c(self) -> f32 {
        self.0
    }
}

impl fmt::Display for Temperature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.1} C", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let t = Temperature::from_celsius(42.5);
        assert_eq!(t.as_degrees_c(), 42.5);
    }

    #[test]
    fn display() {
        let t = Temperature::from_celsius(42.5);
        assert_eq!(t.to_string(), "42.5 C");
    }

    #[test]
    fn ordering() {
        let cold = Temperature::from_celsius(10.0);
        let hot = Temperature::from_celsius(80.0);
        assert!(cold < hot);
    }

    #[test]
    fn negative() {
        let t = Temperature::from_celsius(-40.0);
        assert_eq!(t.as_degrees_c(), -40.0);
        assert_eq!(t.to_string(), "-40.0 C");
    }
}
