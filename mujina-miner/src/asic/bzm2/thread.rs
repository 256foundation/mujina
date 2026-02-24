//! BZM2 HashThread implementation.
//!
//! This module mirrors the BM13xx actor model and performs full BZM2 bring-up
//! before the first task is accepted.

use std::{
    collections::VecDeque,
    env, io,
    sync::{Arc, RwLock},
};

use async_trait::async_trait;
use bitcoin::{
    TxMerkleNode,
    block::{Header as BlockHeader, Version as BlockVersion},
    consensus,
    hashes::Hash as _,
};
use futures::{SinkExt, sink::Sink, stream::Stream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{self, Duration, Instant};
use tokio_stream::StreamExt;

use super::protocol;
use crate::{
    asic::hash_thread::{
        BoardPeripherals, HashTask, HashThread, HashThreadCapabilities, HashThreadError,
        HashThreadEvent, HashThreadStatus, Share, ThreadRemovalSignal,
    },
    job_source::{Extranonce2, MerkleRootKind},
    tracing::prelude::*,
    types::{Difficulty, HashRate},
};

const ENGINE_ROWS: u16 = 20;
const ENGINE_COLS: u16 = 12;
// Invalid or non-existent engine coordinates.
const INVALID_ENGINE_0_ROW: u16 = 0;
const INVALID_ENGINE_0_COL: u16 = 4;
const INVALID_ENGINE_1_ROW: u16 = 0;
const INVALID_ENGINE_1_COL: u16 = 5;
const INVALID_ENGINE_2_ROW: u16 = 19;
const INVALID_ENGINE_2_COL: u16 = 5;
const INVALID_ENGINE_3_ROW: u16 = 19;
const INVALID_ENGINE_3_COL: u16 = 11;
const INVALID_ENGINE_COUNT: usize = 4;
const WORK_ENGINE_COUNT: usize =
    (ENGINE_ROWS as usize * ENGINE_COLS as usize) - INVALID_ENGINE_COUNT;
const ENGINE_EN2_OFFSET_START: u64 = 1;

const SENSOR_REPORT_INTERVAL: u32 = 63;
const THERMAL_TRIP_C: f32 = 115.0;
const VOLTAGE_TRIP_MV: f32 = 500.0;

const PLL_LOCK_MASK: u32 = 0x4;
const REF_CLK_MHZ: f32 = 50.0;
const REF_DIVIDER: u32 = 2;
const POST2_DIVIDER: u32 = 1;
const POST1_DIVIDER: u8 = 1;
const TARGET_FREQ_MHZ: f32 = 800.0;
const DRIVE_STRENGTH_STRONG: u32 = 0x4446_4444;
const ENGINE_CONFIG_ENHANCED_MODE_BIT: u8 = 1 << 2;

const INIT_NOOP_TIMEOUT: Duration = Duration::from_millis(500);
const INIT_READREG_TIMEOUT: Duration = Duration::from_millis(500);
const PLL_LOCK_TIMEOUT: Duration = Duration::from_secs(3);
const PLL_POLL_DELAY: Duration = Duration::from_millis(100);
const SOFT_RESET_DELAY: Duration = Duration::from_millis(1);
const MIDSTATE_COUNT: usize = 4;
const WRITEJOB_CTL_REPLACE: u8 = 3;
const MIN_LEADING_ZEROS: u8 = 32;
const ENGINE_LEADING_ZEROS: u8 = 36;
const ENGINE_ZEROS_TO_FIND: u8 = ENGINE_LEADING_ZEROS - MIN_LEADING_ZEROS;
// Timestamp register uses bit7 for AUTO_CLOCK_UNGATE, so max counter value is 0x7f.
const ENGINE_TIMESTAMP_COUNT: u8 = 0x7f;
const AUTO_CLOCK_UNGATE: u8 = 1;
// Runtime nonce gap value.
const BZM2_NONCE_MINUS: u32 = 0x4c;
// Per-ASIC nonce assignment: each active ASIC searches the full
// nonce space (except 0xffff_ffff).
const BZM2_START_NONCE: u32 = 0x0000_0000;
const BZM2_END_NONCE: u32 = 0xffff_fffe;
const READRESULT_SEQUENCE_SPACE: usize = 64; // sequence byte carries 4 micro-jobs => 6 visible sequence bits
const READRESULT_SLOT_HISTORY: usize = 16;
const READRESULT_ASSIGNMENT_HISTORY_LIMIT: usize =
    READRESULT_SEQUENCE_SPACE * READRESULT_SLOT_HISTORY;
const SANITY_DIAGNOSTIC_LIMIT: u64 = 24;
const SEQUENCE_LOOKUP_DIAGNOSTIC_LIMIT: u64 = 24;
const ZERO_LZ_DIAGNOSTIC_LIMIT: u64 = 24;
const SHA256_IV: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];
const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

#[derive(Debug)]
enum ThreadCommand {
    UpdateTask {
        new_task: HashTask,
        response_tx: oneshot::Sender<std::result::Result<Option<HashTask>, HashThreadError>>,
    },
    ReplaceTask {
        new_task: HashTask,
        response_tx: oneshot::Sender<std::result::Result<Option<HashTask>, HashThreadError>>,
    },
    GoIdle {
        response_tx: oneshot::Sender<std::result::Result<Option<HashTask>, HashThreadError>>,
    },
    #[expect(unused)]
    Shutdown,
}

/// HashThread wrapper for a BZM2 board worker.
pub struct Bzm2Thread {
    name: String,
    command_tx: mpsc::Sender<ThreadCommand>,
    event_rx: Option<mpsc::Receiver<HashThreadEvent>>,
    capabilities: HashThreadCapabilities,
    status: Arc<RwLock<HashThreadStatus>>,
}

impl Bzm2Thread {
    pub fn new<R, W>(
        name: String,
        chip_responses: R,
        chip_commands: W,
        peripherals: BoardPeripherals,
        removal_rx: watch::Receiver<ThreadRemovalSignal>,
        asic_count: u8,
    ) -> Self
    where
        R: Stream<Item = Result<protocol::Response, io::Error>> + Unpin + Send + 'static,
        W: Sink<protocol::Command> + Unpin + Send + 'static,
        W::Error: std::fmt::Debug,
    {
        let (cmd_tx, cmd_rx) = mpsc::channel(10);
        let (evt_tx, evt_rx) = mpsc::channel(100);

        let status = Arc::new(RwLock::new(HashThreadStatus::default()));
        let status_clone = Arc::clone(&status);

        tokio::spawn(async move {
            bzm2_thread_actor(
                cmd_rx,
                evt_tx,
                removal_rx,
                status_clone,
                chip_responses,
                chip_commands,
                peripherals,
                asic_count,
            )
            .await;
        });

        Self {
            name,
            command_tx: cmd_tx,
            event_rx: Some(evt_rx),
            capabilities: HashThreadCapabilities {
                hashrate_estimate: HashRate::from_terahashes(1.0), // Stub
            },
            status,
        }
    }
}

#[async_trait]
impl HashThread for Bzm2Thread {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> &HashThreadCapabilities {
        &self.capabilities
    }

    async fn update_task(
        &mut self,
        new_task: HashTask,
    ) -> std::result::Result<Option<HashTask>, HashThreadError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ThreadCommand::UpdateTask {
                new_task,
                response_tx,
            })
            .await
            .map_err(|_| HashThreadError::ChannelClosed("command channel closed".into()))?;

        response_rx
            .await
            .map_err(|_| HashThreadError::WorkAssignmentFailed("no response from thread".into()))?
    }

    async fn replace_task(
        &mut self,
        new_task: HashTask,
    ) -> std::result::Result<Option<HashTask>, HashThreadError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ThreadCommand::ReplaceTask {
                new_task,
                response_tx,
            })
            .await
            .map_err(|_| HashThreadError::ChannelClosed("command channel closed".into()))?;

        response_rx
            .await
            .map_err(|_| HashThreadError::WorkAssignmentFailed("no response from thread".into()))?
    }

    async fn go_idle(&mut self) -> std::result::Result<Option<HashTask>, HashThreadError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ThreadCommand::GoIdle { response_tx })
            .await
            .map_err(|_| HashThreadError::ChannelClosed("command channel closed".into()))?;

        response_rx
            .await
            .map_err(|_| HashThreadError::WorkAssignmentFailed("no response from thread".into()))?
    }

    fn take_event_receiver(&mut self) -> Option<mpsc::Receiver<HashThreadEvent>> {
        self.event_rx.take()
    }

    fn status(&self) -> HashThreadStatus {
        self.status.read().expect("status lock poisoned").clone()
    }
}

fn init_failed(msg: impl Into<String>) -> HashThreadError {
    HashThreadError::InitializationFailed(msg.into())
}

async fn send_command<W>(
    chip_commands: &mut W,
    command: protocol::Command,
    context: &str,
) -> Result<(), HashThreadError>
where
    W: Sink<protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    chip_commands
        .send(command)
        .await
        .map_err(|e| init_failed(format!("{context}: {e:?}")))
}

async fn drain_input<R>(chip_responses: &mut R)
where
    R: Stream<Item = Result<protocol::Response, io::Error>> + Unpin,
{
    loop {
        match time::timeout(Duration::from_millis(20), chip_responses.next()).await {
            Ok(Some(_)) => continue,
            _ => break,
        }
    }
}

async fn wait_for_noop<R>(
    chip_responses: &mut R,
    expected_asic_id: u8,
    timeout: Duration,
) -> Result<(), HashThreadError>
where
    R: Stream<Item = Result<protocol::Response, io::Error>> + Unpin,
{
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(init_failed(format!(
                "timeout waiting for NOOP response from ASIC 0x{expected_asic_id:02x}"
            )));
        }

        match time::timeout(remaining, chip_responses.next()).await {
            Ok(Some(Ok(protocol::Response::Noop { asic_hw_id, .. })))
                if asic_hw_id == expected_asic_id =>
            {
                return Ok(());
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(e))) => {
                return Err(init_failed(format!("failed while waiting for NOOP: {e}")));
            }
            Ok(None) => {
                return Err(init_failed("response stream closed while waiting for NOOP"));
            }
            Err(_) => {
                return Err(init_failed(format!(
                    "timeout waiting for NOOP response from ASIC 0x{expected_asic_id:02x}"
                )));
            }
        }
    }
}

async fn read_reg_u32<R, W>(
    chip_responses: &mut R,
    chip_commands: &mut W,
    asic_id: u8,
    engine: u16,
    offset: u16,
    timeout: Duration,
    context: &str,
) -> Result<u32, HashThreadError>
where
    R: Stream<Item = Result<protocol::Response, io::Error>> + Unpin,
    W: Sink<protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    send_command(
        chip_commands,
        protocol::Command::read_reg_u32(asic_id, engine, offset),
        context,
    )
    .await?;

    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(init_failed(format!(
                "{context}: timeout waiting for READREG response"
            )));
        }

        match time::timeout(remaining, chip_responses.next()).await {
            Ok(Some(Ok(protocol::Response::ReadReg { asic_hw_id, data })))
                if asic_hw_id == asic_id =>
            {
                return match data {
                    protocol::ReadRegData::U32(value) => Ok(value),
                    protocol::ReadRegData::U16(value) => Ok(value as u32),
                    protocol::ReadRegData::U8(value) => Ok(value as u32),
                };
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(e))) => {
                return Err(init_failed(format!("{context}: stream read error: {e}")));
            }
            Ok(None) => {
                return Err(init_failed(format!("{context}: response stream closed")));
            }
            Err(_) => {
                return Err(init_failed(format!(
                    "{context}: timeout waiting for response"
                )));
            }
        }
    }
}

async fn write_reg_u32<W>(
    chip_commands: &mut W,
    asic_id: u8,
    engine: u16,
    offset: u16,
    value: u32,
    context: &str,
) -> Result<(), HashThreadError>
where
    W: Sink<protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    send_command(
        chip_commands,
        protocol::Command::write_reg_u32_le(asic_id, engine, offset, value),
        context,
    )
    .await
}

async fn write_reg_u8<W>(
    chip_commands: &mut W,
    asic_id: u8,
    engine: u16,
    offset: u16,
    value: u8,
    context: &str,
) -> Result<(), HashThreadError>
where
    W: Sink<protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    send_command(
        chip_commands,
        protocol::Command::write_reg_u8(asic_id, engine, offset, value),
        context,
    )
    .await
}

async fn group_write_u8<W>(
    chip_commands: &mut W,
    asic_id: u8,
    group: u16,
    offset: u16,
    value: u8,
    context: &str,
) -> Result<(), HashThreadError>
where
    W: Sink<protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    send_command(
        chip_commands,
        protocol::Command::multicast_write_u8(asic_id, group, offset, value),
        context,
    )
    .await
}

fn thermal_c_to_tune_code(thermal_c: f32) -> u32 {
    let tune_code = (2048.0 / 4096.0) + (4096.0 * (thermal_c + 293.8) / 631.8);
    tune_code.max(0.0) as u32
}

fn voltage_mv_to_tune_code(voltage_mv: f32) -> u32 {
    let tune_code = (16384.0 / 6.0) * (2.5 * voltage_mv / 706.7 + 3.0 / 16384.0 + 1.0);
    tune_code.max(0.0) as u32
}

fn calc_pll_dividers(freq_mhz: f32, post1_divider: u8) -> (u32, u32) {
    let fb =
        REF_DIVIDER as f32 * (post1_divider as f32 + 1.0) * (POST2_DIVIDER as f32 + 1.0) * freq_mhz
            / REF_CLK_MHZ;
    let mut fb_div = fb as u32;
    if fb - fb_div as f32 > 0.5 {
        fb_div += 1;
    }

    let post_div = (1 << 12) | (POST2_DIVIDER << 9) | ((post1_divider as u32) << 6) | REF_DIVIDER;
    (post_div, fb_div)
}

fn engine_id(row: u16, col: u16) -> u16 {
    ((col & 0x3f) << 6) | (row & 0x3f)
}

