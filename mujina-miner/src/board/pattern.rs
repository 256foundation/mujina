//! Board matching patterns for flexible device detection.
//!
//! This module provides a pattern-based system for matching USB devices to
//! board implementations. Patterns can specify any combination of VID, PID,
//! manufacturer string, product string, and serial number pattern.
//!
//! ## Specificity Scoring
//!
//! When multiple patterns match a device, the most specific pattern wins.
//! Specificity is calculated by assigning points to each matching criterion:
//!
//! - VID specified: +10 points
//! - PID specified: +10 points
//! - Manufacturer exact match: +20 points (regex: +15, contains: +10)
//! - Product exact match: +20 points (regex: +15, contains: +10)
//! - Serial pattern exact: +20 points (regex: +15, contains: +10)
//!
//! ## Example
//!
//! ```rust,ignore
//! // High specificity: vid + pid + exact product = 50 points
//! BoardPattern {
//!     vid: Match::Specific(0x0403),
//!     pid: Match::Specific(0x6015),
//!     manufacturer: Match::Any,
//!     product: Match::Specific(StringMatch::Exact("Bitaxe Gamma")),
//!     serial_pattern: Match::Any,
//! }
//!
//! // Lower specificity: vid + contains manufacturer = 20 points
//! BoardPattern {
//!     vid: Match::Specific(0x0403),
//!     pid: Match::Any,
//!     manufacturer: Match::Specific(StringMatch::Contains("FTDI")),
//!     product: Match::Any,
//!     serial_pattern: Match::Any,
//! }
//! ```

use regex::Regex;

use crate::transport::UsbDeviceInfo;

/// Matching criterion that can be either a wildcard or a specific value.
///
/// This is semantically equivalent to `Option<T>` but uses clearer names:
/// `Match::Any` makes it obvious that we're matching any value (wildcard),
/// while `Match::Specific(v)` shows we require a particular value.
#[derive(Debug, Clone)]
pub enum Match<T> {
    /// Match any value (wildcard)
    Any,
    /// Match a specific value
    Specific(T),
}

impl<T> Match<T> {
    /// Check if this is a wildcard match.
    pub fn is_any(&self) -> bool {
        matches!(self, Match::Any)
    }

    /// Check if this is a specific value match.
    pub fn is_specific(&self) -> bool {
        matches!(self, Match::Specific(_))
    }
}

