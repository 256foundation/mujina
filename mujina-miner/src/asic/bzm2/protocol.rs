//! BZM2 wire protocol and frame codec.
//!
//! This module implements pass-1 support for bring-up:
//! - Command encoding for `NOOP`, `READREG`, `WRITEREG`
//! - Response decoding for `NOOP` and `READREG`
//! - 9-bit TX framing via the BIRDS USB bridge format

use std::io;

use bytes::{Buf, BufMut, Bytes, BytesMut};
use strum::FromRepr;
use tokio_util::codec::{Decoder, Encoder};

use super::error::ProtocolError;
use crate::transport::nine_bit::nine_bit_encode_frame;

pub const ASIC_STRING: &[u8; 3] = b"BZ2";
pub const NOOP_STRING: &[u8; 3] = b"2ZB";
pub const DEFAULT_ASIC_ID: u8 = 0xfa;

pub const ASIC_HW_ID_STRIDE: u8 = 10;
pub const ENGINES_PER_ASIC: usize = 240;

pub const NOTCH_REG: u16 = 0x0fff;
pub const BIST_REG: u16 = 0x0fc0;
pub const BROADCAST_ASIC: u8 = 0xff;
pub const BROADCAST_ENGINE: u16 = 0x00ff;

pub const TERM_BYTE: u8 = 0xa5;
pub const TAR_BYTE: u8 = 0x08;
pub const WRITEJOB_OFFSET: u16 = 41;

fn format_hex(data: &[u8]) -> String {
    data.iter()
        .map(|byte| format!("{:02X}", byte))
        .collect::<Vec<_>>()
        .join(" ")
}

pub mod engine_reg {
    pub const STATUS: u16 = 0x00;
    pub const CONFIG: u16 = 0x01;
    pub const DELAY: u16 = 0x0c;
    pub const MIDSTATE: u16 = 0x10;
    pub const MRRESIDUE: u16 = 0x30;
    pub const START_TIMESTAMP: u16 = 0x34;
    pub const SEQUENCE_ID: u16 = 0x38;
    pub const JOB_CONTROL: u16 = 0x39;
    pub const START_NONCE: u16 = 0x3c;
    pub const END_NONCE: u16 = 0x40;
    pub const TARGET: u16 = 0x44;
    pub const TIMESTAMP_COUNT: u16 = 0x48;
    pub const ZEROS_TO_FIND: u16 = 0x49;
    pub const RESULT_VALID: u16 = 0x70;
    pub const RESULT_SEQUENCE: u16 = 0x71;
    pub const RESULT_TIME: u16 = 0x72;
    pub const RESULT_NONCE: u16 = 0x73;
    pub const RESULT_POP: u16 = 0x77;
}

