//! Bitaxe-raw control protocol implementation.
//!
//! This module implements the packet-based control protocol used by the
//! [bitaxe-raw](https://github.com/bitaxeorg/bitaxe-raw) firmware for
//! managing board peripherals over the control serial channel.
//!
//! # Protocol Overview
//!
//! The protocol tunnels I2C, GPIO, and ADC operations over USB serial using
//! a request-response packet structure. Each request includes an ID that is
//! echoed in the response for correlation.
//!
//! ## Packet Format
//!
//! ```text
//! Request:  [Length:2 LE] [ID:1] [Bus:1] [Page:1] [Command:1] [Data:N]
//! Response: [Length:2 LE] [ID:1] [Data:N]
//! ```
//!
//! **Length field encoding differs between requests and responses:**
//! - **Requests**: Length = total packet size (all fields including length itself)
//! - **Responses**: Length = data bytes only (not including length field or ID)
//!   - Response packet size = 2 (length field) + 1 (ID) + length
//!
//! ## Pages
//!
//! - `0x05` - I2C operations (peripheral communication)
//! - `0x06` - GPIO operations (ASIC reset, status pins)
//! - `0x07` - ADC operations (voltage monitoring)
//!
//! The bus field is always `0x00` in current firmware.
//!
//! ## GPIO Operations
//!
//! For GPIO, the command byte is the pin number itself:
//! - Write: `[pin] [0x00 or 0x01]` (low/high)
//! - Read: `[pin]` -> Response: `[0x00 or 0x01]`
//!
//! ## I2C Operations
//!
//! Standard I2C operations with 7-bit addressing:
//! - Write: `[addr] [data...]`
//! - Read: `[addr] [read_len]` -> Response: `[data...]`
//! - Write-Read: `[addr] [write_data...] [read_len]` -> Response: `[data...]`
//!
//! ## Error Responses
//!
//! Errors are indicated by a response data field starting with `0xFF` followed
//! by an error code. See [`ErrorCode`] for defined error types.

pub mod channel;
pub mod gpio;
pub mod i2c;

use crate::tracing::prelude::*;
use bytes::{BufMut, BytesMut};
use std::{fmt, io};
use tokio_util::codec::{Decoder, Encoder};

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

/// Error response marker
const ERROR_MARKER: u8 = 0xff;

/// Control protocol pages
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Page {
    /// I2C operations (EMC2101, TMP75, INA260)
    I2C = 0x05,
    /// GPIO operations (ASIC reset control)
    GPIO = 0x06,
    /// ADC operations (voltage monitoring)
    ADC = 0x07,
}

/// I2C commands
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum I2CCommand {
    SetFrequency = 0x10,
    Write = 0x20,
    Read = 0x30,
    WriteRead = 0x40,
}

// Note: For GPIO page, the command byte is the pin number itself

/// ADC commands
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ADCCommand {
    ReadVDD = 0x50,
}

/// Control protocol error codes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ErrorCode {
    Timeout = 0x10,
    InvalidCommand = 0x11,
    BufferOverflow = 0x12,
    Custom = 0xff,
}

impl TryFrom<u8> for ErrorCode {
    type Error = u8;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            x if x == Self::Timeout as u8 => Ok(Self::Timeout),
            x if x == Self::InvalidCommand as u8 => Ok(Self::InvalidCommand),
            x if x == Self::BufferOverflow as u8 => Ok(Self::BufferOverflow),
            x if x == Self::Custom as u8 => Ok(Self::Custom),
            _ => Err(value),
        }
    }
}

/// Control protocol packet
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    /// Command ID (echoed in response)
    pub id: u8,
    /// Bus (always 0x00)
    pub bus: u8,
    /// Command page
    pub page: Page,
    /// Page-specific command
    pub command: u8,
    /// Command data
    pub data: Vec<u8>,
}

impl Packet {
    /// Create a new packet with default bus (0x00)
    pub fn new(id: u8, page: Page, command: u8, data: Vec<u8>) -> Self {
        Self {
            id,
            bus: 0x00,
            page,
            command,
            data,
        }
    }

    /// Write a GPIO pin value.
    pub fn gpio_write(id: u8, pin: u8, value: bool) -> Self {
        let data = vec![if value { 0x01 } else { 0x00 }];
        Self::new(id, Page::GPIO, pin, data)
    }

    /// Read a GPIO pin value.
    pub fn gpio_read(id: u8, pin: u8) -> Self {
        Self::new(id, Page::GPIO, pin, vec![])
    }

    /// Encode packet to bytes
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();

        // Calculate total length: 2 (length) + 1 (id) + 1 (bus) + 1 (page) + 1 (command) + data
        let length = (6 + self.data.len()) as u16;

        // Length (little-endian)
        buf.put_u16_le(length);
        // ID
        buf.put_u8(self.id);
        // Bus
        buf.put_u8(self.bus);
        // Page
        buf.put_u8(self.page as u8);
        // Command
        buf.put_u8(self.command);
        // Data
        buf.extend_from_slice(&self.data);

        buf
    }
}

