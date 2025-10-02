//! CRC validation utilities for BM13xx protocol frames.
//!
//! TODO: Remove redundancy between protocol decoder and dissector CRC usage
//! - Both protocol.rs and dissect.rs implement similar CRC validation logic
//! - Consider extracting frame validation helpers to reduce duplication
//! - Unify CRC16 big-endian handling across different code paths
//! - Add comprehensive unit tests using known good/bad frames from captures

use crc_all::CrcAlgo;

/// Calculates a 5-bit CRC using the USB polynomial over a slice of bytes.
///
/// This function implements the CRC-5-USB algorithm which uses polynomial 0x05,
/// an initial value of 0x1f, no output XOR. The algorithm does not use bit reflection.
///
/// Note that while CRCs are conceptually bit-oriented operations, this implementation
/// processes data in byte-sized chunks. The CRC is calculated over the entire sequence
/// of bits in the provided bytes.
pub fn crc5(data: &[u8]) -> u8 {
    let mut crc = CRC5_INIT;
    CRC5.update_crc(&mut crc, data);
    CRC5.finish_crc(&crc)
}

/// Validates data integrity using the CRC-5-USB algorithm.
///
/// This function checks if the data passes CRC validation by calculating the CRC-5
/// and verifying that the result is zero. When a CRC is appended to data, the CRC
/// calculation over the entire data (including the CRC) should yield zero if the data
/// is valid.
pub fn crc5_is_valid(data: &[u8]) -> bool {
    crc5(data) == 0
}

const CRC5_INIT: u8 = 0x1f;

const CRC5: CrcAlgo<u8> = CrcAlgo::<u8>::new(
    0x5,       // polynomial
    5,         // width
    CRC5_INIT, // init
    0,         // xorout
    false,     // reflect
);

/// Calculates a 16-bit CRC using the CRC-16-FALSE algorithm over a slice of bytes.
///
/// This is used for mining job packets in BM13xx chips. The algorithm uses:
/// - Polynomial: 0x1021
/// - Initial value: 0xFFFF
/// - No output XOR
/// - No bit reflection
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc = CRC16_INIT;
    CRC16.update_crc(&mut crc, data);
    CRC16.finish_crc(&crc)
}

/// Validates data integrity using the CRC-16-FALSE algorithm.
///
/// This function checks if the data matches the provided CRC-16 value.
/// Unlike CRC-5, CRC-16 is typically provided as a separate value rather
/// than being appended and validated to zero.
pub fn crc16_is_valid(data: &[u8], expected_crc: &[u8]) -> bool {
    if expected_crc.len() != 2 {
        return false;
    }
    let expected = u16::from_le_bytes([expected_crc[0], expected_crc[1]]);
    crc16(data) == expected
}

const CRC16_INIT: u16 = 0xFFFF;

const CRC16: CrcAlgo<u16> = CrcAlgo::<u16>::new(
    0x1021,     // polynomial (CRC-16-CCITT-FALSE)
    16,         // width
    CRC16_INIT, // init
    0,          // xorout
    false,      // reflect
);

#[cfg(test)]
mod tests {
    use test_case::test_case;

    // TODO: Add unit tests based on actual serial captures
    // - Import frames from ~/mujina/captures/bitaxe-gamma-logic/esp-miner-boot.csv
    // - Test CRC16 big-endian validation for work frames
    // - Test CRC5 validation for response frames
    // - Add test cases for edge cases discovered during capture analysis

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
    fn calculate(frame: &[u8]) {
        let crc = super::crc5(&frame[2..frame.len() - 1]);
        let expect = frame[frame.len() - 1];
        assert_eq!(crc, expect);
    }

    #[test_case(&[0xaa, 0x55, 0x13, 0x70, 0x00, 0x00, 0x00, 0x00, 0x06]; "read_response")]
    fn validate(frame: &[u8]) {
        assert!(super::crc5_is_valid(&frame[2..]));
    }
}
