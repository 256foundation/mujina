//! BM13xx protocol implementation for chip communication.
//!
//! This module handles the encoding and decoding of commands and responses
//! for BM13xx family chips (BM1366, BM1370, etc).

use bitvec::prelude::*;
use bytes::{Buf, BufMut, BytesMut};
use std::io;
use strum::FromRepr;
use tokio_util::codec::{Decoder, Encoder};

use crate::tracing::prelude::*;
use crate::chip::{MiningJob, ChipError};
use super::crc::{crc5, crc5_is_valid, crc16};

#[derive(FromRepr, Copy, Clone)]
#[repr(u8)]
pub enum RegisterAddress {
    ChipAddress = 0x00,
    // MiscControl = 0x18,
    // FastUartConfiguration = 0x28,
    // Pll1Parameter = 0x60,
    // VersionRolling = 0xa4,
    RegA8 = 0xa8,
}

#[derive(Debug)]
pub enum Register {
    ChipAddress {
        chip_id: u16,
        core_count: u8,
        address: u8,
    },
    RegA8 {
        unknown: u32,
    },
}

impl Register {
    fn decode(address: RegisterAddress, bytes: &[u8; 4]) -> Register {
        match address {
            RegisterAddress::ChipAddress => Register::ChipAddress {
                chip_id: u16::from_be_bytes(bytes[0..2].try_into().unwrap()),
                core_count: u8::from_be(bytes[2]),
                address: u8::from_be(bytes[3]),
            },
            RegisterAddress::RegA8 => Register::RegA8 { 
                unknown: u32::from_be_bytes(*bytes)
            },
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
    // SetAddress = 0,
    WriteRegisterOrJob = 1,
    ReadRegister = 2,
    // ChainInactive = 3,
}

pub enum Command {
    ReadRegister {
        all: bool,
        chip_address: u8,
        register_address: RegisterAddress,
    },
    WriteRegister {
        all: bool,
        chip_address: u8,
        register: Register,
    },
    /// Send a job with full block header (BM1370/BM1366 style)
    /// Chip calculates midstates internally
    JobFull {
        job_data: JobFullFormat,
    },
    /// Send a job with pre-calculated midstates (BM1397 style)
    /// Host calculates midstates to save chip computation
    JobMidstate {
        job_data: JobMidstateFormat,
    },
}

/// Full format job structure (BM1370/BM1366).
/// The chip calculates midstates internally from the full block header.
/// All multi-byte values are little-endian in the structure.
/// Hash values (merkle_root, prev_block_hash) are stored in big-endian format.
#[derive(Clone)]
pub struct JobFullFormat {
    pub job_id: u8,
    pub num_midstates: u8,  // Typically 0x01 for BM1370
    pub starting_nonce: [u8; 4],
    pub nbits: [u8; 4],     // Difficulty target
    pub ntime: [u8; 4],     // Timestamp
    pub merkle_root: [u8; 32],     // Full merkle root (big-endian)
    pub prev_block_hash: [u8; 32], // Full previous block hash (big-endian)
    pub version: [u8; 4],   // Block version for version rolling
}

/// Midstate format job structure (BM1397).
/// Host pre-calculates SHA256 midstates to reduce chip workload.
/// Supports up to 4 midstates for version rolling.
#[derive(Clone)]
pub struct JobMidstateFormat {
    pub job_id: u8,
    pub num_midstates: u8,  // 1 or 4 typically
    pub starting_nonce: [u8; 4],
    pub nbits: [u8; 4],     // Difficulty target
    pub ntime: [u8; 4],     // Timestamp
    pub merkle4: [u8; 4],   // Last 4 bytes of merkle root
    pub midstate0: [u8; 32], // Primary midstate
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
            Command::ReadRegister { all, chip_address, register_address } => {
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
                const TOTAL_LEN: u8 = FLAGS_LEN + LENGTH_FIELD_LEN + CHIP_ADDR_LEN + REG_ADDR_LEN + CRC_LEN;
                
                dst.put_u8(TOTAL_LEN);
                dst.put_u8(*chip_address);
                dst.put_u8(*register_address as u8);
            }
            Command::WriteRegister { all, chip_address, register } => {
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
                const TOTAL_LEN: u8 = FLAGS_LEN + LENGTH_FIELD_LEN + CHIP_ADDR_LEN + REG_ADDR_LEN + REG_DATA_LEN + CRC_LEN;

                dst.put_u8(TOTAL_LEN);
                dst.put_u8(*chip_address);
                
                match register {
                    Register::ChipAddress { chip_id, core_count, address } => {
                        dst.put_u8(RegisterAddress::ChipAddress as u8);
                        dst.put_u16(*chip_id);
                        dst.put_u8(*core_count);
                        dst.put_u8(*address);
                    }
                    Register::RegA8 { unknown } => {
                        dst.put_u8(RegisterAddress::RegA8 as u8);
                        dst.put_u32(*unknown);
                    }
                }
            }
            Command::JobFull { job_data } => {
                dst.put_u8(Self::build_flags(
                    CommandFlagsType::Job,
                    false,  // Jobs are never broadcast
                    CommandFlagsCmd::WriteRegisterOrJob,
                ));
                
                const JOB_DATA_LEN: u8 = 82;  // Size of JobFullFormat
                const FLAGS_LEN: u8 = 1;
                const LENGTH_FIELD_LEN: u8 = 1;
                const CRC_LEN: u8 = 2;  // Jobs use CRC16, not CRC5
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
                    false,  // Jobs are never broadcast
                    CommandFlagsCmd::WriteRegisterOrJob,
                ));
                
                // Calculate data length based on number of midstates
                const BASE_LEN: u8 = 18;  // job_id(1) + num_midstates(1) + nonce(4) + nbits(4) + ntime(4) + merkle4(4)
                const MIDSTATE_LEN: u8 = 32;
                let data_len = BASE_LEN + (job_data.num_midstates * MIDSTATE_LEN);
                
                const FLAGS_LEN: u8 = 1;
                const LENGTH_FIELD_LEN: u8 = 1;
                const CRC_LEN: u8 = 2;  // Jobs use CRC16
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
    },
}

