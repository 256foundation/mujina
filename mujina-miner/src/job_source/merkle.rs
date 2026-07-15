//! Merkle root specification for mining jobs.

use anyhow::Result;
use bitcoin::Transaction;
use bitcoin::consensus::deserialize;
use bitcoin::hash_types::TxMerkleNode;
use bitcoin::hashes::{Hash, sha256d};

use super::{Extranonce2, Extranonce2Range};

/// Specifies how to obtain the merkle root for a mining job.
///
/// The merkle root can either be provided directly as a fixed value or computed
/// dynamically from coinbase transaction parts. In Stratum v1, miners typically
/// roll extranonce2 to generate different merkle roots. In Stratum v2 simple
/// mode, the merkle root is fixed and only the nonce is rolled.
#[derive(Debug, Clone)]
pub enum MerkleRootKind {
    /// Pre-computed merkle root that never changes.
    ///
    /// Used for test jobs, dummy work, Stratum v2 simple mode, or any scenario
    /// where the coinbase transaction is already finalized and won't be modified
    /// by the miner.
    Fixed(TxMerkleNode),

    /// Merkle root must be computed from coinbase parts.
    ///
    /// The coinbase transaction contains extranonce fields that can be modified
    /// to generate different merkle roots. Each unique extranonce2 value produces
    /// a different coinbase hash, requiring recomputation of the merkle tree.
    /// This is the standard mode for Stratum v1 pool mining.
    Computed(MerkleRootTemplate),
}

/// Template for computing merkle roots from coinbase transaction parts.
///
/// Contains all the components needed to build a coinbase transaction and compute
/// its merkle root. As extranonce2 is rolled, each unique value produces a different
/// coinbase transaction hash, which propagates up the merkle tree to produce a
/// different merkle root.
#[derive(Debug, Clone)]
pub struct MerkleRootTemplate {
    /// First part of coinbase transaction (before extranonces).
    pub coinbase1: Vec<u8>,

    /// Extranonce1 value assigned by the source.
    ///
    /// This is set once per connection and tends to remain constant for all jobs from
    /// this source.
    pub extranonce1: Vec<u8>,

    /// Extranonce2 range defining the available rolling space.
    ///
    /// The caller will create an iterator from this range to generate different
    /// extranonce2 values for unique block headers. The range's `size` field is
    /// the counter width (1–8 bytes, capped by `u64`).
    pub extranonce2_range: Extranonce2Range,

    /// Full extranonce2 allocation in bytes (≥ `extranonce2_range.size`).
    /// Full extranonce2 byte length as negotiated with the pool (SV2 spec's
    /// `extranonce_size`). When this exceeds the 8-byte u64 counter width,
    /// the remaining bytes are zero-padded in both coinbase insertion and
    /// `SubmitSharesExtended.extranonce`. For SV1/non-SV2 sources this always
    /// equals `extranonce2_range.size`.
    pub extranonce2_size: u8,

    /// Second part of coinbase transaction (after extranonces).
    pub coinbase2: Vec<u8>,

    /// Merkle branches for building the merkle root.
    ///
    /// After hashing the coinbase transaction, these branches are used to climb
    /// the merkle tree to compute the final merkle root for the block header.
    pub merkle_branches: Vec<TxMerkleNode>,
}

impl MerkleRootTemplate {
    /// Compute merkle root for a specific extranonce2 value.
    ///
    /// Builds the complete coinbase transaction by concatenating parts with the
    /// given extranonce2, computes its txid, then climbs the merkle tree using
    /// the branches to produce the final merkle root.
    ///
    /// This is a pure function - it doesn't modify the template. Callers manage
    /// extranonce2 iteration externally via `Extranonce2Iter`.
    pub fn compute_merkle_root(&self, extranonce2: &Extranonce2) -> Result<TxMerkleNode> {
        // Build complete coinbase transaction
        let mut coinbase_bytes = Vec::new();
        coinbase_bytes.extend_from_slice(&self.coinbase1);
        coinbase_bytes.extend_from_slice(&self.extranonce1);
        // Write counter bytes then zero-pad to the pool's full extranonce2_size.
        // Per the SV2 spec, SubmitSharesExtended.extranonce MUST fill exactly
        // `extranonce_size` bytes; when the pool allocates more than 8 bytes the
        // u64 counter covers the first 1–8 bytes and the remainder is zeroed.
        let mut en2_bytes = Vec::new();
        extranonce2.extend_vec(&mut en2_bytes);
        en2_bytes.resize(self.extranonce2_size as usize, 0);
        coinbase_bytes.extend_from_slice(&en2_bytes);
        coinbase_bytes.extend_from_slice(&self.coinbase2);

        // Parse and compute coinbase txid
        let coinbase_tx: Transaction = deserialize(&coinbase_bytes)?;
        let mut current_hash = coinbase_tx.compute_txid().to_byte_array();

        // Climb the merkle tree
        for branch in &self.merkle_branches {
            let mut combined = Vec::new();
            combined.extend_from_slice(&current_hash);
            combined.extend_from_slice(branch.as_byte_array());

            let parent_hash = sha256d::Hash::hash(&combined);
            current_hash = parent_hash.to_byte_array();
        }

        Ok(TxMerkleNode::from_byte_array(current_hash))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job_source::test_blocks::block_881423;

    #[test]
    fn test_compute_merkle_root_with_block_881423() {
        let extranonce2 = *block_881423::EXTRANONCE2;

        // Construct a template from golden values
        let en2_size = extranonce2.size();
        let template = MerkleRootTemplate {
            coinbase1: block_881423::coinbase1_bytes().to_vec(),
            extranonce1: block_881423::extranonce1_bytes().to_vec(),
            extranonce2_range: Extranonce2Range::new(en2_size).unwrap(),
            extranonce2_size: en2_size,
            coinbase2: block_881423::coinbase2_bytes().to_vec(),
            merkle_branches: block_881423::MERKLE_BRANCHES.clone(),
        };

        // Compute merkle root
        let computed_merkle_root = template
            .compute_merkle_root(&extranonce2)
            .expect("merkle root computation should succeed");

        // Verify it matches the known merkle root
        assert_eq!(
            computed_merkle_root,
            *block_881423::MERKLE_ROOT,
            "Computed merkle root doesn't match block 881,423"
        );
    }
}
