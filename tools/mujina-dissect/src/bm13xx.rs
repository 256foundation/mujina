//! BM13xx ASIC protocol parsing for dissecting captured serial data.
//!
//! This module wraps the driver's FrameCodec to dissect BM13xx ASIC protocol
//! frames from captured logic analyzer data. It feeds raw bytes to the same
//! codec used during runtime to ensure consistency between dissection and
//! live operation.
//!
//! Note: Any codec implementation specific to the dissector is a candidate for
//! replacing the mujina-miner codec. The dissector currently experiments with
//! different parsing strategies, but the goal is to converge on a single codec
//! implementation shared between the dissector and the main miner.

use crate::capture::{BaudRate, Channel, SerialEvent};
use bytes::{Buf, BytesMut};
use mujina_miner::asic::bm13xx::{
    crc::{crc16, crc5},
    error::ProtocolError,
    protocol::{Command, FrameCodec, JobFullFormat, Register, RegisterAddress, Response},
};
use std::collections::VecDeque;
use tokio_util::codec::Decoder;

/// Direction of serial communication
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Host to ASIC (CI channel)
    HostToChip,
    /// ASIC to Host (RO channel)
    ChipToHost,
}

impl From<Channel> for Direction {
    fn from(channel: Channel) -> Self {
        match channel {
            Channel::CI => Direction::HostToChip,
            Channel::RO => Direction::ChipToHost,
        }
    }
}

/// Parsed item from streaming parser
#[derive(Debug)]
pub enum ParsedItem {
    ValidFrame {
        command: Command,
        raw_bytes: Vec<u8>,
        timestamps: Vec<f64>,
    },
    ValidResponse {
        response: Response,
        raw_bytes: Vec<u8>,
        timestamps: Vec<f64>,
    },
    InvalidBytes {
        _raw_bytes: Vec<u8>,
        _timestamps: Vec<f64>,
        _reason: String,
    },
}

/// Decoded frame with timing information (legacy)
#[derive(Debug)]
pub enum DecodedFrame {
    Command {
        timestamp: f64,
        command: Command,
        raw_bytes: Vec<u8>,
        _has_errors: bool,
        baud_rate: BaudRate,
    },
    Response {
        timestamp: f64,
        response: Response,
        raw_bytes: Vec<u8>,
        _has_errors: bool,
        baud_rate: BaudRate,
    },
}

/// Streaming command parser for CI (host-to-chip) streams
pub struct CommandStreamingParser {
    buffer: BytesMut,
    byte_queue: VecDeque<(u8, f64)>, // (byte, timestamp)
    invalid_accumulator: Vec<(u8, f64)>,
}

impl CommandStreamingParser {
    pub fn new() -> Self {
        Self {
            buffer: BytesMut::new(),
            byte_queue: VecDeque::new(),
            invalid_accumulator: Vec::new(),
        }
    }

    /// Process a single serial event and yield any completed frames
    pub fn process_event(&mut self, event: &SerialEvent) -> impl Iterator<Item = ParsedItem> + '_ {
        // Add byte to queue with timestamp - skip events with framing errors
        if event.error.is_none() {
            self.byte_queue.push_back((event.data, event.timestamp));
        }

        // Try to extract frames from the queue
        std::iter::from_fn(move || self.try_extract_frame())
    }

    fn try_extract_frame(&mut self) -> Option<ParsedItem> {
        const PREAMBLE: [u8; 2] = [0x55, 0xaa];

        // Convert queue to buffer for parsing
        self.sync_buffer_from_queue();

        if self.buffer.len() < 5 {
            return None; // Need more data
        }

        // Check for valid preamble
        if self.buffer[0] != PREAMBLE[0] || self.buffer[1] != PREAMBLE[1] {
            // Invalid byte - accumulate it
            if let Some((byte, timestamp)) = self.byte_queue.pop_front() {
                self.invalid_accumulator.push((byte, timestamp));
                self.buffer.advance(1);
            }

            // Check if we should flush accumulated invalid bytes
            if !self.invalid_accumulator.is_empty() && self.looks_like_frame_start() {
                return self.flush_invalid_bytes();
            }

            return None;
        }

        // Flush any accumulated invalid bytes first
        if !self.invalid_accumulator.is_empty() {
            return self.flush_invalid_bytes();
        }

        // Try to parse a valid frame
        let frame_length = self.buffer[3] as usize;
        let total_length = 2 + frame_length;

        if self.buffer.len() < total_length {
            return None; // Need more data
        }

        // Extract frame data and timestamps
        let frame_bytes = self.buffer[..total_length].to_vec();
        let frame_timestamps: Vec<f64> = self
            .byte_queue
            .drain(..total_length)
            .map(|(_, ts)| ts)
            .collect();

        self.buffer.advance(total_length);

        // Parse the command frame
        match parse_bm13xx_command_frame(&frame_bytes) {
            Ok(command) => Some(ParsedItem::ValidFrame {
                command,
                raw_bytes: frame_bytes,
                timestamps: frame_timestamps,
            }),
            Err(_) => Some(ParsedItem::InvalidBytes {
                _raw_bytes: frame_bytes,
                _timestamps: frame_timestamps,
                _reason: "crc_failed".to_string(),
            }),
        }
    }

    fn sync_buffer_from_queue(&mut self) {
        // Simpler approach: always rebuild buffer from queue
        // This ensures buffer exactly matches queue state
        self.buffer.clear();
        let queue_bytes: Vec<u8> = self.byte_queue.iter().map(|(b, _)| *b).collect();
        self.buffer.extend_from_slice(&queue_bytes);
    }

    fn looks_like_frame_start(&self) -> bool {
        self.buffer.len() >= 2 && self.buffer[0] == 0x55 && self.buffer[1] == 0xaa
    }

    fn flush_invalid_bytes(&mut self) -> Option<ParsedItem> {
        if self.invalid_accumulator.is_empty() {
            return None;
        }

        let (bytes, timestamps): (Vec<u8>, Vec<f64>) = self.invalid_accumulator.drain(..).unzip();
        Some(ParsedItem::InvalidBytes {
            _raw_bytes: bytes,
            _timestamps: timestamps,
            _reason: "invalid_preamble".to_string(),
        })
    }
}

