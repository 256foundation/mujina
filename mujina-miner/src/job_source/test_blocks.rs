//! Test data from real Bitcoin blocks.
//!
//! This module provides well-known Bitcoin blocks testing, with all their
//! components available in various formats.

/// Block 881,423
///
/// On January 30, 2025, the 256 Foundation held its first "Telehash" fundraiser,
/// directing over 1.12 EH/s from approximately 350 participants---mostly home and
/// small-scale miners aligned with the Foundation's mission to make Bitcoin
/// mining accessible through open-source tools---to a self-hosted mining pool.
/// During the live-streamed event, against incredible odds, they actually solved
/// this block, raising 3.146 BTC (~$330,300 at the time) to bootstrap several
/// open-source Bitcoin mining projects including Mujina.
///
/// This block serves as our primary test data for validating mining job generation
/// and header construction.
pub mod block_881423 {
    use std::str::FromStr;
    use std::sync::LazyLock;

    use bitcoin::block::{Header as BlockHeader, Version};
    use bitcoin::hash_types::{BlockHash, TxMerkleNode};
    use bitcoin::hashes::Hash;
    use bitcoin::pow::CompactTarget;

    use crate::job_source::Extranonce2;

    // Ground truth field values: These typed constants represent the authoritative
    // field values from block 881,423, parsed from the blockchain. They provide
    // the same data as HEADER_BYTES below, but in rust-bitcoin types.
    //
    // LazyLock is used for rust-bitcoin types because their constructors
    // (from_consensus, from_byte_array, etc.) are not const fn and cannot be
    // evaluated at compile time. LazyLock provides thread-safe lazy
    // initialization on first access.

    /// Block version field
    pub static VERSION: LazyLock<Version> =
        LazyLock::new(|| Version::from_consensus(0x2e596000_u32 as i32));

    /// Previous block hash
    pub static PREV_BLOCKHASH: LazyLock<BlockHash> = LazyLock::new(|| {
        BlockHash::from_byte_array([
            0xe3, 0xc9, 0x6a, 0x8a, 0x70, 0xca, 0x15, 0xfe, 0x21, 0x5e, 0x99, 0xf8, 0x4a, 0xb6,
            0x4c, 0x43, 0xe1, 0xb5, 0xd2, 0xc4, 0x3b, 0x54, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ])
    });

    /// Merkle root
    pub static MERKLE_ROOT: LazyLock<TxMerkleNode> = LazyLock::new(|| {
        TxMerkleNode::from_byte_array([
            0x76, 0xf6, 0x3a, 0x35, 0xf1, 0xb8, 0xb5, 0x01, 0x6d, 0x3e, 0xb0, 0xcc, 0xe2, 0xee,
            0xbb, 0xdf, 0x58, 0xbf, 0x8f, 0xbe, 0xa4, 0xe8, 0x70, 0xc8, 0xf7, 0x70, 0x34, 0x6b,
            0xdf, 0xcf, 0x62, 0x2d,
        ])
    });

    /// Block timestamp
    pub const TIME: u32 = 0x679ac169;

    /// Difficulty target (nBits field)
    pub static BITS: LazyLock<CompactTarget> =
        LazyLock::new(|| CompactTarget::from_consensus(0x17029a8a));

    /// Block nonce
    pub const NONCE: u32 = 0xff05fb02;

    // Ground truth wire format: the complete 80-byte header as it appears in the
    // blockchain. The typed field constants above are parsed from these bytes.
    // Consistency tests ensure both representations stay synchronized.

