//! BZM2 HashThread implementation.
//!
//! This module mirrors the BM13xx actor model and performs full BZM2 bring-up
//! before the first task is accepted.

use std::{
    io,
    sync::{Arc, RwLock},
};

use async_trait::async_trait;
use bitcoin::{
    TxMerkleNode,
    block::{Header as BlockHeader, Version as BlockVersion},
    consensus,
    hashes::{HashEngine as _, sha256},
};
use futures::{SinkExt, sink::Sink, stream::Stream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{self, Duration, Instant};
use tokio_stream::StreamExt;

use super::protocol;
use crate::{
    asic::hash_thread::{
        BoardPeripherals, HashTask, HashThread, HashThreadCapabilities, HashThreadError,
        HashThreadEvent, HashThreadStatus, ThreadRemovalSignal,
    },
    job_source::{GeneralPurposeBits, MerkleRootKind},
    tracing::prelude::*,
    types::HashRate,
};

const FIRST_ASIC_ID: u8 = 0x0a;
const ENGINE_ROWS: u16 = 20;
const ENGINE_COLS: u16 = 10;

const SENSOR_REPORT_INTERVAL: u32 = 63;
const THERMAL_TRIP_C: f32 = 115.0;
const VOLTAGE_TRIP_MV: f32 = 500.0;

const PLL_LOCK_MASK: u32 = 0x4;
const REF_CLK_MHZ: f32 = 50.0;
const REF_DIVIDER: u32 = 2;
const POST2_DIVIDER: u32 = 1;
const POST1_DIVIDER: u8 = 1;
const TARGET_FREQ_MHZ: f32 = 800.0;

const DRIVE_STRENGTH_STRONG: u32 = 0x4448_4444;
const ENGINE_CONFIG_ENHANCED_MODE_BIT: u8 = 1 << 2;

const INIT_NOOP_TIMEOUT: Duration = Duration::from_millis(500);
const INIT_READREG_TIMEOUT: Duration = Duration::from_millis(500);
const PLL_LOCK_TIMEOUT: Duration = Duration::from_secs(3);
const PLL_POLL_DELAY: Duration = Duration::from_millis(100);
const SOFT_RESET_DELAY: Duration = Duration::from_millis(1);
const MIDSTATE_COUNT: usize = 4;
const WRITEJOB_CTL_REPLACE: u8 = 3;

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
                hashrate_estimate: HashRate::default(),
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

struct TaskJobPayload {
    midstates: [[u8; 32]; MIDSTATE_COUNT],
    merkle_residue: u32,
    timestamp: u32,
}

fn expand_counter_into_mask(mask: u16, mut counter: u16) -> u16 {
    let mut rolled = 0u16;
    for bit in 0..16 {
        let bit_mask = 1u16 << bit;
        if (mask & bit_mask) != 0 {
            if (counter & 1) != 0 {
                rolled |= bit_mask;
            }
            counter >>= 1;
        }
    }
    rolled
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
    let mut engine = sha256::HashEngine::default();
    engine.input(header_prefix_64);
    let mut midstate = engine.midstate().to_byte_array();
    for word in midstate.chunks_exact_mut(4) {
        // Firmware expects each state word little-endian on the wire.
        word.reverse();
    }
    midstate
}

fn task_to_bzm2_payload(
    task: &HashTask,
    version_counter: u16,
) -> Result<TaskJobPayload, HashThreadError> {
    let template = task.template.as_ref();
    let merkle_root = compute_task_merkle_root(task)?;
    let base_version = template.version.base();
    let version_mask = u16::from_be_bytes(*template.version.gp_bits_mask().as_bytes());

    let mut midstates = [[0u8; 32]; MIDSTATE_COUNT];
    let mut merkle_residue = 0u32;
    let mut timestamp = 0u32;

    for (idx, midstate) in midstates.iter_mut().enumerate() {
        let rolled_bits_u16 =
            expand_counter_into_mask(version_mask, version_counter.wrapping_add(idx as u16));
        let rolled_bits = GeneralPurposeBits::new(rolled_bits_u16.to_be_bytes());
        let rolled_version = rolled_bits.apply_to_version(base_version);

        let header = build_header_bytes(task, rolled_version, merkle_root)?;
        let header_prefix: [u8; 64] = header[..64]
            .try_into()
            .expect("header prefix length is fixed");

        *midstate = compute_midstate_le(&header_prefix);

        if idx == 0 {
            merkle_residue = u32::from_le_bytes(
                header[64..68]
                    .try_into()
                    .expect("slice length is exactly 4 bytes"),
            );
            timestamp = u32::from_le_bytes(
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
    version_counter: u16,
    sequence_id: u8,
) -> Result<(), HashThreadError>
where
    W: Sink<protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    let payload = task_to_bzm2_payload(task, version_counter)?;

    for col in 0..ENGINE_COLS {
        for row in 0..ENGINE_ROWS {
            let engine = engine_id(row, col);
            let commands = protocol::Command::write_job_enhanced(
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
        }
    }

    Ok(())
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

async fn start_warm_up_jobs<W>(chip_commands: &mut W, asic_id: u8) -> Result<(), HashThreadError>
where
    W: Sink<protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    for col in 0..ENGINE_COLS {
        for row in 0..ENGINE_ROWS {
            let engine = engine_id(row, col);
            for _ in 0..2 {
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
        let asic_id = FIRST_ASIC_ID
            .checked_add(index)
            .ok_or_else(|| init_failed("ASIC ID overflow while programming chain IDs"))?;

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
    let mut next_sequence_id: u8 = 0;
    let mut next_version_counter: u16 = 0;
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

                        if let Err(e) = send_task_to_all_engines(
                            &mut chip_commands,
                            &new_task,
                            next_version_counter,
                            next_sequence_id,
                        )
                        .await
                        {
                            error!(error = %e, "Failed to send BZM2 work during update_task");
                            let _ = response_tx.send(Err(e));
                            continue;
                        }

                        debug!(
                            job_id = %new_task.template.id,
                            sequence_id = next_sequence_id,
                            version_counter = next_version_counter,
                            "Sent BZM2 WRITEJOB payloads for update_task"
                        );
                        next_sequence_id = next_sequence_id.wrapping_add(1);
                        next_version_counter =
                            next_version_counter.wrapping_add(MIDSTATE_COUNT as u16);

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

                        if let Err(e) = send_task_to_all_engines(
                            &mut chip_commands,
                            &new_task,
                            next_version_counter,
                            next_sequence_id,
                        )
                        .await
                        {
                            error!(error = %e, "Failed to send BZM2 work during replace_task");
                            let _ = response_tx.send(Err(e));
                            continue;
                        }

                        debug!(
                            job_id = %new_task.template.id,
                            sequence_id = next_sequence_id,
                            version_counter = next_version_counter,
                            "Sent BZM2 WRITEJOB payloads for replace_task"
                        );
                        next_sequence_id = next_sequence_id.wrapping_add(1);
                        next_version_counter =
                            next_version_counter.wrapping_add(MIDSTATE_COUNT as u16);

                        let old_task = current_task.replace(new_task);
                        {
                            let mut s = status.write().expect("status lock poisoned");
                            s.is_active = true;
                        }
                        let _ = response_tx.send(Ok(old_task));
                    }
                    ThreadCommand::GoIdle { response_tx } => {
                        let old_task = current_task.take();
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
    use super::expand_counter_into_mask;

    #[test]
    fn test_expand_counter_into_contiguous_mask() {
        assert_eq!(expand_counter_into_mask(0b0011, 0), 0b0000);
        assert_eq!(expand_counter_into_mask(0b0011, 1), 0b0001);
        assert_eq!(expand_counter_into_mask(0b0011, 2), 0b0010);
        assert_eq!(expand_counter_into_mask(0b0011, 3), 0b0011);
    }

    #[test]
    fn test_expand_counter_into_sparse_mask() {
        assert_eq!(expand_counter_into_mask(0b1010, 0), 0b0000);
        assert_eq!(expand_counter_into_mask(0b1010, 1), 0b0010);
        assert_eq!(expand_counter_into_mask(0b1010, 2), 0b1000);
        assert_eq!(expand_counter_into_mask(0b1010, 3), 0b1010);
    }
}