impl Response {
    fn decode(bytes: &mut BytesMut, _is_version_rolling: bool) -> Result<Response, String> {
        let type_and_crc = bytes[bytes.len() - 1].view_bits::<Lsb0>();
        let type_repr = type_and_crc[5..].load::<u8>();

        match ResponseType::from_repr(type_repr) {
            Some(ResponseType::ReadRegister) => {
                let value: &[u8; 4] = &bytes.split_to(4)[..].try_into().unwrap();
                let chip_address = bytes.get_u8();
                let register_address_repr = bytes.get_u8();

                if let Some(register_address) = RegisterAddress::from_repr(register_address_repr) {
                    let register = Register::decode(register_address, value);
                    Ok(Response::ReadRegister {
                        chip_address,
                        register,
                    })
                } else {
                    Err(format!(
                        "unknown register address 0x{:x}.",
                        register_address_repr
                    ))
                }
            }
            Some(ResponseType::Nonce) => {
                // BM1370 nonce response format (11 bytes total, including preamble):
                // Already consumed: preamble (2 bytes)
                // Remaining: nonce(4) + midstate_num(1) + job_id(1) + version(2) + crc(1)
                let nonce = bytes.get_u32();
                let midstate_num = bytes.get_u8();
                let job_id = bytes.get_u8();
                let version = bytes.get_u16();
                // CRC already consumed
                
                Ok(Response::Nonce {
                    nonce,
                    job_id,
                    midstate_num,
                    version,
                })
            }
            None => Err(format!("unknown response type 0x{:x}.", type_repr)),
        }
    }
}

#[derive(Default)]
pub struct FrameCodec {
    // Controls whether to use the alternative frame format required when version rolling
    // is enabled. When true, uses version rolling compatible format. (default: false)
    version_rolling: bool,
}

impl Encoder<Command> for FrameCodec {
    type Error = io::Error;

    fn encode(&mut self, command: Command, dst: &mut BytesMut) -> Result<(), Self::Error> {
        const COMMAND_PREAMBLE: &[u8] = &[0x55, 0xaa];
        dst.put_slice(COMMAND_PREAMBLE);

        let start_pos = dst.len();
        command.encode(dst);

        // Jobs use CRC16, other commands use CRC5
        match &command {
            Command::JobFull { .. } | Command::JobMidstate { .. } => {
                // Calculate CRC16 over flags + length + data
                let crc = crc16(&dst[start_pos..]);
                dst.put_u16(crc);
            }
            _ => {
                // Calculate CRC5 over everything after preamble
                let crc = crc5(&dst[2..]);
                dst.put_u8(crc);
            }
        }

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

        const PREAMBLE: &[u8] = &[0xaa, 0x55];
        const NONROLLING_FRAME_LEN: usize = PREAMBLE.len() + 7;
        const ROLLING_FRAME_LEN: usize = PREAMBLE.len() + 9;
        const CALL_AGAIN: Result<Option<Response>, io::Error> = Ok(None);

        let frame_len = if self.version_rolling {
            ROLLING_FRAME_LEN
        } else {
            NONROLLING_FRAME_LEN
        };

        if src.len() < frame_len {
            return CALL_AGAIN;
        }

        let mut prospect = src.clone(); // avoid consuming real buffer as we provisionally parse

        if prospect.get_u8() != PREAMBLE[0] {
            src.advance(1);
            return CALL_AGAIN;
        }

        if prospect.get_u8() != PREAMBLE[1] {
            src.advance(1);
            return CALL_AGAIN;
        }

        if !crc5_is_valid(&prospect[..]) {
            src.advance(1);
            return CALL_AGAIN;
        } else {
            src.advance(frame_len);
        }

        match Response::decode(&mut prospect, self.version_rolling) {
            Ok(response) => Ok(Some(response)),
            Err(msg) => {
                warn!(msg);
                CALL_AGAIN
            }
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
                register_address: RegisterAddress::ChipAddress,
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
                register: Register::ChipAddress {
                    chip_id: 0x1370,
                    core_count: 0x00,
                    address: 0x01,
                },
            },
            &[0x55, 0xaa, 0x41, 0x09, 0x01, 0x00, 0x13, 0x70, 0x00, 0x01, 0x0a],
        );
    }

    fn assert_frame_eq(cmd: Command, expect: &[u8]) {
        let mut codec = FrameCodec::default();
        let mut frame = BytesMut::new();
        codec.encode(cmd, &mut frame).unwrap();
        if frame != expect {
            panic!(
                "mismatch!\nexpected: {}\nactual: {}",
                as_hex(expect),
                as_hex(&frame[..])
            )
        }
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

    #[test]
    fn read_register() {
        let wire = &[0xaa, 0x55, 0x13, 0x70, 0x00, 0x00, 0x00, 0x00, 0x06];
        let response = decode_frame(wire).unwrap();

        let Response::ReadRegister {
            chip_address,
            register,
        } = response
        else {
            panic!();
        };

        assert_eq!(chip_address, 0x00);

        let Register::ChipAddress {
            chip_id,
            core_count,
            address,
        } = register
        else {
            panic!();
        };

        assert_eq!(chip_id, 0x1370);
        assert_eq!(core_count, 0x00);
        assert_eq!(address, 0x00);
    }

    fn decode_frame(frame: &[u8]) -> Option<Response> {
        let mut buf = BytesMut::from(frame);
        let mut codec = FrameCodec::default();
        codec.decode(&mut buf).unwrap()
    }
}

