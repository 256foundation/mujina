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
//! Values print as hex in wire order. Slots of up to 8 bytes print in full
//! (`00001234`), so a log line matches the hex a pool logs, and the number
//! of digits gives the width. Wider slots replace the longest run of zero
//! bytes with `::`, as IPv6 addresses do, and append the width in bytes
//! (`::1234/32`). A range prints its first value and its length, in the
//! shape of a Rust struct (`{ start: 40000000, len: 2^30 }`).

use std::fmt;
use std::iter::{self, FusedIterator};

use thiserror::Error;

/// Width in bytes of the counter at the right end of the slot.
const COUNTER_WIDTH: usize = size_of::<u64>();

/// The bytes the miner fills in one extranonce slot.
///
/// An immutable value that goes into a coinbase transaction or a share.
/// Obtain one from an `ExtranonceRange`, from bytes with `from_bytes()`, or
/// from a fixed-size array with `from_array()`.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
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
        let bytes = Vec::from(*self);
        if self.width() <= COUNTER_WIDTH {
            write_bytes(f, &bytes)
        } else {
            write_compressed(f, &bytes)?;
            write!(f, "/{}", self.width)
        }
    }
}

impl fmt::Debug for Extranonce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Extranonce({self})")
    }
}

/// A contiguous run of extranonces in one slot.
///
/// Ranges are immutable. Obtain the full range of a slot with `full()`, or
/// the rest of a slot from a known extranonce with `starting_at()`, then
/// divide it with `split()`. Call `iter()` to roll through the extranonces
/// in a range.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
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

impl fmt::Display for ExtranonceRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{{ start: {}, len: ", self.start())?;
        write_count(f, self.len())?;
        f.write_str(" }")
    }
}

impl fmt::Debug for ExtranonceRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ExtranonceRange {self}")
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

    /// Counts the extranonces left. Deprecated, because a range can hold
    /// more extranonces than `usize` can count.
    ///
    /// `Iterator` gives all iterators `count()`, and an impl cannot remove or
    /// deprecate it. This inherent method hides it in a direct call, so that
    /// the call warns. Generic code and adapters still reach the trait's
    /// version.
    ///
    /// # Panics
    ///
    /// Panics if more than `usize::MAX` extranonces remain, as
    /// `RangeInclusive::count()` does. A range can hold up to 2^64
    /// extranonces, more than `usize` can count on any target.
    #[deprecated(note = "a range can hold more extranonces than `usize` can count")]
    pub fn count(self) -> usize {
        Iterator::count(self)
    }
}

// The methods past `next()` answer from the bounds, because the defaults
// step through the range one value at a time, and a range can hold 2^64.
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

    fn nth(&mut self, n: usize) -> Option<Self::Item> {
        let next = self.next?;
        match next.checked_add(n as u64).filter(|&c| c <= self.last) {
            Some(counter) => {
                self.next = Some(counter);
                self.next()
            }
            None => {
                self.next = None;
                None
            }
        }
    }

    /// Returns the number of extranonces left, without stepping through
    /// them.
    ///
    /// A direct call reaches the deprecated inherent `count()` instead.
    /// Generic code, adapters, and `Iterator::count(iter)` reach this one.
    /// It replaces the default, which steps through the values one at a time
    /// and, on a large range, would never return.
    ///
    /// # Panics
    ///
    /// Panics if more than `usize::MAX` extranonces remain, as
    /// `RangeInclusive::count()` does.
    fn count(self) -> usize {
        usize::try_from(self.remaining()).expect("count overflowed usize")
    }

    fn last(self) -> Option<Self::Item> {
        self.next.map(|_| self.extranonce(self.last))
    }
}

impl DoubleEndedIterator for ExtranonceIter {
    fn next_back(&mut self) -> Option<Self::Item> {
        let next = self.next?;
        let counter = self.last;
        if counter == next {
            self.next = None;
        } else {
            self.last -= 1;
        }
        Some(self.extranonce(counter))
    }

    fn nth_back(&mut self, n: usize) -> Option<Self::Item> {
        let next = self.next?;
        match self.last.checked_sub(n as u64).filter(|&c| c >= next) {
            Some(counter) => {
                self.last = counter;
                self.next_back()
            }
            None => {
                self.next = None;
                None
            }
        }
    }
}

impl FusedIterator for ExtranonceIter {}

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

/// Writes a count of extranonces, as a power of two when it is a large one.
///
/// Splitting a slot evenly gives powers of two, and 2^30 reads more easily
/// than 1073741824.
fn write_count(f: &mut fmt::Formatter<'_>, count: u128) -> fmt::Result {
    if count.is_power_of_two() && count >= 1 << 10 {
        write!(f, "2^{}", count.trailing_zeros())
    } else {
        write!(f, "{count}")
    }
}

/// Writes bytes as hex with the longest run of two or more zero bytes
/// replaced by `::`, choosing the leftmost run on a tie, as RFC 5952 does for
/// IPv6 addresses.
fn write_compressed(f: &mut fmt::Formatter<'_>, bytes: &[u8]) -> fmt::Result {
    let mut best = 0..0;
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        while i < bytes.len() && bytes[i] == 0 {
            i += 1;
        }
        if i - start > best.len() {
            best = start..i;
        }
        i += 1;
    }

    if best.len() < 2 {
        return write_bytes(f, bytes);
    }
    write_bytes(f, &bytes[..best.start])?;
    f.write_str("::")?;
    write_bytes(f, &bytes[best.end..])
}

