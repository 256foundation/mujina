//! CRC validation utilities for BM13xx protocol frames.

/// Calculates a 5-bit CRC using the USB polynomial over a slice of bytes.
///
/// This function implements the CRC-5 algorithm which uses polynomial 0x05,
/// an initial value of 0x1f, no output XOR. The algorithm does not use bit reflection.
///
/// Note that while CRCs are conceptually bit-oriented operations, this implementation
/// processes data in byte-sized chunks. The CRC is calculated over the entire sequence
/// of bits in the provided bytes.
pub fn crc5(data: &[u8]) -> u8 {
    let mut crc: u8 = 0x1f;
    for &byte in data {
        for i in (0..8).rev() {
            let bit = (byte >> i) & 1;
            let top = (crc >> 4) & 1;
            crc = ((crc << 1) & 0x1f) ^ if top ^ bit != 0 { 0x05 } else { 0 };
        }
    }
    crc
}

/// Validates data integrity using the CRC-5 algorithm.
///
/// This function checks whether the data passes CRC validation by
/// calculating the CRC-5 and verifying that the result is zero. Such a
/// check holds for CRC-5 when the CRC is appended as the final bits of
/// the data.
pub fn crc5_is_valid(data: &[u8]) -> bool {
    crc5(data) == 0
}

/// Calculates a 16-bit CRC using the CRC-16-FALSE algorithm over a slice of bytes.
///
/// This is used for mining job packets in BM13xx chips. The algorithm uses:
/// - Polynomial: 0x1021
/// - Initial value: 0xFFFF
/// - No output XOR
/// - No bit reflection
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xffff;
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            if crc & 0x8000 != 0 {
                crc = (crc << 1) ^ 0x1021;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    // Test that a computed CRC5 matches that of a few frames known to be good, taken from the
    // esp-miner source code. Skip the first two bytes, which are a prefix, and the last byte,
    // which is the expected CRC.
    #[test_case(&[0x55, 0xaa, 0x52, 0x05, 0x00, 0x00, 0x0a]; "read_register_0")]
    #[test_case(&[0x55, 0xaa, 0x51, 0x09, 0x00, 0x28, 0x11, 0x30, 0x02, 0x00, 0x03]; "set_baud")]
    #[test_case(&[0x55, 0xaa, 0x40, 0x05, 0x00, 0x00, 0x1c]; "set_chip_address_00")]
    #[test_case(&[0x55, 0xaa, 0x40, 0x05, 0x02, 0x00, 0x01]; "set_chip_address_02")]
    #[test_case(&[0x55, 0xaa, 0x40, 0x05, 0x04, 0x00, 0x03]; "set_chip_address_04")]
    #[test_case(&[0x55, 0xaa, 0x40, 0x05, 0x06, 0x00, 0x1e]; "set_chip_address_06")]
    #[test_case(&[0x55, 0xaa, 0x40, 0x05, 0x08, 0x00, 0x07]; "set_chip_address_08")]
    #[test_case(&[0x55, 0xaa, 0x53, 0x05, 0x00, 0x00, 0x03]; "chain_inactive")]
    #[test_case(&[0x55, 0xaa, 0x51, 0x09, 0x00, 0xa4, 0x90, 0x00, 0xff, 0xff, 0x1c]; "write_version_mask")]
    fn crc5_matches_esp_miner_frames(frame: &[u8]) {
        let crc = super::crc5(&frame[2..frame.len() - 1]);
        let expect = frame[frame.len() - 1];
        assert_eq!(crc, expect);
    }

    // Vectors computed with the crate crc_all 0.2.2.
    #[test_case(b"123456789", 0x0f; "digits_one_to_nine")]
    #[test_case(&[0x00, 0x00, 0x00, 0x00], 0x1b; "four_zero_bytes")]
    #[test_case(&[0xff, 0xff, 0xff, 0xff], 0x1e; "four_ff_bytes")]
    #[test_case(&[0xa5], 0x01; "single_byte")]
    #[test_case(&[0xde, 0xad, 0xbe, 0xef], 0x18; "deadbeef")]
    fn crc5_matches_reference_crate(data: &[u8], expect: u8) {
        assert_eq!(super::crc5(data), expect);
    }

    #[test]
    fn crc5_validates_response_frame() {
        // Skip the two-byte preamble; validation covers the rest.
        let frame = [0xaa, 0x55, 0x13, 0x70, 0x00, 0x00, 0x00, 0x00, 0x06];
        assert!(super::crc5_is_valid(&frame[2..]));
    }

    #[test]
    fn crc5_rejects_corrupt_response_frame() {
        // A frame with one payload byte changed.
        let frame = [0xaa, 0x55, 0x13, 0x70, 0x00, 0x00, 0x00, 0x01, 0x06];
        assert!(!super::crc5_is_valid(&frame[2..]));
    }

    // The "123456789" vector is the check value for CRC-16/IBM-3740
    // (alias CRC-16/CCITT-FALSE) in Greg Cook's catalogue; the
    // remaining vectors were computed with crc_all 0.2.2.
    #[test_case(b"123456789", 0x29b1; "digits_one_to_nine")]
    #[test_case(&[0x00, 0x00, 0x00, 0x00], 0x84c0; "four_zero_bytes")]
    #[test_case(&[0xff, 0xff, 0xff, 0xff], 0x1d0f; "four_ff_bytes")]
    #[test_case(&[0xa5], 0x04bf; "single_byte")]
    #[test_case(&[0xde, 0xad, 0xbe, 0xef], 0x4097; "deadbeef")]
    fn crc16_matches_reference(data: &[u8], expect: u16) {
        assert_eq!(super::crc16(data), expect);
    }

    #[test]
    fn crc16_matches_esp_miner_job() {
        // JobFull frame from an esp-miner capture. Checks the
        // algorithm and the CRC's big-endian wire order against the
        // reference implementation.
        let frame: Vec<u8> = vec![
            0x55, 0xaa, 0x21, 0x56, 0x18, 0x01, 0x00, 0x00, 0x00, 0x00, 0x38, 0xfa, 0x01, 0x17,
            0xdc, 0x17, 0xd6, 0x68, 0x15, 0x16, 0xab, 0x3d, 0x16, 0x42, 0xbb, 0x1f, 0xe2, 0xe2,
            0x37, 0x7f, 0x8a, 0xc5, 0x83, 0xe5, 0xda, 0x99, 0x6c, 0x6b, 0xc7, 0x05, 0x3e, 0xae,
            0x56, 0x4b, 0x02, 0x03, 0xcc, 0x4e, 0xd2, 0x37, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0xa2, 0x5c, 0x00, 0x00, 0xa1, 0xe7, 0xab, 0x5e, 0x5f, 0x24, 0x46, 0xa3,
            0x5f, 0x9c, 0xbb, 0xea, 0x3f, 0x53, 0x16, 0xe5, 0x4e, 0x39, 0x93, 0xde, 0x00, 0x00,
            0x00, 0x20, 0x6b, 0x18,
        ];

        // CRC is calculated over payload (bytes 2..86), excluding preamble and CRC itself
        let payload = &frame[2..86];
        let calculated_crc = super::crc16(payload);

        // Extract CRC from frame (wire format is big-endian: high byte first)
        let wire_crc = u16::from_be_bytes([frame[86], frame[87]]);

        assert_eq!(calculated_crc, wire_crc);
    }
}
