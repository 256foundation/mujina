//! 9-bit serial TX encoding for BZM2 ASIC communication.
//!
//! The BZM2 ASIC uses 9-bit serial (9N1), where the 9th bit marks the start
//! of a new command frame (address byte). When communicating through a USB-CDC
//! bridge (like bitaxe-raw firmware on RP2350), each outgoing 9-bit word is
//! encoded as a pair of bytes over USB:
//!
//! - First byte: lower 8 bits of the 9-bit word (data)
//! - Second byte: bit 8 (0x00 = data, 0x01 = address/frame start)
//!
//! The firmware strips the 9th bit on RX, so responses come back as plain
//! 8-bit bytes and no decoding is needed on the read path.

use bytes::{BufMut, BytesMut};

/// Encode a complete command frame into 9-bit serial format.
///
/// The first byte of the frame gets flag=0x01 (address byte, 9th bit set),
/// all subsequent bytes get flag=0x00 (data bytes). This matches the encoding
/// expected by the bitaxe-raw firmware's PIO UART bridge.
///
/// # Arguments
///
/// * `frame` - Raw protocol bytes for one complete command frame
///
/// # Returns
///
/// Encoded bytes with interleaved flag bytes (2x the input length).
pub fn nine_bit_encode_frame(frame: &[u8]) -> BytesMut {
    let mut encoded = BytesMut::with_capacity(frame.len() * 2);
    for (i, &byte) in frame.iter().enumerate() {
        encoded.put_u8(byte);
        encoded.put_u8(if i == 0 { 0x01 } else { 0x00 });
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_frame_single_byte() {
        let encoded = nine_bit_encode_frame(&[0xAA]);
        assert_eq!(encoded.as_ref(), &[0xAA, 0x01]);
    }

    #[test]
    fn test_encode_frame_multi_byte() {
        let encoded = nine_bit_encode_frame(&[0xFA, 0x0F, 0x42, 0x00]);
        assert_eq!(
            encoded.as_ref(),
            &[
                0xFA, 0x01, // first byte: flag=0x01 (address)
                0x0F, 0x00, // subsequent: flag=0x00 (data)
                0x42, 0x00, 0x00, 0x00,
            ]
        );
    }

    #[test]
    fn test_encode_frame_empty() {
        let encoded = nine_bit_encode_frame(&[]);
        assert!(encoded.is_empty());
    }

    #[test]
    fn test_encode_noop_command() {
        // BZM2 NOOP command (non-EHL): [length_lo, length_hi, header_hi, header_lo]
        // Example: asic_id=0xFA, opcode=NOOP(0xF)
        // header = (0xFA << 8) | (0xF << 4) = 0xFAF0
        // length = 4
        let frame = [0x04, 0x00, 0xFA, 0xF0];
        let encoded = nine_bit_encode_frame(&frame);
        assert_eq!(
            encoded.as_ref(),
            &[
                0x04, 0x01, // length LSB: address byte
                0x00, 0x00, // length MSB: data byte
                0xFA, 0x00, // header byte 1: data byte
                0xF0, 0x00, // header byte 2: data byte
            ]
        );
    }

    #[test]
    fn test_roundtrip() {
        // Encode a frame, then verify the raw pairs match expected format
        let original = vec![0x07, 0x00, 0xFA, 0x20, 0x00, 0x03, 0xFF];
        let encoded = nine_bit_encode_frame(&original);

        // Verify length doubled
        assert_eq!(encoded.len(), original.len() * 2);

        // Verify first pair has flag=0x01
        assert_eq!(encoded[0], original[0]);
        assert_eq!(encoded[1], 0x01);

        // Verify remaining pairs have flag=0x00
        for i in 1..original.len() {
            assert_eq!(encoded[i * 2], original[i]);
            assert_eq!(encoded[i * 2 + 1], 0x00);
        }
    }
}
