//! BM13xx protocol codec wrapper for dissecting captured serial data.
//!
//! This module wraps the driver's FrameCodec to dissect serial frames from
//! captured logic analyzer data. It feeds raw bytes to the same codec used
//! during runtime to ensure consistency.

use crate::capture::{Channel, SerialEvent};
use bytes::{Buf, BytesMut};
use mujina_miner::asic::bm13xx::{
    crc::{crc16, crc5, crc5_is_valid},
    error::ProtocolError,
    protocol::{Command, FrameCodec, JobFullFormat, Register, RegisterAddress, Response},
};
use mujina_miner::tracing::prelude::*;
use std::io;
use tokio_util::codec::Decoder;

/// Direction of serial communication
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// Decoded frame with timing information
#[derive(Debug)]
pub enum DecodedFrame {
    Command {
        timestamp: f64,
        command: Command,
        raw_bytes: Vec<u8>,
        has_errors: bool,
    },
    Response {
        timestamp: f64,
        response: Response,
        raw_bytes: Vec<u8>,
        has_errors: bool,
    },
    Error {
        timestamp: f64,
        error: String,
        raw_bytes: Vec<u8>,
    },
}

impl DecodedFrame {
    pub fn timestamp(&self) -> f64 {
        match self {
            DecodedFrame::Command { timestamp, .. } => *timestamp,
            DecodedFrame::Response { timestamp, .. } => *timestamp,
            DecodedFrame::Error { timestamp, .. } => *timestamp,
        }
    }

    pub fn direction(&self) -> Direction {
        match self {
            DecodedFrame::Command { .. } => Direction::HostToChip,
            DecodedFrame::Response { .. } => Direction::ChipToHost,
            DecodedFrame::Error { .. } => Direction::HostToChip, // Default, could be either
        }
    }
}

/// Codec wrapper that tracks timing and handles both directions
pub struct TimestampedCodec {
    direction: Direction,
    response_codec: FrameCodec,
    command_codec: CommandDecoder,
    buffer: BytesMut,
    // Track byte timestamps and raw data parallel to buffer
    byte_timestamps: Vec<f64>,
    byte_errors: Vec<bool>,
    raw_bytes: Vec<u8>, // Track original bytes for hex output
    // Accumulate consecutive discarded bytes
    pending_discard: Option<PendingDiscard>,
}

/// Tracks consecutive discarded bytes for grouping
#[derive(Debug)]
struct PendingDiscard {
    start_timestamp: f64,
    end_timestamp: f64,
    bytes: Vec<u8>,
    has_errors: bool,
    error_type: String,
}

impl TimestampedCodec {
    /// Create a new timestamped codec for the given direction
    pub fn new(direction: Direction) -> Self {
        Self {
            direction,
            response_codec: FrameCodec::default(),
            command_codec: CommandDecoder::default(),
            buffer: BytesMut::new(),
            byte_timestamps: Vec::new(),
            byte_errors: Vec::new(),
            raw_bytes: Vec::new(),
            pending_discard: None,
        }
    }