pub mod local_reg {
    pub const RESULT_STS_CTL: u16 = 0x00;
    pub const ERROR_LOG0: u16 = 0x01;
    pub const ERROR_LOG1: u16 = 0x02;
    pub const ERROR_LOG2: u16 = 0x03;
    pub const ERROR_LOG3: u16 = 0x04;
    pub const SPI_STS_CTL: u16 = 0x05;
    pub const UART_LINE_CTL: u16 = 0x06;
    pub const UART_TDM_CTL: u16 = 0x07;
    pub const SLOW_CLK_DIV: u16 = 0x08;
    pub const TDM_DELAY: u16 = 0x09;
    pub const UART_TX: u16 = 0x0a;
    pub const ASIC_ID: u16 = 0x0b;
    pub const PLL_CNTRL: u16 = 0x0f;
    pub const PLL_POSTDIV: u16 = 0x10;
    pub const PLL_FBDIV: u16 = 0x11;
    pub const PLL_ENABLE: u16 = 0x12;
    pub const PLL_MISC: u16 = 0x13;
    pub const ENG_SOFT_RESET: u16 = 0x16;
    pub const PLL1_CNTRL: u16 = 0x19;
    pub const PLL1_POSTDIV: u16 = 0x1a;
    pub const PLL1_FBDIV: u16 = 0x1b;
    pub const PLL1_ENABLE: u16 = 0x1c;
    pub const PLL1_MISC: u16 = 0x1d;
    pub const UART_SPI_TAP: u16 = 0x20;
    pub const SENS_TDM_GAP_CNT: u16 = 0x2d;
    pub const DTS_SRST_PD: u16 = 0x2e;
    pub const DTS_CFG: u16 = 0x2f;
    pub const TEMPSENSOR_TUNE_CODE: u16 = 0x30;
    pub const THERMAL_TRIP_STATUS: u16 = 0x31;
    pub const THERMAL_TEMP_CODE: u16 = 0x32;
    pub const THERMAL_SAR_COUNT_LOAD: u16 = 0x34;
    pub const THERMAL_SAR_STATE_RESET: u16 = 0x35;
    pub const SENSOR_THRS_CNT: u16 = 0x3c;
    pub const SENSOR_CLK_DIV: u16 = 0x3d;
    pub const VSENSOR_SRST_PD: u16 = 0x3e;
    pub const VSENSOR_CFG: u16 = 0x3f;
    pub const VOLTAGE_SENSOR_ENABLE: u16 = 0x40;
    pub const VOLTAGE_SENSOR_STATUS: u16 = 0x41;
    pub const VOLTAGE_SENSOR_MISC: u16 = 0x42;
    pub const VOLTAGE_SENSOR_DFT: u16 = 0x43;
    pub const BANDGAP: u16 = 0x45;
    pub const LDO_0_CTL_STS: u16 = 0x46;
    pub const LDO_1_CTL_STS: u16 = 0x47;
    pub const IO_PEPS: u16 = 0x50;
    pub const IO_PEPS_DS: u16 = 0x51;
    pub const IO_PUPDST: u16 = 0x52;
    pub const IO_NON_CLK_DS: u16 = 0x53;
    pub const CKDCCR_0_0: u16 = 0x54;
    pub const CKDCCR_1_0: u16 = 0x55;
    pub const CKDCCR_2_0: u16 = 0x56;
    pub const CKDCCR_3_0: u16 = 0x57;
    pub const CKDCCR_4_0: u16 = 0x58;
    pub const CKDCCR_5_0: u16 = 0x59;
    pub const CKDLLR_0_0: u16 = 0x5a;
    pub const CKDLLR_1_0: u16 = 0x5b;
    pub const CKDCCR_0_1: u16 = 0x5c;
    pub const CKDCCR_1_1: u16 = 0x5d;
    pub const CKDCCR_2_1: u16 = 0x5e;
    pub const CKDCCR_3_1: u16 = 0x5f;
    pub const CKDCCR_4_1: u16 = 0x60;
    pub const CKDCCR_5_1: u16 = 0x61;
    pub const CKDLLR_0_1: u16 = 0x62;
    pub const CKDLLR_1_1: u16 = 0x63;
}

pub mod bist_reg {
    pub const RESULT_FSM_CTL: u16 = 0x00;
    pub const ERROR_LOG0: u16 = 0x01;
    pub const ERROR_LOG1: u16 = 0x02;
    pub const ERROR_LOG2: u16 = 0x03;
    pub const ERROR_LOG3: u16 = 0x04;
    pub const ENABLE: u16 = 0x06;
    pub const CONTROL: u16 = 0x07;
    pub const RESULT_TIMEOUT: u16 = 0x08;
    pub const STATUS: u16 = 0x09;
    pub const JOB_COUNT: u16 = 0x0a;
    pub const GAP_COUNT: u16 = 0x0b;
    pub const ENG_CLK_GATE: u16 = 0x0c;
    pub const INT_START_NONCE: u16 = 0x0d;
    pub const INT_END_NONCE: u16 = 0x0e;
    pub const RESULT_SEL: u16 = 0x17;
    pub const EXPECTED_RES_REG0: u16 = 0x18;
    pub const EXPECTED_RES_REG1: u16 = 0x19;
    pub const EXPECTED_RES_REG2: u16 = 0x1a;
    pub const EXPECTED_RES_REG3: u16 = 0x1b;
    pub const EXP_PAT_REG0: u16 = 0x1c;
    pub const EXP_PAT_REG1: u16 = 0x1d;
    pub const EXP_PAT_REG2: u16 = 0x1e;
    pub const EXP_PAT_REG3: u16 = 0x1f;

    pub const fn exp_pat_subjob0(n: u16) -> u16 {
        0x20 + n
    }

    pub const fn exp_pat_subjob1(n: u16) -> u16 {
        0x80 + n
    }