    /// The complete 80-byte block header in binary form.
    pub const HEADER_BYTES: [u8; 80] = [
        // Version (all fields little-endian)
        0x00, 0x60, 0x59, 0x2e, // Previous block hash
        0xe3, 0xc9, 0x6a, 0x8a, 0x70, 0xca, 0x15, 0xfe, 0x21, 0x5e, 0x99, 0xf8, 0x4a, 0xb6, 0x4c,
        0x43, 0xe1, 0xb5, 0xd2, 0xc4, 0x3b, 0x54, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, // Merkle root
        0x76, 0xf6, 0x3a, 0x35, 0xf1, 0xb8, 0xb5, 0x01, 0x6d, 0x3e, 0xb0, 0xcc, 0xe2, 0xee, 0xbb,
        0xdf, 0x58, 0xbf, 0x8f, 0xbe, 0xa4, 0xe8, 0x70, 0xc8, 0xf7, 0x70, 0x34, 0x6b, 0xdf, 0xcf,
        0x62, 0x2d, // Timestamp
        0x69, 0xc1, 0x9a, 0x67, // Bits
        0x8a, 0x9a, 0x02, 0x17, // Nonce
        0x02, 0xfb, 0x05, 0xff,
    ];

    // Ground truth block hash from the blockchain. Tests verify that both
    // HEADER_BYTES and the typed field constants produce this hash.

    /// Block hash
    pub static BLOCK_HASH: LazyLock<BlockHash> = LazyLock::new(|| {
        BlockHash::from_str("0000000000000000000269d52c24ea451225613aab095d90d771d4e29aa96cdd")
            .unwrap()
    });

    /// Complete block header constructed from ground truth field values
    pub static HEADER: LazyLock<BlockHeader> = LazyLock::new(|| BlockHeader {
        version: *VERSION,
        prev_blockhash: *PREV_BLOCKHASH,
        merkle_root: *MERKLE_ROOT,
        time: TIME,
        bits: *BITS,
        nonce: NONCE,
    });

