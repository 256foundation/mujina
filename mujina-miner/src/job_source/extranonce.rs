//! Extranonce types for Bitcoin mining.
//!
//! In Bitcoin mining, the search space is multidimensional:
//!
//! - **Nonce** (32 bits) - Rolled by hardware
//! - **Version** (maskable bits) - Rolled by hardware if enabled
//! - **Extranonce** (variable size) - Typically rolled by software
//! - **nTime** (32 bits) - Typically rolled by software
//!
//! This module provides three types for managing the extranonce dimension:
//!
//! - `Extranonce`: An immutable value with a specific size (1-8 bytes)
//! - `ExtranonceRange`: A range specification [min, max] with no position state
//! - `ExtranonceIter`: An iterator that generates `Extranonce` values from a range
//!
//! Mining pools allocate a specific byte size for extranonce (typically 4-8 bytes),
//! which determines how many unique coinbase transactions a miner can generate before
//! needing new work. The range type provides splitting for dividing work between
//! domains, while the iterator type handles sequential value generation.

use std::fmt;

use thiserror::Error;

/// Errors that can occur when creating Extranonce types.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ExtranonceError {
    #[error("Invalid extranonce size: {0} (must be 1-8 bytes)")]
    InvalidSize(u8),

    #[error("Value {0} exceeds maximum for size {1} bytes")]
    ValueTooLarge(u64, u8),

    #[error("Invalid range: min {0} >= max {1}")]
    InvalidRange(u64, u64),
}

/// A specific extranonce value with fixed size.
///
/// This is an immutable value type representing a single extranonce that will be
/// serialized into a coinbase transaction or stored in a share. The value is stored
/// as a u64 but serializes to the specified number of bytes (1-8), most significant
/// byte first. Successive values then differ in their last bytes on the wire, and
/// splitting a range fixes the leading bytes, as Stratum V2 subdivides an extranonce.
///
/// Use `ExtranonceRange::iter()` to generate sequences of these values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Extranonce {
    value: u64,
    size: u8,
}

impl Extranonce {
    /// Create a new extranonce value.
    ///
    /// Returns an error if the size is invalid (must be 1-8 bytes) or if the value
    /// is too large to fit in the specified size.
    pub fn new(value: u64, size: u8) -> Result<Self, ExtranonceError> {
        if size == 0 || size > 8 {
            return Err(ExtranonceError::InvalidSize(size));
        }

        let max = Self::max_for_size(size);
        if value > max {
            return Err(ExtranonceError::ValueTooLarge(value, size));
        }

        Ok(Self { value, size })
    }

    /// Get the value as a u64.
    pub fn value(&self) -> u64 {
        self.value
    }

    /// Get the size in bytes.
    pub fn size(&self) -> u8 {
        self.size
    }

    /// Get the maximum value for a given size.
    fn max_for_size(size: u8) -> u64 {
        if size >= 8 {
            u64::MAX
        } else {
            (1u64 << (size * 8)) - 1
        }
    }

    /// Appends the serialized bytes of this extranonce to a vector.
    pub fn extend_vec(&self, vec: &mut Vec<u8>) {
        let bytes = self.value.to_be_bytes();
        vec.extend_from_slice(&bytes[8 - self.size as usize..]);
    }
}

impl From<Extranonce> for Vec<u8> {
    /// Converts to the bytes that go into the coinbase transaction.
    fn from(ext: Extranonce) -> Vec<u8> {
        let mut vec = Vec::with_capacity(ext.size as usize);
        ext.extend_vec(&mut vec);
        vec
    }
}

impl fmt::Display for Extranonce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:0width$x}", self.value, width = self.size as usize * 2)
    }
}

/// A range specification for extranonce values.
///
/// Defines the available extranonce space [min, max] without tracking position.
/// Ranges are immutable and can be split into non-overlapping sub-ranges for
/// distributing work between boards or chip chains.
///
/// Call `.iter()` to create an `ExtranonceIter` for generating values.
///
/// Can be constructed with struct literals or via `new()` / `new_range()` for
/// validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtranonceRange {
    pub min: u64,
    pub max: u64,
    pub size: u8,
}

impl ExtranonceRange {
    /// Create a range covering the full space for the given size.
    pub fn new(size: u8) -> Result<Self, ExtranonceError> {
        if size == 0 || size > 8 {
            return Err(ExtranonceError::InvalidSize(size));
        }

        let max = Extranonce::max_for_size(size);
        Ok(Self { min: 0, max, size })
    }

    /// Create a range with custom bounds.
    pub fn new_range(min: u64, max: u64, size: u8) -> Result<Self, ExtranonceError> {
        if size == 0 || size > 8 {
            return Err(ExtranonceError::InvalidSize(size));
        }

        if min > max {
            return Err(ExtranonceError::InvalidRange(min, max));
        }

        let size_max = Extranonce::max_for_size(size);
        if max > size_max {
            return Err(ExtranonceError::ValueTooLarge(max, size));
        }

        Ok(Self { min, max, size })
    }

