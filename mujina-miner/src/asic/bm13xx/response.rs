//! Frames BM13xx chips send back to the host.
//!
//! BM13xx chips send back two kinds of frames: replies to register
//! reads, and nonce reports when a chip finds passing work.

use bitvec::prelude::*;
use bytes::{Buf, BytesMut};
use strum::FromRepr;

use super::register::{Register, RegisterAddress};
use crate::asic::bm13xx::error::ProtocolError;
use crate::job_source::GeneralPurposeBits;

#[derive(Debug)]
#[cfg_attr(not(test), allow(dead_code))]
pub enum Response {
    ReadRegister {
        chip_address: u8,
        register: Register,
    },
    Nonce {
        nonce: u32,
        job_id: u8,
        midstate_num: u8,
        version: GeneralPurposeBits,
        subcore_id: u8,
    },
}

impl Response {
    pub(super) fn decode(bytes: &mut BytesMut) -> Result<Response, ProtocolError> {
        let type_and_crc = bytes[bytes.len() - 1].view_bits::<Lsb0>();
        let type_repr = type_and_crc[5..].load::<u8>();

        match ResponseType::from_repr(type_repr) {
            Some(ResponseType::ReadRegister) => {
                let value_bytes = bytes.split_to(4);
                let value: [u8; 4] =
                    value_bytes[..]
                        .try_into()
                        .map_err(|_| ProtocolError::BufferTooSmall {
                            need: 4,
                            have: value_bytes.len(),
                        })?;
                let chip_address = bytes.get_u8();
                let register_address_repr = bytes.get_u8();

                if let Some(register_address) = RegisterAddress::from_repr(register_address_repr) {
                    let register = Register::decode(register_address, value)?;
                    Ok(Response::ReadRegister {
                        chip_address,
                        register,
                    })
                } else {
                    Err(ProtocolError::InvalidRegisterAddress(register_address_repr))
                }
            }
            Some(ResponseType::Nonce) => {
                // BM1370 nonce response format (11 bytes total, including preamble):
                // Already consumed: preamble (2 bytes)
                // Remaining: nonce(4) + midstate_num(1) + result_header(1) + version(2) + crc(1)
                let nonce = bytes.get_u32_le();
                let midstate_num = bytes.get_u8();
                let result_header = bytes.get_u8();

                // Version rolling field: 2 bytes, big-endian
                // Occupies bits 13-28 of block version when shifted left 13
                let version_bytes = [bytes.get_u8(), bytes.get_u8()];
                let version = GeneralPurposeBits::from(version_bytes);
                // CRC already consumed

                // Extract job_id and subcore_id from result_header
                // job_id is a 4-bit field (0-15) at bits 7-4 of result_header
                let job_id = (result_header >> 4) & 0x0f;
                let subcore_id = result_header & 0x0f;

                Ok(Response::Nonce {
                    nonce,
                    job_id,
                    midstate_num,
                    version,
                    subcore_id,
                })
            }
            None => Err(ProtocolError::InvalidResponseType(type_repr)),
        }
    }
}

#[derive(FromRepr)]
#[repr(u8)]
enum ResponseType {
    ReadRegister = 0,
    Nonce = 4,
}

#[cfg(test)]
mod tests {
    use bytes::{BufMut, BytesMut};
    use tokio_util::codec::Decoder;

    use super::super::codec::FrameCodec;
    use super::super::register::{ChipId, ChipModel, Register};
    use super::*;
    use crate::asic::bm13xx::crc::crc5_is_valid;

    #[test]
    fn verify_crc_calculation() {
        // Test that our known good frame has valid CRC
        let frame = &[0x13, 0x70, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10]; // without preamble
        assert!(
            crc5_is_valid(frame),
            "Known good frame should have valid CRC"
        );
    }

    #[test]
    fn decoder_with_exact_frame_size() {
        let mut codec = FrameCodec;

        // Exactly 11 bytes - a complete frame
        let mut buf = BytesMut::new();
        buf.put_slice(&[
            0xaa, 0x55, 0x13, 0x70, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10,
        ]);

        let result = codec.decode(&mut buf).unwrap();
        assert!(
            result.is_some(),
            "Should decode frame when buffer has exactly 11 bytes"
        );
    }

