//! Frames the host sends to BM13xx chips.
//!
//! [`RegisterCommand`] handles register access and chain addressing
//! on a BM13xx chain. [`JobCommand`] carries mining work. The two
//! travel with different framing on the wire, so they're separate
//! types rather than one combined enum.

use bitcoin::hashes::Hash;
use bitvec::prelude::*;
use bytes::{BufMut, BytesMut};
use futures::sink::Sink;

use super::register::{Register, RegisterAddress};

/// Sink that accepts both BM13xx command families.
pub trait ChipCommandSink<E>:
    Sink<RegisterCommand, Error = E> + Sink<JobCommand, Error = E>
{
}

impl<T, E> ChipCommandSink<E> for T where
    T: Sink<RegisterCommand, Error = E> + Sink<JobCommand, Error = E>
{
}

/// TYPE=2 frames: register reads/writes and chain addressing. Use CRC5.
#[derive(Debug)]
pub enum RegisterCommand {
    SetChipAddress(SetChipAddress),
    ChainInactive(ChainInactive),
    ReadRegister(ReadRegister),
    WriteRegister(WriteRegister),
}

impl RegisterCommand {
    pub(super) fn encode(&self, dst: &mut BytesMut) {
        match self {
            Self::SetChipAddress(c) => c.encode(dst),
            Self::ChainInactive(c) => c.encode(dst),
            Self::ReadRegister(c) => c.encode(dst),
            Self::WriteRegister(c) => c.encode(dst),
        }
    }
}

/// TYPE=1 frames: mining jobs. Use CRC16.
#[derive(Debug)]
pub enum JobCommand {
    /// Full block header; chip calculates midstates internally (BM1370/BM1362 style).
    JobFull(JobFullFormat),
    /// Host pre-calculated midstates (BM1397 style).
    JobMidstate(JobMidstateFormat),
}

impl JobCommand {
    pub(super) fn encode(&self, dst: &mut BytesMut) {
        match self {
            Self::JobFull(j) => j.encode(dst),
            Self::JobMidstate(j) => j.encode(dst),
        }
    }
}

/// Assign an address to the first unaddressed chip via daisy-chain forwarding.
#[derive(Debug, Clone, Copy)]
pub struct SetChipAddress {
    pub chip_address: u8,
}

impl SetChipAddress {
    fn encode(&self, dst: &mut BytesMut) {
        CommandFlags {
            kind: Kind::Command,
            broadcast: false,
            cmd: Cmd::SetChipAddress,
        }
        .encode(dst);
        dst.put_u8(5); // flags + length + chip_addr + reg_addr + crc5
        dst.put_u8(self.chip_address);
        dst.put_u8(0x00); // reserved
    }
}

/// Put all chips into addressing mode (enables daisy-chain forwarding).
#[derive(Debug, Clone, Copy)]
pub struct ChainInactive;

impl ChainInactive {
    fn encode(&self, dst: &mut BytesMut) {
        CommandFlags {
            kind: Kind::Command,
            broadcast: true,
            cmd: Cmd::ChainInactive,
        }
        .encode(dst);
        dst.put_u8(5); // flags + length + reserved + reserved + crc5
        dst.put_u8(0x00);
        dst.put_u8(0x00);
    }
}

/// Read a register from chip(s).
#[derive(Debug, Clone, Copy)]
pub struct ReadRegister {
    pub destination: Destination,
    pub register_address: RegisterAddress,
}

impl ReadRegister {
    fn encode(&self, dst: &mut BytesMut) {
        CommandFlags {
            kind: Kind::Command,
            broadcast: self.destination.is_broadcast(),
            cmd: Cmd::ReadRegister,
        }
        .encode(dst);
        dst.put_u8(5); // flags + length + chip_addr + reg_addr + crc5
        dst.put_u8(self.destination.address_byte());
        dst.put_u8(self.register_address as u8);
    }
}

