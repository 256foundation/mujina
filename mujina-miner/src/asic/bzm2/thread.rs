//! BZM2 HashThread implementation.
//!
//! This module uses an actor-style `HashThread` implementation and performs
//! full BZM2 bring-up before the first task is accepted.
//!
//! A `Bzm2Thread` represents the hashing worker for one BIRDS board data path.
//! It is responsible for:
//! - asserting and releasing ASIC reset through board-provided peripherals
//! - programming the chip register set needed for mining
//! - translating scheduler work into BZM2 micro-jobs
//! - validating returned results before forwarding shares upstream

use std::{
    collections::VecDeque,
    io,
    sync::{Arc, RwLock},
};

use async_trait::async_trait;
use bitcoin::{TxMerkleNode, block::Version as BlockVersion, hashes::Hash as _};
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
#[cfg(test)]
use hashing::hash_bytes_bzm2_order;
use hashing::{
    Bzm2CheckResult, build_header_bytes, bzm2_double_sha_from_midstate_and_tail, bzm2_tail16_bytes,
    check_result, compute_midstate_le, leading_zero_bits, task_midstate_versions,
};

mod hashing;

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
const ZERO_LZ_DIAGNOSTIC_LIMIT: u64 = 24;

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

/// `HashThread` wrapper for a BZM2 board worker.
///
/// This is a thin handle around a spawned actor task. The actor owns the
/// serial transport and emits [`HashThreadEvent`] updates as it initializes
/// the ASICs and processes work.
pub struct Bzm2Thread {
    name: String,
    command_tx: mpsc::Sender<ThreadCommand>,
    event_rx: Option<mpsc::Receiver<HashThreadEvent>>,
    capabilities: HashThreadCapabilities,
    status: Arc<RwLock<HashThreadStatus>>,
}

impl Bzm2Thread {
    /// Create a new BZM2 hashing worker.
    ///
    /// The thread starts in an uninitialized state. Hardware bring-up happens
    /// lazily when the first task is assigned so board discovery can complete
    /// without immediately programming the ASICs.
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
            bzm2_thread_actor(Bzm2ThreadActor {
                cmd_rx,
                evt_tx,
                removal_rx,
                status: status_clone,
                chip_responses,
                chip_commands,
                peripherals,
                asic_count,
            })
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
    while let Ok(Some(_)) = time::timeout(Duration::from_millis(20), chip_responses.next()).await {}
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

struct SelectedReadResultCandidate {
    assigned: AssignedTask,
    share_version: BlockVersion,
    ntime_offset: u32,
    share_ntime: u32,
    nonce_adjusted: u32,
    nonce_submit: u32,
    hash_bytes: [u8; 32],
    hash: bitcoin::BlockHash,
    check_result: Bzm2CheckResult,
    observed_leading_zeros: u16,
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

async fn set_asic_nonce_range<W>(chip_commands: &mut W, asic_id: u8) -> Result<(), HashThreadError>
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

struct Bzm2ThreadActor<R, W> {
    cmd_rx: mpsc::Receiver<ThreadCommand>,
    evt_tx: mpsc::Sender<HashThreadEvent>,
    removal_rx: watch::Receiver<ThreadRemovalSignal>,
    status: Arc<RwLock<HashThreadStatus>>,
    chip_responses: R,
    chip_commands: W,
    peripherals: BoardPeripherals,
    asic_count: u8,
}

async fn bzm2_thread_actor<R, W>(actor: Bzm2ThreadActor<R, W>)
where
    R: Stream<Item = Result<protocol::Response, io::Error>> + Unpin,
    W: Sink<protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    let Bzm2ThreadActor {
        mut cmd_rx,
        evt_tx,
        mut removal_rx,
        status,
        mut chip_responses,
        mut chip_commands,
        mut peripherals,
        asic_count,
    } = actor;

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
    let mut zero_lz_diagnostic_samples: u64 = 0;
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
                        if let Some(ref old) = current_task {
                            debug!(
                                old_job = %old.template.id,
                                new_job = %new_task.template.id,
                                "Updating work"
                            );
                        } else {
                            debug!(new_job = %new_task.template.id, "Updating work from idle");
                        }

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

