use std::sync::{Arc, RwLock};
use std::time::Instant;

use bitcoin::block::Header as BlockHeader;
use tokio::sync::mpsc;

use crate::asic::hash_thread::{HashThreadEvent, HashThreadStatus, Share};
use crate::types::{Difficulty, LogVerdict};

use super::super::protocol::{self, Bzm2EngineLayout};
use super::dispatch::*;
use super::metrics::*;
use super::*;

/// Where a result came from: `(asic id, logical engine id)`.
///
/// The engine id alone is *not* unique across a chain — every ASIC has the same
/// engine grid. While all chips receive byte-identical work that is harmless,
/// but the moment work differs per ASIC a returned nonce would be attributed to
/// whichever chip's dispatch happened to occupy that engine slot, and validated
/// against the wrong midstate. So a dispatch records which chips it reached,
/// and a result is resolved by the chip that reported it.
type EngineDispatchKey = (u8, u16);

pub(super) async fn handle_result_frame(
    frame: &protocol::TdmResultFrame,
    engine_dispatches: &DispatchRing,
    engine_layout: &Bzm2EngineLayout,
    config: &Bzm2ThreadConfig,
    status: &Arc<RwLock<HashThreadStatus>>,
    event_tx: &mpsc::Sender<HashThreadEvent>,
    runtime_measurements: &mut ThreadRuntimeMeasurementState,
) {
    // Decode first, filter second, and log the decoded result either way.
    // The chip reports an engine address, a sequence id, a sweep index and
    // an offset nonce; version, ntime and the nonce the header actually
    // carries are all host-derived. Logging the wire values next to what
    // was derived from them is what lets a later analysis check the
    // derivation instead of trusting it.
    let decoded = match decode_result(frame, engine_dispatches, engine_layout, config) {
        Ok(decoded) => decoded,
        Err(reason) => {
            runtime_measurements.record_result(frame.asic, Err(reason));
            // Budgeted: this is a per-result-frame record, so it scales with
            // wire traffic rather than with how much is wrong. The counters in
            // `record_result` above are the home for "how many, and why"; this
            // record exists to show the SHAPE of a discard, and a handful does
            // that as well as a quarter of a million.
            match runtime_measurements.discard_log.admit(Instant::now()) {
                LogVerdict::Emit => debug!(
                    asic = frame.asic,
                    engine_address = format!("{:#05x}", frame.engine_address),
                    row = frame.row(),
                    col = frame.col(),
                    status = format!("{:#04x}", frame.status),
                    seq = frame.sequence_id,
                    reported_time = frame.reported_time,
                    wire_nonce = format!("{:#010x}", frame.nonce),
                    reason = %reason,
                    "BZM2 result discarded"
                ),
                LogVerdict::Summarise {
                    folded,
                    span,
                    total,
                } => debug!(
                    folded,
                    span_ms = span.as_millis(),
                    total,
                    "BZM2 results continue to be discarded; individual records folded. \
                     Per-ASIC counts and reasons are in the runtime measurements."
                ),
                LogVerdict::Count => {}
            }
            return;
        }
    };

    let (dispatch, work) = engine_dispatches
        .resolve(decoded.asic, decoded.engine_id, decoded.dispatch_tag)
        .expect("dispatch must exist for a decoded result");
    let share_tx = dispatch.task.share_tx.clone();
    let acceptance_target = acceptance_target(dispatch.task.share_target, config);
    let outcome = accept_decoded(&decoded, dispatch, work, config);

    // Budgeted. This is one record per ACCEPTED result, with about fifteen
    // fields, so unlike the discard above it scales with how well the miner is
    // working -- it is the flood that arrives when bring-up succeeds rather
    // than when it fails. The share itself is reported through the share
    // channel and counted in the runtime measurements; this record exists to
    // let a later analysis check the nonce derivation against the wire values,
    // and a bounded sample does that as well as every result does.
    let accepted_verdict = runtime_measurements.accepted_log.admit(Instant::now());
    if let LogVerdict::Summarise {
        folded,
        span,
        total,
    } = accepted_verdict
    {
        debug!(
            folded,
            span_ms = span.as_millis(),
            total,
            "BZM2 results accepted; individual records folded"
        );
    }
    if accepted_verdict == LogVerdict::Emit {
        debug!(
            asic = decoded.asic,
            engine_address = format!("{:#05x}", decoded.engine_address),
            engine_id = decoded.engine_id,
            seq = decoded.sequence_id,
            micro_job = decoded.micro_job_id,
            reported_time = decoded.reported_time,
            timestamp_count = decoded.timestamp_count,
            wire_nonce = format!("{:#010x}", decoded.wire_nonce),
            nonce_gap = format!("{:#x}", decoded.nonce_gap),
            nonce = format!("{:#010x}", decoded.nonce),
            version = format!("{:#010x}", decoded.version.to_consensus()),
            ntime = decoded.ntime,
            hash = %decoded.hash,
            hash_diff = %Difficulty::from_hash(&decoded.hash),
            target_diff = %Difficulty::from_target(acceptance_target),
            accepted = outcome.is_ok(),
            "BZM2 result"
        );
    }

    let (share, _accepted_at) = match outcome {
        Ok(accepted) => accepted,
        Err(reason) => {
            runtime_measurements.record_result(frame.asic, Err(reason));
            return;
        }
    };
    runtime_measurements.record_result(frame.asic, Ok(()));

    runtime_measurements.record_at(Instant::now(), frame.asic, frame.row(), share.expected_work);
    refresh_status_hashrate(
        status,
        runtime_measurements,
        config.expected_chain_hashrate_ths(),
    );

    if share_tx.send(share).await.is_ok() {
        let snapshot = {
            let mut lock = status.write().unwrap();
            lock.chip_shares_found += 1;
            lock.clone()
        };
        let _ = event_tx.send(HashThreadEvent::StatusUpdate(snapshot)).await;
    }
}

