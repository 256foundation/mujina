use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use bitcoin::block::Header as BlockHeader;
use bitcoin::consensus::serialize;
use bitcoin::hashes::{HashEngine, sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

use crate::asic::hash_thread::{
    HashTask, HashThread, HashThreadCapabilities, HashThreadError, HashThreadEvent,
    HashThreadPowerReading, HashThreadStatus, HashThreadTelemetryUpdate,
    HashThreadTemperatureReading, Share,
};
use crate::job_source::{GeneralPurposeBits, MerkleRootKind};
use crate::tracing::prelude::*;
use crate::transport::serial::{SerialControl, SerialReader, SerialWriter};
use crate::types::{Difficulty, HashRate};

use super::protocol::{
    self, BROADCAST_ASIC, DEFAULT_NONCE_GAP, DEFAULT_TIMESTAMP_COUNT, DtsVsGeneration,
    ENGINE_REG_TARGET, ENGINE_REG_TIMESTAMP_COUNT, ENGINE_REG_ZEROS_TO_FIND, TdmDtsVsFrame,
    TdmFrame, TdmFrameParser, default_engine_coordinates, encode_write_job, encode_write_register,
    leading_zero_threshold, logical_engine_address,
};
use super::uart::{
    Bzm2DiscoveredEngineMap, Bzm2DtsVsConfig, DEFAULT_DTS_VS_QUERY_TIMEOUT,
    configure_dts_vs_stream, discover_engine_map_stream,
};

#[derive(Debug, Clone)]
pub struct Bzm2ThreadConfig {
    pub serial_path: String,
    pub baud_rate: u32,
    pub timestamp_count: u8,
    pub nonce_gap: u32,
    pub dispatch_interval: Duration,
    pub nominal_hashrate_ths: f64,
    pub dts_vs_generation: DtsVsGeneration,
}

impl Bzm2ThreadConfig {
    pub fn new(serial_path: String, baud_rate: u32) -> Self {
        Self {
            serial_path,
            baud_rate,
            timestamp_count: DEFAULT_TIMESTAMP_COUNT,
            nonce_gap: DEFAULT_NONCE_GAP,
            dispatch_interval: Duration::from_millis(500),
            nominal_hashrate_ths: 40.0,
            dts_vs_generation: DtsVsGeneration::Gen2,
        }
    }
}

#[derive(Clone)]
pub struct Bzm2ThreadHandle {
    command_tx: mpsc::Sender<ThreadCommand>,
}

impl Bzm2ThreadHandle {
    pub fn shutdown(&self) {
        let _ = self.command_tx.try_send(ThreadCommand::Shutdown);
    }

    pub async fn query_dts_vs(
        &self,
        asic: u8,
    ) -> Result<HashThreadTelemetryUpdate, HashThreadError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ThreadCommand::QueryDtsVs { asic, response_tx })
            .await
            .map_err(|_| HashThreadError::ChannelClosed("command channel closed".into()))?;
        response_rx
            .await
            .map_err(|_| HashThreadError::TelemetryQueryFailed("thread dropped response".into()))?
    }

    pub async fn discover_engine_map(
        &self,
        asic: u8,
        tdm_prediv_raw: u32,
        tdm_counter: u8,
        timeout: Duration,
    ) -> Result<Bzm2DiscoveredEngineMap, HashThreadError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ThreadCommand::DiscoverEngineMap {
                asic,
                tdm_prediv_raw,
                tdm_counter,
                timeout,
                response_tx,
            })
            .await
            .map_err(|_| HashThreadError::ChannelClosed("command channel closed".into()))?;
        response_rx
            .await
            .map_err(|_| HashThreadError::DiagnosticsFailed("thread dropped response".into()))?
    }
}

#[derive(Debug)]
enum ThreadCommand {
    UpdateTask {
        new_task: HashTask,
        response_tx: oneshot::Sender<Result<Option<HashTask>, HashThreadError>>,
    },
    ReplaceTask {
        new_task: HashTask,
        response_tx: oneshot::Sender<Result<Option<HashTask>, HashThreadError>>,
    },
    GoIdle {
        response_tx: oneshot::Sender<Result<Option<HashTask>, HashThreadError>>,
    },
    QueryDtsVs {
        asic: u8,
        response_tx: oneshot::Sender<Result<HashThreadTelemetryUpdate, HashThreadError>>,
    },
    DiscoverEngineMap {
        asic: u8,
        tdm_prediv_raw: u32,
        tdm_counter: u8,
        timeout: Duration,
        response_tx: oneshot::Sender<Result<Bzm2DiscoveredEngineMap, HashThreadError>>,
    },
    Shutdown,
}

#[derive(Clone)]
struct EngineDispatch {
    task: HashTask,
    merkle_root: bitcoin::TxMerkleNode,
    versions: [bitcoin::block::Version; 4],
    base_sequence: u8,
}

pub struct Bzm2Thread {
    name: String,
    command_tx: mpsc::Sender<ThreadCommand>,
    event_rx: Option<mpsc::Receiver<HashThreadEvent>>,
    capabilities: HashThreadCapabilities,
    status: Arc<RwLock<HashThreadStatus>>,
}

