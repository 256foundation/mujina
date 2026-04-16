//! Conversions from peripheral driver readings to Temperature.

use super::Temperature;
use crate::peripheral::{tmp451, tmp1075};

impl From<tmp1075::Reading> for Temperature {
    fn from(r: tmp1075::Reading) -> Self {
        Self::from_celsius(r.as_degrees_c())
    }
}

impl From<tmp451::Reading> for Temperature {
    fn from(r: tmp451::Reading) -> Self {
        Self::from_celsius(r.as_degrees_c())
    }
}