    /// Get the total number of values in the range.
    ///
    /// Returns `u64::MAX` if the range spans the entire u64 space (the true
    /// count would be 2^64 which doesn't fit in a u64).
    pub fn len(&self) -> u64 {
        // Use saturating_add to handle the full-range case (min=0, max=u64::MAX)
        (self.max - self.min).saturating_add(1)
    }

    /// Check if the range is empty.
    pub fn is_empty(&self) -> bool {
        self.min > self.max
    }

    /// Split this range into `n` non-overlapping sub-ranges.
    ///
    /// Each sub-range will have approximately the same size, with any remainder
    /// distributed among the first few ranges.
    ///
    /// Returns `None` if `n` is 0 or if the range is too small to split.
    pub fn split(&self, n: usize) -> Option<Vec<ExtranonceRange>> {
        if n == 0 {
            return None;
        }

        if n == 1 {
            return Some(vec![self.clone()]);
        }

        let total = self.len();
        if (total as usize) < n {
            return None;
        }

        let chunk_size = total / (n as u64);
        let remainder = total % (n as u64);

        let mut ranges = Vec::with_capacity(n);
        let mut start = self.min;

        for i in 0..n {
            // Distribute remainder among first few chunks
            let size = chunk_size + if (i as u64) < remainder { 1 } else { 0 };
            let end = start + size - 1;

            ranges.push(Self::new_range(start, end, self.size).expect("sub-range should be valid"));

            start = end + 1;
        }

        Some(ranges)
    }

    /// Create an iterator over this range.
    pub fn iter(&self) -> ExtranonceIter {
        ExtranonceIter {
            range: self.clone(),
            current: self.min,
        }
    }
}

/// Iterator that generates `Extranonce` values from a range.
///
/// Created via `ExtranonceRange::iter()`. Implements Rust's `Iterator` trait
/// for use in `for` loops and with iterator combinators.
#[derive(Clone)]
pub struct ExtranonceIter {
    range: ExtranonceRange,
    current: u64,
}

impl ExtranonceIter {
    /// Get the current value without advancing.
    pub fn current(&self) -> Extranonce {
        Extranonce::new(self.current, self.range.size).expect("current should be valid")
    }

    /// Reset to the beginning of the range.
    pub fn reset(&mut self) {
        self.current = self.range.min;
    }
}