    pub const fn exp_pat_subjob2(n: u16) -> u16 {
        0x94 + n
    }

    pub const fn exp_pat_subjob3(n: u16) -> u16 {
        0xa8 + n
    }

    pub const fn job_tce_row(j: u16, t: u16, r: u16) -> u16 {
        0x30 + (0x50 * j) + (0x14 * t) + r
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, FromRepr)]
#[repr(u8)]
pub enum Opcode {
    WriteJob = 0x0,
    ReadResult = 0x1,
    WriteReg = 0x2,
    ReadReg = 0x3,
    MulticastWrite = 0x4,
    DtsVs = 0x0d,
    Loopback = 0x0e,
    Noop = 0x0f,
}

/// Translate logical ASIC index (0..N) to hardware ASIC ID used on UART.
pub fn logical_to_hw_asic_id(logical_asic: u8) -> u8 {
    logical_asic
        .saturating_add(1)
        .saturating_mul(ASIC_HW_ID_STRIDE)
}

/// Translate hardware ASIC ID from UART into logical ASIC index.
pub fn hw_to_logical_asic_id(hw_asic_id: u8) -> Option<u8> {
    if hw_asic_id < ASIC_HW_ID_STRIDE || hw_asic_id % ASIC_HW_ID_STRIDE != 0 {
        return None;
    }

    Some((hw_asic_id / ASIC_HW_ID_STRIDE) - 1)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Push a job payload to one engine.
    WriteJob {
        asic_hw_id: u8,
        engine: u16,
        midstate: [u8; 32],
        merkle_residue: u32,
        timestamp: u32,
        sequence: u8,
        job_ctl: u8,
    },

    /// Send NOOP command.
    Noop { asic_hw_id: u8 },

    /// Read register value (1/2/4 bytes).
    ReadReg {
        asic_hw_id: u8,
        engine: u16,
        offset: u16,
        count: u8,
    },

    /// Write register value (1-255 bytes).
    WriteReg {
        asic_hw_id: u8,
        engine: u16,
        offset: u16,
        value: Bytes,
    },

    /// Write one or more bytes using opcode 0x4 (row/group write).
    MulticastWrite {
        asic_hw_id: u8,
        group: u16,
        offset: u16,
        value: Bytes,
    },
}

impl Command {
    pub fn write_job(
        asic_hw_id: u8,
        engine: u16,
        midstate: [u8; 32],
        merkle_residue: u32,
        timestamp: u32,
        sequence: u8,
        job_ctl: u8,
    ) -> Self {
        Self::WriteJob {
            asic_hw_id,
            engine,
            midstate,
            merkle_residue,
            timestamp,
            sequence,
            job_ctl,
        }
    }

    /// Build the 4-command enhanced-mode WRITEJOB burst.
    ///
    /// Sequence mapping follows bzmd:
    /// `seq_start = (sequence_id % 2) * 4`, then `seq_start + [0,1,2,3]`.
    /// The first three commands carry `job_ctl=0`; the final command carries
    /// the requested `job_ctl` (must be 1 or 3).
    pub fn write_job_enhanced(
        asic_hw_id: u8,
        engine: u16,
        midstates: [[u8; 32]; 4],
        merkle_residue: u32,
        timestamp: u32,
        sequence_id: u8,
        job_ctl: u8,
    ) -> Result<[Self; 4], ProtocolError> {
        if !matches!(job_ctl, 1 | 3) {
            return Err(ProtocolError::InvalidJobControl(job_ctl));
        }

        let seq_start = (sequence_id % 2) * 4;
        Ok([
            Self::write_job(
                asic_hw_id,
                engine,
                midstates[0],
                merkle_residue,
                timestamp,
                seq_start,
                0,
            ),
            Self::write_job(
                asic_hw_id,
                engine,
                midstates[1],
                merkle_residue,
                timestamp,
                seq_start + 1,
                0,
            ),
            Self::write_job(
                asic_hw_id,
                engine,
                midstates[2],
                merkle_residue,
                timestamp,
                seq_start + 2,
                0,
            ),
            Self::write_job(
                asic_hw_id,
                engine,
                midstates[3],
                merkle_residue,
                timestamp,
                seq_start + 3,
                job_ctl,
            ),
        ])
    }

    pub fn read_reg_u32(asic_hw_id: u8, engine: u16, offset: u16) -> Self {
        Self::ReadReg {
            asic_hw_id,
            engine,
            offset,
            count: 4,
        }
    }

