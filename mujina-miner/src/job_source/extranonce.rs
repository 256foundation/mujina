//! Extranonce types for Bitcoin mining.
//!
//! In Bitcoin mining, the search space is multidimensional:
//!
//! - **Nonce** (32 bits) - Rolled by hardware
//! - **Version** (maskable bits) - Rolled by hardware if enabled
//! - **Extranonce** (variable size) - Typically rolled by software
//! - **nTime** (32 bits) - Typically rolled by software
//!
//! The upstream gives the miner a slot of 1--32 bytes in the coinbase
//! transaction, right after the extranonce prefix. This module provides
//! three types for filling that slot:
//!
//! - `Extranonce`: the bytes for one slot
//! - `ExtranonceRange`: a contiguous run of extranonces, which splits into
//!   smaller runs to divide work among hash threads
//! - `ExtranonceIter`: an iterator over the extranonces in a range
//!
//! An extranonce holds a 64-bit counter, written most significant byte
//! first at the right end of the slot. In a slot wider than the counter,
//! the bytes left of the counter are zero. Rolling changes the bytes at the
//! right first, and splitting a range fixes bytes at the left, the way
//! Stratum V2 divides an extranonce between an upstream and its downstreams.
//!
//! A 64-bit counter is enough even split among many hash threads, so the
//! slot's extra width buys nothing. Split into 2^20 parts, each part still
//! holds 2^44 extranonces, which last a 1 EH/s miner 21 hours. A fixed-size
//! integer keeps the types `Copy` and the arithmetic native.
//!
//! Values print as hex in wire order (`00001234`), so a log line matches the
//! hex a pool logs.

use std::fmt;
use std::iter;

use thiserror::Error;

/// Width in bytes of the counter at the right end of the slot.
const COUNTER_WIDTH: usize = size_of::<u64>();

/// The bytes the miner fills in one extranonce slot.
///
/// An immutable value that goes into a coinbase transaction or a share.
/// Obtain one from an `ExtranonceRange`, from bytes with `from_bytes()`, or
/// from a fixed-size array with `from_array()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Extranonce {
    counter: u64,
    width: u8,
}

impl Extranonce {
    /// Maximum width of an extranonce slot in bytes, the limit Stratum V2 sets.
    pub const MAX_WIDTH: usize = 32;

    /// Creates an extranonce from its bytes as they appear in the coinbase.
    ///
    /// Returns an error if the width is outside 1--32 bytes, or if a byte left
    /// of the 8-byte counter is nonzero.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ExtranonceError> {
        let width = check_width(bytes.len())?;

        let (padding, tail) = bytes.split_at(bytes.len().saturating_sub(COUNTER_WIDTH));
        if padding.iter().any(|&b| b != 0) {
            return Err(ExtranonceError::BeyondCounter);
        }

        let mut counter = [0u8; COUNTER_WIDTH];
        counter[COUNTER_WIDTH - tail.len()..].copy_from_slice(tail);
        Ok(Self {
            counter: u64::from_be_bytes(counter),
            width,
        })
    }

    /// Creates an extranonce from an array of 1--8 bytes.
    ///
    /// Unlike `from_bytes()`, this cannot fail. Any such array fits the
    /// counter, and the compiler rejects arrays of other sizes.
    pub const fn from_array<const N: usize>(bytes: [u8; N]) -> Self {
        const { assert!(N >= 1 && N <= COUNTER_WIDTH) };

        let mut counter = [0u8; COUNTER_WIDTH];
        let mut i = 0;
        while i < N {
            counter[COUNTER_WIDTH - N + i] = bytes[i];
            i += 1;
        }
        Self {
            counter: u64::from_be_bytes(counter),
            width: N as u8,
        }
    }

    /// Returns the width of the slot in bytes.
    pub fn width(&self) -> usize {
        self.width as usize
    }

    /// Appends the bytes of this extranonce to a vector.
    pub fn extend_vec(&self, vec: &mut Vec<u8>) {
        vec.extend(self.bytes());
    }

    /// Returns the bytes of the slot in wire order.
    ///
    /// Zeros fill the slot left of the counter, then the counter's low bytes
    /// follow, most significant first.
    fn bytes(&self) -> impl Iterator<Item = u8> {
        let counter_bytes = self.width().min(COUNTER_WIDTH);
        let padding = self.width() - counter_bytes;
        let counter = self.counter.to_be_bytes();
        iter::repeat_n(0, padding).chain(counter.into_iter().skip(COUNTER_WIDTH - counter_bytes))
    }
}