fn is_invalid_engine(row: u16, col: u16) -> bool {
    (row == INVALID_ENGINE_0_ROW && col == INVALID_ENGINE_0_COL)
        || (row == INVALID_ENGINE_1_ROW && col == INVALID_ENGINE_1_COL)
        || (row == INVALID_ENGINE_2_ROW && col == INVALID_ENGINE_2_COL)
        || (row == INVALID_ENGINE_3_ROW && col == INVALID_ENGINE_3_COL)
}

fn logical_engine_index(row: u16, col: u16) -> Option<usize> {
    if row >= ENGINE_ROWS || col >= ENGINE_COLS || is_invalid_engine(row, col) {
        return None;
    }

    let mut logical = 0usize;
    for r in 0..ENGINE_ROWS {
        for c in 0..ENGINE_COLS {
            if is_invalid_engine(r, c) {
                continue;
            }
            if r == row && c == col {
                return Some(logical);
            }
            logical = logical.saturating_add(1);
        }
    }

    None
}

fn engine_extranonce2_for_logical_engine(
    task: &HashTask,
    logical_engine: usize,
) -> Option<Extranonce2> {
    let base = task.en2?;
    let offset = (logical_engine as u64).saturating_add(ENGINE_EN2_OFFSET_START);

    if let Some(range) = task.en2_range.as_ref()
        && range.size == base.size()
    {
        let value = if range.min == 0 && range.max == u64::MAX {
            base.value().wrapping_add(offset)
        } else {
            let span = range.max.saturating_sub(range.min).saturating_add(1);
            let base_value = if base.value() < range.min || base.value() > range.max {
                range.min
            } else {
                base.value()
            };
            let rel = base_value.saturating_sub(range.min);
            range
                .min
                .saturating_add((rel.saturating_add(offset % span)) % span)
        };
        return Extranonce2::new(value, base.size()).ok();
    }

    let width_bits = u32::from(base.size()).saturating_mul(8);
    let max = if width_bits >= 64 {
        u64::MAX
    } else {
        (1u64 << width_bits) - 1
    };
    let value = if max == u64::MAX {
        base.value().wrapping_add(offset)
    } else {
        base.value().wrapping_add(offset) & max
    };
    Extranonce2::new(value, base.size()).ok()
}

fn readresult_sequence_slot(sequence_id: u8) -> u8 {
    sequence_id & 0x3f
}

fn writejob_effective_sequence_id(sequence_id: u8) -> u8 {
    // Keep the thread's assignment tracking in the same sequence domain as
    // Command::write_job (seq_start = (sequence_id % 2) * 4).
    sequence_id % 2
}

