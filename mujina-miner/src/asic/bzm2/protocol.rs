use std::collections::{HashMap, HashSet};

pub const OPCODE_UART_WRITEJOB: u8 = 0x0;
pub const OPCODE_UART_READRESULT: u8 = 0x1;
pub const OPCODE_UART_WRITEREG: u8 = 0x2;
pub const OPCODE_UART_READREG: u8 = 0x3;
pub const OPCODE_UART_MULTICAST_WRITE: u8 = 0x4;
pub const OPCODE_UART_DTS_VS: u8 = 0x0d;
pub const OPCODE_UART_LOOPBACK: u8 = 0x0e;
pub const OPCODE_UART_NOOP: u8 = 0x0f;

pub const BROADCAST_ASIC: u8 = 0xff;
pub const TARGET_BYTE: u8 = 0x08;

pub const ENGINE_REG_TARGET: u8 = 0x44;
pub const ENGINE_REG_TIMESTAMP_COUNT: u8 = 0x48;
pub const ENGINE_REG_ZEROS_TO_FIND: u8 = 0x49;

pub const DEFAULT_TIMESTAMP_COUNT: u8 = 60;
pub const DEFAULT_NONCE_GAP: u32 = 0x28;
pub const LOGICAL_ENGINE_ROWS: u8 = 20;
pub const LOGICAL_ENGINE_COLS: u8 = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DtsVsGeneration {
    Gen1,
    Gen2,
}

