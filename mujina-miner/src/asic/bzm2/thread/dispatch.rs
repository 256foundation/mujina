use std::collections::HashMap;

use bitcoin::block::Header as BlockHeader;
use bitcoin::consensus::serialize;
use bitcoin::hashes::{HashEngine, sha256};
use tokio::io::AsyncWriteExt;

use crate::asic::hash_thread::{HashTask, HashThreadError};
use crate::job_source::{GeneralPurposeBits, MerkleRootKind};
use crate::transport::serial::SerialWriter;

use super::super::protocol::{
    BROADCAST_ASIC, Bzm2EngineLayout, ENGINE_REG_END_NONCE, ENGINE_REG_START_NONCE,
    ENGINE_REG_TARGET, ENGINE_REG_TIMESTAMP_COUNT, ENGINE_REG_ZEROS_TO_FIND, encode_write_job,
    encode_write_register, leading_zero_threshold, logical_engine_address,
};
use super::engine::*;
use super::interlock::*;
use super::*;

const TIMESTAMP_COUNT_AUTO_CLOCK_UNGATE: u8 = 0x80;
/// BIP320 general-purpose version bits tried for the four micro-jobs of one
/// dispatch, in micro-job order. Slot 0 is always the base version. This is
/// the version set in force for every result the thread reconstructs, so it
/// is logged at start-up as run metadata rather than left implicit.
pub(super) const VERSION_CANDIDATES: [u16; 4] = [0, 2, 4, 8];

/// Dispatch tags the job's sequence byte carries: its upper six bits. The
/// lower two name the micro-job, so the byte on the wire is
/// `tag << 2 | micro_job`.
///
/// The silicon returns the byte it was given. Measured, in our own captures,
/// at both ends of it: the stock stack's work comes back 0xfc-0xff (tag 63;
/// one capture: 104,459 of 104,460 results, and
/// another, 119,539 of 119,598), ours 0x00-0x07 (tags 0 and 1;
/// a third capture, all 13,999). So every bit of it has been
/// seen echoed as both 0 and 1. Tags 2-62 are UNMEASURED until a run sends
/// them: a part that dropped a bit would show as results whose tag names no
/// held dispatch, counted `stale_sequence` from the first minute, never as a
/// share for the wrong work.
///
/// One tag bit, as before, cannot tell a hit from dispatch N-2 from one from
/// N: such a hit was decoded against N's work and came out a random hash.
const SEQUENCE_TAGS: u8 = 64;

/// How many recent dispatches a result can still be matched to.
///
/// The loss this exists for is one dispatch deep. The actor does not read
/// while it writes a dispatch, so hits found while the new job crossed the
/// wire are read after it, with the previous dispatch's tag. In one capture,
/// attempt 3, replayed from its wire recordings with the dispatch boundaries
/// taken from the read clock and the parity anchored on its logged shares,
/// nine in ten of them were in the first read after the dispatch that
/// replaced theirs (INFERRED: a hold-out of half the shares put 96% of the
/// other half on the right side), at a 500 ms interval. Four is twice what
/// that needs, and stays well under [`SEQUENCE_TAGS`], so a result older than
/// every held dispatch carries a tag none of them has: it is stale, never
/// decoded against another dispatch's work.
const DISPATCH_RING_DEPTH: usize = 4;
const _: () = assert!(DISPATCH_RING_DEPTH < SEQUENCE_TAGS as usize);

/// The tag dispatch number `base_sequence` goes out under. The counter is a
/// byte and 256 is a multiple of [`SEQUENCE_TAGS`], so the tag sequence runs
/// on unbroken across the counter's wrap.
pub(super) fn dispatch_tag(base_sequence: u8) -> u8 {
    base_sequence % SEQUENCE_TAGS
}

/// The sequence byte one micro-job of a dispatch carries.
fn sequence_byte(tag: u8, micro_job_id: usize) -> u8 {
    (tag << 2) | micro_job_id as u8
}

/// The chip ids a dispatch reached, as a bitmap over the id byte.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct AsicSet([u64; 4]);

impl AsicSet {
    pub(super) fn of(ids: &[u8]) -> Self {
        let mut set = Self::default();
        for &id in ids {
            set.0[usize::from(id / 64)] |= 1u64 << (id % 64);
        }
        set
    }

