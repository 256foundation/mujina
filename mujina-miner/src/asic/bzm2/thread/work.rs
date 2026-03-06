use std::sync::Arc;

use bitcoin::{TxMerkleNode, block::Version as BlockVersion};
use futures::{SinkExt, sink::Sink};

use crate::{
    asic::hash_thread::{HashTask, HashThreadError},
    job_source::{Extranonce2, MerkleRootKind},
};

use super::{
    AUTO_CLOCK_UNGATE, ENGINE_COLS, ENGINE_EN2_OFFSET_START, ENGINE_ROWS, INVALID_ENGINE_0_COL,
    INVALID_ENGINE_0_ROW, INVALID_ENGINE_1_COL, INVALID_ENGINE_1_ROW, INVALID_ENGINE_2_COL,
    INVALID_ENGINE_2_ROW, INVALID_ENGINE_3_COL, INVALID_ENGINE_3_ROW, MIDSTATE_COUNT,
    WORK_ENGINE_COUNT, WRITEJOB_CTL_REPLACE, hashing::build_header_bytes,
    hashing::compute_midstate_le, protocol, write_reg_u8, write_reg_u32,
};

struct TaskJobPayload {
    midstates: [[u8; 32]; MIDSTATE_COUNT],
    merkle_residue: u32,
    timestamp: u32,
}

#[derive(Clone)]
pub(super) struct EngineAssignment {
    pub(super) merkle_root: TxMerkleNode,
    pub(super) extranonce2: Option<Extranonce2>,
    pub(super) midstates: [[u8; 32]; MIDSTATE_COUNT],
}

#[derive(Clone)]
pub(super) struct AssignedTask {
    pub(super) task: HashTask,
    pub(super) merkle_root: TxMerkleNode,
    pub(super) engine_assignments: Arc<[EngineAssignment]>,
    pub(super) microjob_versions: [BlockVersion; MIDSTATE_COUNT],
    pub(super) sequence_id: u8,
    pub(super) timestamp_count: u8,
    pub(super) leading_zeros: u8,
    pub(super) nonce_minus_value: u32,
}

pub(super) fn engine_id(row: u16, col: u16) -> u16 {
    ((col & 0x3f) << 6) | (row & 0x3f)
}

pub(super) fn is_invalid_engine(row: u16, col: u16) -> bool {
    (row == INVALID_ENGINE_0_ROW && col == INVALID_ENGINE_0_COL)
        || (row == INVALID_ENGINE_1_ROW && col == INVALID_ENGINE_1_COL)
        || (row == INVALID_ENGINE_2_ROW && col == INVALID_ENGINE_2_COL)
        || (row == INVALID_ENGINE_3_ROW && col == INVALID_ENGINE_3_COL)
}

pub(super) fn logical_engine_index(row: u16, col: u16) -> Option<usize> {
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

#[cfg(test)]
pub(super) fn task_to_bzm2_payload(
    task: &HashTask,
    merkle_root: TxMerkleNode,
    versions: [BlockVersion; MIDSTATE_COUNT],
) -> Result<[[u8; 32]; MIDSTATE_COUNT], HashThreadError> {
    Ok(build_task_job_payload(task, merkle_root, versions)?.midstates)
}

fn build_task_job_payload(
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

pub(super) async fn send_task_to_all_engines<W>(
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
            let payload =
                build_task_job_payload(&engine_task, merkle_root, versions).map_err(|e| {
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