impl Bzm2Thread {
    pub fn new(
        name: String,
        reader: SerialReader,
        writer: SerialWriter,
        control: SerialControl,
        config: Bzm2ThreadConfig,
    ) -> Self {
        let (command_tx, command_rx) = mpsc::channel(16);
        let (event_tx, event_rx) = mpsc::channel(64);
        let status = Arc::new(RwLock::new(HashThreadStatus::default()));
        let status_clone = Arc::clone(&status);
        let nominal_hashrate_ths = config.nominal_hashrate_ths;

        tokio::spawn(async move {
            bzm2_thread_actor(
                command_rx,
                event_tx,
                status_clone,
                reader,
                writer,
                control,
                config,
            )
            .await;
        });

        Self {
            name,
            command_tx,
            event_rx: Some(event_rx),
            capabilities: HashThreadCapabilities {
                hashrate_estimate: HashRate::from_terahashes(nominal_hashrate_ths),
            },
            status,
        }
    }

    pub fn shutdown_handle(&self) -> Bzm2ThreadHandle {
        Bzm2ThreadHandle {
            command_tx: self.command_tx.clone(),
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
    ) -> Result<Option<HashTask>, HashThreadError> {
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
            .map_err(|_| HashThreadError::WorkAssignmentFailed("thread dropped response".into()))?
    }

    async fn replace_task(
        &mut self,
        new_task: HashTask,
    ) -> Result<Option<HashTask>, HashThreadError> {
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
            .map_err(|_| HashThreadError::WorkAssignmentFailed("thread dropped response".into()))?
    }

    async fn go_idle(&mut self) -> Result<Option<HashTask>, HashThreadError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ThreadCommand::GoIdle { response_tx })
            .await
            .map_err(|_| HashThreadError::ChannelClosed("command channel closed".into()))?;
        response_rx
            .await
            .map_err(|_| HashThreadError::WorkAssignmentFailed("thread dropped response".into()))?
    }

    fn take_event_receiver(&mut self) -> Option<mpsc::Receiver<HashThreadEvent>> {
        self.event_rx.take()
    }

    fn status(&self) -> HashThreadStatus {
        self.status.read().unwrap().clone()
    }
}

async fn bzm2_thread_actor(
    mut command_rx: mpsc::Receiver<ThreadCommand>,
    event_tx: mpsc::Sender<HashThreadEvent>,
    status: Arc<RwLock<HashThreadStatus>>,
    mut reader: SerialReader,
    mut writer: SerialWriter,
    control: SerialControl,
    config: Bzm2ThreadConfig,
) {
    if let Err(err) = control.set_baud_rate(config.baud_rate) {
        warn!(path = %config.serial_path, error = %err, "Failed to set BZM2 baud rate");
    }

    let _ = event_tx
        .send(HashThreadEvent::StatusUpdate(snapshot_status(&status)))
        .await;

    let engine_coords = default_engine_coordinates();
    let mut parser = TdmFrameParser::new(config.dts_vs_generation);
    let mut current_task: Option<HashTask> = None;
    let mut engine_dispatches: HashMap<u16, EngineDispatch> = HashMap::new();
    let mut base_sequence: u8 = 0;
    let mut dispatch_tick = tokio::time::interval(config.dispatch_interval);
    dispatch_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut ntime_tick = tokio::time::interval(Duration::from_secs(1));
    ntime_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut status_tick = tokio::time::interval(Duration::from_secs(5));
    status_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut read_buf = [0u8; 4096];
    let mut dts_vs_configured = false;

    loop {
        tokio::select! {
            Some(command) = command_rx.recv() => {
                match command {
                    ThreadCommand::UpdateTask { new_task, response_tx } => {
                        let old = current_task.replace(new_task);
                        if let Some(ref task) = current_task {
                            if let Err(err) = dispatch_task_to_board(
                                &mut writer,
                                task,
                                base_sequence,
                                &engine_coords,
                                &mut engine_dispatches,
                                &config,
                            ).await {
                                let _ = response_tx.send(Err(err));
                                continue;
                            }
                            base_sequence = base_sequence.wrapping_add(1);
                            set_active(&status, true, config.nominal_hashrate_ths);
                            let _ = event_tx.send(HashThreadEvent::StatusUpdate(snapshot_status(&status))).await;
                        }
                        let _ = response_tx.send(Ok(old));
                    }
                    ThreadCommand::ReplaceTask { new_task, response_tx } => {
                        engine_dispatches.clear();
                        let old = current_task.replace(new_task);
                        if let Some(ref task) = current_task {
                            if let Err(err) = dispatch_task_to_board(
                                &mut writer,
                                task,
                                base_sequence,
                                &engine_coords,
                                &mut engine_dispatches,
                                &config,
                            ).await {
                                let _ = response_tx.send(Err(err));
                                continue;
                            }
                            base_sequence = base_sequence.wrapping_add(1);
                            set_active(&status, true, config.nominal_hashrate_ths);
                            let _ = event_tx.send(HashThreadEvent::StatusUpdate(snapshot_status(&status))).await;
                        }
                        let _ = response_tx.send(Ok(old));
                    }
                    ThreadCommand::GoIdle { response_tx } => {
                        engine_dispatches.clear();
                        let old = current_task.take();
                        set_active(&status, false, config.nominal_hashrate_ths);
                        let _ = event_tx.send(HashThreadEvent::StatusUpdate(snapshot_status(&status))).await;
                        let _ = response_tx.send(Ok(old));
                    }
                    ThreadCommand::QueryDtsVs { asic, response_tx } => {
                        let result = query_dts_vs_telemetry(
                            asic,
                            &mut reader,
                            &mut writer,
                            &mut parser,
                            &engine_dispatches,
                            &config,
                            &status,
                            &event_tx,
                            &mut dts_vs_configured,
                        ).await;
                        let _ = response_tx.send(result);
                    }
                    ThreadCommand::DiscoverEngineMap {
                        asic,
                        tdm_prediv_raw,
                        tdm_counter,
                        timeout,
                        response_tx,
                    } => {
                        if current_task.is_some() {
                            let _ = response_tx.send(Err(HashThreadError::DiagnosticsFailed(
                                "BZM2 engine discovery requires the thread to be idle".into(),
                            )));
                            continue;
                        }
                        let result = discover_engine_map_stream(
                            &mut reader,
                            &mut writer,
                            asic,
                            tdm_prediv_raw,
                            tdm_counter,
                            timeout,
                        )
                        .await
                        .map_err(|err| HashThreadError::DiagnosticsFailed(err.to_string()));
                        let _ = response_tx.send(result);
                    }
                    ThreadCommand::Shutdown => break,
                }
            }
            read_result = reader.read(&mut read_buf) => {
                match read_result {
                    Ok(0) => break,
                    Ok(n) => {
                        let mut should_shutdown = false;
                        for frame in parser.push(&read_buf[..n]) {
                            match frame {
                                TdmFrame::Result(frame) => {
                                    handle_result_frame(
                                        &frame,
                                        &engine_dispatches,
                                        &config,
                                        &status,
                                        &event_tx,
                                    )
                                    .await;
                                }
                                TdmFrame::DtsVs(frame) => {
                                    should_shutdown = handle_dts_vs_frame(&frame, &config, &status, &event_tx).await;
                                    if should_shutdown {
                                        break;
                                    }
                                }
                                TdmFrame::Register(_) | TdmFrame::Noop(_) => {}
                            }
                        }
                        if should_shutdown {
                            break;
                        }
                    }
                    Err(err) => {
                        error!(path = %config.serial_path, error = %err, "BZM2 serial read failed");
                        record_hardware_error(&status);
                        break;
                    }
                }
            }
            _ = dispatch_tick.tick(), if current_task.is_some() => {
                if let Some(ref task) = current_task {
                    match dispatch_task_to_board(
                        &mut writer,
                        task,
                        base_sequence,
                        &engine_coords,
                        &mut engine_dispatches,
                        &config,
                    ).await {
                        Ok(()) => {
                            base_sequence = base_sequence.wrapping_add(1);
                        }
                        Err(err) => {
                            error!(path = %config.serial_path, error = %err, "BZM2 dispatch failed");
                            record_hardware_error(&status);
                        }
                    }
                }
            }
            _ = ntime_tick.tick(), if current_task.is_some() => {
                if let Some(ref mut task) = current_task {
                    task.ntime = task.ntime.wrapping_add(1);
                }
            }
            _ = status_tick.tick() => {
                let _ = event_tx.send(HashThreadEvent::StatusUpdate(snapshot_status(&status))).await;
            }
        }
    }

    set_active(&status, false, config.nominal_hashrate_ths);
    let _ = event_tx
        .send(HashThreadEvent::StatusUpdate(snapshot_status(&status)))
        .await;
}