                        debug!(
                            job_id = %new_task.template.id,
                            write_sequence_id,
                            "Sent BZM2 work to chip"
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
                        if let Some(ref old) = current_task {
                            debug!(
                                old_job = %old.template.id,
                                new_job = %new_task.template.id,
                                "Replacing work"
                            );
                        } else {
                            debug!(new_job = %new_task.template.id, "Replacing work from idle");
                        }

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

                        debug!(
                            job_id = %new_task.template.id,
                            write_sequence_id,
                            "Sent BZM2 work to chip (old work invalidated)"
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
                        debug!("Going idle");

                        let old_task = current_task.take();
                        assigned_tasks.clear();
                        {
                            let mut s = status.write().expect("status lock poisoned");
                            s.is_active = false;
                        }
                        let _ = response_tx.send(Ok(old_task));
                    }
                    ThreadCommand::Shutdown => {
                        info!("Shutdown command received");
                        break;
                    }
                }
            }

            Some(result) = chip_responses.next() => {
                match result {
                    Ok(protocol::Response::Noop { .. }) => {}
                    Ok(protocol::Response::ReadReg { .. }) => {}
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
                            continue;
                        }

                        let row = engine_id & 0x3f;
                        let column = engine_id >> 6;
                        if row >= ENGINE_ROWS || column >= ENGINE_COLS {
                            continue;
                        }
                        if is_invalid_engine(row, column) {
                            continue;
                        }
                        let Some(logical_engine_id) = logical_engine_index(row, column) else {
                            continue;
                        };

                        let Some(resolved_fields) =
                            resolve_readresult_fields(sequence, timecode, |slot| {
                                assigned_tasks.iter().rev().any(|task| {
                                    readresult_sequence_slot(task.sequence_id) == slot
                                })
                            })
                        else {
                            continue;
                        };
                        let sequence_id = resolved_fields.sequence_id;
                        let micro_job_id = resolved_fields.micro_job_id;
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
                            continue;
                        }

                        let nonce_raw = nonce;
                        let mut selected_candidate: Option<SelectedReadResultCandidate> = None;
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
                            let observed_leading_zeros = leading_zero_bits(&hash_bytes);
                            let rank = match check_result {
                                Bzm2CheckResult::Correct => 3,
                                Bzm2CheckResult::NotMeetTarget => 2,
                                Bzm2CheckResult::Error => 1,
                            };

                            if selected_candidate.is_none() || rank > selected_rank {
                                selected_rank = rank;
                                selected_candidate = Some(SelectedReadResultCandidate {
                                    assigned: candidate,
                                    share_version,
                                    ntime_offset,
                                    share_ntime,
                                    nonce_adjusted,
                                    nonce_submit,
                                    hash_bytes,
                                    hash,
                                    check_result,
                                    observed_leading_zeros,
                                });
                                if rank == 3 {
                                    break;
                                }
                            }
                        }

                        let Some(SelectedReadResultCandidate {
                            assigned,
                            share_version,
                            ntime_offset,
                            share_ntime,
                            nonce_adjusted,
                            nonce_submit,
                            hash_bytes,
                            hash,
                            check_result,
                            observed_leading_zeros,
                        }) = selected_candidate
                        else {
                            continue;
                        };

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
                                sequence_id,
                                matched_sequence_id = assigned.sequence_id,
                                micro_job_id,
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
                            let share = Share {
                                nonce: nonce_submit,
                                hash,
                                version: share_version,
                                ntime: share_ntime,
                                extranonce2: assigned.task.en2,
                                expected_work: assigned.task.share_target.to_work(),
                            };

                            if assigned.task.share_tx.send(share).await.is_err() {
                                debug!("Share channel closed (task replaced)");
                            } else {
                                let mut s = status.write().expect("status lock poisoned");
                                s.chip_shares_found = s.chip_shares_found.saturating_add(1);
                            }
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

    debug!("BZM2 thread actor exiting");
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bitcoin::block::Header as BlockHeader;
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
        bzm2_double_sha_from_midstate_and_tail, bzm2_tail16_bytes, check_result,
        hash_bytes_bzm2_order, protocol, resolve_readresult_fields, task_midstate_versions,
        task_to_bzm2_payload,
    };

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
