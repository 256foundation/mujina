//! BM13xx protocol implementation for chip communication.
//!
//! This module handles the encoding and decoding of commands and responses
//! for BM13xx family chips (BM1366, BM1370, etc).

use bitvec::prelude::*;
use bytes::{Buf, BufMut, BytesMut};
use std::io;
use strum::FromRepr;
use tokio_util::codec::{Decoder, Encoder};

use super::crc::{crc16, crc5, crc5_is_valid};
use super::error::ProtocolError;
use crate::asic::{ChipError, MiningJob};
use crate::tracing::prelude::*;

/// Mining frequency with validation and PLL calculation
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frequency {
    mhz: f32,
}

impl Frequency {
    /// Minimum supported frequency in MHz
    pub const MIN_MHZ: f32 = 50.0;
    /// Maximum supported frequency in MHz  
    pub const MAX_MHZ: f32 = 800.0;
    /// Base crystal frequency in MHz
    const CRYSTAL_MHZ: f32 = 25.0;

    /// Create a new frequency with validation
    pub fn from_mhz(mhz: f32) -> Result<Self, ProtocolError> {
        if mhz < Self::MIN_MHZ || mhz > Self::MAX_MHZ {
            return Err(ProtocolError::InvalidFrequency { mhz: mhz as u32 });
        }
        Ok(Self { mhz })
    }

    /// Get frequency in MHz
    pub fn mhz(&self) -> f32 {
        self.mhz
    }

    /// Calculate optimal PLL configuration for this frequency
    pub fn calculate_pll(&self) -> PllConfig {
        let target_freq = self.mhz;
        let mut best_config = PllConfig::new(0xa0, 2, 0x55); // Default
        let mut min_error = f32::MAX;

        // Search for optimal PLL settings
        // ref_divider: 1 or 2
        // post_divider1: 1-7, must be >= post_divider2
        // post_divider2: 1-7
        // fb_divider: 0xa0-0xef (160-239)

        for ref_div in [2, 1] {
            for post_div1 in (1..=7).rev() {
                for post_div2 in (1..=7).rev() {
                    if post_div1 >= post_div2 {
                        // Calculate required feedback divider
                        let fb_div_f =
                            (post_div1 * post_div2) as f32 * target_freq * ref_div as f32
                                / Self::CRYSTAL_MHZ;
                        let fb_div = fb_div_f.round() as u16;

                        if fb_div >= 0xa0 && fb_div <= 0xef {
                            // Calculate actual frequency with these settings
                            let actual_freq = Self::CRYSTAL_MHZ * fb_div as f32
                                / (ref_div as f32 * post_div1 as f32 * post_div2 as f32);
                            let error = (target_freq - actual_freq).abs();

                            if error < min_error && error < 1.0 {
                                min_error = error;
                                // Encode post dividers as per hardware format
                                let post_div = ((post_div1 - 1) << 4) | (post_div2 - 1);
                                best_config = PllConfig::new(fb_div, ref_div, post_div);
                            }
                        }
                    }
                }
            }
        }

        best_config
    }
}

/// PLL configuration for frequency control
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PllConfig {
    /// Main feedback divider
    pub fb_div: u16,
    /// Reference divider
    pub ref_div: u8,
    /// Post divider flags
    pub post_div: u8,
}

impl PllConfig {
    /// Create a new PLL configuration
    pub fn new(fb_div: u16, ref_div: u8, post_div: u8) -> Self {
        Self {
            fb_div,
            ref_div,
            post_div,
        }
    }
}

impl From<u32> for PllConfig {
    fn from(raw: u32) -> Self {
        Self {
            fb_div: (raw & 0xffff) as u16,
            ref_div: ((raw >> 16) & 0xff) as u8,
            post_div: ((raw >> 24) & 0xff) as u8,
        }
    }
}

impl From<PllConfig> for [u8; 4] {
    fn from(config: PllConfig) -> Self {
        let mut bytes = [0u8; 4];
        bytes[0..2].copy_from_slice(&config.fb_div.to_le_bytes());
        bytes[2] = config.ref_div;
        bytes[3] = config.post_div;
        bytes
    }
}

/// Known chip types in the BM13xx family
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipType {
    /// BM1362 - Used in Antminer S19 J Pro (126 chips)
    /// Core count unknown
    BM1362,
    /// BM1366 - Newer generation chip
    BM1366,
    /// BM1370 - Used in Bitaxe Gamma and Antminer S21 Pro
    /// 80 main cores × 16 sub-cores = 1,280 total hashing units
    BM1370,
    /// BM1397 - Previous generation chip
    BM1397,
    /// Unknown chip type with raw ID bytes
    Unknown([u8; 2]),
}

impl ChipType {
    /// Get the raw chip ID bytes
    pub fn id_bytes(&self) -> [u8; 2] {
        match self {
            Self::BM1362 => [0x13, 0x62],
            Self::BM1366 => [0x13, 0x66],
            Self::BM1370 => [0x13, 0x70],
            Self::BM1397 => [0x13, 0x97],
            Self::Unknown(bytes) => *bytes,
        }
    }

    /// Get expected core count for this chip type, if known
    pub fn core_count(&self) -> Option<u32> {
        match self {
            Self::BM1370 => Some(1280), // 80 × 16
            _ => None,
        }
    }
}

impl From<[u8; 2]> for ChipType {
    fn from(bytes: [u8; 2]) -> Self {
        match bytes {
            [0x13, 0x62] => Self::BM1362,
            [0x13, 0x66] => Self::BM1366,
            [0x13, 0x70] => Self::BM1370,
            [0x13, 0x97] => Self::BM1397,
            _ => Self::Unknown(bytes),
        }
    }
}

impl From<ChipType> for [u8; 2] {
    fn from(chip_type: ChipType) -> Self {
        chip_type.id_bytes()
    }
}

/// Nonce range configuration for work distribution.
///
/// NOTE: We store this as a byte array rather than interpreting it as a u32
/// because the exact bit-level interpretation is still being reverse-engineered.
/// The values below are empirically observed from production hardware.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NonceRangeConfig {
    /// Raw bytes as sent over the wire
    bytes: [u8; 4],
}

impl NonceRangeConfig {
    // Nonce range values for different chain lengths (captured from hardware)
    const SINGLE_CHIP: [u8; 4] = [0xff, 0xff, 0xff, 0xff];
    const SMALL_CHAIN: [u8; 4] = [0xff, 0xff, 0xff, 0x1f]; // 2-8 chips
    const MEDIUM_CHAIN: [u8; 4] = [0xff, 0xff, 0xff, 0x0f]; // 9-16 chips
    const LARGE_CHAIN: [u8; 4] = [0xff, 0xff, 0xff, 0x07]; // 17-32 chips
    const XLARGE_CHAIN: [u8; 4] = [0xff, 0xff, 0xff, 0x03]; // 33-64 chips
    const S21_PRO: [u8; 4] = [0x00, 0x00, 0x1e, 0xb5]; // 65-128 chips (empirical)
    const DEFAULT_LARGE: [u8; 4] = [0xff, 0xff, 0xff, 0x01]; // >128 chips

    /// Create config for single chip (full range)
    pub fn single_chip() -> Self {
        Self {
            bytes: Self::SINGLE_CHIP,
        }
    }

    /// Create config for multi-chip chain
    pub fn multi_chip(chain_length: usize) -> Self {
        let bytes = match chain_length {
            1 => Self::SINGLE_CHIP,
            2..=8 => Self::SMALL_CHAIN,
            9..=16 => Self::MEDIUM_CHAIN,
            17..=32 => Self::LARGE_CHAIN,
            33..=64 => Self::XLARGE_CHAIN,
            65..=128 => Self::S21_PRO,
            _ => Self::DEFAULT_LARGE,
        };
        Self { bytes }
    }
}

impl From<NonceRangeConfig> for [u8; 4] {
    fn from(config: NonceRangeConfig) -> Self {
        config.bytes
    }
}

/// Difficulty mask for share submission
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DifficultyMask {
    /// Each byte is bit-reversed
    bytes: [u8; 4],
}

impl DifficultyMask {
    /// Create from difficulty value
    pub fn from_difficulty(difficulty: u32) -> Self {
        // For now, simple mapping
        // Difficulty 256 means the first byte (most significant) should be 0xff
        let bytes = match difficulty {
            256 => [0x00, 0x00, 0x00, 0xff], // As seen in captures
            _ => [0xff, 0xff, 0xff, 0x00],
        };
        Self { bytes }
    }
}

impl From<DifficultyMask> for [u8; 4] {
    fn from(mask: DifficultyMask) -> Self {
        mask.bytes // Already in the correct format
    }
}

/// UART baud rate configuration
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BaudRate {
    /// 115200 baud
    Baud115200,
    /// 1 Mbaud
    Baud1M,
    /// 3 Mbaud (common for multi-chip)
    Baud3M,
    /// Custom baud rate with raw register value
    Custom(u32),
}

impl From<BaudRate> for [u8; 4] {
    fn from(baud: BaudRate) -> Self {
        let value = match baud {
            BaudRate::Baud115200 => 0x00000271, // From ESP-miner
            BaudRate::Baud1M => 0x00000130,
            BaudRate::Baud3M => 0x00003001, // From captures
            BaudRate::Custom(val) => val,
        };
        value.to_le_bytes()
    }
}

/// IO driver strength configuration
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IoDriverStrength {
    /// Drive strength for each signal group (4 bits each)
    strengths: [u8; 8],
}

impl IoDriverStrength {
    /// Normal strength for chips in middle of chain
    pub fn normal() -> Self {
        // 0x11110100 = 0001 0001 0001 0001 0000 0001 0000 0000
        Self {
            strengths: [0x0, 0x0, 0x1, 0x0, 0x1, 0x1, 0x1, 0x1],
        }
    }