/// Write a register to chip(s).
#[derive(Debug, Clone)]
pub struct WriteRegister {
    pub destination: Destination,
    pub register: Register,
}

impl WriteRegister {
    fn encode(&self, dst: &mut BytesMut) {
        CommandFlags {
            kind: Kind::Command,
            broadcast: self.destination.is_broadcast(),
            cmd: Cmd::WriteRegisterOrJob,
        }
        .encode(dst);
        dst.put_u8(9); // flags + length + chip_addr + reg_addr + 4 data + crc5
        dst.put_u8(self.destination.address_byte());
        dst.put_u8(self.register.address() as u8);
        self.register.encode(dst);
    }
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

impl JobFullFormat {
    fn encode(&self, dst: &mut BytesMut) {
        CommandFlags {
            kind: Kind::Job,
            broadcast: false,
            cmd: Cmd::WriteRegisterOrJob,
        }
        .encode(dst);

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
        dst.put_u8(54);

        // job_id is a 4-bit value (0-15), encode into bits 6-3 of job_header
        debug_assert!(self.job_id <= 15, "job_id must be 0-15");
        dst.put_u8(self.job_id << 3);
        dst.put_u8(self.num_midstates);
        dst.put_u32_le(self.starting_nonce);
        dst.put_u32_le(self.nbits.to_consensus());
        dst.put_u32_le(self.ntime);

        let merkle_root_bytes = hash_to_wire_bytes(&self.merkle_root.to_byte_array());
        dst.put_slice(&merkle_root_bytes);

        let prev_hash_bytes = hash_to_wire_bytes(&self.prev_block_hash.to_byte_array());
        dst.put_slice(&prev_hash_bytes);

        dst.put_u32_le(self.version.to_consensus() as u32);
    }
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

impl JobMidstateFormat {
    fn encode(&self, dst: &mut BytesMut) {
        CommandFlags {
            kind: Kind::Job,
            broadcast: false,
            cmd: Cmd::WriteRegisterOrJob,
        }
        .encode(dst);

        // Layout: flags + length + 18 header bytes + num_midstates * 32 + crc16 (2)
        dst.put_u8(1 + 1 + 18 + self.num_midstates * 32 + 2);

        // job_id is a 4-bit value (0-15), encode into bits 6-3 of job_header
        debug_assert!(self.job_id <= 15, "job_id must be 0-15");
        dst.put_u8(self.job_id << 3);
        dst.put_u8(self.num_midstates);
        dst.put_slice(&self.starting_nonce);
        dst.put_slice(&self.nbits);
        dst.put_slice(&self.ntime);
        dst.put_slice(&self.merkle4);
        dst.put_slice(&self.midstate0);

        if let Some(midstate) = &self.midstate1 {
            dst.put_slice(midstate);
        }
        if let Some(midstate) = &self.midstate2 {
            dst.put_slice(midstate);
        }
        if let Some(midstate) = &self.midstate3 {
            dst.put_slice(midstate);
        }
    }
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
    for i in 0..8 {
        let src_word = &wire_bytes[i * 4..(i + 1) * 4];
        let dst_word = &mut hash[(7 - i) * 4..(8 - i) * 4];
        dst_word.copy_from_slice(src_word);
    }
    hash
}

/// Flag byte at the start of every TX frame.
struct CommandFlags {
    kind: Kind,
    broadcast: bool,
    cmd: Cmd,
}

impl CommandFlags {
    fn encode(&self, dst: &mut BytesMut) {
        let mut byte = 0u8;
        let field = byte.view_bits_mut::<Lsb0>();
        field[5..7].store(self.kind as u8);
        field[4..5].store(self.broadcast as u8);
        field[0..4].store(self.cmd as u8);
        dst.put_u8(byte);
    }
}

#[derive(Clone, Copy)]
#[repr(u8)]
enum Kind {
    Job = 1,
    Command = 2,
}

#[derive(Clone, Copy)]
#[repr(u8)]
enum Cmd {
    SetChipAddress = 0,
    WriteRegisterOrJob = 1,
    ReadRegister = 2,
    ChainInactive = 3,
}

#[cfg(test)]
mod tests {
    use std::io;

