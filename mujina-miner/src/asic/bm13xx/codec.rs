//! Wire framing for BM13xx serial communication.
//!
//! [`FrameCodec`] converts between typed commands or responses for
//! BM13xx chips and the bytes on the serial bus, handling the
//! preamble and CRC. It implements [`tokio_util::codec`] traits so a
//! single framed serial port handles both directions.

use bytes::{Buf, BufMut, BytesMut};
use std::{fmt, io};
use tokio_util::codec::{Decoder, Encoder};

use super::command::{JobCommand, RegisterCommand};
use super::register::ChipModel;
use super::response::Response;
use crate::asic::bm13xx::crc::{crc5, crc5_is_valid, crc16};
use crate::tracing::prelude::*;

pub struct FrameCodec {
    model: ChipModel,
}

impl FrameCodec {
    pub fn new(model: ChipModel) -> Self {
        Self { model }
    }
}

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

        match Response::decode(&mut decode_buf, self.model) {
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
