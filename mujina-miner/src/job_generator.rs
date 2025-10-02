//! Job generator for creating mining work locally.
//!
//! This module generates valid Bitcoin block headers for mining, useful for:
//! - Testing hardware without pool connectivity
//! - Maintaining chip operation during network outages
//! - Solo mining operations
//!
//! When pool connectivity is lost, the miner can switch to locally generated
//! jobs to keep ASICs running and prevent thermal cycling.

use crate::asic::MiningJob;
use crate::tracing::prelude::*;
use crate::u256::U256;
use bitcoin::blockdata::block::Header as BlockHeader;
use bitcoin::hash_types::{BlockHash, TxMerkleNode};
use bitcoin::hashes::{sha256d, Hash};
use bitcoin::pow::{CompactTarget, Target};

/// Generates mining jobs locally when pool work is unavailable
pub struct JobGenerator {
    /// Current block height (incremented for each job)
    block_height: u32,
    /// Base timestamp (incremented to ensure unique jobs)
    base_time: u32,
    /// Target for generated jobs
    target: Target,
    /// Version field for block header
    version: i32,
    /// Job ID counter
    job_id_counter: u64,
    /// Optional coinbase address for solo mining
    coinbase_address: Option<String>,
    /// Whether we're in fallback mode (no pool connection)
    fallback_mode: bool,
}

impl JobGenerator {
    /// Create a new job generator with specified difficulty
    ///
    /// The difficulty parameter controls the target:
    /// - 1 = Bitcoin difficulty 1 (for testing)
    /// - Higher values = harder (for production use)
    pub fn new(difficulty: u64) -> Self {
        // For production use during outages, consider using higher difficulty
        // to avoid flooding logs with "found block!" messages that can't be
        // submitted
        let target = Self::difficulty_to_target(difficulty);

        Self {
            block_height: 800_000, // Will be updated from pool/network
            base_time: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as u32,
            target,
            version: bitcoin::blockdata::block::Version::TWO.to_consensus(),
            job_id_counter: 0,
            coinbase_address: None,
            fallback_mode: false,
        }
    }

    /// Create a job generator for production fallback use
    ///
    /// Uses a high difficulty to keep chips busy without finding blocks
    pub fn new_fallback() -> Self {
        // Use difficulty ~1 million to keep chips busy but not find blocks
        // This prevents excessive "found block!" logs during outages
        let mut generator = Self::new(1_000_000);
        generator.fallback_mode = true;
        generator
    }

    /// Update generator state from pool data when available
    pub fn update_from_pool(&mut self, block_height: u32, version: i32) {
        self.block_height = block_height;
        self.version = version;
        self.fallback_mode = false;
    }

    /// Set coinbase address for solo mining
    pub fn set_coinbase_address(&mut self, address: String) {
        self.coinbase_address = Some(address);
    }

    /// Convert difficulty to target
    ///
    /// Bitcoin's difficulty is defined as: difficulty = difficulty_1_target / target
    /// Therefore: target = difficulty_1_target / difficulty
    ///
    /// Uses exact 256-bit integer division with no precision loss.
    fn difficulty_to_target(difficulty: u64) -> Target {
        if difficulty == 0 {
            panic!("Difficulty must be positive");
        }

        if difficulty == 1 {
            // Fast path for difficulty 1
            return Target::from_compact(CompactTarget::from_consensus(0x1d00ffff));
        }

        // Get difficulty 1 target as U256
        // Target::MAX = 0x00000000FFFF0000000000000000000000000000000000000000000000000000
        let max_target_bytes = Target::MAX.to_le_bytes();
        let max_target_u256 = U256::from_le_bytes(max_target_bytes);

        // Perform exact 256-bit / 64-bit division
        let result_u256 = max_target_u256 / difficulty;

        // Convert back to Target
        Target::from_le_bytes(result_u256.to_le_bytes())
    }

