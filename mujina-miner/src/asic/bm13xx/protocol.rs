//! BM13xx protocol implementation for chip communication.
//!
//! This module handles the encoding and decoding of commands and responses
//! for BM13xx family chips (BM1366, BM1370, etc).

use bitcoin::hashes::Hash;
use bitcoin::pow::Work;
use bitvec::prelude::*;
use bytes::{Buf, BufMut, BytesMut};
use std::{fmt, io};
use strum::FromRepr;
use tokio_util::codec::{Decoder, Encoder};

use super::chip_config::ChipConfig;
use super::crc::{crc5, crc5_is_valid, crc16};
use super::error::ProtocolError;
use crate::job_source::GeneralPurposeBits;
use crate::tracing::prelude::*;
use crate::types::{Difficulty, Frequency};

/// Wrapper for formatting byte slices as space-separated hex.
struct HexBytes<'a>(&'a [u8]);

impl fmt::Display for HexBytes<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, byte) in self.0.iter().enumerate() {
            if i > 0 {
                write!(f, " ")?;
            }
            write!(f, "{:02x}", byte)?;
        }
        Ok(())
    }
}

/// PLL configuration for frequency control.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PllDivider {
    /// VCO control flag (0x40 for low VCO, 0x50 for high VCO).
    pub flag: u8,
    /// Feedback divider.
    pub fb_div: u8,
    /// Reference divider (typically 1 or 2).
    pub ref_div: u8,
    /// Post divider, encoded as `((post_div1-1) << 4) | (post_div2-1)`.
    pub post_div: u8,
}

impl PllDivider {
    /// Create a PLL configuration, deriving the VCO flag from the
    /// resulting VCO frequency: `vco >= 2400 MHz` picks flag `0x50`,
    /// otherwise flag `0x40`.
    pub fn new(fb_div: u8, ref_div: u8, post_div: u8) -> Self {
        let vco_mhz = fb_div as f32 * 25.0 / ref_div as f32;
        let flag = if vco_mhz >= 2400.0 { 0x50 } else { 0x40 };
        Self {
            flag,
            fb_div,
            ref_div,
            post_div,
        }
    }

    pub fn encode(&self, dst: &mut BytesMut) {
        dst.put_u8(self.flag);
        dst.put_u8(self.fb_div);
        dst.put_u8(self.ref_div);
        dst.put_u8(self.post_div);
    }

    pub fn decode(bytes: [u8; 4]) -> Self {
        Self {
            flag: bytes[0],
            fb_div: bytes[1],
            ref_div: bytes[2],
            post_div: bytes[3],
        }
    }
}

/// Known chip models in the BM13xx family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipModel {
    /// BM1362 - Used in Antminer S19 J Pro (126 chips)
    /// Core count unknown
    BM1362,
    /// BM1366 - Newer generation chip
    BM1366,
    /// BM1370 - Used in Bitaxe Gamma and Antminer S21 Pro
    /// ~2,040 hash engines organized as 128 domains of ~16 engines each
    BM1370,
    /// BM1397 - Previous generation chip
    BM1397,
    /// Unknown chip model with raw ID bytes.
    Unknown([u8; 2]),
}

impl ChipModel {
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

    /// Returns the expected hash engine count for this model, if known.
    pub fn core_count(&self) -> Option<u32> {
        match self {
            Self::BM1370 => Some(2048), // 128 x 16; esp-miner uses 2040
            _ => None,
        }
    }
}

impl From<[u8; 2]> for ChipModel {
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

impl From<ChipModel> for [u8; 2] {
    fn from(model: ChipModel) -> Self {
        model.id_bytes()
    }
}

/// Nonce range configuration for work distribution.
///
/// NOTE: We store this as a byte array rather than interpreting it as a u32
/// because the exact bit-level interpretation is still being reverse-engineered.
/// The values below are empirically observed from production hardware.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NonceRange {
    /// Raw bytes as sent over the wire
    bytes: [u8; 4],
}

impl NonceRange {
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

    /// Create config from raw 32-bit value (little-endian)
    /// Used for exact configuration from protocol captures
    pub fn from_raw(value: u32) -> Self {
        Self {
            bytes: value.to_le_bytes(),
        }
    }

    pub fn encode(&self, dst: &mut BytesMut) {
        dst.put_slice(&self.bytes);
    }

    pub fn decode(bytes: [u8; 4]) -> Self {
        Self { bytes }
    }
}

/// ASIC difficulty as a power-of-2 exponent.
///
/// BM13xx chips filter nonces using bitmask comparison (`hash &
/// mask == 0`) rather than numerical target comparison (`hash <
/// target`). Each bit in the mask independently halves the pass
/// rate, so only power-of-2 difficulty steps are representable.
/// This type stores the log2 of the difficulty: a value of 8
/// means difficulty 2^8 = 256.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Log2Difficulty {
    exponent: u8,
}

impl Log2Difficulty {
    /// Floor an arbitrary difficulty to the nearest power-of-2
    /// ASIC difficulty.
    ///
    /// The conversion is lossy: non-power-of-2 difficulties are
    /// rounded down. This ensures the actual nonce rate is at least
    /// as high as the rate implied by the input difficulty.
    pub fn from_difficulty(difficulty: Difficulty) -> Self {
        let d = difficulty.as_f64();
        let exponent = if d >= 1.0 { d.log2().floor() as u8 } else { 0 };
        Self { exponent }
    }

    /// The log2 of the difficulty (e.g., 8 for difficulty 256).
    pub const fn exponent(&self) -> u8 {
        self.exponent
    }

    /// Expected work per nonce at this difficulty.
    ///
    /// A nonce that passes the ASIC's difficulty filter represents
    /// this many hashes of work on average.
    pub fn to_work(&self) -> Work {
        Difficulty::from(1_u64 << self.exponent)
            .to_target()
            .to_work()
    }
}

impl fmt::Display for Log2Difficulty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "2^{}", self.exponent)
    }
}

/// Ticket mask controlling ASIC nonce reporting
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TicketMask {
    // Number of additional zero bits required in the bit-reversed hash,
    // beyond the base 32 bits. The chip always requires bits 0-31 of the
    // bit-reversed hash to be zero. This parameter adds bits 32..(32+zero_bits)
    // that must also be zero.
    zero_bits: u8,
}

impl TicketMask {
    /// Create ticket mask from an ASIC difficulty.
    ///
    /// The [`Log2Difficulty`] exponent maps directly to the number
    /// of extra zero bits the chip requires beyond its hardwired
    /// difficulty-1 gate.
    pub const fn new(difficulty: Log2Difficulty) -> Self {
        Self {
            zero_bits: difficulty.exponent(),
        }
    }

    /// Encode ticket mask to wire format bytes
    pub fn to_wire_bytes(&self) -> [u8; 4] {
        if self.zero_bits == 0 {
            return [0, 0, 0, 0];
        }

        // Create mask value: 2^zero_bits - 1
        let mask_value = (1u32 << self.zero_bits) - 1;

        // Encode to wire format with bit-reversal and byte-reversal
        let mut bytes = [0u8; 4];
        for i in 0..4 {
            let byte = ((mask_value >> (8 * i)) & 0xFF) as u8;
            bytes[3 - i] = reverse_bits(byte);
        }

        bytes
    }

    pub fn encode(&self, dst: &mut BytesMut) {
        dst.put_slice(&self.to_wire_bytes());
    }

    pub fn decode(bytes: [u8; 4]) -> Self {
        let mask_value = decode_ticket_mask_bytes(&bytes);
        let zero_bits = mask_value.count_ones() as u8;
        Self { zero_bits }
    }
}

/// Helper function to reverse bits in a byte
fn reverse_bits(byte: u8) -> u8 {
    let mut result = 0u8;
    let mut b = byte;
    for _ in 0..8 {
        result = (result << 1) | (b & 1);
        b >>= 1;
    }
    result
}