    use bitcoin::block::Version;
    use bitcoin::hash_types::TxMerkleNode;
    use bitcoin::hashes::Hash;
    use bitcoin::{BlockHash, CompactTarget};
    use bytes::BytesMut;
    use tokio_util::codec::Encoder;

    use super::super::codec::FrameCodec;
    use super::super::register::{
        AnalogMux, ChipId, ChipModel, CoreCommand, IoDriverStrength, Log2Difficulty,
        MidstateConfig, MiscControl, NonceRange, Register, RegisterAddress, SoftResetControl,
        TicketMask,
    };
    use super::*;
    use crate::asic::bm13xx::crc::crc16;
    use crate::asic::bm13xx::test_data::{esp_miner_job, s19jpro_job};
    use crate::types::Difficulty;

    #[test]
    fn read_register() {
        assert_frame_eq(
            RegisterCommand::ReadRegister(ReadRegister {
                destination: Destination::Broadcast,
                register_address: RegisterAddress::ChipId,
            }),
            &[0x55, 0xaa, 0x52, 0x05, 0x00, 0x00, 0x0a],
        );
    }

    #[test]
    fn write_register_chip_address() {
        assert_frame_eq(
            RegisterCommand::WriteRegister(WriteRegister {
                destination: Destination::Chip(0x01),
                register: Register::ChipId(ChipId {
                    model: ChipModel::BM1370,
                    core_count: 0x00,
                    address: 0x01,
                }),
            }),
            &[
                0x55, 0xaa, 0x41, 0x09, 0x01, 0x00, 0x13, 0x70, 0x00, 0x01, 0x0a,
            ],
        );
    }

    #[test]
    fn write_midstate_config_from_capture() {
        // From S21 Pro capture: TX: 55 AA 51 09 00 A4 90 00 FF FF 1C
        assert_frame_eq(
            RegisterCommand::WriteRegister(WriteRegister {
                destination: Destination::Broadcast, // 0x51 = broadcast
                register: Register::MidstateConfig(MidstateConfig::full_rolling()),
            }),
            &[
                0x55, 0xaa, 0x51, 0x09, 0x00, 0xa4, 0x90, 0x00, 0xff, 0xff, 0x1c,
            ],
        );
    }

    #[test]
    fn write_soft_reset_defaults_from_capture() {
        // From Bitaxe capture: TX: 55 AA 51 09 00 A8 00 07 00 00 03
        assert_frame_eq(
            RegisterCommand::WriteRegister(WriteRegister {
                destination: Destination::Broadcast,
                register: Register::SoftResetControl(SoftResetControl::defaults(ChipModel::BM1370)),
            }),
            &[
                0x55, 0xaa, 0x51, 0x09, 0x00, 0xa8, 0x00, 0x07, 0x00, 0x00, 0x03,
            ],
        );
    }

    #[test]
    fn write_soft_reset_core_reset_from_capture() {
        // From Bitaxe capture: TX: 55 AA 41 09 00 A8 00 07 01 F0 15
        assert_frame_eq(
            RegisterCommand::WriteRegister(WriteRegister {
                destination: Destination::Chip(0x00),
                register: Register::SoftResetControl(SoftResetControl::core_reset(
                    ChipModel::BM1370,
                )),
            }),
            &[
                0x55, 0xaa, 0x41, 0x09, 0x00, 0xa8, 0x00, 0x07, 0x01, 0xf0, 0x15,
            ],
        );
    }

    #[test]
    fn write_soft_reset_defaults_bm1362_from_capture() {
        // From S19j Pro capture: TX: 55 AA 51 09 00 A8 00 00 00 00 01
        assert_frame_eq(
            RegisterCommand::WriteRegister(WriteRegister {
                destination: Destination::Broadcast,
                register: Register::SoftResetControl(SoftResetControl::defaults(ChipModel::BM1362)),
            }),
            &[
                0x55, 0xaa, 0x51, 0x09, 0x00, 0xa8, 0x00, 0x00, 0x00, 0x00, 0x01,
            ],
        );
    }