/// One result frame after reconstruction: the wire values verbatim, the
/// reconstruction parameters that were in force, and everything the host
/// derived from the two. Produced before the target filter, so a result
/// that misses the target is still fully described.
#[derive(Debug, Clone, Copy)]
pub struct DecodedResult {
    // Wire values, verbatim.
    pub asic: u8,
    pub engine_address: u16,
    pub sequence_id: u8,
    pub reported_time: u8,
    pub wire_nonce: u32,
    // Parameters in force when this frame was decoded.
    pub nonce_gap: u32,
    pub timestamp_count: u8,
    // Host-derived.
    pub engine_id: u16,
    pub micro_job_id: usize,
    pub version: bitcoin::block::Version,
    pub ntime: u32,
    pub nonce: u32,
    pub hash: bitcoin::BlockHash,
    /// Which dispatch the result was resolved to: the sequence byte's tag.
    pub dispatch_tag: u8,
    pub key: EngineDispatchKey,
}

/// Resolve a result frame against the dispatch records and rebuild the
/// header it must have come from. No target check: that is a policy applied
/// afterwards, and keeping it out of here is what makes below-target
/// results observable.
fn decode_result(
    frame: &protocol::TdmResultFrame,
    engine_dispatches: &DispatchRing,
    engine_layout: &Bzm2EngineLayout,
    config: &Bzm2ThreadConfig,
) -> Result<DecodedResult, ResultDiscard> {
    if !frame.nonce_valid() {
        return Err(ResultDiscard::InvalidNonce);
    }

    let engine_id = engine_layout
        .logical_engine_id(frame.row(), frame.col())
        .ok_or(ResultDiscard::UnknownEngine)?;
    // Resolved by the reporting ASIC, not the engine alone: the same engine
    // address exists on every chip in the chain. And by the result's own
    // tag, not by whichever dispatch is newest: the hit may be from one the
    // newest has replaced.
    let key: EngineDispatchKey = (frame.asic, engine_id);
    let dispatch_tag = frame.sequence_id >> 2;
    let (dispatch, work) = engine_dispatches.resolve(frame.asic, engine_id, dispatch_tag)?;

    let micro_job_id = (frame.sequence_id & 0x3) as usize;
    let version = dispatch.versions[micro_job_id];
    let ntime_offset = u32::from(config.timestamp_count.saturating_sub(frame.reported_time));
    let ntime = dispatch.task.ntime.wrapping_add(ntime_offset);
    // The chip reports the nonce BYTE-SWAPPED and `nonce_gap` past the one it
    // found. Decoding without the swap turned every real result into a header
    // with ~1 leading zero bit (see
    // `a_public_hardware_result_decodes_to_the_nonce_that_solved_it`).
    let nonce = frame.nonce.wrapping_sub(config.nonce_gap).swap_bytes();

    let header = BlockHeader {
        version,
        prev_blockhash: dispatch.task.template.prev_blockhash,
        merkle_root: work.merkle_root,
        time: ntime,
        bits: dispatch.task.template.bits,
        nonce,
    };

    Ok(DecodedResult {
        asic: frame.asic,
        engine_address: frame.engine_address,
        sequence_id: frame.sequence_id,
        reported_time: frame.reported_time,
        wire_nonce: frame.nonce,
        nonce_gap: config.nonce_gap,
        timestamp_count: config.timestamp_count,
        engine_id,
        micro_job_id,
        version,
        ntime,
        nonce,
        hash: header.block_hash(),
        dispatch_tag,
        key,
    })
}

/// The target a decoded result must meet to be forwarded as a share: the
/// task's share target, relaxed to the configured difficulty floor when
/// that floor is easier. A floor harder than the task target changes
/// nothing, so it can never hide shares the scheduler asked for.
fn acceptance_target(
    task_share_target: bitcoin::pow::Target,
    config: &Bzm2ThreadConfig,
) -> bitcoin::pow::Target {
    match config.result_min_difficulty {
        // A larger target is an easier one.
        Some(floor) => task_share_target.max(floor.to_target()),
        None => task_share_target,
    }
}

/// Apply the acceptance target to a decoded result, yielding the share to
/// forward and the difficulty it was accepted at. `expected_work` is the
/// work of the target actually applied, so a relaxed floor cannot inflate
/// the hashrate estimators downstream.
fn accept_decoded(
    decoded: &DecodedResult,
    dispatch: &Dispatch,
    work: &EngineWork,
    config: &Bzm2ThreadConfig,
) -> Result<(Share, Difficulty), ResultDiscard> {
    let target = acceptance_target(dispatch.task.share_target, config);
    if !target.is_met_by(decoded.hash) {
        return Err(ResultDiscard::BelowTarget);
    }

    Ok((
        Share {
            nonce: decoded.nonce,
            hash: decoded.hash,
            version: decoded.version,
            ntime: decoded.ntime,
            extranonce2: work.en2,
            expected_work: target.to_work(),
        },
        Difficulty::from_target(target),
    ))
}

