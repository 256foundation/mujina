//! Test data from real mining hardware captures.
//!
//! This module provides known-good test data extracted from actual chip
//! communication on Bitaxe Gamma (single BM1370).
//!
//! This module serves as a rosetta stone between Stratum v1, Rust Bitcoin's
//! internal format, and the BM13xx wire protocol. It demonstrates the correct
//! transformations between these formats, validated against a real mining
//! round-trip from pool to chip and back.
//!
//! # Testing Strategy
//!
//! This module is a **data source only**. Tests here validate the internal
//! consistency of the test data itself (e.g., that Stratum constants match
//! wire frame values, that computed merkle roots match captured values).
//!
//! **Parser tests live in their respective modules:**
//! - Stratum parsing tests → `stratum_v1::messages::tests`
//! - Job conversion tests → `job_source::stratum_v1::tests`
//! - Wire protocol tests → `asic::bm13xx::protocol::tests`
//!
//! This separation ensures test_data remains a reference dataset that other
//! modules can depend on without circular dependencies.

use bitcoin::hash_types::TxMerkleNode;
use bitcoin::hashes::Hash;
use bitcoin::pow::CompactTarget;
use bitcoin::BlockHash;
use std::sync::LazyLock;

// Reference capture log showing the complete mining round-trip:
//
// [2025-06-19T14:45:28.918] stratum_task: rx: <MINING_NOTIFY>
//
// [2025-06-19T14:45:46.442] I (131071) bm1370: Send Job: 68
// [2025-06-19T14:45:46.446] tx: [55 AA 21 56 68 01 00 00 00 00 04 3A 02 17 D7 68 54 68
//                                55 19 A7 CB 04 4F 88 72 63 55 91 9E 61 A9 8B CF 71 A0
//                                C2 87 95 EA 54 DB 8C 36 41 4B 06 DD F5 F0 00 00 00 00
//                                00 00 00 00 96 52 01 00 1D 39 96 BC A3 F4 67 0D FC D4
//                                F2 01 C1 62 B9 6D FD 55 64 6B 00 00 00 20 72 1C]
//
// [2025-06-19T14:45:46.519] rx: [AA 55 4C 03 52 75 0C D2 05 A2 9C] [r0]
// [2025-06-19T14:45:46.519] I (131148) bm1370: Job ID: 68, Core: 38/2, Ver: 00B44000
// [2025-06-19T14:45:46.526] I (131148) asic_result: ID: 875b4b7, ver: 20B44000
//                                      Nonce 7552034C diff 29588.0 of 8192.
//
// [2025-06-19T14:45:46.537] I (131155) stratum_api: tx: <MINING_SUBMIT>
// [2025-06-19T14:45:46.603] I (131231) stratum_task: rx: {"id":11,"error":null,"result":true}
// [2025-06-19T14:45:46.604] I (131232) stratum_task: message result accepted
//