impl From<Extranonce> for Vec<u8> {
    /// Converts to the bytes that go into the coinbase transaction.
    fn from(ext: Extranonce) -> Vec<u8> {
        let mut vec = Vec::with_capacity(ext.width());
        ext.extend_vec(&mut vec);
        vec
    }
}

impl fmt::Display for Extranonce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.bytes().try_for_each(|b| write!(f, "{b:02x}"))
    }
}

/// A contiguous run of extranonces in one slot.
///
/// Ranges are immutable. Obtain the full range of a slot with `full()`, or
/// the rest of a slot from a known extranonce with `starting_at()`, then
/// divide it with `split()`. Call `iter()` to roll through the extranonces
/// in a range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExtranonceRange {
    first: u64,
    last: u64,
    width: u8,
}

impl ExtranonceRange {
    /// Creates a range covering all extranonces of the given width.
    ///
    /// In a slot wider than 8 bytes, the range covers all values of the
    /// counter and leaves the bytes left of it zero.
    pub fn full(width: usize) -> Result<Self, ExtranonceError> {
        let width = check_width(width)?;
        Ok(Self {
            first: 0,
            last: max_counter(width),
            width,
        })
    }

    /// Creates a range from `first` to the last extranonce of its slot.
    pub fn starting_at(first: Extranonce) -> Self {
        Self {
            first: first.counter,
            last: max_counter(first.width),
            width: first.width,
        }
    }

    /// Returns the width of the slot in bytes.
    pub fn width(&self) -> usize {
        self.width as usize
    }

    /// Returns the number of extranonces in the range.
    ///
    /// The count is a `u128` because the full range of a slot of 8 bytes or
    /// more holds 2^64 extranonces.
    fn len(&self) -> u128 {
        u128::from(self.last - self.first) + 1
    }

    /// Splits the range into `n` contiguous parts of nearly equal length.
    ///
    /// When `n` does not divide the range evenly, the first parts each get
    /// one extra extranonce. Returns `None` if `n` is zero or exceeds the
    /// number of extranonces.
    pub fn split(&self, n: usize) -> Option<Vec<Self>> {
        let parts = n as u128;
        if parts == 0 || parts > self.len() {
            return None;
        }

        let base = self.len() / parts;
        let extra = self.len() % parts;

        let mut ranges = Vec::with_capacity(n);
        let mut first = self.first;
        for i in 0..parts {
            // Each part holds 1--2^64 values, so its span fits the counter.
            let span = (base + u128::from(i < extra) - 1) as u64;
            let last = first + span;
            ranges.push(Self {
                first,
                last,
                width: self.width,
            });
            // Wraps only after the final part, whose successor is unused.
            first = last.wrapping_add(1);
        }

        Some(ranges)
    }

    /// Returns an iterator over the extranonces in the range, in order.
    pub fn iter(&self) -> ExtranonceIter {
        ExtranonceIter {
            next: Some(self.first),
            last: self.last,
            width: self.width,
        }
    }

    /// Returns the first extranonce in the range.
    pub fn start(&self) -> Extranonce {
        Extranonce {
            counter: self.first,
            width: self.width,
        }
    }
}

/// Iterator over the extranonces in a range.
///
/// Created by `ExtranonceRange::iter()`.
#[derive(Clone)]
pub struct ExtranonceIter {
    next: Option<u64>,
    last: u64,
    width: u8,
}

impl ExtranonceIter {
    fn extranonce(&self, counter: u64) -> Extranonce {
        Extranonce {
            counter,
            width: self.width,
        }
    }

    fn remaining(&self) -> u128 {
        self.next.map_or(0, |next| u128::from(self.last - next) + 1)
    }
}

impl Iterator for ExtranonceIter {
    type Item = Extranonce;

