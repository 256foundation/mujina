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
    /// Target difficulty for generated jobs
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
    /// - 1.0 = Bitcoin difficulty 1.0 (for testing)
    /// - Higher values = harder (for production use)
    pub fn new(difficulty: f64) -> Self {
        // For production use during outages, we might want to use
        // a higher difficulty to avoid flooding logs with "found block!"
        // messages that can't actually be submitted
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
        let mut generator = Self::new(1_000_000.0);
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
    fn difficulty_to_target(difficulty: f64) -> Target {
        if difficulty <= 0.0 {
            panic!("Difficulty must be positive");
        }

        // Bitcoin difficulty 1.0 compact representation
        let diff_1_compact = CompactTarget::from_consensus(0x1d00ffff);

        if difficulty == 1.0 {
            return Target::from_compact(diff_1_compact);
        }

        // For other difficulties, we adjust the compact representation
        // This is simplified - production code would use proper calculations
        if difficulty < 1.0 {
            // Easier than diff 1 - use a higher target value
            // Max target is roughly 0x1d7fffff
            let compact = CompactTarget::from_consensus(0x1d7fffff);
            Target::from_compact(compact)
        } else {
            // Harder than diff 1 - use a lower target value
            // This is approximate for testing
            // Each bit in the exponent represents ~256x difficulty
            let exponent_adj = (difficulty.log2() / 8.0) as u32;
            let compact_bits = 0x1d00ffff_u32.saturating_sub(exponent_adj << 24);
            let compact = CompactTarget::from_consensus(compact_bits);
            Target::from_compact(compact)
        }
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
            prev_block_hash: header.prev_blockhash.as_byte_array().clone(),
            merkle_root: header.merkle_root.as_byte_array().clone(),
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
        // Use recognizable pattern so we know these are fallback blocks
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

    /// Create a header for test mode
    fn create_test_header(&mut self) -> BlockHeader {
        // Create deterministic but unique hashes for testing
        let mut prev_blockhash_bytes = [0u8; 32];
        let height_bytes = self.block_height.to_be_bytes();
        prev_blockhash_bytes[0..4].copy_from_slice(&height_bytes);
        prev_blockhash_bytes[4..8].copy_from_slice(b"TEST");

        let mut merkle_root_bytes = [0u8; 32];
        merkle_root_bytes[0..6].copy_from_slice(b"MUJINA");
        merkle_root_bytes[6..10].copy_from_slice(&self.job_id_counter.to_be_bytes()[4..8]);

        BlockHeader {
            version: bitcoin::blockdata::block::Version::from_consensus(self.version),
            prev_blockhash: BlockHash::from_byte_array(prev_blockhash_bytes),
            merkle_root: TxMerkleNode::from_byte_array(merkle_root_bytes),
            time: self.base_time,
            bits: self.target.to_compact_lossy(),
            nonce: 0,
        }
    }

    /// Get a well-known test job with a known valid nonce
    ///
    /// This uses Bitcoin's genesis block which has a known solution
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
            merkle_root: header.merkle_root.as_byte_array().clone(),
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

    // Previous block hash (32 bytes, internal byte order)
    bytes[4..36].copy_from_slice(header.prev_blockhash.as_byte_array());

    // Merkle root (32 bytes, internal byte order)
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
pub fn verify_nonce(job: &MiningJob, nonce: u32) -> Result<(BlockHash, bool), String> {
    // Update header with nonce
    let mut header_bytes = job.header;
    header_bytes[76..80].copy_from_slice(&nonce.to_le_bytes());

    // Calculate double SHA256 (Bitcoin block hash)
    let hash = sha256d::Hash::hash(&header_bytes);
    let block_hash = BlockHash::from_byte_array(hash.to_byte_array());

    // Convert hash to Target for comparison
    let hash_as_target = Target::from_le_bytes(hash.to_byte_array());

    // Get job target
    let job_target = Target::from_le_bytes(job.target);

    // Check if hash meets target (hash must be <= target)
    let valid = hash_as_target <= job_target;

    if valid {
        info!(
            job_id = job.job_id,
            nonce,
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
    fn test_job_generation() {
        let mut generator = JobGenerator::new(1.0);

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
        let (hash, valid) = verify_nonce(&job, known_nonce).unwrap();
        assert!(valid, "Known good nonce should produce valid hash");

        // The hash should meet difficulty 1 target
        println!("Genesis-like block hash: {:x}", hash);
    }
}