impl DtsVsGeneration {
    pub fn from_env_value(raw: &str) -> Option<Self> {
        match raw.trim() {
            "1" | "gen1" | "GEN1" => Some(Self::Gen1),
            "2" | "gen2" | "GEN2" => Some(Self::Gen2),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TdmRegisterFrame {
    pub asic: u8,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TdmNoopFrame {
    pub asic: u8,
    pub data: [u8; 3],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TdmDtsVsGen1Frame {
    pub asic: u8,
    pub voltage: u16,
    pub voltage_enabled: bool,
    pub thermal_tune_code: u8,
    pub thermal_validity: bool,
    pub thermal_enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TdmDtsVsGen2Frame {
    pub asic: u8,
    pub ch0_voltage: u16,
    pub ch1_voltage: u16,
    pub ch2_voltage: u16,
    pub voltage_shutdown_status: bool,
    pub voltage_enabled: bool,
    pub thermal_tune_code: u16,
    pub thermal_trip_status: bool,
    pub thermal_fault: bool,
    pub thermal_validity: bool,
    pub thermal_enabled: bool,
    pub voltage_fault: bool,
    pub dll0_lock: bool,
    pub dll1_lock: bool,
    pub pll_lock: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TdmDtsVsFrame {
    Gen1(TdmDtsVsGen1Frame),
    Gen2(TdmDtsVsGen2Frame),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TdmFrame {
    Result(TdmResultFrame),
    Register(TdmRegisterFrame),
    DtsVs(TdmDtsVsFrame),
    Noop(TdmNoopFrame),
}

pub struct TdmFrameParser {
    dts_vs_generation: DtsVsGeneration,
    buffer: Vec<u8>,
    expected_read_lengths: HashMap<u8, usize>,
}

impl Default for TdmFrameParser {
    fn default() -> Self {
        Self::new(DtsVsGeneration::Gen2)
    }
}

impl TdmFrameParser {
    pub fn new(dts_vs_generation: DtsVsGeneration) -> Self {
        Self {
            dts_vs_generation,
            buffer: Vec::new(),
            expected_read_lengths: HashMap::new(),
        }
    }

    pub fn expect_read_register_bytes(&mut self, asic: u8, count: usize) {
        self.expected_read_lengths.insert(asic, count);
    }

    pub fn push(&mut self, bytes: &[u8]) -> Vec<TdmFrame> {
        self.buffer.extend_from_slice(bytes);

        let mut frames = Vec::new();
        let mut cursor = 0usize;

        while self.buffer.len().saturating_sub(cursor) >= 2 {
            let asic = self.buffer[cursor];
            let opcode = self.buffer[cursor + 1];

            if asic >= 100 {
                cursor += 1;
                continue;
            }

            match opcode {
                OPCODE_UART_READRESULT => {
                    if self.buffer.len().saturating_sub(cursor) < 10 {
                        break;
                    }

                    let payload = &self.buffer[cursor + 2..cursor + 10];
                    let header = u16::from_be_bytes([payload[0], payload[1]]);
                    let engine_address = header & 0x0fff;
                    let status = (header >> 12) as u8;
                    let nonce = u32::from_le_bytes(payload[2..6].try_into().unwrap());
                    let sequence_id = payload[6];
                    let reported_time = payload[7];

                    frames.push(TdmFrame::Result(TdmResultFrame {
                        asic,
                        engine_address,
                        status,
                        nonce,
                        sequence_id,
                        reported_time,
                    }));
                    cursor += 10;
                }
                OPCODE_UART_READREG => {
                    let Some(&count) = self.expected_read_lengths.get(&asic) else {
                        break;
                    };
                    if self.buffer.len().saturating_sub(cursor) < 2 + count {
                        break;
                    }

                    frames.push(TdmFrame::Register(TdmRegisterFrame {
                        asic,
                        data: self.buffer[cursor + 2..cursor + 2 + count].to_vec(),
                    }));
                    self.expected_read_lengths.remove(&asic);
                    cursor += 2 + count;
                }
                OPCODE_UART_DTS_VS => {
                    let payload_len = match self.dts_vs_generation {
                        DtsVsGeneration::Gen1 => 4,
                        DtsVsGeneration::Gen2 => 8,
                    };
                    if self.buffer.len().saturating_sub(cursor) < 2 + payload_len {
                        break;
                    }

                    let payload = &self.buffer[cursor + 2..cursor + 2 + payload_len];
                    let frame = match self.dts_vs_generation {
                        DtsVsGeneration::Gen1 => {
                            TdmDtsVsFrame::Gen1(parse_dts_vs_gen1(asic, payload))
                        }
                        DtsVsGeneration::Gen2 => {
                            TdmDtsVsFrame::Gen2(parse_dts_vs_gen2(asic, payload))
                        }
                    };
                    frames.push(TdmFrame::DtsVs(frame));
                    cursor += 2 + payload_len;
                }
                OPCODE_UART_NOOP => {
                    if self.buffer.len().saturating_sub(cursor) < 5 {
                        break;
                    }
                    let data = self.buffer[cursor + 2..cursor + 5].try_into().unwrap();
                    frames.push(TdmFrame::Noop(TdmNoopFrame { asic, data }));
                    cursor += 5;
                }
                _ => {
                    cursor += 1;
                }
            }
        }

        if cursor > 0 {
            self.buffer.drain(..cursor);
        }

        frames
    }
}

#[derive(Default)]
pub struct TdmResultParser {
    inner: TdmFrameParser,
}

impl TdmResultParser {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<TdmResultFrame> {
        self.inner
            .push(bytes)
            .into_iter()
            .filter_map(|frame| match frame {
                TdmFrame::Result(result) => Some(result),
                _ => None,
            })
            .collect()
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

pub fn encode_multicast_write(asic: u8, group: u16, offset: u8, value: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(7 + value.len());
    let header = ((asic as u32) << 24)
        | ((OPCODE_UART_MULTICAST_WRITE as u32) << 20)
        | ((group as u32) << 8)
        | offset as u32;

    bytes.extend_from_slice(&((7 + value.len()) as u16).to_le_bytes());
    bytes.extend_from_slice(&header.to_be_bytes());
    bytes.push((value.len() as u8).saturating_sub(1));
    bytes.extend_from_slice(value);
    bytes
}

pub fn encode_read_register(asic: u8, engine_address: u16, offset: u8, count: u8) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(8);
    let header = ((asic as u32) << 24)
        | ((OPCODE_UART_READREG as u32) << 20)
        | ((engine_address as u32) << 8)
        | offset as u32;

    bytes.extend_from_slice(&8u16.to_le_bytes());
    bytes.extend_from_slice(&header.to_be_bytes());
    bytes.push(count.saturating_sub(1));
    bytes.push(TARGET_BYTE);
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

pub fn encode_read_result_command(asic: u8) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(4);
    let header = ((asic as u16) << 8) | ((OPCODE_UART_READRESULT as u16) << 4);
    bytes.extend_from_slice(&4u16.to_le_bytes());
    bytes.extend_from_slice(&header.to_be_bytes());
    bytes
}

pub fn encode_noop(asic: u8) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(4);
    let header = ((asic as u16) << 8) | ((OPCODE_UART_NOOP as u16) << 4);
    bytes.extend_from_slice(&4u16.to_le_bytes());
    bytes.extend_from_slice(&header.to_be_bytes());
    bytes
}

pub fn encode_loopback(asic: u8, data: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(5 + data.len());
    let header = ((asic as u16) << 8) | ((OPCODE_UART_LOOPBACK as u16) << 4);
    bytes.extend_from_slice(&((5 + data.len()) as u16).to_le_bytes());
    bytes.extend_from_slice(&header.to_be_bytes());
    bytes.push((data.len() as u8).saturating_sub(1));
    bytes.extend_from_slice(data);
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

fn parse_dts_vs_gen1(asic: u8, payload: &[u8]) -> TdmDtsVsGen1Frame {
    let raw = u32::from_be_bytes(payload.try_into().unwrap());
    let bytes = raw.to_le_bytes();
    let voltage = (((bytes[1] & 0x07) as u16) << 8) | bytes[0] as u16;

    TdmDtsVsGen1Frame {
        asic,
        voltage,
        voltage_enabled: (bytes[1] & 0x80) != 0,
        thermal_tune_code: bytes[2],
        thermal_validity: (bytes[3] & 0x40) != 0,
        thermal_enabled: (bytes[3] & 0x80) != 0,
    }
}

fn parse_dts_vs_gen2(asic: u8, payload: &[u8]) -> TdmDtsVsGen2Frame {
    let raw = u64::from_be_bytes(payload.try_into().unwrap());
    let bytes = raw.to_le_bytes();

    TdmDtsVsGen2Frame {
        asic,
        ch0_voltage: (((bytes[2] & 0x3f) as u16) << 8) | bytes[3] as u16,
        ch1_voltage: ((bytes[4] as u16) << 6) | ((bytes[5] & 0x3f) as u16),
        ch2_voltage: (((bytes[7] & 0x0f) as u16) << 10)
            | ((bytes[6] as u16) << 2)
            | (((bytes[5] >> 6) & 0x03) as u16),
        voltage_shutdown_status: (bytes[2] & 0x40) != 0,
        voltage_enabled: (bytes[2] & 0x80) != 0,
        thermal_tune_code: (((bytes[0] & 0x0f) as u16) << 8) | bytes[1] as u16,
        thermal_trip_status: (bytes[0] & 0x10) != 0,
        thermal_fault: (bytes[0] & 0x20) != 0,
        thermal_validity: (bytes[0] & 0x40) != 0,
        thermal_enabled: (bytes[0] & 0x80) != 0,
        voltage_fault: (bytes[7] & 0x10) != 0,
        dll0_lock: (bytes[7] & 0x20) != 0,
        dll1_lock: (bytes[7] & 0x40) != 0,
        pll_lock: (bytes[7] & 0x80) != 0,
    }
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

    #[test]
    fn parser_decodes_gen2_dts_vs() {
        let mut parser = TdmFrameParser::new(DtsVsGeneration::Gen2);
        let raw = [
            0x02,
            OPCODE_UART_DTS_VS,
            0xD5,
            0xAB,
            0x34,
            0x12,
            0x45,
            0x96,
            0xA9,
            0xF7,
        ];
        let parsed = parser.push(&raw);
        assert_eq!(parsed.len(), 1);
        match &parsed[0] {
            TdmFrame::DtsVs(TdmDtsVsFrame::Gen2(frame)) => {
                assert_eq!(frame.asic, 0x02);
                assert_eq!(frame.thermal_tune_code, 0x05AB);
                assert!(frame.thermal_trip_status);
                assert!(!frame.thermal_fault);
                assert!(frame.thermal_validity);
                assert!(frame.thermal_enabled);
                assert_eq!(frame.ch0_voltage, 0x1234);
                assert!(frame.voltage_shutdown_status);
                assert!(!frame.voltage_enabled);
                assert_eq!(frame.ch1_voltage, 0x1159);
                assert_eq!(frame.ch2_voltage, 0x1E56);
                assert!(frame.voltage_fault);
                assert!(frame.dll0_lock);
                assert!(frame.dll1_lock);
                assert!(frame.pll_lock);
            }
            other => panic!("unexpected frame: {other:?}"),
        }
    }

    #[test]
    fn parser_decodes_readreg_and_noop() {
        let mut parser = TdmFrameParser::new(DtsVsGeneration::Gen2);
        parser.expect_read_register_bytes(0x03, 4);
        let parsed = parser.push(&[
            0x03,
            OPCODE_UART_READREG,
            0x78,
            0x56,
            0x34,
            0x12,
            0x01,
            OPCODE_UART_NOOP,
            0xaa,
            0xbb,
            0xcc,
        ]);
        assert_eq!(parsed.len(), 2);
        match &parsed[0] {
            TdmFrame::Register(frame) => assert_eq!(frame.data, vec![0x78, 0x56, 0x34, 0x12]),
            other => panic!("unexpected frame: {other:?}"),
        }
        match parsed[1] {
            TdmFrame::Noop(frame) => assert_eq!(frame.data, [0xaa, 0xbb, 0xcc]),
            other => panic!("unexpected frame: {other:?}"),
        }
    }

    #[test]
    fn command_encoders_cover_all_uart_opcodes() {
        let writereg = encode_write_register(1, 2, 3, &[0x44]);
        let writejob = encode_write_job(1, 2, &[0u8; 32], 4, 5, 6, 7);
        let readreg = encode_read_register(1, 2, 3, 4);
        let multicast = encode_multicast_write(1, 2, 3, &[0x44]);
        let readresult = encode_read_result_command(1);
        let noop = encode_noop(1);
        let loopback = encode_loopback(1, &[0xaa, 0xbb]);

        assert_eq!(writereg[3] >> 4, OPCODE_UART_WRITEREG);
        assert_eq!(writejob[3] >> 4, OPCODE_UART_WRITEJOB);
        assert_eq!(readreg[3] >> 4, OPCODE_UART_READREG);
        assert_eq!(multicast[3] >> 4, OPCODE_UART_MULTICAST_WRITE);
        assert_eq!(readresult[3] >> 4, OPCODE_UART_READRESULT);
        assert_eq!(noop[3] >> 4, OPCODE_UART_NOOP);
        assert_eq!(loopback[3] >> 4, OPCODE_UART_LOOPBACK);
    }
}