fn retain_assigned_task(assigned_tasks: &mut VecDeque<AssignedTask>, new_task: AssignedTask) {
    let slot = readresult_sequence_slot(new_task.sequence_id);
    assigned_tasks.push_back(new_task);

    // Keep a small per-slot history so delayed READRESULT frames can still be
    // validated against recent predecessors in the same visible sequence slot.
    let mut slot_count = assigned_tasks
        .iter()
        .filter(|task| readresult_sequence_slot(task.sequence_id) == slot)
        .count();
    while slot_count > READRESULT_SLOT_HISTORY {
        if let Some(index) = assigned_tasks
            .iter()
            .position(|task| readresult_sequence_slot(task.sequence_id) == slot)
        {
            let _ = assigned_tasks.remove(index);
            slot_count = slot_count.saturating_sub(1);
        } else {
            break;
        }
    }

    while assigned_tasks.len() > READRESULT_ASSIGNMENT_HISTORY_LIMIT {
        let _ = assigned_tasks.pop_front();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReadResultFields {
    sequence: u8,
    timecode: u8,
    sequence_id: u8,
    micro_job_id: u8,
    used_masked_fields: bool,
}

fn resolve_readresult_fields(
    sequence_raw: u8,
    timecode_raw: u8,
    has_sequence_slot: impl Fn(u8) -> bool,
) -> Option<ReadResultFields> {
    let sequence_id_raw = sequence_raw / (MIDSTATE_COUNT as u8);
    let sequence_slot_raw = readresult_sequence_slot(sequence_id_raw);
    if has_sequence_slot(sequence_slot_raw) {
        return Some(ReadResultFields {
            sequence: sequence_raw,
            timecode: timecode_raw,
            sequence_id: sequence_id_raw,
            micro_job_id: sequence_raw % (MIDSTATE_COUNT as u8),
            used_masked_fields: false,
        });
    }

    let sequence_masked = sequence_raw & 0x7f;
    let timecode_masked = timecode_raw & 0x7f;
    let sequence_id_masked = sequence_masked / (MIDSTATE_COUNT as u8);
    let sequence_slot_masked = readresult_sequence_slot(sequence_id_masked);
    if (sequence_masked != sequence_raw || timecode_masked != timecode_raw)
        && has_sequence_slot(sequence_slot_masked)
    {
        return Some(ReadResultFields {
            sequence: sequence_masked,
            timecode: timecode_masked,
            sequence_id: sequence_id_masked,
            micro_job_id: sequence_masked % (MIDSTATE_COUNT as u8),
            used_masked_fields: true,
        });
    }

    None
}

struct TaskJobPayload {
    midstates: [[u8; 32]; MIDSTATE_COUNT],
    merkle_residue: u32,
    timestamp: u32,
}

#[derive(Clone)]
struct EngineAssignment {
    merkle_root: TxMerkleNode,
    extranonce2: Option<Extranonce2>,
    midstates: [[u8; 32]; MIDSTATE_COUNT],
}

#[derive(Clone)]
struct AssignedTask {
    task: HashTask,
    merkle_root: TxMerkleNode,
    engine_assignments: Arc<[EngineAssignment]>,
    microjob_versions: [BlockVersion; MIDSTATE_COUNT],
    sequence_id: u8,
    timestamp_count: u8,
    leading_zeros: u8,
    nonce_minus_value: u32,
}

#[derive(Clone, Debug)]
struct ReplayCheckConfig {
    job_id: Option<String>,
    en2_value: u64,
    en2_size: u8,
    ntime: u32,
    nonce: u32,
    version_bits: u32,
}

#[derive(Clone, Debug)]
struct FocusedReadResultConfig {
    adjusted_nonce: Option<u32>,
    raw_nonce: Option<u32>,
    break_on_match: bool,
}

fn format_replay_en2_hex(value: u64, size: u8) -> String {
    format!("{:0width$x}", value, width = size as usize * 2)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Bzm2CheckResult {
    Correct,
    NotMeetTarget,
    Error,
}

// Compute the four version-mask deltas used across the 4-midstate micro-jobs.
fn midstate_version_mask_variants(version_mask: u32) -> [u32; MIDSTATE_COUNT] {
    if version_mask == 0 {
        return [0, 0, 0, 0];
    }

    let mut mask = version_mask;
    let mut cnt: u32 = 0;
    while (mask % 16) == 0 {
        cnt = cnt.saturating_add(1);
        mask /= 16;
    }

    let mut tmp_mask = 0u32;
    if (mask % 16) != 0 {
        tmp_mask = mask % 16;
    } else if (mask % 8) != 0 {
        tmp_mask = mask % 8;
    } else if (mask % 4) != 0 {
        tmp_mask = mask % 4;
    } else if (mask % 2) != 0 {
        tmp_mask = mask % 2;
    }

    for _ in 0..cnt {
        tmp_mask = tmp_mask.saturating_mul(16);
    }

    [
        0,
        tmp_mask,
        version_mask.saturating_sub(tmp_mask),
        version_mask,
    ]
}

// Derive per-midstate block versions from the template base version and gp_bits mask.
fn task_midstate_versions(task: &HashTask) -> [BlockVersion; MIDSTATE_COUNT] {
    let template = task.template.as_ref();
    let base = template.version.base().to_consensus() as u32;
    let gp_mask = u16::from_be_bytes(*template.version.gp_bits_mask().as_bytes()) as u32;
    let version_mask = gp_mask << 13;
    let variants = midstate_version_mask_variants(version_mask);

    variants.map(|variant| BlockVersion::from_consensus((base | variant) as i32))
}

fn check_result(
    sha256_le: &[u8; 32],
    target_le: &[u8; 32],
    leading_zeros: u8,
) -> Bzm2CheckResult {
    let mut i: usize = 31;
    while i > 0 && sha256_le[i] == 0 {
        i -= 1;
    }

    let threshold = 31i32 - i32::from(leading_zeros / 8);
    if (i as i32) > threshold {
        return Bzm2CheckResult::Error;
    }
    if (i as i32) == threshold {
        let mut bit_count = leading_zeros % 8;
        let mut bit_index = 7u8;
        while bit_count > 0 {
            if (sha256_le[i] & (1u8 << bit_index)) != 0 {
                return Bzm2CheckResult::Error;
            }
            bit_count -= 1;
            bit_index = bit_index.saturating_sub(1);
        }
    }

    for k in (1..=31).rev() {
        if sha256_le[k] < target_le[k] {
            return Bzm2CheckResult::Correct;
        }
        if sha256_le[k] > target_le[k] {
            return Bzm2CheckResult::NotMeetTarget;
        }
    }

    Bzm2CheckResult::Correct
}

fn leading_zero_bits(sha256_le: &[u8; 32]) -> u16 {
    let mut bits = 0u16;
    for byte in sha256_le.iter().rev() {
        if *byte == 0 {
            bits = bits.saturating_add(8);
            continue;
        }
        bits = bits.saturating_add(byte.leading_zeros() as u16);
        return bits;
    }
    bits
}

fn sha256_compress_state(initial_state: [u32; 8], block: &[u8; 64]) -> [u32; 8] {
    let mut w = [0u32; 64];
    for (i, chunk) in block.chunks_exact(4).enumerate() {
        w[i] = u32::from_be_bytes(chunk.try_into().expect("chunk size is 4"));
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }

    let mut a = initial_state[0];
    let mut b = initial_state[1];
    let mut c = initial_state[2];
    let mut d = initial_state[3];
    let mut e = initial_state[4];
    let mut f = initial_state[5];
    let mut g = initial_state[6];
    let mut h = initial_state[7];

    for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ ((!e) & g);
        let t1 = h
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(SHA256_K[i])
            .wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);

        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }

    [
        initial_state[0].wrapping_add(a),
        initial_state[1].wrapping_add(b),
        initial_state[2].wrapping_add(c),
        initial_state[3].wrapping_add(d),
        initial_state[4].wrapping_add(e),
        initial_state[5].wrapping_add(f),
        initial_state[6].wrapping_add(g),
        initial_state[7].wrapping_add(h),
    ]
}

fn sha256_state_to_be_bytes(state: [u32; 8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, word) in state.iter().copied().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

fn bzm2_double_sha_from_midstate_and_tail(midstate_le: &[u8; 32], tail16: &[u8; 16]) -> [u8; 32] {
    // 1) resume SHA256 from midstate with 16-byte tail
    // 2) SHA256 the resulting 32-byte digest again.
    let mut resumed_state = [0u32; 8];
    for (i, chunk) in midstate_le.chunks_exact(4).enumerate() {
        resumed_state[i] = u32::from_le_bytes(chunk.try_into().expect("chunk size is 4"));
    }

    let mut first_block = [0u8; 64];
    first_block[..16].copy_from_slice(tail16);
    first_block[16] = 0x80;
    first_block[56..64].copy_from_slice(&(80u64 * 8).to_be_bytes());
    let first_state = sha256_compress_state(resumed_state, &first_block);
    let first_digest = sha256_state_to_be_bytes(first_state);

    let mut second_block = [0u8; 64];
    second_block[..32].copy_from_slice(&first_digest);
    second_block[32] = 0x80;
    second_block[56..64].copy_from_slice(&(32u64 * 8).to_be_bytes());
    let second_state = sha256_compress_state(SHA256_IV, &second_block);
    sha256_state_to_be_bytes(second_state)
}

fn bzm2_tail16_bytes(assigned: &AssignedTask, ntime: u32, nonce_submit: u32) -> [u8; 16] {
    let merkle_root_bytes = consensus::serialize(&assigned.merkle_root);
    let mut tail16 = [0u8; 16];
    tail16[0..4].copy_from_slice(&merkle_root_bytes[28..32]);
    tail16[4..8].copy_from_slice(&ntime.to_le_bytes());
    tail16[8..12].copy_from_slice(&assigned.task.template.bits.to_consensus().to_le_bytes());
    tail16[12..16].copy_from_slice(&nonce_submit.to_le_bytes());
    tail16
}

#[cfg(test)]
fn hash_bytes_bzm2_order(hash: &bitcoin::BlockHash) -> [u8; 32] {
    *hash.as_byte_array()
}

fn format_hex(data: &[u8]) -> String {
    data.iter()
        .map(|byte| format!("{:02X}", byte))
        .collect::<Vec<_>>()
        .join(" ")
}

fn validation_probe_summary(
    assigned: &AssignedTask,
    version: BlockVersion,
    ntime: u32,
    nonce: u32,
) -> String {
    let header = BlockHeader {
        version,
        prev_blockhash: assigned.task.template.prev_blockhash,
        merkle_root: assigned.merkle_root,
        time: ntime,
        bits: assigned.task.template.bits,
        nonce,
    };
    let header_bytes = consensus::serialize(&header);
    let header_prefix: [u8; 64] = header_bytes[..64]
        .try_into()
        .expect("header prefix length is fixed");
    let midstate = compute_midstate_le(&header_prefix);
    let tail16 = bzm2_tail16_bytes(assigned, ntime, nonce);
    let hash_bytes = bzm2_double_sha_from_midstate_and_tail(&midstate, &tail16);
    let target_bytes = assigned.task.share_target.to_le_bytes();
    let check = check_result(&hash_bytes, &target_bytes, assigned.leading_zeros);
    let lz_bits = leading_zero_bits(&hash_bytes);
    format!(
        "v={:#010x},t={:#010x},n={:#010x},chk={:?},lz={},msb={:#04x}",
        version.to_consensus() as u32,
        ntime,
        nonce,
        check,
        lz_bits,
        hash_bytes[31]
    )
}

fn evaluate_check_with_hash_orders(
    assigned: &AssignedTask,
    version: BlockVersion,
    ntime: u32,
    nonce_submit: u32,
) -> (Bzm2CheckResult, u8, Bzm2CheckResult, u8) {
    let evaluate = |candidate_nonce: u32| {
        let header = BlockHeader {
            version,
            prev_blockhash: assigned.task.template.prev_blockhash,
            merkle_root: assigned.merkle_root,
            time: ntime,
            bits: assigned.task.template.bits,
            nonce: candidate_nonce,
        };
        let header_bytes = consensus::serialize(&header);
        let header_prefix: [u8; 64] = header_bytes[..64]
            .try_into()
            .expect("header prefix length is fixed");
        let midstate = compute_midstate_le(&header_prefix);
        let tail16 = bzm2_tail16_bytes(assigned, ntime, candidate_nonce);
        let hash_bytes = bzm2_double_sha_from_midstate_and_tail(&midstate, &tail16);
        let target_bytes = assigned.task.share_target.to_le_bytes();
        let check = check_result(&hash_bytes, &target_bytes, assigned.leading_zeros);
        (check, hash_bytes[31])
    };

    // Keep the legacy "le/be" labels in focused diagnostics, but compare
    // submit-order nonce vs swapped-order nonce to surface byte-order mistakes.
    let (submit_check, submit_msb) = evaluate(nonce_submit);
    let (swapped_check, swapped_msb) = evaluate(nonce_submit.swap_bytes());
    (submit_check, submit_msb, swapped_check, swapped_msb)
}

fn focused_validation_entry(
    label: &str,
    assigned: &AssignedTask,
    sequence: u8,
    timecode: u8,
    nonce: u32,
) -> String {
    let sequence_id = sequence / (MIDSTATE_COUNT as u8);
    let micro_job_id = (sequence % (MIDSTATE_COUNT as u8)) as usize;
    let version = assigned.microjob_versions[micro_job_id];
    let ntime_rev = assigned
        .task
        .ntime
        .wrapping_add(u32::from(assigned.timestamp_count.wrapping_sub(timecode)));
    let ntime_plus = assigned.task.ntime.wrapping_add(u32::from(timecode));
    let (rev_le, rev_le_msb, rev_be, rev_be_msb) =
        evaluate_check_with_hash_orders(assigned, version, ntime_rev, nonce);
    let (plus_le, plus_le_msb, plus_be, plus_be_msb) =
        evaluate_check_with_hash_orders(assigned, version, ntime_plus, nonce);

    format!(
        "{label}(seq={:#04x}/sid={}/mj={},time={:#04x},n={:#010x},rev(le={:?}/{:#04x},be={:?}/{:#04x}),plus(le={:?}/{:#04x},be={:?}/{:#04x}))",
        sequence,
        sequence_id,
        micro_job_id,
        timecode,
        nonce,
        rev_le,
        rev_le_msb,
        rev_be,
        rev_be_msb,
        plus_le,
        plus_le_msb,
        plus_be,
        plus_be_msb
    )
}

fn focused_readresult_diagnostic(
    assigned: &AssignedTask,
    sequence_raw: u8,
    timecode_raw: u8,
    nonce_raw: u32,
) -> String {
    let sequence_masked = sequence_raw & 0x7f;
    let timecode_masked = timecode_raw & 0x7f;
    let nonce_adjusted = nonce_raw.wrapping_sub(assigned.nonce_minus_value);
    let entries = [
        focused_validation_entry(
            "raw_adj",
            assigned,
            sequence_raw,
            timecode_raw,
            nonce_adjusted,
        ),
        focused_validation_entry("raw_raw", assigned, sequence_raw, timecode_raw, nonce_raw),
        focused_validation_entry(
            "m7_adj",
            assigned,
            sequence_masked,
            timecode_masked,
            nonce_adjusted,
        ),
        focused_validation_entry(
            "m7_raw",
            assigned,
            sequence_masked,
            timecode_masked,
            nonce_raw,
        ),
    ];
    entries.join(" | ")
}

fn parse_hex_u32(input: &str) -> Option<u32> {
    let trimmed = input
        .trim()
        .trim_start_matches("0x")
        .trim_start_matches("0X");
    u32::from_str_radix(trimmed, 16).ok()
}

fn parse_u32_env(input: &str) -> Option<u32> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with("0x") || trimmed.starts_with("0X") {
        return parse_hex_u32(trimmed);
    }
    trimmed
        .parse::<u32>()
        .ok()
        .or_else(|| parse_hex_u32(trimmed))
}

fn parse_bool_env_flag(name: &str) -> bool {
    let Ok(raw) = env::var(name) else {
        return false;
    };
    let v = raw.trim();
    v == "1"
        || v.eq_ignore_ascii_case("true")
        || v.eq_ignore_ascii_case("yes")
        || v.eq_ignore_ascii_case("on")
}

fn parse_focused_readresult_config_from_env() -> Option<FocusedReadResultConfig> {
    let adjusted_nonce = match env::var("MUJINA_BZM2_TRACE_NONCE") {
        Ok(v) => {
            let Some(parsed) = parse_u32_env(&v) else {
                warn!(value = %v, "Invalid MUJINA_BZM2_TRACE_NONCE (expected hex or decimal u32)");
                return None;
            };
            Some(parsed)
        }
        Err(_) => None,
    };
    let raw_nonce = match env::var("MUJINA_BZM2_TRACE_RAW_NONCE") {
        Ok(v) => {
            let Some(parsed) = parse_u32_env(&v) else {
                warn!(value = %v, "Invalid MUJINA_BZM2_TRACE_RAW_NONCE (expected hex or decimal u32)");
                return None;
            };
            Some(parsed)
        }
        Err(_) => None,
    };
    if adjusted_nonce.is_none() && raw_nonce.is_none() {
        return None;
    }
    let break_on_match = parse_bool_env_flag("MUJINA_BZM2_TRACE_BREAK_ON_NONCE");
    Some(FocusedReadResultConfig {
        adjusted_nonce,
        raw_nonce,
        break_on_match,
    })
}

fn parse_replay_check_config_from_env() -> Option<ReplayCheckConfig> {
    let en2_hex = match env::var("MUJINA_BZM2_REPLAY_EN2") {
        Ok(v) => v,
        Err(_) => return None,
    };
    let ntime_s = match env::var("MUJINA_BZM2_REPLAY_NTIME") {
        Ok(v) => v,
        Err(_) => {
            warn!("MUJINA_BZM2_REPLAY_EN2 is set but MUJINA_BZM2_REPLAY_NTIME is missing");
            return None;
        }
    };
    let nonce_s = match env::var("MUJINA_BZM2_REPLAY_NONCE") {
        Ok(v) => v,
        Err(_) => {
            warn!("MUJINA_BZM2_REPLAY_EN2 is set but MUJINA_BZM2_REPLAY_NONCE is missing");
            return None;
        }
    };
    let version_bits_s = match env::var("MUJINA_BZM2_REPLAY_VERSION_BITS") {
        Ok(v) => v,
        Err(_) => {
            warn!("MUJINA_BZM2_REPLAY_EN2 is set but MUJINA_BZM2_REPLAY_VERSION_BITS is missing");
            return None;
        }
    };

    let en2_trim = en2_hex
        .trim()
        .trim_start_matches("0x")
        .trim_start_matches("0X");
    if en2_trim.is_empty() || (en2_trim.len() % 2) != 0 || en2_trim.len() > 16 {
        warn!(
            en2 = %en2_hex,
            "Invalid MUJINA_BZM2_REPLAY_EN2 (must be 1-8 bytes of hex)"
        );
        return None;
    }

    let en2_size = (en2_trim.len() / 2) as u8;
    let mut en2_bytes = [0u8; 8];
    for (idx, pair) in en2_trim.as_bytes().chunks_exact(2).enumerate() {
        let Ok(byte_str) = std::str::from_utf8(pair) else {
            warn!(en2 = %en2_hex, "Invalid UTF-8 in MUJINA_BZM2_REPLAY_EN2");
            return None;
        };
        let Ok(byte) = u8::from_str_radix(byte_str, 16) else {
            warn!(en2 = %en2_hex, "Invalid MUJINA_BZM2_REPLAY_EN2 hex");
            return None;
        };
        en2_bytes[idx] = byte;
    }
    // Stratum submit extranonce2 is sent as raw bytes hex. Extranonce2 stores value as little-endian.
    let en2_value = u64::from_le_bytes(en2_bytes);

    let Some(ntime) = parse_hex_u32(&ntime_s) else {
        warn!(ntime = %ntime_s, "Invalid MUJINA_BZM2_REPLAY_NTIME hex");
        return None;
    };
    let Some(nonce) = parse_hex_u32(&nonce_s) else {
        warn!(nonce = %nonce_s, "Invalid MUJINA_BZM2_REPLAY_NONCE hex");
        return None;
    };
    let Some(version_bits) = parse_hex_u32(&version_bits_s) else {
        warn!(
            version_bits = %version_bits_s,
            "Invalid MUJINA_BZM2_REPLAY_VERSION_BITS hex"
        );
        return None;
    };
    let job_id = env::var("MUJINA_BZM2_REPLAY_JOB_ID")
        .ok()
        .filter(|s| !s.trim().is_empty());

    Some(ReplayCheckConfig {
        job_id,
        en2_value,
        en2_size,
        ntime,
        nonce,
        version_bits,
    })
}

fn log_replay_check_for_task(config: &ReplayCheckConfig, assigned: &AssignedTask) -> bool {
    if let Some(job_id) = &config.job_id
        && assigned.task.template.id.as_str() != job_id
    {
        debug!(
            configured_job_id = %job_id,
            assigned_job_id = %assigned.task.template.id,
            "BZM2 replay check skipped (job_id mismatch)"
        );
        return false;
    }

    let Ok(config_en2) = Extranonce2::new(config.en2_value, config.en2_size) else {
        debug!(
            job_id = %assigned.task.template.id,
            configured_en2 = %format_replay_en2_hex(config.en2_value, config.en2_size),
            "BZM2 replay check skipped (configured extranonce2 invalid for configured size)"
        );
        return false;
    };

    let matched_engine = assigned
        .engine_assignments
        .iter()
        .position(|engine| engine.extranonce2 == Some(config_en2));
    let (task_en2, replay_merkle_root) = if let Some(logical_engine_id) = matched_engine {
        (
            config_en2,
            assigned.engine_assignments[logical_engine_id].merkle_root,
        )
    } else {
        let Some(task_en2) = assigned.task.en2 else {
            debug!(
                job_id = %assigned.task.template.id,
                configured_en2 = %format_replay_en2_hex(config.en2_value, config.en2_size),
                "BZM2 replay check skipped (assigned task has no extranonce2)"
            );
            return false;
        };
        if task_en2 != config_en2 {
            debug!(
                job_id = %assigned.task.template.id,
                configured_en2 = %format_replay_en2_hex(config.en2_value, config.en2_size),
                assigned_en2 = %task_en2,
                "BZM2 replay check skipped (extranonce2 mismatch)"
            );
            return false;
        }
        (task_en2, assigned.merkle_root)
    };

    let base_version = assigned.task.template.version.base().to_consensus() as u32;
    let replay_version_u32 = base_version | config.version_bits;
    let replay_version = BlockVersion::from_consensus(replay_version_u32 as i32);
    let matched_microjob = assigned
        .microjob_versions
        .iter()
        .position(|v| v.to_consensus() as u32 == replay_version_u32);

    let header = BlockHeader {
        version: replay_version,
        prev_blockhash: assigned.task.template.prev_blockhash,
        merkle_root: replay_merkle_root,
        time: config.ntime,
        bits: assigned.task.template.bits,
        nonce: config.nonce,
    };
    let header_bytes = consensus::serialize(&header);
    let replay_midstate = matched_microjob
        .and_then(|idx| {
            matched_engine.map(|logical_engine_id| {
                assigned.engine_assignments[logical_engine_id].midstates[idx]
            })
        })
        .unwrap_or_else(|| {
            let header_prefix: [u8; 64] = header_bytes[..64]
                .try_into()
                .expect("header prefix length is fixed");
            compute_midstate_le(&header_prefix)
        });
    let replay_tail16 = bzm2_tail16_bytes(assigned, config.ntime, config.nonce);
    let hash_bzm2 = bzm2_double_sha_from_midstate_and_tail(&replay_midstate, &replay_tail16);
    let hash = bitcoin::BlockHash::from_byte_array(hash_bzm2);
    let target_bytes = assigned.task.share_target.to_le_bytes();
    let check_result = check_result(&hash_bzm2, &target_bytes, assigned.leading_zeros);
    let achieved_difficulty = Difficulty::from_hash(&hash);
    let target_difficulty = Difficulty::from_target(assigned.task.share_target);

    debug!(
        job_id = %assigned.task.template.id,
        assigned_sequence_id = assigned.sequence_id,
        assigned_en2 = %task_en2,
        replay_en2 = format_args!("{:0width$x}", config.en2_value, width = config.en2_size as usize * 2),
        replay_ntime = format_args!("{:#010x}", config.ntime),
        replay_nonce = format_args!("{:#010x}", config.nonce),
        replay_version_bits = format_args!("{:#010x}", config.version_bits),
        replay_version = format_args!("{:#010x}", replay_version_u32),
        matched_logical_engine = ?matched_engine,
        matched_microjob = ?matched_microjob,
        check_result = ?check_result,
        achieved_difficulty = %achieved_difficulty,
        target_difficulty = %target_difficulty,
        hash_bzm2 = %format_hex(&hash_bzm2),
        header = %format_hex(&header_bytes),
        "BZM2 replay check"
    );
    true
}

fn compute_task_merkle_root(task: &HashTask) -> Result<TxMerkleNode, HashThreadError> {
    let template = task.template.as_ref();
    match &template.merkle_root {
        MerkleRootKind::Computed(_) => {
            let en2 = task.en2.as_ref().ok_or_else(|| {
                HashThreadError::WorkAssignmentFailed(
                    "EN2 is required for computed merkle roots".into(),
                )
            })?;
            template.compute_merkle_root(en2).map_err(|e| {
                HashThreadError::WorkAssignmentFailed(format!("failed to compute merkle root: {e}"))
            })
        }
        MerkleRootKind::Fixed(merkle_root) => Ok(*merkle_root),
    }
}

fn build_header_bytes(
    task: &HashTask,
    version: BlockVersion,
    merkle_root: TxMerkleNode,
) -> Result<[u8; 80], HashThreadError> {
    let template = task.template.as_ref();
    let header = BlockHeader {
        version,
        prev_blockhash: template.prev_blockhash,
        merkle_root,
        time: task.ntime,
        bits: template.bits,
        nonce: 0,
    };

    let bytes = consensus::serialize(&header);
    let len = bytes.len();
    bytes.try_into().map_err(|_| {
        HashThreadError::WorkAssignmentFailed(format!("unexpected serialized header size: {}", len))
    })
}

fn compute_midstate_le(header_prefix_64: &[u8; 64]) -> [u8; 32] {
    // Midstate derivation: SHA256-compress the first 64-byte header block and
    // send the raw SHA256 state words in little-endian byte order (OpenSSL ctx.h on x86).
    let mut w = [0u32; 64];
    for (i, chunk) in header_prefix_64.chunks_exact(4).enumerate() {
        w[i] = u32::from_be_bytes(chunk.try_into().expect("chunk size is 4"));
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }

    let mut a = SHA256_IV[0];
    let mut b = SHA256_IV[1];
    let mut c = SHA256_IV[2];
    let mut d = SHA256_IV[3];
    let mut e = SHA256_IV[4];
    let mut f = SHA256_IV[5];
    let mut g = SHA256_IV[6];
    let mut h = SHA256_IV[7];

    for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ ((!e) & g);
        let t1 = h
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(SHA256_K[i])
            .wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);

        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }

    let state = [
        SHA256_IV[0].wrapping_add(a),
        SHA256_IV[1].wrapping_add(b),
        SHA256_IV[2].wrapping_add(c),
        SHA256_IV[3].wrapping_add(d),
        SHA256_IV[4].wrapping_add(e),
        SHA256_IV[5].wrapping_add(f),
        SHA256_IV[6].wrapping_add(g),
        SHA256_IV[7].wrapping_add(h),
    ];

    let mut out = [0u8; 32];
    for (i, word) in state.iter().copied().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    out
}

