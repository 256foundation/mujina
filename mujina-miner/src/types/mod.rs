//! Core types for mujina-miner.
//!
//! This module provides a unified location for type definitions used throughout
//! the miner. It re-exports commonly used types from rust-bitcoin and defines
//! mining-specific types.

mod bitcoin_impls;
mod difficulty;
mod hash_rate;
mod share_rate;

use std::time::Duration;

use crate::u256::U256;

// Re-export frequently used bitcoin types for convenience
pub use bitcoin::block::Header as BlockHeader;
pub use bitcoin::{Amount, BlockHash, Network, Target, Transaction, TxOut, Work};
pub use difficulty::Difficulty;
pub use hash_rate::HashRate;
pub use share_rate::ShareRate;

/// Calculate expected shares per second at given difficulty and hashrate.
///
/// Formula: shares_per_sec = hashrate / (difficulty * 2^32)
///
/// This represents the statistical average; actual share arrival follows
/// a Poisson distribution.
pub fn expected_shares_per_second(difficulty: Difficulty, hashrate: HashRate) -> f64 {
    let hashes_per_share = difficulty.as_f64() * (u32::MAX as f64 + 1.0);
    f64::from(hashrate) / hashes_per_share
}

/// Calculate expected time between shares at given difficulty and hashrate.
///
/// Returns the average time to find a share (1 / shares_per_sec).
/// Actual time varies due to randomness in hash mining.
pub fn expected_time_to_share(difficulty: Difficulty, hashrate: HashRate) -> Duration {
    let shares_per_sec = expected_shares_per_second(difficulty, hashrate);
    if shares_per_sec <= 0.0 {
        return Duration::MAX;
    }
    Duration::from_secs_f64(1.0 / shares_per_sec)
}

/// Calculate expected time between shares from a Target and hashrate.
///
/// Uses rust-bitcoin's `Target::difficulty_float()` directly, avoiding
/// intermediate Difficulty conversion.
pub fn expected_time_to_share_from_target(target: Target, hashrate: HashRate) -> Duration {
    if hashrate.0 == 0 {
        return Duration::MAX;
    }
    let difficulty_float = target.difficulty_float();
    let hashes_per_share = difficulty_float * (u32::MAX as f64 + 1.0);
    let shares_per_sec = hashrate.0 as f64 / hashes_per_share;
    if shares_per_sec <= 0.0 {
        return Duration::MAX;
    }
    Duration::from_secs_f64(1.0 / shares_per_sec)
}

/// Calculate target to achieve the given share rate at the given hashrate.
///
/// A hash is valid if hash < target. With hashes uniform over [0, 2^256),
/// expected hashes per share = 2^256 / target, so target = 2^256 / hashes_per_share.
pub fn target_for_share_rate(rate: ShareRate, hashrate: HashRate) -> Target {
    let hashes_per_share = hashrate.hashes_in(rate.as_interval());
    if hashes_per_share <= 1 {
        return Target::MAX;
    }

    Target::from(U256::MAX / hashes_per_share)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expected_shares_per_second() {
        // At 1 GH/s with difficulty 1024, expect roughly one share every ~4398 seconds
        let diff = Difficulty::from(1024);
        let hashrate = HashRate::from_gigahashes(1.0);

        let shares_per_sec = expected_shares_per_second(diff, hashrate);
        // 1 GH/s = 1e9 H/s, difficulty 1024 = 1024 * 2^32 hashes per share
        // shares/sec = 1e9 / (1024 * 4294967296) ≈ 0.000227 shares/sec
        assert!((shares_per_sec - 0.000227).abs() < 0.000001);
    }

    #[test]
    fn test_expected_time_to_share() {
        // At 1 GH/s with difficulty 1024
        let diff = Difficulty::from(1024);
        let hashrate = HashRate::from_gigahashes(1.0);

        let time_to_share = expected_time_to_share(diff, hashrate);
        // Should be ~4398 seconds (over an hour)
        assert!((time_to_share.as_secs_f64() - 4398.0).abs() < 1.0);
    }

    #[test]
    fn test_share_calculations_extreme_hashrates() {
        // Very low hashrate (CPU miner: 1 MH/s)
        let diff = Difficulty::from(256);
        let hashrate = HashRate::from_megahashes(1.0);
        let time_to_share = expected_time_to_share(diff, hashrate);
        // Should be over 1 million seconds
        assert!(time_to_share.as_secs() > 1_000_000);

        // Very high hashrate (datacenter: 100 TH/s)
        let diff = Difficulty::from(100_000);
        let hashrate = HashRate::from_terahashes(100.0);
        let shares_per_sec = expected_shares_per_second(diff, hashrate);
        // Should be roughly 0.23 shares per second
        assert!((shares_per_sec - 0.233).abs() < 0.01);
    }

    #[test]
    fn test_target_for_share_rate() {
        // 1 TH/s with 6 shares/min (10 second interval) = ~2328 difficulty
        let hashrate = HashRate::from_terahashes(1.0);
        let rate = ShareRate::per_minute(6.0);
        let target = target_for_share_rate(rate, hashrate);
        // 1e12 * 10 / 2^32 ≈ 2328 difficulty
        let diff = Difficulty::from_target(target);
        assert!((diff.as_u64() as i64 - 2328).abs() < 10);

        // Round-trip: target -> rate -> target should be close
        let original = Difficulty::from(1024).to_target();
        let hashrate = HashRate::from_gigahashes(1.0);
        let interval = expected_time_to_share_from_target(original, hashrate);
        let rate = ShareRate::from_interval(interval);
        let recovered = target_for_share_rate(rate, hashrate);
        // Compare via difficulty since target comparison is awkward
        let recovered_diff = Difficulty::from_target(recovered);
        assert!((recovered_diff.as_u64() as i64 - 1024).abs() < 2);
    }

    #[test]
    fn test_expected_time_to_share_from_target() {
        // Should match expected_time_to_share when using equivalent difficulty
        let difficulty = Difficulty::from(1024);
        let target = difficulty.to_target();
        let hashrate = HashRate::from_terahashes(1.0);

        let time_from_difficulty = expected_time_to_share(difficulty, hashrate);
        let time_from_target = expected_time_to_share_from_target(target, hashrate);

        // Should be very close (small floating point differences allowed)
        let diff_secs = (time_from_difficulty.as_secs_f64() - time_from_target.as_secs_f64()).abs();
        assert!(diff_secs < 1.0, "Times differ by {} seconds", diff_secs);

        // Zero hashrate should return Duration::MAX
        let zero_hashrate = HashRate(0);
        assert_eq!(
            expected_time_to_share_from_target(target, zero_hashrate),
            Duration::MAX
        );
    }
}