// Bytes go out on the wire least-significant byte first.
// Multi-byte fields are sent most-significant byte first, i.e., big-endian.

/// Protocol handler for BM13xx family chips.
/// 
/// Encodes high-level operations into chip-specific commands and
/// decodes chip responses into meaningful results.
pub struct BM13xxProtocol {
    /// Whether version rolling is enabled
    version_rolling: bool,
}

impl BM13xxProtocol {
    /// Create a new protocol instance.
    pub fn new(version_rolling: bool) -> Self {
        Self { version_rolling }
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
            num_midstates: 0x01,  // BM1370 typically uses 1
            starting_nonce: [0x00, 0x00, 0x00, 0x00],  // Start at 0
            nbits: job.nbits.to_le_bytes(),
            ntime: job.ntime.to_le_bytes(),
            merkle_root: job.merkle_root,  // Should be big-endian
            prev_block_hash: job.prev_block_hash,  // Should be big-endian
            version: job.version.to_le_bytes(),
        };
        
        Command::JobFull { job_data }
    }
    
    /// Get the initialization sequence for a chip.
    /// 
    /// Returns a vector of commands to configure the chip for mining:
    /// 1. Set PLL parameters for desired frequency
    /// 2. Enable version rolling if supported
    /// 3. Configure other chip-specific settings
    pub fn initialization_sequence(&self, chip_address: u8) -> Vec<Command> {
        let mut commands = Vec::new();
        
        // TODO: Add actual initialization commands
        // For now, just read the chip address register as a test
        commands.push(Command::ReadRegister {
            all: false,
            chip_address,
            register_address: RegisterAddress::ChipAddress,
        });
        
        commands
    }
    
    /// Decode a response into a mining result.
    pub fn decode_response(&self, response: Response, _chip_address: u8) -> Result<MiningResult, ChipError> {
        match response {
            Response::ReadRegister { chip_address: _, register } => {
                Ok(MiningResult::RegisterRead(register))
            }
            Response::Nonce { nonce, job_id, midstate_num: _, version } => {
                // Extract core ID from nonce (bits 25-31)
                let core_id = ((nonce >> 25) & 0x7f) as u8;
                
                // Extract actual job ID (upper 7 bits of job_id field, shifted left by 1)
                let actual_job_id = ((job_id & 0xf0) >> 1) as u64;
                
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
    pub fn read_register(&self, chip_address: u8, register: RegisterAddress) -> Command {
        Command::ReadRegister {
            all: false,
            chip_address,
            register_address: register,
        }
    }
    
    /// Create a command to write a register.
    /// 
    /// Note: This is a placeholder - actual register encoding depends on the register type
    pub fn write_register(&self, chip_address: u8, register: RegisterAddress, value: u32) -> Command {
        // TODO: Properly encode register based on type
        // For now, just handle RegA8 as an example
        let register_value = match register {
            RegisterAddress::ChipAddress => {
                // Can't write chip address register
                panic!("Cannot write to chip address register");
            }
            RegisterAddress::RegA8 => Register::RegA8 { unknown: value },
        };
        
        Command::WriteRegister {
            all: false,
            chip_address,
            register: register_value,
        }
    }
    
    /// Create a broadcast command to discover all chips.
    pub fn discover_chips() -> Command {
        Command::ReadRegister {
            all: true,  // Broadcast
            chip_address: 0,
            register_address: RegisterAddress::ChipAddress,
        }
    }
}

/// Results from protocol operations
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