/// Raw Stratum JSON messages from the capture.
///
/// These are the actual wire messages exchanged with the pool. The rosetta
/// stone tests parse these and validate against the broken-out constants.
pub mod stratum_json {
    /// mining.notify message received from pool
    ///
    /// This is the exact JSON received over the wire that triggered the
    /// mining round resulting in an accepted share.
    pub const MINING_NOTIFY: &str = r#"{
  "id": null,
  "method": "mining.notify",
  "params": [
    "875b4b7",
    "6b6455fd6db962c101f2d4fc0d67f4a3bc96391d000152960000000000000000",
    "02000000010000000000000000000000000000000000000000000000000000000000000000ffffffff170330c30d5075626c69632d506f6f6c",
    "ffffffff02e5b5c61200000000220020984a77c289084ff2d434c316bdada021c6c183d507c8a20d3b159b09ac02fe280000000000000000266a24aa21a9edb98ee50410ed4abd48401ed484fc874409d086a3faf0816136a8ad6168314c5800000000",
    [
      "21af451ddb51e887ff1feb5592b87290098565035eb8500031aedcc776d4e72a",
      "c5af269519c809a9546d5a58ca6445d3dbb80cb7045448ecc48309af034da8f8",
      "fb9f8f9959f6bb0ceb63fa53aed1d5a615c6b6d3f50a468ea89a45a1234bda74",
      "a4f4fee8e5fc19ca8d93e67b9236c37ddb864982010434745c0abfe9b914980c",
      "33092206642744fbe5499c3e621cd5c6b52733e54fbebd869f070082b807f740",
      "3b857e32c5cff4864efab967b9a456ca03b2167ab96bd9076ce294c8a67a7fe2",
      "881a07cd881d0c3e590b4b090ea8d58e1439dc56c63686f7de23c47045441e30",
      "315e4dbcc8e7b1c9d594a73978268791880dddb2c26eec8e75768668dad99d80",
      "69952b77c632be16b1ac7ac7048f13d4e962b2e215d79a343f01e6e281d7c304",
      "fc63eb4392c4d6c6d689788875fca35143fdcd4f4a82e8698e0e441751a70b4a",
      "09e419bbe20aa3a7640f1b91f50599ceddff899e90d3f18951ad5418c4850a6b",
      "004978aa346b4f1880bcadb3ca3792d771ee6aeca427f61e74baba44b75cfb88"
    ],
    "20000000",
    "17023a04",
    "685468d7",
    false
  ]
}"#;

    /// mining.submit message sent to pool
    ///
    /// This is the share submission that was accepted by the pool.
    /// Note: username is redacted in this capture.
    pub const MINING_SUBMIT: &str = r#"{
  "id": 11,
  "method": "mining.submit",
  "params": [
    "bc1q...bitaxe",
    "875b4b7",
    "17000000",
    "685468d7",
    "7552034c",
    "00b44000"
  ]
}"#;
}

/// Test data from Bitaxe Gamma esp-miner with complete Stratum round-trip
pub mod esp_miner_job {
    use super::*;

    /// Wire protocol TX frame (job sent to chip)
    pub mod wire_tx {
        use super::*;

        /// Complete wire frame (TX to chip)
        ///
        /// All other values are extracted from this wire capture.
        pub const FRAME: [u8; 88] = [
            0x55, 0xAA, 0x21, 0x56, 0x68, 0x01, 0x00, 0x00, 0x00, 0x00, 0x04, 0x3A, 0x02, 0x17,
            0xD7, 0x68, 0x54, 0x68, 0x55, 0x19, 0xA7, 0xCB, 0x04, 0x4F, 0x88, 0x72, 0x63, 0x55,
            0x91, 0x9E, 0x61, 0xA9, 0x8B, 0xCF, 0x71, 0xA0, 0xC2, 0x87, 0x95, 0xEA, 0x54, 0xDB,
            0x8C, 0x36, 0x41, 0x4B, 0x06, 0xDD, 0xF5, 0xF0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x96, 0x52, 0x01, 0x00, 0x1D, 0x39, 0x96, 0xBC, 0xA3, 0xF4, 0x67, 0x0D,
            0xFC, 0xD4, 0xF2, 0x01, 0xC1, 0x62, 0xB9, 0x6D, 0xFD, 0x55, 0x64, 0x6B, 0x00, 0x00,
            0x00, 0x20, 0x72, 0x1C,
        ];

        // Macro to define TX frame slices
        macro_rules! tx_slice {
            ($name:ident, $range:expr) => {
                pub static $name: LazyLock<&'static [u8]> = LazyLock::new(|| &FRAME[$range]);
            };
        }

        // Slice constants define frame layout
        tx_slice!(JOB_ID_BYTE, 4..5);
        tx_slice!(NUM_MIDSTATES_BYTE, 5..6);
        tx_slice!(STARTING_NONCE_BYTES, 6..10);
        tx_slice!(NBITS_BYTES, 10..14);
        tx_slice!(NTIME_BYTES, 14..18);
        tx_slice!(MERKLE_ROOT_BYTES, 18..50);
        tx_slice!(PREV_BLOCK_HASH_BYTES, 50..82);
        tx_slice!(VERSION_BYTES, 82..86);

        /// Job ID extracted from wire (shift right 3 to get 4-bit value)
        pub static JOB_ID: LazyLock<u8> = LazyLock::new(|| JOB_ID_BYTE[0] >> 3);

        /// Network difficulty (nbits from block header)
        pub static NBITS: LazyLock<CompactTarget> = LazyLock::new(|| {
            CompactTarget::from_consensus(u32::from_le_bytes((*NBITS_BYTES).try_into().unwrap()))
        });

