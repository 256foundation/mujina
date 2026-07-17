use bitcoin::{
    TxMerkleNode,
    block::{Header as BlockHeader, Version as BlockVersion},
    consensus,
    hashes::{Hash as _, HashEngine as _, sha256},
};

use crate::asic::hash_thread::{HashTask, HashThreadError};

use super::{AssignedTask, MIDSTATE_COUNT};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Bzm2CheckResult {
    Correct,
    NotMeetTarget,
    Error,
}

pub(super) fn midstate_version_mask_variants(version_mask: u32) -> [u32; MIDSTATE_COUNT] {
    if version_mask == 0 {
        return [0, 0, 0, 0];
    }

    let mut mask = version_mask;
    let mut cnt: u32 = 0;
    while mask.is_multiple_of(16) {
        cnt = cnt.saturating_add(1);
        mask /= 16;
    }

    let mut tmp_mask = 0u32;
    if !mask.is_multiple_of(16) {
        tmp_mask = mask % 16;
    } else if !mask.is_multiple_of(8) {
        tmp_mask = mask % 8;
    } else if !mask.is_multiple_of(4) {
        tmp_mask = mask % 4;
    } else if !mask.is_multiple_of(2) {
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

pub(super) fn task_midstate_versions(task: &HashTask) -> [BlockVersion; MIDSTATE_COUNT] {
    let template = task.template.as_ref();
    let base = template.version.base().to_consensus() as u32;
    let gp_mask = u16::from_be_bytes(*template.version.gp_bits_mask().as_bytes()) as u32;
    let version_mask = gp_mask << 13;
    let variants = midstate_version_mask_variants(version_mask);

    variants.map(|variant| BlockVersion::from_consensus((base | variant) as i32))
}

pub(super) fn check_result(
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

pub(super) fn leading_zero_bits(sha256_le: &[u8; 32]) -> u16 {
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

fn bzm2_midstate_to_sha256_midstate(midstate_le: &[u8; 32]) -> sha256::Midstate {
    let mut midstate_be = [0u8; 32];
    for (be_chunk, le_chunk) in midstate_be
        .chunks_exact_mut(4)
        .zip(midstate_le.chunks_exact(4))
    {
        let word = u32::from_le_bytes(le_chunk.try_into().expect("chunk size is 4"));
        be_chunk.copy_from_slice(&word.to_be_bytes());
    }

    sha256::Midstate::from_byte_array(midstate_be)
}

fn sha256_midstate_to_bzm2_le(midstate: sha256::Midstate) -> [u8; 32] {
    let midstate_be = midstate.to_byte_array();
    let mut midstate_le = [0u8; 32];
    for (le_chunk, be_chunk) in midstate_le
        .chunks_exact_mut(4)
        .zip(midstate_be.chunks_exact(4))
    {
        let word = u32::from_be_bytes(be_chunk.try_into().expect("chunk size is 4"));
        le_chunk.copy_from_slice(&word.to_le_bytes());
    }

    midstate_le
}

pub(super) fn bzm2_double_sha_from_midstate_and_tail(
    midstate_le: &[u8; 32],
    tail16: &[u8; 16],
) -> [u8; 32] {
    let mut engine =
        sha256::HashEngine::from_midstate(bzm2_midstate_to_sha256_midstate(midstate_le), 64);
    engine.input(tail16);

    sha256::Hash::from_engine(engine)
        .hash_again()
        .to_byte_array()
}

pub(super) fn bzm2_tail16_bytes(
    assigned: &AssignedTask,
    ntime: u32,
    nonce_submit: u32,
) -> [u8; 16] {
    let merkle_root_bytes = consensus::serialize(&assigned.merkle_root);
    let mut tail16 = [0u8; 16];
    tail16[0..4].copy_from_slice(&merkle_root_bytes[28..32]);
    tail16[4..8].copy_from_slice(&ntime.to_le_bytes());
    tail16[8..12].copy_from_slice(&assigned.task.template.bits.to_consensus().to_le_bytes());
    tail16[12..16].copy_from_slice(&nonce_submit.to_le_bytes());
    tail16
}

pub(super) fn build_header_bytes(
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
        HashThreadError::WorkAssignmentFailed(format!("unexpected serialized header size: {len}"))
    })
}

pub(super) fn compute_midstate_le(header_prefix_64: &[u8; 64]) -> [u8; 32] {
    let mut engine = sha256::HashEngine::default();
    engine.input(header_prefix_64);
    sha256_midstate_to_bzm2_le(engine.midstate())
}

#[cfg(test)]
pub(super) fn hash_bytes_bzm2_order(hash: &bitcoin::BlockHash) -> [u8; 32] {
    *hash.as_byte_array()
}

#[cfg(test)]
mod tests {
    use bitcoin::hashes::{Hash as _, HashEngine as _, sha256d};

    use super::{
        Bzm2CheckResult, bzm2_double_sha_from_midstate_and_tail,
        bzm2_midstate_to_sha256_midstate, check_result, compute_midstate_le,
        hash_bytes_bzm2_order, midstate_version_mask_variants, sha256_midstate_to_bzm2_le,
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
    fn test_check_result_leading_zeros_error() {
        let mut hash = [0u8; 32];
        let target = [0xffu8; 32];
        hash[31] = 0x80;
        assert_eq!(check_result(&hash, &target, 32), Bzm2CheckResult::Error);
    }

    #[test]
    fn test_check_result_accepts_required_leading_zeros() {
        let mut hash = [0u8; 32];
        let target = [0xffu8; 32];
        hash[27] = 0x3f;
        assert_eq!(check_result(&hash, &target, 34), Bzm2CheckResult::Correct);
    }

    #[test]
    fn test_check_result_rejects_missing_partial_zero_bits() {
        let mut hash = [0u8; 32];
        let target = [0xffu8; 32];
        hash[27] = 0x40;
        assert_eq!(check_result(&hash, &target, 34), Bzm2CheckResult::Error);
    }

    #[test]
    fn test_check_result_target_compare() {
        let mut hash = [0u8; 32];
        let mut target = [0u8; 32];

        hash[1] = 0x10;
        target[1] = 0x20;
        assert_eq!(check_result(&hash, &target, 32), Bzm2CheckResult::Correct);

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
    fn test_midstate_conversion_round_trip_preserves_words() {
        let sha256_midstate = bitcoin::hashes::sha256::Midstate::from_byte_array(
            core::array::from_fn(|i| i as u8),
        );
        let bzm2_midstate = sha256_midstate_to_bzm2_le(sha256_midstate);

        assert_eq!(
            bzm2_midstate_to_sha256_midstate(&bzm2_midstate).to_byte_array(),
            sha256_midstate.to_byte_array()
        );
    }

    #[test]
    fn test_compute_midstate_le_matches_bitcoin_sha256_engine() {
        let header_prefix = core::array::from_fn(|i| i as u8);
        let mut engine = bitcoin::hashes::sha256::HashEngine::default();
        engine.input(&header_prefix);

        assert_eq!(
            compute_midstate_le(&header_prefix),
            sha256_midstate_to_bzm2_le(engine.midstate())
        );
    }

    #[test]
    fn test_bzm2_double_sha_matches_bitcoin_double_sha_for_full_header() {
        let header_bytes: [u8; 80] = core::array::from_fn(|i| i as u8);
        let header_prefix: [u8; 64] = header_bytes[..64]
            .try_into()
            .expect("header prefix must be 64 bytes");
        let header_tail: [u8; 16] = header_bytes[64..]
            .try_into()
            .expect("header tail must be 16 bytes");

        assert_eq!(
            bzm2_double_sha_from_midstate_and_tail(
                &compute_midstate_le(&header_prefix),
                &header_tail,
            ),
            sha256d::Hash::hash(&header_bytes).to_byte_array()
        );
    }

    #[test]
    fn test_bzm2_double_sha_matches_known_trace_sample() {
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
}