impl Iterator for ExtranonceIter {
    type Item = Extranonce;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current > self.range.max {
            return None;
        }
        let val = Extranonce::new(self.current, self.range.size).ok()?;
        self.current += 1;
        Some(val)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        if self.current > self.range.max {
            (0, Some(0))
        } else {
            let remaining = (self.range.max - self.current + 1).min(usize::MAX as u64) as usize;
            (remaining, Some(remaining))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Extranonce value type tests
    #[test]
    fn test_extranonce_new() {
        let ext = Extranonce::new(0, 4).unwrap();
        assert_eq!(ext.value(), 0);
        assert_eq!(ext.size(), 4);

        let ext = Extranonce::new(0x1234, 4).unwrap();
        assert_eq!(ext.value(), 0x1234);
    }

    #[test]
    fn test_extranonce_errors() {
        // Invalid size
        assert!(matches!(
            Extranonce::new(0, 0),
            Err(ExtranonceError::InvalidSize(0))
        ));
        assert!(matches!(
            Extranonce::new(0, 9),
            Err(ExtranonceError::InvalidSize(9))
        ));

        // Value too large for size
        assert!(matches!(
            Extranonce::new(0x100, 1),
            Err(ExtranonceError::ValueTooLarge(0x100, 1))
        ));
        assert!(matches!(
            Extranonce::new(0x1_0000, 2),
            Err(ExtranonceError::ValueTooLarge(0x1_0000, 2))
        ));
    }

    #[test]
    fn test_extranonce_to_bytes() {
        let ext = Extranonce::new(0, 4).unwrap();
        assert_eq!(Vec::<u8>::from(ext), vec![0, 0, 0, 0]);

        let ext = Extranonce::new(0x1234, 4).unwrap();
        assert_eq!(Vec::<u8>::from(ext), vec![0, 0, 0x12, 0x34]); // Big-endian

        let ext = Extranonce::new(0xab, 1).unwrap();
        assert_eq!(Vec::<u8>::from(ext), vec![0xab]);
    }

    #[test]
    fn test_extranonce_display() {
        let ext = Extranonce::new(0, 4).unwrap();
        assert_eq!(format!("{}", ext), "00000000");

        let ext = Extranonce::new(0x1234, 4).unwrap();
        assert_eq!(format!("{}", ext), "00001234");

        let ext = Extranonce::new(0xab, 2).unwrap();
        assert_eq!(format!("{}", ext), "00ab");
    }

    // ExtranonceRange tests
    #[test]
    fn test_range_new() {
        let range = ExtranonceRange::new(4).unwrap();
        assert_eq!(range.len(), 1u64 << 32);
        assert!(!range.is_empty());
    }

    #[test]
    fn test_range_new_range() {
        let range = ExtranonceRange::new_range(0x1000, 0x2000, 4).unwrap();
        assert_eq!(range.len(), 0x1001);
    }

    #[test]
    fn test_range_split() {
        let range = ExtranonceRange::new_range(0, 99, 1).unwrap();
        let splits = range.split(4).unwrap();

        assert_eq!(splits.len(), 4);
        assert_eq!(splits[0].len(), 25);
        assert_eq!(splits[1].len(), 25);
        assert_eq!(splits[2].len(), 25);
        assert_eq!(splits[3].len(), 25);

        // Check boundaries
        assert_eq!(splits[0].min, 0);
        assert_eq!(splits[0].max, 24);
        assert_eq!(splits[1].min, 25);
        assert_eq!(splits[1].max, 49);
        assert_eq!(splits[2].min, 50);
        assert_eq!(splits[2].max, 74);
        assert_eq!(splits[3].min, 75);
        assert_eq!(splits[3].max, 99);
    }

    #[test]
    fn test_range_split_with_remainder() {
        let range = ExtranonceRange::new_range(0, 9, 1).unwrap();
        let splits = range.split(3).unwrap();

        assert_eq!(splits.len(), 3);
        // 10 values split 3 ways: 4, 3, 3
        assert_eq!(splits[0].len(), 4);
        assert_eq!(splits[1].len(), 3);
        assert_eq!(splits[2].len(), 3);
    }

    #[test]
    fn test_split_fixes_leading_bytes() {
        let range = ExtranonceRange::new(4).unwrap();
        let splits = range.split(4).unwrap();

        // Each part's values share their first byte, the way a Stratum V2
        // upstream gives each downstream a longer prefix.
        for (i, part) in splits.iter().enumerate() {
            let first = Vec::<u8>::from(part.iter().next().unwrap());
            assert_eq!(first, vec![(i as u8) << 6, 0, 0, 0]);
        }
    }

    #[test]
    fn test_iter_rolls_trailing_bytes() {
        let range = ExtranonceRange::new(4).unwrap();
        let bytes: Vec<Vec<u8>> = range.iter().skip(255).take(2).map(Vec::from).collect();

        assert_eq!(bytes, vec![vec![0, 0, 0, 0xff], vec![0, 0, 1, 0]]);
    }

    // ExtranonceIter tests
    #[test]
    fn test_iter_basic() {
        let range = ExtranonceRange::new_range(0, 2, 1).unwrap();
        let mut iter = range.iter();

        assert_eq!(iter.next().unwrap().value(), 0);
        assert_eq!(iter.next().unwrap().value(), 1);
        assert_eq!(iter.next().unwrap().value(), 2);
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_iter_current_and_reset() {
        let range = ExtranonceRange::new_range(10, 20, 1).unwrap();
        let mut iter = range.iter();

        assert_eq!(iter.current().value(), 10);
        iter.next();
        iter.next();
        assert_eq!(iter.current().value(), 12);

        iter.reset();
        assert_eq!(iter.current().value(), 10);
    }

    #[test]
    fn test_iter_for_loop() {
        let range = ExtranonceRange::new_range(0, 4, 1).unwrap();
        let values: Vec<u64> = range.iter().map(|ex2| ex2.value()).collect();

        assert_eq!(values, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn test_iter_size_hint() {
        let range = ExtranonceRange::new_range(0, 99, 1).unwrap();
        let iter = range.iter();

        let (lower, upper) = iter.size_hint();
        assert_eq!(lower, 100);
        assert_eq!(upper, Some(100));
    }

    #[test]
    fn test_full_u64_range_len() {
        // 8-byte extranonce spans entire u64 space (min=0, max=u64::MAX)
        let range = ExtranonceRange::new(8).unwrap();
        // len() would be 2^64 but that doesn't fit in u64, so saturates to MAX
        assert_eq!(range.len(), u64::MAX);
    }

    #[test]
    fn test_iter_combinators() {
        let range = ExtranonceRange::new_range(0, 99, 1).unwrap();

        // Take first 10
        let values: Vec<u64> = range.iter().take(10).map(|ex2| ex2.value()).collect();
        assert_eq!(values.len(), 10);
        assert_eq!(values[0], 0);
        assert_eq!(values[9], 9);

        // Skip first 90, take 5
        let values: Vec<u64> = range
            .iter()
            .skip(90)
            .take(5)
            .map(|ex2| ex2.value())
            .collect();
        assert_eq!(values, vec![90, 91, 92, 93, 94]);
    }
}