/// Decode a result frame and apply the acceptance target, yielding the share
/// to forward, the difficulty it was accepted at, and its dispatch key.
///
/// The live path in `handle_result_frame` does the same two steps itself so
/// it can log the decoded result before the target is applied; this
/// composition exists for tests that only care about the outcome.
#[cfg(test)]
fn reconstruct_share_from_result(
    frame: &protocol::TdmResultFrame,
    engine_dispatches: &DispatchRing,
    engine_layout: &Bzm2EngineLayout,
    config: &Bzm2ThreadConfig,
) -> Result<(Share, Difficulty, EngineDispatchKey), ResultDiscard> {
    let decoded = decode_result(frame, engine_dispatches, engine_layout, config)?;
    let (dispatch, work) = engine_dispatches
        .resolve(decoded.asic, decoded.engine_id, decoded.dispatch_tag)
        .expect("dispatch must exist for a decoded result");
    let (share, accepted_at) = accept_decoded(&decoded, dispatch, work, config)?;
    Ok((share, accepted_at, decoded.key))
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::super::protocol;
    use super::super::engine::EngineGate;
    use super::super::test_support::*;
    use super::*;
    use crate::job_source::{GeneralPurposeBits, JobTemplate, MerkleRootKind, VersionTemplate};
    use crate::transport::{SerialConfig, SerialStream};
    use bitcoin::hashes::Hash;
    use bitcoin::pow::Target;
    use nix::pty::openpty;
    use std::os::unix::io::IntoRawFd;
    use tokio::io::AsyncReadExt;
    use tokio::sync::mpsc as tokio_mpsc;
    fn leading_zero_bits(hash: &bitcoin::BlockHash) -> u32 {
        let mut n = 0;
        for byte in hash.to_byte_array().iter().rev() {
            if *byte == 0 {
                n += 8;
            } else {
                return n + byte.leading_zeros();
            }
        }
        n
    }

    /// A REAL hardware result must decode to the nonce that solved it.
    ///
    /// Mujina's decode subtracted a gap of 0x28 and used the nonce as it came
    /// off the wire. On this public frame that yields 0x2d6dac5c, whose header
    /// hash has 1 leading zero bit: nothing Mujina reconstructed from real
    /// silicon could ever have been a share. The chip reports the nonce
    /// byte-swapped and 0x4c past the one it found (in the all-TCE enhanced
    /// mode that the engine Config write selects). Replaying our own capture
    /// of the vendor stack mining, captured on hardware,
    /// under this rule validates 84,586 of 119,598 result frames; under the
    /// old rule, none.
    #[test]
    fn a_public_hardware_result_decodes_to_the_nonce_that_solved_it() {
        let (task, merkle_root) = public_vector_task();
        let engine_layout = Bzm2EngineLayout::default();
        let (row, col) = protocol::default_engine_coordinates()[0];
        let engine_id = engine_layout.logical_engine_id(row, col).unwrap();
        let version = bitcoin::block::Version::from_consensus(0x3fff_0000);
        let engine_dispatches = one_dispatch(&[0], engine_id, task, merkle_root, [version; 4], 0);
        let frame = protocol::TdmResultFrame {
            asic: 0,
            engine_address: protocol::logical_engine_address(row, col),
            status: 0x8,
            nonce: 0x2d6d_ac84,
            sequence_id: 0,
            reported_time: DEFAULT_TIMESTAMP_COUNT - 4,
        };
        let config = Bzm2ThreadConfig::new("/dev/null".into(), 5_000_000, 55.0);
        let d = decode_result(&frame, &engine_dispatches, &engine_layout, &config).unwrap();

        assert_eq!(
            d.ntime, 0x6a5d_cd1d,
            "four timestamp ticks past the job's ntime"
        );
        assert_eq!(
            d.nonce, 0x38ac_6d2d,
            "wire 0x2d6dac84: minus the gap, byte-swapped"
        );
        assert!(
            leading_zero_bits(&d.hash) >= 32,
            "the decoded header must meet difficulty 1; it has {} leading zero bits",
            leading_zero_bits(&d.hash)
        );
    }

    #[tokio::test]
    async fn parsed_uart_frame_emits_share_and_status_event() {
        let mut task = test_task();
        let merkle_root = bitcoin::TxMerkleNode::all_zeros();
        let versions = compute_micro_versions(&task);
        let engine_layout = Bzm2EngineLayout::default();
        let (row, col) = protocol::default_engine_coordinates()[0];
        let engine_id = engine_layout.logical_engine_id(row, col).unwrap();
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

        let mut engine_dispatches =
            one_dispatch(&[0], engine_id, task.clone(), merkle_root, versions, 0);

        let config = Bzm2ThreadConfig::new("/dev/null".into(), 5_000_000, 55.0);
        let status = Arc::new(RwLock::new(HashThreadStatus {
            hashrate: HashRate::from_terahashes(40.0),
            is_active: true,
            ..Default::default()
        }));
        let mut runtime_measurements = ThreadRuntimeMeasurementState::new();
        let (event_tx, mut event_rx) = tokio_mpsc::channel(4);
        let (share_tx, mut share_rx) = tokio_mpsc::channel(4);
        engine_dispatches.newest_mut().task.share_tx = share_tx;

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
        assert!(
            reconstruct_share_from_result(&frames[0], &engine_dispatches, &engine_layout, &config)
                .is_ok()
        );

        handle_result_frame(
            &frames[0],
            &engine_dispatches,
            &engine_layout,
            &config,
            &status,
            &event_tx,
            &mut runtime_measurements,
        )
        .await;

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
                assert!(u64::from(snapshot.hashrate) > 0);
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn reconstructs_share_from_matching_result_frame() {
        let mut task = test_task();

        let merkle_root = bitcoin::TxMerkleNode::all_zeros();
        let versions = compute_micro_versions(&task);
        let engine_layout = Bzm2EngineLayout::default();
        let (row, col) = protocol::default_engine_coordinates()[0];
        let engine_id = engine_layout.logical_engine_id(row, col).unwrap();
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

        let engine_dispatches =
            one_dispatch(&[0], engine_id, task.clone(), merkle_root, versions, 0);

        let config = Bzm2ThreadConfig::new("/dev/null".into(), 5_000_000, 55.0);
        let (share, target_diff, reconstructed_key) =
            reconstruct_share_from_result(&frame, &engine_dispatches, &engine_layout, &config)
                .unwrap();

        assert_eq!(reconstructed_key, (0u8, engine_id));
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

    /// Two chips holding *different* work at the *same* engine address must not
    /// be confused. Keyed by engine id alone this reconstructs against whichever
    /// dispatch happens to occupy the slot, validating a nonce from ASIC 1
    /// against ASIC 0's midstate.
    #[test]
    fn result_frames_route_by_reporting_asic_not_engine_alone() {
        let engine_layout = Bzm2EngineLayout::default();
        let (row, col) = protocol::default_engine_coordinates()[0];
        let engine_id = engine_layout.logical_engine_id(row, col).unwrap();
        let nonce = 0;

        // Same engine, same everything except the merkle root - which is what
        // makes the two chips' work distinguishable in the resulting hash.
        let merkle_a = bitcoin::TxMerkleNode::all_zeros();
        let merkle_b = bitcoin::TxMerkleNode::from_byte_array([0x11u8; 32]);

        let mut task_a = test_task();
        let mut task_b = test_task();
        let versions = compute_micro_versions(&task_a);

        let header_hash = |merkle_root, task: &HashTask| {
            bitcoin::block::Header {
                version: versions[0],
                prev_blockhash: task.template.prev_blockhash,
                merkle_root,
                time: task.ntime,
                bits: task.template.bits,
                nonce,
            }
            .block_hash()
        };
        let hash_a = header_hash(merkle_a, &task_a);
        let hash_b = header_hash(merkle_b, &task_b);
        assert_ne!(hash_a, hash_b, "test setup must make the two jobs differ");
        task_a.share_target = Difficulty::from_hash(&hash_a).to_target();
        task_b.share_target = Difficulty::from_hash(&hash_b).to_target();

        // Two dispatches under one tag, each unicast to one chip.
        let mut engine_dispatches = DispatchRing::default();
        engine_dispatches.begin(0, &task_a, versions, AsicSet::of(&[0]), 1);
        engine_dispatches.record_engine(
            engine_id,
            EngineWork {
                en2: task_a.en2,
                merkle_root: merkle_a,
            },
        );
        engine_dispatches.begin(0, &task_b, versions, AsicSet::of(&[1]), 1);
        engine_dispatches.record_engine(
            engine_id,
            EngineWork {
                en2: task_b.en2,
                merkle_root: merkle_b,
            },
        );

        // ASIC 1 reports the result.
        let frame = protocol::TdmResultFrame {
            asic: 1,
            engine_address: protocol::logical_engine_address(row, col),
            status: 0x8,
            nonce: nonce + DEFAULT_NONCE_GAP,
            sequence_id: 0,
            reported_time: DEFAULT_TIMESTAMP_COUNT,
        };

        let config = Bzm2ThreadConfig::new("/dev/null".into(), 5_000_000, 55.0);
        let (share, _target_diff, key) =
            reconstruct_share_from_result(&frame, &engine_dispatches, &engine_layout, &config)
                .expect("ASIC 1's result should reconstruct against ASIC 1's dispatch");

        assert_eq!(key, (1u8, engine_id), "result routed to the wrong ASIC");
        assert_eq!(
            share.hash, hash_b,
            "share was validated against the wrong chip's job"
        );

        // The same engine address reported by ASIC 0 must resolve to the other
        // job. Checking both directions is what makes this a real test of the
        // routing rather than of one lucky map ordering.
        let frame_from_a = protocol::TdmResultFrame { asic: 0, ..frame };
        let (share_a, _, key_a) = reconstruct_share_from_result(
            &frame_from_a,
            &engine_dispatches,
            &engine_layout,
            &config,
        )
        .expect("ASIC 0's result should reconstruct against ASIC 0's dispatch");
        assert_eq!(key_a, (0u8, engine_id));
        assert_eq!(
            share_a.hash, hash_a,
            "identical engine address on a different chip resolved to the wrong job"
        );
    }

    /// Records as one dispatch leaves them: `task` sent under `tag` to
    /// `asics`, with engine `engine_id` given `merkle_root` and the task's
    /// own extranonce2.
    fn one_dispatch(
        asics: &[u8],
        engine_id: u16,
        task: HashTask,
        merkle_root: bitcoin::TxMerkleNode,
        versions: [bitcoin::block::Version; 4],
        tag: u8,
    ) -> DispatchRing {
        let mut ring = DispatchRing::default();
        ring.begin(tag, &task, versions, AsicSet::of(asics), 1);
        ring.record_engine(
            engine_id,
            EngineWork {
                en2: task.en2,
                merkle_root,
            },
        );
        ring
    }

    /// A dispatched job on ASIC 0 at the first default engine, with the task
    /// share target set so that nonce 0 in micro-job 0 at base ntime meets
    /// it exactly. Returns everything a result-frame test needs.
    struct ResultFixture {
        engine_layout: Bzm2EngineLayout,
        engine_dispatches: DispatchRing,
        engine_address: u16,
        expected_hash: bitcoin::BlockHash,
    }

    fn result_fixture() -> ResultFixture {
        let mut task = test_task();
        let merkle_root = bitcoin::TxMerkleNode::all_zeros();
        let versions = compute_micro_versions(&task);
        let engine_layout = Bzm2EngineLayout::default();
        let (row, col) = protocol::default_engine_coordinates()[0];
        let engine_id = engine_layout.logical_engine_id(row, col).unwrap();
        let expected_hash = bitcoin::block::Header {
            version: versions[0],
            prev_blockhash: task.template.prev_blockhash,
            merkle_root,
            time: task.ntime,
            bits: task.template.bits,
            nonce: 0,
        }
        .block_hash();
        task.share_target = Difficulty::from_hash(&expected_hash).to_target();

        let engine_dispatches = one_dispatch(&[0], engine_id, task, merkle_root, versions, 0);

        ResultFixture {
            engine_layout,
            engine_dispatches,
            engine_address: protocol::logical_engine_address(row, col),
            expected_hash,
        }
    }

    /// The frame the chip would return for the fixture's winning nonce.
    fn winning_frame(fixture: &ResultFixture) -> protocol::TdmResultFrame {
        protocol::TdmResultFrame {
            asic: 0,
            engine_address: fixture.engine_address,
            status: 0x8,
            nonce: DEFAULT_NONCE_GAP,
            sequence_id: 0,
            reported_time: DEFAULT_TIMESTAMP_COUNT,
        }
    }

    #[test]
    fn decoded_result_carries_wire_values_and_parameters_in_force() {
        let fixture = result_fixture();
        let mut config = Bzm2ThreadConfig::new("/dev/null".into(), 5_000_000, 55.0);
        config.nonce_gap = 0x30;
        config.timestamp_count = 50;
        let frame = protocol::TdmResultFrame {
            nonce: 0x1234_5678,
            sequence_id: 2,
            reported_time: 47,
            ..winning_frame(&fixture)
        };

        let decoded = decode_result(
            &frame,
            &fixture.engine_dispatches,
            &fixture.engine_layout,
            &config,
        )
        .unwrap();

        // Wire values pass through untouched.
        assert_eq!(decoded.wire_nonce, 0x1234_5678);
        assert_eq!(decoded.sequence_id, 2);
        assert_eq!(decoded.reported_time, 47);
        assert_eq!(decoded.engine_address, fixture.engine_address);
        // The parameters recorded are the ones actually used.
        assert_eq!(decoded.nonce_gap, 0x30);
        assert_eq!(decoded.timestamp_count, 50);
        // And the derivation is exactly wire minus gap, base plus (count - reported).
        // Minus the gap in force, then byte-swapped: the chip reports it that way.
        assert_eq!(decoded.nonce, (0x1234_5678u32 - 0x30).swap_bytes());
        assert_eq!(decoded.ntime, test_task().ntime + 3);
        assert_eq!(decoded.micro_job_id, 2);
        let dispatch = fixture.engine_dispatches.newest();
        assert_eq!(decoded.version, dispatch.versions[2]);
    }

    #[test]
    fn result_with_invalid_nonce_status_is_discarded_as_invalid_nonce() {
        let fixture = result_fixture();
        let config = Bzm2ThreadConfig::new("/dev/null".into(), 5_000_000, 55.0);
        let frame = protocol::TdmResultFrame {
            status: 0,
            ..winning_frame(&fixture)
        };
        let outcome = decode_result(
            &frame,
            &fixture.engine_dispatches,
            &fixture.engine_layout,
            &config,
        );
        assert_eq!(outcome.unwrap_err(), ResultDiscard::InvalidNonce);
    }

    #[test]
    fn result_from_engine_outside_layout_is_discarded_as_unknown_engine() {
        let fixture = result_fixture();
        let config = Bzm2ThreadConfig::new("/dev/null".into(), 5_000_000, 55.0);
        let (row, col) = protocol::default_engine_coordinates()[0];
        // A layout that omits the reporting engine.
        let layout = Bzm2EngineLayout::from_active_coordinates(
            protocol::default_engine_coordinates()
                .into_iter()
                .filter(|&(r, c)| (r, c) != (row, col)),
        );
        let outcome = decode_result(
            &winning_frame(&fixture),
            &fixture.engine_dispatches,
            &layout,
            &config,
        );
        assert_eq!(outcome.unwrap_err(), ResultDiscard::UnknownEngine);
    }

    #[test]
    fn result_with_no_dispatch_record_is_discarded_as_no_dispatch() {
        let fixture = result_fixture();
        let config = Bzm2ThreadConfig::new("/dev/null".into(), 5_000_000, 55.0);
        // Same engine, but reported by a chip nothing was dispatched to.
        let frame = protocol::TdmResultFrame {
            asic: 7,
            ..winning_frame(&fixture)
        };
        let outcome = decode_result(
            &frame,
            &fixture.engine_dispatches,
            &fixture.engine_layout,
            &config,
        );
        assert_eq!(outcome.unwrap_err(), ResultDiscard::NoDispatch);
    }

    #[test]
    fn result_whose_tag_names_no_held_dispatch_is_discarded_as_stale() {
        let mut fixture = result_fixture();
        let config = Bzm2ThreadConfig::new("/dev/null".into(), 5_000_000, 55.0);
        // The engine holds a dispatch, under tag 1; the frame carries tag 0,
        // which no held dispatch has.
        fixture.engine_dispatches.newest_mut().tag = 1;
        let outcome = decode_result(
            &winning_frame(&fixture),
            &fixture.engine_dispatches,
            &fixture.engine_layout,
            &config,
        );
        assert_eq!(outcome.unwrap_err(), ResultDiscard::StaleSequence);
    }

    #[test]
    fn decoded_result_missing_target_is_discarded_as_below_target() {
        let mut fixture = result_fixture();
        let config = Bzm2ThreadConfig::new("/dev/null".into(), 5_000_000, 55.0);
        // No hash can meet a zero target.
        fixture.engine_dispatches.newest_mut().task.share_target = Target::ZERO;
        let outcome = reconstruct_share_from_result(
            &winning_frame(&fixture),
            &fixture.engine_dispatches,
            &fixture.engine_layout,
            &config,
        );
        assert_eq!(outcome.unwrap_err(), ResultDiscard::BelowTarget);
    }

    #[test]
    fn difficulty_floor_accepts_below_task_target_at_the_floor_work() {
        let mut fixture = result_fixture();
        fixture.engine_dispatches.newest_mut().task.share_target = Target::ZERO;
        let floor = Difficulty::from_hash(&fixture.expected_hash);
        let mut config = Bzm2ThreadConfig::new("/dev/null".into(), 5_000_000, 55.0);
        config.result_min_difficulty = Some(floor);

        let (share, accepted_at, _) = reconstruct_share_from_result(
            &winning_frame(&fixture),
            &fixture.engine_dispatches,
            &fixture.engine_layout,
            &config,
        )
        .expect("a result meeting the floor is forwarded");

        assert_eq!(accepted_at.to_target(), floor.to_target());
        assert_eq!(
            share.expected_work,
            floor.to_target().to_work(),
            "expected_work must be the floor's work, or the estimators over-count"
        );
    }

    #[test]
    fn difficulty_floor_harder_than_task_target_changes_nothing() {
        let fixture = result_fixture();
        let task_target = fixture.engine_dispatches.newest().task.share_target;
        let mut config = Bzm2ThreadConfig::new("/dev/null".into(), 5_000_000, 55.0);
        config.result_min_difficulty = Some(Difficulty::MAX);

        let (share, accepted_at, _) = reconstruct_share_from_result(
            &winning_frame(&fixture),
            &fixture.engine_dispatches,
            &fixture.engine_layout,
            &config,
        )
        .expect("a floor cannot hide a share the task target accepts");

        assert_eq!(accepted_at.to_target(), task_target);
        assert_eq!(share.expected_work, task_target.to_work());
    }

    #[tokio::test]
    async fn result_counters_partition_every_frame_by_outcome() {
        let mut fixture = result_fixture();
        let config = Bzm2ThreadConfig::new("/dev/null".into(), 5_000_000, 55.0);
        let status = Arc::new(RwLock::new(HashThreadStatus {
            is_active: true,
            ..Default::default()
        }));
        let mut runtime_measurements = ThreadRuntimeMeasurementState::new();
        let (event_tx, _event_rx) = tokio_mpsc::channel(8);
        let (share_tx, _share_rx) = tokio_mpsc::channel(8);
        fixture.engine_dispatches.newest_mut().task.share_tx = share_tx;

        let accepted = winning_frame(&fixture);
        let invalid = protocol::TdmResultFrame {
            status: 0,
            ..accepted
        };
        let below = protocol::TdmResultFrame {
            nonce: DEFAULT_NONCE_GAP + 1,
            ..accepted
        };
        let unknown_chip = protocol::TdmResultFrame {
            asic: 3,
            ..accepted
        };
        let stale = protocol::TdmResultFrame {
            sequence_id: 4,
            ..accepted
        };
        for frame in [&accepted, &invalid, &below, &unknown_chip, &stale] {
            handle_result_frame(
                frame,
                &fixture.engine_dispatches,
                &fixture.engine_layout,
                &config,
                &status,
                &event_tx,
                &mut runtime_measurements,
            )
            .await;
        }

        let metrics = runtime_measurements.snapshot_at(Instant::now());
        let by_asic = |asic: u8| {
            metrics
                .asics
                .iter()
                .find(|m| m.asic == asic)
                .map(|m| m.results)
                .unwrap_or_default()
        };
        assert_eq!(
            by_asic(0),
            Bzm2ResultCounters {
                decoded: 2,
                accepted: 1,
                invalid_nonce: 1,
                stale_sequence: 1,
                below_target: 1,
                ..Default::default()
            }
        );
        assert_eq!(
            by_asic(3),
            Bzm2ResultCounters {
                no_dispatch: 1,
                ..Default::default()
            },
            "a discard is still attributed to the chip that reported it"
        );
        assert_eq!(
            metrics.results,
            by_asic(0).add(by_asic(3)),
            "thread total is the sum of its ASICs"
        );
        assert_eq!(
            metrics.results.decoded,
            metrics.results.accepted + metrics.results.below_target,
            "decoded partitions into accepted and below_target"
        );
    }

    /// A task over block 881,423's coinbase, so every engine and every
    /// dispatch hashes its own extranonce2, with the share target set so
    /// that nonce 0, micro-job 0, at the task's ntime, on the FIRST
    /// extranonce2 the task hands out, meets it exactly. Returns that
    /// extranonce2 and the hash it gives.
    fn first_en2_wins_task() -> (HashTask, crate::job_source::Extranonce2, bitcoin::BlockHash) {
        use crate::job_source::test_blocks::block_881423;
        let en2_size = block_881423::EXTRANONCE2.size();
        let range = crate::job_source::Extranonce2Range::new(en2_size).unwrap();
        let mut task = test_task();
        task.template = Arc::new(JobTemplate {
            id: "computed".into(),
            prev_blockhash: bitcoin::BlockHash::all_zeros(),
            version: VersionTemplate::new(
                bitcoin::block::Version::from_consensus(0x2000_0000),
                GeneralPurposeBits::full(),
            )
            .unwrap(),
            bits: bitcoin::pow::CompactTarget::from_consensus(0x1d00_ffff),
            share_target: task.share_target,
            time: task.ntime,
            merkle_root: MerkleRootKind::Computed(crate::job_source::MerkleRootTemplate {
                coinbase1: block_881423::coinbase1_bytes().to_vec(),
                extranonce1: block_881423::extranonce1_bytes().to_vec(),
                extranonce2_range: range.clone(),
                coinbase2: block_881423::coinbase2_bytes().to_vec(),
                merkle_branches: block_881423::MERKLE_BRANCHES.clone(),
            }),
        });
        task.en2_range = Some(range);
        let en2 = crate::job_source::Extranonce2::new(7, en2_size).unwrap();
        task.en2 = Some(en2);
        let hash = bitcoin::block::Header {
            version: compute_micro_versions(&task)[0],
            prev_blockhash: task.template.prev_blockhash,
            merkle_root: task.template.compute_merkle_root(&en2).unwrap(),
            time: task.ntime,
            bits: task.template.bits,
            nonce: 0,
        }
        .block_hash();
        task.share_target = Difficulty::from_hash(&hash).to_target();
        (task, en2, hash)
    }

    /// Dispatch once and return the sequence byte the dispatch's first job
    /// carried, READ BACK OFF THE BUS: the tests below must hand the decoder
    /// the byte the silicon was given, not recompute the encoding under test.
    /// A macro so the dispatch-record type is inferred, not named.
    macro_rules! dispatch_and_read_first_sequence {
        ($writer:expr, $reader:expr, $task:expr, $base:expr, $layout:expr,
             $records:expr, $config:expr, $cursor:expr) => {{
            dispatch_task_to_board(
                &mut $writer,
                $task,
                $base,
                $layout,
                &mut $records,
                $config,
                &mut satisfied_interlock(),
                &EngineGate::Ready,
                &mut $cursor,
            )
            .await
            .unwrap();
            let mut bytes = Vec::new();
            let mut buf = vec![0u8; 1024];
            while let Ok(Ok(n)) =
                tokio::time::timeout(Duration::from_millis(150), $reader.read(&mut buf)).await
            {
                if n == 0 {
                    break;
                }
                bytes.extend_from_slice(&buf[..n]);
            }
            let mut i = 0;
            let mut first = None;
            while i + 2 <= bytes.len() {
                let len = u16::from_le_bytes([bytes[i], bytes[i + 1]]) as usize;
                if len == 0 {
                    break;
                }
                if len == 48 && first.is_none() {
                    first = Some(bytes[i + 46]);
                }
                i += len;
            }
            first.expect("the dispatch wrote a job")
        }};
    }

    /// The frame the chip returns for nonce 0 in micro-job 0 of the job
    /// that carried `sequence_id`, at the job's own ntime, from ASIC 0's
    /// engine (0, 0).
    fn nonce_zero_result(sequence_id: u8) -> protocol::TdmResultFrame {
        protocol::TdmResultFrame {
            asic: 0,
            engine_address: protocol::logical_engine_address(0, 0),
            status: 0x8,
            nonce: DEFAULT_NONCE_GAP,
            sequence_id,
            reported_time: DEFAULT_TIMESTAMP_COUNT,
        }
    }

    /// A HIT STILL IN FLIGHT WHEN THE NEXT DISPATCH LANDS IS A SHARE.
    ///
    /// The actor does not read while it writes a dispatch, so every hit an
    /// engine found while the ~50 kB dispatch crossed the wire is read after
    /// every record has moved on to the new job. Keyed on one parity bit,
    /// each was discarded as stale_sequence: 3,182 of the 13,999 result
    /// frames in one capture (the three threads'
    /// own discard counters as their last log summaries give them, against
    /// the frames in the three wire recordings, none of which had another
    /// decode-stage reason). Two clean_jobs fell inside that run's mining,
    /// and only 35 results of any kind were read between one and the first
    /// share of the job it brought; the pool would have taken the rest.
    #[tokio::test]
    async fn a_hit_for_the_previous_dispatch_is_a_share_after_the_next_lands() {
        let pty = openpty(None, None).unwrap();
        let writer_side =
            SerialStream::from_fd(pty.master.into_raw_fd(), SerialConfig::default()).unwrap();
        let reader_side =
            SerialStream::from_fd(pty.slave.into_raw_fd(), SerialConfig::default()).unwrap();
        let (_reader_a, mut writer, _control_a) = writer_side.split();
        let (mut reader, _writer_b, _control_b) = reader_side.split();

        let (task, en2, expected_hash) = first_en2_wins_task();
        let config = Bzm2ThreadConfig::new("/dev/null".into(), 5_000_000, 55.0);
        let layout = Bzm2EngineLayout::from_active_coordinates(vec![(0, 0)]);
        let mut records = Default::default();
        let mut cursor = 0u64;
        let seq_n = dispatch_and_read_first_sequence!(
            writer, reader, &task, 0, &layout, records, &config, cursor
        );
        // Dispatch N+1 lands before the hit from N is read.
        dispatch_and_read_first_sequence!(
            writer, reader, &task, 1, &layout, records, &config, cursor
        );

        let (share, _, _) =
            reconstruct_share_from_result(&nonce_zero_result(seq_n), &records, &layout, &config)
                .expect("a hit from dispatch N read after N+1 landed is a share; its job is live");
        assert_eq!(share.hash, expected_hash);
        assert_eq!(
            share.extranonce2,
            Some(en2),
            "the share names the extranonce2 dispatch N hashed, not N+1's"
        );
    }

    /// THE WRONG-RECORD CASE: a hit from two dispatches back is decoded
    /// against its OWN work.
    ///
    /// One parity bit cannot tell N-2 from N, so a late hit from N-2 was
    /// decoded against N's merkle root and came out a random hash, counted
    /// below_target. One capture logged one: hash_diff 0. In
    /// another, four of the fifteen decoded results the log kept
    /// verbatim have hash_diff under 4e-9 (01:52:16-01:52:18), the shape a
    /// wrong record leaves; a correct decode clears the chip's own filter,
    /// thousands here.
    #[tokio::test]
    async fn a_hit_from_two_dispatches_back_is_decoded_against_its_own_work() {
        let pty = openpty(None, None).unwrap();
        let writer_side =
            SerialStream::from_fd(pty.master.into_raw_fd(), SerialConfig::default()).unwrap();
        let reader_side =
            SerialStream::from_fd(pty.slave.into_raw_fd(), SerialConfig::default()).unwrap();
        let (_reader_a, mut writer, _control_a) = writer_side.split();
        let (mut reader, _writer_b, _control_b) = reader_side.split();

        let (task, en2, expected_hash) = first_en2_wins_task();
        let config = Bzm2ThreadConfig::new("/dev/null".into(), 5_000_000, 55.0);
        let layout = Bzm2EngineLayout::from_active_coordinates(vec![(0, 0)]);
        let mut records = Default::default();
        let mut cursor = 0u64;
        let seq_n = dispatch_and_read_first_sequence!(
            writer, reader, &task, 0, &layout, records, &config, cursor
        );
        for base in 1..=2u8 {
            dispatch_and_read_first_sequence!(
                writer, reader, &task, base, &layout, records, &config, cursor
            );
        }

        let (share, _, _) =
            reconstruct_share_from_result(&nonce_zero_result(seq_n), &records, &layout, &config)
                .expect("a hit from N-2 is decoded against N-2's work and meets its target");
        assert_eq!(share.hash, expected_hash);
        assert_eq!(share.extranonce2, Some(en2));
    }

    /// A HIT OLDER THAN EVERY DISPATCH STILL HELD IS STALE, NEVER DECODED
    /// AGAINST SOMEBODY ELSE'S WORK. The records are bounded; a result whose
    /// dispatch has left them must say so, not borrow the newest record that
    /// happens to share its low bits.
    #[tokio::test]
    async fn a_hit_older_than_every_held_dispatch_is_stale_not_misdecoded() {
        let pty = openpty(None, None).unwrap();
        let writer_side =
            SerialStream::from_fd(pty.master.into_raw_fd(), SerialConfig::default()).unwrap();
        let reader_side =
            SerialStream::from_fd(pty.slave.into_raw_fd(), SerialConfig::default()).unwrap();
        let (_reader_a, mut writer, _control_a) = writer_side.split();
        let (mut reader, _writer_b, _control_b) = reader_side.split();

        let (task, _en2, _hash) = first_en2_wins_task();
        let config = Bzm2ThreadConfig::new("/dev/null".into(), 5_000_000, 55.0);
        let layout = Bzm2EngineLayout::from_active_coordinates(vec![(0, 0)]);
        let mut records = Default::default();
        let mut cursor = 0u64;
        let seq_first = dispatch_and_read_first_sequence!(
            writer, reader, &task, 0, &layout, records, &config, cursor
        );
        // Sixteen more: far past any bounded history this thread should keep.
        for base in 1..=16u8 {
            dispatch_and_read_first_sequence!(
                writer, reader, &task, base, &layout, records, &config, cursor
            );
        }
        let outcome = reconstruct_share_from_result(
            &nonce_zero_result(seq_first),
            &records,
            &layout,
            &config,
        );
        assert_eq!(
            outcome.map(|(share, _, _)| share.extranonce2).unwrap_err(),
            ResultDiscard::StaleSequence
        );
    }

    /// THE DISPATCH COUNTER WRAPS; THE WORK IT NAMES MUST NOT. The counter
    /// is a byte, so dispatch 256 reuses dispatch 0's number. A hit from the
    /// last dispatch before the wrap, read after the first two after it,
    /// still resolves to its own work.
    #[tokio::test]
    async fn a_hit_from_across_the_counter_wrap_resolves_to_its_own_work() {
        let pty = openpty(None, None).unwrap();
        let writer_side =
            SerialStream::from_fd(pty.master.into_raw_fd(), SerialConfig::default()).unwrap();
        let reader_side =
            SerialStream::from_fd(pty.slave.into_raw_fd(), SerialConfig::default()).unwrap();
        let (_reader_a, mut writer, _control_a) = writer_side.split();
        let (mut reader, _writer_b, _control_b) = reader_side.split();

        let (task, en2, expected_hash) = first_en2_wins_task();
        let config = Bzm2ThreadConfig::new("/dev/null".into(), 5_000_000, 55.0);
        let layout = Bzm2EngineLayout::from_active_coordinates(vec![(0, 0)]);
        let mut records = Default::default();
        let mut cursor = 0u64;
        let seq_255 = dispatch_and_read_first_sequence!(
            writer, reader, &task, 255, &layout, records, &config, cursor
        );
        for base in [0u8, 1] {
            dispatch_and_read_first_sequence!(
                writer, reader, &task, base, &layout, records, &config, cursor
            );
        }
        let (share, _, _) =
            reconstruct_share_from_result(&nonce_zero_result(seq_255), &records, &layout, &config)
                .expect("the hit from dispatch 255 resolves to 255's work after the wrap");
        assert_eq!(share.hash, expected_hash);
        assert_eq!(share.extranonce2, Some(en2));
    }
}