/// Streaming response parser for RO (chip-to-host) streams
pub struct ResponseStreamingParser {
    buffer: BytesMut,
    byte_queue: VecDeque<(u8, f64)>, // (byte, timestamp)
    invalid_accumulator: Vec<(u8, f64)>,
    frame_codec: FrameCodec, // Reuse existing response decoder
}

impl ResponseStreamingParser {
    pub fn new() -> Self {
        Self {
            buffer: BytesMut::new(),
            byte_queue: VecDeque::new(),
            invalid_accumulator: Vec::new(),
            frame_codec: FrameCodec::default(),
        }
    }

    /// Process a single serial event and yield any completed frames
    pub fn process_event(&mut self, event: &SerialEvent) -> impl Iterator<Item = ParsedItem> + '_ {
        // Add byte to queue with timestamp - skip events with framing errors for now
        if event.error.is_none() {
            self.byte_queue.push_back((event.data, event.timestamp));
        }

        // Try to extract frames from the queue
        std::iter::from_fn(move || self.try_extract_frame())
    }

    fn try_extract_frame(&mut self) -> Option<ParsedItem> {
        // Convert queue to buffer for parsing
        self.sync_buffer_from_queue();

        if self.buffer.len() < 3 {
            return None; // Need more data for minimum response frame
        }

        // First, flush any accumulated invalid bytes if we see a valid response start
        if !self.invalid_accumulator.is_empty() && self.looks_like_response_start() {
            return self.flush_invalid_bytes();
        }

        // Remove invalid leading bytes that aren't response preamble
        while !self.buffer.is_empty() && self.buffer[0] != 0xaa {
            if let Some((byte, timestamp)) = self.byte_queue.pop_front() {
                self.invalid_accumulator.push((byte, timestamp));
                self.buffer.advance(1);
            } else {
                break;
            }
        }

        // Try to decode using existing FrameCodec
        let buffer_before = self.buffer.clone();
        match self.frame_codec.decode(&mut self.buffer) {
            Ok(Some(response)) => {
                let consumed_bytes = buffer_before.len() - self.buffer.len();
                let frame_bytes = buffer_before[..consumed_bytes].to_vec();
                let frame_timestamps: Vec<f64> = self
                    .byte_queue
                    .drain(..consumed_bytes)
                    .map(|(_, ts)| ts)
                    .collect();

                Some(ParsedItem::ValidResponse {
                    response,
                    raw_bytes: frame_bytes,
                    timestamps: frame_timestamps,
                })
            }
            Ok(None) => None, // Need more data
            Err(_) => {
                // Invalid byte - accumulate it
                if let Some((byte, timestamp)) = self.byte_queue.pop_front() {
                    self.invalid_accumulator.push((byte, timestamp));
                    self.buffer.advance(1);
                }

                // Flush invalid bytes if we see a potential frame start
                if !self.invalid_accumulator.is_empty() && self.looks_like_response_start() {
                    self.flush_invalid_bytes()
                } else {
                    None
                }
            }
        }
    }

    fn sync_buffer_from_queue(&mut self) {
        // Simpler approach: always rebuild buffer from queue
        // This ensures buffer exactly matches queue state
        self.buffer.clear();
        let queue_bytes: Vec<u8> = self.byte_queue.iter().map(|(b, _)| *b).collect();
        self.buffer.extend_from_slice(&queue_bytes);
    }

    fn looks_like_response_start(&self) -> bool {
        self.buffer.len() >= 2 && self.buffer[0] == 0xaa && self.buffer[1] == 0x55
    }

    fn flush_invalid_bytes(&mut self) -> Option<ParsedItem> {
        if self.invalid_accumulator.is_empty() {
            return None;
        }

        let (bytes, timestamps): (Vec<u8>, Vec<f64>) = self.invalid_accumulator.drain(..).unzip();
        Some(ParsedItem::InvalidBytes {
            _raw_bytes: bytes,
            _timestamps: timestamps,
            _reason: "invalid_response".to_string(),
        })
    }
}