    fn intersects(&self, other: &Self) -> bool {
        self.0.iter().zip(other.0.iter()).any(|(a, b)| a & b != 0)
    }
}

/// What one engine address was given in one dispatch. Everything else about
/// the dispatch is shared by every engine and lives once, in [`Dispatch`].
#[derive(Debug, Clone, Copy)]
pub(super) struct EngineWork {
    pub(super) merkle_root: bitcoin::TxMerkleNode,
}

/// One dispatch: one task, written to every engine address, under one tag.
///
/// Recorded once per engine, not once per engine per chip: the job is
/// broadcast, so every chip in `asics` holds the same work at an address.
/// Keyed per chip, a 100-chip chain cloned the whole task 23,600 times a
/// dispatch, twice a second.
pub(super) struct Dispatch {
    pub(super) tag: u8,
    asics: AsicSet,
    engines: HashMap<u16, EngineWork>,
}

/// The dispatches a result may still be matched to, newest last.
///
/// This is how a miner keeps work it has already replaced: the job id on
/// the wire indexes a bounded history of recent jobs, and the pool's
/// `clean_jobs` empties it. Here the id is [`dispatch_tag`]'s, the history
/// is [`DISPATCH_RING_DEPTH`] deep, and `clean_jobs` arrives as ReplaceTask,
/// which calls [`Self::invalidate`]. An UpdateTask (`clean_jobs=false`)
/// leaves the history alone: the pool still takes shares for those jobs,
/// and the scheduler still holds their share channels.
#[derive(Default)]
pub(super) struct DispatchRing {
    dispatches: std::collections::VecDeque<Dispatch>,
}

impl DispatchRing {
    /// Open the record for a new dispatch; engines are added to it as they
    /// are written. The oldest dispatch leaves when the ring is full, and so
    /// does any earlier one under the same tag to the same chips -- a failed
    /// dispatch is retried under its tag, and the chips now hold the retry.
    pub(super) fn begin(&mut self, tag: u8, asics: AsicSet, engine_count: usize) {
        self.dispatches
            .retain(|d| !(d.tag == tag && d.asics.intersects(&asics)));
        while self.dispatches.len() >= DISPATCH_RING_DEPTH {
            self.dispatches.pop_front();
        }
        self.dispatches.push_back(Dispatch {
            tag,
            asics,
            engines: HashMap::with_capacity(engine_count),
        });
    }

    /// Record what one engine of the newest dispatch was given, once its
    /// job is on the wire.
    pub(super) fn record_engine(&mut self, engine_id: u16, work: EngineWork) {
        if let Some(newest) = self.dispatches.back_mut() {
            newest.engines.insert(engine_id, work);
        }
    }

    /// Forget every dispatch: the pool has invalidated their jobs
    /// (`clean_jobs`), or the chain has been told to stop. A result for any
    /// of them is discarded from here on; none can be forwarded.
    pub(super) fn invalidate(&mut self) {
        self.dispatches.clear();
    }
}