    /// Strong drive for domain boundary chips
    pub fn domain_boundary() -> Self {
        // 0x1111f100 = 0001 0001 0001 0001 1111 0001 0000 0000
        Self {
            strengths: [0x0, 0x0, 0x1, 0xf, 0x1, 0x1, 0x1, 0x1],
        }
    }
}

impl From<IoDriverStrength> for [u8; 4] {
    fn from(strength: IoDriverStrength) -> Self {
        // Pack 8 4-bit values into 4 bytes (2 per byte)
        // Each byte contains two strength values: [high_nibble|low_nibble]
        [
            strength.strengths[0] | (strength.strengths[1] << 4),
            strength.strengths[2] | (strength.strengths[3] << 4),
            strength.strengths[4] | (strength.strengths[5] << 4),
            strength.strengths[6] | (strength.strengths[7] << 4),
        ]
    }
}

impl IoDriverStrength {
    /// Get the raw bytes for testing
    pub fn as_bytes(&self) -> [u8; 4] {
        (*self).into()
    }
}

/// Version mask for version rolling
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VersionMask {
    /// Which bits can be rolled
    mask: u16,
    /// Enable flag and other control bits
    control: u16,
}

impl VersionMask {
    /// Full 16-bit mask for version rolling
    const FULL_MASK: u16 = 0xffff;
    /// Control bits to enable version rolling
    const CONTROL_ENABLE: u16 = 0x0090;

    /// Create version mask with all lower 16 bits enabled
    pub fn full_rolling() -> Self {
        Self {
            mask: Self::FULL_MASK,
            control: Self::CONTROL_ENABLE,
        }
    }
}

impl From<VersionMask> for [u8; 4] {
    fn from(mask: VersionMask) -> Self {
        let mut bytes = [0u8; 4];
        bytes[0..2].copy_from_slice(&mask.control.to_le_bytes());
        bytes[2..4].copy_from_slice(&mask.mask.to_le_bytes());
        bytes
    }
}

#[derive(FromRepr, Copy, Clone, Debug)]
#[repr(u8)]
pub enum RegisterAddress {
    ChipId = 0x00,
    PllDivider = 0x08,
    NonceRange = 0x10,
    TicketMask = 0x14,
    MiscControl = 0x18,
    UartBaud = 0x28,
    UartRelay = 0x2C,
    CoreRegister = 0x3C,
    AnalogMux = 0x54,
    IoDriverStrength = 0x58,
    Pll3Parameter = 0x68,
    VersionMask = 0xA4,
    InitControl = 0xA8,
    MiscSettings = 0xB9,
}

#[derive(Debug, Clone)]
pub enum Register {
    ChipId {
        chip_type: ChipType, // Chip type identifier
        core_count: u8,      // Core configuration byte
        address: u8,         // Assigned chip address
    },
    PllDivider(PllConfig),
    NonceRange(NonceRangeConfig),
    TicketMask(DifficultyMask),
    MiscControl {
        raw_value: u32,
    },
    UartBaud(BaudRate),
    UartRelay {
        raw_value: u32, // Domain relay configuration (complex format)
    },
    CoreRegister {
        raw_value: u32,
    },
    AnalogMux {
        raw_value: u32,
    },
    IoDriverStrength(IoDriverStrength),
    Pll3Parameter {
        raw_value: u32,
    },
    VersionMask(VersionMask),
    InitControl {
        raw_value: u32,
    },
    MiscSettings {
        raw_value: u32,
    },
}

impl Register {
    fn decode(address: RegisterAddress, bytes: &[u8; 4]) -> Register {
        let raw_value = u32::from_le_bytes(*bytes);
        match address {
            RegisterAddress::ChipId => Register::ChipId {
                chip_type: ChipType::from([bytes[0], bytes[1]]),
                core_count: bytes[2],
                address: bytes[3],
            },
            RegisterAddress::PllDivider => Register::PllDivider(raw_value.into()),
            RegisterAddress::NonceRange => Register::NonceRange(NonceRangeConfig { bytes: *bytes }),
            RegisterAddress::TicketMask => Register::TicketMask(DifficultyMask { bytes: *bytes }),
            RegisterAddress::MiscControl => Register::MiscControl { raw_value },
            RegisterAddress::UartBaud => {
                // Decode known baud rates
                let baud = match raw_value {
                    0x00000271 => BaudRate::Baud115200,
                    0x00000130 => BaudRate::Baud1M,
                    0x00003001 => BaudRate::Baud3M,
                    other => BaudRate::Custom(other),
                };
                Register::UartBaud(baud)
            }
            RegisterAddress::UartRelay => Register::UartRelay { raw_value },
            RegisterAddress::CoreRegister => Register::CoreRegister { raw_value },
            RegisterAddress::AnalogMux => Register::AnalogMux { raw_value },
            RegisterAddress::IoDriverStrength => {
                // Parse driver strength from raw value
                let mut strengths = [0u8; 8];
                for i in 0..8 {
                    strengths[i] = ((raw_value >> (i * 4)) & 0xf) as u8;
                }
                Register::IoDriverStrength(IoDriverStrength { strengths })
            }
            RegisterAddress::Pll3Parameter => Register::Pll3Parameter { raw_value },
            RegisterAddress::VersionMask => {
                let mask = (raw_value >> 16) as u16;
                let control = (raw_value & 0xffff) as u16;
                Register::VersionMask(VersionMask { mask, control })
            }
            RegisterAddress::InitControl => Register::InitControl { raw_value },
            RegisterAddress::MiscSettings => Register::MiscSettings { raw_value },
        }
    }

    /// Get the register address for this register
    fn address(&self) -> RegisterAddress {
        match self {
            Register::ChipId { .. } => RegisterAddress::ChipId,
            Register::PllDivider(_) => RegisterAddress::PllDivider,
            Register::NonceRange(_) => RegisterAddress::NonceRange,
            Register::TicketMask(_) => RegisterAddress::TicketMask,
            Register::MiscControl { .. } => RegisterAddress::MiscControl,
            Register::UartBaud(_) => RegisterAddress::UartBaud,
            Register::UartRelay { .. } => RegisterAddress::UartRelay,
            Register::CoreRegister { .. } => RegisterAddress::CoreRegister,
            Register::AnalogMux { .. } => RegisterAddress::AnalogMux,
            Register::IoDriverStrength(_) => RegisterAddress::IoDriverStrength,
            Register::Pll3Parameter { .. } => RegisterAddress::Pll3Parameter,
            Register::VersionMask(_) => RegisterAddress::VersionMask,
            Register::InitControl { .. } => RegisterAddress::InitControl,
            Register::MiscSettings { .. } => RegisterAddress::MiscSettings,
        }
    }

    /// Encode the register data (not the address)
    fn encode_data(&self, dst: &mut BytesMut) {
        match self {
            Register::ChipId {
                chip_type,
                core_count,
                address,
            } => {
                dst.put_slice(&chip_type.id_bytes());
                dst.put_u8(*core_count);
                dst.put_u8(*address);
            }
            Register::PllDivider(config) => {
                let bytes: [u8; 4] = (*config).into();
                dst.put_slice(&bytes);
            }
            Register::NonceRange(config) => {
                let bytes: [u8; 4] = (*config).into();
                dst.put_slice(&bytes);
            }
            Register::TicketMask(mask) => {
                let bytes: [u8; 4] = (*mask).into();
                dst.put_slice(&bytes);
            }
            Register::UartBaud(baud) => {
                let bytes: [u8; 4] = (*baud).into();
                dst.put_slice(&bytes);
            }
            Register::MiscControl { raw_value }
            | Register::UartRelay { raw_value }
            | Register::CoreRegister { raw_value }
            | Register::AnalogMux { raw_value }
            | Register::Pll3Parameter { raw_value }
            | Register::InitControl { raw_value }
            | Register::MiscSettings { raw_value } => {
                dst.put_u32_le(*raw_value);
            }
            Register::IoDriverStrength(strength) => {
                let bytes: [u8; 4] = (*strength).into();
                dst.put_slice(&bytes);
            }
            Register::VersionMask(mask) => {
                let bytes: [u8; 4] = (*mask).into();
                dst.put_slice(&bytes);
            }
        }
    }
}

#[repr(u8)]
enum CommandFlagsType {
    Job = 1,
    Command = 2,
}

#[repr(u8)]
enum CommandFlagsCmd {
    SetChipAddress = 0,
    WriteRegisterOrJob = 1,
    ReadRegister = 2,
    ChainInactive = 3,
}

#[derive(Debug)]
#[expect(
    dead_code,
    reason = "JobMidstate variant will be used for BM1397 support"
)]
pub enum Command {
    /// Set address for a chip in the chain
    SetChipAddress { chip_address: u8 },
    /// Prepare chain for address assignment
    ChainInactive,
    /// Read a register from chip(s)
    ReadRegister {
        all: bool,
        chip_address: u8,
        register_address: RegisterAddress,
    },
    /// Write a register to chip(s)
    WriteRegister {
        all: bool,
        chip_address: u8,
        register: Register,
    },
    /// Send a job with full block header (BM1370/BM1362 style)
    /// Chip calculates midstates internally
    JobFull { job_data: JobFullFormat },
    /// Send a job with pre-calculated midstates (BM1397 style)
    /// Host calculates midstates to save chip computation
    JobMidstate { job_data: JobMidstateFormat },
}