/// String matching strategy for device attributes.
///
/// Uses `&'static str` so patterns can be used in const/static contexts
/// (like inventory::submit! blocks). Regex patterns are compiled at runtime
/// when first used.
#[derive(Debug, Clone)]
pub enum StringMatch {
    /// Exact string match (case-sensitive, most specific)
    Exact(&'static str),
    /// Regular expression pattern match (pattern compiled at runtime)
    Regex(&'static str),
    /// Case-insensitive substring match (least specific)
    Contains(&'static str),
}

/// BCD version matching strategy for USB bcdDevice field.
///
/// USB bcdDevice is a BCD-encoded version number. The convention used here is:
/// `0xJJMN` = version `JJ.M.N`, where major (`JJ`) indicates hardware revision
/// and minor.patch (`M.N`) indicates firmware version.
#[derive(Debug, Clone, Copy)]
pub enum BcdVersionMatch {
    /// Match exact bcdDevice value
    Exact(u16),
    /// Match only the major version (hardware revision).
    /// For `0xJJMN`, this matches any device where `JJ` equals the specified value.
    Major(u8),
}

impl BcdVersionMatch {
    /// Check if this matcher matches the given bcdDevice value.
    pub fn matches(&self, bcd_device: u16) -> bool {
        match self {
            BcdVersionMatch::Exact(expected) => bcd_device == *expected,
            BcdVersionMatch::Major(major) => {
                // Extract major version (upper byte) from 0xJJMN
                let device_major = (bcd_device >> 8) as u8;
                device_major == *major
            }
        }
    }

    /// Calculate specificity score for this matcher type.
    fn specificity(&self) -> u32 {
        match self {
            BcdVersionMatch::Exact(_) => 15,
            BcdVersionMatch::Major(_) => 10,
        }
    }
}

impl StringMatch {
    /// Check if this matcher matches the given string.
    pub fn matches(&self, s: &Option<String>) -> bool {
        match s {
            None => false, // No string to match against
            Some(value) => match self {
                StringMatch::Exact(expected) => value == *expected,
                StringMatch::Regex(pattern) => {
                    // Compile regex at runtime when needed
                    // For device matching (rare operation), compilation cost is negligible
                    match Regex::new(pattern) {
                        Ok(re) => re.is_match(value),
                        Err(_) => {
                            // Invalid regex pattern - log and fail to match
                            tracing::warn!("Invalid regex pattern: {}", pattern);
                            false
                        }
                    }
                }
                StringMatch::Contains(substring) => {
                    value.to_lowercase().contains(&substring.to_lowercase())
                }
            },
        }
    }

    /// Calculate specificity score for this matcher type.
    fn specificity(&self) -> u32 {
        match self {
            StringMatch::Exact(_) => 20,
            StringMatch::Regex(_) => 15,
            StringMatch::Contains(_) => 10,
        }
    }
}

/// Pattern for matching USB devices to board types.
///
/// Each field uses `Match<T>` to explicitly show whether it's a wildcard
/// (`Match::Any`) or requires a specific value (`Match::Specific(v)`).
/// The more specific fields, the higher the pattern's specificity score.
#[derive(Debug, Clone)]
pub struct BoardPattern {
    /// USB Vendor ID
    pub vid: Match<u16>,
    /// USB Product ID
    pub pid: Match<u16>,
    /// USB device release number (bcdDevice).
    /// Uses BCD format `0xJJMN` = version `JJ.M.N` where major indicates
    /// hardware revision and minor.patch indicates firmware version.
    pub bcd_device: Match<BcdVersionMatch>,
    /// Manufacturer string match
    pub manufacturer: Match<StringMatch>,
    /// Product string match
    pub product: Match<StringMatch>,
    /// Serial number pattern
    pub serial_pattern: Match<StringMatch>,
}

impl BoardPattern {
    /// Create a new pattern with all fields as wildcards.
    ///
    /// This is a const fn so it can be used in static contexts, though
    /// struct update syntax (`..wildcard()`) won't work in statics due
    /// to limitations around temporaries in const evaluation.
    pub const fn wildcard() -> Self {
        Self {
            vid: Match::Any,
            pid: Match::Any,
            bcd_device: Match::Any,
            manufacturer: Match::Any,
            product: Match::Any,
            serial_pattern: Match::Any,
        }
    }

    /// Check if this pattern matches the given device.
    ///
    /// Returns true if all specified fields match. `Match::Any` fields
    /// always match (wildcards).
    pub fn matches(&self, device: &UsbDeviceInfo) -> bool {
        // VID must match if specified
        if let Match::Specific(vid) = self.vid
            && device.vid != vid
        {
            return false;
        }

        // PID must match if specified
        if let Match::Specific(pid) = self.pid
            && device.pid != pid
        {
            return false;
        }

        // bcdDevice must match if specified
        if let Match::Specific(ref matcher) = self.bcd_device
            && !matcher.matches(device.bcd_device)
        {
            return false;
        }

        // Manufacturer must match if specified
        if let Match::Specific(ref matcher) = self.manufacturer
            && !matcher.matches(&device.manufacturer)
        {
            return false;
        }

        // Product must match if specified
        if let Match::Specific(ref matcher) = self.product
            && !matcher.matches(&device.product)
        {
            return false;
        }

        // Serial pattern must match if specified
        if let Match::Specific(ref matcher) = self.serial_pattern
            && !matcher.matches(&device.serial_number)
        {
            return false;
        }

        true
    }

    /// Calculate the specificity score for this pattern.
    ///
    /// Higher scores indicate more specific patterns. When multiple patterns
    /// match a device, the one with the highest specificity is selected.
    pub fn specificity(&self) -> u32 {
        let mut score = 0;

        if self.vid.is_specific() {
            score += 10;
        }
        if self.pid.is_specific() {
            score += 10;
        }
        if let Match::Specific(ref m) = self.bcd_device {
            score += m.specificity();
        }
        if let Match::Specific(ref m) = self.manufacturer {
            score += m.specificity();
        }
        if let Match::Specific(ref m) = self.product {
            score += m.specificity();
        }
        if let Match::Specific(ref m) = self.serial_pattern {
            score += m.specificity();
        }

        score
    }
}

// NOTE: No Default implementation provided.
//
// While we could implement Default (returning wildcard()), it's not particularly
// useful because BoardPattern is typically constructed in static inventory::submit!
// blocks where Default::default() cannot be called (not const). The explicit
// wildcard() const fn exists for const contexts, and Default would only work
// in runtime code where you can just write out the fields anyway.

#[cfg(test)]
mod tests {
    use super::*;

    fn make_device(
        vid: u16,
        pid: u16,
        manufacturer: Option<&str>,
        product: Option<&str>,
        serial: Option<&str>,
    ) -> UsbDeviceInfo {
        UsbDeviceInfo {
            vid,
            pid,
            serial_number: serial.map(|s| s.to_string()),
            manufacturer: manufacturer.map(|s| s.to_string()),
            product: product.map(|s| s.to_string()),
            device_path: "/sys/devices/test".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn test_exact_match() {
        let pattern = BoardPattern {
            vid: Match::Specific(0x1234),
            pid: Match::Specific(0x5678),
            bcd_device: Match::Any,
            manufacturer: Match::Specific(StringMatch::Exact("ACME Corp")),
            product: Match::Specific(StringMatch::Exact("Widget Pro")),
            serial_pattern: Match::Any,
        };

        // Exact match
        let device = make_device(0x1234, 0x5678, Some("ACME Corp"), Some("Widget Pro"), None);
        assert!(pattern.matches(&device));

        // Wrong manufacturer
        let device = make_device(0x1234, 0x5678, Some("Other Corp"), Some("Widget Pro"), None);
        assert!(!pattern.matches(&device));

        // Missing manufacturer
        let device = make_device(0x1234, 0x5678, None, Some("Widget Pro"), None);
        assert!(!pattern.matches(&device));
    }

    #[test]
    fn test_regex_match() {
        let pattern = BoardPattern {
            vid: Match::Specific(0x1234),
            pid: Match::Any,
            bcd_device: Match::Any,
            manufacturer: Match::Any,
            product: Match::Specific(StringMatch::Regex(r"Bitaxe.*Gamma")),
            serial_pattern: Match::Any,
        };

        let device = make_device(0x1234, 0x5678, None, Some("Bitaxe Ultra Gamma"), None);
        assert!(pattern.matches(&device));

        let device = make_device(0x1234, 0x5678, None, Some("Bitaxe Gamma"), None);
        assert!(pattern.matches(&device));

        let device = make_device(0x1234, 0x5678, None, Some("Bitaxe Ultra"), None);
        assert!(!pattern.matches(&device));
    }

    #[test]
    fn test_contains_match() {
        let pattern = BoardPattern {
            vid: Match::Specific(0x0403),
            pid: Match::Any,
            bcd_device: Match::Any,
            manufacturer: Match::Specific(StringMatch::Contains("FTDI")),
            product: Match::Any,
            serial_pattern: Match::Any,
        };

        let device = make_device(0x0403, 0x6015, Some("FTDI"), None, None);
        assert!(pattern.matches(&device));

        let device = make_device(0x0403, 0x6015, Some("FTDI LLC"), None, None);
        assert!(pattern.matches(&device)); // Contains "FTDI"

        let device = make_device(0x0403, 0x6015, Some("ftdi corporation"), None, None);
        assert!(pattern.matches(&device)); // Case insensitive

        let device = make_device(0x0403, 0x6015, Some("Silicon Labs"), None, None);
        assert!(!pattern.matches(&device));
    }

    #[test]
    fn test_wildcard_matching() {
        let pattern = BoardPattern {
            vid: Match::Specific(0x1234),
            pid: Match::Any,
            bcd_device: Match::Any,
            manufacturer: Match::Any,
            product: Match::Any,
            serial_pattern: Match::Any,
        };

        // Should match any device with VID 0x1234
        let device = make_device(
            0x1234,
            0xAAAA,
            Some("Manufacturer A"),
            Some("Product A"),
            Some("SN123"),
        );
        assert!(pattern.matches(&device));

        let device = make_device(0x1234, 0xBBBB, None, None, None);
        assert!(pattern.matches(&device));

        // Wrong VID
        let device = make_device(0x9999, 0xAAAA, None, None, None);
        assert!(!pattern.matches(&device));
    }

    #[test]
    fn test_specificity_ordering() {
        // More fields specified = higher specificity
        let vid_only = BoardPattern {
            vid: Match::Specific(0x1234),
            pid: Match::Any,
            bcd_device: Match::Any,
            manufacturer: Match::Any,
            product: Match::Any,
            serial_pattern: Match::Any,
        };
        let vid_and_pid = BoardPattern {
            vid: Match::Specific(0x1234),
            pid: Match::Specific(0x5678),
            bcd_device: Match::Any,
            manufacturer: Match::Any,
            product: Match::Any,
            serial_pattern: Match::Any,
        };
        assert!(vid_and_pid.specificity() > vid_only.specificity());

        // Exact match beats regex beats contains
        let exact = BoardPattern {
            vid: Match::Specific(0x1234),
            pid: Match::Any,
            bcd_device: Match::Any,
            manufacturer: Match::Specific(StringMatch::Exact("ACME")),
            product: Match::Any,
            serial_pattern: Match::Any,
        };
        let regex = BoardPattern {
            vid: Match::Specific(0x1234),
            pid: Match::Any,
            bcd_device: Match::Any,
            manufacturer: Match::Specific(StringMatch::Regex("ACME")),
            product: Match::Any,
            serial_pattern: Match::Any,
        };
        let contains = BoardPattern {
            vid: Match::Specific(0x1234),
            pid: Match::Any,
            bcd_device: Match::Any,
            manufacturer: Match::Specific(StringMatch::Contains("ACME")),
            product: Match::Any,
            serial_pattern: Match::Any,
        };
        assert!(exact.specificity() > regex.specificity());
        assert!(regex.specificity() > contains.specificity());

        // VID+PID+exact manufacturer beats VID+PID alone
        let with_manufacturer = BoardPattern {
            vid: Match::Specific(0x1234),
            pid: Match::Specific(0x5678),
            bcd_device: Match::Any,
            manufacturer: Match::Specific(StringMatch::Exact("ACME")),
            product: Match::Any,
            serial_pattern: Match::Any,
        };
        assert!(with_manufacturer.specificity() > vid_and_pid.specificity());

        // Full specification beats partial specification
        let full_spec = BoardPattern {
            vid: Match::Specific(0x1234),
            pid: Match::Specific(0x5678),
            bcd_device: Match::Any,
            manufacturer: Match::Specific(StringMatch::Exact("ACME")),
            product: Match::Specific(StringMatch::Exact("Widget")),
            serial_pattern: Match::Specific(StringMatch::Exact("SN123")),
        };
        assert!(full_spec.specificity() > with_manufacturer.specificity());
    }

    #[test]
    fn test_best_match_selection() {
        // Simulate the registry selecting the best matching pattern
        let device = make_device(
            0x0403,
            0x6015,
            Some("FTDI"),
            Some("Bitaxe Gamma"),
            Some("BX001"),
        );

        // Generic FTDI fallback - matches any FTDI device
        let generic_ftdi = BoardPattern {
            vid: Match::Specific(0x0403),
            pid: Match::Any,
            bcd_device: Match::Any,
            manufacturer: Match::Specific(StringMatch::Contains("FTDI")),
            product: Match::Any,
            serial_pattern: Match::Any,
        };

        // Specific Bitaxe - matches VID+PID+product
        let bitaxe = BoardPattern {
            vid: Match::Specific(0x0403),
            pid: Match::Specific(0x6015),
            bcd_device: Match::Any,
            manufacturer: Match::Any,
            product: Match::Specific(StringMatch::Regex("Bitaxe")),
            serial_pattern: Match::Any,
        };

        // Very specific Bitaxe Gamma - matches everything
        let bitaxe_gamma = BoardPattern {
            vid: Match::Specific(0x0403),
            pid: Match::Specific(0x6015),
            bcd_device: Match::Any,
            manufacturer: Match::Any,
            product: Match::Specific(StringMatch::Exact("Bitaxe Gamma")),
            serial_pattern: Match::Specific(StringMatch::Regex("^BX")),
        };

        // All three patterns match the device
        assert!(generic_ftdi.matches(&device));
        assert!(bitaxe.matches(&device));
        assert!(bitaxe_gamma.matches(&device));

        // But the most specific one should win
        assert!(bitaxe_gamma.specificity() > bitaxe.specificity());
        assert!(bitaxe.specificity() > generic_ftdi.specificity());

        // Simulate registry selection: pick the highest specificity
        let patterns = vec![&generic_ftdi, &bitaxe, &bitaxe_gamma];
        let best_match = patterns
            .into_iter()
            .filter(|p| p.matches(&device))
            .max_by_key(|p| p.specificity())
            .unwrap();

        // The most specific pattern should be selected
        assert_eq!(best_match.specificity(), bitaxe_gamma.specificity());
    }

    #[test]
    fn test_serial_pattern_matching() {
        let pattern = BoardPattern {
            vid: Match::Specific(0x1234),
            pid: Match::Any,
            bcd_device: Match::Any,
            manufacturer: Match::Any,
            product: Match::Any,
            serial_pattern: Match::Specific(StringMatch::Regex(r"^E1-\d+$")),
        };

        let device = make_device(0x1234, 0x5678, None, None, Some("E1-12345"));
        assert!(pattern.matches(&device));

        let device = make_device(0x1234, 0x5678, None, None, Some("E1-999"));
        assert!(pattern.matches(&device));

        let device = make_device(0x1234, 0x5678, None, None, Some("E2-12345"));
        assert!(!pattern.matches(&device));

        let device = make_device(0x1234, 0x5678, None, None, None);
        assert!(!pattern.matches(&device)); // No serial number
    }

    fn make_device_with_bcd(
        vid: u16,
        pid: u16,
        bcd_device: u16,
        manufacturer: Option<&str>,
        product: Option<&str>,
        serial: Option<&str>,
    ) -> UsbDeviceInfo {
        UsbDeviceInfo {
            vid,
            pid,
            bcd_device,
            serial_number: serial.map(|s| s.to_string()),
            manufacturer: manufacturer.map(|s| s.to_string()),
            product: product.map(|s| s.to_string()),
            device_path: "/sys/devices/test".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn test_bcd_version_exact_match() {
        let pattern = BoardPattern {
            vid: Match::Any,
            pid: Match::Any,
            bcd_device: Match::Specific(BcdVersionMatch::Exact(0x0500)),
            manufacturer: Match::Any,
            product: Match::Any,
            serial_pattern: Match::Any,
        };

        // Exact match
        let device = make_device_with_bcd(0x1234, 0x5678, 0x0500, None, None, None);
        assert!(pattern.matches(&device));

        // Wrong bcdDevice
        let device = make_device_with_bcd(0x1234, 0x5678, 0x0501, None, None, None);
        assert!(!pattern.matches(&device));
    }

    #[test]
    fn test_bcd_version_major_match() {
        // Match hardware revision 5 (0x05xx)
        let pattern = BoardPattern {
            vid: Match::Any,
            pid: Match::Any,
            bcd_device: Match::Specific(BcdVersionMatch::Major(5)),
            manufacturer: Match::Any,
            product: Match::Any,
            serial_pattern: Match::Any,
        };

        // 0x0500 = version 5.0.0
        let device = make_device_with_bcd(0x1234, 0x5678, 0x0500, None, None, None);
        assert!(pattern.matches(&device));

        // 0x0512 = version 5.1.2
        let device = make_device_with_bcd(0x1234, 0x5678, 0x0512, None, None, None);
        assert!(pattern.matches(&device));

        // 0x05FF = version 5.15.15 (max minor/patch)
        let device = make_device_with_bcd(0x1234, 0x5678, 0x05FF, None, None, None);
        assert!(pattern.matches(&device));

        // 0x0400 = version 4.0.0 (wrong major)
        let device = make_device_with_bcd(0x1234, 0x5678, 0x0400, None, None, None);
        assert!(!pattern.matches(&device));

        // 0x0600 = version 6.0.0 (wrong major)
        let device = make_device_with_bcd(0x1234, 0x5678, 0x0600, None, None, None);
        assert!(!pattern.matches(&device));
    }

    #[test]
    fn test_bcd_version_specificity() {
        // Exact match should have higher specificity than major match
        let exact = BoardPattern {
            vid: Match::Any,
            pid: Match::Any,
            bcd_device: Match::Specific(BcdVersionMatch::Exact(0x0500)),
            manufacturer: Match::Any,
            product: Match::Any,
            serial_pattern: Match::Any,
        };

        let major = BoardPattern {
            vid: Match::Any,
            pid: Match::Any,
            bcd_device: Match::Specific(BcdVersionMatch::Major(5)),
            manufacturer: Match::Any,
            product: Match::Any,
            serial_pattern: Match::Any,
        };

        let wildcard = BoardPattern {
            vid: Match::Any,
            pid: Match::Any,
            bcd_device: Match::Any,
            manufacturer: Match::Any,
            product: Match::Any,
            serial_pattern: Match::Any,
        };

        assert!(exact.specificity() > major.specificity());
        assert!(major.specificity() > wildcard.specificity());
    }
}