async fn handle_dts_vs_frame(
    frame: &TdmDtsVsFrame,
    config: &Bzm2ThreadConfig,
    status: &Arc<RwLock<HashThreadStatus>>,
    event_tx: &mpsc::Sender<HashThreadEvent>,
) -> bool {
    if let Some(update) = build_dts_vs_telemetry_update(frame, config) {
        if let Some(reading) = update.temperatures.first() {
            set_temperature(status, reading.temperature_c);
        }
        let _ = event_tx
            .send(HashThreadEvent::TelemetryUpdate(update))
            .await;
    }

    match frame {
        TdmDtsVsFrame::Gen1(frame) => {
            trace!(
                path = %config.serial_path,
                asic = frame.asic,
                voltage = frame.voltage,
                voltage_enabled = frame.voltage_enabled,
                thermal_tune_code = frame.thermal_tune_code,
                thermal_validity = frame.thermal_validity,
                thermal_enabled = frame.thermal_enabled,
                "BZM2 DTS/VS telemetry frame"
            );
            false
        }
        TdmDtsVsFrame::Gen2(frame) => {
            trace!(
                path = %config.serial_path,
                asic = frame.asic,
                thermal_trip = frame.thermal_trip_status,
                thermal_fault = frame.thermal_fault,
                voltage_fault = frame.voltage_fault,
                voltage_shutdown = frame.voltage_shutdown_status,
                thermal_tune_code = frame.thermal_tune_code,
                ch0_voltage = frame.ch0_voltage,
                ch1_voltage = frame.ch1_voltage,
                ch2_voltage = frame.ch2_voltage,
                "BZM2 DTS/VS gen2 telemetry frame"
            );

            if frame.thermal_trip_status
                || frame.thermal_fault
                || frame.voltage_fault
                || frame.voltage_shutdown_status
            {
                warn!(
                    path = %config.serial_path,
                    asic = frame.asic,
                    thermal_trip = frame.thermal_trip_status,
                    thermal_fault = frame.thermal_fault,
                    voltage_fault = frame.voltage_fault,
                    voltage_shutdown = frame.voltage_shutdown_status,
                    "BZM2 hardware fault reported by DTS/VS frame"
                );
                record_hardware_error(status);
                return true;
            }
            false
        }
    }
}