/// Full format job structure (BM1370/BM1362).
/// The chip calculates midstates internally from the full block header.
/// All multi-byte values are little-endian in the structure.
/// Hash values (merkle_root, prev_block_hash) are stored in big-endian format.
#[derive(Debug, Clone)]
pub struct JobFullFormat {
    pub job_id: u8,
    pub num_midstates: u8, // Typically 0x01 for BM1370
    pub starting_nonce: [u8; 4],
    pub nbits: [u8; 4],            // Difficulty target
    pub ntime: [u8; 4],            // Timestamp
    pub merkle_root: [u8; 32],     // Full merkle root (big-endian)
    pub prev_block_hash: [u8; 32], // Full previous block hash (big-endian)
    pub version: [u8; 4],          // Block version for version rolling
}

/// Midstate format job structure (BM1397).
/// Host pre-calculates SHA256 midstates to reduce chip workload.
/// Supports up to 4 midstates for version rolling.
#[derive(Debug, Clone)]
pub struct JobMidstateFormat {
    pub job_id: u8,
    pub num_midstates: u8, // 1 or 4 typically
    pub starting_nonce: [u8; 4],
    pub nbits: [u8; 4],              // Difficulty target
    pub ntime: [u8; 4],              // Timestamp
    pub merkle4: [u8; 4],            // Last 4 bytes of merkle root
    pub midstate0: [u8; 32],         // Primary midstate
    pub midstate1: Option<[u8; 32]>, // Optional for version rolling
    pub midstate2: Option<[u8; 32]>, // Optional for version rolling
    pub midstate3: Option<[u8; 32]>, // Optional for version rolling
}

impl Command {
    fn build_flags(typ: CommandFlagsType, all: bool, cmd: CommandFlagsCmd) -> u8 {
        let mut flags = 0u8;
        let field = flags.view_bits_mut::<Lsb0>();
        field[5..7].store(typ as u8);
        field[4..5].store(all as u8);
        field[0..4].store(cmd as u8);
        flags
    }

    fn encode(&self, dst: &mut BytesMut) {
        match self {
            Command::SetChipAddress { chip_address } => {
                dst.put_u8(Self::build_flags(
                    CommandFlagsType::Command,
                    false, // Never broadcast
                    CommandFlagsCmd::SetChipAddress,
                ));

                const FLAGS_LEN: u8 = 1;
                const CHIP_ADDR_LEN: u8 = 1;
                const REG_ADDR_LEN: u8 = 1; // Always 0x00 for set address
                const LENGTH_FIELD_LEN: u8 = 1;
                const CRC_LEN: u8 = 1;
                const TOTAL_LEN: u8 =
                    FLAGS_LEN + LENGTH_FIELD_LEN + CHIP_ADDR_LEN + REG_ADDR_LEN + CRC_LEN;

                dst.put_u8(TOTAL_LEN);
                dst.put_u8(*chip_address);
                dst.put_u8(0x00); // Register address is always 0x00
            }
            Command::ChainInactive => {
                dst.put_u8(Self::build_flags(
                    CommandFlagsType::Command,
                    true, // Always broadcast
                    CommandFlagsCmd::ChainInactive,
                ));

                // From capture: 55 AA 53 05 00 00 03
                // Length field (0x05) includes everything after preamble except itself
                const FLAGS_LEN: u8 = 1; // 0x53
                const CHIP_ADDR_LEN: u8 = 1; // 0x00
                const REG_ADDR_LEN: u8 = 1; // 0x00
                const CRC_LEN: u8 = 1; // 0x03
                const TOTAL_LEN: u8 = FLAGS_LEN + CHIP_ADDR_LEN + REG_ADDR_LEN + CRC_LEN + 1; // +1 for length field

                dst.put_u8(TOTAL_LEN);
                dst.put_u8(0x00); // Chip address
                dst.put_u8(0x00); // Register address
            }
            Command::ReadRegister {
                all,
                chip_address,
                register_address,
            } => {
                dst.put_u8(Self::build_flags(
                    CommandFlagsType::Command,
                    *all,
                    CommandFlagsCmd::ReadRegister,
                ));

                const FLAGS_LEN: u8 = 1;
                const CHIP_ADDR_LEN: u8 = 1;
                const REG_ADDR_LEN: u8 = 1;
                const LENGTH_FIELD_LEN: u8 = 1;
                const CRC_LEN: u8 = 1;
                const TOTAL_LEN: u8 =
                    FLAGS_LEN + LENGTH_FIELD_LEN + CHIP_ADDR_LEN + REG_ADDR_LEN + CRC_LEN;

                dst.put_u8(TOTAL_LEN);
                dst.put_u8(*chip_address);
                dst.put_u8(*register_address as u8);
            }
            Command::WriteRegister {
                all,
                chip_address,
                register,
            } => {
                dst.put_u8(Self::build_flags(
                    CommandFlagsType::Command,
                    *all,
                    CommandFlagsCmd::WriteRegisterOrJob,
                ));

                const FLAGS_LEN: u8 = 1;
                const CHIP_ADDR_LEN: u8 = 1;
                const REG_ADDR_LEN: u8 = 1;
                const REG_DATA_LEN: u8 = 4;
                const LENGTH_FIELD_LEN: u8 = 1;
                const CRC_LEN: u8 = 1;
                const TOTAL_LEN: u8 = FLAGS_LEN
                    + LENGTH_FIELD_LEN
                    + CHIP_ADDR_LEN
                    + REG_ADDR_LEN
                    + REG_DATA_LEN
                    + CRC_LEN;

                dst.put_u8(TOTAL_LEN);
                dst.put_u8(*chip_address);
                dst.put_u8(register.address() as u8);
                register.encode_data(dst);
            }
            Command::JobFull { job_data } => {
                dst.put_u8(Self::build_flags(
                    CommandFlagsType::Job,
                    false, // Jobs are never broadcast
                    CommandFlagsCmd::WriteRegisterOrJob,
                ));

                const JOB_DATA_LEN: u8 = 82; // Size of JobFullFormat
                const FLAGS_LEN: u8 = 1;
                const LENGTH_FIELD_LEN: u8 = 1;
                const CRC_LEN: u8 = 2; // Jobs use CRC16, not CRC5
                const TOTAL_LEN: u8 = FLAGS_LEN + LENGTH_FIELD_LEN + JOB_DATA_LEN + CRC_LEN;

                dst.put_u8(TOTAL_LEN);

                // Write job data
                dst.put_u8(job_data.job_id);
                dst.put_u8(job_data.num_midstates);
                dst.put_slice(&job_data.starting_nonce);
                dst.put_slice(&job_data.nbits);
                dst.put_slice(&job_data.ntime);
                dst.put_slice(&job_data.merkle_root);
                dst.put_slice(&job_data.prev_block_hash);
                dst.put_slice(&job_data.version);
            }
            Command::JobMidstate { job_data } => {
                dst.put_u8(Self::build_flags(
                    CommandFlagsType::Job,
                    false, // Jobs are never broadcast
                    CommandFlagsCmd::WriteRegisterOrJob,
                ));

                // Calculate data length based on number of midstates
                const BASE_LEN: u8 = 18; // job_id(1) + num_midstates(1) + nonce(4) + nbits(4) + ntime(4) + merkle4(4)
                const MIDSTATE_LEN: u8 = 32;
                let data_len = BASE_LEN + (job_data.num_midstates * MIDSTATE_LEN);

                const FLAGS_LEN: u8 = 1;
                const LENGTH_FIELD_LEN: u8 = 1;
                const CRC_LEN: u8 = 2; // Jobs use CRC16
                let total_len = FLAGS_LEN + LENGTH_FIELD_LEN + data_len + CRC_LEN;

                dst.put_u8(total_len);

                // Write job data
                dst.put_u8(job_data.job_id);
                dst.put_u8(job_data.num_midstates);
                dst.put_slice(&job_data.starting_nonce);
                dst.put_slice(&job_data.nbits);
                dst.put_slice(&job_data.ntime);
                dst.put_slice(&job_data.merkle4);
                dst.put_slice(&job_data.midstate0);

                // Write optional midstates
                if let Some(midstate) = &job_data.midstate1 {
                    dst.put_slice(midstate);
                }
                if let Some(midstate) = &job_data.midstate2 {
                    dst.put_slice(midstate);
                }
                if let Some(midstate) = &job_data.midstate3 {
                    dst.put_slice(midstate);
                }
            }
        }
    }
}

#[derive(FromRepr)]
#[repr(u8)]
enum ResponseType {
    ReadRegister = 0,
    Nonce = 4,
}

#[derive(Debug)]
#[cfg_attr(not(test), allow(dead_code))]
pub enum Response {
    ReadRegister {
        chip_address: u8,
        register: Register,
    },
    Nonce {
        nonce: u32,
        job_id: u8,
        midstate_num: u8,
        version: u16,
        subcore_id: u8,
    },
}

impl Response {
    fn decode(bytes: &mut BytesMut) -> Result<Response, ProtocolError> {
        let type_and_crc = bytes[bytes.len() - 1].view_bits::<Lsb0>();
        let type_repr = type_and_crc[5..].load::<u8>();

        match ResponseType::from_repr(type_repr) {
            Some(ResponseType::ReadRegister) => {
                let value_bytes = bytes.split_to(4);
                let value: [u8; 4] =
                    value_bytes[..]
                        .try_into()
                        .map_err(|_| ProtocolError::BufferTooSmall {
                            need: 4,
                            have: value_bytes.len(),
                        })?;
                let chip_address = bytes.get_u8();
                let register_address_repr = bytes.get_u8();

                if let Some(register_address) = RegisterAddress::from_repr(register_address_repr) {
                    let register = Register::decode(register_address, &value);
                    Ok(Response::ReadRegister {
                        chip_address,
                        register,
                    })
                } else {
                    Err(ProtocolError::InvalidRegisterAddress(register_address_repr))
                }
            }
            Some(ResponseType::Nonce) => {
                // BM1370 nonce response format (11 bytes total, including preamble):
                // Already consumed: preamble (2 bytes)
                // Remaining: nonce(4) + midstate_num(1) + result_header(1) + version(2) + crc(1)
                let nonce = bytes.get_u32_le();
                let midstate_num = bytes.get_u8();
                let result_header = bytes.get_u8();
                let version = bytes.get_u16_le();
                // CRC already consumed

                // Extract job_id (bits 7-4) and subcore_id (bits 3-0) from result_header
                let job_id = (result_header >> 4) & 0x0f;
                let subcore_id = result_header & 0x0f;

                Ok(Response::Nonce {
                    nonce,
                    job_id,
                    midstate_num,
                    version,
                    subcore_id,
                })
            }
            None => Err(ProtocolError::InvalidResponseType(type_repr)),
        }
    }
}

