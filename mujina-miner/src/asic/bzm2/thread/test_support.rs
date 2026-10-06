use std::sync::Arc;
use std::time::Duration;

use crate::asic::hash_thread::HashTask;
use crate::job_source::{GeneralPurposeBits, MerkleRootKind};

use super::*;

use crate::job_source::{JobTemplate, VersionTemplate};
use bitcoin::hashes::Hash;
use bitcoin::pow::Target;
use tokio::sync::mpsc as tokio_mpsc;

/// An interlock with a fresh, comfortable reading. Tests that exercise
/// dispatch should not also be exercising the interlock; the interlock has
/// its own tests.
pub(super) fn satisfied_interlock() -> ThermalInterlock {
    let mut i = ThermalInterlock::new(DEFAULT_THERMAL_CEILING_C, Duration::from_secs(30));
    i.observe(0, 45.0);
    i
}

pub(super) fn test_task() -> HashTask {
    let share_target = Target::from_be_bytes([
        0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff,
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

pub(super) fn hex_32(s: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap();
    }
    out
}

/// The job behind a PUBLIC captured BZM2 hardware result:
/// johnny9/ESP-Miner-Bonanza `components/asic/test/test_bzm.c`, "BZM 1002
/// nonce gap reproduces captured Stage 7 hardware proof". Header bytes are
/// as that test copies them into the header. Its expected nonce was
/// re-derived here independently, by double-SHA of the header built from
/// these bytes: 34 leading zero bits.
pub(super) fn public_vector_task() -> (HashTask, bitcoin::TxMerkleNode) {
    let merkle_root = bitcoin::TxMerkleNode::from_byte_array(hex_32(
        "24f9e61126a07d32f112cfeb01955e0067e577088d6f1b97f1dba64f6c96a113",
    ));
    let mut task = test_task();
    task.template = Arc::new(JobTemplate {
        id: "public-hardware-vector".into(),
        prev_blockhash: bitcoin::BlockHash::from_byte_array(hex_32(
            "a3617ee3ae390721613cf7d57a0a2c505e2b873f208701000000000000000000",
        )),
        version: VersionTemplate::new(
            bitcoin::block::Version::from_consensus(0x2000_0000),
            GeneralPurposeBits::full(),
        )
        .unwrap(),
        bits: bitcoin::pow::CompactTarget::from_consensus(0x1702_369d),
        share_target: task.share_target,
        time: 0x6a5d_cd19,
        merkle_root: MerkleRootKind::Fixed(merkle_root),
    });
    task.ntime = 0x6a5d_cd19;
    (task, merkle_root)
}