/// Control protocol response
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// Command ID (echoed from request)
    pub id: u8,
    /// Response data (empty on error)
    pub data: Vec<u8>,
    /// Error if response indicates failure
    pub error: Option<ResponseError>,
}

/// Response error details
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseError {
    pub code: ErrorCode,
    pub message: Option<String>,
}

impl Response {
    /// Parse a response from bytes
    pub fn parse(bytes: &[u8]) -> Result<Self, io::Error> {
        if bytes.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Response too short",
            ));
        }

        // Length is already consumed by the codec
        let id = bytes[0];
        let data = &bytes[1..];

        // Check for error response
        if data.len() >= 2 && data[0] == ERROR_MARKER {
            let code = ErrorCode::try_from(data[1]).map_err(|unknown| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Unknown error code: 0x{:02x}", unknown),
                )
            })?;

            let message = if code == ErrorCode::Custom && data.len() > 2 {
                Some(String::from_utf8_lossy(&data[2..]).to_string())
            } else {
                None
            };

            Ok(Response {
                id,
                data: vec![],
                error: Some(ResponseError { code, message }),
            })
        } else {
            Ok(Response {
                id,
                data: data.to_vec(),
                error: None,
            })
        }
    }

    /// Check if this is an error response
    pub fn is_error(&self) -> bool {
        self.error.is_some()
    }

    /// Get error details if this is an error response
    pub fn error(&self) -> Option<&ResponseError> {
        self.error.as_ref()
    }
}

/// Tokio codec for the control protocol
pub struct ControlCodec {
    /// Maximum packet size to prevent memory allocation issues
    max_length: usize,
}

impl Default for ControlCodec {
    fn default() -> Self {
        Self {
            max_length: 4096, // Reasonable max for control packets
        }
    }
}

impl Decoder for ControlCodec {
    type Item = Response;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.len() < 2 {
            // Not enough data for length field
            return Ok(None);
        }

        // Peek at length without consuming
        let length_field = u16::from_le_bytes([src[0], src[1]]) as usize;

        // In bitaxe-raw, the length field contains the length of the response data only,
        // NOT including the 2-byte length field itself or the 1-byte ID.
        // Total packet size = 2 (length) + 1 (ID) + length_field
        let total_packet_size = 2 + 1 + length_field;

        if total_packet_size > self.max_length {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Packet too large: {} bytes", total_packet_size),
            ));
        }

        if src.len() < total_packet_size {
            // Not enough data for complete packet
            return Ok(None);
        }

        // Consume the complete packet
        let packet_data = src.split_to(total_packet_size);

        // Skip the 2-byte length field
        let response_data = &packet_data[2..];

        let response = Response::parse(response_data)?;

        trace!(
            id = response.id,
            status = if response.error.is_some() { "ERR" } else { "OK" },
            data = %HexBytes(&response.data),
            frame = %HexBytes(&packet_data),
            "RX control"
        );

        Ok(Some(response))
    }
}

impl Encoder<Packet> for ControlCodec {
    type Error = io::Error;

    fn encode(&mut self, item: Packet, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let encoded = item.encode();
        if encoded.len() > self.max_length {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Packet too large: {} bytes", encoded.len()),
            ));
        }
        trace!(
            id = item.id,
            page = ?item.page,
            cmd = %format!("0x{:02x}", item.command),
            data = %HexBytes(&item.data),
            frame = %HexBytes(&encoded),
            "TX control"
        );
        dst.extend_from_slice(&encoded);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gpio_packet_encoding() {
        // Test GPIO write low
        let packet = Packet::gpio_write(0x42, 0, false);
        let encoded = packet.encode();

        assert_eq!(encoded[0], 0x07); // length low byte
        assert_eq!(encoded[1], 0x00); // length high byte
        assert_eq!(encoded[2], 0x42); // id
        assert_eq!(encoded[3], 0x00); // bus
        assert_eq!(encoded[4], 0x06); // GPIO page
        assert_eq!(encoded[5], 0x00); // command byte is pin 0
        assert_eq!(encoded[6], 0x00); // data: low

        // Test GPIO write high
        let packet = Packet::gpio_write(0x42, 0, true);
        let encoded = packet.encode();
        assert_eq!(encoded[6], 0x01); // data: high

        // Test GPIO pin 5
        let packet = Packet::gpio_write(0x42, 5, true);
        let encoded = packet.encode();
        assert_eq!(encoded[5], 0x05); // command byte is pin 5
    }

    #[test]
    fn test_response_parsing() {
        // Success response with data
        let response_bytes = vec![0x42, 0x01]; // id=0x42, data=[0x01]
        let response = Response::parse(&response_bytes).unwrap();
        assert_eq!(response.id, 0x42);
        assert_eq!(response.data, vec![0x01]);
        assert!(!response.is_error());

        // Error response
        let error_bytes = vec![0x42, 0xff, 0x11]; // id=0x42, error marker, invalid command
        let response = Response::parse(&error_bytes).unwrap();
        assert_eq!(response.id, 0x42);
        assert!(response.is_error());
        assert_eq!(response.error().unwrap().code, ErrorCode::InvalidCommand);
    }
}