        /// Block timestamp
        pub static NTIME: LazyLock<u32> =
            LazyLock::new(|| u32::from_le_bytes((*NTIME_BYTES).try_into().unwrap()));

        /// Block version
        pub static VERSION: LazyLock<bitcoin::block::Version> = LazyLock::new(|| {
            bitcoin::block::Version::from_consensus(u32::from_le_bytes(
                (*VERSION_BYTES).try_into().unwrap(),
            ) as i32)
        });

        /// Previous block hash
        /// Wire format: little-endian 32-byte hash split into 8 4-byte little-endian words,
        /// sent most significant word first.
        /// Convert to internal format by reversing the order of the words.
        pub static PREV_BLOCKHASH: LazyLock<BlockHash> = LazyLock::new(|| {
            let wire_bytes: [u8; 32] = (*PREV_BLOCK_HASH_BYTES).try_into().unwrap();
            let mut internal_bytes = [0u8; 32];

            // Reverse the order of 4-byte words (word 0<->7, 1<->6, 2<->5, 3<->4)
            for i in 0..8 {
                let src_word = &wire_bytes[i * 4..(i + 1) * 4];
                let dst_word = &mut internal_bytes[(7 - i) * 4..(8 - i) * 4];
                dst_word.copy_from_slice(src_word);
            }

            BlockHash::from_byte_array(internal_bytes)
        });

        /// Merkle root
        /// Wire format: little-endian 32-byte hash split into 8 4-byte little-endian words,
        /// sent most significant word first.
        /// Convert to internal format by reversing the order of the words.
        pub static MERKLE_ROOT: LazyLock<bitcoin::hash_types::TxMerkleNode> = LazyLock::new(|| {
            let wire_bytes: [u8; 32] = (*MERKLE_ROOT_BYTES).try_into().unwrap();
            let mut internal_bytes = [0u8; 32];

            // Reverse the order of 4-byte words (word 0<->7, 1<->6, 2<->5, 3<->4)
            for i in 0..8 {
                let src_word = &wire_bytes[i * 4..(i + 1) * 4];
                let dst_word = &mut internal_bytes[(7 - i) * 4..(8 - i) * 4];
                dst_word.copy_from_slice(src_word);
            }

            bitcoin::hash_types::TxMerkleNode::from_byte_array(internal_bytes)
        });
    }

    /// Wire protocol RX frame (nonce response from chip)
    pub mod wire_rx {
        use super::*;

        /// Nonce response frame (RX from chip)
        pub const FRAME: [u8; 11] = [
            0xAA, 0x55, 0x4C, 0x03, 0x52, 0x75, 0x0C, 0xD2, 0x05, 0xA2, 0x9C,
        ];

        // Macro to define RX frame slices
        macro_rules! rx_slice {
            ($name:ident, $range:expr) => {
                pub static $name: LazyLock<&'static [u8]> = LazyLock::new(|| &FRAME[$range]);
            };
        }

        // Slice constants for RX response
        rx_slice!(NONCE_BYTES, 2..6);
        rx_slice!(MIDSTATE_BYTE, 6..7);
        rx_slice!(RESULT_HEADER_BYTE, 7..8);
        rx_slice!(VERSION_ROLLING_BYTES, 8..10);

        /// Nonce from chip response
        pub static NONCE: LazyLock<u32> =
            LazyLock::new(|| u32::from_le_bytes((*NONCE_BYTES).try_into().unwrap()));

        /// Midstate number from response
        pub static MIDSTATE_NUM: LazyLock<u8> = LazyLock::new(|| MIDSTATE_BYTE[0]);

        /// Job ID from response (byte 7, bits 7-4)
        pub static JOB_ID: LazyLock<u8> = LazyLock::new(|| (RESULT_HEADER_BYTE[0] >> 4) & 0x0F);

        /// Subcore ID from response (byte 7, bits 3-0)
        pub static SUBCORE_ID: LazyLock<u8> = LazyLock::new(|| RESULT_HEADER_BYTE[0] & 0x0F);

        /// Version rolling field from chip response (bytes 8-9, big-endian u16)
        /// This 16-bit value occupies bits 13-28 of the block version when shifted left 13.
        pub static VERSION_ROLLING_FIELD: LazyLock<u16> =
            LazyLock::new(|| u16::from_be_bytes((*VERSION_ROLLING_BYTES).try_into().unwrap()));
    }