    pub fn write_reg_u8(asic_hw_id: u8, engine: u16, offset: u16, value: u8) -> Self {
        Self::WriteReg {
            asic_hw_id,
            engine,
            offset,
            value: Bytes::copy_from_slice(&[value]),
        }
    }

    pub fn write_reg_u32_le(asic_hw_id: u8, engine: u16, offset: u16, value: u32) -> Self {
        Self::WriteReg {
            asic_hw_id,
            engine,
            offset,
            value: Bytes::copy_from_slice(&value.to_le_bytes()),
        }
    }

    pub fn multicast_write_u8(asic_hw_id: u8, group: u16, offset: u16, value: u8) -> Self {
        Self::MulticastWrite {
            asic_hw_id,
            group,
            offset,
            value: Bytes::copy_from_slice(&[value]),
        }
    }

    fn encode_raw(&self) -> Result<BytesMut, ProtocolError> {
        let mut raw = BytesMut::new();

        match self {
            Self::WriteJob {
                asic_hw_id,
                engine,
                midstate,
                merkle_residue,
                timestamp,
                sequence,
                job_ctl,
            } => {
                // WRITEJOB command:
                // [header:u32_be][midstate:32][merkle_residue:u32_le]
                // [timestamp:u32_le][sequence:u8][job_ctl:u8]
                raw.reserve(46);
                raw.put_u32(build_full_header(
                    *asic_hw_id,
                    Opcode::WriteJob,
                    *engine,
                    WRITEJOB_OFFSET,
                ));
                raw.extend_from_slice(midstate);
                raw.put_u32_le(*merkle_residue);
                raw.put_u32_le(*timestamp);
                raw.put_u8(*sequence);
                raw.put_u8(*job_ctl);
            }
            Self::Noop { asic_hw_id } => {
                // NOOP command:
                // [asic_hw_id][opcode<<4]
                raw.reserve(2);
                raw.put_u16(build_short_header(*asic_hw_id, Opcode::Noop));
            }
            Self::ReadReg {
                asic_hw_id,
                engine,
                offset,
                count,
            } => {
                if !matches!(*count, 1 | 2 | 4) {
                    return Err(ProtocolError::InvalidReadRegCount(*count));
                }

                // READREG command
                // [header:u32_be][count-1][TAR_BYTE]
                raw.reserve(6);
                raw.put_u32(build_full_header(
                    *asic_hw_id,
                    Opcode::ReadReg,
                    *engine,
                    *offset,
                ));
                raw.put_u8(count.saturating_sub(1));
                raw.put_u8(TAR_BYTE);
            }
            Self::WriteReg {
                asic_hw_id,
                engine,
                offset,
                value,
            } => {
                if value.is_empty() {
                    return Err(ProtocolError::EmptyWritePayload);
                }
                if value.len() > usize::from(u8::MAX) {
                    return Err(ProtocolError::WritePayloadTooLarge(value.len()));
                }

                // WRITEREG command (no length prefix):
                // [header:u32_be][count-1][data...]
                raw.reserve(5 + value.len());
                raw.put_u32(build_full_header(
                    *asic_hw_id,
                    Opcode::WriteReg,
                    *engine,
                    *offset,
                ));
                raw.put_u8((value.len() as u8).saturating_sub(1));
                raw.extend_from_slice(value);
            }
            Self::MulticastWrite {
                asic_hw_id,
                group,
                offset,
                value,
            } => {
                if value.is_empty() {
                    return Err(ProtocolError::EmptyWritePayload);
                }
                if value.len() > usize::from(u8::MAX) {
                    return Err(ProtocolError::WritePayloadTooLarge(value.len()));
                }

                raw.reserve(5 + value.len());
                raw.put_u32(build_full_header(
                    *asic_hw_id,
                    Opcode::MulticastWrite,
                    *group,
                    *offset,
                ));
                raw.put_u8((value.len() as u8).saturating_sub(1));
                raw.extend_from_slice(value);
            }
        }

        Ok(raw)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadRegData {
    U8(u8),
    U32(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    Noop { asic_hw_id: u8, signature: [u8; 3] },
    ReadReg { asic_hw_id: u8, data: ReadRegData },
}

/// BZM2 frame codec.
///
/// Encoder emits 9-bit-translated TX bytes (`[data, flag]` pairs) using
/// `nine_bit_encode_frame`. Decoder expects plain 8-bit RX bytes in TDM mode.
#[derive(Debug, Clone)]
pub struct FrameCodec {
    readreg_response_size: usize,
}

impl FrameCodec {
    /// Create codec with explicit READREG response payload size (1 or 4 bytes).
    pub fn new(readreg_response_size: usize) -> Result<Self, ProtocolError> {
        if !matches!(readreg_response_size, 1 | 4) {
            return Err(ProtocolError::UnsupportedReadRegResponseSize(
                readreg_response_size,
            ));
        }

        Ok(Self {
            readreg_response_size,
        })
    }

    fn io_error(err: ProtocolError) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, err)
    }
}

impl Default for FrameCodec {
    fn default() -> Self {
        Self {
            readreg_response_size: 4,
        }
    }
}

impl Encoder<Command> for FrameCodec {
    type Error = io::Error;

