use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crate::asic::hash_thread::HashThreadStatus;
use crate::types::{HashRate, HashrateEstimator, LogBudget, Work};

const RUNTIME_MEASUREMENT_WINDOW: Duration = Duration::from_secs(5 * 60);

// Legacy source treats rows 0-9 as the bottom stack (PLL0) and rows 10-19 as
// the top stack (PLL1).
const PLL_STACK_SPLIT_ROW: u8 = 10;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Bzm2PllRuntimeMetrics {
    pub throughput_hs: Option<u64>,
    pub scheduler_share_count: u64,
}

/// Why a result frame did not become a share.
///
/// Every frame the chip returns ends up either forwarded as a share or
/// counted under exactly one of these. Before this existed the five
/// discard sites all collapsed into a silent `None`, which left any
/// measurement that needs a denominator — hit rate per window, nonce-gap
/// verification, sequence desync detection — without one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultDiscard {
    /// The status bits say the frame carries no valid nonce.
    InvalidNonce,
    /// The engine address is not in the active layout.
    UnknownEngine,
    /// No held dispatch reached `(asic, engine)`: a result from work this
    /// thread never sent, or sent before its records were invalidated.
    NoDispatch,
    /// Dispatches reached `(asic, engine)`, but none under the result's
    /// tag: its dispatch has left the bounded history, or was invalidated
    /// by a `clean_jobs` replace. An earlier version also counted every hit
    /// from the dispatch just replaced, which was still valid at the pool.
    StaleSequence,
    /// The reconstructed header hashes above the acceptance target.
    BelowTarget,
}

impl std::fmt::Display for ResultDiscard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidNonce => "invalid_nonce",
            Self::UnknownEngine => "unknown_engine",
            Self::NoDispatch => "no_dispatch",
            Self::StaleSequence => "stale_sequence",
            Self::BelowTarget => "below_target",
        })
    }
}

/// Running tally of result-frame outcomes.
///
/// `decoded` counts frames that reconstructed to a header, whether or not
/// they met the target, so `decoded == accepted + below_target`. The other
/// four reasons stop before a header exists and are counted only in their
/// own field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Bzm2ResultCounters {
    pub decoded: u64,
    pub accepted: u64,
    pub invalid_nonce: u64,
    pub unknown_engine: u64,
    pub no_dispatch: u64,
    pub stale_sequence: u64,
    pub below_target: u64,
}

impl Bzm2ResultCounters {
    fn record_accepted(&mut self) {
        self.decoded = self.decoded.saturating_add(1);
        self.accepted = self.accepted.saturating_add(1);
    }

    fn record_discard(&mut self, reason: ResultDiscard) {
        let slot = match reason {
            ResultDiscard::InvalidNonce => &mut self.invalid_nonce,
            ResultDiscard::UnknownEngine => &mut self.unknown_engine,
            ResultDiscard::NoDispatch => &mut self.no_dispatch,
            ResultDiscard::StaleSequence => &mut self.stale_sequence,
            ResultDiscard::BelowTarget => {
                self.decoded = self.decoded.saturating_add(1);
                &mut self.below_target
            }
        };
        *slot = slot.saturating_add(1);
    }

