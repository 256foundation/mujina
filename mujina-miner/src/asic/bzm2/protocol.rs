use std::collections::HashSet;

pub const OPCODE_UART_READRESULT: u8 = 0x1;
pub const OPCODE_UART_WRITEREG: u8 = 0x2;
pub const OPCODE_UART_WRITEJOB: u8 = 0x0;

pub const BROADCAST_ASIC: u8 = 0xff;

pub const ENGINE_REG_TARGET: u8 = 0x44;
pub const ENGINE_REG_TIMESTAMP_COUNT: u8 = 0x48;
pub const ENGINE_REG_ZEROS_TO_FIND: u8 = 0x49;

pub const DEFAULT_TIMESTAMP_COUNT: u8 = 60;
pub const DEFAULT_NONCE_GAP: u32 = 0x28;
pub const LOGICAL_ENGINE_ROWS: u8 = 20;
pub const LOGICAL_ENGINE_COLS: u8 = 12;

#[derive(Debug, Clone, Copy)]
pub struct TdmResultFrame {
    pub asic: u8,
    pub engine_address: u16,
    pub status: u8,
    pub nonce: u32,
    pub sequence_id: u8,
    pub reported_time: u8,
}

impl TdmResultFrame {
    pub fn row(self) -> u8 {
        (self.engine_address & 0x3f) as u8
    }

    pub fn col(self) -> u8 {
        (self.engine_address >> 6) as u8
    }

    pub fn logical_engine_id(self) -> Option<u16> {
        logical_engine_id(self.row(), self.col())
    }

    pub fn nonce_valid(self) -> bool {
        (self.status & 0x8) != 0
    }
}

#[derive(Default)]
pub struct TdmResultParser {
    buffer: Vec<u8>,
}

impl TdmResultParser {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<TdmResultFrame> {
        self.buffer.extend_from_slice(bytes);

        let mut frames = Vec::new();
        let mut cursor = 0usize;

        while self.buffer.len().saturating_sub(cursor) >= 10 {
            let asic = self.buffer[cursor];
            let opcode = self.buffer[cursor + 1];

            if asic >= 100 || opcode != OPCODE_UART_READRESULT {
                cursor += 1;
                continue;
            }

            let payload = &self.buffer[cursor + 2..cursor + 10];
            let header = u16::from_be_bytes([payload[0], payload[1]]);
            let engine_address = header & 0x0fff;
            let status = (header >> 12) as u8;
            let nonce = u32::from_le_bytes(payload[2..6].try_into().unwrap());
            let sequence_id = payload[6];
            let reported_time = payload[7];

            frames.push(TdmResultFrame {
                asic,
                engine_address,
                status,
                nonce,
                sequence_id,
                reported_time,
            });
            cursor += 10;
        }

        if cursor > 0 {
            self.buffer.drain(..cursor);
        }

        frames
    }
}

pub fn encode_write_register(asic: u8, engine_address: u16, offset: u8, value: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(7 + value.len());
    let header = ((asic as u32) << 24)
        | ((OPCODE_UART_WRITEREG as u32) << 20)
        | ((engine_address as u32) << 8)
        | offset as u32;

    bytes.extend_from_slice(&((7 + value.len()) as u16).to_le_bytes());
    bytes.extend_from_slice(&header.to_be_bytes());
    bytes.push((value.len() as u8).saturating_sub(1));
    bytes.extend_from_slice(value);
    bytes
}

pub fn encode_write_job(
    asic: u8,
    engine_address: u16,
    midstate: &[u8; 32],
    merkle_root_residue: u32,
    ntime: u32,
    sequence_id: u8,
    job_control: u8,
) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(48);
    let header = ((asic as u32) << 24)
        | ((OPCODE_UART_WRITEJOB as u32) << 20)
        | ((engine_address as u32) << 8)
        | 41u32;

    bytes.extend_from_slice(&(48u16).to_le_bytes());
    bytes.extend_from_slice(&header.to_be_bytes());
    bytes.extend_from_slice(midstate);
    bytes.extend_from_slice(&merkle_root_residue.to_le_bytes());
    bytes.extend_from_slice(&ntime.to_le_bytes());
    bytes.push(sequence_id);
    bytes.push(job_control);
    bytes
}

pub fn logical_engine_address(row: u8, col: u8) -> u16 {
    ((col as u16) << 6) | row as u16
}

pub fn logical_engine_id(row: u8, col: u8) -> Option<u16> {
    if row >= LOGICAL_ENGINE_ROWS || col >= LOGICAL_ENGINE_COLS {
        return None;
    }
    if default_excluded_engines().contains(&(row, col)) {
        return None;
    }

    let excluded = default_excluded_engines();
    let mut id = 0u16;
    for c in 0..LOGICAL_ENGINE_COLS {
        for r in 0..LOGICAL_ENGINE_ROWS {
            if excluded.contains(&(r, c)) {
                continue;
            }
            if r == row && c == col {
                return Some(id);
            }
            id += 1;
        }
    }

    None
}

pub fn default_excluded_engines() -> HashSet<(u8, u8)> {
    HashSet::from([(0, 4), (0, 5), (19, 5), (19, 11)])
}

pub fn default_engine_coordinates() -> Vec<(u8, u8)> {
    let excluded = default_excluded_engines();
    let mut coords = Vec::new();
    for col in 0..LOGICAL_ENGINE_COLS {
        for row in 0..LOGICAL_ENGINE_ROWS {
            if excluded.contains(&(row, col)) {
                continue;
            }
            coords.push((row, col));
        }
    }
    coords
}

pub fn leading_zero_threshold(target: bitcoin::pow::Target) -> u8 {
    let bytes = target.to_be_bytes();
    let mut zeros = 0u8;

    'outer: for byte in bytes {
        if byte == 0 {
            zeros = zeros.saturating_add(8);
            continue;
        }

        for bit in (0..8).rev() {
            if (byte & (1 << bit)) == 0 {
                zeros = zeros.saturating_add(1);
            } else {
                break 'outer;
            }
        }
        break;
    }

    zeros.clamp(32, 64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_decodes_tdm_result() {
        let mut parser = TdmResultParser::default();
        let frame = [
            0x02,
            OPCODE_UART_READRESULT,
            0x41,
            0x23,
            0x78,
            0x56,
            0x34,
            0x12,
            0x05,
            0x09,
        ];

        let parsed = parser.push(&frame);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].asic, 0x02);
        assert_eq!(parsed[0].status, 0x4);
        assert_eq!(parsed[0].engine_address, 0x0123);
        assert_eq!(parsed[0].nonce, 0x1234_5678);
        assert_eq!(parsed[0].sequence_id, 0x05);
        assert_eq!(parsed[0].reported_time, 0x09);
    }
}
