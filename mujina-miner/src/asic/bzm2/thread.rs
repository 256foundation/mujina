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
    io,
    sync::{Arc, RwLock},
};

use async_trait::async_trait;
use futures::{sink::Sink, stream::Stream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{self, Duration};
use tokio_stream::StreamExt;

use super::protocol;
use crate::{
    asic::hash_thread::{
        BoardPeripherals, HashTask, HashThread, HashThreadCapabilities, HashThreadError,
        HashThreadEvent, HashThreadStatus, Share, ThreadRemovalSignal,
    },
    tracing::prelude::*,
    types::{Difficulty, HashRate},
};
use bringup::{initialize_chip, write_reg_u8, write_reg_u32};
use hashing::{Bzm2CheckResult, task_midstate_versions};
#[cfg(test)]
use hashing::{
    bzm2_double_sha_from_midstate_and_tail, bzm2_tail16_bytes, check_result, hash_bytes_bzm2_order,
};
use tracker::{AssignmentTracker, SelectedReadResultCandidate};
use work::{AssignedTask, is_invalid_engine, logical_engine_index, send_task_to_all_engines};
#[cfg(test)]
use work::{EngineAssignment, task_to_bzm2_payload};

mod bringup;
mod hashing;
mod tracker;
mod work;

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

#[derive(Clone, Copy)]
enum TaskAssignmentMode {
    Update,
    Replace,
}

impl TaskAssignmentMode {
    fn transition_message(self, had_old_task: bool) -> &'static str {
        match (self, had_old_task) {
            (Self::Update, true) => "Updating work",
            (Self::Update, false) => "Updating work from idle",
            (Self::Replace, true) => "Replacing work",
            (Self::Replace, false) => "Replacing work from idle",
        }
    }

    fn send_failure_context(self) -> &'static str {
        match self {
            Self::Update => "update_task",
            Self::Replace => "replace_task",
        }
    }

    fn sent_message(self) -> &'static str {
        match self {
            Self::Update => "Sent BZM2 work to chip",
            Self::Replace => "Sent BZM2 work to chip (old work invalidated)",
        }
    }
}

async fn assign_task<R, W>(
    chip_responses: &mut R,
    chip_commands: &mut W,
    peripherals: &mut BoardPeripherals,
    asic_count: u8,
    chip_initialized: &mut bool,
    current_task: &mut Option<HashTask>,
    assignment_tracker: &mut AssignmentTracker,
    status: &Arc<RwLock<HashThreadStatus>>,
    new_task: HashTask,
    mode: TaskAssignmentMode,
) -> Result<Option<HashTask>, HashThreadError>
where
    R: Stream<Item = Result<protocol::Response, io::Error>> + Unpin,
    W: Sink<protocol::Command> + Unpin,
    W::Error: std::fmt::Debug,
{
    if let Some(old) = current_task.as_ref() {
        debug!(
            old_job = %old.template.id,
            new_job = %new_task.template.id,
            "{}",
            mode.transition_message(true)
        );
    } else {
        debug!(
            new_job = %new_task.template.id,
            "{}",
            mode.transition_message(false)
        );
    }

    if !*chip_initialized {
        match initialize_chip(chip_responses, chip_commands, peripherals, asic_count).await {
            Ok(ids) => {
                *chip_initialized = true;
                info!(asic_ids = ?ids, "BZM2 initialization completed");
            }
            Err(e) => {
                error!(error = %e, "BZM2 chip initialization failed");
                return Err(e);
            }
        }
    }

    let microjob_versions = task_midstate_versions(&new_task);
    let write_sequence_id = assignment_tracker.current_write_sequence_id();

    let engine_assignments = send_task_to_all_engines(
        chip_commands,
        &new_task,
        microjob_versions,
        write_sequence_id,
        ENGINE_ZEROS_TO_FIND,
        ENGINE_TIMESTAMP_COUNT,
    )
    .await
    .map_err(|e| {
        error!(
            error = %e,
            command = mode.send_failure_context(),
            "Failed to send BZM2 work"
        );
        e
    })?;

    let Some(default_assignment) = engine_assignments.first().cloned() else {
        let e = HashThreadError::WorkAssignmentFailed(format!(
            "no engine assignments produced for {}",
            mode.send_failure_context()
        ));
        error!(
            error = %e,
            command = mode.send_failure_context(),
            "Failed to send BZM2 work"
        );
        return Err(e);
    };

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
    assignment_tracker.retain(new_assigned_task);

    debug!(
        job_id = %new_task.template.id,
        write_sequence_id,
        "{}",
        mode.sent_message()
    );
    assignment_tracker.advance_sequence();

    let old_task = current_task.replace(new_task);
    {
        let mut s = status.write().expect("status lock poisoned");
        s.is_active = true;
    }

    Ok(old_task)
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
    let mut assignment_tracker = AssignmentTracker::new();
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
                        let result = assign_task(
                            &mut chip_responses,
                            &mut chip_commands,
                            &mut peripherals,
                            asic_count,
                            &mut chip_initialized,
                            &mut current_task,
                            &mut assignment_tracker,
                            &status,
                            new_task,
                            TaskAssignmentMode::Update,
                        )
                        .await;
                        let _ = response_tx.send(result);
                    }
                    ThreadCommand::ReplaceTask { new_task, response_tx } => {
                        let result = assign_task(
                            &mut chip_responses,
                            &mut chip_commands,
                            &mut peripherals,
                            asic_count,
                            &mut chip_initialized,
                            &mut current_task,
                            &mut assignment_tracker,
                            &status,
                            new_task,
                            TaskAssignmentMode::Replace,
                        )
                        .await;
                        let _ = response_tx.send(result);
                    }
                    ThreadCommand::GoIdle { response_tx } => {
                        debug!("Going idle");

                        let old_task = current_task.take();
                        assignment_tracker.clear();
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

                        let nonce_raw = nonce;
                        let Some(SelectedReadResultCandidate {
                            assigned,
                            sequence_id,
                            micro_job_id,
                            timecode_effective,
                            slot_candidate_count,
                            share_version,
                            ntime_offset,
                            share_ntime,
                            nonce_adjusted,
                            nonce_submit,
                            hash_bytes,
                            hash,
                            check_result,
                            observed_leading_zeros,
                        }) = assignment_tracker
                            .resolve_candidate(logical_engine_id, sequence, timecode, nonce_raw)
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
        hash_bytes_bzm2_order, protocol, task_midstate_versions, task_to_bzm2_payload,
    };

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
                midstates: payload,
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