#[derive(Default)]
pub struct FrameCodec {
    last_buffer_size: usize,
}

impl Encoder<Command> for FrameCodec {
    type Error = io::Error;

    fn encode(&mut self, command: Command, dst: &mut BytesMut) -> Result<(), Self::Error> {
        const PREAMBLE: [u8; 2] = [0x55, 0xaa];
        dst.put_slice(&PREAMBLE);

        let start_pos = dst.len();
        command.encode(dst);

        // Jobs use CRC16, other commands use CRC5
        match &command {
            Command::JobFull { .. } | Command::JobMidstate { .. } => {
                // Calculate CRC16 over flags + length + data
                let crc = crc16(&dst[start_pos..]);
                dst.put_u16_le(crc);
            }
            _ => {
                // Calculate CRC5 over everything after preamble
                let crc = crc5(&dst[2..]);
                dst.put_u8(crc);
            }
        }

        // Log the encoded frame for debugging
        let frame_bytes = &dst[dst.len() - (dst.len() - start_pos + 2)..];
        trace!(
            "TX: {:?} ({} bytes) => {:02x?}",
            command,
            frame_bytes.len(),
            frame_bytes
        );

        Ok(())
    }
}

impl Decoder for FrameCodec {
    type Item = Response;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        // Return Ok(Item) with a valid frame, or Ok(None) if to be called again, potentially with
        // more data. Returning an Error causes the stream to be terminated, so don't do that.
        //
        // There are three cases:
        //
        // 1. More data needed
        // 2. Invalid frame
        // 3. Valid frame
        //
        // In the case of an invalid frame, consume the first byte and request another call by
        // returning Ok(None). In the case of a valid frame, consume that frame's worth of bytes.

        const PREAMBLE: [u8; 2] = [0xaa, 0x55];
        // All BM13xx responses are 11 bytes (2 preamble + 9 data)
        const FRAME_LEN: usize = PREAMBLE.len() + 9;
        const CALL_AGAIN: Result<Option<Response>, io::Error> = Ok(None);

        // Log significant buffer changes
        if src.len() != self.last_buffer_size {
            if src.len() > self.last_buffer_size + 5 || // Growing significantly
               (self.last_buffer_size >= FRAME_LEN && src.len() < FRAME_LEN)
            {
                // Dropped below frame size
                trace!(
                    "Decoder buffer: {} → {} bytes ({})",
                    self.last_buffer_size,
                    src.len(),
                    if src.len() > self.last_buffer_size {
                        "growing"
                    } else {
                        "shrinking"
                    }
                );
            }
            self.last_buffer_size = src.len();
        }

        if src.len() < FRAME_LEN {
            return CALL_AGAIN;
        }

        // Check preamble without consuming the buffer
        if src[0] != PREAMBLE[0] {
            src.advance(1);
            return CALL_AGAIN;
        }

        if src[1] != PREAMBLE[1] {
            src.advance(1);
            return CALL_AGAIN;
        }

        // Validate CRC5 over the entire frame (excluding preamble)
        // CRC5 is computed over the 9 data bytes after the preamble
        if !crc5_is_valid(&src[2..FRAME_LEN]) {
            trace!(
                "Frame sync lost: CRC5 failed for potential frame at position 0. Searching for next frame..."
            );
            src.advance(1);
            return CALL_AGAIN;
        }

        // We have a valid frame with correct CRC
        // Save the frame bytes before consuming
        let frame_bytes = src[..FRAME_LEN].to_vec();

        // Create a buffer for decoding
        let mut decode_buf = BytesMut::from(&src[..FRAME_LEN]);
        decode_buf.advance(2); // Skip preamble for Response::decode

        match Response::decode(&mut decode_buf) {
            Ok(response) => {
                // Only advance if decode was successful
                src.advance(FRAME_LEN);

                // Log the received frame for debugging
                trace!(
                    "RX: {:?} ({} bytes) => {:02x?}",
                    response,
                    FRAME_LEN,
                    frame_bytes
                );
                Ok(Some(response))
            }
            Err(err) => {
                warn!("Failed to decode response: {}", err);
                // Advance by 1 to try to find next valid frame
                src.advance(1);
                CALL_AGAIN
            }
        }
    }
}

#[cfg(test)]
mod init_tests {
    use super::*;

    #[test]
    fn multi_chip_init_sequence() {
        let protocol = BM13xxProtocol::new();
        let commands = protocol.multi_chip_init(65); // S21 Pro has 65 chips

        // Verify the sequence starts with version rolling enable
        assert!(matches!(
            &commands[0],
            Command::WriteRegister {
                all: true,
                chip_address: 0x00,
                register: Register::VersionMask(_),
            }
        ));

        // Verify chain inactive command
        let chain_inactive_pos = commands
            .iter()
            .position(|c| matches!(c, Command::ChainInactive))
            .expect("ChainInactive command not found in initialization sequence");
        assert!(chain_inactive_pos > 0);

        // Verify chip addressing starts after chain inactive
        let first_address_pos = chain_inactive_pos + 1;
        assert!(matches!(
            &commands[first_address_pos],
            Command::SetChipAddress { chip_address: 0x00 }
        ));

        // Verify we have 65 address assignments
        let address_commands: Vec<_> = commands[first_address_pos..first_address_pos + 65]
            .iter()
            .collect();
        assert_eq!(address_commands.len(), 65);

        // Verify addresses increment by 2
        for (i, cmd) in address_commands.iter().enumerate() {
            match cmd {
                Command::SetChipAddress { chip_address } => {
                    assert_eq!(*chip_address, (i * 2) as u8);
                }
                _ => panic!("Expected SetChipAddress command, got {:?}", cmd),
            }
        }
    }

    #[test]
    fn domain_configuration() {
        let protocol = BM13xxProtocol::new();
        let commands = protocol.configure_domains(65, 5); // 65 chips, 5 per domain

        // Should have 13 domains
        let io_strength_commands: Vec<_> = commands
            .iter()
            .filter(|c| {
                matches!(
                    c,
                    Command::WriteRegister {
                        register: Register::IoDriverStrength { .. },
                        ..
                    }
                )
            })
            .collect();
        assert_eq!(io_strength_commands.len(), 13);

        // Check first domain boundary (chip 8 = address 0x08)
        let first_boundary = io_strength_commands[0];
        if let Command::WriteRegister {
            chip_address,
            register: Register::IoDriverStrength(strength),
            ..
        } = first_boundary
        {
            assert_eq!(*chip_address, 0x08); // 5th chip (index 4) * 2
            let strength_bytes: [u8; 4] = (*strength).into();
            // Expected bytes from hardware capture
            assert_eq!(strength_bytes, [0x00, 0xf1, 0x11, 0x11]);
        }
    }

    #[test]
    fn frequency_ramp_sequence() {
        let protocol = BM13xxProtocol::new();
        let start = Frequency::from_mhz(400.0).unwrap();
        let target = Frequency::from_mhz(600.0).unwrap();
        let commands = protocol.frequency_ramp(start, target, 5);

        assert_eq!(commands.len(), 5);

        // Verify it's a gradual increase - check that PLL configs are generated
        for cmd in commands.iter() {
            assert!(matches!(
                cmd,
                Command::WriteRegister {
                    register: Register::PllDivider(_),
                    all: true,
                    ..
                }
            ));
        }
    }

    #[test]
    fn pll_calculation_produces_valid_frequencies() {
        // Test cases from serial captures showing PLL values sent by esp-miner
        // Note: esp-miner uses first-found algorithm while we find optimal settings
        // Format: (target_mhz, [fb_div, ref_div, post_div] from esp-miner)
        let test_cases = vec![
            (62.5, [0xd2, 0x02, 0x65]),  // 62.50MHz
            (75.0, [0xd2, 0x02, 0x64]),  // 75.00MHz
            (100.0, [0xe0, 0x02, 0x63]), // 100.00MHz
            (400.0, [0xe0, 0x02, 0x60]), // 400.00MHz
            (500.0, [0xa2, 0x02, 0x30]), // 500.00MHz -> esp-miner gives 506.25MHz
        ];

        for (target_mhz, esp_miner_raw) in test_cases {
            let freq = Frequency::from_mhz(target_mhz).unwrap();
            let pll = freq.calculate_pll();

            // Calculate actual frequencies for both esp-miner and our values
            let esp_post_div1 = ((esp_miner_raw[2] >> 4) & 0xf) + 1;
            let esp_post_div2 = (esp_miner_raw[2] & 0xf) + 1;
            let esp_actual_mhz = 25.0 * esp_miner_raw[0] as f32
                / (esp_miner_raw[1] as f32 * esp_post_div1 as f32 * esp_post_div2 as f32);

            let our_post_div1 = ((pll.post_div >> 4) & 0xf) + 1;
            let our_post_div2 = (pll.post_div & 0xf) + 1;
            let our_actual_mhz = 25.0 * pll.fb_div as f32
                / (pll.ref_div as f32 * our_post_div1 as f32 * our_post_div2 as f32);

            // Calculate errors
            let esp_error = (target_mhz - esp_actual_mhz).abs();
            let our_error = (target_mhz - our_actual_mhz).abs();

            println!("Target: {:.2}MHz", target_mhz);
            println!(
                "  esp-miner: fb={:#04x} ref={} post={:#04x} -> {:.2}MHz (error: {:.4}MHz)",
                esp_miner_raw[0], esp_miner_raw[1], esp_miner_raw[2], esp_actual_mhz, esp_error
            );
            println!(
                "  Our calc:  fb={:#04x} ref={} post={:#04x} -> {:.2}MHz (error: {:.4}MHz)",
                pll.fb_div, pll.ref_div, pll.post_div, our_actual_mhz, our_error
            );

            // Verify our calculation produces valid PLL parameters
            assert!(
                pll.fb_div >= 0xa0 && pll.fb_div <= 0xef,
                "fb_div out of range: {:#04x}",
                pll.fb_div
            );
            assert!(
                pll.ref_div == 1 || pll.ref_div == 2,
                "ref_div invalid: {}",
                pll.ref_div
            );

            // Verify our error is reasonable (within 1MHz)
            assert!(
                our_error < 1.0,
                "Frequency error too large: {:.2}MHz for target {}MHz",
                our_error,
                target_mhz
            );

            // Our algorithm should produce equal or better results
            // Allow small tolerance for floating point comparison
            assert!(
                our_error <= esp_error + 0.01,
                "Our algorithm produced worse result than esp-miner for {}MHz",
                target_mhz
            );
        }
    }