async fn query_dts_vs_telemetry(
    asic: u8,
    reader: &mut SerialReader,
    writer: &mut SerialWriter,
    parser: &mut TdmFrameParser,
    engine_dispatches: &HashMap<u16, EngineDispatch>,
    config: &Bzm2ThreadConfig,
    status: &Arc<RwLock<HashThreadStatus>>,
    event_tx: &mpsc::Sender<HashThreadEvent>,
    dts_vs_configured: &mut bool,
) -> Result<HashThreadTelemetryUpdate, HashThreadError> {
    if !*dts_vs_configured {
        configure_dts_vs_stream(writer, reader, &Bzm2DtsVsConfig::default())
            .await
            .map_err(|err| HashThreadError::TelemetryQueryFailed(err.to_string()))?;
        *dts_vs_configured = true;
    }

    let deadline = tokio::time::Instant::now() + DEFAULT_DTS_VS_QUERY_TIMEOUT;
    let mut read_buf = [0u8; 512];
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(HashThreadError::TelemetryQueryFailed(format!(
                "timed out waiting for DTS/VS frame from ASIC {asic:#x}"
            )));
        }
        let remaining = deadline.saturating_duration_since(now);
        let read = tokio::time::timeout(remaining, reader.read(&mut read_buf))
            .await
            .map_err(|_| {
                HashThreadError::TelemetryQueryFailed(format!(
                    "timed out waiting for DTS/VS frame from ASIC {asic:#x}"
                ))
            })
            .and_then(|result| {
                result.map_err(|err| HashThreadError::TelemetryQueryFailed(err.to_string()))
            })?;
        if read == 0 {
            return Err(HashThreadError::TelemetryQueryFailed(
                "serial stream closed while waiting for DTS/VS data".into(),
            ));
        }

        for frame in parser.push(&read_buf[..read]) {
            match frame {
                TdmFrame::Result(frame) => {
                    handle_result_frame(&frame, engine_dispatches, config, status, event_tx).await;
                }
                TdmFrame::DtsVs(frame) => {
                    let frame_asic = dts_vs_frame_asic(&frame);
                    let update =
                        build_dts_vs_telemetry_update(&frame, config).ok_or_else(|| {
                            HashThreadError::TelemetryQueryFailed(
                                "failed to build DTS/VS telemetry update".into(),
                            )
                        })?;
                    let should_shutdown =
                        handle_dts_vs_frame(&frame, config, status, event_tx).await;
                    if should_shutdown {
                        return Err(HashThreadError::TelemetryQueryFailed(
                            "DTS/VS query observed a hardware fault".into(),
                        ));
                    }
                    if frame_asic == asic {
                        return Ok(update);
                    }
                }
                TdmFrame::Register(_) | TdmFrame::Noop(_) => {}
            }
        }
    }
}

async fn dispatch_task_to_board(
    writer: &mut SerialWriter,
    task: &HashTask,
    base_sequence: u8,
    engine_coords: &[(u8, u8)],
    engine_dispatches: &mut HashMap<u16, EngineDispatch>,
    config: &Bzm2ThreadConfig,
) -> Result<(), HashThreadError> {
    let merkle_root = match &task.template.merkle_root {
        MerkleRootKind::Fixed(root) => *root,
        MerkleRootKind::Computed(_) => {
            let en2 = task.en2.as_ref().ok_or_else(|| {
                HashThreadError::WorkAssignmentFailed(
                    "BZM2 requires extranonce2 for computed merkle root".into(),
                )
            })?;
            task.template.compute_merkle_root(en2).map_err(|err| {
                HashThreadError::WorkAssignmentFailed(format!(
                    "BZM2 merkle root computation failed: {err}"
                ))
            })?
        }
    };

    let versions = compute_micro_versions(task);
    let midstates = versions.map(|version| compute_midstate(task, merkle_root, version));
    let header_bytes = serialize(&BlockHeader {
        version: versions[0],
        prev_blockhash: task.template.prev_blockhash,
        merkle_root,
        time: task.ntime,
        bits: task.template.bits,
        nonce: 0,
    });
    let merkle_root_residue = u32::from_le_bytes(header_bytes[64..68].try_into().unwrap());
    let lead_zeros = leading_zero_threshold(task.share_target).saturating_sub(32);
    let timestamp_count = config.timestamp_count;
    let bits = task.template.bits.to_consensus();

    for &(row, col) in engine_coords {
        let engine_address = logical_engine_address(row, col);

        writer
            .write_all(&encode_write_register(
                BROADCAST_ASIC,
                engine_address,
                ENGINE_REG_ZEROS_TO_FIND,
                &[lead_zeros],
            ))
            .await
            .map_err(|err| {
                HashThreadError::WorkAssignmentFailed(format!("Failed to write lead zeros: {err}"))
            })?;

        writer
            .write_all(&encode_write_register(
                BROADCAST_ASIC,
                engine_address,
                ENGINE_REG_TIMESTAMP_COUNT,
                &[timestamp_count],
            ))
            .await
            .map_err(|err| {
                HashThreadError::WorkAssignmentFailed(format!(
                    "Failed to write timestamp count: {err}"
                ))
            })?;

        writer
            .write_all(&encode_write_register(
                BROADCAST_ASIC,
                engine_address,
                ENGINE_REG_TARGET,
                &bits.to_le_bytes(),
            ))
            .await
            .map_err(|err| {
                HashThreadError::WorkAssignmentFailed(format!("Failed to write target bits: {err}"))
            })?;

        let seq_start = (base_sequence % 2) * 4;
        for (micro_job_id, midstate) in midstates.iter().enumerate() {
            let job_control = if micro_job_id == 3 { 3 } else { 0 };
            writer
                .write_all(&encode_write_job(
                    BROADCAST_ASIC,
                    engine_address,
                    midstate,
                    merkle_root_residue,
                    task.ntime,
                    seq_start + micro_job_id as u8,
                    job_control,
                ))
                .await
                .map_err(|err| {
                    HashThreadError::WorkAssignmentFailed(format!("Failed to write job: {err}"))
                })?;
        }

        engine_dispatches.insert(
            protocol::logical_engine_id(row, col).unwrap(),
            EngineDispatch {
                task: task.clone(),
                merkle_root,
                versions,
                base_sequence,
            },
        );
    }

    Ok(())
}