    fn next(&mut self) -> Option<Self::Item> {
        let counter = self.next?;
        self.next = (counter < self.last).then(|| counter + 1);
        Some(self.extranonce(counter))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let exact = usize::try_from(self.remaining()).ok();
        (exact.unwrap_or(usize::MAX), exact)
    }
}

/// Errors from creating extranonce types.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ExtranonceError {
    #[error("invalid extranonce width: {0} bytes (must be 1-32)")]
    InvalidWidth(usize),

    #[error("extranonce has nonzero bytes left of its 8-byte counter")]
    BeyondCounter,
}

fn check_width(width: usize) -> Result<u8, ExtranonceError> {
    if (1..=Extranonce::MAX_WIDTH).contains(&width) {
        Ok(width as u8)
    } else {
        Err(ExtranonceError::InvalidWidth(width))
    }
}

/// Returns the largest counter value that fits in a slot of the given width.
fn max_counter(width: u8) -> u64 {
    match u32::from(width) * 8 {
        bits if bits >= u64::BITS => u64::MAX,
        bits => (1 << bits) - 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(ext: Extranonce) -> Vec<u8> {
        Vec::from(ext)
    }

    fn ext(bytes: &[u8]) -> Extranonce {
        Extranonce::from_bytes(bytes).unwrap()
    }

    /// Returns the first `count` extranonces from `first`.
    fn run(first: &[u8], count: u64) -> ExtranonceRange {
        let first = ext(first);
        ExtranonceRange {
            first: first.counter,
            last: first.counter + count - 1,
            width: first.width,
        }
    }

    /// The largest `usize`, as a counter value. Tests of the iterator's
    /// `usize` limits use it so that they hold on any pointer width.
    const USIZE_MAX: u64 = usize::MAX as u64;

    /// Returns the 8-byte extranonces from zero through counter `last`.
    fn through(last: u64) -> ExtranonceRange {
        ExtranonceRange {
            first: 0,
            last,
            width: 8,
        }
    }

    // Extranonce tests

    #[test]
    fn bytes_round_trip_at_each_width() {
        for width in [1, 4, 8, 9, 32] {
            let mut wire = vec![0u8; width];
            let last = wire.len() - 1;
            wire[last] = 0xab;
            if width > 1 {
                wire[width.saturating_sub(COUNTER_WIDTH)] = 0x12;
            }
            assert_eq!(bytes(ext(&wire)), wire, "width {width}");
        }
    }

    #[test]
    fn from_array_matches_from_bytes() {
        assert_eq!(Extranonce::from_array([0xab]), ext(&[0xab]));
        let wire = [1, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(Extranonce::from_array(wire), ext(&wire));
    }

    #[test]
    fn from_bytes_rejects_bad_widths() {
        assert_eq!(
            Extranonce::from_bytes(&[]),
            Err(ExtranonceError::InvalidWidth(0))
        );
        assert_eq!(
            Extranonce::from_bytes(&[0; 33]),
            Err(ExtranonceError::InvalidWidth(33))
        );
    }

    #[test]
    fn from_bytes_rejects_nonzero_bytes_left_of_counter() {
        let mut wire = [0u8; 9];
        wire[0] = 1;
        assert_eq!(
            Extranonce::from_bytes(&wire),
            Err(ExtranonceError::BeyondCounter)
        );
    }

    #[test]
    fn display_prints_wire_bytes_as_hex() {
        assert_eq!(ext(&[0, 0, 0x12, 0x34]).to_string(), "00001234");
        assert_eq!(ext(&[0xab]).to_string(), "ab");
        assert_eq!(ext(&[0; 8]).to_string(), "0000000000000000");
    }

    // ExtranonceRange tests

    #[test]
    fn full_range_counts_all_values_of_the_counter() {
        assert_eq!(ExtranonceRange::full(1).unwrap().len(), 256);
        assert_eq!(ExtranonceRange::full(4).unwrap().len(), 1 << 32);
        assert_eq!(ExtranonceRange::full(8).unwrap().len(), 1 << 64);
        assert_eq!(ExtranonceRange::full(32).unwrap().len(), 1 << 64);
    }

    #[test]
    fn full_rejects_bad_widths() {
        assert_eq!(
            ExtranonceRange::full(0),
            Err(ExtranonceError::InvalidWidth(0))
        );
        assert_eq!(
            ExtranonceRange::full(33),
            Err(ExtranonceError::InvalidWidth(33))
        );
    }

    #[test]
    fn wide_slot_rolls_the_counter_at_the_right() {
        let range = ExtranonceRange::full(32).unwrap();
        let second = range.iter().nth(1).unwrap();

        let mut expected = vec![0u8; 32];
        expected[31] = 1;
        assert_eq!(bytes(second), expected);
    }

    #[test]
    fn starting_at_runs_to_the_end_of_the_slot() {
        let range = ExtranonceRange::starting_at(ext(&[0xfd]));
        let values: Vec<_> = range.iter().map(bytes).collect();
        assert_eq!(values, vec![vec![0xfd], vec![0xfe], vec![0xff]]);
    }

    #[test]
    fn starting_at_runs_to_the_end_of_the_counter_in_a_wide_slot() {
        let range = ExtranonceRange::starting_at(ext(&[0; 32]));
        assert_eq!(range.len(), 1 << 64);
    }

    #[test]
    fn split_distributes_the_remainder_to_the_first_parts() {
        let range = run(&[0], 10);
        let lens: Vec<_> = range.split(3).unwrap().iter().map(|r| r.len()).collect();
        assert_eq!(lens, vec![4, 3, 3]);
    }

    #[test]
    fn split_refuses_more_parts_than_values() {
        let range = run(&[0], 2);
        assert!(range.split(0).is_none());
        assert!(range.split(3).is_none());
        assert_eq!(range.split(2).unwrap().len(), 2);
    }

    #[test]
    fn split_fixes_leading_bytes() {
        let range = ExtranonceRange::full(4).unwrap();
        let parts = range.split(4).unwrap();

        // Each part's values share their first byte, the way a Stratum V2
        // upstream gives each downstream a longer prefix.
        for (i, part) in parts.iter().enumerate() {
            let first = bytes(part.start());
            assert_eq!(first, vec![(i as u8) << 6, 0, 0, 0]);
        }
    }

    #[test]
    fn split_parts_cover_the_range_without_gap_or_overlap() {
        let range = run(&[0x10], 16);
        let whole: Vec<_> = range.iter().collect();
        for n in 1..=7 {
            let joined: Vec<_> = range
                .split(n)
                .unwrap()
                .iter()
                .flat_map(|r| r.iter())
                .collect();
            assert_eq!(joined, whole, "split into {n}");
        }
    }

    #[test]
    fn split_full_counter_range() {
        let range = ExtranonceRange::full(8).unwrap();
        let parts = range.split(3).unwrap();
        assert_eq!(parts.iter().map(|r| r.len()).sum::<u128>(), 1 << 64);
    }

    // ExtranonceIter tests

    #[test]
    fn iter_rolls_trailing_bytes() {
        let range = ExtranonceRange::full(4).unwrap();
        let values: Vec<_> = range.iter().skip(255).take(2).map(bytes).collect();
        assert_eq!(values, vec![vec![0, 0, 0, 0xff], vec![0, 0, 1, 0]]);
    }

    #[test]
    fn iter_stops_at_the_last_counter_value() {
        let mut wire = [0xff; 8];
        wire[7] = 0xfe;
        let range = ExtranonceRange::starting_at(ext(&wire));
        let mut iter = range.iter();

        assert_eq!(iter.next().map(bytes), Some(wire.to_vec()));
        assert_eq!(iter.next().map(bytes), Some(vec![0xff; 8]));
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn iter_size_hint_is_exact() {
        let range = run(&[0], 100);
        let mut iter = range.iter();
        iter.next();
        assert_eq!(iter.size_hint(), (99, Some(99)));
    }

    #[test]
    fn iter_size_hint_is_exact_up_to_usize() {
        let iter = through(USIZE_MAX - 1).iter();
        assert_eq!(iter.size_hint(), (usize::MAX, Some(usize::MAX)));
    }

    #[test]
    fn iter_size_hint_has_no_upper_bound_past_usize() {
        let iter = through(USIZE_MAX).iter();
        assert_eq!(iter.size_hint(), (usize::MAX, None));
    }
}