    /// Field-wise sum, used to derive a thread total from its ASICs so the
    /// total can never disagree with the parts.
    pub(super) fn add(self, other: Self) -> Self {
        Self {
            decoded: self.decoded.saturating_add(other.decoded),
            accepted: self.accepted.saturating_add(other.accepted),
            invalid_nonce: self.invalid_nonce.saturating_add(other.invalid_nonce),
            unknown_engine: self.unknown_engine.saturating_add(other.unknown_engine),
            no_dispatch: self.no_dispatch.saturating_add(other.no_dispatch),
            stale_sequence: self.stale_sequence.saturating_add(other.stale_sequence),
            below_target: self.below_target.saturating_add(other.below_target),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Bzm2AsicRuntimeMetrics {
    pub asic: u8,
    pub throughput_hs: Option<u64>,
    pub scheduler_share_count: u64,
    pub results: Bzm2ResultCounters,
    pub plls: [Bzm2PllRuntimeMetrics; 2],
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Bzm2ThreadRuntimeMetrics {
    pub throughput_hs: Option<u64>,
    /// Sum over `asics`; derived at snapshot time, never stored separately.
    pub results: Bzm2ResultCounters,
    pub asics: Vec<Bzm2AsicRuntimeMetrics>,
}

struct PllRuntimeMeasurement {
    estimator: HashrateEstimator,
    scheduler_share_count: u64,
}

impl PllRuntimeMeasurement {
    fn new() -> Self {
        Self {
            estimator: HashrateEstimator::new(RUNTIME_MEASUREMENT_WINDOW),
            scheduler_share_count: 0,
        }
    }

    fn record_at(&mut self, at: Instant, work: Work) {
        self.estimator.record_at(at, work);
        self.scheduler_share_count = self.scheduler_share_count.saturating_add(1);
    }

    fn snapshot_at(&mut self, now: Instant) -> Bzm2PllRuntimeMetrics {
        Bzm2PllRuntimeMetrics {
            throughput_hs: self
                .estimator
                .settled_hashrate()
                .map(u64::from)
                .or_else(|| {
                    self.estimator
                        .has_samples()
                        .then(|| u64::from(self.estimator.hashrate_at(now)))
                }),
            scheduler_share_count: self.scheduler_share_count,
        }
    }
}

struct AsicRuntimeMeasurement {
    estimator: HashrateEstimator,
    scheduler_share_count: u64,
    results: Bzm2ResultCounters,
    plls: [PllRuntimeMeasurement; 2],
}

impl AsicRuntimeMeasurement {
    fn new() -> Self {
        Self {
            estimator: HashrateEstimator::new(RUNTIME_MEASUREMENT_WINDOW),
            scheduler_share_count: 0,
            results: Bzm2ResultCounters::default(),
            plls: [PllRuntimeMeasurement::new(), PllRuntimeMeasurement::new()],
        }
    }

    fn record_at(&mut self, at: Instant, pll_index: usize, work: Work) {
        self.estimator.record_at(at, work);
        self.scheduler_share_count = self.scheduler_share_count.saturating_add(1);
        self.plls[pll_index].record_at(at, work);
    }

    fn snapshot_at(&mut self, now: Instant, asic: u8) -> Bzm2AsicRuntimeMetrics {
        Bzm2AsicRuntimeMetrics {
            asic,
            throughput_hs: self
                .estimator
                .settled_hashrate()
                .map(u64::from)
                .or_else(|| {
                    self.estimator
                        .has_samples()
                        .then(|| u64::from(self.estimator.hashrate_at(now)))
                }),
            scheduler_share_count: self.scheduler_share_count,
            results: self.results,
            plls: [self.plls[0].snapshot_at(now), self.plls[1].snapshot_at(now)],
        }
    }
}

pub(super) struct ThreadRuntimeMeasurementState {
    estimator: HashrateEstimator,
    asics: BTreeMap<u8, AsicRuntimeMeasurement>,
    /// Budget for the per-result discard log.
    ///
    /// The discard is already counted, per ASIC and per reason, by
    /// `record_result` below -- so the debug record is a second copy of a
    /// fact that already has a home, and it is written once per result frame.
    /// Measured on 2026-09-17: 234,446 discards, all `no_dispatch`, produced
    /// 51 MB in a thirty-second observe window and the unit's API answered 0
    /// of 3 probes
    /// (measured on hardware).
    ///
    /// `no_dispatch` is not a defect in observer mode: there is no job source,
    /// so nothing was dispatched, while the chain keeps returning results from
    /// work the vendor stack gave it before the handover. Every one of them is
    /// expected, and logging each individually buys nothing that the counter
    /// does not already carry.
    pub(super) discard_log: LogBudget,
    /// Budget for the per-accepted-result debug record.
    ///
    /// Same class as `discard_log` but the opposite trigger: this one floods
    /// when the miner is working. At a board's nominal result rate a full
    /// fifteen-field record per result is not something the control board's
    /// flash can absorb.
    pub(super) accepted_log: LogBudget,
}

impl ThreadRuntimeMeasurementState {
    pub(super) fn new() -> Self {
        Self {
            estimator: HashrateEstimator::new(RUNTIME_MEASUREMENT_WINDOW),
            asics: BTreeMap::new(),
            discard_log: LogBudget::new(5, Duration::from_secs(1)),
            accepted_log: LogBudget::new(5, Duration::from_secs(1)),
        }
    }

    pub(super) fn record_at(&mut self, at: Instant, asic: u8, row: u8, work: Work) {
        let pll_index = pll_index_for_row(row);
        self.estimator.record_at(at, work);
        self.asics
            .entry(asic)
            .or_insert_with(AsicRuntimeMeasurement::new)
            .record_at(at, pll_index, work);
    }

    /// Count one result frame's outcome against the ASIC that reported it.
    /// A discard is still attributable: the ASIC id is on the wire even
    /// when nothing else in the frame can be resolved.
    pub(super) fn record_result(&mut self, asic: u8, outcome: Result<(), ResultDiscard>) {
        let counters = &mut self
            .asics
            .entry(asic)
            .or_insert_with(AsicRuntimeMeasurement::new)
            .results;
        match outcome {
            Ok(()) => counters.record_accepted(),
            Err(reason) => counters.record_discard(reason),
        }
    }

    pub(super) fn snapshot_at(&mut self, now: Instant) -> Bzm2ThreadRuntimeMetrics {
        let asics: Vec<Bzm2AsicRuntimeMetrics> = self
            .asics
            .iter_mut()
            .map(|(&asic, measurement)| measurement.snapshot_at(now, asic))
            .collect();
        Bzm2ThreadRuntimeMetrics {
            throughput_hs: self
                .estimator
                .settled_hashrate()
                .map(u64::from)
                .or_else(|| {
                    self.estimator
                        .has_samples()
                        .then(|| u64::from(self.estimator.hashrate_at(now)))
                }),
            results: asics
                .iter()
                .fold(Bzm2ResultCounters::default(), |sum, asic| {
                    sum.add(asic.results)
                }),
            asics,
        }
    }

    fn current_hashrate(
        &mut self,
        now: Instant,
        is_active: bool,
        nominal_hashrate_ths: f64,
    ) -> HashRate {
        if !is_active {
            return HashRate::default();
        }

        let measured = self.estimator.settled_hashrate().or_else(|| {
            self.estimator
                .has_samples()
                .then(|| self.estimator.hashrate_at(now))
        });
        match measured {
            Some(hashrate) if !hashrate.is_zero() => hashrate,
            _ => HashRate::from_terahashes(nominal_hashrate_ths),
        }
    }
}

pub(super) fn refresh_status_hashrate(
    status: &Arc<RwLock<HashThreadStatus>>,
    runtime_measurements: &mut ThreadRuntimeMeasurementState,
    nominal_hashrate_ths: f64,
) {
    let now = Instant::now();
    let mut lock = status.write().unwrap();
    lock.hashrate =
        runtime_measurements.current_hashrate(now, lock.is_active, nominal_hashrate_ths);
}

fn pll_index_for_row(row: u8) -> usize {
    if row < PLL_STACK_SPLIT_ROW { 0 } else { 1 }
}

#[cfg(all(test, unix))]
mod tests {

    use super::*;

    #[test]
    fn result_discard_names_are_the_snake_case_the_logs_carry() {
        // These strings are keys in logged tallies; renaming one breaks every
        // reader of an older log. Pinned here since the derive that produced
        // them is gone.
        let names: Vec<String> = [
            super::ResultDiscard::InvalidNonce,
            super::ResultDiscard::UnknownEngine,
            super::ResultDiscard::NoDispatch,
            super::ResultDiscard::StaleSequence,
            super::ResultDiscard::BelowTarget,
        ]
        .iter()
        .map(|d| d.to_string())
        .collect();
        assert_eq!(
            names,
            [
                "invalid_nonce",
                "unknown_engine",
                "no_dispatch",
                "stale_sequence",
                "below_target"
            ]
        );
    }

    #[test]
    fn runtime_metrics_track_per_asic_and_per_pll_throughput() {
        let base = Instant::now();
        let mut runtime = ThreadRuntimeMeasurementState::new();
        let work = bitcoin::pow::Work::from_le_bytes({
            let mut bytes = [0u8; 32];
            bytes[..8].copy_from_slice(&1_000u64.to_le_bytes());
            bytes
        });

        runtime.record_at(base, 2, 0, work);
        runtime.record_at(base + Duration::from_secs(10), 2, 12, work);
        runtime.record_at(base + Duration::from_secs(20), 2, 12, work);
        let snapshot = runtime.snapshot_at(base + Duration::from_secs(20));

        assert_eq!(snapshot.asics.len(), 1);
        let asic = &snapshot.asics[0];
        assert_eq!(asic.asic, 2);
        assert_eq!(asic.scheduler_share_count, 3);
        assert_eq!(asic.throughput_hs, Some(150));
        assert_eq!(asic.plls[0].scheduler_share_count, 1);
        assert_eq!(asic.plls[0].throughput_hs, Some(50));
        assert_eq!(asic.plls[1].scheduler_share_count, 2);
        assert_eq!(asic.plls[1].throughput_hs, Some(200));
    }
}
