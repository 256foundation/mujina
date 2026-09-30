use std::collections::{HashMap, HashSet};

pub const OPCODE_UART_WRITEJOB: u8 = 0x0;
pub const OPCODE_UART_READRESULT: u8 = 0x1;
pub const OPCODE_UART_WRITEREG: u8 = 0x2;
pub const OPCODE_UART_READREG: u8 = 0x3;
pub const OPCODE_UART_MULTICAST_WRITE: u8 = 0x4;
/// Block header, as named by one analysis tool's enum. Deliberately NOT in
/// `is_known_opcode` -- see that function.
pub const OPCODE_UART_BLOCK_HDR: u8 = 0x5;
pub const OPCODE_UART_DTS_VS: u8 = 0x0d;
pub const OPCODE_UART_LOOPBACK: u8 = 0x0e;
pub const OPCODE_UART_NOOP: u8 = 0x0f;

/// Is this one of the opcodes the part actually emits?
///
/// Used to tell a correctly-sized frame from a mis-sized one: after a good
/// frame the next byte pair is a header, so an unrecognised opcode there means
/// we consumed the wrong number of bytes.
///
/// `OPCODE_UART_BLOCK_HDR` (0x5) is absent on purpose. It appears in one
/// analysis tool's enum, and in nothing else: four independent implementations
/// of this protocol list the same eight opcodes and none includes it, and
/// it occurs in none of our captures. I argued for keeping it on the grounds
/// that an unknown opcode gets treated as noise -- but that reasoning runs the
/// wrong way here. The two errors are not symmetric. Admitting an opcode the
/// part never sends lets the resync lock onto a false boundary and corrupt
/// every frame after it until the next resync; rejecting a real one costs a
/// single frame, because the next header resyncs us. Widening the accepted set
/// on the weakest evidence we have buys the worse failure.
fn is_known_opcode(op: u8) -> bool {
    matches!(
        op,
        OPCODE_UART_WRITEJOB
            | OPCODE_UART_READRESULT
            | OPCODE_UART_WRITEREG
            | OPCODE_UART_READREG
            | OPCODE_UART_MULTICAST_WRITE
            | OPCODE_UART_DTS_VS
            | OPCODE_UART_LOOPBACK
            | OPCODE_UART_NOOP
    )
}

pub const BROADCAST_ASIC: u8 = 0xff;
pub const TARGET_BYTE: u8 = 0x08;

pub const ENGINE_REG_TARGET: u8 = 0x44;
pub const ENGINE_REG_START_NONCE: u8 = 0x3c;
pub const ENGINE_REG_TIMESTAMP_COUNT: u8 = 0x48;
pub const ENGINE_REG_ZEROS_TO_FIND: u8 = 0x49;
pub const ENGINE_REG_END_NONCE: u8 = 0x40;
/// Engine STATUS. Read 4 bytes from here and byte 1 is CONFIG.
pub const ENGINE_REG_STATUS: u8 = 0x00;
/// Engine CONFIG: the four TCE clock gates and the performance-boost bit.
pub const ENGINE_REG_CONFIG: u8 = 0x01;
/// CONFIG with every TCE ungated and performance boost on: the mode
/// `DEFAULT_NONCE_GAP` is measured in.
pub const ENGINE_CONFIG_RUN_ALL_TCE: u8 = 0x04;
/// CONFIG's four TCE clock-gate bits (0, 1, 5, 6). A set bit gates that TCE:
/// it holds work and never hashes it.
///
/// MEASURED on hardware: after Mujina's engine
/// soft reset CONFIG reads 0x77, all four gates set, on ASIC 0 and ASIC 99;
/// written 0x04, it reads back 0x14. Dispatching for 125 s to
/// engines in that gated state and got no result at idle power.
pub const ENGINE_CONFIG_TCE_GATES: u8 = 0x63;

