//! Device version from bitaxe-raw's bcdDevice encoding.

use std::fmt;

/// Device version decoded from the bitaxe-raw bcdDevice convention.
///
/// USB bcdDevice is a vendor-defined release number. The bitaxe-raw
/// firmware encodes it as `0xJJMN`:
///   - `JJ` (bits 15:8) -- hardware revision
///   - `M`  (bits 7:4)  -- firmware minor version
///   - `N`  (bits 3:0)  -- firmware patch version
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceVersion {
    hardware: u8,
    firmware_minor: u8,
    firmware_patch: u8,
}

impl DeviceVersion {
    /// Decode from a raw USB bcdDevice value.
    pub fn from_bcd(bcd: u16) -> Self {
        Self {
            hardware: (bcd >> 8) as u8,
            firmware_minor: ((bcd >> 4) & 0x0F) as u8,
            firmware_patch: (bcd & 0x0F) as u8,
        }
    }

    /// Hardware revision (`JJ` field).
    pub fn hardware(&self) -> u8 {
        self.hardware
    }

    /// Firmware minor version (`M` field).
    pub fn firmware_minor(&self) -> u8 {
        self.firmware_minor
    }

    /// Firmware patch version (`N` field).
    pub fn firmware_patch(&self) -> u8 {
        self.firmware_patch
    }
}

impl fmt::Display for DeviceVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "hw{:02X} fw{}.{}",
            self.hardware, self.firmware_minor, self.firmware_patch
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_bcd() {
        let v = DeviceVersion::from_bcd(0x0510);
        assert_eq!(v.hardware(), 0x05);
        assert_eq!(v.firmware_minor(), 1);
        assert_eq!(v.firmware_patch(), 0);

        let v = DeviceVersion::from_bcd(0x0000);
        assert_eq!(v.hardware(), 0x00);
        assert_eq!(v.firmware_minor(), 0);
        assert_eq!(v.firmware_patch(), 0);

        let v = DeviceVersion::from_bcd(0xFF9A);
        assert_eq!(v.hardware(), 0xFF);
        assert_eq!(v.firmware_minor(), 9);
        assert_eq!(v.firmware_patch(), 0x0A);
    }

    #[test]
    fn display() {
        let v = DeviceVersion::from_bcd(0x0510);
        assert_eq!(v.to_string(), "hw05 fw1.0");
    }
}
