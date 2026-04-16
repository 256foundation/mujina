//! Core types for mujina-miner.
//!
//! This module provides a unified location for type definitions used throughout
//! the miner. It re-exports commonly used types from rust-bitcoin and defines
//! mining-specific types.

mod bitcoin_impls;
mod debounced_alarm;
mod difficulty;
mod hash_rate;
mod hashrate_estimator;
mod share_rate;
mod temperature;

use std::time::Duration;

// Re-export frequently used bitcoin types for convenience
pub use bitcoin::block::Header as BlockHeader;
pub use bitcoin::{Amount, BlockHash, Network, Target, Transaction, TxOut, Work};
pub use debounced_alarm::{AlarmStatus, DebouncedAlarm};
pub use difficulty::Difficulty;
pub use hash_rate::HashRate;
pub use hashrate_estimator::HashrateEstimator;
pub use share_rate::ShareRate;
pub use temperature::Temperature;

/// Calculate expected time between shares at given difficulty and hashrate.
///
/// Returns the average time to find a share. Actual time varies due to
/// randomness in hash mining.
pub fn expected_time_to_share(difficulty: Difficulty, hashrate: HashRate) -> Duration {
    let hashes_per_share = difficulty.as_f64() * (u32::MAX as f64 + 1.0);
    let shares_per_sec = f64::from(hashrate) / hashes_per_share;
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_expected_time_to_share_extreme_hashrates() {
        // Very low hashrate (CPU miner: 1 MH/s)
        let diff = Difficulty::from(256);
        let hashrate = HashRate::from_megahashes(1.0);
        let time_to_share = expected_time_to_share(diff, hashrate);
        // Should be over 1 million seconds
        assert!(time_to_share.as_secs() > 1_000_000);

        // Very high hashrate (datacenter: 100 TH/s)
        let diff = Difficulty::from(100_000);
        let hashrate = HashRate::from_terahashes(100.0);
        let time_to_share = expected_time_to_share(diff, hashrate);
        // ~4.3 seconds between shares
        assert!((time_to_share.as_secs_f64() - 4.3).abs() < 0.1);
    }

    #[test]
    fn test_target_for_share_rate() {
        // 1 TH/s with 6 shares/min (10 second interval) = ~2328 difficulty
        let hashrate = HashRate::from_terahashes(1.0);
        let rate = ShareRate::per_minute(6.0);
        let target = rate.to_target(hashrate);
        // 1e12 * 10 / 2^32 ≈ 2328 difficulty
        let diff = Difficulty::from_target(target);
        assert!((diff.as_u64() as i64 - 2328).abs() < 10);

        // Round-trip: target -> rate -> target should be close
        let original = Difficulty::from(1024).to_target();
        let hashrate = HashRate::from_gigahashes(1.0);
        let interval = expected_time_to_share_from_target(original, hashrate);
        let rate = ShareRate::from_interval(interval);
        let recovered = rate.to_target(hashrate);
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