async fn handle_result_frame(
    frame: &protocol::TdmResultFrame,
    engine_dispatches: &HashMap<u16, EngineDispatch>,
    config: &Bzm2ThreadConfig,
    status: &Arc<RwLock<HashThreadStatus>>,
    event_tx: &mpsc::Sender<HashThreadEvent>,
) {
    let Some((share, target_diff, engine_id)) =
        reconstruct_share_from_result(frame, engine_dispatches, config)
    else {
        return;
    };

    let share_tx = {
        let dispatch = engine_dispatches
            .get(&engine_id)
            .expect("dispatch must exist for reconstructed share");
        dispatch.task.share_tx.clone()
    };

    if share_tx.send(share.clone()).await.is_ok() {
        let snapshot = {
            let mut lock = status.write().unwrap();
            lock.chip_shares_found += 1;
            lock.clone()
        };
        let _ = event_tx.send(HashThreadEvent::StatusUpdate(snapshot)).await;
    }

    trace!(
        engine_id,
        seq = frame.sequence_id,
        nonce = format!("{:#010x}", share.nonce),
        hash = %share.hash,
        hash_diff = %Difficulty::from_hash(&share.hash),
        target_diff = %target_diff,
        "BZM2 share accepted"
    );
}

fn reconstruct_share_from_result(
    frame: &protocol::TdmResultFrame,
    engine_dispatches: &HashMap<u16, EngineDispatch>,
    config: &Bzm2ThreadConfig,
) -> Option<(Share, Difficulty, u16)> {
    if !frame.nonce_valid() {
        return None;
    }

    let engine_id = frame.logical_engine_id()?;
    let dispatch = engine_dispatches.get(&engine_id)?;

    let hardware_base_sequence = frame.sequence_id / 4;
    if (dispatch.base_sequence % 2) != hardware_base_sequence {
        return None;
    }

    let micro_job_id = (frame.sequence_id % 4) as usize;
    let version = dispatch.versions[micro_job_id];
    let ntime_offset = u32::from(config.timestamp_count.saturating_sub(frame.reported_time));
    let ntime = dispatch.task.ntime.wrapping_add(ntime_offset);
    let nonce = frame.nonce.wrapping_sub(config.nonce_gap);

    let header = BlockHeader {
        version,
        prev_blockhash: dispatch.task.template.prev_blockhash,
        merkle_root: dispatch.merkle_root,
        time: ntime,
        bits: dispatch.task.template.bits,
        nonce,
    };
    let hash = header.block_hash();

    if !dispatch.task.share_target.is_met_by(hash) {
        return None;
    }

    Some((
        Share {
            nonce,
            hash,
            version,
            ntime,
            extranonce2: dispatch.task.en2,
            expected_work: dispatch.task.share_target.to_work(),
        },
        Difficulty::from_target(dispatch.task.share_target),
        engine_id,
    ))
}

fn compute_micro_versions(task: &HashTask) -> [bitcoin::block::Version; 4] {
    let candidates = [0u16, 2, 4, 8];
    let mut versions = [task.template.version.base(); 4];

    for (slot, candidate) in candidates.into_iter().enumerate() {
        let gp_bits = GeneralPurposeBits::new(candidate.to_be_bytes());
        versions[slot] = task
            .template
            .version
            .apply_gp_bits(&gp_bits)
            .unwrap_or_else(|_| task.template.version.base());
    }

    versions
}

fn compute_midstate(
    task: &HashTask,
    merkle_root: bitcoin::TxMerkleNode,
    version: bitcoin::block::Version,
) -> [u8; 32] {
    let header_bytes = serialize(&BlockHeader {
        version,
        prev_blockhash: task.template.prev_blockhash,
        merkle_root,
        time: task.ntime,
        bits: task.template.bits,
        nonce: 0,
    });

    let mut engine = sha256::HashEngine::default();
    engine.input(&header_bytes[..64]);
    engine.midstate().to_byte_array()
}

fn snapshot_status(status: &Arc<RwLock<HashThreadStatus>>) -> HashThreadStatus {
    status.read().unwrap().clone()
}

fn set_active(status: &Arc<RwLock<HashThreadStatus>>, is_active: bool, nominal_hashrate_ths: f64) {
    let mut lock = status.write().unwrap();
    lock.is_active = is_active;
    lock.hashrate = if is_active {
        HashRate::from_terahashes(nominal_hashrate_ths)
    } else {
        HashRate::default()
    };
}