/// Helper function to decode ticket mask bytes back to mask value
fn decode_ticket_mask_bytes(bytes: &[u8; 4]) -> u32 {
    // Reverse the encoding process: undo byte reversal and bit reversal
    let mut mask_value = 0u32;
    for i in 0..4 {
        let byte = reverse_bits(bytes[3 - i]);
        mask_value |= (byte as u32) << (8 * i);
    }
    mask_value
}

/// UART baud rate configuration
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UartBaud {
    /// 115200 baud
    Baud115200,
    /// 1 Mbaud
    Baud1M,
    /// 3 Mbaud (common for multi-chip)
    Baud3M,
    /// Custom baud rate with raw register value
    Custom(u32),
}

impl UartBaud {
    pub fn encode(&self, dst: &mut BytesMut) {
        let value = match self {
            // From esp-miner BM1370/BM1366/BM1368 default baud config
            UartBaud::Baud115200 => 0x00000271,
            // From esp-miner BM1370_set_max_baud/BM1366_set_max_baud/BM1368_set_max_baud
            // All three chips use identical register value for 1Mbaud
            UartBaud::Baud1M => 0x00023011,
            // From S21 Pro captures (BM1370 multi-chip chains)
            UartBaud::Baud3M => 0x00003001,
            UartBaud::Custom(val) => *val,
        };
        dst.put_u32_le(value);
    }
    pub fn decode(bytes: [u8; 4]) -> Self {
        match u32::from_le_bytes(bytes) {
            0x00000271 => UartBaud::Baud115200,
            0x00000130 => UartBaud::Baud1M,
            0x00003001 => UartBaud::Baud3M,
            other => UartBaud::Custom(other),
        }
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

    pub fn encode(&self, dst: &mut BytesMut) {
        // Pack 8 4-bit values into 4 bytes (2 per byte).
        // Each byte contains two strength values: [high_nibble|low_nibble].
        dst.put_u8(self.strengths[0] | (self.strengths[1] << 4));
        dst.put_u8(self.strengths[2] | (self.strengths[3] << 4));
        dst.put_u8(self.strengths[4] | (self.strengths[5] << 4));
        dst.put_u8(self.strengths[6] | (self.strengths[7] << 4));
    }

    pub fn decode(bytes: [u8; 4]) -> Self {
        let raw = u32::from_le_bytes(bytes);
        let mut strengths = [0u8; 8];
        for (i, strength) in strengths.iter_mut().enumerate() {
            *strength = ((raw >> (i * 4)) & 0xf) as u8;
        }
        Self { strengths }
    }
}

/// Version mask for version rolling
#[derive(Clone, Copy, PartialEq)]
pub struct VersionMask {
    /// Which bits can be rolled
    mask: u16,
    /// Enable flag and other control bits
    control: u16,
}

impl VersionMask {
    /// Full 16-bit mask for version rolling
    const FULL_MASK: u16 = 0xffff;
    /// Fixed control pattern used by all implementations to enable version rolling
    const ENABLE_ROLLING: u16 = 0x0090;

    /// Create version mask with all lower 16 bits enabled
    pub fn full_rolling() -> Self {
        Self {
            mask: Self::FULL_MASK,
            control: Self::ENABLE_ROLLING,
        }
    }

    pub fn encode(&self, dst: &mut BytesMut) {
        dst.put_u16_le(self.control);
        dst.put_u16_le(self.mask);
    }

    pub fn decode(bytes: [u8; 4]) -> Self {
        let raw = u32::from_le_bytes(bytes);
        Self {
            control: (raw & 0xffff) as u16,
            mask: (raw >> 16) as u16,
        }
    }
}

impl fmt::Debug for VersionMask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let control_str = if self.control == Self::ENABLE_ROLLING {
            "ENABLE_ROLLING".to_string()
        } else {
            format!("{:#06x}", self.control)
        };
        f.debug_struct("VersionMask")
            .field("mask", &format_args!("{:#06x}", self.mask))
            .field("control", &control_str)
            .finish()
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
    Core = 0x3C,
    AnalogMux = 0x54,
    IoDriverStrength = 0x58,
    Pll3Parameter = 0x68,
    VersionMask = 0xA4,
    InitControl = 0xA8,
    MiscSettings = 0xB9,
}

/// Chip model + core count + assigned chain address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChipId {
    pub model: ChipModel,
    pub core_count: u8,
    pub address: u8,
}

impl ChipId {
    pub fn encode(&self, dst: &mut BytesMut) {
        dst.put_slice(&self.model.id_bytes());
        dst.put_u8(self.core_count);
        dst.put_u8(self.address);
    }
    pub fn decode(bytes: [u8; 4]) -> Self {
        Self {
            model: ChipModel::from([bytes[0], bytes[1]]),
            core_count: bytes[2],
            address: bytes[3],
        }
    }
}

