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
//! Two response formats exist; see [`ResponseFormat`] for details.
//!
//! ## Request Packet Format
//!
//! ```text
//! [Length:2 LE] [ID:1] [Bus:1] [Page:1] [Command:1] [Data:N]
//! ```
//!
//! Length is the total packet size including all fields. ID is an
//! auto-assigned sequence number echoed in the response for
//! correlation. Bus is always `0x00` in current firmware. Page
//! selects the peripheral subsystem (see [`Page`]). Command and
//! Data are page-specific; see the submodules for details.
//!
//! See [`Packet`] for the programmatic representation.

pub mod channel;
pub mod gpio;
pub mod i2c;
pub mod led;
pub mod system;

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

/// Response frame format version.
///
/// The bitaxe-raw protocol has two response formats that differ in
/// framing and error signaling. The caller selects the variant based
/// on knowledge of the board, e.g., the firmware version reported
/// via USB `bcdDevice`.
///
/// ## V0 (original)
///
/// ```text
/// [Length:2 LE] [ID:1] [Data:N]
/// ```
///
/// Length = number of data bytes only (excludes length field and ID).
/// Total packet size = length + 3.
///
/// Protocol v0 intends to signal errors with a `0xFF` marker as
/// the first data byte, but that position is shared with normal
/// response payload. A command that legitimately returns `0xFF` as
/// its first byte is indistinguishable from an error, so error
/// detection is not reliable at the framing level.
///
/// ## V1
///
/// ```text
/// [Length:2 LE] [ID:1] [Status:1] [Data:N]
/// ```
///
/// Length = total packet size (same convention as requests). Status
/// is `0x00` for success, or an [`ErrorCode`] value for errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseFormat {
    V0,
    V1,
}

/// Success status code in v1 responses.
const STATUS_OK: u8 = 0x00;

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
    /// LED operations (SK6812 RGB)
    LED = 0x08,
    /// System operations (reboot)
    System = 0x09,
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

/// LED commands
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum LEDCommand {
    SetRGB = 0x10,
}

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
    ///
    /// The packet ID is left unset; [`ControlChannel::send_packet`]
    /// assigns it before transmission.
    pub fn new(page: Page, command: u8, data: Vec<u8>) -> Self {
        Self {
            id: 0,
            bus: 0x00,
            page,
            command,
            data,
        }
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
    /// Parse a v0 response from bytes (after the length field is
    /// stripped).
    ///
    /// All responses are treated as success. Protocol v0 intends to
    /// signal errors with a `0xFF` marker as the first data byte, but
    /// that position is also where normal payload data lives. A
    /// command that legitimately returns `0xFF` as its first byte
    /// (e.g., an LED command echoing R=255) is indistinguishable from
    /// an error response, so we cannot reliably detect errors here.
    fn parse_v0(bytes: &[u8]) -> Result<Self, io::Error> {
        if bytes.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Response too short",
            ));
        }

        let id = bytes[0];
        let data = bytes[1..].to_vec();

        Ok(Response {
            id,
            data,
            error: None,
        })
    }

    /// Parse a v1 response from bytes (after the length field is
    /// stripped).
    ///
    /// V1 has an explicit status byte: `0x00` for success, otherwise
    /// an [`ErrorCode`] value.
    fn parse_v1(bytes: &[u8]) -> Result<Self, io::Error> {
        if bytes.len() < 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "v1 response too short (need ID + status)",
            ));
        }

        let id = bytes[0];
        let status = bytes[1];
        let payload = &bytes[2..];

        if status == STATUS_OK {
            return Ok(Response {
                id,
                data: payload.to_vec(),
                error: None,
            });
        }

        let code = ErrorCode::try_from(status).map_err(|unknown| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Unknown error code: 0x{:02x}", unknown),
            )
        })?;

        let message = if code == ErrorCode::Custom && !payload.is_empty() {
            Some(String::from_utf8_lossy(payload).to_string())
        } else {
            None
        };

        Ok(Response {
            id,
            data: vec![],
            error: Some(ResponseError { code, message }),
        })
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
    format: ResponseFormat,
    max_length: usize,
}

impl ControlCodec {
    pub fn new(format: ResponseFormat) -> Self {
        Self {
            format,
            max_length: 4096,
        }
    }
}

impl Decoder for ControlCodec {
    type Item = Response;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.len() < 2 {
            return Ok(None);
        }

        let length_field = u16::from_le_bytes([src[0], src[1]]) as usize;

        // V0: length = data bytes only, total = length + 3
        // V1: length = total packet size
        let total_packet_size = match self.format {
            ResponseFormat::V0 => 2 + 1 + length_field,
            ResponseFormat::V1 => length_field,
        };

        if total_packet_size > self.max_length {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Packet too large: {} bytes", total_packet_size),
            ));
        }

        if src.len() < total_packet_size {
            return Ok(None);
        }

        let packet_data = src.split_to(total_packet_size);
        let response_data = &packet_data[2..];

        let response = match self.format {
            ResponseFormat::V0 => Response::parse_v0(response_data)?,
            ResponseFormat::V1 => Response::parse_v1(response_data)?,
        };

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
    fn v0_response_parsing() {
        let response = Response::parse_v0(&[0x42, 0x01]).unwrap();
        assert_eq!(response.id, 0x42);
        assert_eq!(response.data, vec![0x01]);
        assert!(!response.is_error());

        // Data starting with 0xFF is valid payload, not an error
        let response = Response::parse_v0(&[0x42, 0xff, 0x00, 0x00]).unwrap();
        assert_eq!(response.id, 0x42);
        assert_eq!(response.data, vec![0xff, 0x00, 0x00]);
        assert!(!response.is_error());
    }

    #[test]
    fn v1_response_parsing() {
        // Success: status 0x00
        let response = Response::parse_v1(&[0x42, 0x00, 0x01]).unwrap();
        assert_eq!(response.id, 0x42);
        assert_eq!(response.data, vec![0x01]);
        assert!(!response.is_error());

        // Success with data starting with 0xFF (valid payload, not an error)
        let response = Response::parse_v1(&[0x42, 0x00, 0xff, 0x00, 0x00]).unwrap();
        assert_eq!(response.data, vec![0xff, 0x00, 0x00]);
        assert!(!response.is_error());

        // Error: InvalidCommand
        let response = Response::parse_v1(&[0x42, 0x11]).unwrap();
        assert_eq!(response.id, 0x42);
        assert!(response.is_error());
        assert_eq!(response.error().unwrap().code, ErrorCode::InvalidCommand);

        // Error with message
        let response = Response::parse_v1(&[0x42, 0xff, b'b', b'a', b'd']).unwrap();
        assert!(response.is_error());
        assert_eq!(response.error().unwrap().code, ErrorCode::Custom);
        assert_eq!(response.error().unwrap().message.as_deref(), Some("bad"));
    }
}