fn record_hardware_error(status: &Arc<RwLock<HashThreadStatus>>) {
    let mut lock = status.write().unwrap();
    lock.hardware_errors = lock.hardware_errors.saturating_add(1);
}

fn set_temperature(status: &Arc<RwLock<HashThreadStatus>>, temperature_c: Option<f32>) {
    let mut lock = status.write().unwrap();
    lock.temperature_c = temperature_c;
}

fn build_dts_vs_telemetry_update(
    frame: &TdmDtsVsFrame,
    config: &Bzm2ThreadConfig,
) -> Option<HashThreadTelemetryUpdate> {
    let prefix = sensor_prefix(&config.serial_path);
    match frame {
        TdmDtsVsFrame::Gen1(frame) => Some(HashThreadTelemetryUpdate {
            temperatures: Vec::new(),
            powers: vec![HashThreadPowerReading {
                name: format!("{prefix}-asic-{}-vs", frame.asic),
                voltage_v: frame
                    .voltage_enabled
                    .then(|| legacy_tune_code_to_voltage_v(frame.voltage)),
                current_a: None,
                power_w: None,
            }],
        }),
        TdmDtsVsFrame::Gen2(frame) => Some(HashThreadTelemetryUpdate {
            temperatures: vec![HashThreadTemperatureReading {
                name: format!("{prefix}-asic-{}-dts", frame.asic),
                temperature_c: (frame.thermal_enabled && frame.thermal_validity)
                    .then(|| legacy_tune_code_to_temperature_c(frame.thermal_tune_code)),
            }],
            powers: vec![
                HashThreadPowerReading {
                    name: format!("{prefix}-asic-{}-vs-ch0", frame.asic),
                    voltage_v: frame
                        .voltage_enabled
                        .then(|| legacy_tune_code_to_voltage_v(frame.ch0_voltage)),
                    current_a: None,
                    power_w: None,
                },
                HashThreadPowerReading {
                    name: format!("{prefix}-asic-{}-vs-ch1", frame.asic),
                    voltage_v: frame
                        .voltage_enabled
                        .then(|| legacy_tune_code_to_voltage_v(frame.ch1_voltage)),
                    current_a: None,
                    power_w: None,
                },
                HashThreadPowerReading {
                    name: format!("{prefix}-asic-{}-vs-ch2", frame.asic),
                    voltage_v: frame
                        .voltage_enabled
                        .then(|| legacy_tune_code_to_voltage_v(frame.ch2_voltage)),
                    current_a: None,
                    power_w: None,
                },
            ],
        }),
    }
}

fn dts_vs_frame_asic(frame: &TdmDtsVsFrame) -> u8 {
    match frame {
        TdmDtsVsFrame::Gen1(frame) => frame.asic,
        TdmDtsVsFrame::Gen2(frame) => frame.asic,
    }
}

fn sensor_prefix(serial_path: &str) -> String {
    Path::new(serial_path)
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or(serial_path)
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}

fn legacy_tune_code_to_temperature_c(tune_code: u16) -> f32 {
    let resolution_power = 4096.0_f32;
    -293.8 + 631.8 * ((tune_code as f32) - (2048.0 / resolution_power)) / 4096.0
}

