//! Merkle root specification for mining jobs.

use anyhow::Result;
use bitcoin::Transaction;
use bitcoin::consensus::deserialize;
use bitcoin::hash_types::TxMerkleNode;
use bitcoin::hashes::{Hash, sha256d};

use super::{Extranonce, ExtranonceRange};

/// Specifies how to obtain the merkle root for a mining job.
///
/// The merkle root can either be provided directly as a fixed value or computed
/// dynamically from coinbase transaction parts. Miners typically roll the
/// extranonce to generate different merkle roots. In Stratum v2 simple
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
    /// to generate different merkle roots. Each unique extranonce value produces
    /// a different coinbase hash, requiring recomputation of the merkle tree.
    /// This is the standard mode for Stratum v1 pool mining.
    Computed(MerkleRootTemplate),
}

/// Template for computing merkle roots from coinbase transaction parts.
///
/// Contains all the components needed to build a coinbase transaction and compute
/// its merkle root. As the extranonce rolls, each unique value produces a different
/// coinbase transaction hash, which propagates up the merkle tree to produce a
/// different merkle root.
#[derive(Debug, Clone)]
pub struct MerkleRootTemplate {
    /// Coinbase transaction bytes before the extranonce prefix.
    pub coinbase_prefix: Vec<u8>,

    /// Extranonce bytes fixed by the upstream.
    ///
    /// The upstream assigns these once per connection or channel. They tend to
    /// stay the same for all jobs from a source.
    pub extranonce_prefix: Vec<u8>,

    /// Extranonce space available for rolling.
    ///
    /// The caller will create an iterator from this range to generate different
    /// extranonce values for unique block headers.
    pub extranonce_range: ExtranonceRange,

    /// Coinbase transaction bytes after the extranonce.
    pub coinbase_suffix: Vec<u8>,

    /// Merkle branches for building the merkle root.
    ///
    /// After hashing the coinbase transaction, these branches are used to climb
    /// the merkle tree to compute the final merkle root for the block header.
    pub merkle_branches: Vec<TxMerkleNode>,
}

impl MerkleRootTemplate {
    /// Compute merkle root for a specific extranonce value.
    ///
    /// Builds the complete coinbase transaction by concatenating parts with the
    /// given extranonce, computes its txid, then climbs the merkle tree using
    /// the branches to produce the final merkle root.
    ///
    /// This is a pure function - it doesn't modify the template. Callers manage
    /// extranonce iteration externally via `ExtranonceIter`.
    pub fn compute_merkle_root(&self, extranonce: &Extranonce) -> Result<TxMerkleNode> {
        // Build complete coinbase transaction
        let mut coinbase_bytes = Vec::new();
        coinbase_bytes.extend_from_slice(&self.coinbase_prefix);
        coinbase_bytes.extend_from_slice(&self.extranonce_prefix);
        extranonce.extend_vec(&mut coinbase_bytes);
        coinbase_bytes.extend_from_slice(&self.coinbase_suffix);

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
        let extranonce = block_881423::EXTRANONCE;

        // Construct a template from golden values
        let template = MerkleRootTemplate {
            coinbase_prefix: block_881423::coinbase_prefix_bytes().to_vec(),
            extranonce_prefix: block_881423::extranonce_prefix_bytes().to_vec(),
            extranonce_range: ExtranonceRange::full(extranonce.width()).unwrap(),
            coinbase_suffix: block_881423::coinbase_suffix_bytes().to_vec(),
            merkle_branches: block_881423::MERKLE_BRANCHES.clone(),
        };

        // Compute merkle root
        let computed_merkle_root = template
            .compute_merkle_root(&extranonce)
            .expect("merkle root computation should succeed");

        // Verify it matches the known merkle root
        assert_eq!(
            computed_merkle_root,
            *block_881423::MERKLE_ROOT,
            "Computed merkle root doesn't match block 881,423"
        );
    }
}