    #[test]
    fn nonce_range_configuration() {
        let protocol = BM13xxProtocol::new();

        // Test single chip - full range
        let commands = protocol.configure_nonce_ranges(1);
        assert_eq!(commands.len(), 1);
        if let Command::WriteRegister {
            register: Register::NonceRange(config),
            all: true,
            ..
        } = &commands[0]
        {
            let config_bytes: [u8; 4] = (*config).into();
            assert_eq!(config_bytes, [0xff, 0xff, 0xff, 0xff]);
        }

        // Test S21 Pro configuration (65 chips)
        let commands = protocol.configure_nonce_ranges(65);
        assert_eq!(commands.len(), 1);
        if let Command::WriteRegister {
            register: Register::NonceRange(config),
            ..
        } = &commands[0]
        {
            let config_bytes: [u8; 4] = (*config).into();
            assert_eq!(config_bytes, [0x00, 0x00, 0x1e, 0xb5]);
        }

        // Test small chain
        let commands = protocol.configure_nonce_ranges(8);
        if let Command::WriteRegister {
            register: Register::NonceRange(config),
            ..
        } = &commands[0]
        {
            let config_bytes: [u8; 4] = (*config).into();
            assert_eq!(config_bytes, [0xff, 0xff, 0xff, 0x1f]);
        }
    }

    #[test]
    fn job_distribution() {
        use crate::asic::MiningJob;

        let protocol = BM13xxProtocol::new();

        // Create a test header
        let mut header = [0u8; 80];
        // Version (0x20000000 in little-endian)
        header[0..4].copy_from_slice(&[0x00, 0x00, 0x00, 0x20]);
        // Previous block hash
        header[4..36].copy_from_slice(&[0x00; 32]);
        // Merkle root
        header[36..68].copy_from_slice(&[0x11; 32]);
        // ntime (0x6767675c in little-endian)
        header[68..72].copy_from_slice(&[0x5c, 0x67, 0x67, 0x67]);
        // nbits (0x170e3ab4 in little-endian)
        header[72..76].copy_from_slice(&[0xb4, 0x3a, 0x0e, 0x17]);
        // nonce (placeholder)
        header[76..80].copy_from_slice(&[0x00; 4]);

        let job = MiningJob::from_header(
            123, header, [0xff; 32], // target
            0,          // nonce_start
            0xffffffff, // nonce_range
        );

        let commands = protocol.distribute_job(&job, 65, 0x18);

        // Should have one broadcast job command
        assert_eq!(commands.len(), 1);
        assert!(matches!(&commands[0], Command::JobFull { .. }));
    }

    #[test]
    fn multi_chip_init_includes_nonce_range() {
        let protocol = BM13xxProtocol::new();
        let commands = protocol.multi_chip_init(65);

        // Find the nonce range configuration
        let nonce_range_cmd = commands.iter().find(|c| {
            matches!(
                c,
                Command::WriteRegister {
                    register: Register::NonceRange { .. },
                    ..
                }
            )
        });

        assert!(nonce_range_cmd.is_some());

        if let Some(Command::WriteRegister {
            register: Register::NonceRange(config),
            ..
        }) = nonce_range_cmd
        {
            let config_bytes: [u8; 4] = (*config).into();
            assert_eq!(config_bytes, [0x00, 0x00, 0x1e, 0xb5]); // S21 Pro value
        }
    }
}

#[cfg(test)]
mod command_tests {
    use super::*;

    #[test]
    fn read_register() {
        assert_frame_eq(
            Command::ReadRegister {
                all: true,
                chip_address: 0,
                register_address: RegisterAddress::ChipId,
            },
            &[0x55, 0xaa, 0x52, 0x05, 0x00, 0x00, 0x0a],
        );
    }

    #[test]
    fn write_register_chip_address() {
        assert_frame_eq(
            Command::WriteRegister {
                all: false,
                chip_address: 0x01,
                register: Register::ChipId {
                    chip_type: ChipType::BM1370,
                    core_count: 0x00,
                    address: 0x01,
                },
            },
            &[
                0x55, 0xaa, 0x41, 0x09, 0x01, 0x00, 0x13, 0x70, 0x00, 0x01, 0x0a,
            ],
        );
    }

    // Tests from actual captures
    #[test]
    fn write_version_mask_from_capture() {
        // From S21 Pro capture: TX: 55 AA 51 09 00 A4 90 00 FF FF 1C
        assert_frame_eq(
            Command::WriteRegister {
                all: true, // 0x51 = broadcast
                chip_address: 0x00,
                register: Register::VersionMask(VersionMask::full_rolling()),
            },
            &[
                0x55, 0xaa, 0x51, 0x09, 0x00, 0xa4, 0x90, 0x00, 0xff, 0xff, 0x1c,
            ],
        );
    }

    #[test]
    fn write_init_control_from_capture() {
        // From Bitaxe capture: TX: 55 AA 51 09 00 A8 00 07 00 00 03
        // Value 0x00 07 00 00 in little-endian = 0x00000700
        assert_frame_eq(
            Command::WriteRegister {
                all: true,
                chip_address: 0x00,
                register: Register::InitControl {
                    raw_value: 0x00000700,
                },
            },
            &[
                0x55, 0xaa, 0x51, 0x09, 0x00, 0xa8, 0x00, 0x07, 0x00, 0x00, 0x03,
            ],
        );
    }

    #[test]
    fn write_misc_control_from_capture() {
        // From Bitaxe capture: TX: 55 AA 51 09 00 18 F0 00 C1 00 04
        assert_frame_eq(
            Command::WriteRegister {
                all: true,
                chip_address: 0x00,
                register: Register::MiscControl {
                    raw_value: 0x00C100F0,
                },
            },
            &[
                0x55, 0xaa, 0x51, 0x09, 0x00, 0x18, 0xf0, 0x00, 0xc1, 0x00, 0x04,
            ],
        );
    }

    #[test]
    fn chain_inactive_from_capture() {
        // From S21 Pro capture: TX: 55 AA 53 05 00 00 03
        assert_frame_eq(
            Command::ChainInactive,
            &[0x55, 0xaa, 0x53, 0x05, 0x00, 0x00, 0x03],
        );
    }

    #[test]
    fn set_chip_address_from_capture() {
        // From S21 Pro capture: TX: 55 AA 40 05 04 00 03 (assign address 0x04)
        assert_frame_eq(
            Command::SetChipAddress { chip_address: 0x04 },
            &[0x55, 0xaa, 0x40, 0x05, 0x04, 0x00, 0x03],
        );
    }

    #[test]
    fn write_core_register_sequence() {
        // From Bitaxe capture: TX: 55 AA 51 09 00 3C 80 00 8B 00 12
        assert_frame_eq(
            Command::WriteRegister {
                all: true,
                chip_address: 0x00,
                register: Register::CoreRegister {
                    raw_value: 0x008B0080,
                },
            },
            &[
                0x55, 0xaa, 0x51, 0x09, 0x00, 0x3c, 0x80, 0x00, 0x8b, 0x00, 0x12,
            ],
        );
    }

    #[test]
    fn write_ticket_mask_from_capture() {
        // From S21 Pro capture: TX: 55 AA 51 09 00 14 00 00 00 FF 08
        assert_frame_eq(
            Command::WriteRegister {
                all: true,
                chip_address: 0x00,
                register: Register::TicketMask(DifficultyMask::from_difficulty(256)),
            },
            &[
                0x55, 0xaa, 0x51, 0x09, 0x00, 0x14, 0x00, 0x00, 0x00, 0xff, 0x08,
            ],
        );
    }

    #[test]
    fn write_nonce_range_from_capture() {
        // From S21 Pro capture: TX: 55 AA 51 09 00 10 00 00 1E B5 0F
        assert_frame_eq(
            Command::WriteRegister {
                all: true,
                chip_address: 0x00,
                register: Register::NonceRange(NonceRangeConfig::multi_chip(65)),
            },
            &[
                0x55, 0xaa, 0x51, 0x09, 0x00, 0x10, 0x00, 0x00, 0x1e, 0xb5, 0x0f,
            ],
        );
    }

