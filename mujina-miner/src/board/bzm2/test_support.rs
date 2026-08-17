//! Shared PTY-backed BZM2 chain emulator for board tests.

use std::fs;
use std::io::{Read, Write};

use crate::asic::bzm2::protocol::{OPCODE_UART_NOOP, encode_noop, encode_write_register};

pub(super) fn spawn_chain_emulator(
    master: std::os::fd::OwnedFd,
    chain_len: u8,
    start_id: u8,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut file = fs::File::from(master);
        for offset in 0..chain_len {
            let mut noop_request = vec![0u8; encode_noop(crate::asic::bzm2::DEFAULT_ASIC_ID).len()];
            file.read_exact(&mut noop_request).unwrap();
            assert_eq!(
                noop_request,
                encode_noop(crate::asic::bzm2::DEFAULT_ASIC_ID)
            );
            file.write_all(&[
                crate::asic::bzm2::DEFAULT_ASIC_ID,
                OPCODE_UART_NOOP,
                b'B',
                b'Z',
                b'2',
            ])
            .unwrap();

            let assigned = start_id.saturating_add(offset);
            let expected_write = encode_write_register(
                crate::asic::bzm2::DEFAULT_ASIC_ID,
                crate::asic::bzm2::NOTCH_REG,
                0x0b,
                &(assigned as u32).to_le_bytes(),
            );
            let mut write_request = vec![0u8; expected_write.len()];
            file.read_exact(&mut write_request).unwrap();
            assert_eq!(write_request, expected_write);

            let mut assigned_noop = vec![0u8; encode_noop(assigned).len()];
            file.read_exact(&mut assigned_noop).unwrap();
            assert_eq!(assigned_noop, encode_noop(assigned));
            file.write_all(&[assigned, OPCODE_UART_NOOP, b'B', b'Z', b'2'])
                .unwrap();
        }

        let mut final_probe = vec![0u8; encode_noop(crate::asic::bzm2::DEFAULT_ASIC_ID).len()];
        file.read_exact(&mut final_probe).unwrap();
        assert_eq!(final_probe, encode_noop(crate::asic::bzm2::DEFAULT_ASIC_ID));
    })
}