fn task_to_bzm2_payload(
    task: &HashTask,
    merkle_root: TxMerkleNode,
    versions: [BlockVersion; MIDSTATE_COUNT],
) -> Result<TaskJobPayload, HashThreadError> {
    let mut midstates = [[0u8; 32]; MIDSTATE_COUNT];
    let mut merkle_residue = 0u32;
    let mut timestamp = 0u32;

    for (idx, midstate) in midstates.iter_mut().enumerate() {
        let header = build_header_bytes(task, versions[idx], merkle_root)?;
        let header_prefix: [u8; 64] = header[..64]
            .try_into()
            .expect("header prefix length is fixed");

        *midstate = compute_midstate_le(&header_prefix);

        if idx == 0 {
            merkle_residue = u32::from_be_bytes(
                header[64..68]
                    .try_into()
                    .expect("slice length is exactly 4 bytes"),
            );
            timestamp = u32::from_be_bytes(
                header[68..72]
                    .try_into()
                    .expect("slice length is exactly 4 bytes"),
            );
        }
    }

    Ok(TaskJobPayload {
        midstates,
        merkle_residue,
        timestamp,
    })
}

fn log_bzm2_job_fingerprint(
    task: &HashTask,
    merkle_root: TxMerkleNode,
    versions: [BlockVersion; MIDSTATE_COUNT],
    payload: &TaskJobPayload,
    sequence_id: u8,
    zeros_to_find: u8,
    timestamp_count: u8,
) -> Result<(), HashThreadError> {
    let target_swapped = task.template.bits.to_consensus().swap_bytes();
    let target_reg_bytes = target_swapped.to_le_bytes();
    let merkle_root_bytes = consensus::serialize(&merkle_root);
    let en2_dbg = task
        .en2
        .as_ref()
        .map(|v| format!("{v:?}"))
        .unwrap_or_else(|| "None".to_owned());

    let mut version_map = Vec::with_capacity(MIDSTATE_COUNT);
    let mut header_tails = Vec::with_capacity(MIDSTATE_COUNT);
    let mut header_full = Vec::with_capacity(MIDSTATE_COUNT);
    let mut midstates_hex = Vec::with_capacity(MIDSTATE_COUNT);

    for (idx, version) in versions.iter().copied().enumerate() {
        let header = build_header_bytes(task, version, merkle_root)?;
        version_map.push(format!("mj{idx}={:#010x}", version.to_consensus() as u32));
        header_tails.push(format!("mj{idx}={}", format_hex(&header[64..80])));
        header_full.push(format!("mj{idx}={}", format_hex(&header)));
        midstates_hex.push(format!("mj{idx}={}", format_hex(&payload.midstates[idx])));
    }

    debug!(
        job_id = %task.template.id,
        sequence_id,
        ntime = format_args!("{:#x}", task.ntime),
        template_time = format_args!("{:#x}", task.template.time),
        bits = format_args!("{:#x}", task.template.bits.to_consensus()),
        share_target = %task.share_target,
        en2 = %en2_dbg,
        zeros_to_find,
        timestamp_count,
        target_reg = %format_hex(&target_reg_bytes),
        merkle_root = %format_hex(&merkle_root_bytes),
        payload_merkle_residue = format_args!("{:#010x}", payload.merkle_residue),
        payload_timestamp = format_args!("{:#010x}", payload.timestamp),
        versions = %version_map.join(" "),
        header_tail = %header_tails.join(" | "),
        midstates = %midstates_hex.join(" | "),
        headers = %header_full.join(" | "),
        "BZM2 job fingerprint"
    );

    Ok(())
}

async fn send_task_to_all_engines<W>(
    chip_commands: &mut W,
    task: &HashTask,
    versions: [BlockVersion; MIDSTATE_COUNT],
    sequence_id: u8,
    zeros_to_find: u8,
    timestamp_count: u8,
) -> Result<Vec<EngineAssignment>, HashThreadError>
where
    W: Sink<protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    // `data[2]` comes from big-endian nbits bytes copied into
    // a little-endian u32, so the numeric value is byte-swapped consensus nbits.
    let target = task.template.bits.to_consensus().swap_bytes();
    let timestamp_reg_value = ((AUTO_CLOCK_UNGATE & 0x1) << 7) | (timestamp_count & 0x7f);
    let mut engine_assignments = Vec::with_capacity(WORK_ENGINE_COUNT);
    let mut fingerprint_logged = false;

    for row in 0..ENGINE_ROWS {
        for col in 0..ENGINE_COLS {
            if is_invalid_engine(row, col) {
                continue;
            }

            let Some(logical_engine_id) = logical_engine_index(row, col) else {
                continue;
            };
            let engine = engine_id(row, col);
            let mut engine_task = task.clone();
            engine_task.en2 = engine_extranonce2_for_logical_engine(task, logical_engine_id);
            let merkle_root = compute_task_merkle_root(&engine_task).map_err(|e| {
                HashThreadError::WorkAssignmentFailed(format!(
                    "failed to derive per-engine merkle root for logical engine {logical_engine_id} (row {row} col {col}): {e}"
                ))
            })?;
            let payload = task_to_bzm2_payload(&engine_task, merkle_root, versions).map_err(|e| {
                HashThreadError::WorkAssignmentFailed(format!(
                    "failed to derive per-engine payload for logical engine {logical_engine_id} (row {row} col {col}): {e}"
                ))
            })?;
            if !fingerprint_logged {
                fingerprint_logged = true;
                if let Err(e) = log_bzm2_job_fingerprint(
                    &engine_task,
                    merkle_root,
                    versions,
                    &payload,
                    sequence_id,
                    zeros_to_find,
                    timestamp_count,
                ) {
                    warn!(error = %e, "Failed to emit BZM2 job fingerprint");
                }
            }
            debug!(
                logical_engine_id,
                engine_hw_id = format_args!("{:#05x}", engine),
                row,
                column = col,
                sequence_id,
                extranonce2 = ?engine_task.en2,
                data0 = format_args!("{:#010x}", payload.merkle_residue),
                data1 = format_args!("{:#010x}", payload.timestamp),
                data2 = format_args!("{:#010x}", target),
                "BZM2 dispatch map"
            );

            write_reg_u8(
                chip_commands,
                protocol::BROADCAST_ASIC,
                engine,
                protocol::engine_reg::ZEROS_TO_FIND,
                zeros_to_find,
                "task assign: ZEROS_TO_FIND",
            )
            .await?;

            write_reg_u8(
                chip_commands,
                protocol::BROADCAST_ASIC,
                engine,
                protocol::engine_reg::TIMESTAMP_COUNT,
                timestamp_reg_value,
                "task assign: TIMESTAMP_COUNT",
            )
            .await?;

            write_reg_u32(
                chip_commands,
                protocol::BROADCAST_ASIC,
                engine,
                protocol::engine_reg::TARGET,
                target,
                "task assign: TARGET",
            )
            .await?;

            let commands = protocol::Command::write_job(
                protocol::BROADCAST_ASIC,
                engine,
                payload.midstates,
                payload.merkle_residue,
                payload.timestamp,
                sequence_id,
                WRITEJOB_CTL_REPLACE,
            )
            .map_err(|e| {
                HashThreadError::WorkAssignmentFailed(format!(
                    "failed to build WRITEJOB payload for engine 0x{engine:03x}: {e}"
                ))
            })?;

            for command in commands {
                chip_commands.send(command).await.map_err(|e| {
                    HashThreadError::WorkAssignmentFailed(format!(
                        "failed to send WRITEJOB to engine 0x{engine:03x}: {e:?}"
                    ))
                })?;
            }
            engine_assignments.push(EngineAssignment {
                merkle_root,
                extranonce2: engine_task.en2,
                midstates: payload.midstates,
            });
        }
    }

    if engine_assignments.len() != WORK_ENGINE_COUNT {
        return Err(HashThreadError::WorkAssignmentFailed(format!(
            "unexpected BZM2 engine assignment count: got {}, expected {}",
            engine_assignments.len(),
            WORK_ENGINE_COUNT
        )));
    }

    Ok(engine_assignments)
}

async fn configure_sensors<R, W>(
    chip_responses: &mut R,
    chip_commands: &mut W,
    read_asic_id: u8,
) -> Result<(), HashThreadError>
where
    R: Stream<Item = Result<protocol::Response, io::Error>> + Unpin,
    W: Sink<protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    let thermal_trip_code = thermal_c_to_tune_code(THERMAL_TRIP_C);
    let voltage_trip_code = voltage_mv_to_tune_code(VOLTAGE_TRIP_MV);

    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::UART_TX,
        0xF,
        "enable sensors: UART_TX",
    )
    .await?;

    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::SLOW_CLK_DIV,
        2,
        "enable sensors: SLOW_CLK_DIV",
    )
    .await?;

    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::SENSOR_CLK_DIV,
        (8 << 5) | 8,
        "enable sensors: SENSOR_CLK_DIV",
    )
    .await?;

    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::DTS_SRST_PD,
        1 << 8,
        "enable sensors: DTS_SRST_PD",
    )
    .await?;

    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::SENS_TDM_GAP_CNT,
        SENSOR_REPORT_INTERVAL,
        "enable sensors: SENS_TDM_GAP_CNT",
    )
    .await?;

    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::DTS_CFG,
        0,
        "enable sensors: DTS_CFG",
    )
    .await?;

    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::SENSOR_THRS_CNT,
        (10 << 16) | 10,
        "enable sensors: SENSOR_THRS_CNT",
    )
    .await?;

    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::TEMPSENSOR_TUNE_CODE,
        0x8001 | (thermal_trip_code << 1),
        "enable sensors: TEMPSENSOR_TUNE_CODE",
    )
    .await?;

    let bandgap = read_reg_u32(
        chip_responses,
        chip_commands,
        read_asic_id,
        protocol::NOTCH_REG,
        protocol::local_reg::BANDGAP,
        INIT_READREG_TIMEOUT,
        "enable sensors: read BANDGAP",
    )
    .await?;
    let bandgap_updated = (bandgap & !0xF) | 0x3;
    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::BANDGAP,
        bandgap_updated,
        "enable sensors: write BANDGAP",
    )
    .await?;

    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::VSENSOR_SRST_PD,
        1 << 8,
        "enable sensors: VSENSOR_SRST_PD",
    )
    .await?;

    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::VSENSOR_CFG,
        (8 << 28) | (1 << 24),
        "enable sensors: VSENSOR_CFG",
    )
    .await?;

    let vs_enable = (voltage_trip_code << 16) | (voltage_trip_code << 1) | 1;
    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::VOLTAGE_SENSOR_ENABLE,
        vs_enable,
        "enable sensors: VOLTAGE_SENSOR_ENABLE",
    )
    .await?;

    Ok(())
}

