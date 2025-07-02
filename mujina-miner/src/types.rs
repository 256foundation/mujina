//! Core types for mujina-miner.
//!
//! This module provides a unified location for type definitions used throughout
//! the miner. It re-exports commonly used types from rust-bitcoin and defines
//! mining-specific types.

// Re-export frequently used bitcoin types for convenience
pub use bitcoin::block::Header as BlockHeader;
pub use bitcoin::{Amount, BlockHash, Network, Target, Transaction, TxOut, Work};

use bitcoin::hashes::sha256d;

/// A mining job sent to ASIC chips.
#[derive(Debug, Clone)]
pub struct Job {
    /// The block header to mine
    pub header: BlockHeader,
    /// Unique identifier for this job
    pub job_id: u64,
    /// Current merkle root
    pub merkle_root: sha256d::Hash,
    /// Encoded difficulty target
    pub nbits: u32,
    /// Time offset for rolling
    pub ntime_offset: u32,
}

/// A share (valid nonce) found by an ASIC chip.
#[derive(Debug, Clone)]
pub struct Share {
    /// Job this share is for
    pub job_id: u64,
    /// The winning nonce
    pub nonce: u32,
    /// Timestamp when found
    pub ntime: u32,
    /// Which chip found it
    pub chip_id: u8,
}

/// Hashrate measurement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HashRate(pub u64); // hashes per second

impl HashRate {
    /// Create from megahashes per second
    pub fn from_megahashes(mh: f64) -> Self {
        Self((mh * 1_000_000.0) as u64)
    }

    /// Create from gigahashes per second
    pub fn from_gigahashes(gh: f64) -> Self {
        Self((gh * 1_000_000_000.0) as u64)
    }

    /// Create from terahashes per second
    pub fn from_terahashes(th: f64) -> Self {
        Self((th * 1_000_000_000_000.0) as u64)
    }

    /// Get value as megahashes per second
    pub fn as_megahashes(&self) -> f64 {
        self.0 as f64 / 1_000_000.0
    }

    /// Get value as gigahashes per second
    pub fn as_gigahashes(&self) -> f64 {
        self.0 as f64 / 1_000_000_000.0
    }

    /// Get value as terahashes per second
    pub fn as_terahashes(&self) -> f64 {
        self.0 as f64 / 1_000_000_000_000.0
    }

    /// Format as human-readable string with appropriate units
    pub fn to_human_readable(&self) -> String {
        if self.0 >= 1_000_000_000_000 {
            format!("{:.2} TH/s", self.as_terahashes())
        } else if self.0 >= 1_000_000_000 {
            format!("{:.2} GH/s", self.as_gigahashes())
        } else if self.0 >= 1_000_000 {
            format!("{:.2} MH/s", self.as_megahashes())
        } else {
            format!("{} H/s", self.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hashrate_conversions() {
        let rate = HashRate::from_terahashes(100.0);
        assert_eq!(rate.as_terahashes(), 100.0);
        assert_eq!(rate.as_gigahashes(), 100_000.0);
        assert_eq!(rate.to_human_readable(), "100.00 TH/s");

        let rate = HashRate::from_gigahashes(500.0);
        assert_eq!(rate.as_gigahashes(), 500.0);
        assert_eq!(rate.to_human_readable(), "500.00 GH/s");
    }
}