// Placeholder newtypes for registers whose bit layout is not yet decomposed.
// Each wraps a raw u32 written little-endian to the wire.
macro_rules! raw_u32_register {
    ($($(#[$meta:meta])* $name:ident),* $(,)?) => {
        $(
            $(#[$meta])*
            #[derive(Debug, Clone, Copy, PartialEq, Eq)]
            pub struct $name(pub u32);

            impl $name {
                pub fn encode(&self, dst: &mut BytesMut) {
                    dst.put_u32_le(self.0);
                }
                pub fn decode(bytes: [u8; 4]) -> Self {
                    Self(u32::from_le_bytes(bytes))
                }
            }
        )*
    };
}

raw_u32_register! {
    MiscControl,
    UartRelay,
    AnalogMux,
    Pll3Parameter,
    InitControl,
    MiscSettings,
}

/// Transmitted big-endian, unlike the other raw-u32 registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Core(pub u32);

impl Core {
    pub fn encode(&self, dst: &mut BytesMut) {
        dst.put_u32(self.0);
    }
    pub fn decode(bytes: [u8; 4]) -> Self {
        Self(u32::from_be_bytes(bytes))
    }
}

#[derive(Debug, Clone)]
pub enum Register {
    ChipId(ChipId),
    PllDivider(PllDivider),
    NonceRange(NonceRange),
    TicketMask(TicketMask),
    MiscControl(MiscControl),
    UartBaud(UartBaud),
    UartRelay(UartRelay),
    Core(Core),
    AnalogMux(AnalogMux),
    IoDriverStrength(IoDriverStrength),
    Pll3Parameter(Pll3Parameter),
    VersionMask(VersionMask),
    InitControl(InitControl),
    MiscSettings(MiscSettings),
}

impl Register {
    pub fn decode(address: RegisterAddress, bytes: [u8; 4]) -> Register {
        match address {
            RegisterAddress::ChipId => Register::ChipId(ChipId::decode(bytes)),
            RegisterAddress::PllDivider => Register::PllDivider(PllDivider::decode(bytes)),
            RegisterAddress::NonceRange => Register::NonceRange(NonceRange::decode(bytes)),
            RegisterAddress::TicketMask => Register::TicketMask(TicketMask::decode(bytes)),
            RegisterAddress::MiscControl => Register::MiscControl(MiscControl::decode(bytes)),
            RegisterAddress::UartBaud => Register::UartBaud(UartBaud::decode(bytes)),
            RegisterAddress::UartRelay => Register::UartRelay(UartRelay::decode(bytes)),
            RegisterAddress::Core => Register::Core(Core::decode(bytes)),
            RegisterAddress::AnalogMux => Register::AnalogMux(AnalogMux::decode(bytes)),
            RegisterAddress::IoDriverStrength => {
                Register::IoDriverStrength(IoDriverStrength::decode(bytes))
            }
            RegisterAddress::Pll3Parameter => Register::Pll3Parameter(Pll3Parameter::decode(bytes)),
            RegisterAddress::VersionMask => Register::VersionMask(VersionMask::decode(bytes)),
            RegisterAddress::InitControl => Register::InitControl(InitControl::decode(bytes)),
            RegisterAddress::MiscSettings => Register::MiscSettings(MiscSettings::decode(bytes)),
        }
    }

    /// Get the register address for this register
    fn address(&self) -> RegisterAddress {
        match self {
            Register::ChipId(_) => RegisterAddress::ChipId,
            Register::PllDivider(_) => RegisterAddress::PllDivider,
            Register::NonceRange(_) => RegisterAddress::NonceRange,
            Register::TicketMask(_) => RegisterAddress::TicketMask,
            Register::MiscControl(_) => RegisterAddress::MiscControl,
            Register::UartBaud(_) => RegisterAddress::UartBaud,
            Register::UartRelay(_) => RegisterAddress::UartRelay,
            Register::Core(_) => RegisterAddress::Core,
            Register::AnalogMux(_) => RegisterAddress::AnalogMux,
            Register::IoDriverStrength(_) => RegisterAddress::IoDriverStrength,
            Register::Pll3Parameter(_) => RegisterAddress::Pll3Parameter,
            Register::VersionMask(_) => RegisterAddress::VersionMask,
            Register::InitControl(_) => RegisterAddress::InitControl,
            Register::MiscSettings(_) => RegisterAddress::MiscSettings,
        }
    }

    /// Encode the register data (not the address)
    fn encode(&self, dst: &mut BytesMut) {
        match self {
            Register::ChipId(r) => r.encode(dst),
            Register::PllDivider(r) => r.encode(dst),
            Register::NonceRange(r) => r.encode(dst),
            Register::TicketMask(r) => r.encode(dst),
            Register::MiscControl(r) => r.encode(dst),
            Register::UartBaud(r) => r.encode(dst),
            Register::UartRelay(r) => r.encode(dst),
            Register::Core(r) => r.encode(dst),
            Register::AnalogMux(r) => r.encode(dst),
            Register::IoDriverStrength(r) => r.encode(dst),
            Register::Pll3Parameter(r) => r.encode(dst),
            Register::VersionMask(r) => r.encode(dst),
            Register::InitControl(r) => r.encode(dst),
            Register::MiscSettings(r) => r.encode(dst),
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

/// Convert Bitcoin internal hash format to BM13xx wire format.
///
/// Bitcoin uses little-endian 32-byte hashes internally. The BM13xx wire
/// protocol expects these hashes with 4-byte words reversed:
/// - Split the 32 bytes into 8 4-byte words
/// - Reverse word order (word 0 with 7, 1 with 6, 2 with 5, 3 with 4)
///
/// Example:
/// Internal: [w0_byte0, w0_byte1, w0_byte2, w0_byte3, w1_..., w7_byte3]
/// Wire:     [w7_byte0, w7_byte1, w7_byte2, w7_byte3, w6_..., w0_byte3]
pub fn hash_to_wire_bytes(hash: &[u8; 32]) -> [u8; 32] {
    let mut wire_bytes = [0u8; 32];
    // Reverse the order of 4-byte words
    for i in 0..8 {
        let src_word = &hash[i * 4..(i + 1) * 4];
        let dst_word = &mut wire_bytes[(7 - i) * 4..(8 - i) * 4];
        dst_word.copy_from_slice(src_word);
    }
    wire_bytes
}

/// Convert BM13xx wire format to Bitcoin internal hash format.
///
/// Inverse of `hash_to_wire_bytes`. Takes wire bytes and reverses the 4-byte
/// word order to produce Bitcoin's internal little-endian format.
pub fn hash_from_wire_bytes(wire_bytes: &[u8; 32]) -> [u8; 32] {
    let mut hash = [0u8; 32];
    // Reverse the order of 4-byte words
    for i in 0..8 {
        let src_word = &wire_bytes[i * 4..(i + 1) * 4];
        let dst_word = &mut hash[(7 - i) * 4..(8 - i) * 4];
        dst_word.copy_from_slice(src_word);
    }
    hash
}

/// Target of a register read or write.
///
/// Broadcast frames go to every chip on the chain; the address byte
/// is ignored on the wire. Chip-directed frames carry the assigned
/// address of a single chip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Destination {
    Broadcast,
    Chip(u8),
}

impl Destination {
    fn is_broadcast(self) -> bool {
        matches!(self, Self::Broadcast)
    }

    fn address_byte(self) -> u8 {
        match self {
            Self::Broadcast => 0x00,
            Self::Chip(addr) => addr,
        }
    }
}

/// TYPE=2 frames: register reads/writes and chain addressing. Use CRC5.
#[derive(Debug)]
pub enum RegisterCommand {
    /// Assign an address to the first unaddressed chip via daisy-chain forwarding
    SetChipAddress { chip_address: u8 },
    /// Put all chips into addressing mode (enables daisy-chain forwarding)
    ChainInactive,
    /// Read a register from chip(s)
    ReadRegister {
        destination: Destination,
        register_address: RegisterAddress,
    },
    /// Write a register to chip(s)
    WriteRegister {
        destination: Destination,
        register: Register,
    },
}

/// TYPE=1 frames: mining jobs. Use CRC16.
#[derive(Debug)]
pub enum JobCommand {
    /// Send a job with full block header (BM1370/BM1362 style)
    /// Chip calculates midstates internally
    JobFull { job_data: JobFullFormat },
    /// Send a job with pre-calculated midstates (BM1397 style)
    /// Host calculates midstates to save chip computation
    JobMidstate { job_data: JobMidstateFormat },
}

/// Full format job structure
///
/// The chip calculates midstates internally from the full block header.
/// This structure uses Bitcoin types internally; conversion to/from wire format
/// happens during encoding/decoding.
#[derive(Debug, Clone)]
pub struct JobFullFormat {
    /// 4-bit job identifier (0-15), encoded into bits 6-3 of job_header on wire
    pub job_id: u8,
    /// Number of midstates (typically 0x01 for BM1370)
    pub num_midstates: u8,
    /// Starting nonce value (typically 0x00000000)
    pub starting_nonce: u32,
    /// Encoded difficulty target
    pub nbits: bitcoin::CompactTarget,
    /// Block timestamp (Unix time)
    pub ntime: u32,
    /// Transaction merkle tree root
    pub merkle_root: bitcoin::hash_types::TxMerkleNode,
    /// Previous block hash
    pub prev_block_hash: bitcoin::BlockHash,
    /// Block version (base version, chip may roll additional bits)
    pub version: bitcoin::block::Version,
}

/// Midstate format job structure (BM1397?).
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

fn build_flags(typ: CommandFlagsType, broadcast: bool, cmd: CommandFlagsCmd) -> u8 {
    let mut flags = 0u8;
    let field = flags.view_bits_mut::<Lsb0>();
    field[5..7].store(typ as u8);
    field[4..5].store(broadcast as u8);
    field[0..4].store(cmd as u8);
    flags
}

impl RegisterCommand {
    fn encode(&self, dst: &mut BytesMut) {
        match self {
            Self::SetChipAddress { chip_address } => {
                dst.put_u8(build_flags(
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
                dst.put_u8(0x00); // Reserved byte (always 0x00)
            }
            Self::ChainInactive => {
                dst.put_u8(build_flags(
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
                dst.put_u8(0x00); // Reserved byte
                dst.put_u8(0x00); // Reserved byte
            }
            Self::ReadRegister {
                destination,
                register_address,
            } => {
                dst.put_u8(build_flags(
                    CommandFlagsType::Command,
                    destination.is_broadcast(),
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
                dst.put_u8(destination.address_byte());
                dst.put_u8(*register_address as u8);
            }
            Self::WriteRegister {
                destination,
                register,
            } => {
                dst.put_u8(build_flags(
                    CommandFlagsType::Command,
                    destination.is_broadcast(),
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
                dst.put_u8(destination.address_byte());
                dst.put_u8(register.address() as u8);
                register.encode(dst);
            }
        }
    }
}

impl JobCommand {
    fn encode(&self, dst: &mut BytesMut) {
        match self {
            Self::JobFull { job_data } => {
                dst.put_u8(build_flags(
                    CommandFlagsType::Job,
                    false, // Jobs are never broadcast
                    CommandFlagsCmd::WriteRegisterOrJob,
                ));

                // Captures from factory firmware use this value on both BM1362
                // (S19 J Pro) and BM1370 (S21 Pro). esp-miner firmware on
                // Bitaxe sends 86 instead; the BM1370 appears to tolerate
                // both.
                //
                // Hypothesis for why 54: a single midstate JobMidstate frame
                // is exactly 54 bytes on the wire (flags + length + header +
                // midstate0 + crc16). JobFull transmits 88 bytes but declares
                // its length as if the frame were in midstate format, where
                // prev_block_hash is folded into midstate0 rather than sent
                // raw.
                const JOB_LENGTH_BYTE: u8 = 54;
                dst.put_u8(JOB_LENGTH_BYTE);

                // Write job data
                // job_id is a 4-bit value (0-15), encode into bits 6-3 of job_header
                debug_assert!(job_data.job_id <= 15, "job_id must be 0-15");
                dst.put_u8(job_data.job_id << 3);
                dst.put_u8(job_data.num_midstates);
                dst.put_u32_le(job_data.starting_nonce);
                dst.put_u32_le(job_data.nbits.to_consensus());
                dst.put_u32_le(job_data.ntime);

                // Convert merkle_root from Bitcoin internal format to wire format
                let merkle_root_bytes = hash_to_wire_bytes(&job_data.merkle_root.to_byte_array());
                dst.put_slice(&merkle_root_bytes);

                // Convert prev_block_hash from Bitcoin internal format to wire format
                let prev_hash_bytes = hash_to_wire_bytes(&job_data.prev_block_hash.to_byte_array());
                dst.put_slice(&prev_hash_bytes);

                dst.put_u32_le(job_data.version.to_consensus() as u32);
            }
            Self::JobMidstate { job_data } => {
                dst.put_u8(build_flags(
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
                // job_id is a 4-bit value (0-15), encode into bits 6-3 of job_header
                debug_assert!(job_data.job_id <= 15, "job_id must be 0-15");
                dst.put_u8(job_data.job_id << 3);
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
        version: GeneralPurposeBits,
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
                    let register = Register::decode(register_address, value);
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

                // Version rolling field: 2 bytes, big-endian
                // Occupies bits 13-28 of block version when shifted left 13
                let version_bytes = [bytes.get_u8(), bytes.get_u8()];
                let version = GeneralPurposeBits::from(version_bytes);
                // CRC already consumed

                // Extract job_id and subcore_id from result_header
                // job_id is a 4-bit field (0-15) at bits 7-4 of result_header
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
pub struct FrameCodec;

impl Encoder<RegisterCommand> for FrameCodec {
    type Error = io::Error;

    fn encode(&mut self, command: RegisterCommand, dst: &mut BytesMut) -> Result<(), Self::Error> {
        const PREAMBLE: [u8; 2] = [0x55, 0xaa];
        dst.put_slice(&PREAMBLE);

        let start_pos = dst.len();
        command.encode(dst);
        let crc = crc5(&dst[start_pos..]);
        dst.put_u8(crc);

        trace!(
            cmd = ?command,
            bytes = dst.len(),
            frame = %HexBytes(dst.as_ref()),
            "TX BM13xx"
        );

        Ok(())
    }
}

impl Encoder<JobCommand> for FrameCodec {
    type Error = io::Error;

    fn encode(&mut self, command: JobCommand, dst: &mut BytesMut) -> Result<(), Self::Error> {
        const PREAMBLE: [u8; 2] = [0x55, 0xaa];
        dst.put_slice(&PREAMBLE);

        let start_pos = dst.len();
        command.encode(dst);
        let crc = crc16(&dst[start_pos..]);
        dst.put_slice(&crc.to_be_bytes());

        trace!(
            cmd = ?command,
            bytes = dst.len(),
            frame = %HexBytes(dst.as_ref()),
            "TX BM13xx"
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
                    resp = ?response,
                    bytes = FRAME_LEN,
                    frame = %HexBytes(&frame_bytes),
                    "RX BM13xx"
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
            RegisterCommand::WriteRegister {
                destination: Destination::Broadcast,
                register: Register::VersionMask(_),
            }
        ));

        // Verify chain inactive command
        let chain_inactive_pos = commands
            .iter()
            .position(|c| matches!(c, RegisterCommand::ChainInactive))
            .expect("ChainInactive command not found in initialization sequence");
        assert!(chain_inactive_pos > 0);

        // Verify chip addressing starts after chain inactive
        let first_address_pos = chain_inactive_pos + 1;
        assert!(matches!(
            &commands[first_address_pos],
            RegisterCommand::SetChipAddress { chip_address: 0x00 }
        ));

        // Verify we have 65 address assignments
        let address_commands: Vec<_> = commands[first_address_pos..first_address_pos + 65]
            .iter()
            .collect();
        assert_eq!(address_commands.len(), 65);

        // Verify addresses increment by 2
        for (i, cmd) in address_commands.iter().enumerate() {
            match cmd {
                RegisterCommand::SetChipAddress { chip_address } => {
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
                    RegisterCommand::WriteRegister {
                        register: Register::IoDriverStrength { .. },
                        ..
                    }
                )
            })
            .collect();
        assert_eq!(io_strength_commands.len(), 13);

        // Check first domain boundary (chip 8 = address 0x08)
        let first_boundary = io_strength_commands[0];
        if let RegisterCommand::WriteRegister {
            destination: Destination::Chip(chip_address),
            register: Register::IoDriverStrength(strength),
        } = first_boundary
        {
            assert_eq!(*chip_address, 0x08); // 5th chip (index 4) * 2
            let mut buf = BytesMut::new();
            strength.encode(&mut buf);
            // Expected bytes from hardware capture
            assert_eq!(&buf[..], &[0x00, 0xf1, 0x11, 0x11]);
        }
    }

    #[test]
    fn nonce_range_configuration() {
        let protocol = BM13xxProtocol::new();

        // Test single chip - full range
        let commands = protocol.configure_nonce_ranges(1);
        assert_eq!(commands.len(), 1);
        if let RegisterCommand::WriteRegister {
            register: Register::NonceRange(config),
            destination: Destination::Broadcast,
        } = &commands[0]
        {
            let mut buf = BytesMut::new();
            config.encode(&mut buf);
            assert_eq!(&buf[..], &[0xff, 0xff, 0xff, 0xff]);
        }

        // Test S21 Pro configuration (65 chips)
        let commands = protocol.configure_nonce_ranges(65);
        assert_eq!(commands.len(), 1);
        if let RegisterCommand::WriteRegister {
            register: Register::NonceRange(config),
            ..
        } = &commands[0]
        {
            let mut buf = BytesMut::new();
            config.encode(&mut buf);
            assert_eq!(&buf[..], &[0x00, 0x00, 0x1e, 0xb5]);
        }

        // Test small chain
        let commands = protocol.configure_nonce_ranges(8);
        if let RegisterCommand::WriteRegister {
            register: Register::NonceRange(config),
            ..
        } = &commands[0]
        {
            let mut buf = BytesMut::new();
            config.encode(&mut buf);
            assert_eq!(&buf[..], &[0xff, 0xff, 0xff, 0x1f]);
        }
    }

    #[test]
    fn multi_chip_init_includes_nonce_range() {
        let protocol = BM13xxProtocol::new();
        let commands = protocol.multi_chip_init(65);

        // Find the nonce range configuration
        let nonce_range_cmd = commands.iter().find(|c| {
            matches!(
                c,
                RegisterCommand::WriteRegister {
                    register: Register::NonceRange { .. },
                    ..
                }
            )
        });

        assert!(nonce_range_cmd.is_some());

        if let Some(RegisterCommand::WriteRegister {
            register: Register::NonceRange(config),
            ..
        }) = nonce_range_cmd
        {
            let mut buf = BytesMut::new();
            config.encode(&mut buf);
            assert_eq!(&buf[..], &[0x00, 0x00, 0x1e, 0xb5]); // S21 Pro value
        }
    }
}

#[cfg(test)]
mod command_tests {
    use super::*;
    use crate::asic::bm13xx::test_data::s19jpro_job;
    use bitcoin::block::Version;
    use bitcoin::hash_types::TxMerkleNode;
    use bitcoin::hashes::Hash;
    use bitcoin::{BlockHash, CompactTarget};

    #[test]
    fn read_register() {
        assert_frame_eq(
            RegisterCommand::ReadRegister {
                destination: Destination::Broadcast,
                register_address: RegisterAddress::ChipId,
            },
            &[0x55, 0xaa, 0x52, 0x05, 0x00, 0x00, 0x0a],
        );
    }

    #[test]
    fn write_register_chip_address() {
        assert_frame_eq(
            RegisterCommand::WriteRegister {
                destination: Destination::Chip(0x01),
                register: Register::ChipId(ChipId {
                    model: ChipModel::BM1370,
                    core_count: 0x00,
                    address: 0x01,
                }),
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
            RegisterCommand::WriteRegister {
                destination: Destination::Broadcast, // 0x51 = broadcast
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
            RegisterCommand::WriteRegister {
                destination: Destination::Broadcast,
                register: Register::InitControl(InitControl(0x00000700)),
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
            RegisterCommand::WriteRegister {
                destination: Destination::Broadcast,
                register: Register::MiscControl(MiscControl(0x00C100F0)),
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
            RegisterCommand::ChainInactive,
            &[0x55, 0xaa, 0x53, 0x05, 0x00, 0x00, 0x03],
        );
    }

    #[test]
    fn set_chip_address_from_capture() {
        // From S21 Pro capture: TX: 55 AA 40 05 04 00 03 (assign address 0x04)
        assert_frame_eq(
            RegisterCommand::SetChipAddress { chip_address: 0x04 },
            &[0x55, 0xaa, 0x40, 0x05, 0x04, 0x00, 0x03],
        );
    }

    #[test]
    fn write_core_register_sequence() {
        // From Bitaxe capture: TX: 55 AA 51 09 00 3C 80 00 8B 00 12
        // Core register uses big-endian encoding
        assert_frame_eq(
            RegisterCommand::WriteRegister {
                destination: Destination::Broadcast,
                // Big-endian: produces bytes 80 00 8B 00
                register: Register::Core(Core(0x80008B00)),
            },
            &[
                0x55, 0xaa, 0x51, 0x09, 0x00, 0x3c, 0x80, 0x00, 0x8b, 0x00, 0x12,
            ],
        );
    }

    #[test]
    fn write_ticket_mask_from_capture() {
        // From S21 Pro capture: TX: 55 AA 51 09 00 14 00 00 00 FF 08
        // Difficulty 256 = 8 zero_bits
        let log2_diff = Log2Difficulty::from_difficulty(Difficulty::from(256_u64));
        assert_frame_eq(
            RegisterCommand::WriteRegister {
                destination: Destination::Broadcast,
                register: Register::TicketMask(TicketMask::new(log2_diff)),
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
            RegisterCommand::WriteRegister {
                destination: Destination::Broadcast,
                register: Register::NonceRange(NonceRange::multi_chip(65)),
            },
            &[
                0x55, 0xaa, 0x51, 0x09, 0x00, 0x10, 0x00, 0x00, 0x1e, 0xb5, 0x0f,
            ],
        );
    }

    #[test]
    fn job_full_format_encoding() {
        use bitcoin::CompactTarget;

        // Test BM1370 job packet encoding with patterns that verify word-swapping
        // Use sequential bytes so we can verify word reversal
        // Internal format: [w0, w1, w2, w3, w4, w5, w6, w7] (each word is 4 bytes)
        // Wire format: [w7, w6, w5, w4, w3, w2, w1, w0]
        let merkle_internal = [
            0x00, 0x01, 0x02, 0x03, // word 0
            0x04, 0x05, 0x06, 0x07, // word 1
            0x08, 0x09, 0x0a, 0x0b, // word 2
            0x0c, 0x0d, 0x0e, 0x0f, // word 3
            0x10, 0x11, 0x12, 0x13, // word 4
            0x14, 0x15, 0x16, 0x17, // word 5
            0x18, 0x19, 0x1a, 0x1b, // word 6
            0x1c, 0x1d, 0x1e, 0x1f, // word 7
        ];
        let prev_hash_internal = [
            0x20, 0x21, 0x22, 0x23, // word 0
            0x24, 0x25, 0x26, 0x27, // word 1
            0x28, 0x29, 0x2a, 0x2b, // word 2
            0x2c, 0x2d, 0x2e, 0x2f, // word 3
            0x30, 0x31, 0x32, 0x33, // word 4
            0x34, 0x35, 0x36, 0x37, // word 5
            0x38, 0x39, 0x3a, 0x3b, // word 6
            0x3c, 0x3d, 0x3e, 0x3f, // word 7
        ];

        let job = JobFullFormat {
            job_id: 0x00,
            num_midstates: 0x01,
            starting_nonce: 0x00000000,
            nbits: CompactTarget::from_consensus(0x6ad60e17),
            ntime: 0x208c7366,
            merkle_root: bitcoin::hash_types::TxMerkleNode::from_byte_array(merkle_internal),
            prev_block_hash: bitcoin::BlockHash::from_byte_array(prev_hash_internal),
            version: bitcoin::block::Version::from_consensus(0x20000000),
        };

        let mut codec = FrameCodec;
        let mut frame = BytesMut::new();
        codec
            .encode(
                JobCommand::JobFull {
                    job_data: job.clone(),
                },
                &mut frame,
            )
            .expect("Failed to encode job command");

        // Verify packet structure
        assert_eq!(&frame[0..2], &[0x55, 0xaa]); // Preamble
        assert_eq!(frame[2], 0x21); // TYPE_JOB | GROUP_SINGLE | CMD_WRITE
        assert_eq!(frame[3], 54); // Length byte per factory captures (not a byte count)
        assert_eq!(frame[4], job.job_id);
        assert_eq!(frame[5], job.num_midstates);
        assert_eq!(&frame[6..10], &job.starting_nonce.to_le_bytes());
        assert_eq!(&frame[10..14], &job.nbits.to_consensus().to_le_bytes());
        assert_eq!(&frame[14..18], &job.ntime.to_le_bytes());

        // Verify merkle_root word-swapping: wire should have word 7 first, then 6, etc.
        let expected_merkle_wire = [
            0x1c, 0x1d, 0x1e, 0x1f, // word 7 (was last)
            0x18, 0x19, 0x1a, 0x1b, // word 6
            0x14, 0x15, 0x16, 0x17, // word 5
            0x10, 0x11, 0x12, 0x13, // word 4
            0x0c, 0x0d, 0x0e, 0x0f, // word 3
            0x08, 0x09, 0x0a, 0x0b, // word 2
            0x04, 0x05, 0x06, 0x07, // word 1
            0x00, 0x01, 0x02, 0x03, // word 0 (was first)
        ];
        assert_eq!(&frame[18..50], &expected_merkle_wire);

        // Verify prev_block_hash word-swapping
        let expected_prev_hash_wire = [
            0x3c, 0x3d, 0x3e, 0x3f, // word 7 (was last)
            0x38, 0x39, 0x3a, 0x3b, // word 6
            0x34, 0x35, 0x36, 0x37, // word 5
            0x30, 0x31, 0x32, 0x33, // word 4
            0x2c, 0x2d, 0x2e, 0x2f, // word 3
            0x28, 0x29, 0x2a, 0x2b, // word 2
            0x24, 0x25, 0x26, 0x27, // word 1
            0x20, 0x21, 0x22, 0x23, // word 0 (was first)
        ];
        assert_eq!(&frame[50..82], &expected_prev_hash_wire);

        assert_eq!(&frame[82..86], &job.version.to_consensus().to_le_bytes());

        // Verify CRC16 (big-endian)
        assert_eq!(frame.len(), 88);
        let crc_bytes = &frame[86..88];
        let calculated_crc = crc16(&frame[2..86]);
        let frame_crc = u16::from_be_bytes([crc_bytes[0], crc_bytes[1]]);
        assert_eq!(calculated_crc, frame_crc);
    }

    #[test]
    fn job_full_matches_esp_miner_capture() {
        use crate::asic::bm13xx::test_data::esp_miner_job;

        // Build JobFullFormat from high-level Bitcoin types
        // Verify encoding produces exact wire bytes from hardware capture
        let job = JobFullFormat {
            job_id: *esp_miner_job::wire_tx::JOB_ID,
            num_midstates: esp_miner_job::wire_tx::NUM_MIDSTATES_BYTE[0],
            starting_nonce: u32::from_le_bytes(
                (*esp_miner_job::wire_tx::STARTING_NONCE_BYTES)
                    .try_into()
                    .unwrap(),
            ),
            nbits: *esp_miner_job::wire_tx::NBITS,
            ntime: *esp_miner_job::wire_tx::NTIME,
            merkle_root: *esp_miner_job::wire_tx::MERKLE_ROOT,
            prev_block_hash: *esp_miner_job::wire_tx::PREV_BLOCKHASH,
            version: *esp_miner_job::wire_tx::VERSION,
        };

        let mut codec = FrameCodec;
        let mut frame = BytesMut::new();
        codec
            .encode(
                JobCommand::JobFull {
                    job_data: job.clone(),
                },
                &mut frame,
            )
            .expect("Failed to encode job command");

        // Our body bytes match esp-miner's wire capture. Byte 3 (length byte)
        // and bytes 86..88 (CRC16) intentionally differ; see JOB_LENGTH_BYTE.
        assert_eq!(&frame[4..86], &esp_miner_job::wire_tx::FRAME[4..86]);
    }

    #[test]
    fn job_full_matches_s19jpro_factory_capture() {
        let job = job_full_from_wire(&s19jpro_job::wire_tx::FRAME);

        let mut codec = FrameCodec;
        let mut frame = BytesMut::new();
        codec
            .encode(JobCommand::JobFull { job_data: job }, &mut frame)
            .expect("Failed to encode job command");

        // Byte-for-byte match, length byte and CRC16 included: the
        // encoder reproduces factory firmware exactly.
        assert_eq!(&frame[..], &s19jpro_job::wire_tx::FRAME[..]);
    }

    /// Builds a JobFullFormat from a captured JobFull wire frame.
    fn job_full_from_wire(frame: &[u8; 88]) -> JobFullFormat {
        // The wire sends each 32-byte hash as eight 4-byte words, most
        // significant word first; internal order reverses the words.
        fn hash_from_wire(wire: &[u8]) -> [u8; 32] {
            let mut internal = [0u8; 32];
            for i in 0..8 {
                internal[(7 - i) * 4..(8 - i) * 4].copy_from_slice(&wire[i * 4..(i + 1) * 4]);
            }
            internal
        }

        JobFullFormat {
            job_id: frame[4] >> 3,
            num_midstates: frame[5],
            starting_nonce: u32::from_le_bytes(frame[6..10].try_into().unwrap()),
            nbits: CompactTarget::from_consensus(u32::from_le_bytes(
                frame[10..14].try_into().unwrap(),
            )),
            ntime: u32::from_le_bytes(frame[14..18].try_into().unwrap()),
            merkle_root: TxMerkleNode::from_byte_array(hash_from_wire(&frame[18..50])),
            prev_block_hash: BlockHash::from_byte_array(hash_from_wire(&frame[50..82])),
            version: Version::from_consensus(
                u32::from_le_bytes(frame[82..86].try_into().unwrap()) as i32
            ),
        }
    }

    fn assert_frame_eq<C>(cmd: C, expect: &[u8])
    where
        FrameCodec: Encoder<C, Error = io::Error>,
    {
        let mut codec = FrameCodec;
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
        let mut codec = FrameCodec;

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

        let Register::ChipId(ChipId {
            model,
            core_count,
            address,
        }) = register
        else {
            panic!("Expected ChipId register, got {:?}", register);
        };

        assert_eq!(model, ChipModel::BM1370);
        assert_eq!(core_count, 0x00);
        assert_eq!(address, 0x00);
    }

    fn decode_frame(frame: &[u8]) -> Option<Response> {
        let mut buf = BytesMut::from(frame);
        let mut codec = FrameCodec;
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

        // From protocol doc: nonce 0x40A60018 -> Main core 32, nonce value 0x00A60018
        assert_eq!(nonce, 0x40a60018);
        assert_eq!(midstate_num, 0x02);

        // Result header: 0x99 -> bits[7:4]=9 (job_id), bits[3:0]=9 (subcore_id)
        assert_eq!(job_id, 9);
        assert_eq!(subcore_id, 9);

        // Version
        assert_eq!(version, GeneralPurposeBits::new([0x22, 0xF9]));

        // Verify main core extraction
        let main_core = (nonce >> 25) & 0x7f;
        assert_eq!(main_core, 32);
    }

    #[test]
    fn decode_multiple_nonce_responses() {
        // Additional nonce responses from S21 Pro capture
        let test_cases = vec![
            // RX: AA 55 07 35 CD CF 02 5E 00 2E 96
            // result_header=0x5e: bits[7:4]=5, bits[3:0]=14
            // version bytes [0x00, 0x2E] big-endian = 0x002E
            (
                &[
                    0xaa, 0x55, 0x07, 0x35, 0xcd, 0xcf, 0x02, 0x5e, 0x00, 0x2e, 0x96,
                ],
                0xcfcd3507,
                0x02,
                5,
                14,
                GeneralPurposeBits::new([0x00, 0x2E]),
            ),
            // RX: AA 55 46 03 32 E7 00 C3 2C 83 99
            // result_header=0xc3: bits[7:4]=12, bits[3:0]=3
            // version bytes [0x2C, 0x83] big-endian = 0x2C83
            (
                &[
                    0xaa, 0x55, 0x46, 0x03, 0x32, 0xe7, 0x00, 0xc3, 0x2c, 0x83, 0x99,
                ],
                0xe7320346,
                0x00,
                12,
                3,
                GeneralPurposeBits::new([0x2C, 0x83]),
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
        let mut codec = FrameCodec;

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
        let mut codec = FrameCodec;

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
        let mut codec = FrameCodec;

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
        let mut codec = FrameCodec;

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
        let mut codec = FrameCodec;

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
        let mut codec = FrameCodec;

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
        let mut codec = FrameCodec;

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
        let mut codec = FrameCodec;

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
        let mut codec = FrameCodec;

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

    #[test]
    fn decode_nonce_response_from_esp_miner_capture() {
        use crate::asic::bm13xx::test_data::esp_miner_job;

        // Decode nonce response from hardware capture and verify against test data
        let response =
            decode_frame(&esp_miner_job::wire_rx::FRAME).expect("Should decode valid frame");

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

        // Verify all fields match test data
        assert_eq!(nonce, *esp_miner_job::wire_rx::NONCE);
        assert_eq!(midstate_num, *esp_miner_job::wire_rx::MIDSTATE_NUM);
        assert_eq!(job_id, *esp_miner_job::wire_rx::JOB_ID);
        assert_eq!(subcore_id, *esp_miner_job::wire_rx::SUBCORE_ID);
        // VERSION_ROLLING_FIELD is u16, convert to big-endian bytes
        let expected_bytes = esp_miner_job::wire_rx::VERSION_ROLLING_FIELD.to_be_bytes();
        assert_eq!(version, GeneralPurposeBits::new(expected_bytes));

        // Verify version rolling field shifted left 13 matches submit VERSION
        let bits_as_u16 = u16::from_be_bytes(*version.as_bytes());
        let version_shifted = (bits_as_u16 as u32) << 13;
        assert_eq!(
            version_shifted,
            *esp_miner_job::submit::VERSION,
            "Version rolling field << 13 should match mining.submit version"
        );
    }

    #[test]
    fn test_full_mining_round_trip() {
        use crate::asic::bm13xx::test_data::esp_miner_job;
        use crate::types::Difficulty;
        use bitcoin::block::Header as BlockHeader;

        // Build JobFullFormat, encode to wire, decode nonce response,
        // apply version rolling, compute hash, and verify difficulty.
        let job = JobFullFormat {
            job_id: *esp_miner_job::wire_tx::JOB_ID,
            num_midstates: esp_miner_job::wire_tx::NUM_MIDSTATES_BYTE[0],
            starting_nonce: u32::from_le_bytes(
                (*esp_miner_job::wire_tx::STARTING_NONCE_BYTES)
                    .try_into()
                    .unwrap(),
            ),
            nbits: *esp_miner_job::notify::NBITS,
            ntime: *esp_miner_job::notify::NTIME,
            merkle_root: *esp_miner_job::notify::MERKLE_ROOT,
            prev_block_hash: *esp_miner_job::notify::PREV_BLOCKHASH,
            version: *esp_miner_job::notify::VERSION,
        };

        let mut codec = FrameCodec;
        let mut tx_frame = BytesMut::new();
        codec
            .encode(
                JobCommand::JobFull {
                    job_data: job.clone(),
                },
                &mut tx_frame,
            )
            .expect("Should encode JobFull command");

        // Our body bytes match esp-miner's wire capture. Byte 3 (length byte)
        // and bytes 86..88 (CRC16) intentionally differ; see JOB_LENGTH_BYTE.
        assert_eq!(&tx_frame[4..86], &esp_miner_job::wire_tx::FRAME[4..86]);

        let rx_response =
            decode_frame(&esp_miner_job::wire_rx::FRAME).expect("Should decode RX frame");

        let Response::Nonce {
            nonce,
            job_id: rx_job_id,
            version: version_rolling,
            ..
        } = rx_response
        else {
            panic!("Expected Nonce response");
        };

        assert_eq!(rx_job_id, job.job_id, "Job ID should round-trip");

        let full_version = version_rolling.apply_to_version(job.version);
        let header = BlockHeader {
            version: full_version,
            prev_blockhash: job.prev_block_hash,
            merkle_root: job.merkle_root,
            time: job.ntime,
            bits: job.nbits,
            nonce,
        };

        let hash = header.block_hash();
        let difficulty = Difficulty::from_hash(&hash);

        // Allow +/-1 tolerance for integer division rounding
        let expected = Difficulty::from(esp_miner_job::EXPECTED_HASH_DIFFICULTY as u64);
        assert!(
            difficulty >= Difficulty::from(expected.as_u64() - 1)
                && difficulty <= Difficulty::from(expected.as_u64() + 1),
            "Hash difficulty should match esp-miner result"
        );
        assert!(
            difficulty >= Difficulty::from(esp_miner_job::POOL_SHARE_DIFFICULTY_INT),
            "Hash should meet pool difficulty"
        );
    }
}

#[cfg(test)]
mod log2_difficulty_tests {
    use super::*;
    use crate::types::Difficulty;

    #[test]
    fn power_of_two_difficulty_exact() {
        let diff = Log2Difficulty::from_difficulty(Difficulty::from(256_u64));
        assert_eq!(diff.exponent(), 8);
    }

    #[test]
    fn non_power_of_two_floors() {
        // 300 is between 2^8=256 and 2^9=512, should floor to 8
        let diff = Log2Difficulty::from_difficulty(Difficulty::from(300_u64));
        assert_eq!(diff.exponent(), 8);
    }

    #[test]
    fn difficulty_one() {
        let diff = Log2Difficulty::from_difficulty(Difficulty::from(1_u64));
        assert_eq!(diff.exponent(), 0);
    }

    #[test]
    fn large_difficulty() {
        let diff = Log2Difficulty::from_difficulty(Difficulty::from(65536_u64));
        assert_eq!(diff.exponent(), 16);
    }

    #[test]
    fn display() {
        let diff = Log2Difficulty::from_difficulty(Difficulty::from(256_u64));
        assert_eq!(format!("{diff}"), "2^8");
    }

    #[test]
    fn to_work_matches_target_to_work() {
        // Log2Difficulty's to_work should agree with computing work
        // from the equivalent target directly.
        let diff = Log2Difficulty::from_difficulty(Difficulty::from(256_u64));
        let expected = Difficulty::from(256_u64).to_target().to_work();
        assert_eq!(diff.to_work(), expected);
    }
}

#[cfg(test)]
mod ticket_mask_tests {
    use super::*;
    use crate::types::Difficulty;

    #[test]
    fn wire_encoding_difficulty_256() {
        // 8 zero_bits -> mask 0xFF -> [00, 00, 00, FF]
        let diff = Log2Difficulty::from_difficulty(Difficulty::from(256_u64));
        let bytes = TicketMask::new(diff).to_wire_bytes();
        assert_eq!(bytes, [0x00, 0x00, 0x00, 0xFF]);
    }

    #[test]
    fn wire_encoding_difficulty_1024() {
        // 10 zero_bits -> mask 0x3FF -> [00, 00, C0, FF]
        let diff = Log2Difficulty::from_difficulty(Difficulty::from(1024_u64));
        let bytes = TicketMask::new(diff).to_wire_bytes();
        assert_eq!(bytes, [0x00, 0x00, 0xC0, 0xFF]);
    }

    #[test]
    fn wire_encoding_difficulty_65536() {
        // 16 zero_bits -> mask 0xFFFF -> [00, 00, FF, FF]
        let diff = Log2Difficulty::from_difficulty(Difficulty::from(65536_u64));
        let bytes = TicketMask::new(diff).to_wire_bytes();
        assert_eq!(bytes, [0x00, 0x00, 0xFF, 0xFF]);
    }

    #[test]
    fn wire_encoding_difficulty_1() {
        // 0 zero_bits -> [00, 00, 00, 00]
        let diff = Log2Difficulty::from_difficulty(Difficulty::from(1_u64));
        let bytes = TicketMask::new(diff).to_wire_bytes();
        assert_eq!(bytes, [0x00, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn encode_matches_to_wire_bytes() {
        let diff = Log2Difficulty::from_difficulty(Difficulty::from(256_u64));
        let mask = TicketMask::new(diff);
        let mut buf = BytesMut::new();
        mask.encode(&mut buf);
        assert_eq!(&buf[..], &[0x00, 0x00, 0x00, 0xFF]);
    }

    #[test]
    fn reverse_bits_examples() {
        assert_eq!(reverse_bits(0x00), 0x00);
        assert_eq!(reverse_bits(0xFF), 0xFF);
        assert_eq!(reverse_bits(0x01), 0x80);
        assert_eq!(reverse_bits(0x80), 0x01);
        assert_eq!(reverse_bits(0x03), 0xC0);
        assert_eq!(reverse_bits(0x0F), 0xF0);
    }
}

// Bytes go out on the wire least-significant byte first.
// Multi-byte fields are sent most-significant byte first, i.e., big-endian.

/// Protocol handler for BM13xx family chips.
///
/// Encodes high-level operations into chip-specific commands and
/// decodes chip responses into meaningful results.
pub struct BM13xxProtocol {}

impl Default for BM13xxProtocol {
    fn default() -> Self {
        Self::new()
    }
}

impl BM13xxProtocol {
    /// Create a new protocol instance.
    pub fn new() -> Self {
        Self {}
    }

    /// Helper to create a broadcast write command
    fn broadcast_write(&self, register: Register) -> RegisterCommand {
        RegisterCommand::WriteRegister {
            destination: Destination::Broadcast,
            register,
        }
    }

    /// Helper to create a targeted write command
    #[cfg_attr(not(test), allow(dead_code))]
    fn write_to(&self, chip_address: u8, register: Register) -> RegisterCommand {
        RegisterCommand::WriteRegister {
            destination: Destination::Chip(chip_address),
            register,
        }
    }

    /// Returns the initialization sequence for a single chip (e.g., Bitaxe).
    ///
    /// The commands configure the chip for mining:
    /// 1. Enable version rolling
    /// 2. Set PLL parameters for desired frequency using the chip's
    ///    family-specific PLL parameters
    ///
    /// Returns `None` when `frequency` is unreachable for this chip
    /// model.
    pub fn single_chip_init(
        &self,
        chip_config: &ChipConfig,
        frequency: Frequency,
    ) -> Option<Vec<RegisterCommand>> {
        let pll_config = chip_config.calculate_pll(frequency)?;
        Some(vec![
            self.broadcast_write(Register::VersionMask(VersionMask::full_rolling())),
            self.broadcast_write(Register::PllDivider(pll_config)),
        ])
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
    pub fn multi_chip_init(&self, chain_length: usize) -> Vec<RegisterCommand> {
        // Multi-chip initialization register values
        const INIT_CONTROL_VALUE: u32 = 0x00000700;
        const MISC_CONTROL_MULTI_CHIP: u32 = 0x0000c1f0;
        const CORE_REG_INIT_1: u32 = 0x00008b80;
        const CORE_REG_INIT_2: u32 = 0x0c800080;
        const ADDRESS_INCREMENT: u8 = 2;

        // Pre-allocate for efficiency (rough estimate of commands)
        let mut commands = Vec::with_capacity(10 + chain_length);

        // Step 1: Enable version rolling on all chips (broadcast)
        commands.push(self.broadcast_write(Register::VersionMask(VersionMask::full_rolling())));

        // Step 2: Configure init control register
        commands.push(self.broadcast_write(Register::InitControl(InitControl(INIT_CONTROL_VALUE))));

        // Step 3: Configure misc control
        commands.push(
            self.broadcast_write(Register::MiscControl(MiscControl(MISC_CONTROL_MULTI_CHIP))),
        );

        // Step 4: Set chain inactive for address assignment
        commands.push(RegisterCommand::ChainInactive);

        // Step 5: Assign addresses (increment by 2)
        for i in 0..chain_length {
            let address = (i as u8) * ADDRESS_INCREMENT;
            commands.push(RegisterCommand::SetChipAddress {
                chip_address: address,
            });
        }

        // Step 6: Configure core registers on all chips
        commands.push(self.broadcast_write(Register::Core(Core(CORE_REG_INIT_1))));
        commands.push(self.broadcast_write(Register::Core(Core(CORE_REG_INIT_2))));

        // Step 7: Set ticket mask (difficulty 256 = ~1 nonce/sec at 1 TH/s)
        let log2_diff = Log2Difficulty::from_difficulty(Difficulty::from(256_u64));
        commands.push(self.broadcast_write(Register::TicketMask(TicketMask::new(log2_diff))));

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
    pub fn configure_domains(
        &self,
        chain_length: usize,
        chips_per_domain: usize,
    ) -> Vec<RegisterCommand> {
        const UART_RELAY_BASE: u32 = 0x03000000;
        const ADDRESS_INCREMENT: u8 = 2;

        let mut commands = Vec::new();
        let num_domains = chain_length.div_ceil(chips_per_domain);

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
                Register::UartRelay(UartRelay(UART_RELAY_BASE | (relay_offset << 8))),
            ));

            // Configure last chip in domain
            if first_chip != last_chip {
                let last_address = (last_chip as u8) * ADDRESS_INCREMENT;
                commands.push(self.write_to(
                    last_address,
                    Register::UartRelay(UartRelay(UART_RELAY_BASE | (relay_offset << 8))),
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
    pub fn configure_nonce_ranges(&self, chain_length: usize) -> Vec<RegisterCommand> {
        let mut commands = Vec::new();

        // Calculate nonce range based on chain length
        let nonce_config = NonceRange::multi_chip(chain_length);

        // Write nonce range to all chips
        commands.push(self.broadcast_write(Register::NonceRange(nonce_config)));

        commands
    }

    /// Create a command to read a register.
    pub fn read_register(&self, chip_address: u8, register: RegisterAddress) -> RegisterCommand {
        RegisterCommand::ReadRegister {
            destination: Destination::Chip(chip_address),
            register_address: register,
        }
    }

    /// Set UART baud rate on all chips
    pub fn set_baudrate(&self, baudrate: UartBaud) -> RegisterCommand {
        RegisterCommand::WriteRegister {
            destination: Destination::Broadcast,
            register: Register::UartBaud(baudrate),
        }
    }

    /// Create a command to write a register.
    ///
    /// Note: This is a placeholder - actual register encoding depends on the register type
    pub fn write_register(
        &self,
        chip_address: u8,
        register: RegisterAddress,
        value: u32,
    ) -> Result<RegisterCommand, ProtocolError> {
        // TODO: Properly encode register based on type
        // For now, just handle RegA8 as an example
        let register_value = match register {
            RegisterAddress::ChipId => {
                // Can't write chip ID register directly
                return Err(ProtocolError::ReadOnlyRegister(register));
            }
            RegisterAddress::PllDivider => {
                Register::PllDivider(PllDivider::decode(value.to_le_bytes()))
            }
            RegisterAddress::NonceRange => Register::NonceRange(NonceRange {
                bytes: value.to_le_bytes(),
            }),
            RegisterAddress::TicketMask => {
                let bytes = value.to_le_bytes();
                let mask_value = decode_ticket_mask_bytes(&bytes);
                let zero_bits = mask_value.count_ones() as u8;
                Register::TicketMask(TicketMask { zero_bits })
            }
            RegisterAddress::MiscControl => Register::MiscControl(MiscControl(value)),
            RegisterAddress::UartBaud => Register::UartBaud(UartBaud::Custom(value)),
            RegisterAddress::UartRelay => Register::UartRelay(UartRelay(value)),
            RegisterAddress::Core => Register::Core(Core(value)),
            RegisterAddress::AnalogMux => Register::AnalogMux(AnalogMux(value)),
            RegisterAddress::IoDriverStrength => {
                let mut strengths = [0u8; 8];
                for (i, strength) in strengths.iter_mut().enumerate() {
                    *strength = ((value >> (i * 4)) & 0xf) as u8;
                }
                Register::IoDriverStrength(IoDriverStrength { strengths })
            }
            RegisterAddress::Pll3Parameter => Register::Pll3Parameter(Pll3Parameter(value)),
            RegisterAddress::VersionMask => {
                let mask = (value >> 16) as u16;
                let control = (value & 0xffff) as u16;
                Register::VersionMask(VersionMask { mask, control })
            }
            RegisterAddress::InitControl => Register::InitControl(InitControl(value)),
            RegisterAddress::MiscSettings => Register::MiscSettings(MiscSettings(value)),
        };

        Ok(RegisterCommand::WriteRegister {
            destination: Destination::Chip(chip_address),
            register: register_value,
        })
    }

    /// Create a broadcast command to discover all chips.
    pub fn discover_chips() -> RegisterCommand {
        RegisterCommand::ReadRegister {
            destination: Destination::Broadcast,
            register_address: RegisterAddress::ChipId,
        }
    }
}