    /// Expected hash difficulty (from esp-miner validation, pool accepted)
    pub const EXPECTED_HASH_DIFFICULTY: f64 = 29588.0;

    /// Pool share difficulty threshold (from Stratum mining.set_difficulty)
    pub const POOL_SHARE_DIFFICULTY: f64 = 8192.0;

    /// Pool share difficulty as integer for tests
    pub const POOL_SHARE_DIFFICULTY_INT: u64 = 8192;

    /// Version mask authorized by pool (standard BIP320 mask)
    ///
    /// This is the typical mask returned by pools in mining.configure response.
    /// Bits 13-28 (0x1fffe000) are available for version rolling.
    pub const VERSION_MASK: u32 = 0x1fffe000;

    /// Extranonce1 from mining.subscribe response
    pub const STRATUM_EXTRANONCE1: &str = "4128064f";

    /// Extranonce2 size from mining.subscribe response
    pub const STRATUM_EXTRANONCE2_SIZE: usize = 4;

    /// Fields from mining.notify in the capture
    pub mod notify {
        use super::*;

        /// Job ID from params[0] (hex string)
        pub const JOB_ID_STRING: &str = "875b4b7";

        /// Previous block hash from params[1] (goofy stratum encoding)
        pub const PREV_BLOCKHASH_STRING: &str =
            "6b6455fd6db962c101f2d4fc0d67f4a3bc96391d000152960000000000000000";

        /// Coinbase1 from params[2] (hex string)
        pub const COINBASE1: &str = "02000000010000000000000000000000000000000000000000000000000000000000000000ffffffff170330c30d5075626c69632d506f6f6c";

        /// Coinbase2 from params[3] (hex string)
        pub const COINBASE2: &str = "ffffffff02e5b5c61200000000220020984a77c289084ff2d434c316bdada021c6c183d507c8a20d3b159b09ac02fe280000000000000000266a24aa21a9edb98ee50410ed4abd48401ed484fc874409d086a3faf0816136a8ad6168314c5800000000";

        /// Merkle branches from params[4]
        pub const MERKLE_BRANCH_STRINGS: [&str; 12] = [
            "21af451ddb51e887ff1feb5592b87290098565035eb8500031aedcc776d4e72a",
            "c5af269519c809a9546d5a58ca6445d3dbb80cb7045448ecc48309af034da8f8",
            "fb9f8f9959f6bb0ceb63fa53aed1d5a615c6b6d3f50a468ea89a45a1234bda74",
            "a4f4fee8e5fc19ca8d93e67b9236c37ddb864982010434745c0abfe9b914980c",
            "33092206642744fbe5499c3e621cd5c6b52733e54fbebd869f070082b807f740",
            "3b857e32c5cff4864efab967b9a456ca03b2167ab96bd9076ce294c8a67a7fe2",
            "881a07cd881d0c3e590b4b090ea8d58e1439dc56c63686f7de23c47045441e30",
            "315e4dbcc8e7b1c9d594a73978268791880dddb2c26eec8e75768668dad99d80",
            "69952b77c632be16b1ac7ac7048f13d4e962b2e215d79a343f01e6e281d7c304",
            "fc63eb4392c4d6c6d689788875fca35143fdcd4f4a82e8698e0e441751a70b4a",
            "09e419bbe20aa3a7640f1b91f50599ceddff899e90d3f18951ad5418c4850a6b",
            "004978aa346b4f1880bcadb3ca3792d771ee6aeca427f61e74baba44b75cfb88",
        ];

        /// Network difficulty bits from params[12] (big-endian hex string)
        pub const NBITS_STRING: &str = "17023a04";

        /// Block version from params[13] (big-endian hex string)
        pub const VERSION_STRING: &str = "20000000";

        /// Block timestamp from params[14] (big-endian hex string)
        pub const NTIME_STRING: &str = "685468d7";

        /// Clean jobs flag from params[15]
        pub const CLEAN_JOBS: bool = false;

