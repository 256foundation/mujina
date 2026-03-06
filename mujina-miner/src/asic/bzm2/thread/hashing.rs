#[cfg(test)]
use bitcoin::hashes::Hash as _;
use bitcoin::{
    TxMerkleNode,
    block::{Header as BlockHeader, Version as BlockVersion},
    consensus,
};

use crate::asic::hash_thread::{HashTask, HashThreadError};

use super::{AssignedTask, MIDSTATE_COUNT};

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

pub(super) fn bzm2_double_sha_from_midstate_and_tail(
    midstate_le: &[u8; 32],
    tail16: &[u8; 16],
) -> [u8; 32] {
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

#[cfg(test)]
pub(super) fn hash_bytes_bzm2_order(hash: &bitcoin::BlockHash) -> [u8; 32] {
    *hash.as_byte_array()
}

#[cfg(test)]
mod tests {
    use bitcoin::hashes::Hash as _;

    use super::{
        Bzm2CheckResult, bzm2_double_sha_from_midstate_and_tail, check_result,
        hash_bytes_bzm2_order, midstate_version_mask_variants,
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