fn write_bytes(f: &mut fmt::Formatter<'_>, bytes: &[u8]) -> fmt::Result {
    bytes.iter().try_for_each(|b| write!(f, "{b:02x}"))
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
    fn display_prints_short_slots_in_full() {
        assert_eq!(ext(&[0, 0, 0x12, 0x34]).to_string(), "00001234");
        assert_eq!(ext(&[0xab]).to_string(), "ab");
        assert_eq!(ext(&[0; 8]).to_string(), "0000000000000000");
    }

    #[test]
    fn display_compresses_wide_slots_and_shows_width() {
        let mut wire = [0u8; 32];
        wire[30..].copy_from_slice(&[0x12, 0x34]);
        assert_eq!(ext(&wire).to_string(), "::1234/32");
        assert_eq!(ext(&[0; 32]).to_string(), "::/32");
    }

    #[test]
    fn compression_takes_the_longest_run_of_zeros() {
        let wire = [0, 0, 0x12, 0, 0, 0, 0x34, 0, 0];
        assert_eq!(ext(&wire).to_string(), "000012::340000/9");
    }

    #[test]
    fn compression_takes_the_leftmost_of_equal_runs() {
        let wire = [0, 0, 1, 0, 0, 2, 3, 4, 5];
        assert_eq!(ext(&wire).to_string(), "::01000002030405/9");
    }

    #[test]
    fn compression_skips_a_lone_zero_byte() {
        let wire = [0, 1, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(ext(&wire).to_string(), "000102030405060708/9");
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

    #[test]
    fn range_display_gives_start_and_len() {
        let parts = ExtranonceRange::full(4).unwrap().split(4).unwrap();
        assert_eq!(parts[1].to_string(), "{ start: 40000000, len: 2^30 }");

        let parts = ExtranonceRange::full(4).unwrap().split(3).unwrap();
        assert_eq!(parts[0].to_string(), "{ start: 00000000, len: 1431655766 }");
    }

    #[test]
    fn range_display_writes_small_lengths_in_decimal() {
        let range = run(&[0x22, 0x0c, 0xf1, 0xad], 101);
        assert_eq!(range.to_string(), "{ start: 220cf1ad, len: 101 }");

        let range = run(&[0, 0], 512);
        assert_eq!(range.to_string(), "{ start: 0000, len: 512 }");
    }

    #[test]
    fn range_display_shows_width_of_wide_slots() {
        let range = ExtranonceRange::full(32).unwrap();
        assert_eq!(range.to_string(), "{ start: ::/32, len: 2^64 }");
    }

    #[test]
    fn debug_names_the_type() {
        assert_eq!(format!("{:?}", ext(&[0xab])), "Extranonce(ab)");
        let range = ExtranonceRange::full(1).unwrap();
        assert_eq!(
            format!("{range:?}"),
            "ExtranonceRange { start: 00, len: 256 }"
        );
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

    #[test]
    fn iter_nth_jumps_without_walking() {
        let mut iter = through(USIZE_MAX).iter();
        let last = USIZE_MAX.to_be_bytes().to_vec();
        assert_eq!(iter.nth(usize::MAX).map(bytes), Some(last));
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn iter_nth_past_the_end_exhausts() {
        let mut iter = run(&[0], 3).iter();
        assert_eq!(iter.nth(3), None);
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn iter_nth_back_jumps_without_walking() {
        let mut iter = through(USIZE_MAX).iter();
        assert_eq!(iter.nth_back(usize::MAX).map(bytes), Some(vec![0; 8]));
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn iter_nth_back_stops_at_the_front() {
        let mut iter = run(&[0], 3).iter();
        iter.next();
        assert_eq!(iter.nth_back(1).map(bytes), Some(vec![1]));
        assert_eq!(iter.next(), None);

        let mut iter = run(&[0], 3).iter();
        assert_eq!(iter.nth_back(3), None);
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn iter_count_answers_from_the_bounds() {
        let iter = through(USIZE_MAX - 1).iter();
        assert_eq!(Iterator::count(iter), usize::MAX);

        let mut iter = run(&[0], 10).iter();
        iter.next();
        iter.next_back();
        assert_eq!(Iterator::count(iter), 8);
    }

    #[test]
    #[should_panic(expected = "count overflowed usize")]
    fn iter_count_past_usize_panics() {
        Iterator::count(through(USIZE_MAX).iter());
    }

    #[test]
    fn iter_last_of_a_full_counter() {
        let iter = ExtranonceRange::full(8).unwrap().iter();
        assert_eq!(iter.last().map(bytes), Some(vec![0xff; 8]));
    }

    #[test]
    fn iter_runs_from_both_ends() {
        let mut iter = run(&[0], 3).iter();
        assert_eq!(iter.next_back().map(bytes), Some(vec![2]));
        assert_eq!(iter.next().map(bytes), Some(vec![0]));
        assert_eq!(iter.next_back().map(bytes), Some(vec![1]));
        assert_eq!(iter.next(), None);
        assert_eq!(iter.next_back(), None);
    }

    #[test]
    fn iter_rev_starts_at_the_end_of_the_slot() {
        let range = ExtranonceRange::full(1).unwrap();
        let values: Vec<_> = range.iter().rev().take(2).map(bytes).collect();
        assert_eq!(values, vec![vec![0xff], vec![0xfe]]);
    }
}