    /// Feed a serial event to the codec and get any decoded frames
    pub fn feed_event(&mut self, event: &SerialEvent) -> Vec<DecodedFrame> {
        let mut results = Vec::new();

        // Check if we need to flush pending discard before processing new data
        if let Some(ref pending) = self.pending_discard {
            // If there's a time gap, flush the pending discard
            if event.timestamp - pending.end_timestamp > 0.01 {
                // 10ms gap
                if let Some(discard) = self.pending_discard.take() {
                    results.push(self.create_discard_frame(discard));
                }
            }
        }

        // Add byte to buffer and timestamp tracking
        self.buffer.extend_from_slice(&[event.data]);
        self.byte_timestamps.push(event.timestamp);
        self.byte_errors.push(event.error.is_some());
        self.raw_bytes.push(event.data);

        // Try to decode frames from the buffer
        loop {
            let buffer_len_before = self.buffer.len();
            let timestamp_len_before = self.byte_timestamps.len();

            // Capture buffer content before decoding to track what gets consumed
            let buffer_snapshot = self.buffer.clone();

            match self.direction {
                Direction::HostToChip => {
                    // Use CommandDecoder for command frames
                    match self.command_codec.decode(&mut self.buffer) {
                        Ok(Some(command)) => {
                            let consumed_bytes = buffer_len_before - self.buffer.len();
                            let frame_timestamps = self
                                .byte_timestamps
                                .drain(..consumed_bytes)
                                .collect::<Vec<_>>();
                            let frame_errors =
                                self.byte_errors.drain(..consumed_bytes).collect::<Vec<_>>();

                            // Extract the exact bytes that were consumed from the front of the buffer
                            let frame_bytes = buffer_snapshot[..consumed_bytes].to_vec();
                            // Also drain the corresponding raw_bytes tracking
                            self.raw_bytes.drain(..consumed_bytes);

                            // Flush any pending discard before adding successful frame
                            if let Some(discard) = self.pending_discard.take() {
                                results.push(self.create_discard_frame(discard));
                            }

                            let frame = DecodedFrame::Command {
                                // Use timestamp of the last byte of the frame
                                timestamp: frame_timestamps
                                    .last()
                                    .copied()
                                    .unwrap_or(event.timestamp),
                                command,
                                raw_bytes: frame_bytes,
                                has_errors: frame_errors.iter().any(|&e| e),
                            };
                            results.push(frame);
                        }
                        Ok(None) => {
                            // Need more data - restore timestamp tracking
                            break;
                        }
                        Err(e) => {
                            // Decoder advanced by 1 byte (standard behavior)
                            let consumed_bytes = buffer_len_before - self.buffer.len();
                            if consumed_bytes > 0 {
                                let discarded_timestamps = self
                                    .byte_timestamps
                                    .drain(..consumed_bytes)
                                    .collect::<Vec<_>>();
                                let discarded_errors =
                                    self.byte_errors.drain(..consumed_bytes).collect::<Vec<_>>();
                                let discarded_bytes = buffer_snapshot[..consumed_bytes].to_vec();
                                self.raw_bytes.drain(..consumed_bytes);

                                let timestamp = discarded_timestamps
                                    .last()
                                    .copied()
                                    .unwrap_or(event.timestamp);
                                let has_errors = discarded_errors.iter().any(|&e| e);
                                self.add_to_pending_discard(
                                    timestamp,
                                    discarded_bytes,
                                    has_errors,
                                    format!("Command decode error: {}", e),
                                );
                            }
                        }
                    }
                }
                Direction::ChipToHost => {
                    // Use FrameCodec for response frames
                    match self.response_codec.decode(&mut self.buffer) {
                        Ok(Some(response)) => {
                            let consumed_bytes = buffer_len_before - self.buffer.len();
                            let frame_timestamps = self
                                .byte_timestamps
                                .drain(..consumed_bytes)
                                .collect::<Vec<_>>();
                            let frame_errors =
                                self.byte_errors.drain(..consumed_bytes).collect::<Vec<_>>();

                            // Extract the exact bytes that were consumed from the front of the buffer
                            let frame_bytes = buffer_snapshot[..consumed_bytes].to_vec();
                            // Also drain the corresponding raw_bytes tracking
                            self.raw_bytes.drain(..consumed_bytes);

                            // Flush any pending discard before adding successful frame
                            if let Some(discard) = self.pending_discard.take() {
                                results.push(self.create_discard_frame(discard));
                            }

                            let frame = DecodedFrame::Response {
                                // Use timestamp of the last byte of the frame
                                timestamp: frame_timestamps
                                    .last()
                                    .copied()
                                    .unwrap_or(event.timestamp),
                                response,
                                raw_bytes: frame_bytes,
                                has_errors: frame_errors.iter().any(|&e| e),
                            };
                            results.push(frame);
                        }
                        Ok(None) => {
                            // Decoder either needs more data or advanced buffer
                            let consumed_bytes = buffer_len_before - self.buffer.len();
                            if consumed_bytes > 0 {
                                // Decoder discarded bytes - add to pending discard
                                let discarded_timestamps = self
                                    .byte_timestamps
                                    .drain(..consumed_bytes)
                                    .collect::<Vec<_>>();
                                let discarded_errors =
                                    self.byte_errors.drain(..consumed_bytes).collect::<Vec<_>>();
                                let discarded_bytes = buffer_snapshot[..consumed_bytes].to_vec();
                                self.raw_bytes.drain(..consumed_bytes);

                                let timestamp = discarded_timestamps
                                    .last()
                                    .copied()
                                    .unwrap_or(event.timestamp);
                                let has_errors = discarded_errors.iter().any(|&e| e);
                                self.add_to_pending_discard(
                                    timestamp,
                                    discarded_bytes,
                                    has_errors,
                                    "Response decoder discarded invalid bytes".to_string(),
                                );
                            } else {
                                // Actually need more data
                                break;
                            }
                        }
                        Err(e) => {
                            // Decoder advanced by 1 byte (standard behavior)
                            let consumed_bytes = buffer_len_before - self.buffer.len();
                            if consumed_bytes > 0 {
                                let discarded_timestamps = self
                                    .byte_timestamps
                                    .drain(..consumed_bytes)
                                    .collect::<Vec<_>>();
                                let discarded_errors =
                                    self.byte_errors.drain(..consumed_bytes).collect::<Vec<_>>();
                                let discarded_bytes = buffer_snapshot[..consumed_bytes].to_vec();
                                self.raw_bytes.drain(..consumed_bytes);

                                let timestamp = discarded_timestamps
                                    .last()
                                    .copied()
                                    .unwrap_or(event.timestamp);
                                let has_errors = discarded_errors.iter().any(|&e| e);
                                self.add_to_pending_discard(
                                    timestamp,
                                    discarded_bytes,
                                    has_errors,
                                    format!("Response decode error: {}", e),
                                );
                            }
                        }
                    }
                }
            }

            // Safety check - if buffer didn't change, break to avoid infinite loop
            if self.buffer.len() == buffer_len_before
                && self.byte_timestamps.len() == timestamp_len_before
            {
                break;
            }
        }

        results
    }