        /// Previous block hash for hash validation
        /// Stratum sends 8 little-endian 4-byte words, but each 4-byte word is printed as
        /// big-endian hex. To get the actual bytes, decode hex then swap each 4-byte word.
        pub static PREV_BLOCKHASH: LazyLock<BlockHash> = LazyLock::new(|| {
            let mut bytes = hex::decode(PREV_BLOCKHASH_STRING).expect("valid hex");
            // Swap bytes within each 4-byte word
            for chunk in bytes.chunks_mut(4) {
                chunk.reverse();
            }
            BlockHash::from_byte_array(bytes.try_into().expect("32 bytes"))
        });

        /// Merkle branches parsed as TxMerkleNode (internal order)
        pub static MERKLE_BRANCHES: LazyLock<Vec<TxMerkleNode>> = LazyLock::new(|| {
            MERKLE_BRANCH_STRINGS
                .iter()
                .map(|s| {
                    let bytes = hex::decode(s).expect("valid hex");
                    TxMerkleNode::from_byte_array(bytes.try_into().expect("32 bytes"))
                })
                .collect()
        });

        /// Computed merkle root for hash validation
        /// Computed from coinbase transaction and merkle branches
        pub static MERKLE_ROOT: LazyLock<TxMerkleNode> = LazyLock::new(|| {
            use bitcoin::consensus::deserialize;
            use bitcoin::Transaction;

            // Build coinbase transaction
            let mut coinbase_bytes = Vec::new();
            coinbase_bytes.extend(hex::decode(COINBASE1).expect("valid coinbase1"));
            coinbase_bytes.extend(&hex::decode(STRATUM_EXTRANONCE1).expect("valid extranonce1"));
            coinbase_bytes.extend_from_slice(&*submit::EXTRANONCE2);
            coinbase_bytes.extend(hex::decode(COINBASE2).expect("valid coinbase2"));

            let coinbase_tx: Transaction =
                deserialize(&coinbase_bytes).expect("valid coinbase transaction");
            let coinbase_txid = coinbase_tx.compute_txid();

            // Apply merkle branches
            let merkle_root_bytes =
                MERKLE_BRANCHES
                    .iter()
                    .fold(coinbase_txid.to_byte_array(), |hash, branch| {
                        let mut combined = Vec::new();
                        combined.extend_from_slice(&hash);
                        combined.extend_from_slice(&branch.to_byte_array());
                        TxMerkleNode::hash(&combined).to_byte_array()
                    });

            TxMerkleNode::from_byte_array(merkle_root_bytes)
        });

        /// Network difficulty bits parsed as CompactTarget
        pub static NBITS: LazyLock<CompactTarget> = LazyLock::new(|| {
            let value = u32::from_be_bytes(
                hex::decode(NBITS_STRING)
                    .expect("valid hex")
                    .try_into()
                    .expect("4 bytes"),
            );
            CompactTarget::from_consensus(value)
        });

        /// Block version parsed as Version
        pub static VERSION: LazyLock<bitcoin::block::Version> = LazyLock::new(|| {
            let value = u32::from_be_bytes(
                hex::decode(VERSION_STRING)
                    .expect("valid hex")
                    .try_into()
                    .expect("4 bytes"),
            );
            bitcoin::block::Version::from_consensus(value as i32)
        });

        /// Block timestamp parsed as u32
        pub static NTIME: LazyLock<u32> = LazyLock::new(|| {
            u32::from_be_bytes(
                hex::decode(NTIME_STRING)
                    .expect("valid hex")
                    .try_into()
                    .expect("4 bytes"),
            )
        });
    }

    /// Fields from mining.submit in the capture
    pub mod submit {
        use super::*;

        /// Job ID from params[1] (Stratum job ID, not wire protocol job_id)
        pub const JOB_ID_STRING: &str = "875b4b7";

        /// Extranonce2 from params[2] (hex string)
        pub const EXTRANONCE2_STRING: &str = "17000000";

        /// Block timestamp from params[3] (hex string, should match notify::NTIME)
        pub const NTIME_STRING: &str = "685468d7";

        /// Nonce from params[4] (hex string)
        pub const NONCE_STRING: &str = "7552034c";

        /// Version rolling field from params[5] (hex string, RX_VERSION_ROLLING_FIELD << 13)
        pub const VERSION_STRING: &str = "00b44000";

        /// Extranonce2 parsed as bytes
        pub static EXTRANONCE2: LazyLock<[u8; 4]> = LazyLock::new(|| {
            hex::decode(EXTRANONCE2_STRING)
                .expect("valid hex")
                .try_into()
                .expect("4 bytes")
        });