pub const DEFAULT_TIMESTAMP_COUNT: u8 = 60;
/// How far past the found nonce the engine reports it, in the all-TCE
/// enhanced mode that the engine Config value 0x04 selects. Other TCE
/// configurations report other gaps, so this constant is only right while the
/// engines are configured that way. It was 0x28, and no real result ever
/// decoded under it: a public captured hardware result, and 84,586 of 119,598
/// result frames in our capture of the vendor stack mining,
/// validate at 0x4c with the nonce byte-swapped.
pub const DEFAULT_NONCE_GAP: u32 = 0x4c;
pub const DEFAULT_BOARD_END_NONCE: u32 = 0xffff_ffff;
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
    /// DTS/VS frames dropped because what followed them was not a header.
    /// A stream that desyncs constantly is a misconfigured sensor generation,
    /// and this count is the evidence that says so.
    dts_vs_desyncs: u64,
    /// Bytes thrown away to regain frame alignment.
    ///
    /// The parser slides forward one byte at a time when it cannot recognise an
    /// opcode, which is what makes it survive a framing slip -- but sliding
    /// silently makes a degrading chain look identical to a healthy one.
    ///
    /// MEASURED, and it is why this counter exists: on hardware the vendor
    /// daemon met exactly this on board 0 entering calibration, logged four
    /// unknown messages with a real byte walking across the frame boundary,
    /// called the result an addressing failure and cut a 3 kW rail. Our own
    /// 1 Hz capture has the rail at 17.625 V and the outlet at 41 degC in the
    /// sample before it went dark, so nothing was wrong with the board itself.
    ///
    /// Recovering silently is better than dying. Recovering silently and saying
    /// nothing is how a chain degrades for a week unnoticed, so the bytes are
    /// counted and reported: a handful over a run is a line doing its job, a
    /// steadily climbing count is a chain asking for attention.
    resync_bytes: u64,
}

impl Default for TdmFrameParser {
    fn default() -> Self {
        Self::new(DtsVsGeneration::Gen2)
    }
}

impl TdmFrameParser {
    /// How many DTS/VS frames were dropped as mis-framed.
    pub fn dts_vs_desyncs(&self) -> u64 {
        self.dts_vs_desyncs
    }

    /// Bytes discarded to regain alignment. See [`Self::resync_bytes`].
    pub fn resync_bytes(&self) -> u64 {
        self.resync_bytes
    }

    pub fn new(dts_vs_generation: DtsVsGeneration) -> Self {
        Self {
            dts_vs_generation,
            buffer: Vec::new(),
            expected_read_lengths: HashMap::new(),
            dts_vs_desyncs: 0,
            resync_bytes: 0,
        }
    }