// As handle_dts_vs_frame: the dispatch touches each of these, and the
// hardware runs exercised this signature as written.
#[allow(clippy::too_many_arguments)]
pub(super) async fn dispatch_task_to_board(
    writer: &mut SerialWriter,
    task: &HashTask,
    base_sequence: u8,
    engine_layout: &Bzm2EngineLayout,
    engine_dispatches: &mut DispatchRing,
    config: &Bzm2ThreadConfig,
    interlock: &mut ThermalInterlock,
    engine_gate: &EngineGate,
    en2_cursor: &mut u64,
) -> Result<(), HashThreadError> {
    // Checked here as well as by the callers, for the same reason as the
    // interlock below: a caller that forgets it sends work to gated engines.
    if let Some(why) = engine_gate.refusal() {
        return Err(HashThreadError::WorkAssignmentFailed(format!(
            "engines not ready: {why}"
        )));
    }
    // The interlock is consulted here rather than at the call sites, because
    // there are three call sites and there will be more. A safety check a
    // caller can forget is a safety check that will be forgotten.
    if let Err(refusal) = interlock.check() {
        interlock.record_refusal();
        warn!(
            path = %config.serial_path,
            reason = %refusal,
            refusals = interlock.refusals(),
            "Thermal interlock refused to dispatch work"
        );
        return Err(HashThreadError::WorkAssignmentFailed(format!(
            "thermal interlock: {refusal}"
        )));
    }

    let versions = compute_micro_versions(task);
    let lead_zeros = leading_zero_threshold(task.share_target).saturating_sub(32);
    let timestamp_count = config.timestamp_count | TIMESTAMP_COUNT_AUTO_CLOCK_UNGATE;
    let bits = task.template.bits.to_consensus();

    // ONE HEADER PER ENGINE ADDRESS, and per-ASIC nonce slices set at attach.
    //
    // The silicon does not split work: the same job on two engines, or on
    // two ASICs, returns the same nonce (measured on hardware: 7,802
    // groups of one job reported from several places, none disagreeing). This
    // used to broadcast one header and the full nonce range everywhere, so a
    // hundred ASICs times 236 engines hashed the same nonces 23,600 times over
    // -- about one engine's worth of unique work, a share every few days at
    // pool difficulty. Now each engine address gets its own extranonce2, so
    // its own merkle root; the slices set at attach separate the ASICs; and
    // the cursor advances every dispatch, so no two dispatches repeat.
    let engine_count = engine_layout.active_coordinates().len();
    // The job is broadcast, so every chip on the bus holds it; results are
    // resolved by the chips this thread addresses, as before.
    let tag = dispatch_tag(base_sequence);
    engine_dispatches.begin(tag, AsicSet::of(&config.asic_ids), engine_count);
    for (index, &(row, col)) in engine_layout.active_coordinates().iter().enumerate() {
        let engine_address = logical_engine_address(row, col);
        let work = work_for_engine(task, *en2_cursor, index)?;
        let merkle_root = work.merkle_root;
        let midstates = versions.map(|version| compute_midstate(task, merkle_root, version));
        let header_bytes = serialize(&BlockHeader {
            version: versions[0],
            prev_blockhash: task.template.prev_blockhash,
            merkle_root,
            time: task.ntime,
            bits: task.template.bits,
            nonce: 0,
        });
        // THE LAST THREE HEADER WORDS GO OUT BYTE-SWAPPED: merkle residue,
        // ntime and nBits are big-endian on the wire, where the engine hashes
        // them. `encode_write_job` writes its u32 fields little-endian, so the
        // residue is read big-endian here and ntime is swapped at the call.
        // Pinned by `the_job_goes_out_in_the_byte_order_the_silicon_hashes`
        // against a public hardware vector.
        let merkle_root_residue = u32::from_be_bytes(header_bytes[64..68].try_into().unwrap());

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
                &bits.to_be_bytes(),
            ))
            .await
            .map_err(|err| {
                HashThreadError::WorkAssignmentFailed(format!("Failed to write target bits: {err}"))
            })?;

        // No START/END here: the nonce slices are per ASIC and set at attach.
        // A broadcast write of the full range would put every ASIC back on
        // the same nonces.
        for (micro_job_id, midstate) in midstates.iter().enumerate() {
            let job_control = if micro_job_id == 3 { 3 } else { 0 };
            writer
                .write_all(&encode_write_job(
                    BROADCAST_ASIC,
                    engine_address,
                    midstate,
                    merkle_root_residue,
                    task.ntime.swap_bytes(),
                    sequence_byte(tag, micro_job_id),
                    job_control,
                ))
                .await
                .map_err(|err| {
                    HashThreadError::WorkAssignmentFailed(format!("Failed to write job: {err}"))
                })?;
        }

        // Recorded once its job is on the wire, and alongside the dispatches
        // before it: a hit from those is still read after this one lands.
        let engine_id = engine_layout.logical_engine_id(row, col).unwrap();
        engine_dispatches.record_engine(engine_id, work);
    }
    *en2_cursor = en2_cursor.wrapping_add(engine_count as u64);

    Ok(())
}