    #[test]
    fn write_soft_reset_core_reset_bm1362_from_capture() {
        // From S19j Pro capture: TX: 55 AA 41 09 00 A8 00 00 00 02 03
        assert_frame_eq(
            RegisterCommand::WriteRegister(WriteRegister {
                destination: Destination::Chip(0x00),
                register: Register::SoftResetControl(SoftResetControl::core_reset(
                    ChipModel::BM1362,
                )),
            }),
            &[
                0x55, 0xaa, 0x41, 0x09, 0x00, 0xa8, 0x00, 0x00, 0x00, 0x02, 0x03,
            ],
        );
    }

    #[test]
    fn write_analog_mux_from_capture() {
        // From S21 Pro capture: TX: 55 AA 51 09 00 54 00 00 00 02 18
        assert_frame_eq(
            RegisterCommand::WriteRegister(WriteRegister {
                destination: Destination::Broadcast,
                register: Register::AnalogMux(AnalogMux::bring_up(ChipModel::BM1370)),
            }),
            &[
                0x55, 0xaa, 0x51, 0x09, 0x00, 0x54, 0x00, 0x00, 0x00, 0x02, 0x18,
            ],
        );
    }

    #[test]
    fn write_analog_mux_bm1362_from_capture() {
        // From S19j Pro capture: TX: 55 AA 51 09 00 54 00 00 00 03 1D
        assert_frame_eq(
            RegisterCommand::WriteRegister(WriteRegister {
                destination: Destination::Broadcast,
                register: Register::AnalogMux(AnalogMux::bring_up(ChipModel::BM1362)),
            }),
            &[
                0x55, 0xaa, 0x51, 0x09, 0x00, 0x54, 0x00, 0x00, 0x00, 0x03, 0x1d,
            ],
        );
    }

    #[test]
    fn write_misc_control_from_capture() {
        // From Bitaxe capture: TX: 55 AA 51 09 00 18 F0 00 C1 00 04
        assert_frame_eq(
            RegisterCommand::WriteRegister(WriteRegister {
                destination: Destination::Broadcast,
                register: Register::MiscControl(MiscControl(0x00C100F0)),
            }),
            &[
                0x55, 0xaa, 0x51, 0x09, 0x00, 0x18, 0xf0, 0x00, 0xc1, 0x00, 0x04,
            ],
        );
    }

    #[test]
    fn chain_inactive_from_capture() {
        // From S21 Pro capture: TX: 55 AA 53 05 00 00 03
        assert_frame_eq(
            RegisterCommand::ChainInactive(ChainInactive),
            &[0x55, 0xaa, 0x53, 0x05, 0x00, 0x00, 0x03],
        );
    }

    #[test]
    fn set_chip_address_from_capture() {
        // From S21 Pro capture: TX: 55 AA 40 05 04 00 03 (assign address 0x04)
        assert_frame_eq(
            RegisterCommand::SetChipAddress(SetChipAddress { chip_address: 0x04 }),
            &[0x55, 0xaa, 0x40, 0x05, 0x04, 0x00, 0x03],
        );
    }

    #[test]
    fn write_core_command_from_capture() {
        // From Bitaxe capture: TX: 55 AA 51 09 00 3C 80 00 8B 00 12
        assert_frame_eq(
            RegisterCommand::WriteRegister(WriteRegister {
                destination: Destination::Broadcast,
                register: Register::CoreMailbox(CoreCommand::write_all(
                    CoreCommand::OVERLAP_MONITOR,
                    0x00,
                )),
            }),
            &[
                0x55, 0xaa, 0x51, 0x09, 0x00, 0x3c, 0x80, 0x00, 0x8b, 0x00, 0x12,
            ],
        );
    }