    /// Drop any partial frame held from before a break in the stream, and say
    /// how many bytes that was.
    ///
    /// For when something else has read the line: a diagnostic's own reads
    /// take bytes this parser never sees, so whatever it was holding is the
    /// front half of a frame whose back half is gone. Joined to what arrives
    /// next, it is a frame that never existed -- measured on hardware as a
    /// corroborated voltage fault from a part with implausible codes, which
    /// stopped a chain, measured on hardware.
    pub fn discard_partial(&mut self) -> usize {
        let n = self.buffer.len();
        self.buffer.clear();
        n
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

            // Resync heuristic: an id this large in the asic position is almost
            // always line noise in the supported small-id chains, so skip it as
            // a stray byte. DTS/VS frames are exempt: they carry the
            // thermal-trip / voltage-fault bits that drive the protective
            // shutdown and must never be silently dropped by an ad-hoc id bound
            // (an operator can also legitimately push ids >= 100 via
            // MUJINA_BZM2_ENUM_START_ID). A fault frame that reaches the handler
            // is logged loudly there before any shutdown action is taken.
            if asic >= 100 && opcode != OPCODE_UART_DTS_VS {
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
                        // No READREG was requested for this id, so these bytes are
                        // stray/noise (the long-lived streaming parser never sets
                        // an expected read length). Resync one byte forward like
                        // the unknown-opcode arm instead of breaking: a bare
                        // READREG-looking prefix sitting at cursor 0 would
                        // otherwise wedge framing forever and grow `self.buffer`
                        // without bound on every subsequent push.
                        cursor += 1;
                        continue;
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

                    // Look ahead one header before trusting this frame.
                    //
                    // A DTS/VS frame carries no length: the payload is four
                    // bytes in one sensor generation and eight in the other,
                    // and the parser takes which from CONFIGURATION. Configure
                    // the wrong one and it swallows the next frame's header,
                    // everything after shifts, and the bytes it then reads as
                    // fault and validity flags are unrelated data. Measured on
                    // hardware: a device outside the chain, a temperature
                    // below ambient, and a fault that never happened.
                    //
                    // After a correctly-sized frame the next byte pair is
                    // another header. If it is not a known opcode, the length
                    // was wrong or the stream is desynced -- and both want the
                    // same treatment. Emitting a frame we can already tell is
                    // mis-parsed is strictly worse than dropping it, because
                    // the frame carries fault bits.
                    let next = cursor + 2 + payload_len;
                    if self.buffer.len().saturating_sub(next) >= 2
                        && !is_known_opcode(self.buffer[next + 1] & 0x0f)
                    {
                        self.dts_vs_desyncs = self.dts_vs_desyncs.saturating_add(1);
                        cursor += 1;
                        continue;
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
                    cursor = next;
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
                    // One byte forward, and COUNTED. This is the whole of the
                    // resync, and the count is the only evidence that it
                    // happened -- the frames it recovers afterwards look
                    // exactly like frames that never needed recovering.
                    self.resync_bytes = self.resync_bytes.saturating_add(1);
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bzm2EngineLayout {
    active_coordinates: Vec<(u8, u8)>,
    logical_ids_by_address: HashMap<u16, u16>,
}

impl Bzm2EngineLayout {
    pub fn from_active_coordinates<I>(coords: I) -> Self
    where
        I: IntoIterator<Item = (u8, u8)>,
    {
        let mut active_coordinates = coords
            .into_iter()
            .filter(|(row, col)| *row < LOGICAL_ENGINE_ROWS && *col < LOGICAL_ENGINE_COLS)
            .collect::<Vec<_>>();
        active_coordinates.sort_by_key(|(row, col)| (*col, *row));
        active_coordinates.dedup();

        let logical_ids_by_address = active_coordinates
            .iter()
            .enumerate()
            .map(|(logical_id, (row, col))| (logical_engine_address(*row, *col), logical_id as u16))
            .collect();

        Self {
            active_coordinates,
            logical_ids_by_address,
        }
    }

    pub fn active_coordinates(&self) -> &[(u8, u8)] {
        &self.active_coordinates
    }

    pub fn active_engine_count(&self) -> usize {
        self.active_coordinates.len()
    }

    pub fn logical_engine_id(&self, row: u8, col: u8) -> Option<u16> {
        self.logical_engine_id_from_address(logical_engine_address(row, col))
    }

    pub fn logical_engine_id_from_address(&self, engine_address: u16) -> Option<u16> {
        self.logical_ids_by_address.get(&engine_address).copied()
    }
}

impl Default for Bzm2EngineLayout {
    fn default() -> Self {
        Self::from_active_coordinates(default_engine_coordinates())
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

pub fn physical_engine_coordinates() -> Vec<(u8, u8)> {
    let mut coords = Vec::new();
    for col in 0..LOGICAL_ENGINE_COLS {
        for row in 0..LOGICAL_ENGINE_ROWS {
            coords.push((row, col));
        }
    }
    coords
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
    // Index the payload in WIRE ORDER.
    //
    // This previously did `u64::from_be_bytes(..).to_le_bytes()`, which reverses
    // the eight payload bytes, and then indexed the reversed array with the byte
    // numbers of the ORIGINAL layout -- so every field landed seven bytes from
    // where it belongs. Verified against 421,643 captured frames from 100 ASICs
    // (measured on hardware):
    // thermal_validity was asserted on 0.00% of frames before and 100.00% after,
    // and the tune codes went from a single nonsense value to a tight
    // 2095..=2224 -- 29.3 C to 49.2 C on an idle observe chain, against the
    // 59-80 C the vendor stack printed on a hashing one.
    //
    // It survived review because `parse_dts_vs_gen1` does the same reversal but
    // has its indices written against the REVERSED order, so it lands correctly.
    // Two functions, opposite conventions, one shared-looking prelude.
    //
    // The damage was invisible because the plausibility gate was doing its job:
    // every mirrored reading fell outside PLAUSIBLE_DIE_MIN_C..=PLAUSIBLE_DIE_MAX_C
    // (the one frame that passed decoded to -175 C) and was dropped, so the
    // symptom was "no per-ASIC telemetry" rather than "wrong per-ASIC telemetry",
    // and we blamed the chain, in our own captures.
    let bytes: [u8; 8] = payload.try_into().unwrap();

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
    #[test]
    fn gen2_decodes_captured_frames_in_wire_order() {
        // Bytes lifted verbatim from a real capture, not synthesised:
        // captured on hardware
        // Expected values come from the capture and from what a die can
        // physically be, NOT from re-running this function's own arithmetic.
        let asic70 = [0xc8, 0x9b, 0x98, 0x98, 0x62, 0x50, 0xae, 0x82];
        let f = parse_dts_vs_gen2(70, &asic70);
        assert!(f.thermal_enabled, "captured frame has thermal enabled");
        assert!(
            f.thermal_validity,
            "captured frame has a valid thermal reading"
        );
        assert_eq!(f.thermal_tune_code, 2203);
        assert!(!f.thermal_trip_status);
        assert!(!f.thermal_fault);

        // Its neighbour on the same chain, captured microseconds later. Adjacent
        // devices on one board cannot be far apart thermally; that agreement is
        // the check, and it cannot be satisfied by a mirrored decode.
        let asic71 = [0xc8, 0x93, 0x98, 0x73, 0x62, 0x49, 0xad, 0x82];
        let g = parse_dts_vs_gen2(71, &asic71);
        assert!(g.thermal_validity);
        let spread = (f.thermal_tune_code as i32 - g.thermal_tune_code as i32).abs();
        assert!(spread < 64, "neighbouring dies disagreed by {spread} codes");

        // A frame that genuinely carries neither flag must still be refused, so
        // the fix cannot be mistaken for weakening the gate.
        let quiet = [0x03, 0xf3, 0x02, 0x00, 0x00, 0x0e, 0x03, 0xf3];
        let q = parse_dts_vs_gen2(0, &quiet);
        assert!(!q.thermal_enabled);
        assert!(!q.thermal_validity);
    }

    #[test]
    fn a_mis_sized_dts_vs_frame_is_dropped_rather_than_emitted() {
        // The failure this guards: configured for the longer sensor
        // generation against a part speaking the shorter one. The parser
        // swallows the next frame's header, and the bytes it reads as fault
        // and validity flags are unrelated data.
        let mut dec = TdmFrameParser::new(DtsVsGeneration::Gen2);

        // A short (Gen1-shaped) DTS/VS frame, then bytes that do not form a
        // valid header where a Gen2-sized read would leave the cursor.
        let mut wire = vec![0x00, OPCODE_UART_DTS_VS, 0x11, 0x22, 0x33, 0x44];
        wire.extend_from_slice(&[0x55, 0x05, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB]);

        let frames = dec.push(&wire);
        assert!(
            !frames.iter().any(|f| matches!(f, TdmFrame::DtsVs(_))),
            "a frame the parser can already tell is mis-sized must not be emitted"
        );
        assert!(dec.dts_vs_desyncs() >= 1, "and the drop must be counted");
    }

    #[test]
    fn a_correctly_sized_dts_vs_frame_still_parses() {
        // The other half: the check must not eat good frames. A full Gen2
        // payload followed by a valid header parses normally.
        let mut dec = TdmFrameParser::new(DtsVsGeneration::Gen2);
        let mut wire = vec![0x00, OPCODE_UART_DTS_VS];
        wire.extend_from_slice(&[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]);
        wire.extend_from_slice(&[0x00, OPCODE_UART_NOOP, 0xAA, 0xBB, 0xCC]);

        let frames = dec.push(&wire);
        assert!(
            frames.iter().any(|f| matches!(f, TdmFrame::DtsVs(_))),
            "a well-formed frame must survive the lookahead"
        );
        assert_eq!(dec.dts_vs_desyncs(), 0);
    }

    use super::*;

    #[test]
    fn write_register_encoder_matches_legacy_wire_format() {
        let encoded = encode_write_register(0x12, 0x0345, 0x67, &[0x78, 0x56, 0x34, 0x12]);
        assert_eq!(
            encoded,
            vec![
                0x0b, 0x00, 0x12, 0x23, 0x45, 0x67, 0x03, 0x78, 0x56, 0x34, 0x12
            ]
        );
    }

    #[test]
    fn read_register_encoder_matches_legacy_wire_format() {
        let encoded = encode_read_register(0x12, 0x0345, 0x67, 4);
        assert_eq!(
            encoded,
            vec![0x08, 0x00, 0x12, 0x33, 0x45, 0x67, 0x03, TARGET_BYTE]
        );
    }

    #[test]
    fn noop_and_loopback_encoders_match_legacy_wire_format() {
        assert_eq!(encode_noop(0x12), vec![0x04, 0x00, 0x12, 0xf0]);
        assert_eq!(
            encode_loopback(0x12, &[0xaa, 0xbb, 0xcc]),
            vec![0x08, 0x00, 0x12, 0xe0, 0x02, 0xaa, 0xbb, 0xcc]
        );
    }

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
        // Payload in WIRE ORDER. This fixture previously ran back-to-front,
        // matching a decoder that reversed the bytes and then indexed them as
        // if it had not -- so it asserted the bug instead of the behaviour.
        let raw = [
            0x02,
            OPCODE_UART_DTS_VS,
            0xF7,
            0xA9,
            0x96,
            0x45,
            0x12,
            0x34,
            0xAB,
            0xD5,
        ];
        let parsed = parser.push(&raw);
        assert_eq!(parsed.len(), 1);
        match &parsed[0] {
            TdmFrame::DtsVs(TdmDtsVsFrame::Gen2(frame)) => {
                assert_eq!(frame.asic, 0x02);
                assert_eq!(frame.thermal_tune_code, 0x07A9);
                assert!(frame.thermal_trip_status);
                assert!(frame.thermal_fault);
                assert!(frame.thermal_validity);
                assert!(frame.thermal_enabled);
                assert_eq!(frame.ch0_voltage, 0x1645);
                assert!(!frame.voltage_shutdown_status);
                assert!(frame.voltage_enabled);
                assert_eq!(frame.ch1_voltage, 0x04B4);
                assert_eq!(frame.ch2_voltage, 0x16AC);
                assert!(frame.voltage_fault);
                assert!(!frame.dll0_lock);
                assert!(frame.dll1_lock);
                assert!(frame.pll_lock);
            }
            other => panic!("unexpected frame: {other:?}"),
        }
    }

    #[test]
    fn parser_decodes_gen1_dts_vs() {
        let mut parser = TdmFrameParser::new(DtsVsGeneration::Gen1);
        let raw = [0x02, OPCODE_UART_DTS_VS, 0x91, 0xab, 0xcd, 0x45];
        let parsed = parser.push(&raw);
        assert_eq!(parsed.len(), 1);
        match &parsed[0] {
            TdmFrame::DtsVs(TdmDtsVsFrame::Gen1(frame)) => {
                assert_eq!(frame.asic, 0x02);
                assert_eq!(frame.voltage, 0x545);
                assert!(frame.voltage_enabled);
                assert_eq!(frame.thermal_tune_code, 0xab);
                assert!(!frame.thermal_validity);
                assert!(frame.thermal_enabled);
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
        match &parsed[1] {
            TdmFrame::Noop(frame) => assert_eq!(frame.data, [0xaa, 0xbb, 0xcc]),
            other => panic!("unexpected frame: {other:?}"),
        }
    }

    #[test]
    fn parser_keeps_dts_vs_trip_frame_from_high_asic_id() {
        let mut parser = TdmFrameParser::new(DtsVsGeneration::Gen2);
        // A Gen2 DTS/VS frame from asic id 100 with the thermal-trip bit set.
        // The `asic >= 100` resync heuristic must NOT drop it: silently
        // discarding it would disable over-temp protection for that device.
        // Trip bit lives in payload byte 0, in wire order.
        let parsed = parser.push(&[100, OPCODE_UART_DTS_VS, 0x10, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(parsed.len(), 1);
        match &parsed[0] {
            TdmFrame::DtsVs(TdmDtsVsFrame::Gen2(frame)) => {
                assert_eq!(frame.asic, 100);
                assert!(frame.thermal_trip_status);
            }
            other => panic!("unexpected frame: {other:?}"),
        }
    }

    /// The byte pattern that killed a board under the vendor daemon.
    ///
    /// MEASURED on hardware, board 0 entering calibration.
    /// The daemon logged, in order:
    ///
    /// ```text
    /// uart 0 unknown message 0x0 0x0:   unknown opcode
    /// uart 0 unknown message 0x0 0x0:   unknown opcode
    /// uart 0 unknown message 0x0 0x0:   unknown opcode
    /// uart 0 unknown message 0x0 0x84:  unknown opcode
    /// uart 0 unknown message 0x84 0xcf: asic id check fail
    /// ```
    ///
    /// That is not a dead part. `0x84` walks from the second position to the
    /// first: the receiver has lost byte alignment and is reading the stream at
    /// the wrong offset. The daemon treated the shifted result as an addressing
    /// failure, cut the rail, and exited -- our own 1 Hz capture shows the rail
    /// at 17.625 V and the outlet at 41 degC in the sample before it went dark,
    /// so nothing was wrong with the board's power or temperature. A framing
    /// slip ended a 3 kW run.
    ///
    /// This parser slides forward a byte at a time instead, and the point of
    /// this test is that a real frame following the garbage is still recovered
    /// intact. Losing alignment must cost the frames that were mangled and
    /// nothing else.
    #[test]
    fn the_desync_that_killed_a_board_costs_us_only_the_mangled_bytes() {
        let mut parser = TdmFrameParser::new(DtsVsGeneration::Gen2);
        parser.expect_read_register_bytes(0x03, 4);

        // The garbage exactly as the daemon reported it, byte for byte.
        let garbage = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x84, 0x84, 0xcf];
        let none_yet = parser.push(&garbage);
        assert!(
            none_yet.is_empty(),
            "desynced bytes must not be decoded into frames: {none_yet:?}"
        );

        // A good register reply immediately after it.
        let recovered = parser.push(&[0x03, OPCODE_UART_READREG, 0x78, 0x56, 0x34, 0x12]);
        assert_eq!(
            recovered.len(),
            1,
            "the first valid frame after a desync must be recovered"
        );
        match &recovered[0] {
            TdmFrame::Register(frame) => {
                assert_eq!(frame.data, vec![0x78, 0x56, 0x34, 0x12]);
                assert_eq!(frame.asic, 0x03);
            }
            other => panic!("unexpected frame: {other:?}"),
        }

        // And a NOOP after that, to show the parser is genuinely back in step
        // rather than having matched once by luck.
        let after = parser.push(&[0x01, OPCODE_UART_NOOP, 0xaa, 0xbb, 0xcc]);
        assert_eq!(after.len(), 1, "the parser must stay in step");

        // AND IT MUST NOT BE SILENT. Recovering is better than dying; recovering
        // without saying so is how a chain degrades unnoticed for a week. The
        // frames that come back afterwards look exactly like frames that never
        // needed recovering, so this count is the only evidence it happened.
        // Seven, not ten: the parser discards the run of zeros and then stops
        // at 0x84, which looks enough like a header to be worth waiting on. It
        // slides past that on the next push, once what follows proves it was
        // not one. Waiting rather than discarding is the right behaviour at a
        // buffer boundary -- a real frame can straddle one.
        assert!(
            parser.resync_bytes() >= 7,
            "the discarded bytes must be counted, got {}",
            parser.resync_bytes()
        );
    }

    /// A clean stream must not report a resync it did not need.
    ///
    /// A counter that ticks on healthy traffic is worse than no counter: it
    /// trains a reader to ignore it, and then it cannot report the real thing.
    #[test]
    fn a_clean_stream_counts_no_resync() {
        let mut parser = TdmFrameParser::new(DtsVsGeneration::Gen2);
        let frames = parser.push(&[0x01, OPCODE_UART_NOOP, 0xaa, 0xbb, 0xcc]);
        assert_eq!(frames.len(), 1);
        assert_eq!(
            parser.resync_bytes(),
            0,
            "a frame that parsed first time discarded nothing"
        );
    }

    #[test]
    fn parser_resyncs_after_unknown_prefix_and_partial_frames() {
        let mut parser = TdmFrameParser::new(DtsVsGeneration::Gen2);
        parser.expect_read_register_bytes(0x03, 4);

        let first = parser.push(&[0xfe, 0xaa, 0x03, OPCODE_UART_READREG, 0x78, 0x56]);
        assert!(first.is_empty());

        let second = parser.push(&[0x34, 0x12, 0x01, OPCODE_UART_NOOP, 0xaa, 0xbb, 0xcc]);
        assert_eq!(second.len(), 2);
        match &second[0] {
            TdmFrame::Register(frame) => assert_eq!(frame.data, vec![0x78, 0x56, 0x34, 0x12]),
            other => panic!("unexpected frame: {other:?}"),
        }
        match &second[1] {
            TdmFrame::Noop(frame) => assert_eq!(frame.data, [0xaa, 0xbb, 0xcc]),
            other => panic!("unexpected frame: {other:?}"),
        }
    }

    #[test]
    fn parser_bounds_buffer_and_recovers_on_readreg_without_pending_count() {
        let mut parser = TdmFrameParser::new(DtsVsGeneration::Gen2);

        // A READREG-looking prefix that never has a pending read length is pure
        // line noise on the long-lived streaming parser. Before the fix this hit
        // a no-count `break` at cursor 0, wedging framing and growing the buffer
        // by two bytes on every push. Feed it many times and confirm the buffer
        // stays bounded rather than accumulating ~2000 bytes.
        for _ in 0..1000 {
            assert!(parser.push(&[0x02, OPCODE_UART_READREG]).is_empty());
        }
        assert!(
            parser.buffer.len() <= 4,
            "buffer grew unbounded on malformed READREG noise: {} bytes",
            parser.buffer.len()
        );

        // Framing is not wedged: a subsequent well-formed NOOP frame is still
        // decoded once the noise resyncs forward.
        let recovered = parser.push(&[0x05, OPCODE_UART_NOOP, 0xaa, 0xbb, 0xcc]);
        assert_eq!(recovered.len(), 1);
        match &recovered[0] {
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

    /// The accepted set is the resync's whole notion of "this is a frame
    /// boundary", so it is pinned rather than left to whatever the constants
    /// happen to say. Every entry here is attested by at least two independent
    /// implementations of this protocol.
    #[test]
    fn accepted_opcodes_are_exactly_the_attested_eight() {
        let accepted: Vec<u8> = (0u8..=0xff).filter(|op| is_known_opcode(*op)).collect();
        assert_eq!(accepted, vec![0x0, 0x1, 0x2, 0x3, 0x4, 0xd, 0xe, 0xf]);
    }

    /// 0x5 is named by one analysis tool and by nothing else, and appears in no
    /// capture we hold. Admitting it would let the resync lock onto a false
    /// boundary; rejecting a real opcode would cost one frame. Asymmetric, so
    /// it stays out until something the part actually sent says otherwise.
    #[test]
    fn block_hdr_opcode_is_not_accepted_as_a_frame_boundary() {
        assert!(!is_known_opcode(OPCODE_UART_BLOCK_HDR));
    }
}