/// The work one engine index hashes in this dispatch: that engine's own
/// extranonce2, and the merkle root it gives.
///
/// Extranonce2 values come from the task's range, starting at its scheduled
/// value, `cursor + index` further on, wrapping within the range. A fixed
/// merkle root (header-only work, not issued by the scheduler) has nothing
/// to vary, so every engine gets the same one, and the task's extranonce2.
fn work_for_engine(
    task: &HashTask,
    cursor: u64,
    index: usize,
) -> Result<EngineWork, HashThreadError> {
    let template = match &task.template.merkle_root {
        MerkleRootKind::Fixed(root) => {
            return Ok(EngineWork { merkle_root: *root });
        }
        MerkleRootKind::Computed(template) => template,
    };
    let range = task
        .en2_range
        .clone()
        .unwrap_or_else(|| template.extranonce2_range.clone());
    let start = task.en2.as_ref().map_or(range.min, |en2| en2.value());
    let span = range.len();
    let offset = (start.wrapping_sub(range.min))
        .wrapping_add(cursor)
        .wrapping_add(index as u64);
    let value = range.min.wrapping_add(if span == u64::MAX {
        offset
    } else {
        offset % span
    });
    let en2 = crate::job_source::Extranonce2::new(value, range.size).map_err(|err| {
        HashThreadError::WorkAssignmentFailed(format!("BZM2 extranonce2 {value}: {err}"))
    })?;
    let merkle_root = task.template.compute_merkle_root(&en2).map_err(|err| {
        HashThreadError::WorkAssignmentFailed(format!("BZM2 merkle root computation failed: {err}"))
    })?;
    Ok(EngineWork { merkle_root })
}

/// Per-ASIC nonce slices: disjoint, even-aligned, covering the 32-bit space.
///
/// The last slice ends at 0xffff_fffe so every boundary stays even.
pub(super) fn nonce_slices(asic_count: usize) -> Vec<(u32, u32)> {
    let n = asic_count.max(1) as u64;
    let step = (((1u64 << 32) / n) as u32) & !1;
    (0..n)
        .map(|k| {
            let start = (k as u32).wrapping_mul(step);
            let end = if k == n - 1 {
                0xffff_fffe
            } else {
                start + step - 2
            };
            (start, end)
        })
        .collect()
}

/// Write each ASIC's nonce slice to every active engine on it, unicast.
///
/// Unicast is the addressing measured on hardware to reach one ASIC only. About
/// half a megabyte for a hundred-device chain, roughly a second at 5 Mbaud,
/// once per attach and once per re-arm: a reset clears these registers.
pub(super) async fn program_nonce_slices(
    writer: &mut SerialWriter,
    engine_layout: &Bzm2EngineLayout,
    config: &Bzm2ThreadConfig,
) -> std::io::Result<()> {
    let slices = nonce_slices(config.asic_ids.len());
    for (&asic, &(start, end)) in config.asic_ids.iter().zip(slices.iter()) {
        for &(row, col) in engine_layout.active_coordinates() {
            let address = logical_engine_address(row, col);
            writer
                .write_all(&encode_write_register(
                    asic,
                    address,
                    ENGINE_REG_START_NONCE,
                    &start.to_le_bytes(),
                ))
                .await?;
            writer
                .write_all(&encode_write_register(
                    asic,
                    address,
                    ENGINE_REG_END_NONCE,
                    &end.to_le_bytes(),
                ))
                .await?;
        }
    }
    writer.flush().await
}