    #[test]
    fn read_register() {
        // 11-byte register read response from captures
        let wire = &[
            0xaa, 0x55, 0x13, 0x70, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10,
        ];
        let response = decode_frame(wire).expect("decode_frame should return Some for valid frame");

        let Response::ReadRegister {
            chip_address,
            register,
        } = response
        else {
            panic!("Expected ReadRegister response, got {:?}", response);
        };

        assert_eq!(chip_address, 0x00);

        let Register::ChipId(ChipId {
            model,
            core_count,
            address,
        }) = register
        else {
            panic!("Expected ChipId register, got {:?}", register);
        };

        assert_eq!(model, ChipModel::BM1370);
        assert_eq!(core_count, 0x00);
        assert_eq!(address, 0x00);
    }

    fn decode_frame(frame: &[u8]) -> Option<Response> {
        let mut buf = BytesMut::from(frame);
        let mut codec = FrameCodec;
        codec.decode(&mut buf).expect("Failed to decode frame")
    }

    #[test]
    fn reject_register_response_with_unknown_chip_id() {
        // A ChipId register read response whose id bytes match no
        // supported model; body only, preamble stripped as
        // Response::decode expects
        let mut buf = BytesMut::from(&[0x12, 0x34, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00][..]);

        let result = Response::decode(&mut buf);

        assert!(matches!(
            result,
            Err(ProtocolError::UnknownChipId([0x12, 0x34]))
        ));
    }

    #[test]
    fn decode_nonce_response_from_capture() {
        // From Bitaxe capture: RX: AA 55 18 00 A6 40 02 99 22 F9 91
        let wire = &[
            0xaa, 0x55, 0x18, 0x00, 0xa6, 0x40, 0x02, 0x99, 0x22, 0xf9, 0x91,
        ];
        let response = decode_frame(wire).expect("decode_frame should return Some for valid frame");

        let Response::Nonce {
            nonce,
            job_id,
            midstate_num,
            version,
            subcore_id,
        } = response
        else {
            panic!("Expected nonce response");
        };

        // From protocol doc: nonce 0x40A60018 -> Main core 32, nonce value 0x00A60018
        assert_eq!(nonce, 0x40a60018);
        assert_eq!(midstate_num, 0x02);

        // Result header: 0x99 -> bits[7:4]=9 (job_id), bits[3:0]=9 (subcore_id)
        assert_eq!(job_id, 9);
        assert_eq!(subcore_id, 9);

        // Version
        assert_eq!(version, GeneralPurposeBits::new([0x22, 0xF9]));

        // Verify main core extraction
        let main_core = (nonce >> 25) & 0x7f;
        assert_eq!(main_core, 32);
    }

    #[test]
    fn decode_multiple_nonce_responses() {
        // Additional nonce responses from S21 Pro capture
        let test_cases = vec![
            // RX: AA 55 07 35 CD CF 02 5E 00 2E 96
            // result_header=0x5e: bits[7:4]=5, bits[3:0]=14
            // version bytes [0x00, 0x2E] big-endian = 0x002E
            (
                &[
                    0xaa, 0x55, 0x07, 0x35, 0xcd, 0xcf, 0x02, 0x5e, 0x00, 0x2e, 0x96,
                ],
                0xcfcd3507,
                0x02,
                5,
                14,
                GeneralPurposeBits::new([0x00, 0x2E]),
            ),
            // RX: AA 55 46 03 32 E7 00 C3 2C 83 99
            // result_header=0xc3: bits[7:4]=12, bits[3:0]=3
            // version bytes [0x2C, 0x83] big-endian = 0x2C83
            (
                &[
                    0xaa, 0x55, 0x46, 0x03, 0x32, 0xe7, 0x00, 0xc3, 0x2c, 0x83, 0x99,
                ],
                0xe7320346,
                0x00,
                12,
                3,
                GeneralPurposeBits::new([0x2C, 0x83]),
            ),
        ];

        for (wire, exp_nonce, exp_midstate, exp_job_id, exp_subcore, exp_version) in test_cases {
            let response =
                decode_frame(wire).expect("decode_frame should return Some for valid frame");

            let Response::Nonce {
                nonce,
                job_id,
                midstate_num,
                version,
                subcore_id,
            } = response
            else {
                panic!("Expected nonce response");
            };

            assert_eq!(nonce, exp_nonce);
            assert_eq!(midstate_num, exp_midstate);
            assert_eq!(job_id, exp_job_id);
            assert_eq!(subcore_id, exp_subcore);
            assert_eq!(version, exp_version);
        }
    }