    /// Flush any remaining data at end of stream
    pub fn flush(&mut self) -> Vec<DecodedFrame> {
        let mut results = Vec::new();

        // Flush any pending discard
        if let Some(discard) = self.pending_discard.take() {
            results.push(self.create_discard_frame(discard));
        }

        if !self.buffer.is_empty() && !self.byte_timestamps.is_empty() {
            // Create error frame for any remaining data
            let frame = DecodedFrame::Error {
                timestamp: self.byte_timestamps.last().copied().unwrap_or(0.0),
                error: "Incomplete frame at end of stream".to_string(),
                raw_bytes: self.raw_bytes.clone(),
            };
            results.push(frame);
        }

        results
    }

    /// Create a frame from accumulated discarded bytes
    fn create_discard_frame(&self, discard: PendingDiscard) -> DecodedFrame {
        let count = discard.bytes.len();
        let error_msg = if count == 1 {
            format!("{} (1 byte)", discard.error_type)
        } else {
            format!("{} ({} bytes)", discard.error_type, count)
        };

        DecodedFrame::Error {
            timestamp: discard.end_timestamp,
            error: error_msg,
            raw_bytes: discard.bytes,
        }
    }

    /// Add bytes to pending discard or create new discard group
    fn add_to_pending_discard(
        &mut self,
        timestamp: f64,
        bytes: Vec<u8>,
        has_errors: bool,
        error_type: String,
    ) {
        match &mut self.pending_discard {
            Some(ref mut pending) => {
                // Add to existing pending discard
                pending.end_timestamp = timestamp;
                pending.bytes.extend_from_slice(&bytes);
                pending.has_errors = pending.has_errors || has_errors;
            }
            None => {
                // Create new pending discard
                self.pending_discard = Some(PendingDiscard {
                    start_timestamp: timestamp,
                    end_timestamp: timestamp,
                    bytes,
                    has_errors,
                    error_type,
                });
            }
        }
    }
}

/// Command decoder for dissection purposes
///
/// Unlike FrameCodec which decodes responses, this decodes command frames
/// with variable lengths and proper broadcast write register parsing.
#[derive(Default)]
pub struct CommandDecoder {
    last_buffer_size: usize,
}

impl Decoder for CommandDecoder {
    type Item = Command;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        const PREAMBLE: [u8; 2] = [0x55, 0xaa];
        const MIN_FRAME_LEN: usize = 5; // Minimum command frame size
                                        // Return Ok(None) to request more data