    fn encode(&mut self, item: Command, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let raw = item.encode_raw().map_err(Self::io_error)?;
        let encoded = nine_bit_encode_frame(&raw);
        tracing::debug!(
            raw = %format_hex(&raw),
            encoded = %format_hex(&encoded),
            "BZM2 tx frame"
        );
        dst.extend_from_slice(&encoded);
        Ok(())
    }
}

impl Decoder for FrameCodec {
    type Item = Response;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        loop {
            // Minimum frame is [asic_hw_id, opcode]
            if src.len() < 2 {
                return Ok(None);
            }

            let opcode = match Opcode::from_repr(src[1]) {
                Some(op) => op,
                None => {
                    // Byte-level resync when stream is misaligned.
                    tracing::debug!(
                        dropped = format_args!("0x{:02X}", src[0]),
                        next = format_args!("0x{:02X}", src[1]),
                        "BZM2 rx resync: dropping byte"
                    );
                    src.advance(1);
                    continue;
                }
            };

            match opcode {
                Opcode::Noop => {
                    if src.len() < 5 {
                        return Ok(None);
                    }
                    tracing::debug!(rx = %format_hex(&src[..5]), "BZM2 rx NOOP frame");

                    let mut frame = src.split_to(5);
                    let asic_hw_id = frame.get_u8();
                    let _opcode = frame.get_u8();
                    let mut signature = [0u8; 3];
                    frame.copy_to_slice(&mut signature);

                    if signature != *NOOP_STRING {
                        return Err(Self::io_error(ProtocolError::InvalidNoopSignature(
                            signature,
                        )));
                    }

                    return Ok(Some(Response::Noop {
                        asic_hw_id,
                        signature,
                    }));
                }
                Opcode::ReadReg => {
                    let frame_len = 2 + self.readreg_response_size;
                    if src.len() < frame_len {
                        return Ok(None);
                    }
                    tracing::debug!(
                        rx = %format_hex(&src[..frame_len]),
                        "BZM2 rx READREG frame"
                    );

                    let mut frame = src.split_to(frame_len);
                    let asic_hw_id = frame.get_u8();
                    let _opcode = frame.get_u8();
                    let data = match self.readreg_response_size {
                        1 => ReadRegData::U8(frame.get_u8()),
                        4 => ReadRegData::U32(frame.get_u32_le()),
                        n => {
                            return Err(Self::io_error(
                                ProtocolError::UnsupportedReadRegResponseSize(n),
                            ));
                        }
                    };

                    return Ok(Some(Response::ReadReg { asic_hw_id, data }));
                }
                // Pass-1 decoder only surfaces NOOP and READREG. Drop other
                // fixed-length TDM messages so callers can keep waiting for
                // the response type they care about.
                Opcode::ReadResult | Opcode::DtsVs => {
                    const TDM_FIXED_LEN: usize = 10; // [asic:u8][opcode:u8][payload:8]
                    if src.len() < TDM_FIXED_LEN {
                        return Ok(None);
                    }
                    tracing::trace!(
                        opcode = opcode as u8,
                        rx = %format_hex(&src[..TDM_FIXED_LEN]),
                        "BZM2 rx skipping telemetry frame"
                    );
                    src.advance(TDM_FIXED_LEN);
                    continue;
                }
                other => {
                    let preview_len = src.len().min(32);
                    tracing::debug!(
                        opcode = format_args!("0x{:02X}", other as u8),
                        buffer_len = src.len(),
                        buffer_preview = %format_hex(&src[..preview_len]),
                        "BZM2 rx unsupported opcode, resync by dropping one byte"
                    );
                    src.advance(1);
                    continue;
                }
            }
        }
    }
}