pub(super) fn compute_micro_versions(task: &HashTask) -> [bitcoin::block::Version; 4] {
    let mut versions = [task.template.version.base(); 4];

    for (slot, candidate) in VERSION_CANDIDATES.into_iter().enumerate() {
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
    // State words go out LITTLE-endian: the engine reads each word natively,
    // while `bitcoin_hashes` returns them big-endian, as a digest is written.
    // Word order is kept; only the bytes within each word are reversed.
    let mut midstate = engine.midstate().to_byte_array();
    for word in midstate.chunks_exact_mut(4) {
        word.reverse();
    }
    midstate
}

#[cfg(test)]
impl DispatchRing {
    // Test-only views of the dispatch records, kept as a plain
    // impl outside `mod tests` so results.rs's tests can call them too.
    pub(super) fn newest(&self) -> &Dispatch {
        self.dispatches.back().expect("a dispatch was recorded")
    }

    pub(super) fn len(&self) -> usize {
        self.dispatches.len()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::job_source::GeneralPurposeBits;
    use crate::transport::{SerialConfig, SerialStream};
    use bitcoin::hashes::Hash;

    use nix::pty::openpty;
    use std::os::unix::io::IntoRawFd;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn dispatch_is_refused_when_the_interlock_says_no() {
        // The property that matters: the refusal reaches the dispatch path,
        // not just the interlock's own check. Same setup as the fanout test,
        // so the only difference is the interlock.
        let pty = openpty(None, None).unwrap();
        let writer_side =
            SerialStream::from_fd(pty.master.into_raw_fd(), SerialConfig::default()).unwrap();
        let (_reader_a, mut writer, _control_a) = writer_side.split();

        let task = test_task();
        let mut engine_dispatches = DispatchRing::default();
        let config = Bzm2ThreadConfig::new("/dev/null".into(), 5_000_000, 55.0);
        let engine_layout = Bzm2EngineLayout::from_active_coordinates(vec![(0, 0)]);
        let mut lock = ThermalInterlock::new(85.0, Duration::from_secs(30));

        let err = dispatch_task_to_board(
            &mut writer,
            &task,
            0,
            &engine_layout,
            &mut engine_dispatches,
            &config,
            &mut lock,
            &EngineGate::Ready,
            &mut 0,
        )
        .await
        .expect_err("a chain with no temperature reading must not be given work");
        assert!(
            format!("{err}").contains("thermal interlock"),
            "the refusal must name itself, got: {err}"
        );
        assert_eq!(lock.refusals(), 1);

        // And it lets the same work through once the chain reports.
        lock.observe(0, 55.0);
        dispatch_task_to_board(
            &mut writer,
            &task,
            0,
            &engine_layout,
            &mut engine_dispatches,
            &config,
            &mut lock,
            &EngineGate::Ready,
            &mut 0,
        )
        .await
        .expect("a fresh, cool reading permits dispatch");
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

    /// The midstate goes out as SHA-256 state words in LITTLE-endian bytes.
    ///
    /// `bitcoin_hashes` hands back the state words big-endian, which is how a
    /// digest is written; the engine reads each word natively. The expected
    /// bytes were computed independently of this crate: a from-constants
    /// SHA-256 compression of the header's first 64 bytes, self-checked
    /// against a standard library digest before use.
    #[test]
    fn the_midstate_goes_out_as_little_endian_state_words() {
        let (task, merkle_root) = public_vector_task();
        let midstate = compute_midstate(
            &task,
            merkle_root,
            bitcoin::block::Version::from_consensus(0x3fff_0000),
        );
        assert_eq!(
            midstate,
            hex_32("9fd177e1d1c00a0bb92227f3fc01ac91a73d9b5caaa74d885cdd99e888ee6d00"),
            "state words must be little-endian on the wire (big-endian reads e177d19f...)"
        );
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
        let mut engine_dispatches = DispatchRing::default();
        let config = Bzm2ThreadConfig::new("/dev/null".into(), 5_000_000, 55.0);
        let engine_coords = vec![(0, 0), (0, 1)];
        let engine_layout = Bzm2EngineLayout::from_active_coordinates(engine_coords.clone());

        dispatch_task_to_board(
            &mut writer,
            &task,
            1,
            &engine_layout,
            &mut engine_dispatches,
            &config,
            &mut satisfied_interlock(),
            &EngineGate::Ready,
            &mut 0,
        )
        .await
        .unwrap();

        // ZEROS, TIMESTAMP_COUNT, TARGET, four jobs. No START/END: the nonce
        // slices are per ASIC, set at attach, and never broadcast.
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
        // One dispatch, recorded once per engine address.
        assert_eq!(engine_dispatches.len(), 1);
        assert_eq!(
            engine_dispatches.newest().engines.len(),
            engine_coords.len()
        );

        let packet_lengths = bytes
            .chunks_exact(expected_bytes_per_engine)
            .map(|chunk| {
                [
                    u16::from_le_bytes([chunk[0], chunk[1]]) as usize,
                    u16::from_le_bytes([chunk[8], chunk[9]]) as usize,
                    u16::from_le_bytes([chunk[16], chunk[17]]) as usize,
                    u16::from_le_bytes([chunk[27], chunk[28]]) as usize,
                ]
            })
            .collect::<Vec<_>>();
        assert!(packet_lengths.iter().all(|lens| *lens == [8, 8, 11, 48]));

        for chunk in bytes.chunks_exact(expected_bytes_per_engine) {
            assert_eq!(
                chunk[7],
                leading_zero_threshold(task.share_target).saturating_sub(32)
            );
            assert_eq!(chunk[15], 0x80 | DEFAULT_TIMESTAMP_COUNT);
            // nBits goes out big-endian (see
            // `the_job_goes_out_in_the_byte_order_the_silicon_hashes`).
            assert_eq!(
                &chunk[23..27],
                &task.template.bits.to_consensus().to_be_bytes()
            );
            // No nonce-range write anywhere in a dispatch.
            for frame_start in [0usize, 8, 16] {
                assert_ne!(chunk[frame_start + 5], ENGINE_REG_START_NONCE);
                assert_ne!(chunk[frame_start + 5], ENGINE_REG_END_NONCE);
            }
        }

        let last_packet_start = bytes.len() - 48;
        assert_eq!(
            u16::from_le_bytes([bytes[last_packet_start], bytes[last_packet_start + 1]]) as usize,
            48
        );
        assert_eq!(bytes[last_packet_start + 46], 7);
        assert_eq!(bytes[last_packet_start + 47], 3);
    }

    #[test]
    fn runtime_engine_layout_compresses_logical_ids_after_missing_engines() {
        let layout = Bzm2EngineLayout::from_active_coordinates([(0, 0), (0, 6), (19, 10)]);

        assert_eq!(layout.active_engine_count(), 3);
        assert_eq!(layout.logical_engine_id(0, 0), Some(0));
        assert_eq!(layout.logical_engine_id(0, 6), Some(1));
        assert_eq!(layout.logical_engine_id(19, 10), Some(2));
        assert_eq!(layout.logical_engine_id(0, 1), None);
    }

    /// THE SEQUENCE BYTE IS `tag << 2 | micro_job`, and the tag runs on
    /// across the dispatch counter's wrap. Tags 0 and 1 give the bytes the
    /// one-bit encoding did (0-7), which is all a run had ever put on the
    /// wire before this fix.
    #[test]
    fn the_sequence_byte_is_the_dispatch_tag_then_the_micro_job() {
        assert_eq!(sequence_byte(dispatch_tag(0), 0), 0x00);
        assert_eq!(sequence_byte(dispatch_tag(1), 3), 0x07);
        assert_eq!(sequence_byte(dispatch_tag(6), 3), (6 << 2) | 3);
        assert_eq!(sequence_byte(dispatch_tag(63), 3), 0xff);
        assert_eq!(dispatch_tag(64), 0, "64 tags");
        // 256 dispatches later the byte counter is back where it began and
        // so is the tag: no two of the last SEQUENCE_TAGS dispatches share one.
        let tags: Vec<u8> = (0..SEQUENCE_TAGS as u16)
            .map(|k| dispatch_tag(255u8.wrapping_add(k as u8)))
            .collect();
        let distinct: std::collections::HashSet<_> = tags.iter().collect();
        assert_eq!(distinct.len(), SEQUENCE_TAGS as usize);
    }

    #[test]
    fn micro_versions_come_from_the_logged_candidate_set() {
        let task = test_task();
        let versions = compute_micro_versions(&task);
        for (slot, candidate) in VERSION_CANDIDATES.into_iter().enumerate() {
            let expected = task
                .template
                .version
                .apply_gp_bits(&GeneralPurposeBits::new(candidate.to_be_bytes()))
                .unwrap();
            assert_eq!(versions[slot], expected, "slot {slot}");
        }
    }

    /// The slices cover the 32-bit space, in even-aligned pieces, with no
    /// overlap and no gap -- overlap is duplicated work, a gap is lost work.
    #[test]
    fn nonce_slices_partition_the_space_into_even_disjoint_slices() {
        for n in [1usize, 2, 3, 100] {
            let slices = nonce_slices(n);
            assert_eq!(slices.len(), n);
            assert_eq!(slices[0].0, 0, "the first slice starts at zero");
            assert_eq!(
                slices[n - 1].1,
                0xffff_fffe,
                "the last slice ends at the top"
            );
            for &(start, end) in &slices {
                assert_eq!(start % 2, 0, "slice start must be even");
                assert_eq!(end % 2, 0, "slice end must be even");
                assert!(start <= end);
            }
            for pair in slices.windows(2) {
                assert_eq!(
                    pair[1].0.wrapping_sub(pair[0].1),
                    2,
                    "slices must abut: {:#x}..{:#x} then {:#x}",
                    pair[0].0,
                    pair[0].1,
                    pair[1].0
                );
            }
        }
    }
}