        /// Block timestamp parsed as u32
        pub static NTIME: LazyLock<u32> = LazyLock::new(|| {
            u32::from_be_bytes(
                hex::decode(NTIME_STRING)
                    .expect("valid hex")
                    .try_into()
                    .expect("4 bytes"),
            )
        });

        /// Nonce parsed as u32
        pub static NONCE: LazyLock<u32> = LazyLock::new(|| {
            u32::from_be_bytes(
                hex::decode(NONCE_STRING)
                    .expect("valid hex")
                    .try_into()
                    .expect("4 bytes"),
            )
        });

        /// Version rolling field parsed as u32 (RX_VERSION_ROLLING_FIELD << 13)
        pub static VERSION: LazyLock<u32> = LazyLock::new(|| {
            u32::from_be_bytes(
                hex::decode(VERSION_STRING)
                    .expect("valid hex")
                    .try_into()
                    .expect("4 bytes"),
            )
        });
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Rosetta stone test: validate raw JSON matches broken-out constants.
        ///
        /// This validates that stratum_json::MINING_NOTIFY contains the same
        /// values as the manually-extracted notify::* constants.
        #[test]
        fn test_mining_notify_json_matches_constants() {
            // Parse the raw JSON message
            let json: serde_json::Value =
                serde_json::from_str(super::super::stratum_json::MINING_NOTIFY)
                    .expect("Failed to parse MINING_NOTIFY JSON");

            // Extract params array
            let params = json["params"].as_array().expect("params not an array");

            // Validate each field matches the broken-out constants
            assert_eq!(
                params[0].as_str().unwrap(),
                notify::JOB_ID_STRING,
                "job_id mismatch"
            );

            assert_eq!(
                params[1].as_str().unwrap(),
                notify::PREV_BLOCKHASH_STRING,
                "prev_blockhash mismatch"
            );

            assert_eq!(
                params[2].as_str().unwrap(),
                notify::COINBASE1,
                "coinbase1 mismatch"
            );

            assert_eq!(
                params[3].as_str().unwrap(),
                notify::COINBASE2,
                "coinbase2 mismatch"
            );

            // Validate merkle branches
            let branches = params[4].as_array().expect("merkle_branches not an array");
            assert_eq!(
                branches.len(),
                notify::MERKLE_BRANCH_STRINGS.len(),
                "merkle branch count mismatch"
            );
            for (i, branch) in branches.iter().enumerate() {
                assert_eq!(
                    branch.as_str().unwrap(),
                    notify::MERKLE_BRANCH_STRINGS[i],
                    "merkle branch {} mismatch",
                    i
                );
            }

            assert_eq!(
                params[5].as_str().unwrap(),
                notify::VERSION_STRING,
                "version mismatch"
            );

            assert_eq!(
                params[6].as_str().unwrap(),
                notify::NBITS_STRING,
                "nbits mismatch"
            );

            assert_eq!(
                params[7].as_str().unwrap(),
                notify::NTIME_STRING,
                "ntime mismatch"
            );

            assert_eq!(
                params[8].as_bool().unwrap(),
                notify::CLEAN_JOBS,
                "clean_jobs mismatch"
            );
        }

        /// Rosetta stone test: validate raw JSON matches broken-out constants.
        #[test]
        fn test_mining_submit_json_matches_constants() {
            let json: serde_json::Value =
                serde_json::from_str(super::super::stratum_json::MINING_SUBMIT)
                    .expect("Failed to parse MINING_SUBMIT JSON");

            let params = json["params"].as_array().expect("params not an array");

            // params[0] is username (redacted in capture)
            assert_eq!(
                params[1].as_str().unwrap(),
                submit::JOB_ID_STRING,
                "job_id mismatch"
            );

            assert_eq!(
                params[2].as_str().unwrap(),
                submit::EXTRANONCE2_STRING,
                "extranonce2 mismatch"
            );

            assert_eq!(
                params[3].as_str().unwrap(),
                submit::NTIME_STRING,
                "ntime mismatch"
            );

            assert_eq!(
                params[4].as_str().unwrap(),
                submit::NONCE_STRING,
                "nonce mismatch"
            );

            assert_eq!(
                params[5].as_str().unwrap(),
                submit::VERSION_STRING,
                "version_bits mismatch"
            );
        }

