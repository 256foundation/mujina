pub mod bm13xx;
pub mod hash_thread;

/// Information about a chip
#[derive(Debug, Clone)]
pub struct ChipInfo {
    /// Chip model identifier (e.g., [0x13, 0x70] for BM1370)
    pub chip_id: [u8; 2],
    /// Number of hashing cores
    pub core_count: u32,
    /// Chip address on the serial bus
    pub address: u8,
    /// Whether the chip supports version rolling
    pub supports_version_rolling: bool,
}