        // Log significant buffer changes
        if src.len() != self.last_buffer_size {
            if src.len() > self.last_buffer_size + 5
                || (self.last_buffer_size >= MIN_FRAME_LEN && src.len() < MIN_FRAME_LEN)
            {
                trace!(
                    "Command decoder buffer: {} → {} bytes ({})",
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

        if src.len() < MIN_FRAME_LEN {
            return Ok(None);
        }

        // Check preamble
        if src[0] != PREAMBLE[0] {
            src.advance(1);
            return Ok(None);
        }

        if src[1] != PREAMBLE[1] {
            src.advance(1);
            return Ok(None);
        }

        // Get frame length from length field
        if src.len() < 4 {
            return Ok(None);
        }

        let frame_length = src[3] as usize;
        let total_length = 2 + frame_length; // preamble + frame

        if src.len() < total_length {
            return Ok(None); // Need more data
        }

        // Extract frame data for parsing
        let frame_data = src[..total_length].to_vec();

        // Parse the command frame
        match self.parse_command_frame(&frame_data) {
            Ok(command) => {
                // Only advance if parse was successful
                src.advance(total_length);

                trace!(
                    "TX: {:?} ({} bytes) => {:02x?}",
                    command,
                    total_length,
                    frame_data
                );
                Ok(Some(command))
            }
            Err(err) => {
                trace!("Failed to decode command: {} => {:02x?}", err, frame_data);
                // Advance by 1 byte and continue looking (same pattern as FrameCodec)
                src.advance(1);
                Ok(None)
            }
        }
    }
}

impl CommandDecoder {
    /// Parse a command frame with proper broadcast write register handling
    fn parse_command_frame(&self, data: &[u8]) -> Result<Command, ProtocolError> {
        if data.len() < 5 {
            return Err(ProtocolError::InvalidFrame);
        }

        // data[0..2] is preamble (already validated)
        let type_flags = data[2];
        let _length = data[3] as usize;

        // Parse type flags according to protocol documentation
        let is_work = (type_flags & 0x40) == 0;
        let is_broadcast = (type_flags & 0x10) != 0;
        let cmd = type_flags & 0x1f;

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
            // Register frames use CRC5 - calculate and compare (not crc5_is_valid)
            if data.len() >= 3 {
                let payload = &data[2..data.len() - 1]; // Exclude preamble and CRC byte
                let expected_crc = data[data.len() - 1];
                let calculated_crc = crc5(payload);
                calculated_crc == expected_crc
            } else {
                false
            }
        };

        if !crc_valid {
            trace!("CRC validation failed for frame: {:02x?}", data);
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

        // Parse register commands - with CORRECTED broadcast write register parsing
        let command = match (cmd, is_broadcast) {
            (0, false) => Command::SetChipAddress {
                chip_address: data[4],
            },
            (1, false) => {
                // Non-broadcast write register: chip_addr + reg_addr + data[4]
                if data.len() >= 10 {
                    let chip_address = data[4];
                    let reg_addr = RegisterAddress::from_repr(data[5])
                        .ok_or(ProtocolError::InvalidRegisterAddress(data[5]))?;
                    let value_bytes: [u8; 4] = data[6..10].try_into().unwrap();
                    let register = Register::decode(reg_addr, &value_bytes);
                    Command::WriteRegister {
                        all: false,
                        chip_address,
                        register,
                    }
                } else {
                    return Err(ProtocolError::InvalidFrame);
                }
            }
            (2, false) => {
                // Non-broadcast read register: chip_addr + reg_addr
                if data.len() >= 6 {
                    let chip_address = data[4];
                    let reg_addr = RegisterAddress::from_repr(data[5])
                        .ok_or(ProtocolError::InvalidRegisterAddress(data[5]))?;
                    Command::ReadRegister {
                        all: false,
                        chip_address,
                        register_address: reg_addr,
                    }
                } else {
                    return Err(ProtocolError::InvalidFrame);
                }
            }
            (1, true) => {
                // CORRECTED: Broadcast write register: chip_addr(0x00) + reg_addr + data[4]
                // Protocol doc: | 0x55 0xAA | Type/Flags | Length | Chip_Addr | Reg_Addr | Data[4] | CRC5 |
                if data.len() >= 10 {
                    let chip_address = data[4]; // Should be 0x00 for broadcast
                    let reg_addr = RegisterAddress::from_repr(data[5])
                        .ok_or(ProtocolError::InvalidRegisterAddress(data[5]))?;
                    let value_bytes: [u8; 4] = data[6..10].try_into().unwrap();
                    let register = Register::decode(reg_addr, &value_bytes);
                    Command::WriteRegister {
                        all: true,
                        chip_address,
                        register,
                    }
                } else {
                    return Err(ProtocolError::InvalidFrame);
                }
            }
            (2, true) => {
                // CORRECTED: Broadcast read register: chip_addr(0x00) + reg_addr
                if data.len() >= 6 {
                    let chip_address = data[4]; // Should be 0x00 for broadcast
                    let reg_addr = RegisterAddress::from_repr(data[5])
                        .ok_or(ProtocolError::InvalidRegisterAddress(data[5]))?;
                    Command::ReadRegister {
                        all: true,
                        chip_address,
                        register_address: reg_addr,
                    }
                } else {
                    return Err(ProtocolError::InvalidFrame);
                }
            }
            (3, false) => Command::ChainInactive,
            _ => return Err(ProtocolError::InvalidFrame),
        };

        Ok(command)
    }
}