        #[test]
        fn test_stratum_header_fields_match_wire() {
            // Compare parsed Stratum values with wire frame values
            assert_eq!(
                *wire_tx::VERSION,
                *notify::VERSION,
                "Version from Stratum doesn't match wire frame"
            );

            assert_eq!(
                *wire_tx::NBITS,
                *notify::NBITS,
                "Nbits from Stratum doesn't match wire frame"
            );

            assert_eq!(
                *wire_tx::NTIME,
                *notify::NTIME,
                "Ntime from Stratum doesn't match wire frame"
            );
        }

        #[test]
        fn test_job_id_round_trip() {
            assert_eq!(
                *wire_tx::JOB_ID,
                *wire_rx::JOB_ID,
                "Job ID sent to chip should match job ID in response"
            );
        }

        #[test]
        fn test_nonce_response_matches_submit() {
            // Verify nonce from RX frame matches mining.submit params[4]
            assert_eq!(
                *wire_rx::NONCE,
                *submit::NONCE,
                "Nonce from RX frame should match mining.submit params[4]"
            );

            // Verify version rolling field shifted left 13 matches mining.submit params[5]
            let version_rolling_shifted = (*wire_rx::VERSION_ROLLING_FIELD as u32) << 13;
            assert_eq!(
                version_rolling_shifted,
                *submit::VERSION,
                "wire_rx::VERSION_ROLLING_FIELD << 13 should match mining.submit params[5]"
            );

            // Verify ntime from submit matches ntime from mining.notify
            assert_eq!(
                *submit::NTIME,
                *notify::NTIME,
                "mining.submit ntime should match mining.notify ntime"
            );
        }

        #[test]
        fn test_stratum_prev_blockhash_matches_wire() {
            // Verify parsed Stratum prev_blockhash matches wire
            assert_eq!(
                *wire_tx::PREV_BLOCKHASH,
                *notify::PREV_BLOCKHASH,
                "Stratum prev_blockhash should match wire prev_blockhash"
            );
        }

        #[test]
        fn test_merkle_root_from_stratum_matches_wire() {
            // Verify computed merkle root from Stratum matches wire
            assert_eq!(
                *notify::MERKLE_ROOT,
                *wire_tx::MERKLE_ROOT,
                "Computed merkle root from Stratum data doesn't match wire"
            );
        }

        #[test]
        fn test_block_hash_validation() {
            use crate::types::Difficulty;
            use bitcoin::block::Header as BlockHeader;

            // Build full version: base version OR'd with version rolling field shifted left 13
            let base_version = wire_tx::VERSION.to_consensus();
            let version_rolling_field = *wire_rx::VERSION_ROLLING_FIELD;
            let full_version = bitcoin::block::Version::from_consensus(
                base_version | ((version_rolling_field as i32) << 13),
            );

            // Build block header using Stratum-derived values for hashes
            let header = BlockHeader {
                version: full_version,
                prev_blockhash: *notify::PREV_BLOCKHASH, // Word-swapped from Stratum
                merkle_root: *notify::MERKLE_ROOT,       // Computed from Stratum
                time: *wire_tx::NTIME,
                bits: *wire_tx::NBITS,
                nonce: *wire_rx::NONCE,
            };

            // Compute block hash and difficulty
            let hash = header.block_hash();
            let difficulty = Difficulty::from_hash(&hash);

            println!("Block hash: {}", hash);
            println!("Difficulty: {}", difficulty);

            // Verify difficulty matches expected value from esp-miner logs
            // Allow +/-1 tolerance for integer division rounding
            let expected = Difficulty::from(EXPECTED_HASH_DIFFICULTY as u64);
            assert!(
                difficulty >= Difficulty::from(expected.as_u64() - 1)
                    && difficulty <= Difficulty::from(expected.as_u64() + 1),
                "Hash difficulty mismatch: computed={}, expected={}",
                difficulty,
                expected
            );

            // Verify this would be a valid pool share
            assert!(
                difficulty >= Difficulty::from(POOL_SHARE_DIFFICULTY_INT),
                "Hash difficulty {} should exceed pool difficulty {}",
                difficulty,
                POOL_SHARE_DIFFICULTY_INT
            );
        }
    }
}
