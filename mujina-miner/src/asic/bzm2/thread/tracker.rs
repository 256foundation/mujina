use std::collections::VecDeque;

use bitcoin::block::Version as BlockVersion;
use bitcoin::hashes::Hash as _;

use super::{
    Bzm2CheckResult, MIDSTATE_COUNT, READRESULT_ASSIGNMENT_HISTORY_LIMIT, READRESULT_SLOT_HISTORY,
    hashing::bzm2_double_sha_from_midstate_and_tail, hashing::bzm2_tail16_bytes,
    hashing::check_result, hashing::leading_zero_bits, work::AssignedTask,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReadResultFields {
    sequence: u8,
    timecode: u8,
    sequence_id: u8,
    micro_job_id: u8,
    used_masked_fields: bool,
}

pub(super) struct SelectedReadResultCandidate {
    pub(super) assigned: AssignedTask,
    pub(super) sequence_id: u8,
    pub(super) micro_job_id: u8,
    pub(super) timecode_effective: u8,
    pub(super) slot_candidate_count: usize,
    pub(super) share_version: BlockVersion,
    pub(super) ntime_offset: u32,
    pub(super) share_ntime: u32,
    pub(super) nonce_adjusted: u32,
    pub(super) nonce_submit: u32,
    pub(super) hash_bytes: [u8; 32],
    pub(super) hash: bitcoin::BlockHash,
    pub(super) check_result: Bzm2CheckResult,
    pub(super) observed_leading_zeros: u16,
}

pub(super) struct AssignmentTracker {
    assignments: VecDeque<AssignedTask>,
    next_sequence_id: u8,
}

impl AssignmentTracker {
    pub(super) fn new() -> Self {
        Self {
            assignments: VecDeque::with_capacity(READRESULT_ASSIGNMENT_HISTORY_LIMIT),
            next_sequence_id: 0,
        }
    }

    pub(super) fn current_write_sequence_id(&self) -> u8 {
        writejob_effective_sequence_id(self.next_sequence_id)
    }

    pub(super) fn retain(&mut self, new_task: AssignedTask) {
        let slot = readresult_sequence_slot(new_task.sequence_id);
        self.assignments.push_back(new_task);

        let mut slot_count = self
            .assignments
            .iter()
            .filter(|task| readresult_sequence_slot(task.sequence_id) == slot)
            .count();
        while slot_count > READRESULT_SLOT_HISTORY {
            if let Some(index) = self
                .assignments
                .iter()
                .position(|task| readresult_sequence_slot(task.sequence_id) == slot)
            {
                let _ = self.assignments.remove(index);
                slot_count = slot_count.saturating_sub(1);
            } else {
                break;
            }
        }

        while self.assignments.len() > READRESULT_ASSIGNMENT_HISTORY_LIMIT {
            let _ = self.assignments.pop_front();
        }
    }

    pub(super) fn advance_sequence(&mut self) {
        self.next_sequence_id = self.next_sequence_id.wrapping_add(1);
    }

    pub(super) fn clear(&mut self) {
        self.assignments.clear();
    }

    pub(super) fn resolve_candidate(
        &self,
        logical_engine_id: usize,
        sequence: u8,
        timecode: u8,
        nonce_raw: u32,
    ) -> Option<SelectedReadResultCandidate> {
        let resolved_fields = resolve_readresult_fields(sequence, timecode, |slot| {
            self.assignments
                .iter()
                .rev()
                .any(|task| readresult_sequence_slot(task.sequence_id) == slot)
        })?;
        let sequence_id = resolved_fields.sequence_id;
        let micro_job_id = resolved_fields.micro_job_id;
        let timecode_effective = resolved_fields.timecode;
        let sequence_slot = readresult_sequence_slot(sequence_id);
        let slot_candidates: Vec<AssignedTask> = self
            .assignments
            .iter()
            .rev()
            .filter(|task| readresult_sequence_slot(task.sequence_id) == sequence_slot)
            .cloned()
            .collect();
        let slot_candidate_count = slot_candidates.len();
        if slot_candidate_count == 0 {
            return None;
        }

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
            let ntime_offset =
                u32::from(candidate.timestamp_count.wrapping_sub(timecode_effective));
            let share_ntime = candidate.task.ntime.wrapping_add(ntime_offset);
            let nonce_adjusted = nonce_raw.wrapping_sub(candidate.nonce_minus_value);
            let nonce_submit = nonce_adjusted.swap_bytes();

            let tail16 = bzm2_tail16_bytes(&candidate, share_ntime, nonce_submit);
            let hash_bytes = bzm2_double_sha_from_midstate_and_tail(&selected_midstate, &tail16);
            let hash = bitcoin::BlockHash::from_byte_array(hash_bytes);
            let target_bytes = candidate.task.share_target.to_le_bytes();
            let check_result = check_result(&hash_bytes, &target_bytes, candidate.leading_zeros);
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
                });
                if rank == 3 {
                    break;
                }
            }
        }

        selected_candidate
    }
}

fn readresult_sequence_slot(sequence_id: u8) -> u8 {
    sequence_id & 0x3f
}

fn writejob_effective_sequence_id(sequence_id: u8) -> u8 {
    sequence_id % 2
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

#[cfg(test)]
mod tests {
    use super::resolve_readresult_fields;

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
}