    #[test]
    fn write_core_command_bm1362_from_capture() {
        // From S19j Pro capture: TX: 55 AA 51 09 00 3C 80 00 85 40 0C
        assert_frame_eq(
            RegisterCommand::WriteRegister(WriteRegister {
                destination: Destination::Broadcast,
                register: Register::CoreMailbox(CoreCommand::write_all(
                    CoreCommand::CLOCK_SELECT,
                    0x40,
                )),
            }),
            &[
                0x55, 0xaa, 0x51, 0x09, 0x00, 0x3c, 0x80, 0x00, 0x85, 0x40, 0x0c,
            ],
        );
    }

    #[test]
    fn write_ticket_mask_from_capture() {
        // From S21 Pro capture: TX: 55 AA 51 09 00 14 00 00 00 FF 08
        // Difficulty 256 = 8 zero_bits
        let log2_diff = Log2Difficulty::from_difficulty(Difficulty::from(256_u64));
        assert_frame_eq(
            RegisterCommand::WriteRegister(WriteRegister {
                destination: Destination::Broadcast,
                register: Register::TicketMask(TicketMask::new(log2_diff)),
            }),
            &[
                0x55, 0xaa, 0x51, 0x09, 0x00, 0x14, 0x00, 0x00, 0x00, 0xff, 0x08,
            ],
        );
    }

    #[test]
    fn write_nonce_range_from_capture() {
        // From S21 Pro capture: TX: 55 AA 51 09 00 10 00 00 1E B5 0F
        assert_frame_eq(
            RegisterCommand::WriteRegister(WriteRegister {
                destination: Destination::Broadcast,
                register: Register::NonceRange(NonceRange::multi_chip(65)),
            }),
            &[
                0x55, 0xaa, 0x51, 0x09, 0x00, 0x10, 0x00, 0x00, 0x1e, 0xb5, 0x0f,
            ],
        );
    }

    #[test]
    fn write_io_driver_strength_from_capture() {
        // From S19 J Pro and S21 Pro captures: TX: 55 AA 51 09 00 58 00 01 11 11 0D
        assert_frame_eq(
            RegisterCommand::WriteRegister(WriteRegister {
                destination: Destination::Broadcast,
                register: Register::IoDriverStrength(IoDriverStrength::normal()),
            }),
            &[
                0x55, 0xaa, 0x51, 0x09, 0x00, 0x58, 0x00, 0x01, 0x11, 0x11, 0x0d,
            ],
        );
    }

    #[test]
    fn write_domain_boundary_io_driver_strength_from_capture() {
        // From S21 Pro capture: TX: 55 AA 41 09 08 58 00 01 F1 11 1F
        assert_frame_eq(
            RegisterCommand::WriteRegister(WriteRegister {
                destination: Destination::Chip(0x08),
                register: Register::IoDriverStrength(IoDriverStrength::domain_boundary()),
            }),
            &[
                0x55, 0xaa, 0x41, 0x09, 0x08, 0x58, 0x00, 0x01, 0xf1, 0x11, 0x1f,
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
            .encode(JobCommand::JobFull(job.clone()), &mut frame)
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
            .encode(JobCommand::JobFull(job.clone()), &mut frame)
            .expect("Failed to encode job command");

        // Our body bytes match esp-miner's wire capture. Byte 3 (length
        // byte) and bytes 86..88 (CRC16) intentionally differ; see the
        // length-byte comment in JobFullFormat::encode.
        assert_eq!(&frame[4..86], &esp_miner_job::wire_tx::FRAME[4..86]);
    }

    #[test]
    fn job_full_matches_s19jpro_factory_capture() {
        let job = job_full_from_wire(&s19jpro_job::wire_tx::FRAME);

        let mut codec = FrameCodec;
        let mut frame = BytesMut::new();
        codec
            .encode(JobCommand::JobFull(job), &mut frame)
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