fn legacy_tune_code_to_voltage_v(tune_code: u16) -> f32 {
    let resolution_power = 16384.0_f32;
    0.4 * 0.7067 * (6.0 * (tune_code as f32) / 16384.0 - 3.0 / resolution_power - 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job_source::{GeneralPurposeBits, JobTemplate, VersionTemplate};
    use crate::transport::{SerialConfig, SerialStream};
    use bitcoin::hashes::Hash;
    use bitcoin::pow::Target;
    use nix::pty::openpty;
    use std::collections::HashMap as StdHashMap;
    use std::os::unix::io::IntoRawFd;
    use tokio::sync::mpsc as tokio_mpsc;

    fn test_task() -> HashTask {
        let share_target = Target::from_be_bytes([
            0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff,
        ]);
        let template = Arc::new(JobTemplate {
            id: "bzm2-test".into(),
            prev_blockhash: bitcoin::BlockHash::all_zeros(),
            version: VersionTemplate::new(
                bitcoin::block::Version::from_consensus(0x2000_0000),
                GeneralPurposeBits::full(),
            )
            .unwrap(),
            bits: bitcoin::pow::CompactTarget::from_consensus(0x1d00ffff),
            share_target,
            time: 1_700_000_000,
            merkle_root: MerkleRootKind::Fixed(bitcoin::TxMerkleNode::all_zeros()),
        });
        let (share_tx, _share_rx) = tokio_mpsc::channel(4);
        HashTask {
            template,
            en2_range: None,
            en2: None,
            share_target,
            ntime: 1_700_000_000,
            share_tx,
        }
    }

    #[test]
    fn midstate_changes_with_micro_job_versions() {
        let task = test_task();
        let merkle_root = bitcoin::TxMerkleNode::all_zeros();
        let versions = compute_micro_versions(&task);
        let a = compute_midstate(&task, merkle_root, versions[0]);
        let b = compute_midstate(&task, merkle_root, versions[1]);
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn dispatch_writes_expected_packet_fanout() {
        let pty = openpty(None, None).unwrap();
        let writer_side =
            SerialStream::from_fd(pty.master.into_raw_fd(), SerialConfig::default()).unwrap();
        let reader_side =
            SerialStream::from_fd(pty.slave.into_raw_fd(), SerialConfig::default()).unwrap();
        let (_reader_a, mut writer, _control_a) = writer_side.split();
        let (mut reader, _writer_b, _control_b) = reader_side.split();

        let task = test_task();
        let mut engine_dispatches = StdHashMap::new();
        let config = Bzm2ThreadConfig::new("/dev/null".into(), 5_000_000);
        let engine_coords = vec![(0, 0), (0, 1)];

        dispatch_task_to_board(
            &mut writer,
            &task,
            1,
            &engine_coords,
            &mut engine_dispatches,
            &config,
        )
        .await
        .unwrap();

        let expected_bytes_per_engine = 8 + 8 + 11 + (48 * 4);
        let expected_total = expected_bytes_per_engine * engine_coords.len();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(250);
        let mut buf = vec![0u8; 512];
        let mut bytes = Vec::with_capacity(expected_total);
        while bytes.len() < expected_total {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out before collecting the full dispatch stream"
            );
            let n = tokio::time::timeout(remaining, reader.read(&mut buf))
                .await
                .unwrap()
                .unwrap();
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&buf[..n]);
        }

        assert_eq!(bytes.len(), expected_total);
        assert_eq!(engine_dispatches.len(), engine_coords.len());

        let first_packet_len = u16::from_le_bytes([bytes[0], bytes[1]]) as usize;
        assert_eq!(first_packet_len, 8);

        let last_packet_start = bytes.len() - 48;
        assert_eq!(
            u16::from_le_bytes([bytes[last_packet_start], bytes[last_packet_start + 1]]) as usize,
            48
        );
        assert_eq!(bytes[last_packet_start + 46], 7);
        assert_eq!(bytes[last_packet_start + 47], 3);
    }

    #[tokio::test]
    async fn parsed_uart_frame_emits_share_and_status_event() {
        let mut task = test_task();
        let merkle_root = bitcoin::TxMerkleNode::all_zeros();
        let versions = compute_micro_versions(&task);
        let (row, col) = default_engine_coordinates()[0];
        let engine_id = protocol::logical_engine_id(row, col).unwrap();
        let nonce = 0;
        let expected_hash = bitcoin::block::Header {
            version: versions[0],
            prev_blockhash: task.template.prev_blockhash,
            merkle_root,
            time: task.ntime,
            bits: task.template.bits,
            nonce,
        }
        .block_hash();
        task.share_target = Difficulty::from_hash(&expected_hash).to_target();

        let mut engine_dispatches = StdHashMap::new();
        engine_dispatches.insert(
            engine_id,
            EngineDispatch {
                task: task.clone(),
                merkle_root,
                versions,
                base_sequence: 0,
            },
        );

        let config = Bzm2ThreadConfig::new("/dev/null".into(), 5_000_000);
        let status = Arc::new(RwLock::new(HashThreadStatus {
            hashrate: HashRate::from_terahashes(40.0),
            is_active: true,
            ..Default::default()
        }));
        let (event_tx, mut event_rx) = tokio_mpsc::channel(4);
        let (share_tx, mut share_rx) = tokio_mpsc::channel(4);
        engine_dispatches.get_mut(&engine_id).unwrap().task.share_tx = share_tx;

        let engine_address = protocol::logical_engine_address(row, col);
        let header = ((0x8u16) << 12) | engine_address;
        let mut raw = Vec::with_capacity(10);
        raw.push(0);
        raw.push(protocol::OPCODE_UART_READRESULT);
        raw.extend_from_slice(&header.to_be_bytes());
        raw.extend_from_slice(&(nonce + DEFAULT_NONCE_GAP).to_le_bytes());
        raw.push(0);
        raw.push(DEFAULT_TIMESTAMP_COUNT);

        let mut parser = protocol::TdmResultParser::default();
        let frames = parser.push(&raw);
        assert_eq!(frames.len(), 1);
        assert!(reconstruct_share_from_result(&frames[0], &engine_dispatches, &config).is_some());

        handle_result_frame(&frames[0], &engine_dispatches, &config, &status, &event_tx).await;

        let share = tokio::time::timeout(Duration::from_millis(250), share_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(share.nonce, nonce);
        assert_eq!(share.ntime, task.ntime);
        assert_eq!(share.version, versions[0]);
        assert_eq!(share.hash, expected_hash);

        let status_update = tokio::time::timeout(Duration::from_millis(250), event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        match status_update {
            HashThreadEvent::StatusUpdate(snapshot) => {
                assert!(snapshot.is_active);
                assert_eq!(snapshot.chip_shares_found, 1);
                assert_eq!(snapshot.hashrate, HashRate::from_terahashes(40.0));
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }
    #[tokio::test]
    async fn gen2_dts_vs_emits_api_telemetry() {
        let config = Bzm2ThreadConfig::new("/dev/ttyUSB0".into(), 5_000_000);
        let status = Arc::new(RwLock::new(HashThreadStatus::default()));
        let (event_tx, mut event_rx) = tokio_mpsc::channel(4);
        let frame = TdmDtsVsFrame::Gen2(protocol::TdmDtsVsGen2Frame {
            asic: 2,
            ch0_voltage: 0x1645,
            ch1_voltage: 0x04B4,
            ch2_voltage: 0x16AC,
            voltage_shutdown_status: false,
            voltage_enabled: true,
            thermal_tune_code: 0x07A9,
            thermal_trip_status: false,
            thermal_fault: false,
            thermal_validity: true,
            thermal_enabled: true,
            voltage_fault: false,
            dll0_lock: false,
            dll1_lock: true,
            pll_lock: true,
        });

        let should_shutdown = handle_dts_vs_frame(&frame, &config, &status, &event_tx).await;
        assert!(!should_shutdown);

        let update = event_rx.recv().await.unwrap();
        match update {
            HashThreadEvent::TelemetryUpdate(update) => {
                assert_eq!(update.temperatures.len(), 1);
                assert_eq!(update.temperatures[0].name, "ttyUSB0-asic-2-dts");
                let temp = update.temperatures[0].temperature_c.unwrap();
                assert!((temp - legacy_tune_code_to_temperature_c(0x07A9)).abs() < 0.01);

                assert_eq!(update.powers.len(), 3);
                assert_eq!(update.powers[0].name, "ttyUSB0-asic-2-vs-ch0");
                assert!(
                    (update.powers[0].voltage_v.unwrap() - legacy_tune_code_to_voltage_v(0x1645))
                        .abs()
                        < 0.0001
                );
                assert_eq!(update.powers[1].name, "ttyUSB0-asic-2-vs-ch1");
                assert_eq!(update.powers[2].name, "ttyUSB0-asic-2-vs-ch2");
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let snapshot = status.read().unwrap().clone();
        assert!(
            (snapshot.temperature_c.unwrap() - legacy_tune_code_to_temperature_c(0x07A9)).abs()
                < 0.01
        );
    }

    #[tokio::test]
    async fn gen2_dts_vs_fault_shuts_down_live_thread() {
        let pty = openpty(None, None).unwrap();
        let thread_side =
            SerialStream::from_fd(pty.master.into_raw_fd(), SerialConfig::default()).unwrap();
        let host_side =
            SerialStream::from_fd(pty.slave.into_raw_fd(), SerialConfig::default()).unwrap();
        let (reader, writer, control) = thread_side.split();
        let (_host_reader, mut host_writer, _host_control) = host_side.split();

        let mut config = Bzm2ThreadConfig::new("/dev/null".into(), 5_000_000);
        config.dts_vs_generation = protocol::DtsVsGeneration::Gen2;
        let mut thread = Bzm2Thread::new("BZM2 test".into(), reader, writer, control, config);
        let mut event_rx = thread.take_event_receiver().unwrap();

        let initial = tokio::time::timeout(Duration::from_millis(250), event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(initial, HashThreadEvent::StatusUpdate(_)));

        host_writer
            .write_all(&[
                0x00,
                protocol::OPCODE_UART_DTS_VS,
                0xD5,
                0xAB,
                0x34,
                0x12,
                0x45,
                0x96,
                0xA9,
                0xF7,
            ])
            .await
            .unwrap();

        let mut saw_fault_status = false;
        let closed = tokio::time::timeout(Duration::from_secs(1), async {
            while let Some(event) = event_rx.recv().await {
                if let HashThreadEvent::StatusUpdate(status) = event {
                    if !status.is_active && status.hardware_errors >= 1 {
                        saw_fault_status = true;
                    }
                }
            }
        })
        .await;

        assert!(
            closed.is_ok(),
            "thread should exit after DTS/VS hardware fault"
        );
        assert!(
            saw_fault_status,
            "thread should publish a final faulted status"
        );
    }

    #[test]
    fn reconstructs_share_from_matching_result_frame() {
        let mut task = test_task();

        let merkle_root = bitcoin::TxMerkleNode::all_zeros();
        let versions = compute_micro_versions(&task);
        let (row, col) = default_engine_coordinates()[0];
        let engine_id = protocol::logical_engine_id(row, col).unwrap();
        let nonce = 0;
        let expected_hash = bitcoin::block::Header {
            version: versions[0],
            prev_blockhash: task.template.prev_blockhash,
            merkle_root,
            time: task.ntime,
            bits: task.template.bits,
            nonce,
        }
        .block_hash();
        task.share_target = Difficulty::from_hash(&expected_hash).to_target();
        let frame = protocol::TdmResultFrame {
            asic: 0,
            engine_address: protocol::logical_engine_address(row, col),
            status: 0x8,
            nonce: nonce + DEFAULT_NONCE_GAP,
            sequence_id: 0,
            reported_time: DEFAULT_TIMESTAMP_COUNT,
        };

        let mut engine_dispatches = StdHashMap::new();
        engine_dispatches.insert(
            engine_id,
            EngineDispatch {
                task: task.clone(),
                merkle_root,
                versions,
                base_sequence: 0,
            },
        );

        let config = Bzm2ThreadConfig::new("/dev/null".into(), 5_000_000);
        let (share, target_diff, reconstructed_engine_id) =
            reconstruct_share_from_result(&frame, &engine_dispatches, &config).unwrap();

        assert_eq!(reconstructed_engine_id, engine_id);
        assert_eq!(share.nonce, nonce);
        assert_eq!(share.ntime, task.ntime);
        assert_eq!(share.version, versions[0]);
        assert_eq!(
            share.hash,
            bitcoin::block::Header {
                version: versions[0],
                prev_blockhash: task.template.prev_blockhash,
                merkle_root,
                time: task.ntime,
                bits: task.template.bits,
                nonce,
            }
            .block_hash()
        );
        assert_eq!(target_diff, Difficulty::from_target(task.share_target));
    }
}
