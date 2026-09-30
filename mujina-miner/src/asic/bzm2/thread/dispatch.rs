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
use super::metrics::*;
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

    fn contains(&self, id: u8) -> bool {
        self.0[usize::from(id / 64)] & (1u64 << (id % 64)) != 0
    }

    fn intersects(&self, other: &Self) -> bool {
        self.0.iter().zip(other.0.iter()).any(|(a, b)| a & b != 0)
    }
}

/// What one engine address was given in one dispatch. Everything else about
/// the dispatch is shared by every engine and lives once, in [`Dispatch`].
#[derive(Debug, Clone, Copy)]
pub(super) struct EngineWork {
    /// The extranonce2 this engine's header was built from: what a share
    /// from it must name.
    pub(super) en2: Option<crate::job_source::Extranonce2>,
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
    /// The task as dispatched, ntime included. Each engine's own
    /// extranonce2 is in `engines`, not here.
    pub(super) task: HashTask,
    pub(super) versions: [bitcoin::block::Version; 4],
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
    pub(super) fn begin(
        &mut self,
        tag: u8,
        task: &HashTask,
        versions: [bitcoin::block::Version; 4],
        asics: AsicSet,
        engine_count: usize,
    ) {
        self.dispatches
            .retain(|d| !(d.tag == tag && d.asics.intersects(&asics)));
        while self.dispatches.len() >= DISPATCH_RING_DEPTH {
            self.dispatches.pop_front();
        }
        self.dispatches.push_back(Dispatch {
            tag,
            task: task.clone(),
            versions,
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

    /// The dispatch, and the engine's work in it, that a result from `asic`'s
    /// `engine_id` tagged `tag` was found on.
    ///
    /// `NoDispatch` when no held dispatch reached that engine on that chip
    /// at all; `StaleSequence` when some did, but none under this tag: its
    /// dispatch has left the ring, or was invalidated.
    pub(super) fn resolve(
        &self,
        asic: u8,
        engine_id: u16,
        tag: u8,
    ) -> Result<(&Dispatch, &EngineWork), ResultDiscard> {
        let mut reached = false;
        for dispatch in self.dispatches.iter().rev() {
            if !dispatch.asics.contains(asic) {
                continue;
            }
            let Some(work) = dispatch.engines.get(&engine_id) else {
                continue;
            };
            if dispatch.tag == tag {
                return Ok((dispatch, work));
            }
            reached = true;
        }
        Err(if reached {
            ResultDiscard::StaleSequence
        } else {
            ResultDiscard::NoDispatch
        })
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
    engine_dispatches.begin(
        tag,
        task,
        versions,
        AsicSet::of(&config.asic_ids),
        engine_count,
    );
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
            return Ok(EngineWork {
                en2: task.en2,
                merkle_root: *root,
            });
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
    Ok(EngineWork {
        en2: Some(en2),
        merkle_root,
    })
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

    pub(super) fn newest_mut(&mut self) -> &mut Dispatch {
        self.dispatches.back_mut().expect("a dispatch was recorded")
    }

    pub(super) fn len(&self) -> usize {
        self.dispatches.len()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::job_source::{GeneralPurposeBits, JobTemplate, VersionTemplate};
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

    /// The last three header words go out byte-swapped, as the engine hashes
    /// them: merkle residue, ntime and nBits all big-endian on the wire.
    #[tokio::test]
    async fn the_job_goes_out_in_the_byte_order_the_silicon_hashes() {
        let pty = openpty(None, None).unwrap();
        let writer_side =
            SerialStream::from_fd(pty.master.into_raw_fd(), SerialConfig::default()).unwrap();
        let reader_side =
            SerialStream::from_fd(pty.slave.into_raw_fd(), SerialConfig::default()).unwrap();
        let (_reader_a, mut writer, _control_a) = writer_side.split();
        let (mut reader, _writer_b, _control_b) = reader_side.split();

        let (task, merkle_root) = public_vector_task();
        let mut engine_dispatches = DispatchRing::default();
        let config = Bzm2ThreadConfig::new("/dev/null".into(), 5_000_000, 55.0);
        let engine_layout = Bzm2EngineLayout::from_active_coordinates(vec![(0, 0)]);
        dispatch_task_to_board(
            &mut writer,
            &task,
            0,
            &engine_layout,
            &mut engine_dispatches,
            &config,
            &mut satisfied_interlock(),
            &EngineGate::Ready,
            &mut 0,
        )
        .await
        .unwrap();

        // Everything one engine's dispatch writes, whatever its length.
        let mut bytes = Vec::new();
        let mut buf = vec![0u8; 512];
        while let Ok(Ok(n)) =
            tokio::time::timeout(Duration::from_millis(150), reader.read(&mut buf)).await
        {
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&buf[..n]);
        }

        // Walk the length-prefixed frames rather than assume offsets.
        let mut frames = Vec::new();
        let mut i = 0;
        while i + 2 <= bytes.len() {
            let len = u16::from_le_bytes([bytes[i], bytes[i + 1]]) as usize;
            frames.push(&bytes[i..i + len]);
            i += len;
        }
        let target = frames
            .iter()
            .find(|f| f.len() == 11 && f[5] == ENGINE_REG_TARGET)
            .expect("dispatch writes the target register");
        assert_eq!(
            &target[7..11],
            &[0x17, 0x02, 0x36, 0x9d],
            "nBits big-endian"
        );

        let jobs: Vec<_> = frames.iter().filter(|f| f.len() == 48).collect();
        assert_eq!(jobs.len(), 4);
        let dispatch = engine_dispatches.newest();
        for (slot, job) in jobs.iter().enumerate() {
            assert_eq!(
                &job[6..38],
                &compute_midstate(&task, merkle_root, dispatch.versions[slot]),
                "slot {slot} carries its own version's midstate"
            );
            assert_eq!(
                &job[38..42],
                &[0x13, 0xa1, 0x96, 0x6c],
                "merkle residue big-endian"
            );
            assert_eq!(&job[42..46], &[0x6a, 0x5d, 0xcd, 0x19], "ntime big-endian");
        }
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

    #[tokio::test]
    async fn dispatch_uses_runtime_engine_layout() {
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
        let engine_layout = Bzm2EngineLayout::from_active_coordinates([(0, 0), (19, 10)]);

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

        let mut buf = vec![0u8; 512];
        let mut bytes = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(250);
        // ZEROS, TIMESTAMP_COUNT, TARGET, four jobs. No START/END: the nonce
        // slices are per ASIC, set at attach, and never broadcast.
        let expected_bytes_per_engine = 8 + 8 + 11 + (48 * 4);
        while bytes.len() < expected_bytes_per_engine * engine_layout.active_engine_count() {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let n = tokio::time::timeout(remaining, reader.read(&mut buf))
                .await
                .unwrap()
                .unwrap();
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&buf[..n]);
        }

        let first_engine = logical_engine_address(0, 0);
        let second_engine = logical_engine_address(19, 10);
        let touched_engines = bytes
            .chunks_exact(expected_bytes_per_engine)
            .map(|chunk| u32::from_be_bytes([chunk[2], chunk[3], chunk[4], chunk[5]]))
            .map(|header| ((header >> 8) & 0x0fff) as u16)
            .collect::<Vec<_>>();
        assert!(touched_engines.contains(&first_engine));
        assert!(touched_engines.contains(&second_engine));
        assert!(!touched_engines.contains(&logical_engine_address(0, 1)));
        assert_eq!(engine_dispatches.newest().engines.len(), 2);
        assert!(engine_dispatches.resolve(0, 0, dispatch_tag(1)).is_ok());
        assert!(engine_dispatches.resolve(0, 1, dispatch_tag(1)).is_ok());
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

    /// THE HISTORY IS BOUNDED, AND IT FORGETS OLDEST FIRST. Driven as the
    /// dispatch tick drives it, twice a second for as long as the chain
    /// mines, and across the counter's wrap: the ring never holds more than
    /// DISPATCH_RING_DEPTH dispatches, the ones it holds are the newest, and
    /// a hit from any dispatch older than those is stale, not resolved.
    #[test]
    fn the_dispatch_history_never_exceeds_its_depth_and_forgets_oldest_first() {
        let task = test_task();
        let versions = compute_micro_versions(&task);
        let work = work_for_engine(&task, 0, 0).unwrap();
        let mut ring = DispatchRing::default();
        let mut base = 0u8;
        let mut sent: Vec<u8> = Vec::new();
        for _ in 0..600 {
            ring.begin(dispatch_tag(base), &task, versions, AsicSet::of(&[0]), 1);
            ring.record_engine(0, work);
            sent.push(dispatch_tag(base));
            base = base.wrapping_add(1);
            assert!(
                ring.len() <= DISPATCH_RING_DEPTH,
                "{} dispatches held after {}",
                ring.len(),
                sent.len()
            );
            // Any tag sent in the last SEQUENCE_TAGS dispatches names one
            // dispatch only, so each is either held or gone.
            for (age, &tag) in sent.iter().rev().take(SEQUENCE_TAGS as usize).enumerate() {
                let outcome = ring.resolve(0, 0, tag).map(|(dispatch, _)| dispatch.tag);
                if age < DISPATCH_RING_DEPTH {
                    assert_eq!(outcome, Ok(tag), "dispatch {age} back is still held");
                } else {
                    assert_eq!(
                        outcome,
                        Err(ResultDiscard::StaleSequence),
                        "dispatch {age} back has left the history"
                    );
                }
            }
        }
        assert_eq!(ring.len(), DISPATCH_RING_DEPTH);
    }

    /// NOTHING DISPATCHED BEFORE AN INVALIDATION RESOLVES AFTER IT, WHATEVER
    /// ITS TAG. ReplaceTask (`clean_jobs`) and GoIdle invalidate the history,
    /// and a share is only ever built from the work its result resolves to,
    /// so this is the property that keeps a share for invalidated work from
    /// being built at all. The tags the invalidated dispatches carried come
    /// round again within SEQUENCE_TAGS dispatches, so it is checked for
    /// every tag through two full cycles of them.
    #[test]
    fn no_result_resolves_to_work_dispatched_before_an_invalidation() {
        let before = test_task();
        let mut after = test_task();
        after.ntime = before.ntime + 9_000;
        let versions = compute_micro_versions(&before);
        let work = work_for_engine(&before, 0, 0).unwrap();
        let mut ring = DispatchRing::default();
        let mut base = 0u8;
        for _ in 0..DISPATCH_RING_DEPTH {
            ring.begin(dispatch_tag(base), &before, versions, AsicSet::of(&[0]), 1);
            ring.record_engine(0, work);
            base = base.wrapping_add(1);
        }
        ring.invalidate();
        assert_eq!(ring.len(), 0);
        for tag in 0..SEQUENCE_TAGS {
            assert_eq!(
                ring.resolve(0, 0, tag).map(|(dispatch, _)| dispatch.tag),
                Err(ResultDiscard::NoDispatch),
                "tag {tag} resolved after the invalidation"
            );
        }
        let mut resolved = 0;
        for _ in 0..2 * SEQUENCE_TAGS as usize {
            ring.begin(dispatch_tag(base), &after, versions, AsicSet::of(&[0]), 1);
            ring.record_engine(0, work);
            base = base.wrapping_add(1);
            for tag in 0..SEQUENCE_TAGS {
                if let Ok((dispatch, _)) = ring.resolve(0, 0, tag) {
                    resolved += 1;
                    assert_eq!(
                        dispatch.task.ntime, after.ntime,
                        "tag {tag} resolved to work dispatched before the invalidation"
                    );
                }
            }
        }
        assert!(
            resolved > 0,
            "nothing resolved at all: the check proves nothing"
        );
    }

    /// A RETRY TAKES THE PLACE OF THE ATTEMPT IT RETRIES. The counter
    /// advances only when a dispatch succeeds, so one that fails part way is
    /// retried under the same tag, and the chips then hold the retry. The
    /// failed attempt leaves the history instead of taking a slot from a
    /// dispatch whose hits may still be in flight.
    #[test]
    fn a_retried_dispatch_replaces_its_failed_attempt_in_the_history() {
        let task = test_task();
        let versions = compute_micro_versions(&task);
        let work = work_for_engine(&task, 0, 0).unwrap();
        let mut ring = DispatchRing::default();
        let last = DISPATCH_RING_DEPTH as u8 - 1;
        for tag in 0..last {
            ring.begin(tag, &task, versions, AsicSet::of(&[0]), 2);
            ring.record_engine(0, work);
            ring.record_engine(1, work);
        }
        // Fails after engine 0 is written; retried in full.
        ring.begin(last, &task, versions, AsicSet::of(&[0]), 2);
        ring.record_engine(0, work);
        ring.begin(last, &task, versions, AsicSet::of(&[0]), 2);
        ring.record_engine(0, work);
        ring.record_engine(1, work);

        assert_eq!(ring.len(), DISPATCH_RING_DEPTH);
        assert!(
            ring.resolve(0, 1, 0).is_ok(),
            "the failed attempt cost the oldest dispatch its place in the history"
        );
        assert_eq!(
            ring.resolve(0, 1, last)
                .map(|(dispatch, _)| dispatch.engines.len()),
            Ok(2),
            "the tag resolves to the retry, which reached both engines"
        );
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

    /// EVERY ENGINE ADDRESS GETS ITS OWN HEADER, AND NO DISPATCH REPEATS ONE.
    ///
    /// The silicon does not split work (measured on hardware: the same job on different
    /// ASICs and engines returns the same nonce), so identical headers are
    /// identical hashing. This used to give every engine one header, and
    /// every dispatch of a task the same header again within the same second.
    #[tokio::test]
    async fn every_engine_gets_its_own_header_and_no_dispatch_repeats_one() {
        let pty = openpty(None, None).unwrap();
        let writer_side =
            SerialStream::from_fd(pty.master.into_raw_fd(), SerialConfig::default()).unwrap();
        let reader_side =
            SerialStream::from_fd(pty.slave.into_raw_fd(), SerialConfig::default()).unwrap();
        let (_reader_a, mut writer, _control_a) = writer_side.split();
        let (mut reader, _writer_b, _control_b) = reader_side.split();

        // A real coinbase: block 881,423's, whose merkle root is known.
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
        task.en2_range = Some(range.clone());
        task.en2 = Some(crate::job_source::Extranonce2::new(7, en2_size).unwrap());

        let mut config = Bzm2ThreadConfig::new("/dev/null".into(), 5_000_000, 55.0);
        config.asic_ids = vec![0, 1];
        let engine_layout = Bzm2EngineLayout::from_active_coordinates(vec![(0, 0), (1, 0), (2, 0)]);
        let mut engine_dispatches = DispatchRing::default();
        let mut cursor = 0u64;
        let mut midstates_by_dispatch = Vec::new();
        for base_sequence in 0..2u8 {
            dispatch_task_to_board(
                &mut writer,
                &task,
                base_sequence,
                &engine_layout,
                &mut engine_dispatches,
                &config,
                &mut satisfied_interlock(),
                &EngineGate::Ready,
                &mut cursor,
            )
            .await
            .unwrap();
            let mut bytes = Vec::new();
            let mut buf = vec![0u8; 1024];
            while let Ok(Ok(n)) =
                tokio::time::timeout(Duration::from_millis(150), reader.read(&mut buf)).await
            {
                if n == 0 {
                    break;
                }
                bytes.extend_from_slice(&buf[..n]);
            }
            let mut midstates = Vec::new();
            let mut i = 0;
            while i + 2 <= bytes.len() {
                let len = u16::from_le_bytes([bytes[i], bytes[i + 1]]) as usize;
                if len == 48 {
                    midstates.push(bytes[i + 6..i + 38].to_vec());
                }
                i += len;
            }
            assert_eq!(midstates.len(), 3 * 4, "three engines, four slots each");
            let distinct: std::collections::HashSet<_> = midstates.iter().collect();
            assert_eq!(
                distinct.len(),
                midstates.len(),
                "two engine slots got the same header in one dispatch"
            );
            // Each engine's record carries the extranonce2 it hashed, and the
            // merkle root that extranonce2 gives -- what a share must name.
            let dispatch = engine_dispatches.newest();
            for work in dispatch.engines.values() {
                let en2 = work.en2.as_ref().unwrap();
                assert_eq!(
                    work.merkle_root,
                    dispatch.task.template.compute_merkle_root(en2).unwrap()
                );
            }
            midstates_by_dispatch.push(midstates);
        }
        let first: std::collections::HashSet<_> = midstates_by_dispatch[0].iter().collect();
        assert!(
            midstates_by_dispatch[1].iter().all(|m| !first.contains(m)),
            "a second dispatch at the same ntime repeated a header"
        );
        assert_eq!(
            cursor, 6,
            "the cursor advances one extranonce2 per engine per dispatch"
        );
    }
}