    #[test]
    fn job_full_format_encoding() {
        // Test BM1370 job packet encoding
        let job = JobFullFormat {
            job_id: 0x00,
            num_midstates: 0x01,
            starting_nonce: [0x00, 0x00, 0x00, 0x00],
            nbits: [0x17, 0x0e, 0xd6, 0x6a],
            ntime: [0x66, 0x73, 0x8c, 0x20],
            merkle_root: [0xaa; 32],           // Simple test pattern
            prev_block_hash: [0xbb; 32],       // Simple test pattern
            version: [0x00, 0x00, 0x00, 0x20], // Version 32
        };

        let mut codec = FrameCodec::default();
        let mut frame = BytesMut::new();
        codec
            .encode(
                Command::JobFull {
                    job_data: job.clone(),
                },
                &mut frame,
            )
            .expect("Failed to encode job command");

        // Verify packet structure
        assert_eq!(&frame[0..2], &[0x55, 0xaa]); // Preamble
        assert_eq!(frame[2], 0x21); // TYPE_JOB | GROUP_SINGLE | CMD_WRITE
        assert_eq!(frame[3], 86); // Total length
        assert_eq!(frame[4], job.job_id);
        assert_eq!(frame[5], job.num_midstates);
        assert_eq!(&frame[6..10], &job.starting_nonce);
        assert_eq!(&frame[10..14], &job.nbits);
        assert_eq!(&frame[14..18], &job.ntime);
        assert_eq!(&frame[18..50], &job.merkle_root);
        assert_eq!(&frame[50..82], &job.prev_block_hash);
        assert_eq!(&frame[82..86], &job.version);

        // Verify CRC16
        assert_eq!(frame.len(), 88);
        let crc_bytes = &frame[86..88];
        let calculated_crc = crc16(&frame[2..86]);
        let frame_crc = u16::from_le_bytes([crc_bytes[0], crc_bytes[1]]);
        assert_eq!(calculated_crc, frame_crc);
    }

    fn assert_frame_eq(cmd: Command, expect: &[u8]) {
        let mut codec = FrameCodec::default();
        let mut frame = BytesMut::new();
        codec
            .encode(cmd, &mut frame)
            .expect("Failed to encode command for test");

        assert_eq!(
            &frame[..],
            expect,
            "\nFrame mismatch!\nExpected: {}\nActual:   {}",
            as_hex(expect),
            as_hex(&frame[..])
        );
    }

    fn as_hex(bytes: &[u8]) -> String {
        bytes
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<Vec<String>>()
            .join(" ")
    }
}

#[cfg(test)]
mod response_tests {
    use super::*;
    use bytes::BufMut;

    #[test]
    fn verify_crc_calculation() {
        // Test that our known good frame has valid CRC
        let frame = &[0x13, 0x70, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10]; // without preamble
        assert!(
            crc5_is_valid(frame),
            "Known good frame should have valid CRC"
        );
    }

    #[test]
    fn decoder_with_exact_frame_size() {
        let mut codec = FrameCodec::default();

        // Exactly 11 bytes - a complete frame
        let mut buf = BytesMut::new();
        buf.put_slice(&[
            0xaa, 0x55, 0x13, 0x70, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10,
        ]);

        let result = codec.decode(&mut buf).unwrap();
        assert!(
            result.is_some(),
            "Should decode frame when buffer has exactly 11 bytes"
        );
    }

    #[test]
    fn read_register() {
        // 11-byte register read response from captures
        let wire = &[
            0xaa, 0x55, 0x13, 0x70, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10,
        ];
        let response = decode_frame(wire).expect("decode_frame should return Some for valid frame");

        let Response::ReadRegister {
            chip_address,
            register,
        } = response
        else {
            panic!("Expected ReadRegister response, got {:?}", response);
        };

        assert_eq!(chip_address, 0x00);

        let Register::ChipId {
            chip_type,
            core_count,
            address,
        } = register
        else {
            panic!("Expected ChipId register, got {:?}", register);
        };

        assert_eq!(chip_type, ChipType::BM1370);
        assert_eq!(core_count, 0x00);
        assert_eq!(address, 0x00);
    }

    fn decode_frame(frame: &[u8]) -> Option<Response> {
        let mut buf = BytesMut::from(frame);
        let mut codec = FrameCodec::default();
        codec.decode(&mut buf).expect("Failed to decode frame")
    }

    #[test]
    fn decode_nonce_response_from_capture() {
        // From Bitaxe capture: RX: AA 55 18 00 A6 40 02 99 22 F9 91
        let wire = &[
            0xaa, 0x55, 0x18, 0x00, 0xa6, 0x40, 0x02, 0x99, 0x22, 0xf9, 0x91,
        ];
        let response = decode_frame(wire).expect("decode_frame should return Some for valid frame");

        let Response::Nonce {
            nonce,
            job_id,
            midstate_num,
            version,
            subcore_id,
        } = response
        else {
            panic!("Expected nonce response");
        };

        // From protocol doc: nonce 0x40A60018 → Main core 32, nonce value 0x00A60018
        assert_eq!(nonce, 0x40a60018);
        assert_eq!(midstate_num, 0x02);

        // Result header: 0x99 → job_id=9 (bits 7-4), subcore_id=9 (bits 3-0)
        assert_eq!(job_id, 9);
        assert_eq!(subcore_id, 9);

        // Version: 0xF922
        assert_eq!(version, 0xf922);

        // Verify main core extraction
        let main_core = (nonce >> 25) & 0x7f;
        assert_eq!(main_core, 32);
    }

    #[test]
    fn decode_multiple_nonce_responses() {
        // Additional nonce responses from S21 Pro capture
        let test_cases = vec![
            // RX: AA 55 07 35 CD CF 02 5E 00 2E 96
            (
                &[
                    0xaa, 0x55, 0x07, 0x35, 0xcd, 0xcf, 0x02, 0x5e, 0x00, 0x2e, 0x96,
                ],
                0xcfcd3507,
                0x02,
                5,
                14,
                0x2e00,
            ),
            // RX: AA 55 46 03 32 E7 00 C3 2C 83 99
            (
                &[
                    0xaa, 0x55, 0x46, 0x03, 0x32, 0xe7, 0x00, 0xc3, 0x2c, 0x83, 0x99,
                ],
                0xe7320346,
                0x00,
                12,
                3,
                0x832c,
            ),
        ];

        for (wire, exp_nonce, exp_midstate, exp_job_id, exp_subcore, exp_version) in test_cases {
            let response =
                decode_frame(wire).expect("decode_frame should return Some for valid frame");

            let Response::Nonce {
                nonce,
                job_id,
                midstate_num,
                version,
                subcore_id,
            } = response
            else {
                panic!("Expected nonce response");
            };

            assert_eq!(nonce, exp_nonce);
            assert_eq!(midstate_num, exp_midstate);
            assert_eq!(job_id, exp_job_id);
            assert_eq!(subcore_id, exp_subcore);
            assert_eq!(version, exp_version);
        }
    }