    /// Generate the next mining job
    pub fn next_job(&mut self) -> MiningJob {
        // Update timestamp to current time
        self.base_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as u32;

        // Create block header
        let header = if self.fallback_mode {
            self.create_fallback_header()
        } else {
            self.create_test_header()
        };

        // Serialize header to bytes
        let header_bytes = serialize_header(&header);

        // Convert target to byte array
        let mut target_bytes = [0u8; 32];
        let target_u256 = self.target.to_le_bytes();
        target_bytes.copy_from_slice(&target_u256);

        let job = MiningJob {
            job_id: self.job_id_counter,
            header: header_bytes,
            target: target_bytes,
            nonce_start: 0,
            nonce_range: u32::MAX, // Search full range
            version: header.version.to_consensus() as u32,
            prev_block_hash: *header.prev_blockhash.as_byte_array(),
            merkle_root: *header.merkle_root.as_byte_array(),
            ntime: header.time,
            nbits: header.bits.to_consensus(),
        };

        self.job_id_counter += 1;

        if self.fallback_mode {
            debug!(
                job_id = job.job_id,
                mode = "fallback",
                "Generated fallback job to maintain hashrate"
            );
        } else {
            info!(
                job_id = job.job_id,
                block_height = self.block_height,
                bits = format!("{:08x}", job.nbits),
                "Generated mining job"
            );
        }

        job
    }

    /// Create a header for fallback mode (pool disconnected)
    fn create_fallback_header(&mut self) -> BlockHeader {
        // Use recognizable pattern to identify fallback blocks
        let mut prev_hash = [0u8; 32];
        prev_hash[0..8].copy_from_slice(b"FALLBACK");

        let mut merkle_root = [0u8; 32];
        merkle_root[0..7].copy_from_slice(b"MUJINA\0");

        BlockHeader {
            version: bitcoin::blockdata::block::Version::from_consensus(self.version),
            prev_blockhash: BlockHash::from_byte_array(prev_hash),
            merkle_root: TxMerkleNode::from_byte_array(merkle_root),
            time: self.base_time,
            bits: self.target.to_compact_lossy(),
            nonce: 0,
        }
    }

    /// Create a header for test mode using real pool data
    fn create_test_header(&mut self) -> BlockHeader {
        // Real block data from pool capture (job ID: 875880a from esp-miner logs)
        // This should produce valid nonces since it's from actual mining work
        let prev_blockhash_bytes = [
            0x3a, 0x3c, 0x8e, 0xa0, 0xe8, 0x3a, 0xc0, 0x1f, 0x75, 0x98, 0x8d, 0xe3, 0xfa, 0x28,
            0x52, 0xae, 0xfe, 0xe2, 0x90, 0x65, 0x00, 0x01, 0xbd, 0x05, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];

        // Merkle root from real pool data, but vary it slightly for each job
        // to simulate different transactions in each block
        let mut merkle_root_bytes = [
            0x9c, 0xb5, 0x3d, 0x58, 0x19, 0xf4, 0xb7, 0xe6, 0x29, 0xb1, 0xf7, 0xac, 0x4a, 0x87,
            0x2c, 0x14, 0x71, 0x64, 0x4f, 0x8e, 0xbd, 0x97, 0x18, 0x2a, 0x4d, 0x00, 0x29, 0x0d,
            0x72, 0x63, 0xd4, 0x87,
        ];

        // Vary the last 4 bytes based on job counter to make each job unique
        let counter_bytes = (self.job_id_counter as u32).to_le_bytes();
        merkle_root_bytes[28..32].copy_from_slice(&counter_bytes);

        // Use easier difficulty than the original pool job (0x17023a04)
        // to increase chances of finding nonces at our test frequency
        let test_bits = CompactTarget::from_consensus(0x1d00ffff); // Difficulty 1

        // Use original time but increment slightly for each job
        let time = 0x68546858 + self.job_id_counter as u32;

        BlockHeader {
            version: bitcoin::blockdata::block::Version::from_consensus(0x20000000),
            prev_blockhash: BlockHash::from_byte_array(prev_blockhash_bytes),
            merkle_root: TxMerkleNode::from_byte_array(merkle_root_bytes),
            time,
            bits: test_bits,
            nonce: 0,
        }
    }