/// Standalone BM13xx command frame parser (shared between old and new implementations)
fn parse_bm13xx_command_frame(data: &[u8]) -> Result<Command, ProtocolError> {
    if data.len() < 5 {
        return Err(ProtocolError::InvalidFrame);
    }

    // data[0..2] is preamble (already validated)
    let type_flags = data[2];
    let _length = data[3] as usize;

    // Parse type flags according to protocol documentation
    let is_work = (type_flags & 0x40) == 0;
    let is_broadcast = (type_flags & 0x10) != 0;
    let cmd = type_flags & 0x0f;

    // Validate CRC
    let crc_valid = if is_work {
        // Work frames use CRC16
        if data.len() >= 4 {
            let payload_end = data.len() - 2;
            let crc_bytes = &data[payload_end..];
            let payload = &data[2..payload_end];
            let expected_crc = u16::from_be_bytes([crc_bytes[0], crc_bytes[1]]);
            let calculated_crc = crc16(payload);
            calculated_crc == expected_crc
        } else {
            false
        }
    } else {
        // Register frames use CRC5
        if data.len() >= 3 {
            let payload = &data[2..data.len() - 1];
            let expected_crc = data[data.len() - 1];
            let calculated_crc = crc5(payload);
            calculated_crc == expected_crc
        } else {
            false
        }
    };

    if !crc_valid {
        return Err(ProtocolError::InvalidFrame);
    }

    if is_work {
        // Parse work frame (JobFull)
        let job_data_len = _length - 4;
        if job_data_len == 82 && data.len() >= 2 + _length {
            let job_data_bytes = &data[4..(4 + 82)];
            let job_data = JobFullFormat {
                job_id: job_data_bytes[0],
                num_midstates: job_data_bytes[1],
                starting_nonce: job_data_bytes[2..6].try_into().unwrap(),
                nbits: job_data_bytes[6..10].try_into().unwrap(),
                ntime: job_data_bytes[10..14].try_into().unwrap(),
                merkle_root: job_data_bytes[14..46].try_into().unwrap(),
                prev_block_hash: job_data_bytes[46..78].try_into().unwrap(),
                version: job_data_bytes[78..82].try_into().unwrap(),
            };
            return Ok(Command::JobFull { job_data });
        } else {
            return Err(ProtocolError::InvalidFrame);
        }
    }

    // Parse register commands
    let command = match (cmd, is_broadcast) {
        (0, false) => Command::SetChipAddress {
            chip_address: data[4],
        },
        (1, true) => {
            let register_address =
                RegisterAddress::from_repr(data[5]).ok_or(ProtocolError::InvalidFrame)?;
            let register_value = [data[6], data[7], data[8], data[9]];
            let register = Register::decode(register_address, &register_value);
            Command::WriteRegister {
                broadcast: true,
                chip_address: 0,
                register,
            }
        }
        (1, false) => {
            let register_address =
                RegisterAddress::from_repr(data[5]).ok_or(ProtocolError::InvalidFrame)?;
            let register_value = [data[6], data[7], data[8], data[9]];
            let register = Register::decode(register_address, &register_value);
            Command::WriteRegister {
                broadcast: false,
                chip_address: data[4],
                register,
            }
        }
        (2, true) => {
            let register_address =
                RegisterAddress::from_repr(data[5]).ok_or(ProtocolError::InvalidFrame)?;
            Command::ReadRegister {
                broadcast: true,
                chip_address: 0,
                register_address,
            }
        }
        (2, false) => {
            let register_address =
                RegisterAddress::from_repr(data[5]).ok_or(ProtocolError::InvalidFrame)?;
            Command::ReadRegister {
                broadcast: false,
                chip_address: data[4],
                register_address,
            }
        }
        _ => return Err(ProtocolError::InvalidFrame),
    };

    Ok(command)
}

impl DecodedFrame {
    pub fn timestamp(&self) -> f64 {
        match self {
            DecodedFrame::Command { timestamp, .. } => *timestamp,
            DecodedFrame::Response { timestamp, .. } => *timestamp,
        }
    }

    pub fn direction(&self) -> Direction {
        match self {
            DecodedFrame::Command { .. } => Direction::HostToChip,
            DecodedFrame::Response { .. } => Direction::ChipToHost,
        }
    }

    pub fn baud_rate(&self) -> BaudRate {
        match self {
            DecodedFrame::Command { baud_rate, .. } => *baud_rate,
            DecodedFrame::Response { baud_rate, .. } => *baud_rate,
        }
    }
}