    #[test]
    fn decoder_handles_partial_frames() {
        let mut codec = FrameCodec::default();

        // Test with incomplete frame (less than 11 bytes)
        let mut buf = BytesMut::new();
        buf.put_slice(&[0xaa, 0x55, 0x13, 0x70, 0x00]); // Only 5 bytes

        let result = codec.decode(&mut buf).unwrap();
        assert!(result.is_none(), "Should return None for incomplete frame");
        assert_eq!(buf.len(), 5, "Buffer should not be consumed");

        // Add more bytes to complete the frame
        buf.put_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x10]); // Complete to 11 bytes

        let result = codec.decode(&mut buf).unwrap();
        assert!(result.is_some(), "Should decode complete frame");
        assert_eq!(buf.len(), 0, "Buffer should be fully consumed");
    }

    #[test]
    fn decoder_handles_corrupted_crc() {
        let mut codec = FrameCodec::default();

        // Valid frame with corrupted CRC (last byte)
        let mut buf = BytesMut::new();
        buf.put_slice(&[
            0xaa, 0x55, 0x13, 0x70, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF,
        ]); // Bad CRC

        let result = codec.decode(&mut buf).unwrap();
        assert!(result.is_none(), "Should reject frame with bad CRC");
        assert_eq!(
            buf.len(),
            10,
            "Should consume 1 byte when searching for valid frame"
        );
    }

    #[test]
    fn decoder_finds_frame_after_garbage() {
        let mut codec = FrameCodec::default();

        // Garbage bytes followed by valid frame
        let mut buf = BytesMut::new();
        buf.put_slice(&[0xFF, 0xEE, 0xDD]); // Garbage
        buf.put_slice(&[
            0xaa, 0x55, 0x13, 0x70, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10,
        ]); // Valid frame

        // First calls should skip garbage
        assert!(codec.decode(&mut buf).unwrap().is_none());
        assert!(codec.decode(&mut buf).unwrap().is_none());
        assert!(codec.decode(&mut buf).unwrap().is_none());

        // Should find valid frame
        let result = codec.decode(&mut buf).unwrap();
        assert!(result.is_some(), "Should find valid frame after garbage");
        assert_eq!(buf.len(), 0, "All data should be consumed");
    }

    #[test]
    fn decoder_handles_false_start() {
        let mut codec = FrameCodec::default();

        // Frame that starts with 0xAA but not followed by 0x55
        let mut buf = BytesMut::new();
        buf.put_slice(&[0xaa, 0x00]); // False start
        buf.put_slice(&[
            0xaa, 0x55, 0x13, 0x70, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10,
        ]); // Valid frame

        // Total buffer: [AA, 00, AA, 55, 13, 70, 00, 00, 00, 00, 00, 00, 10] = 13 bytes
        assert_eq!(buf.len(), 13, "Initial buffer should have 13 bytes");

        // First decode: sees AA at pos 0, but 00 at pos 1, so should skip 1 byte
        let first = codec.decode(&mut buf).unwrap();
        assert!(first.is_none(), "First decode should return None");
        assert_eq!(buf.len(), 12, "Should have consumed 1 byte");

        // Buffer now: [00, AA, 55, 13, 70, 00, 00, 00, 00, 00, 00, 10] = 12 bytes
        // Second decode: sees 00 at pos 0, should skip 1 byte
        let second = codec.decode(&mut buf).unwrap();
        assert!(second.is_none(), "Second decode should return None");
        assert_eq!(buf.len(), 11, "Should have consumed another byte");

        // Buffer now: [AA, 55, 13, 70, 00, 00, 00, 00, 00, 00, 10] = 11 bytes = valid frame
        // Third decode should succeed
        let result = codec.decode(&mut buf);
        match result {
            Ok(Some(Response::ReadRegister { .. })) => {} // Success
            Ok(Some(other)) => panic!("Expected ReadRegister, got {:?}", other),
            Ok(None) => panic!(
                "Expected Some, got None. Buffer len: {}, contents: {:02x?}",
                buf.len(),
                &buf[..]
            ),
            Err(e) => panic!("Decode error: {}", e),
        }
    }

    #[test]
    fn decoder_handles_back_to_back_frames() {
        let mut codec = FrameCodec::default();

        // Two valid frames back-to-back
        let mut buf = BytesMut::new();
        // First frame: register read
        buf.put_slice(&[
            0xaa, 0x55, 0x13, 0x70, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10,
        ]);
        // Second frame: nonce response
        buf.put_slice(&[
            0xaa, 0x55, 0x18, 0x00, 0xa6, 0x40, 0x02, 0x99, 0x22, 0xf9, 0x91,
        ]);

        // Decode first frame
        let result1 = codec.decode(&mut buf).unwrap();
        assert!(matches!(result1, Some(Response::ReadRegister { .. })));
        assert_eq!(buf.len(), 11, "Should have second frame remaining");

        // Decode second frame
        let result2 = codec.decode(&mut buf).unwrap();
        assert!(matches!(result2, Some(Response::Nonce { .. })));
        assert_eq!(buf.len(), 0, "Buffer should be empty");
    }

    #[test]
    fn decoder_handles_real_s21_pro_frames() {
        let mut codec = FrameCodec::default();

        // Real frames from S21 Pro capture
        let frames = vec![
            [
                0xaa, 0x55, 0x07, 0x35, 0xcd, 0xcf, 0x02, 0x5e, 0x00, 0x2e, 0x96,
            ],
            [
                0xaa, 0x55, 0x7b, 0x8d, 0x81, 0x60, 0x02, 0x55, 0x00, 0x85, 0x81,
            ],
            [
                0xaa, 0x55, 0x32, 0x2a, 0x84, 0x5a, 0x02, 0x52, 0x01, 0xb2, 0x8c,
            ],
        ];

        for frame in frames {
            let mut buf = BytesMut::new();
            buf.put_slice(&frame);

            let result = codec.decode(&mut buf).unwrap();
            assert!(result.is_some(), "Should decode real S21 Pro frame");
            assert!(
                matches!(result, Some(Response::Nonce { .. })),
                "Should be nonce response"
            );
        }
    }

    #[test]
    fn decoder_handles_stream_with_lost_bytes() {
        let mut codec = FrameCodec::default();

        // Simulate a stream where some bytes in the middle are lost
        let mut buf = BytesMut::new();
        // Start of first frame
        buf.put_slice(&[0xaa, 0x55, 0x13, 0x70, 0x00]); // 5 bytes
                                                        // Lost bytes... skip to middle of nowhere
        buf.put_slice(&[0x99, 0x22, 0xf9]); // Random bytes
                                            // Valid complete frame
        buf.put_slice(&[
            0xaa, 0x55, 0x18, 0x00, 0xa6, 0x40, 0x02, 0x99, 0x22, 0xf9, 0x91,
        ]);

        // Decoder should skip the incomplete/corrupted data and find the valid frame
        let mut found_valid = false;
        for _ in 0..20 {
            // Try up to 20 times
            if let Some(response) = codec.decode(&mut buf).unwrap() {
                assert!(matches!(response, Response::Nonce { .. }));
                found_valid = true;
                break;
            }
        }
        assert!(found_valid, "Should eventually find the valid frame");
    }

    #[test]
    fn decoder_handles_mid_frame_start() {
        let mut codec = FrameCodec::default();

        // Start reading in the middle of a frame
        let mut buf = BytesMut::new();
        // Last 5 bytes of some frame
        buf.put_slice(&[0x02, 0x99, 0x22, 0xf9, 0x91]);
        // Valid complete frame
        buf.put_slice(&[
            0xaa, 0x55, 0x50, 0x03, 0x41, 0xd6, 0x00, 0x81, 0x18, 0x01, 0x9b,
        ]);

        // Total: 5 + 11 = 16 bytes
        // Should skip the partial frame bytes one by one until finding the valid frame
        for i in 0..5 {
            let result = codec.decode(&mut buf).unwrap();
            assert!(result.is_none(), "Decode {} should return None", i + 1);
            assert_eq!(
                buf.len(),
                16 - i - 1,
                "Should have consumed {} bytes",
                i + 1
            );
        }

        // Now we should have the valid frame
        let result = codec.decode(&mut buf).unwrap();
        assert!(
            result.is_some(),
            "Should find valid frame after partial data"
        );
        assert!(
            matches!(result, Some(Response::Nonce { .. })),
            "Should be nonce response"
        );
    }

    #[test]
    fn decoder_validates_real_register_responses() {
        // Test all register read responses are handled correctly
        let mut codec = FrameCodec::default();

        // Standard chip detection response
        let mut buf = BytesMut::new();
        buf.put_slice(&[
            0xaa, 0x55, 0x13, 0x70, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10,
        ]);

        let response = codec.decode(&mut buf).unwrap().unwrap();
        match response {
            Response::ReadRegister {
                chip_address,
                register,
            } => {
                assert_eq!(chip_address, 0x00);
                assert!(matches!(register, Register::ChipId { .. }));
            }
            _ => panic!("Expected ReadRegister response"),
        }
    }
}

// Bytes go out on the wire least-significant byte first.
// Multi-byte fields are sent most-significant byte first, i.e., big-endian.

/// Protocol handler for BM13xx family chips.
///
/// Encodes high-level operations into chip-specific commands and
/// decodes chip responses into meaningful results.
pub struct BM13xxProtocol {}

impl BM13xxProtocol {
    /// Create a new protocol instance.
    pub fn new() -> Self {
        Self {}
    }

    /// Helper to create a broadcast write command
    fn broadcast_write(&self, register: Register) -> Command {
        Command::WriteRegister {
            all: true,
            chip_address: 0x00,
            register,
        }
    }

    /// Helper to create a targeted write command
    #[cfg_attr(not(test), allow(dead_code))]
    fn write_to(&self, chip_address: u8, register: Register) -> Command {
        Command::WriteRegister {
            all: false,
            chip_address,
            register,
        }
    }

    /// Encode a mining job into a chip command.
    ///
    /// For BM1370, this uses the full format where the chip calculates midstates.
    /// Job IDs should be managed by the caller and cycled appropriately.
    pub fn encode_mining_job(&self, job: &MiningJob, job_id: u8) -> Command {
        // Convert MiningJob to JobFullFormat for BM1370
        // Note: The caller is responsible for:
        // - Converting hash values to big-endian format
        // - Managing job ID assignment and cycling

        let job_data = JobFullFormat {
            job_id,
            num_midstates: 0x01,                      // BM1370 typically uses 1
            starting_nonce: [0x00, 0x00, 0x00, 0x00], // Start at 0
            nbits: job.nbits.to_le_bytes(),
            ntime: job.ntime.to_le_bytes(),
            merkle_root: job.merkle_root,         // Should be big-endian
            prev_block_hash: job.prev_block_hash, // Should be big-endian
            version: job.version.to_le_bytes(),
        };

        Command::JobFull { job_data }
    }

    /// Get the initialization sequence for a single chip (e.g., Bitaxe).
    ///
    /// Returns a vector of commands to configure the chip for mining:
    /// 1. Set PLL parameters for desired frequency
    /// 2. Enable version rolling if supported
    /// 3. Configure other chip-specific settings
    pub fn single_chip_init(&self, frequency: Frequency) -> Vec<Command> {
        let mut commands = Vec::new();

        // Enable version rolling with mask 0xFFFF
        commands.push(self.broadcast_write(Register::VersionMask(VersionMask::full_rolling())));

        // Configure PLL for desired frequency
        let pll_config = frequency.calculate_pll();
        commands.push(self.broadcast_write(Register::PllDivider(pll_config)));

        commands
    }

    /// Initialize a multi-chip chain (e.g., S21 Pro, S19 J Pro).
    ///
    /// This follows the initialization sequence from production miners:
    /// 1. Enable version rolling on all chips
    /// 2. Configure initial settings
    /// 3. Set chain inactive and assign addresses
    /// 4. Configure domain boundaries
    /// 5. Ramp up frequency gradually
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn multi_chip_init(&self, chain_length: usize) -> Vec<Command> {
        // Multi-chip initialization register values
        const INIT_CONTROL_VALUE: u32 = 0x00000700;
        const MISC_CONTROL_MULTI_CHIP: u32 = 0x0000c1f0;
        const CORE_REG_INIT_1: u32 = 0x00008b80;
        const CORE_REG_INIT_2: u32 = 0x0c800080;
        const ADDRESS_INCREMENT: u8 = 2;

        let mut commands = Vec::new();

        // Step 1: Enable version rolling on all chips (broadcast)
        commands.push(self.broadcast_write(Register::VersionMask(VersionMask::full_rolling())));

        // Step 2: Configure init control register
        commands.push(self.broadcast_write(Register::InitControl {
            raw_value: INIT_CONTROL_VALUE,
        }));

        // Step 3: Configure misc control
        commands.push(self.broadcast_write(Register::MiscControl {
            raw_value: MISC_CONTROL_MULTI_CHIP,
        }));

        // Step 4: Set chain inactive for address assignment
        commands.push(Command::ChainInactive);

        // Step 5: Assign addresses (increment by 2)
        for i in 0..chain_length {
            let address = (i as u8) * ADDRESS_INCREMENT;
            commands.push(Command::SetChipAddress {
                chip_address: address,
            });
        }

        // Step 6: Configure core registers on all chips
        commands.push(self.broadcast_write(Register::CoreRegister {
            raw_value: CORE_REG_INIT_1,
        }));
        commands.push(self.broadcast_write(Register::CoreRegister {
            raw_value: CORE_REG_INIT_2,
        }));

        // Step 7: Set ticket mask (difficulty)
        commands
            .push(self.broadcast_write(Register::TicketMask(DifficultyMask::from_difficulty(256))));

        // Step 8: Configure IO driver strength on all chips
        commands.push(self.broadcast_write(Register::IoDriverStrength(IoDriverStrength::normal())));

        // Step 9: Configure nonce range partitioning
        commands.extend(self.configure_nonce_ranges(chain_length));

        commands
    }