    /// Get a well-known test job with a known valid nonce
    ///
    /// Uses Bitcoin's genesis block parameters with known valid nonce.
    /// This is useful for verifying that header serialization, hashing,
    /// and difficulty checking logic work correctly.
    pub fn known_good_job() -> (MiningJob, u32) {
        // Use parameters similar to Bitcoin's genesis block
        let header = BlockHeader {
            version: bitcoin::blockdata::block::Version::ONE,
            prev_blockhash: BlockHash::all_zeros(),
            merkle_root: TxMerkleNode::from_slice(
                &hex::decode("3ba3edfd7a7b12b27ac72c3e67768f617fc81bc3888a51323a9fb8aa4b1e5e4a")
                    .unwrap(),
            )
            .unwrap(),
            time: 0x495fab29,
            bits: CompactTarget::from_consensus(0x1d00ffff),
            nonce: 0,
        };

        let header_bytes = serialize_header(&header);
        let target = Target::from_compact(header.bits);
        let mut target_bytes = [0u8; 32];
        let target_u256 = target.to_le_bytes();
        target_bytes.copy_from_slice(&target_u256);

        let job = MiningJob {
            job_id: 999,
            header: header_bytes,
            target: target_bytes,
            nonce_start: 0,
            nonce_range: u32::MAX,
            version: 1,
            prev_block_hash: [0; 32],
            merkle_root: *header.merkle_root.as_byte_array(),
            ntime: header.time,
            nbits: header.bits.to_consensus(),
        };

        // Genesis block nonce
        let known_good_nonce = 2083236893u32;

        (job, known_good_nonce)
    }
}

/// Serialize a block header to the 80-byte format expected by miners
fn serialize_header(header: &BlockHeader) -> [u8; 80] {
    let mut bytes = [0u8; 80];

    // Version (4 bytes, little-endian)
    bytes[0..4].copy_from_slice(&header.version.to_consensus().to_le_bytes());

    // Previous block hash (32 bytes)
    // Bitcoin hashes are stored in their natural byte order (how they're computed)
    bytes[4..36].copy_from_slice(header.prev_blockhash.as_byte_array());

    // Merkle root (32 bytes)
    // Bitcoin hashes are stored in their natural byte order (how they're computed)
    bytes[36..68].copy_from_slice(header.merkle_root.as_byte_array());

    // Time (4 bytes, little-endian)
    bytes[68..72].copy_from_slice(&header.time.to_le_bytes());

    // Bits (4 bytes, little-endian)
    bytes[72..76].copy_from_slice(&header.bits.to_consensus().to_le_bytes());

    // Nonce (4 bytes, little-endian)
    bytes[76..80].copy_from_slice(&header.nonce.to_le_bytes());

    bytes
}