    /// Get version bytes
    pub fn version_bytes() -> &'static [u8] {
        &HEADER_BYTES[0..4]
    }

    /// Get previous block hash bytes
    pub fn prev_hash_bytes() -> &'static [u8] {
        &HEADER_BYTES[4..36]
    }

    /// Get merkle root bytes
    pub fn merkle_root_bytes() -> &'static [u8] {
        &HEADER_BYTES[36..68]
    }

    /// Get timestamp bytes
    pub fn time_bytes() -> &'static [u8] {
        &HEADER_BYTES[68..72]
    }

    /// Get bits bytes
    pub fn bits_bytes() -> &'static [u8] {
        &HEADER_BYTES[72..76]
    }

    /// Get nonce bytes
    pub fn nonce_bytes() -> &'static [u8] {
        &HEADER_BYTES[76..80]
    }

    // Coinbase transaction and Stratum mining job components extracted from the actual
    // block as mined. The block contained 1,514 transactions.

    /// Complete coinbase transaction as it appears in the block (SegWit format)
    ///
    /// This is the ground truth coinbase transaction. It should be reconstructible
    /// from the Stratum components (COINBASE1 + extranonces + COINBASE2).
    ///
    /// For merkle tree computation, use Transaction::compute_txid() which internally
    /// hashes the legacy format (without witness data). For the witness commitment,
    /// use Transaction::compute_wtxid() which hashes this full SegWit format.
    pub const COINBASE_TX: &[u8] = &[
        0x01, 0x00, 0x00, 0x00, 0x00, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0x3d, 0x03,
        0x0f, 0x73, 0x0d, 0x00, 0x04, 0x69, 0xc1, 0x9a, 0x67, 0x04, 0x83, 0x0c, 0xee, 0x22, 0x0c,
        0xf1, 0xad, 0x9a, 0x67, 0xb6, 0x60, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0a, 0x41, 0x70,
        0x6f, 0x6c, 0x6c, 0x6f, 0x19, 0x2f, 0x6d, 0x69, 0x6e, 0x65, 0x64, 0x20, 0x62, 0x79, 0x20,
        0x32, 0x35, 0x36, 0x20, 0x46, 0x6f, 0x75, 0x6e, 0x64, 0x61, 0x74, 0x69, 0x6f, 0x6e, 0x2f,
        0xff, 0xff, 0xff, 0xff, 0x02, 0x75, 0xaa, 0xc0, 0x12, 0x00, 0x00, 0x00, 0x00, 0x16, 0x00,
        0x14, 0xc6, 0x4b, 0x1b, 0x92, 0x83, 0xba, 0x1e, 0xa8, 0x6b, 0xb9, 0xe7, 0xb6, 0x96, 0xb0,
        0xc8, 0xf6, 0x8d, 0xad, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x26,
        0x6a, 0x24, 0xaa, 0x21, 0xa9, 0xed, 0xb3, 0x95, 0x56, 0x0a, 0xb7, 0x20, 0x68, 0xc2, 0xaf,
        0xb1, 0xd7, 0xe6, 0xc7, 0xdb, 0x26, 0xb8, 0x13, 0x48, 0x20, 0x89, 0xd2, 0xc4, 0xd2, 0x47,
        0x1b, 0x90, 0x36, 0xc5, 0xe8, 0x26, 0x14, 0x06, 0x01, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00,
    ];

    // Stratum v1 split points for the coinbase transaction.
    //
    // The extranonce space starts at byte 54 of COINBASE_TX and spans 8 bytes (4 for
    // extranonce1, 4 for extranonce2). This split is our interpretation based on the
    // transaction structure - other pools might split differently.

    /// Coinbase transaction prefix (coinbase1 in Stratum v1)
    ///
    /// Everything before the extranonce space: version + witness flag + input count +
    /// prev output + prev index + scriptsig length + first part of scriptsig.
    pub fn coinbase1_bytes() -> &'static [u8] {
        &COINBASE_TX[0..54]
    }

    /// Extranonce1 (pool-assigned) as used in this block
    pub fn extranonce1_bytes() -> &'static [u8] {
        &COINBASE_TX[54..58]
    }

    /// Extranonce2 (miner-chosen) as used in this block
    pub fn extranonce2_bytes() -> &'static [u8] {
        &COINBASE_TX[58..62]
    }

    /// Coinbase transaction suffix (coinbase2 in Stratum v1)
    ///
    /// Everything after the extranonce space: rest of scriptsig + sequence + outputs +
    /// witness + locktime.
    pub fn coinbase2_bytes() -> &'static [u8] {
        &COINBASE_TX[62..]
    }

    /// Extranonce1 as Extranonce2 type
    pub static EXTRANONCE1: LazyLock<Extranonce2> = LazyLock::new(|| {
        let bytes = extranonce1_bytes();
        let value = u32::from_le_bytes(bytes.try_into().unwrap());
        Extranonce2::new(value as u64, bytes.len() as u8).unwrap()
    });

    /// Extranonce2 as Extranonce2 type
    pub static EXTRANONCE2: LazyLock<Extranonce2> = LazyLock::new(|| {
        let bytes = extranonce2_bytes();
        let value = u32::from_le_bytes(bytes.try_into().unwrap());
        Extranonce2::new(value as u64, bytes.len() as u8).unwrap()
    });

    /// Merkle branches as raw byte arrays
    ///
    /// These are the sibling hashes needed to compute the merkle root from the coinbase
    /// transaction hash.
    pub const MERKLE_BRANCHES_BYTES: [[u8; 32]; 11] = [
        [
            0x42, 0x82, 0x35, 0x7a, 0xb0, 0xa2, 0xf4, 0xe8, 0xe5, 0x62, 0xc8, 0xc5, 0xea, 0xe1,
            0xd6, 0x3b, 0x55, 0x90, 0x68, 0xf9, 0x07, 0x23, 0x74, 0xb7, 0x2e, 0x26, 0xb8, 0x8a,
            0xc8, 0x41, 0x90, 0x8f,
        ],
        [
            0xc2, 0xbb, 0xae, 0x90, 0xd0, 0x6f, 0x80, 0x25, 0xad, 0xe3, 0x51, 0x6c, 0xa2, 0xe4,
            0x2d, 0x79, 0x31, 0x8b, 0x6e, 0x56, 0xfc, 0xcb, 0x96, 0x09, 0xdf, 0x85, 0xa6, 0x54,
            0xa8, 0x34, 0x10, 0x68,
        ],
        [
            0xcb, 0xb3, 0x00, 0x84, 0xac, 0xbc, 0xb5, 0xce, 0x6d, 0x54, 0x36, 0xe8, 0x53, 0xbc,
            0x61, 0x24, 0x66, 0xdf, 0x0c, 0x00, 0x70, 0x2b, 0xe6, 0xf5, 0x1d, 0x47, 0x07, 0x2f,
            0x1a, 0x5d, 0x04, 0x80,
        ],
        [
            0xbb, 0x09, 0x2e, 0x53, 0x3f, 0x70, 0xb7, 0x92, 0x46, 0xfc, 0x9e, 0x4b, 0xdc, 0x9b,
            0x81, 0xac, 0x83, 0x32, 0xe5, 0x6f, 0x26, 0x51, 0xe3, 0xb2, 0x37, 0xa1, 0x50, 0xd6,
            0xd8, 0x07, 0xb0, 0x81,
        ],
        [
            0x0d, 0x32, 0x95, 0x99, 0x88, 0x73, 0x34, 0x47, 0xcb, 0x34, 0x03, 0x26, 0xa1, 0xfe,
            0x4d, 0x3c, 0x2c, 0xff, 0x5d, 0x63, 0x6e, 0x32, 0xa4, 0xe0, 0x8c, 0x3f, 0xc5, 0xa2,
            0x34, 0xf6, 0xf6, 0xbc,
        ],
        [
            0x8b, 0xa3, 0x47, 0x79, 0x40, 0x92, 0x4a, 0x90, 0x06, 0x7c, 0xff, 0x3d, 0xd3, 0x0b,
            0xc4, 0x69, 0x16, 0xcf, 0x4c, 0x2f, 0x73, 0xfb, 0x62, 0x29, 0xb8, 0xf8, 0x19, 0x7b,
            0x83, 0xb6, 0xdd, 0xb9,
        ],
        [
            0x3f, 0xe9, 0x7c, 0xc5, 0xde, 0xc1, 0xcd, 0xdd, 0x66, 0xc9, 0xd3, 0x78, 0x08, 0x88,
            0xc1, 0x45, 0xc1, 0x24, 0x18, 0x59, 0x0c, 0xd9, 0x83, 0xc4, 0x87, 0x1b, 0x1b, 0xf0,
            0x3f, 0xab, 0xbf, 0x9e,
        ],
        [
            0xbd, 0x1c, 0x6a, 0x3b, 0xcd, 0x0e, 0xba, 0xa0, 0xec, 0xa5, 0x4d, 0x9c, 0x80, 0x25,
            0x40, 0xf3, 0x12, 0xc7, 0x70, 0x54, 0x35, 0x60, 0x1d, 0x77, 0x6b, 0x4f, 0x59, 0xbb,
            0x52, 0xcf, 0xf6, 0x7d,
        ],
        [
            0x86, 0x38, 0xc1, 0x01, 0x90, 0xb8, 0xf4, 0xf5, 0x58, 0xba, 0xfd, 0xbb, 0x57, 0xf4,
            0x62, 0x26, 0xc3, 0x49, 0xa8, 0x3f, 0x22, 0x9f, 0x43, 0x11, 0x43, 0xae, 0x75, 0xe4,
            0x25, 0xfb, 0x5c, 0xbd,
        ],
        [
            0x6d, 0x6f, 0x4e, 0xae, 0x41, 0xcc, 0xcb, 0x21, 0xa7, 0xc9, 0x7a, 0xc4, 0x96, 0xd9,
            0xf8, 0x9b, 0x8a, 0x4f, 0x77, 0x23, 0x89, 0xb7, 0x6f, 0x76, 0x91, 0x51, 0x9d, 0xda,
            0x38, 0x26, 0x42, 0x21,
        ],
        [
            0x82, 0xda, 0xd7, 0xa1, 0xe2, 0x61, 0x3a, 0xf0, 0xe6, 0x1f, 0x77, 0xf0, 0x3d, 0x62,
            0x08, 0xc8, 0x3e, 0x72, 0xf9, 0x17, 0xda, 0xde, 0x42, 0x35, 0xcc, 0x35, 0xe3, 0x7e,
            0xe3, 0xad, 0x6e, 0x3f,
        ],
    ];

    /// Merkle branches as TxMerkleNode types
    pub static MERKLE_BRANCHES: LazyLock<Vec<TxMerkleNode>> = LazyLock::new(|| {
        MERKLE_BRANCHES_BYTES
            .iter()
            .map(|bytes| TxMerkleNode::from_byte_array(*bytes))
            .collect()
    });

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn test_version_consistency() {
            assert_eq!(
                Version::from_consensus(i32::from_le_bytes(version_bytes().try_into().unwrap())),
                *VERSION
            );
        }

        #[test]
        fn test_prev_blockhash_consistency() {
            assert_eq!(
                BlockHash::from_byte_array(prev_hash_bytes().try_into().unwrap()),
                *PREV_BLOCKHASH
            );
        }

        #[test]
        fn test_merkle_root_consistency() {
            assert_eq!(
                TxMerkleNode::from_byte_array(merkle_root_bytes().try_into().unwrap()),
                *MERKLE_ROOT
            );
        }

        #[test]
        fn test_time_consistency() {
            assert_eq!(u32::from_le_bytes(time_bytes().try_into().unwrap()), TIME);
        }

        #[test]
        fn test_bits_consistency() {
            assert_eq!(
                CompactTarget::from_consensus(u32::from_le_bytes(bits_bytes().try_into().unwrap())),
                *BITS
            );
        }

        #[test]
        fn test_nonce_consistency() {
            assert_eq!(u32::from_le_bytes(nonce_bytes().try_into().unwrap()), NONCE);
        }

        #[test]
        fn test_block_hash_consistency() {
            // Check hash from HEADER_BYTES against ground truth
            let hash_from_bytes = BlockHash::hash(&HEADER_BYTES);
            assert_eq!(hash_from_bytes, *BLOCK_HASH, "HEADER_BYTES hash mismatch");

            // Check hash from HEADER against ground truth
            let hash_from_header = HEADER.block_hash();
            assert_eq!(hash_from_header, *BLOCK_HASH, "HEADER hash mismatch");
        }

        #[test]
        fn test_coinbase_reconstruction_from_stratum() {
            // Reconstruct coinbase transaction from Stratum components
            let mut reconstructed = Vec::new();
            reconstructed.extend_from_slice(coinbase1_bytes());
            reconstructed.extend_from_slice(extranonce1_bytes());
            reconstructed.extend_from_slice(extranonce2_bytes());
            reconstructed.extend_from_slice(coinbase2_bytes());

            assert_eq!(
                reconstructed.as_slice(),
                COINBASE_TX,
                "Reconstructed coinbase from Stratum components doesn't match the coinbase"
            );
        }

        #[test]
        fn test_merkle_root_from_coinbase_and_branches() {
            use bitcoin::consensus::deserialize;
            use bitcoin::Transaction;

            // Parse the coinbase transaction
            let coinbase_tx: Transaction = deserialize(COINBASE_TX).expect("Valid transaction");

            // Compute txid
            // In SegWit, merkle trees use legacy txids, not wtxids
            let coinbase_txid = coinbase_tx.compute_txid();

            // Start with the coinbase txid
            let mut current_hash = coinbase_txid.to_byte_array();

            // Apply each merkle branch
            for branch in MERKLE_BRANCHES_BYTES.iter() {
                let mut combined = Vec::new();
                combined.extend_from_slice(&current_hash);
                combined.extend_from_slice(branch);

                let parent_hash = TxMerkleNode::hash(&combined);
                current_hash = parent_hash.to_byte_array();
            }

            // Final hash should be the merkle root
            let computed_merkle_root = TxMerkleNode::from_byte_array(current_hash);

            assert_eq!(
                computed_merkle_root, *MERKLE_ROOT,
                "Computed merkle root from coinbase and branches doesn't match ground truth"
            );
        }
    }
}