fn build_short_header(asic_hw_id: u8, opcode: Opcode) -> u16 {
    ((asic_hw_id as u16) << 8) | ((opcode as u16) << 4)
}

fn build_full_header(asic_hw_id: u8, opcode: Opcode, engine: u16, offset: u16) -> u32 {
    ((asic_hw_id as u32) << 24) | ((opcode as u32) << 20) | ((engine as u32) << 8) | (offset as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_asic_id_translation() {
        assert_eq!(logical_to_hw_asic_id(0), 10);
        assert_eq!(logical_to_hw_asic_id(1), 20);
        assert_eq!(hw_to_logical_asic_id(10), Some(0));
        assert_eq!(hw_to_logical_asic_id(20), Some(1));
        assert_eq!(hw_to_logical_asic_id(9), None);
        assert_eq!(hw_to_logical_asic_id(11), None);
    }

    #[test]
    fn test_encode_noop_frame() {
        let cmd = Command::Noop { asic_hw_id: 0xfa };
        let raw = cmd.encode_raw().expect("encode should succeed");
        assert_eq!(raw.as_ref(), &[0xfa, 0xf0]);

        let mut codec = FrameCodec::default();
        let mut encoded = BytesMut::new();
        codec
            .encode(cmd, &mut encoded)
            .expect("encode should succeed");

        assert_eq!(encoded.as_ref(), &[0xfa, 0x01, 0xf0, 0x00]);
    }

    #[test]
    fn test_encode_readreg_u32_frame() {
        let cmd = Command::read_reg_u32(0x0a, NOTCH_REG, local_reg::ASIC_ID);
        let raw = cmd.encode_raw().expect("encode should succeed");

        // header = (0x0a << 24) | (0x3 << 20) | (0x0fff << 8) | 0x0b
        assert_eq!(raw.as_ref(), &[0x0a, 0x3f, 0xff, 0x0b, 0x03, TAR_BYTE]);
    }

    #[test]
    fn test_encode_writereg_u32_frame() {
        let cmd = Command::write_reg_u32_le(0x0a, NOTCH_REG, local_reg::UART_TX, 0x1234_5678);
        let raw = cmd.encode_raw().expect("encode should succeed");

        // count byte = 4 - 1 = 3
        assert_eq!(
            raw.as_ref(),
            &[0x0a, 0x2f, 0xff, 0x0a, 0x03, 0x78, 0x56, 0x34, 0x12,]
        );
    }

    #[test]
    fn test_encode_multicast_write_u8_frame() {
        let cmd = Command::multicast_write_u8(0x0a, 0x0012, engine_reg::CONFIG, 0x04);
        let raw = cmd.encode_raw().expect("encode should succeed");
        assert_eq!(raw.as_ref(), &[0x0a, 0x40, 0x12, 0x01, 0x00, 0x04]);
    }

    #[test]
    fn test_encode_writejob_frame() {
        let mut midstate = [0u8; 32];
        for (i, byte) in midstate.iter_mut().enumerate() {
            *byte = i as u8;
        }

        let cmd = Command::write_job(0x0a, 0x0123, midstate, 0x1122_3344, 0x5566_7788, 0xfe, 0x03);

        let raw = cmd.encode_raw().expect("encode should succeed");
        assert_eq!(&raw[..4], [0x0a, 0x01, 0x23, 0x29]);
        assert_eq!(&raw[4..36], midstate);
        assert_eq!(&raw[36..40], 0x1122_3344u32.to_le_bytes());
        assert_eq!(&raw[40..44], 0x5566_7788u32.to_le_bytes());
        assert_eq!(raw[44], 0xfe);
        assert_eq!(raw[45], 0x03);
    }

    #[test]
    fn test_writejob_enhanced_builds_four_commands() {
        let mut midstates = [[0u8; 32]; 4];
        midstates[0][0] = 0x10;
        midstates[1][0] = 0x20;
        midstates[2][0] = 0x30;
        midstates[3][0] = 0x40;

        let cmds =
            Command::write_job_enhanced(0x0a, 0x0123, midstates, 0x1122_3344, 0x5566_7788, 0xff, 3)
                .expect("enhanced writejob should build");

        let raw0 = cmds[0].clone().encode_raw().expect("encode should succeed");
        let raw1 = cmds[1].clone().encode_raw().expect("encode should succeed");
        let raw2 = cmds[2].clone().encode_raw().expect("encode should succeed");
        let raw3 = cmds[3].clone().encode_raw().expect("encode should succeed");

        assert_eq!(raw0[44], 4);
        assert_eq!(raw1[44], 5);
        assert_eq!(raw2[44], 6);
        assert_eq!(raw3[44], 7);
        assert_eq!(raw0[45], 0);
        assert_eq!(raw1[45], 0);
        assert_eq!(raw2[45], 0);
        assert_eq!(raw3[45], 3);
        assert_eq!(raw0[4], 0x10);
        assert_eq!(raw1[4], 0x20);
        assert_eq!(raw2[4], 0x30);
        assert_eq!(raw3[4], 0x40);
    }

    #[test]
    fn test_writejob_enhanced_rejects_invalid_job_ctl() {
        let midstates = [[0u8; 32]; 4];
        let err = Command::write_job_enhanced(0x0a, 0x0123, midstates, 0, 0, 0, 0x02)
            .expect_err("invalid job_ctl should fail");
        assert!(matches!(err, ProtocolError::InvalidJobControl(0x02)));
    }

    #[test]
    fn test_decode_noop_response() {
        let mut codec = FrameCodec::default();
        let mut src = BytesMut::from(&[0x0a, Opcode::Noop as u8, b'2', b'Z', b'B'][..]);

        let response = codec.decode(&mut src).expect("decode should succeed");
        assert_eq!(
            response,
            Some(Response::Noop {
                asic_hw_id: 0x0a,
                signature: *NOOP_STRING,
            })
        );
        assert!(src.is_empty());
    }

    #[test]
    fn test_decode_readreg_u32_response() {
        let mut codec = FrameCodec::default();
        let mut src = BytesMut::from(&[0x0a, Opcode::ReadReg as u8, 0x78, 0x56, 0x34, 0x12][..]);

        let response = codec.decode(&mut src).expect("decode should succeed");
        assert_eq!(
            response,
            Some(Response::ReadReg {
                asic_hw_id: 0x0a,
                data: ReadRegData::U32(0x1234_5678),
            })
        );
        assert!(src.is_empty());
    }

    #[test]
    fn test_decode_readreg_u8_response() {
        let mut codec = FrameCodec::new(1).expect("codec should construct");
        let mut src = BytesMut::from(&[0x0a, Opcode::ReadReg as u8, 0xab][..]);

        let response = codec.decode(&mut src).expect("decode should succeed");
        assert_eq!(
            response,
            Some(Response::ReadReg {
                asic_hw_id: 0x0a,
                data: ReadRegData::U8(0xab),
            })
        );
        assert!(src.is_empty());
    }

    #[test]
    fn test_decode_resync_from_garbage() {
        let mut codec = FrameCodec::default();
        let mut src = BytesMut::from(&[0xaa, 0xbb, 0x0a, Opcode::Noop as u8, b'2', b'Z', b'B'][..]);

        let response = codec.decode(&mut src).expect("decode should succeed");
        assert_eq!(
            response,
            Some(Response::Noop {
                asic_hw_id: 0x0a,
                signature: *NOOP_STRING,
            })
        );
        assert!(src.is_empty());
    }

    #[test]
    fn test_decode_skips_tdm_telemetry_before_noop() {
        let mut codec = FrameCodec::default();
        let mut src = BytesMut::from(
            &[
                0x0a,
                Opcode::DtsVs as u8,
                0x00,
                0x01,
                0x02,
                0x03,
                0x04,
                0x05,
                0x06,
                0x07,
                0x0a,
                Opcode::Noop as u8,
                b'2',
                b'Z',
                b'B',
            ][..],
        );

        let response = codec.decode(&mut src).expect("decode should succeed");
        assert_eq!(
            response,
            Some(Response::Noop {
                asic_hw_id: 0x0a,
                signature: *NOOP_STRING,
            })
        );
        assert!(src.is_empty());
    }
}