/// Verify that a nonce produces a valid hash for the given job
///
/// When chips use version rolling, they may modify the lower 16 bits of the
/// version field (bits 16-31 of the 32-bit version). The `rolled_version`
/// parameter contains these bits as returned by the chip.
pub fn verify_nonce(
    job: &MiningJob,
    nonce: u32,
    rolled_version: u16,
) -> Result<(BlockHash, bool), String> {
    // Validate that job target matches nbits encoding
    let nbits_target = Target::from_compact(CompactTarget::from_consensus(job.nbits));
    let job_target = Target::from_le_bytes(job.target);
    if nbits_target != job_target {
        return Err(format!(
            "Job target mismatch: nbits 0x{:08x} expands to {:?} but job.target is {:?}",
            job.nbits, nbits_target, job_target
        ));
    }

    // Update header with nonce and version
    let mut header_bytes = job.header;
    header_bytes[76..80].copy_from_slice(&nonce.to_le_bytes());

    // Apply version rolling: replace bits 16-31 of the version field
    // The version field is at bytes 0-3 (little-endian)
    let original_version = u32::from_le_bytes(header_bytes[0..4].try_into().unwrap());

    // Clear bits 16-31 and apply the rolled version
    let new_version = (original_version & 0x0000_FFFF) | ((rolled_version as u32) << 16);
    header_bytes[0..4].copy_from_slice(&new_version.to_le_bytes());

    // Calculate double SHA256 (Bitcoin block hash)
    let hash = sha256d::Hash::hash(&header_bytes);
    let block_hash = BlockHash::from_byte_array(hash.to_byte_array());

    // Convert hash to Target for comparison
    let hash_as_target = Target::from_le_bytes(hash.to_byte_array());

    // Check if hash meets target (hash must be <= target)
    let valid = hash_as_target <= job_target;

    if valid {
        info!(
            job_id = job.job_id,
            nonce,
            version = rolled_version,
            hash = format!("{:x}", block_hash),
            "Found valid nonce!"
        );
    }

    Ok((block_hash, valid))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_difficulty_to_target_conversion() {
        // Test difficulty 1 produces the expected target
        let target_1 = JobGenerator::difficulty_to_target(1);
        let expected_1 = Target::from_compact(CompactTarget::from_consensus(0x1d00ffff));
        assert_eq!(
            target_1, expected_1,
            "Difficulty 1 should match consensus target"
        );

        // Test that higher difficulty produces lower target
        let target_2 = JobGenerator::difficulty_to_target(2);
        assert!(
            target_2 < target_1,
            "Difficulty 2 should have lower target than 1"
        );

        // Test that the relationship is exact (no rounding errors)
        // For difficulty 2, target should be exactly half of difficulty 1
        let target_1_u256 = U256::from_le_bytes(target_1.to_le_bytes());
        let target_2_u256 = U256::from_le_bytes(target_2.to_le_bytes());
        let doubled = target_2_u256 * 2;
        assert_eq!(
            doubled, target_1_u256,
            "Difficulty 2 target * 2 should exactly equal difficulty 1 target"
        );

        // Test difficulty 1M (for fallback mode)
        let target_1m = JobGenerator::difficulty_to_target(1_000_000);
        assert!(
            target_1m < target_1,
            "Difficulty 1M should have much lower target than 1"
        );

        // Test very high difficulty like real Bitcoin network (100 trillion)
        let target_100t = JobGenerator::difficulty_to_target(100_000_000_000_000);
        assert!(
            target_100t < target_1m,
            "Difficulty 100T should have much lower target than 1M"
        );

        // Verify the round-trip: target -> difficulty_float() should be close
        // Note: difficulty_float() uses f64 so there will be some precision loss
        let diff_100t_roundtrip = target_100t.difficulty_float();
        let ratio_100t = diff_100t_roundtrip / 100_000_000_000_000.0;
        assert!(
            (ratio_100t - 1.0).abs() < 0.01,
            "Round-trip difficulty for 100T should be within 1%, got ratio {}",
            ratio_100t
        );
    }

    #[test]
    fn test_job_generation() {
        let mut generator = JobGenerator::new(1);

        let job1 = generator.next_job();
        let job2 = generator.next_job();

        // Jobs should be different
        assert_ne!(job1.job_id, job2.job_id);
        assert_ne!(job1.merkle_root, job2.merkle_root);

        // But target should be the same
        assert_eq!(job1.target, job2.target);
        assert_eq!(job1.nbits, job2.nbits);
    }

    #[test]
    fn test_fallback_mode() {
        let mut generator = JobGenerator::new_fallback();
        let job = generator.next_job();

        // Check fallback header pattern
        assert_eq!(&job.prev_block_hash[0..8], b"FALLBACK");

        // Fallback should use high difficulty
        assert!(job.nbits < 0x1d00ffff); // Harder than difficulty 1
    }

    #[test]
    fn test_known_good_nonce() {
        let (job, known_nonce) = JobGenerator::known_good_job();

        // Verify the known good nonce produces a valid hash
        // Genesis block doesn't use version rolling, so version bits are 0
        let (hash, valid) = verify_nonce(&job, known_nonce, 0).unwrap();
        assert!(valid, "Known good nonce should produce valid hash");

        // The hash should meet difficulty 1 target
        println!("Genesis-like block hash: {:x}", hash);
    }
}