async fn set_frequency<R, W>(
    chip_responses: &mut R,
    chip_commands: &mut W,
    read_asic_id: u8,
) -> Result<(), HashThreadError>
where
    R: Stream<Item = Result<protocol::Response, io::Error>> + Unpin,
    W: Sink<protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    let (post_div, fb_div) = calc_pll_dividers(TARGET_FREQ_MHZ, POST1_DIVIDER);

    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::PLL_FBDIV,
        fb_div,
        "set frequency: PLL_FBDIV",
    )
    .await?;
    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::PLL_POSTDIV,
        post_div,
        "set frequency: PLL_POSTDIV",
    )
    .await?;

    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::PLL1_FBDIV,
        fb_div,
        "set frequency: PLL1_FBDIV",
    )
    .await?;
    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::PLL1_POSTDIV,
        post_div,
        "set frequency: PLL1_POSTDIV",
    )
    .await?;

    time::sleep(Duration::from_millis(1)).await;

    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::PLL_ENABLE,
        1,
        "set frequency: PLL_ENABLE",
    )
    .await?;
    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::PLL1_ENABLE,
        1,
        "set frequency: PLL1_ENABLE",
    )
    .await?;

    let deadline = Instant::now() + PLL_LOCK_TIMEOUT;
    for pll_enable_offset in [
        protocol::local_reg::PLL_ENABLE,
        protocol::local_reg::PLL1_ENABLE,
    ] {
        loop {
            let lock = read_reg_u32(
                chip_responses,
                chip_commands,
                read_asic_id,
                protocol::NOTCH_REG,
                pll_enable_offset,
                INIT_READREG_TIMEOUT,
                "set frequency: wait PLL lock",
            )
            .await?;
            if (lock & PLL_LOCK_MASK) != 0 {
                break;
            }

            if Instant::now() >= deadline {
                return Err(init_failed(format!(
                    "set frequency: PLL at offset 0x{pll_enable_offset:02x} failed to lock"
                )));
            }

            time::sleep(PLL_POLL_DELAY).await;
        }
    }

    Ok(())
}

async fn soft_reset<W>(chip_commands: &mut W, asic_id: u8) -> Result<(), HashThreadError>
where
    W: Sink<protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    write_reg_u32(
        chip_commands,
        asic_id,
        protocol::NOTCH_REG,
        protocol::local_reg::ENG_SOFT_RESET,
        0,
        "soft reset assert",
    )
    .await?;
    time::sleep(SOFT_RESET_DELAY).await;
    write_reg_u32(
        chip_commands,
        asic_id,
        protocol::NOTCH_REG,
        protocol::local_reg::ENG_SOFT_RESET,
        1,
        "soft reset release",
    )
    .await?;
    time::sleep(SOFT_RESET_DELAY).await;
    Ok(())
}

async fn set_all_clock_gates<W>(chip_commands: &mut W, asic_id: u8) -> Result<(), HashThreadError>
where
    W: Sink<protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    for group_id in 0..ENGINE_ROWS {
        group_write_u8(
            chip_commands,
            asic_id,
            group_id,
            protocol::engine_reg::CONFIG,
            ENGINE_CONFIG_ENHANCED_MODE_BIT,
            "set all clock gates",
        )
        .await?;
    }
    Ok(())
}

async fn set_asic_nonce_range<W>(
    chip_commands: &mut W,
    asic_id: u8,
) -> Result<(), HashThreadError>
where
    W: Sink<protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    let start_nonce = BZM2_START_NONCE;
    let end_nonce = BZM2_END_NONCE;

    for col in 0..ENGINE_COLS {
        for row in 0..ENGINE_ROWS {
            if is_invalid_engine(row, col) {
                continue;
            }
            let engine = engine_id(row, col);
            write_reg_u32(
                chip_commands,
                asic_id,
                engine,
                protocol::engine_reg::START_NONCE,
                start_nonce,
                "set nonce range: START_NONCE",
            )
            .await?;
            write_reg_u32(
                chip_commands,
                asic_id,
                engine,
                protocol::engine_reg::END_NONCE,
                end_nonce,
                "set nonce range: END_NONCE",
            )
            .await?;
        }
    }

    Ok(())
}

async fn start_warm_up_jobs<W>(chip_commands: &mut W, asic_id: u8) -> Result<(), HashThreadError>
where
    W: Sink<protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    for col in 0..ENGINE_COLS {
        for row in 0..ENGINE_ROWS {
            if is_invalid_engine(row, col) {
                continue;
            }
            let engine = engine_id(row, col);

            write_reg_u8(
                chip_commands,
                asic_id,
                engine,
                protocol::engine_reg::TIMESTAMP_COUNT,
                0xff,
                "warm-up: TIMESTAMP_COUNT",
            )
            .await?;

            for seq in [0xfc, 0xfd, 0xfe, 0xff] {
                write_reg_u8(
                    chip_commands,
                    asic_id,
                    engine,
                    protocol::engine_reg::SEQUENCE_ID,
                    seq,
                    "warm-up: SEQUENCE_ID",
                )
                .await?;
            }

            write_reg_u8(
                chip_commands,
                asic_id,
                engine,
                protocol::engine_reg::JOB_CONTROL,
                1,
                "warm-up: JOB_CONTROL",
            )
            .await?;
        }
    }
    Ok(())
}

async fn initialize_chip<R, W>(
    chip_responses: &mut R,
    chip_commands: &mut W,
    peripherals: &mut BoardPeripherals,
    asic_count: u8,
) -> Result<Vec<u8>, HashThreadError>
where
    R: Stream<Item = Result<protocol::Response, io::Error>> + Unpin,
    W: Sink<protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    if asic_count == 0 {
        return Err(init_failed("asic_count must be > 0"));
    }

    if let Some(ref mut asic_enable) = peripherals.asic_enable {
        asic_enable
            .enable()
            .await
            .map_err(|e| init_failed(format!("failed to release reset for BZM2 bring-up: {e}")))?;
    }
    time::sleep(Duration::from_millis(200)).await;

    drain_input(chip_responses).await;

    send_command(
        chip_commands,
        protocol::Command::Noop {
            asic_hw_id: protocol::DEFAULT_ASIC_ID,
        },
        "default ping",
    )
    .await?;
    wait_for_noop(chip_responses, protocol::DEFAULT_ASIC_ID, INIT_NOOP_TIMEOUT).await?;
    debug!("BZM2 default ASIC ID ping succeeded");

    let mut asic_ids = Vec::with_capacity(asic_count as usize);
    for index in 0..asic_count {
        let asic_id = protocol::logical_to_hw_asic_id(index);
        if protocol::hw_to_logical_asic_id(asic_id) != Some(index) {
            return Err(init_failed(format!(
                "invalid ASIC ID mapping for logical index {} -> 0x{:02x}",
                index, asic_id
            )));
        }

        write_reg_u32(
            chip_commands,
            protocol::DEFAULT_ASIC_ID,
            protocol::NOTCH_REG,
            protocol::local_reg::ASIC_ID,
            asic_id as u32,
            "program chain IDs",
        )
        .await?;
        time::sleep(Duration::from_millis(50)).await;

        let readback = read_reg_u32(
            chip_responses,
            chip_commands,
            asic_id,
            protocol::NOTCH_REG,
            protocol::local_reg::ASIC_ID,
            INIT_READREG_TIMEOUT,
            "verify programmed ASIC ID",
        )
        .await?;

        if (readback & 0xff) as u8 != asic_id {
            return Err(init_failed(format!(
                "ASIC ID verify mismatch for 0x{asic_id:02x}: read 0x{readback:08x}"
            )));
        }

        asic_ids.push(asic_id);
    }
    debug!(asic_ids = ?asic_ids, "BZM2 chain IDs programmed");

    drain_input(chip_responses).await;
    for &asic_id in &asic_ids {
        send_command(
            chip_commands,
            protocol::Command::Noop {
                asic_hw_id: asic_id,
            },
            "per-ASIC ping",
        )
        .await?;
        wait_for_noop(chip_responses, asic_id, INIT_NOOP_TIMEOUT).await?;
    }
    debug!("BZM2 per-ASIC ping succeeded");

    let first_asic = *asic_ids
        .first()
        .ok_or_else(|| init_failed("no ASIC IDs programmed"))?;

    debug!("Configuring BZM2 sensors");
    configure_sensors(chip_responses, chip_commands, first_asic).await?;
    debug!("Configuring BZM2 PLL");
    set_frequency(chip_responses, chip_commands, first_asic).await?;

    write_reg_u8(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::CKDCCR_5_0,
        0x00,
        "disable DLL0",
    )
    .await?;
    write_reg_u8(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::CKDCCR_5_1,
        0x00,
        "disable DLL1",
    )
    .await?;

    let uart_tdm_control = (0x7f << 9) | (100 << 1) | 1;
    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::UART_TDM_CTL,
        uart_tdm_control,
        "enable UART TDM mode",
    )
    .await?;

    write_reg_u32(
        chip_commands,
        first_asic,
        protocol::NOTCH_REG,
        protocol::local_reg::IO_PEPS_DS,
        DRIVE_STRENGTH_STRONG,
        "set drive strength",
    )
    .await?;

    for &asic_id in &asic_ids {
        debug!(asic_id, "BZM2 soft reset + clock gate + warm-up start");
        soft_reset(chip_commands, asic_id).await?;
        set_all_clock_gates(chip_commands, asic_id).await?;
        set_asic_nonce_range(chip_commands, asic_id).await?;
        start_warm_up_jobs(chip_commands, asic_id).await?;
        debug!(asic_id, "BZM2 warm-up complete");
    }

    write_reg_u32(
        chip_commands,
        protocol::BROADCAST_ASIC,
        protocol::NOTCH_REG,
        protocol::local_reg::RESULT_STS_CTL,
        0x10,
        "enable TDM results",
    )
    .await?;

    Ok(asic_ids)
}