    /// Configure domain boundaries for a multi-chip chain.
    ///
    /// Domains are groups of chips that share signal integrity settings.
    /// This configures IO driver strength and UART relay for domain boundaries.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn configure_domains(&self, chain_length: usize, chips_per_domain: usize) -> Vec<Command> {
        const UART_RELAY_BASE: u32 = 0x03000000;
        const ADDRESS_INCREMENT: u8 = 2;

        let mut commands = Vec::new();
        let num_domains = (chain_length + chips_per_domain - 1) / chips_per_domain;

        // Configure IO driver strength at domain boundaries
        for domain in 0..num_domains {
            let last_chip_in_domain = ((domain + 1) * chips_per_domain - 1).min(chain_length - 1);
            let chip_address = (last_chip_in_domain as u8) * ADDRESS_INCREMENT;

            commands.push(self.write_to(
                chip_address,
                Register::IoDriverStrength(IoDriverStrength::domain_boundary()),
            ));
        }

        // Configure UART relay for each domain
        for domain in 0..num_domains {
            let first_chip = domain * chips_per_domain;
            let last_chip = ((domain + 1) * chips_per_domain - 1).min(chain_length - 1);

            // Configure first chip in domain
            let first_address = (first_chip as u8) * ADDRESS_INCREMENT;
            let relay_offset = (domain * chips_per_domain) as u32;
            commands.push(self.write_to(
                first_address,
                Register::UartRelay {
                    raw_value: UART_RELAY_BASE | (relay_offset << 8),
                },
            ));

            // Configure last chip in domain
            if first_chip != last_chip {
                let last_address = (last_chip as u8) * ADDRESS_INCREMENT;
                commands.push(self.write_to(
                    last_address,
                    Register::UartRelay {
                        raw_value: UART_RELAY_BASE | (relay_offset << 8),
                    },
                ));
            }
        }

        commands
    }

    /// Configure nonce range partitioning for multi-chip operation.
    ///
    /// This distributes the 32-bit nonce space across all chips in the chain
    /// to avoid duplicate work. Each chip searches a unique portion of the nonce space.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn configure_nonce_ranges(&self, chain_length: usize) -> Vec<Command> {
        let mut commands = Vec::new();

        // Calculate nonce range based on chain length
        let nonce_config = NonceRangeConfig::multi_chip(chain_length);

        // Write nonce range to all chips
        commands.push(self.broadcast_write(Register::NonceRange(nonce_config)));

        commands
    }

    /// Distribute a mining job across chips with proper nonce space partitioning.
    ///
    /// For multi-chip chains, each chip gets the same job but searches different
    /// portions of the nonce space based on their chip address and NONCE_RANGE setting.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn distribute_job(
        &self,
        job: &MiningJob,
        _chain_length: usize,
        job_id: u8,
    ) -> Vec<Command> {
        let mut commands = Vec::new();

        // For BM1370/BM1362, jobs are broadcast to all chips
        // The NONCE_RANGE register handles partitioning
        let job_cmd = self.encode_mining_job(job, job_id);
        commands.push(job_cmd);

        commands
    }

    /// Generate frequency ramping sequence for gradual clock increase.
    ///
    /// This prevents power spikes and thermal stress during startup.
    pub fn frequency_ramp(
        &self,
        start: Frequency,
        target: Frequency,
        steps: usize,
    ) -> Vec<Command> {
        let mut commands = Vec::new();

        if steps <= 1 {
            // Direct jump to target frequency
            commands.push(self.broadcast_write(Register::PllDivider(target.calculate_pll())));
            return commands;
        }

        // Calculate frequency steps
        let start_mhz = start.mhz();
        let target_mhz = target.mhz();
        let freq_delta = (target_mhz - start_mhz) / (steps as f32 - 1.0);

        for i in 0..steps {
            let freq_mhz = start_mhz + freq_delta * i as f32;
            // Safe to unwrap because we're interpolating between valid frequencies
            let freq =
                Frequency::from_mhz(freq_mhz).expect("Interpolated frequency should be valid");
            commands.push(self.broadcast_write(Register::PllDivider(freq.calculate_pll())));
        }

        commands
    }

    /// Decode a response into a mining result.
    #[expect(
        dead_code,
        reason = "Will be used for nonce verification when pool is connected"
    )]
    pub fn decode_response(
        &self,
        response: Response,
        _chip_address: u8,
    ) -> Result<MiningResult, ChipError> {
        match response {
            Response::ReadRegister {
                chip_address: _,
                register,
            } => Ok(MiningResult::RegisterRead(register)),
            Response::Nonce {
                nonce,
                job_id,
                midstate_num: _,
                version,
                subcore_id,
            } => {
                // Extract main core ID from nonce (bits 25-31)
                let main_core_id = ((nonce >> 25) & 0x7f) as u8;

                // Full core ID combines main core and subcore
                let core_id = (main_core_id << 4) | subcore_id;

                // Job ID is already extracted correctly (4 bits)
                let actual_job_id = job_id as u64;

                // Combine version bits with nonce for version rolling
                // Version bits come in bits 15:0, need to shift to 28:13
                let _version_bits = (version as u32) << 13;

                Ok(MiningResult::NonceFound {
                    job_id: actual_job_id,
                    nonce,
                    core_id,
                })
            }
        }
    }

    /// Create a command to read a register.
    #[expect(dead_code, reason = "Will be used for chip diagnostics and monitoring")]
    pub fn read_register(&self, chip_address: u8, register: RegisterAddress) -> Command {
        Command::ReadRegister {
            all: false,
            chip_address,
            register_address: register,
        }
    }

    /// Set UART baud rate on all chips
    #[expect(dead_code, reason = "Will be used for baud rate negotiation")]
    pub fn set_baudrate(&self, baudrate: BaudRate) -> Command {
        Command::WriteRegister {
            all: true,
            chip_address: 0x00,
            register: Register::UartBaud(baudrate),
        }
    }

    /// Create a command to write a register.
    ///
    /// Note: This is a placeholder - actual register encoding depends on the register type
    #[expect(dead_code, reason = "Will be used for chip configuration")]
    pub fn write_register(
        &self,
        chip_address: u8,
        register: RegisterAddress,
        value: u32,
    ) -> Result<Command, ProtocolError> {
        // TODO: Properly encode register based on type
        // For now, just handle RegA8 as an example
        let register_value = match register {
            RegisterAddress::ChipId => {
                // Can't write chip ID register directly
                return Err(ProtocolError::ReadOnlyRegister(register));
            }
            RegisterAddress::PllDivider => Register::PllDivider(value.into()),
            RegisterAddress::NonceRange => Register::NonceRange(NonceRangeConfig {
                bytes: value.to_le_bytes(),
            }),
            RegisterAddress::TicketMask => Register::TicketMask(DifficultyMask {
                bytes: value.to_le_bytes(),
            }),
            RegisterAddress::MiscControl => Register::MiscControl { raw_value: value },
            RegisterAddress::UartBaud => Register::UartBaud(BaudRate::Custom(value)),
            RegisterAddress::UartRelay => Register::UartRelay { raw_value: value },
            RegisterAddress::CoreRegister => Register::CoreRegister { raw_value: value },
            RegisterAddress::AnalogMux => Register::AnalogMux { raw_value: value },
            RegisterAddress::IoDriverStrength => {
                let mut strengths = [0u8; 8];
                for i in 0..8 {
                    strengths[i] = ((value >> (i * 4)) & 0xf) as u8;
                }
                Register::IoDriverStrength(IoDriverStrength { strengths })
            }
            RegisterAddress::Pll3Parameter => Register::Pll3Parameter { raw_value: value },
            RegisterAddress::VersionMask => {
                let mask = (value >> 16) as u16;
                let control = (value & 0xffff) as u16;
                Register::VersionMask(VersionMask { mask, control })
            }
            RegisterAddress::InitControl => Register::InitControl { raw_value: value },
            RegisterAddress::MiscSettings => Register::MiscSettings { raw_value: value },
        };

        Ok(Command::WriteRegister {
            all: false,
            chip_address,
            register: register_value,
        })
    }

    /// Create a broadcast command to discover all chips.
    pub fn discover_chips() -> Command {
        Command::ReadRegister {
            all: true, // Broadcast
            chip_address: 0,
            register_address: RegisterAddress::ChipId,
        }
    }
}

/// Results from protocol operations
#[expect(dead_code, reason = "Will be used when pool connection is implemented")]
pub enum MiningResult {
    /// A register was read
    RegisterRead(Register),
    /// A nonce was found
    NonceFound {
        job_id: u64,
        nonce: u32,
        core_id: u8,
    },
}