    #[test]
    fn decoder_handles_partial_frames() {
        let mut codec = FrameCodec;

        // Test with incomplete frame (less than 11 bytes)
        let mut buf = BytesMut::new();
        buf.put_slice(&[0xaa, 0x55, 0x13, 0x70, 0x00]); // Only 5 bytes

        let result = codec.decode(&mut buf).unwrap();
        assert!(result.is_none(), "Should return None for incomplete frame");
        assert_eq!(buf.len(), 5, "Buffer should not be consumed");

        // Add more bytes to complete the frame
        buf.put_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x10]); // Complete to 11 bytes

        let result = codec.decode(&mut buf).unwrap();
        assert!(result.is_some(), "Should decode complete frame");
        assert_eq!(buf.len(), 0, "Buffer should be fully consumed");
    }

    #[test]
    fn decoder_handles_corrupted_crc() {
        let mut codec = FrameCodec;

        // Valid frame with corrupted CRC (last byte)
        let mut buf = BytesMut::new();
        buf.put_slice(&[
            0xaa, 0x55, 0x13, 0x70, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF,
        ]); // Bad CRC

        let result = codec.decode(&mut buf).unwrap();
        assert!(result.is_none(), "Should reject frame with bad CRC");
        assert_eq!(
            buf.len(),
            10,
            "Should consume 1 byte when searching for valid frame"
        );
    }

    #[test]
    fn decoder_finds_frame_after_garbage() {
        let mut codec = FrameCodec;

        // Garbage bytes followed by valid frame
        let mut buf = BytesMut::new();
        buf.put_slice(&[0xFF, 0xEE, 0xDD]); // Garbage
        buf.put_slice(&[
            0xaa, 0x55, 0x13, 0x70, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10,
        ]); // Valid frame

        // First calls should skip garbage
        assert!(codec.decode(&mut buf).unwrap().is_none());
        assert!(codec.decode(&mut buf).unwrap().is_none());
        assert!(codec.decode(&mut buf).unwrap().is_none());

        // Should find valid frame
        let result = codec.decode(&mut buf).unwrap();
        assert!(result.is_some(), "Should find valid frame after garbage");
        assert_eq!(buf.len(), 0, "All data should be consumed");
    }

    #[test]
    fn decoder_handles_false_start() {
        let mut codec = FrameCodec;

        // Frame that starts with 0xAA but not followed by 0x55
        let mut buf = BytesMut::new();
        buf.put_slice(&[0xaa, 0x00]); // False start
        buf.put_slice(&[
            0xaa, 0x55, 0x13, 0x70, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10,
        ]); // Valid frame

        // Total buffer: [AA, 00, AA, 55, 13, 70, 00, 00, 00, 00, 00, 00, 10] = 13 bytes
        assert_eq!(buf.len(), 13, "Initial buffer should have 13 bytes");

        // First decode: sees AA at pos 0, but 00 at pos 1, so should skip 1 byte
        let first = codec.decode(&mut buf).unwrap();
        assert!(first.is_none(), "First decode should return None");
        assert_eq!(buf.len(), 12, "Should have consumed 1 byte");

        // Buffer now: [00, AA, 55, 13, 70, 00, 00, 00, 00, 00, 00, 10] = 12 bytes
        // Second decode: sees 00 at pos 0, should skip 1 byte
        let second = codec.decode(&mut buf).unwrap();
        assert!(second.is_none(), "Second decode should return None");
        assert_eq!(buf.len(), 11, "Should have consumed another byte");

        // Buffer now: [AA, 55, 13, 70, 00, 00, 00, 00, 00, 00, 10] = 11 bytes = valid frame
        // Third decode should succeed
        let result = codec.decode(&mut buf);
        match result {
            Ok(Some(Response::ReadRegister { .. })) => {} // Success
            Ok(Some(other)) => panic!("Expected ReadRegister, got {:?}", other),
            Ok(None) => panic!(
                "Expected Some, got None. Buffer len: {}, contents: {:02x?}",
                buf.len(),
                &buf[..]
            ),
            Err(e) => panic!("Decode error: {}", e),
        }
    }

    #[test]
    fn decoder_handles_back_to_back_frames() {
        let mut codec = FrameCodec;

        // Two valid frames back-to-back
        let mut buf = BytesMut::new();
        // First frame: register read
        buf.put_slice(&[
            0xaa, 0x55, 0x13, 0x70, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10,
        ]);
        // Second frame: nonce response
        buf.put_slice(&[
            0xaa, 0x55, 0x18, 0x00, 0xa6, 0x40, 0x02, 0x99, 0x22, 0xf9, 0x91,
        ]);

        // Decode first frame
        let result1 = codec.decode(&mut buf).unwrap();
        assert!(matches!(result1, Some(Response::ReadRegister { .. })));
        assert_eq!(buf.len(), 11, "Should have second frame remaining");

        // Decode second frame
        let result2 = codec.decode(&mut buf).unwrap();
        assert!(matches!(result2, Some(Response::Nonce { .. })));
        assert_eq!(buf.len(), 0, "Buffer should be empty");
    }

    #[test]
    fn decoder_handles_real_s21_pro_frames() {
        let mut codec = FrameCodec;

        // Real frames from S21 Pro capture
        let frames = vec![
            [
                0xaa, 0x55, 0x07, 0x35, 0xcd, 0xcf, 0x02, 0x5e, 0x00, 0x2e, 0x96,
            ],
            [
                0xaa, 0x55, 0x7b, 0x8d, 0x81, 0x60, 0x02, 0x55, 0x00, 0x85, 0x81,
            ],
            [
                0xaa, 0x55, 0x32, 0x2a, 0x84, 0x5a, 0x02, 0x52, 0x01, 0xb2, 0x8c,
            ],
        ];

        for frame in frames {
            let mut buf = BytesMut::new();
            buf.put_slice(&frame);

            let result = codec.decode(&mut buf).unwrap();
            assert!(result.is_some(), "Should decode real S21 Pro frame");
            assert!(
                matches!(result, Some(Response::Nonce { .. })),
                "Should be nonce response"
            );
        }
    }

    #[test]
    fn decoder_handles_stream_with_lost_bytes() {
        let mut codec = FrameCodec;

        // Simulate a stream where some bytes in the middle are lost
        let mut buf = BytesMut::new();
        // Start of first frame
        buf.put_slice(&[0xaa, 0x55, 0x13, 0x70, 0x00]); // 5 bytes
        // Lost bytes... skip to middle of nowhere
        buf.put_slice(&[0x99, 0x22, 0xf9]); // Random bytes
        // Valid complete frame
        buf.put_slice(&[
            0xaa, 0x55, 0x18, 0x00, 0xa6, 0x40, 0x02, 0x99, 0x22, 0xf9, 0x91,
        ]);

        // Decoder should skip the incomplete/corrupted data and find the valid frame
        let mut found_valid = false;
        for _ in 0..20 {
            // Try up to 20 times
            if let Some(response) = codec.decode(&mut buf).unwrap() {
                assert!(matches!(response, Response::Nonce { .. }));
                found_valid = true;
                break;
            }
        }
        assert!(found_valid, "Should eventually find the valid frame");
    }

    #[test]
    fn decoder_handles_mid_frame_start() {
        let mut codec = FrameCodec;

        // Start reading in the middle of a frame
        let mut buf = BytesMut::new();
        // Last 5 bytes of some frame
        buf.put_slice(&[0x02, 0x99, 0x22, 0xf9, 0x91]);
        // Valid complete frame
        buf.put_slice(&[
            0xaa, 0x55, 0x50, 0x03, 0x41, 0xd6, 0x00, 0x81, 0x18, 0x01, 0x9b,
        ]);

        // Total: 5 + 11 = 16 bytes
        // Should skip the partial frame bytes one by one until finding the valid frame
        for i in 0..5 {
            let result = codec.decode(&mut buf).unwrap();
            assert!(result.is_none(), "Decode {} should return None", i + 1);
            assert_eq!(
                buf.len(),
                16 - i - 1,
                "Should have consumed {} bytes",
                i + 1
            );
        }

        // Now we should have the valid frame
        let result = codec.decode(&mut buf).unwrap();
        assert!(
            result.is_some(),
            "Should find valid frame after partial data"
        );
        assert!(
            matches!(result, Some(Response::Nonce { .. })),
            "Should be nonce response"
        );
    }

    #[test]
    fn decoder_validates_real_register_responses() {
        // Test all register read responses are handled correctly
        let mut codec = FrameCodec;

        // Standard chip detection response
        let mut buf = BytesMut::new();
        buf.put_slice(&[
            0xaa, 0x55, 0x13, 0x70, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10,
        ]);

        let response = codec.decode(&mut buf).unwrap().unwrap();
        match response {
            Response::ReadRegister {
                chip_address,
                register,
            } => {
                assert_eq!(chip_address, 0x00);
                assert!(matches!(register, Register::ChipId { .. }));
            }
            _ => panic!("Expected ReadRegister response"),
        }
    }

    #[test]
    fn decode_nonce_response_from_esp_miner_capture() {
        use crate::asic::bm13xx::test_data::esp_miner_job;

        // Decode nonce response from hardware capture and verify against test data
        let response =
            decode_frame(&esp_miner_job::wire_rx::FRAME).expect("Should decode valid frame");

        let Response::Nonce {
            nonce,
            job_id,
            midstate_num,
            version,
            subcore_id,
        } = response
        else {
            panic!("Expected nonce response");
        };

        // Verify all fields match test data
        assert_eq!(nonce, *esp_miner_job::wire_rx::NONCE);
        assert_eq!(midstate_num, *esp_miner_job::wire_rx::MIDSTATE_NUM);
        assert_eq!(job_id, *esp_miner_job::wire_rx::JOB_ID);
        assert_eq!(subcore_id, *esp_miner_job::wire_rx::SUBCORE_ID);
        // VERSION_ROLLING_FIELD is u16, convert to big-endian bytes
        let expected_bytes = esp_miner_job::wire_rx::VERSION_ROLLING_FIELD.to_be_bytes();
        assert_eq!(version, GeneralPurposeBits::new(expected_bytes));

        // Verify version rolling field shifted left 13 matches submit VERSION
        let bits_as_u16 = u16::from_be_bytes(*version.as_bytes());
        let version_shifted = (bits_as_u16 as u32) << 13;
        assert_eq!(
            version_shifted,
            *esp_miner_job::submit::VERSION,
            "Version rolling field << 13 should match mining.submit version"
        );
    }

    #[test]
    fn test_full_mining_round_trip() {
        use bitcoin::block::Header as BlockHeader;
        use tokio_util::codec::Encoder;

        use super::super::command::{JobCommand, JobFullFormat};
        use crate::asic::bm13xx::test_data::esp_miner_job;
        use crate::types::Difficulty;

        // Build JobFullFormat, encode to wire, decode nonce response,
        // apply version rolling, compute hash, and verify difficulty.
        let job = JobFullFormat {
            job_id: *esp_miner_job::wire_tx::JOB_ID,
            num_midstates: esp_miner_job::wire_tx::NUM_MIDSTATES_BYTE[0],
            starting_nonce: u32::from_le_bytes(
                (*esp_miner_job::wire_tx::STARTING_NONCE_BYTES)
                    .try_into()
                    .unwrap(),
            ),
            nbits: *esp_miner_job::notify::NBITS,
            ntime: *esp_miner_job::notify::NTIME,
            merkle_root: *esp_miner_job::notify::MERKLE_ROOT,
            prev_block_hash: *esp_miner_job::notify::PREV_BLOCKHASH,
            version: *esp_miner_job::notify::VERSION,
        };

        let mut codec = FrameCodec;
        let mut tx_frame = BytesMut::new();
        codec
            .encode(JobCommand::JobFull(job.clone()), &mut tx_frame)
            .expect("Should encode JobFull command");

        // Our body bytes match esp-miner's wire capture. Byte 3 (length
        // byte) and bytes 86..88 (CRC16) intentionally differ; see the
        // length-byte comment in JobFullFormat::encode.
        assert_eq!(&tx_frame[4..86], &esp_miner_job::wire_tx::FRAME[4..86]);

        let rx_response =
            decode_frame(&esp_miner_job::wire_rx::FRAME).expect("Should decode RX frame");

        let Response::Nonce {
            nonce,
            job_id: rx_job_id,
            version: version_rolling,
            ..
        } = rx_response
        else {
            panic!("Expected Nonce response");
        };

        assert_eq!(rx_job_id, job.job_id, "Job ID should round-trip");

        let full_version = version_rolling.apply_to_version(job.version);
        let header = BlockHeader {
            version: full_version,
            prev_blockhash: job.prev_block_hash,
            merkle_root: job.merkle_root,
            time: job.ntime,
            bits: job.nbits,
            nonce,
        };

        let hash = header.block_hash();
        let difficulty = Difficulty::from_hash(&hash);

        // Allow +/-1 tolerance for integer division rounding
        let expected = Difficulty::from(esp_miner_job::EXPECTED_HASH_DIFFICULTY as u64);
        assert!(
            difficulty >= Difficulty::from(expected.as_u64() - 1)
                && difficulty <= Difficulty::from(expected.as_u64() + 1),
            "Hash difficulty should match esp-miner result"
        );
        assert!(
            difficulty >= Difficulty::from(esp_miner_job::POOL_SHARE_DIFFICULTY_INT),
            "Hash should meet pool difficulty"
        );
    }
}