async fn bzm2_thread_actor<R, W>(
    mut cmd_rx: mpsc::Receiver<ThreadCommand>,
    evt_tx: mpsc::Sender<HashThreadEvent>,
    mut removal_rx: watch::Receiver<ThreadRemovalSignal>,
    status: Arc<RwLock<HashThreadStatus>>,
    mut chip_responses: R,
    mut chip_commands: W,
    mut peripherals: BoardPeripherals,
    asic_count: u8,
) where
    R: Stream<Item = Result<protocol::Response, io::Error>> + Unpin,
    W: Sink<protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    if let Some(ref mut asic_enable) = peripherals.asic_enable
        && let Err(e) = asic_enable.disable().await
    {
        warn!(error = %e, "Failed to disable BZM2 ASIC on thread startup");
    }

    let mut chip_initialized = false;
    let mut current_task: Option<HashTask> = None;
    let mut assigned_tasks: VecDeque<AssignedTask> =
        VecDeque::with_capacity(READRESULT_ASSIGNMENT_HISTORY_LIMIT);
    let mut next_sequence_id: u8 = 0;
    let mut sanity_candidates_total: u64 = 0;
    let mut sanity_candidates_meet_task: u64 = 0;
    let mut sanity_best_difficulty: Option<Difficulty> = None;
    let mut sanity_diagnostic_samples: u64 = 0;
    let mut sequence_lookup_diagnostic_samples: u64 = 0;
    let mut zero_lz_diagnostic_samples: u64 = 0;
    let replay_check_config = parse_replay_check_config_from_env();
    let focused_readresult_config = parse_focused_readresult_config_from_env();
    if let Some(cfg) = replay_check_config.as_ref() {
        info!(
            replay_job_id = ?cfg.job_id,
            replay_en2 = %format_replay_en2_hex(cfg.en2_value, cfg.en2_size),
            replay_ntime = format_args!("{:#010x}", cfg.ntime),
            replay_nonce = format_args!("{:#010x}", cfg.nonce),
            replay_version_bits = format_args!("{:#010x}", cfg.version_bits),
            "BZM2 replay check configured"
        );
    }
    if let Some(cfg) = focused_readresult_config.as_ref() {
        info!(
            trace_nonce = ?cfg.adjusted_nonce.map(|n| format!("{:#010x}", n)),
            trace_raw_nonce = ?cfg.raw_nonce.map(|n| format!("{:#010x}", n)),
            break_on_match = cfg.break_on_match,
            "BZM2 focused READRESULT tracing configured"
        );
    }
    let mut replay_check_hits: u64 = 0;
    let mut status_ticker = time::interval(Duration::from_secs(5));
    status_ticker.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = removal_rx.changed() => {
                let signal = removal_rx.borrow().clone();
                if signal != ThreadRemovalSignal::Running {
                    {
                        let mut s = status.write().expect("status lock poisoned");
                        s.is_active = false;
                    }
                    break;
                }
            }

            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    ThreadCommand::UpdateTask { new_task, response_tx } => {
                        if !chip_initialized {
                            match initialize_chip(&mut chip_responses, &mut chip_commands, &mut peripherals, asic_count).await {
                                Ok(ids) => {
                                    chip_initialized = true;
                                    info!(
                                        asic_ids = ?ids,
                                        "BZM2 initialization completed"
                                    );
                                }
                                Err(e) => {
                                    error!(error = %e, "BZM2 chip initialization failed");
                                    let _ = response_tx.send(Err(e));
                                    continue;
                                }
                            }
                        }

                        let microjob_versions = task_midstate_versions(&new_task);
                        let write_sequence_id = writejob_effective_sequence_id(next_sequence_id);

                        let engine_assignments = match send_task_to_all_engines(
                            &mut chip_commands,
                            &new_task,
                            microjob_versions,
                            write_sequence_id,
                            ENGINE_ZEROS_TO_FIND,
                            ENGINE_TIMESTAMP_COUNT,
                        )
                        .await
                        {
                            Ok(assignments) => assignments,
                            Err(e) => {
                                error!(error = %e, "Failed to send BZM2 work during update_task");
                                let _ = response_tx.send(Err(e));
                                continue;
                            }
                        };
                        let Some(default_assignment) = engine_assignments.first().cloned() else {
                            let e = HashThreadError::WorkAssignmentFailed(
                                "no engine assignments produced for update_task".into(),
                            );
                            error!(error = %e, "Failed to send BZM2 work during update_task");
                            let _ = response_tx.send(Err(e));
                            continue;
                        };

                        // `job_ctl=3` behavior: old jobs are canceled on every assign.
                        let new_assigned_task = AssignedTask {
                            task: new_task.clone(),
                            merkle_root: default_assignment.merkle_root,
                            engine_assignments: Arc::from(engine_assignments.into_boxed_slice()),
                            microjob_versions,
                            sequence_id: write_sequence_id,
                            timestamp_count: ENGINE_TIMESTAMP_COUNT,
                            leading_zeros: ENGINE_LEADING_ZEROS,
                            nonce_minus_value: BZM2_NONCE_MINUS,
                        };
                        retain_assigned_task(&mut assigned_tasks, new_assigned_task);
                        if let Some(cfg) = replay_check_config.as_ref()
                            && let Some(assigned) = assigned_tasks.back()
                        {
                            if log_replay_check_for_task(cfg, assigned) {
                                replay_check_hits = replay_check_hits.saturating_add(1);
                                trace!(
                                    replay_check_hits,
                                    "BZM2 replay check matched on update_task"
                                );
                            }
                        }

                        debug!(
                            job_id = %new_task.template.id,
                            sequence_id = next_sequence_id,
                            write_sequence_id,
                            "Sent BZM2 WRITEJOB payloads for update_task"
                        );
                        next_sequence_id = next_sequence_id.wrapping_add(1);

                        let old_task = current_task.replace(new_task);
                        {
                            let mut s = status.write().expect("status lock poisoned");
                            s.is_active = true;
                        }
                        let _ = response_tx.send(Ok(old_task));
                    }
                    ThreadCommand::ReplaceTask { new_task, response_tx } => {
                        if !chip_initialized {
                            match initialize_chip(&mut chip_responses, &mut chip_commands, &mut peripherals, asic_count).await {
                                Ok(ids) => {
                                    chip_initialized = true;
                                    info!(
                                        asic_ids = ?ids,
                                        "BZM2 initialization completed"
                                    );
                                }
                                Err(e) => {
                                    error!(error = %e, "BZM2 chip initialization failed");
                                    let _ = response_tx.send(Err(e));
                                    continue;
                                }
                            }
                        }

                        let microjob_versions = task_midstate_versions(&new_task);
                        let write_sequence_id = writejob_effective_sequence_id(next_sequence_id);

                        let engine_assignments = match send_task_to_all_engines(
                            &mut chip_commands,
                            &new_task,
                            microjob_versions,
                            write_sequence_id,
                            ENGINE_ZEROS_TO_FIND,
                            ENGINE_TIMESTAMP_COUNT,
                        )
                        .await
                        {
                            Ok(assignments) => assignments,
                            Err(e) => {
                                error!(error = %e, "Failed to send BZM2 work during replace_task");
                                let _ = response_tx.send(Err(e));
                                continue;
                            }
                        };
                        let Some(default_assignment) = engine_assignments.first().cloned() else {
                            let e = HashThreadError::WorkAssignmentFailed(
                                "no engine assignments produced for replace_task".into(),
                            );
                            error!(error = %e, "Failed to send BZM2 work during replace_task");
                            let _ = response_tx.send(Err(e));
                            continue;
                        };

                        // `job_ctl=3` behavior: old jobs are canceled on every assign.
                        let new_assigned_task = AssignedTask {
                            task: new_task.clone(),
                            merkle_root: default_assignment.merkle_root,
                            engine_assignments: Arc::from(engine_assignments.into_boxed_slice()),
                            microjob_versions,
                            sequence_id: write_sequence_id,
                            timestamp_count: ENGINE_TIMESTAMP_COUNT,
                            leading_zeros: ENGINE_LEADING_ZEROS,
                            nonce_minus_value: BZM2_NONCE_MINUS,
                        };
                        retain_assigned_task(&mut assigned_tasks, new_assigned_task);
                        if let Some(cfg) = replay_check_config.as_ref()
                            && let Some(assigned) = assigned_tasks.back()
                        {
                            if log_replay_check_for_task(cfg, assigned) {
                                replay_check_hits = replay_check_hits.saturating_add(1);
                                trace!(
                                    replay_check_hits,
                                    "BZM2 replay check matched on replace_task"
                                );
                            }
                        }

                        debug!(
                            job_id = %new_task.template.id,
                            sequence_id = next_sequence_id,
                            write_sequence_id,
                            "Sent BZM2 WRITEJOB payloads for replace_task"
                        );
                        next_sequence_id = next_sequence_id.wrapping_add(1);

                        let old_task = current_task.replace(new_task);
                        {
                            let mut s = status.write().expect("status lock poisoned");
                            s.is_active = true;
                        }
                        let _ = response_tx.send(Ok(old_task));
                    }
                    ThreadCommand::GoIdle { response_tx } => {
                        let old_task = current_task.take();
                        assigned_tasks.clear();
                        {
                            let mut s = status.write().expect("status lock poisoned");
                            s.is_active = false;
                        }
                        let _ = response_tx.send(Ok(old_task));
                    }
                    ThreadCommand::Shutdown => {
                        break;
                    }
                }
            }

            Some(result) = chip_responses.next() => {
                match result {
                    Ok(protocol::Response::Noop { asic_hw_id, signature }) => {
                        trace!(asic_hw_id, signature = ?signature, "BZM2 NOOP response");
                    }
                    Ok(protocol::Response::ReadReg { asic_hw_id, data }) => {
                        trace!(asic_hw_id, data = ?data, "BZM2 READREG response");
                    }
                    Ok(protocol::Response::DtsVs { asic_hw_id, data }) => {
                        // Temporarily suppress noisy DTS/VS logging while debugging share flow.
                        let _ = (asic_hw_id, data);
                    }
                    Ok(protocol::Response::ReadResult {
                        asic_hw_id,
                        engine_id,
                        status: result_status,
                        nonce,
                        sequence,
                        timecode,
                    }) => {
                        // status bit3 indicates a valid nonce candidate.
                        if (result_status & 0x8) == 0 {
                            trace!(
                                asic_hw_id,
                                engine_id,
                                result_status,
                                nonce,
                                sequence,
                                timecode,
                                "Ignoring BZM2 READRESULT without valid-nonce flag"
                            );
                            continue;
                        }

                        let row = engine_id & 0x3f;
                        let column = engine_id >> 6;
                        if row >= ENGINE_ROWS || column >= ENGINE_COLS {
                            trace!(
                                asic_hw_id,
                                engine_id,
                                row,
                                column,
                                sequence,
                                "Ignoring BZM2 READRESULT with unmapped engine coordinates"
                            );
                            continue;
                        }
                        if is_invalid_engine(row, column) {
                            trace!(
                                asic_hw_id,
                                engine_id,
                                row,
                                column,
                                sequence,
                                "Ignoring BZM2 READRESULT from invalid engine coordinate"
                            );
                            continue;
                        }
                        let Some(logical_engine_id) = logical_engine_index(row, column) else {
                            trace!(
                                asic_hw_id,
                                engine_id,
                                row,
                                column,
                                sequence,
                                "Ignoring BZM2 READRESULT with unmapped logical engine index"
                            );
                            continue;
                        };

                        let sequence_id_raw = sequence / (MIDSTATE_COUNT as u8);
                        let sequence_masked = sequence & 0x7f;
                        let sequence_id_masked = sequence_masked / (MIDSTATE_COUNT as u8);
                        let micro_job_id_masked = sequence_masked % (MIDSTATE_COUNT as u8);
                        let timecode_masked = timecode & 0x7f;
                        let Some(resolved_fields) =
                            resolve_readresult_fields(sequence, timecode, |slot| {
                                assigned_tasks.iter().rev().any(|task| {
                                    readresult_sequence_slot(task.sequence_id) == slot
                                })
                            })
                        else {
                            if sequence_lookup_diagnostic_samples < SEQUENCE_LOOKUP_DIAGNOSTIC_LIMIT {
                                sequence_lookup_diagnostic_samples =
                                    sequence_lookup_diagnostic_samples.saturating_add(1);
                                let masked_match = assigned_tasks
                                    .iter()
                                    .rev()
                                    .find(|task| {
                                        readresult_sequence_slot(task.sequence_id)
                                            == sequence_id_masked
                                    })
                                    .map(|task| task.sequence_id);
                                let recent_slots: Vec<u8> = assigned_tasks
                                    .iter()
                                    .rev()
                                    .take(6)
                                    .map(|task| readresult_sequence_slot(task.sequence_id))
                                    .collect();
                                let recent_sequence_ids: Vec<u8> = assigned_tasks
                                    .iter()
                                    .rev()
                                    .take(6)
                                    .map(|task| task.sequence_id)
                                    .collect();
                                debug!(
                                    asic_hw_id,
                                    engine_id,
                                    sequence_raw = format_args!("{:#04x}", sequence),
                                    sequence_id_raw,
                                    sequence_masked = format_args!("{:#04x}", sequence_masked),
                                    sequence_id_masked,
                                    micro_job_id_masked,
                                    timecode_raw = format_args!("{:#04x}", timecode),
                                    timecode_masked = format_args!("{:#04x}", timecode_masked),
                                    masked_lookup_hit = masked_match.is_some(),
                                    masked_lookup_sequence_id = ?masked_match,
                                    recent_slots = ?recent_slots,
                                    recent_sequence_ids = ?recent_sequence_ids,
                                    "BZM2 READRESULT lookup diagnostic"
                                );
                            }
                            trace!(
                                asic_hw_id,
                                engine_id,
                                sequence_id_raw,
                                sequence,
                                timecode,
                                "Ignoring BZM2 READRESULT with no assigned task"
                            );
                            continue;
                        };
                        let sequence_id = resolved_fields.sequence_id;
                        let micro_job_id = resolved_fields.micro_job_id;
                        let sequence_effective = resolved_fields.sequence;
                        let timecode_effective = resolved_fields.timecode;
                        let sequence_slot = readresult_sequence_slot(sequence_id);
                        let slot_candidates: Vec<AssignedTask> = assigned_tasks
                            .iter()
                            .rev()
                            .filter(|task| readresult_sequence_slot(task.sequence_id) == sequence_slot)
                            .cloned()
                            .collect();
                        let slot_candidate_count = slot_candidates.len();
                        if slot_candidate_count == 0 {
                            trace!(
                                asic_hw_id,
                                engine_id,
                                sequence_id,
                                sequence_raw = sequence,
                                sequence_effective,
                                "Ignoring BZM2 READRESULT with no assigned task after field resolution"
                            );
                            continue;
                        }

                        let nonce_raw = nonce;
                        let mut selected_candidate: Option<(
                            AssignedTask,
                            BlockVersion,
                            [u8; 32],
                            u32,
                            u32,
                            u32,
                            u32,
                            BlockHeader,
                            [u8; 16],
                            [u8; 32],
                            bitcoin::BlockHash,
                            [u8; 32],
                            Bzm2CheckResult,
                            u16,
                            Difficulty,
                            Difficulty,
                            f64,
                            f64,
                        )> = None;
                        let mut selected_rank = 0u8;

                        for mut candidate in slot_candidates {
                            let Some(engine_assignment) =
                                candidate.engine_assignments.get(logical_engine_id).cloned()
                            else {
                                continue;
                            };
                            candidate.merkle_root = engine_assignment.merkle_root;
                            candidate.task.en2 = engine_assignment.extranonce2;
                            let share_version = candidate.microjob_versions[micro_job_id as usize];
                            let selected_midstate = engine_assignment.midstates[micro_job_id as usize];
                            // Result time is reverse-counted and must be
                            // converted into a forward ntime offset.
                            let ntime_offset =
                                u32::from(candidate.timestamp_count.wrapping_sub(timecode_effective));
                            let share_ntime = candidate.task.ntime.wrapping_add(ntime_offset);
                            // READRESULT mapping:
                            // READRESULT nonce is first adjusted by nonce_minus, then byte-swapped
                            // for reconstructed header hashing and Stratum submit nonce field.
                            let nonce_adjusted = nonce_raw.wrapping_sub(candidate.nonce_minus_value);
                            let nonce_submit = nonce_adjusted.swap_bytes();

                            // Build a canonical header for logging/replay diagnostics.
                            let header = BlockHeader {
                                version: share_version,
                                prev_blockhash: candidate.task.template.prev_blockhash,
                                merkle_root: candidate.merkle_root,
                                time: share_ntime,
                                bits: candidate.task.template.bits,
                                nonce: nonce_submit,
                            };
                            let tail16 = bzm2_tail16_bytes(&candidate, share_ntime, nonce_submit);
                            let hash_bytes =
                                bzm2_double_sha_from_midstate_and_tail(&selected_midstate, &tail16);
                            let hash = bitcoin::BlockHash::from_byte_array(hash_bytes);
                            let target_bytes = candidate.task.share_target.to_le_bytes();
                            let check_result = check_result(
                                &hash_bytes,
                                &target_bytes,
                                candidate.leading_zeros,
                            );
                            let observed_leading_zeros =
                                leading_zero_bits(&hash_bytes);
                            let achieved_difficulty = Difficulty::from_hash(&hash);
                            let target_difficulty =
                                Difficulty::from_target(candidate.task.share_target);
                            let achieved_difficulty_f64 = achieved_difficulty.as_f64();
                            let target_difficulty_f64 = target_difficulty.as_f64();
                            let rank = match check_result {
                                Bzm2CheckResult::Correct => 3,
                                Bzm2CheckResult::NotMeetTarget => 2,
                                Bzm2CheckResult::Error => 1,
                            };

                            if selected_candidate.is_none() || rank > selected_rank {
                                selected_rank = rank;
                                selected_candidate = Some((
                                    candidate,
                                    share_version,
                                    selected_midstate,
                                    ntime_offset,
                                    share_ntime,
                                    nonce_adjusted,
                                    nonce_submit,
                                    header,
                                    tail16,
                                    hash_bytes,
                                    hash,
                                    target_bytes,
                                    check_result,
                                    observed_leading_zeros,
                                    achieved_difficulty,
                                    target_difficulty,
                                    achieved_difficulty_f64,
                                    target_difficulty_f64,
                                ));
                                if rank == 3 {
                                    break;
                                }
                            }
                        }

                        let Some((
                            assigned,
                            share_version,
                            selected_midstate,
                            ntime_offset,
                            share_ntime,
                            nonce_adjusted,
                            nonce_submit,
                            header,
                            tail16,
                            hash_bytes,
                            hash,
                            target_bytes,
                            check_result,
                            observed_leading_zeros,
                            achieved_difficulty,
                            target_difficulty,
                            achieved_difficulty_f64,
                            target_difficulty_f64,
                        )) = selected_candidate
                        else {
                            trace!(
                                asic_hw_id,
                                engine_id,
                                logical_engine_id,
                                sequence_id,
                                slot_candidate_count,
                                "Ignoring BZM2 READRESULT without a usable retained assignment"
                            );
                            continue;
                        };

                        if slot_candidate_count > 1 {
                            trace!(
                                asic_hw_id,
                                engine_id,
                                logical_engine_id,
                                sequence_id,
                                matched_sequence_id = assigned.sequence_id,
                                slot_candidate_count,
                                "BZM2 READRESULT evaluated retained slot history"
                            );
                        }

                        if resolved_fields.used_masked_fields {
                            trace!(
                                asic_hw_id,
                                engine_id,
                                sequence_raw = format_args!("{:#04x}", sequence),
                                sequence_effective = format_args!("{:#04x}", sequence_effective),
                                timecode_raw = format_args!("{:#04x}", timecode),
                                timecode_effective = format_args!("{:#04x}", timecode_effective),
                                "BZM2 READRESULT using masked sequence/timecode fields"
                            );
                        }

                        sanity_candidates_total = sanity_candidates_total.saturating_add(1);
                        if sanity_best_difficulty.map_or(true, |best| achieved_difficulty > best) {
                            sanity_best_difficulty = Some(achieved_difficulty);
                        }

                        if let Some(cfg) = focused_readresult_config.as_ref() {
                            let adjusted_match = cfg.adjusted_nonce.map_or(true, |n| n == nonce_adjusted);
                            let raw_match = cfg.raw_nonce.map_or(true, |n| n == nonce_raw);
                            if adjusted_match && raw_match {
                                let header_bytes = consensus::serialize(&header);
                                let merkle_root_bytes = consensus::serialize(&assigned.merkle_root);
                                let header_prefix: [u8; 64] = header_bytes[..64]
                                    .try_into()
                                    .expect("header prefix length is fixed");
                                let derived_midstate = compute_midstate_le(&header_prefix);
                                let mut hash_rev = hash_bytes;
                                hash_rev.reverse();
                                debug!(
                                    asic_hw_id,
                                    engine_hw_id = engine_id,
                                    logical_engine_id,
                                    sequence_raw = format_args!("{:#04x}", sequence),
                                    sequence_effective = format_args!("{:#04x}", sequence_effective),
                                    sequence_id,
                                    micro_job_id,
                                    timecode_raw = format_args!("{:#04x}", timecode),
                                    timecode_effective = format_args!("{:#04x}", timecode_effective),
                                    nonce_raw = format_args!("{:#010x}", nonce_raw),
                                    nonce_adjusted = format_args!("{:#010x}", nonce_adjusted),
                                    nonce_submit = format_args!("{:#010x}", nonce_submit),
                                    nonce_minus_value = format_args!("{:#x}", assigned.nonce_minus_value),
                                    ntime_offset,
                                    ntime = format_args!("{:#010x}", share_ntime),
                                    version = format_args!("{:#010x}", share_version.to_consensus() as u32),
                                    bits = format_args!("{:#010x}", assigned.task.template.bits.to_consensus()),
                                    extranonce2 = ?assigned.task.en2,
                                    merkle_root = %format_hex(&merkle_root_bytes),
                                    midstate = %format_hex(&selected_midstate),
                                    derived_midstate = %format_hex(&derived_midstate),
                                    header = %format_hex(&header_bytes),
                                    tail16 = %format_hex(&tail16),
                                    hash_bzm2_order = %format_hex(&hash_bytes),
                                    hash_reversed = %format_hex(&hash_rev),
                                    hash_msb_bzm2 = format_args!("{:#04x}", hash_bytes[31]),
                                    target = %format_hex(&target_bytes),
                                    check_result = ?check_result,
                                    observed_leading_zeros_bits = observed_leading_zeros,
                                    achieved_difficulty = %achieved_difficulty,
                                    achieved_difficulty_f64 = format_args!("{:.3e}", achieved_difficulty_f64),
                                    target_difficulty = %target_difficulty,
                                    target_difficulty_f64 = format_args!("{:.3e}", target_difficulty_f64),
                                    "BZM2 focused READRESULT trace"
                                );
                                if cfg.break_on_match {
                                    panic!(
                                        "BZM2 focused READRESULT breakpoint hit: engine_hw_id={:#x} logical_engine_id={} sequence={:#x} timecode={:#x} raw_nonce={:#010x} adjusted_nonce={:#010x}",
                                        engine_id, logical_engine_id, sequence, timecode, nonce_raw, nonce_adjusted
                                    );
                                }
                            }
                        }

                        if check_result == Bzm2CheckResult::Error
                            && observed_leading_zeros == 0
                            && zero_lz_diagnostic_samples < ZERO_LZ_DIAGNOSTIC_LIMIT
                        {
                            zero_lz_diagnostic_samples =
                                zero_lz_diagnostic_samples.saturating_add(1);
                            warn!(
                                asic_hw_id,
                                engine_hw_id = engine_id,
                                logical_engine_id,
                                sequence_raw = format_args!("{:#04x}", sequence),
                                sequence_effective = format_args!("{:#04x}", sequence_effective),
                                sequence_id,
                                matched_sequence_id = assigned.sequence_id,
                                micro_job_id,
                                timecode_raw = format_args!("{:#04x}", timecode),
                                timecode_effective = format_args!("{:#04x}", timecode_effective),
                                slot_candidate_count,
                                nonce_raw = format_args!("{:#010x}", nonce_raw),
                                nonce_adjusted = format_args!("{:#010x}", nonce_adjusted),
                                nonce_submit = format_args!("{:#010x}", nonce_submit),
                                nonce_minus_value = format_args!("{:#x}", assigned.nonce_minus_value),
                                ntime_offset,
                                ntime = format_args!("{:#010x}", share_ntime),
                                version = format_args!("{:#010x}", share_version.to_consensus() as u32),
                                observed_leading_zeros_bits = observed_leading_zeros,
                                required_leading_zeros_bits = assigned.leading_zeros,
                                hash_msb = format_args!("{:#04x}", hash_bytes[31]),
                                "BZM2 READRESULT valid-flag nonce reconstructed with zero leading zeros"
                            );
                        }

                        if check_result == Bzm2CheckResult::Correct {
                            sanity_candidates_meet_task =
                                sanity_candidates_meet_task.saturating_add(1);
                            let share = Share {
                                nonce: nonce_submit,
                                hash,
                                version: share_version,
                                ntime: share_ntime,
                                extranonce2: assigned.task.en2,
                                expected_work: assigned.task.share_target.to_work(),
                            };

                            if assigned.task.share_tx.send(share).await.is_ok() {
                                let mut s = status.write().expect("status lock poisoned");
                                s.chip_shares_found = s.chip_shares_found.saturating_add(1);
                            }

                            trace!(
                                asic_hw_id,
                                engine_hw_id = engine_id,
                                logical_engine_id,
                                sequence_id,
                                micro_job_id,
                                nonce = format_args!("{:#010x}", nonce_submit),
                                nonce_adjusted = format_args!("{:#010x}", nonce_adjusted),
                                sequence,
                                timecode,
                                ntime_offset,
                                expected_sequence_id = assigned.sequence_id,
                                nonce_minus_value = format_args!("{:#x}", assigned.nonce_minus_value),
                                observed_leading_zeros_bits = observed_leading_zeros,
                                achieved_difficulty = %achieved_difficulty,
                                achieved_difficulty_f64 = format_args!("{:.3e}", achieved_difficulty_f64),
                                target_difficulty = %target_difficulty,
                                target_difficulty_f64 = format_args!("{:.3e}", target_difficulty_f64),
                                "BZM2 candidate met task share target"
                            );
                        } else if check_result == Bzm2CheckResult::NotMeetTarget {
                            trace!(
                                asic_hw_id,
                                engine_hw_id = engine_id,
                                logical_engine_id,
                                sequence_id,
                                micro_job_id,
                                nonce = format_args!("{:#010x}", nonce_submit),
                                nonce_adjusted = format_args!("{:#010x}", nonce_adjusted),
                                sequence,
                                timecode,
                                ntime_offset,
                                expected_sequence_id = assigned.sequence_id,
                                nonce_minus_value = format_args!("{:#x}", assigned.nonce_minus_value),
                                observed_leading_zeros_bits = observed_leading_zeros,
                                achieved_difficulty = %achieved_difficulty,
                                achieved_difficulty_f64 = format_args!("{:.3e}", achieved_difficulty_f64),
                                target_difficulty = %target_difficulty,
                                target_difficulty_f64 = format_args!("{:.3e}", target_difficulty_f64),
                                "BZM2 nonce filtered by share target"
                            );
                        } else {
                            if sanity_diagnostic_samples < SANITY_DIAGNOSTIC_LIMIT {
                                sanity_diagnostic_samples = sanity_diagnostic_samples.saturating_add(1);

                                let header_bytes = consensus::serialize(&header);
                                let base_ntime = assigned.task.ntime;
                                let mut probes = Vec::new();
                                let focused = focused_readresult_diagnostic(
                                    &assigned,
                                    sequence,
                                    timecode,
                                    nonce_raw,
                                );

                                probes.push(format!(
                                    "current({})",
                                    validation_probe_summary(
                                        &assigned,
                                        share_version,
                                        share_ntime,
                                        nonce_submit,
                                    )
                                ));
                                probes.push(format!(
                                    "raw_nonce({})",
                                    validation_probe_summary(&assigned, share_version, share_ntime, nonce_raw)
                                ));
                                for gap in [0x14u32, 0x28, 0x4c, 0x98] {
                                    probes.push(format!(
                                        "gap_{gap:#x}({})",
                                        validation_probe_summary(
                                            &assigned,
                                            share_version,
                                            share_ntime,
                                            nonce_raw.wrapping_sub(gap).swap_bytes(),
                                        )
                                    ));
                                }
                                probes.push(format!(
                                    "time_base({})",
                                    validation_probe_summary(
                                        &assigned,
                                        share_version,
                                        base_ntime,
                                        nonce_submit,
                                    )
                                ));
                                probes.push(format!(
                                    "time_plus_tc({})",
                                    validation_probe_summary(
                                        &assigned,
                                        share_version,
                                        base_ntime.wrapping_add(u32::from(timecode)),
                                        nonce_submit,
                                    )
                                ));
                                probes.push(format!(
                                    "time_minus_tc({})",
                                    validation_probe_summary(
                                        &assigned,
                                        share_version,
                                        base_ntime.wrapping_sub(u32::from(timecode)),
                                        nonce_submit,
                                    )
                                ));
                                for (alt_idx, alt_version) in
                                    assigned.microjob_versions.iter().copied().enumerate()
                                {
                                    probes.push(format!(
                                        "ver_mj{alt_idx}({})",
                                        validation_probe_summary(
                                            &assigned,
                                            alt_version,
                                            share_ntime,
                                            nonce_submit,
                                        )
                                    ));
                                }

                                debug!(
                                    asic_hw_id,
                                    engine_id,
                                    sequence_id,
                                    micro_job_id,
                                    sequence,
                                    timecode,
                                    result_status,
                                    nonce_raw,
                                    nonce_adjusted = format_args!("{:#010x}", nonce_adjusted),
                                    nonce_submit = format_args!("{:#010x}", nonce_submit),
                                    assigned_sequence_id = assigned.sequence_id,
                                    assigned_timestamp_count = assigned.timestamp_count,
                                    assigned_nonce_minus = format_args!("{:#x}", assigned.nonce_minus_value),
                                    base_ntime = format_args!("{:#x}", base_ntime),
                                    selected_ntime = format_args!("{:#x}", share_ntime),
                                    selected_version = format_args!("{:#x}", share_version.to_consensus() as u32),
                                    bits = format_args!("{:#x}", assigned.task.template.bits.to_consensus()),
                                    header = %format_hex(&header_bytes),
                                    focused = %focused,
                                    probes = %probes.join(" | "),
                                    "BZM2 READRESULT sanity diagnostic"
                                );
                            }

                            trace!(
                                asic_hw_id,
                                engine_hw_id = engine_id,
                                logical_engine_id,
                                sequence_id,
                                micro_job_id,
                                nonce = format_args!("{:#010x}", nonce_submit),
                                nonce_adjusted = format_args!("{:#010x}", nonce_adjusted),
                                sequence,
                                timecode,
                                ntime_offset,
                                expected_sequence_id = assigned.sequence_id,
                                nonce_minus_value = format_args!("{:#x}", assigned.nonce_minus_value),
                                observed_leading_zeros_bits = observed_leading_zeros,
                                hash_msb = format_args!("{:#04x}", hash_bytes[31]),
                                "BZM2 nonce rejected by leading-zeros sanity check"
                            );
                        }

                        if sanity_candidates_total % 500 == 0 {
                            debug!(
                                total_candidates = sanity_candidates_total,
                                candidates_meeting_task_target = sanity_candidates_meet_task,
                                best_achieved_difficulty = %sanity_best_difficulty
                                    .expect("sanity_best_difficulty is set when total_candidates > 0"),
                                best_achieved_difficulty_f64 = format_args!("{:.3e}", sanity_best_difficulty
                                    .expect("sanity_best_difficulty is set when total_candidates > 0")
                                    .as_f64()),
                                current_target_difficulty = %target_difficulty,
                                current_target_difficulty_f64 = format_args!("{:.3e}", target_difficulty_f64),
                                "BZM2 candidate sanity summary"
                            );
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "Error reading BZM2 response stream");
                    }
                }
            }

            _ = status_ticker.tick() => {
                let snapshot = status.read().expect("status lock poisoned").clone();
                let _ = evt_tx.send(HashThreadEvent::StatusUpdate(snapshot)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bitcoin::{block::Header as BlockHeader, hashes::Hash as _};
    use bytes::BytesMut;
    use serde_json::json;
    use tokio::sync::mpsc;
    use tokio_util::codec::Decoder as _;

    use crate::{
        asic::hash_thread::HashTask,
        job_source::{
            Extranonce2, Extranonce2Range, GeneralPurposeBits, JobTemplate, MerkleRootKind,
            MerkleRootTemplate, VersionTemplate,
        },
        stratum_v1::JobNotification,
        types::Difficulty,
    };

    use super::{
        AssignedTask, BZM2_NONCE_MINUS, Bzm2CheckResult, ENGINE_LEADING_ZEROS,
        ENGINE_TIMESTAMP_COUNT, EngineAssignment, MIDSTATE_COUNT, WORK_ENGINE_COUNT,
        bzm2_double_sha_from_midstate_and_tail, bzm2_tail16_bytes, midstate_version_mask_variants,
        check_result, hash_bytes_bzm2_order, protocol, resolve_readresult_fields,
        task_to_bzm2_payload, task_midstate_versions,
    };

    #[test]
    fn test_midstate_version_mask_variants_for_full_mask() {
        assert_eq!(
            midstate_version_mask_variants(0x1fff_e000),
            [0x0000_0000, 0x0000_e000, 0x1fff_0000, 0x1fff_e000]
        );
    }

    #[test]
    fn test_midstate_version_mask_variants_for_zero_mask() {
        assert_eq!(midstate_version_mask_variants(0), [0, 0, 0, 0]);
    }

    #[test]
    fn test_resolve_readresult_fields_prefers_raw_when_slot_exists() {
        let active_slots = [32u8, 0u8];
        let fields = resolve_readresult_fields(0x80, 0xbc, |slot| active_slots.contains(&slot))
            .expect("raw slot should resolve");
        assert_eq!(fields.sequence, 0x80);
        assert_eq!(fields.timecode, 0xbc);
        assert_eq!(fields.sequence_id, 32);
        assert_eq!(fields.micro_job_id, 0);
        assert!(!fields.used_masked_fields);
    }

    #[test]
    fn test_resolve_readresult_fields_uses_masked_fallback() {
        let active_slots = [0u8];
        let fields = resolve_readresult_fields(0x82, 0xbc, |slot| active_slots.contains(&slot))
            .expect("masked slot should resolve");
        assert_eq!(fields.sequence, 0x02);
        assert_eq!(fields.timecode, 0x3c);
        assert_eq!(fields.sequence_id, 0);
        assert_eq!(fields.micro_job_id, 2);
        assert!(fields.used_masked_fields);
    }

    #[test]
    fn test_resolve_readresult_fields_none_when_no_slot_matches() {
        let active_slots = [0u8];
        let fields = resolve_readresult_fields(0xfd, 0x7f, |slot| active_slots.contains(&slot));
        assert!(fields.is_none());
    }

    #[test]
    fn test_check_result_leading_zeros_error() {
        let mut hash = [0u8; 32];
        let target = [0xffu8; 32];
        hash[31] = 0x80;
        assert_eq!(
            check_result(&hash, &target, 32),
            Bzm2CheckResult::Error
        );
    }

    #[test]
    fn test_check_result_accepts_required_leading_zeros() {
        let mut hash = [0u8; 32];
        let target = [0xffu8; 32];
        hash[27] = 0x3f;
        assert_eq!(
            check_result(&hash, &target, 34),
            Bzm2CheckResult::Correct
        );
    }

    #[test]
    fn test_check_result_rejects_missing_partial_zero_bits() {
        let mut hash = [0u8; 32];
        let target = [0xffu8; 32];
        hash[27] = 0x40;
        assert_eq!(
            check_result(&hash, &target, 34),
            Bzm2CheckResult::Error
        );
    }

    #[test]
    fn test_check_result_target_compare() {
        let mut hash = [0u8; 32];
        let mut target = [0u8; 32];

        hash[1] = 0x10;
        target[1] = 0x20;
        assert_eq!(
            check_result(&hash, &target, 32),
            Bzm2CheckResult::Correct
        );

        hash[1] = 0x30;
        target[1] = 0x20;
        assert_eq!(
            check_result(&hash, &target, 32),
            Bzm2CheckResult::NotMeetTarget
        );
    }

    #[test]
    fn test_hash_bytes_bzm2_order_keeps_digest_order() {
        let src = core::array::from_fn(|i| i as u8);
        let hash = bitcoin::BlockHash::from_byte_array(src);
        assert_eq!(hash_bytes_bzm2_order(&hash), src);
    }

    #[test]
    fn test_bzm2_double_sha_matches_known_trace_sample() {
        // Captured from birds-bzm2 valid-share-hash-input logging.
        let midstate =
            hex::decode("07348faef527b8ec3733171cb0781bc545efb4220d71e0a5b54af23de2106bfd")
                .expect("midstate hex should parse");
        let tail16 =
            hex::decode("ef70e3ac38979a6903f301176467a52b").expect("tail16 hex should parse");
        let expected_double_sha =
            hex::decode("25ef6a2327c5304bd263126a6a38ad16c3b27cd8b647085624a7130000000000")
                .expect("double sha hex should parse");
        let midstate: [u8; 32] = midstate.try_into().expect("midstate must be 32 bytes");
        let tail16: [u8; 16] = tail16.try_into().expect("tail16 must be 16 bytes");
        let expected_double_sha: [u8; 32] = expected_double_sha
            .try_into()
            .expect("double sha must be 32 bytes");
        assert_eq!(
            bzm2_double_sha_from_midstate_and_tail(&midstate, &tail16),
            expected_double_sha
        );
    }

    #[test]
    fn test_readresult_hash_check_with_known_good_bzm2_share() {
        // Job + accepted share captured from known working messages
        // - notify: job_id=18965aa3c6b2c4cf, ntime=0x699a9733, version mask 0x1fffe000
        // - accepted submit: en2=7200000000000000, ntime=699a9735, nonce=1c1a2bff, vmask=1fff0000
        let notify_params = json!([
            "18965aa3c6b2c4cf",
            "fe207277906478ce38c2ea1089c75d1da29c36ff0000a8a70000000000000000",
            "02000000010000000000000000000000000000000000000000000000000000000000000000ffffffff2c03304f0e01000438979a69041dd270030c",
            "0e6879647261706f6f6c2f32353666ffffffff02e575a31200000000160014c64b1b9283ba1ea86bb9e7b696b0c8f68dad04000000000000000000266a24aa21a9ed413814acda23cadaad2f189d0dd7794ab6892d1eaad4b1a1433156a31ccb62a800000000",
            [
                "be51038f82c6f95e407ff56a88a85e179935927e20ec26994e453c858c52b2d5",
                "41f1b3ef96540488c96e6a53ca5156541082ab6d670e87069d84ca600fe32323",
                "ef3b47f15c4e98960b53cbd23c6bc6ce29ffcfa6d5c23b869db0a8e5699e7b0d",
                "48653d2575674cfd6417dee08bafd2de5246ff615c8b3af9829d21d972ad4e73",
                "2013f4b7781327c760228203e073a252ed48547770c7033fb283e521dbf062d2",
                "a83041e9c9bdc76e5fe2be707c6b114d6f33a4e42632fe8d79f1015e1a0c8caf",
                "8f136aca72f1f36a1e7ac1a40b3a2dd0cf7fc8e36be6a8c1f520933b1511cdf0",
                "93dc2365dce4dece9d317654715c0a7bcfa6a175afba9693199dd0dacb9bab15",
                "3f11ffc73e9f01af072a495c47b03bec824eeab3fc7e92e1f52907d16516764d"
            ],
            "20000000",
            "1701f303",
            "699a9733",
            true
        ]);
        let job = JobNotification::from_stratum_params(
            notify_params
                .as_array()
                .expect("notify_params must be an array"),
        )
        .expect("notify params should parse");

        let en2_size = 8u8;
        let en2_bytes = hex::decode("7200000000000000").expect("en2 hex should parse");
        let en2_value =
            u64::from_le_bytes(en2_bytes.as_slice().try_into().expect("en2 size must be 8"));
        let en2 = Extranonce2::new(en2_value, en2_size).expect("en2 should construct");

        let template = Arc::new(JobTemplate {
            id: job.job_id,
            prev_blockhash: job.prev_hash,
            version: VersionTemplate::new(
                job.version,
                GeneralPurposeBits::from(&0x1fffe000u32.to_be_bytes()),
            )
            .expect("version template should construct"),
            bits: job.nbits,
            share_target: Difficulty::from(1000u64).to_target(),
            time: job.ntime,
            merkle_root: MerkleRootKind::Computed(MerkleRootTemplate {
                coinbase1: job.coinbase1,
                extranonce1: hex::decode("e1a253ac").expect("extranonce1 hex should parse"),
                extranonce2_range: Extranonce2Range::new(en2_size)
                    .expect("en2 range should construct"),
                coinbase2: job.coinbase2,
                merkle_branches: job.merkle_branches,
            }),
        });

        let (share_tx, _share_rx) = mpsc::channel(1);
        let task = HashTask {
            template: Arc::clone(&template),
            en2_range: Some(Extranonce2Range::new(en2_size).expect("en2 range should construct")),
            en2: Some(en2),
            share_target: Difficulty::from(1000u64).to_target(),
            ntime: template.time,
            share_tx,
        };

        let merkle_root = template
            .compute_merkle_root(&en2)
            .expect("merkle root should compute");
        let microjob_versions = task_midstate_versions(&task);
        let payload = task_to_bzm2_payload(&task, merkle_root, microjob_versions)
            .expect("payload should derive");
        let engine_assignments = vec![
            EngineAssignment {
                merkle_root,
                extranonce2: task.en2,
                midstates: payload.midstates,
            };
            WORK_ENGINE_COUNT
        ];
        let assigned = AssignedTask {
            task,
            merkle_root,
            engine_assignments: Arc::from(engine_assignments.into_boxed_slice()),
            microjob_versions,
            sequence_id: 0,
            timestamp_count: ENGINE_TIMESTAMP_COUNT,
            leading_zeros: ENGINE_LEADING_ZEROS,
            nonce_minus_value: BZM2_NONCE_MINUS,
        };

        // Reconstruct an on-wire READRESULT frame for the accepted share:
        // status=0x8 (valid), engine_id=0x001, sequence=2 (micro-job 2), timecode=0x3a.
        // READRESULT adjusted nonce is byte-swapped before Stratum submit.
        let expected_nonce_submit = 0x1c1a_2bffu32;
        let expected_nonce_adjusted = expected_nonce_submit.swap_bytes();
        let expected_ntime = 0x699a_9735u32;
        let expected_version = 0x3fff_0000u32;
        let ntime_delta = expected_ntime.wrapping_sub(assigned.task.ntime);
        assert_eq!(
            ntime_delta, 2,
            "test fixture ntime delta must match capture"
        );

        let raw_nonce = expected_nonce_adjusted.wrapping_add(BZM2_NONCE_MINUS);
        let raw_frame = [
            0x0a,
            protocol::Opcode::ReadResult as u8,
            0x80,
            0x01,
            (raw_nonce & 0xff) as u8,
            ((raw_nonce >> 8) & 0xff) as u8,
            ((raw_nonce >> 16) & 0xff) as u8,
            ((raw_nonce >> 24) & 0xff) as u8,
            0x02,
            ENGINE_TIMESTAMP_COUNT.wrapping_sub(ntime_delta as u8),
        ];

        let mut codec = protocol::FrameCodec::default();
        let mut src = BytesMut::from(&raw_frame[..]);
        let response = codec
            .decode(&mut src)
            .expect("decode should succeed")
            .expect("frame should decode");

        let protocol::Response::ReadResult {
            engine_id,
            status,
            nonce: nonce_raw,
            sequence,
            timecode,
            ..
        } = response
        else {
            panic!("expected READRESULT response");
        };
        assert_eq!(engine_id, 0x001);
        assert_eq!(status, 0x8);

        let sequence_id = sequence / (MIDSTATE_COUNT as u8);
        let micro_job_id = sequence % (MIDSTATE_COUNT as u8);
        assert_eq!(sequence_id, assigned.sequence_id);

        let share_version = assigned.microjob_versions[micro_job_id as usize];
        let ntime_offset = u32::from(assigned.timestamp_count.wrapping_sub(timecode));
        let share_ntime = assigned.task.ntime.wrapping_add(ntime_offset);
        let nonce_adjusted = nonce_raw.wrapping_sub(assigned.nonce_minus_value);
        let nonce_submit = nonce_adjusted.swap_bytes();

        assert_eq!(share_version.to_consensus() as u32, expected_version);
        assert_eq!(share_ntime, expected_ntime);
        assert_eq!(nonce_adjusted, expected_nonce_adjusted);
        assert_eq!(nonce_submit, expected_nonce_submit);

        let header = BlockHeader {
            version: share_version,
            prev_blockhash: assigned.task.template.prev_blockhash,
            merkle_root: assigned.merkle_root,
            time: share_ntime,
            bits: assigned.task.template.bits,
            nonce: nonce_submit,
        };
        let hash = header.block_hash();
        let hash_bytes = hash_bytes_bzm2_order(&hash);
        let tail16 = bzm2_tail16_bytes(&assigned, share_ntime, nonce_submit);
        let bzm2_hash_bytes = bzm2_double_sha_from_midstate_and_tail(
            &assigned.engine_assignments[0].midstates[micro_job_id as usize],
            &tail16,
        );
        let target_bytes = assigned.task.share_target.to_le_bytes();

        assert_eq!(hash_bytes, bzm2_hash_bytes);
        assert_eq!(
            check_result(&hash_bytes, &target_bytes, assigned.leading_zeros),
            Bzm2CheckResult::Correct
        );
        assert!(assigned.task.share_target.is_met_by(hash));
    }
}
