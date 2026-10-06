//! Energy efficiency: joules per terahash, and the provenance of the power
//! figure it was derived from.
//!
//! Efficiency is the objective an efficiency-oriented miner is actually trying
//! to optimise, and it cannot be optimised without being measured. Selecting a
//! lower fixed voltage/frequency point is a *guess* at efficiency; dividing real
//! watts by real hashrate is a measurement of it.
//!
//! The provenance flag is the load-bearing part of this module. Every shipping
//! miner firmware closes its watt-denominated loops on a model rather than a
//! meter, and a model is not a safe basis for a control loop that can drive
//! silicon outside its operating window. Carrying provenance alongside the value
//! lets a controller refuse to act on a number it should not trust, instead of
//! discovering the difference the expensive way.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::types::HashRate;

/// Hashes in one terahash. `HashRate` counts hashes per second, so dividing by
/// this converts to TH/s.
const HASHES_PER_TERAHASH: f64 = 1e12;

/// Where a power figure came from.
///
/// Ordering matters: [`Estimated`](Self::Estimated) is deliberately the weaker
/// value, and any combination of readings takes the weakest provenance present.
/// A figure derived partly from an estimate is an estimate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerProvenance {
    /// Read from a real meter on the board — a PMBus regulator, a shunt
    /// monitor, or equivalent. Safe to close a control loop on.
    Measured,
    /// Derived rather than metered: a model, a nameplate figure, or a coarser
    /// measurement apportioned across domains.
    ///
    /// Useful for display and for trend-watching. **Not** a safe basis for a
    /// loop that moves voltage or frequency.
    Estimated,
}

impl PowerProvenance {
    /// Human-facing label, used in telemetry and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Measured => "measured",
            Self::Estimated => "estimated",
        }
    }

    /// Whether a control loop may act on a figure with this provenance.
    pub fn is_actionable(self) -> bool {
        matches!(self, Self::Measured)
    }

    /// Combine two provenances, taking the weaker.
    ///
    /// Any arithmetic mixing a measured and an estimated figure yields an
    /// estimate — there is no such thing as a partly-measured number.
    pub fn combine(self, other: Self) -> Self {
        if self == Self::Measured && other == Self::Measured {
            Self::Measured
        } else {
            Self::Estimated
        }
    }
}

/// Where in the delivery chain a power figure was taken.
///
/// There is no single "J/TH" — there are three, and the *differences between
/// them* are the diagnostics:
///
/// | measurement | tells you |
/// |---|---|
/// | [`Asic`](Self::Asic) | silicon performance: what the dies actually consume for the work they do |
/// | [`Board`](Self::Board) − [`Asic`](Self::Asic) | **vampire load** — MCU, display, wifi, buzzer, fans: everything that draws power without contributing hashrate |
/// | [`Wall`](Self::Wall) − [`Board`](Self::Board) | **PSU conversion loss** — the cost of the supply you chose |
///
/// Reporting only one of these hides the other two. A board whose silicon
/// efficiency is excellent can still be a poor product if its vampire load is
/// large or its PSU is cheap, and neither is visible from ASIC figures alone.
/// Tracked over time, a rising board delta is how a failing fan announces
/// itself before it fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerDomain {
    /// One ASIC's own consumption.
    ///
    /// On a series-stacked part this is genuinely measurable rather than
    /// apportioned: the on-die voltage sensor gives that die's voltage, and
    /// stack current is common to every ASIC in the string, so
    /// `P = V_die × I_string`.
    Asic,
    /// Everything on the board: the ASICs plus the controller MCU, display,
    /// radio, fans and any other housekeeping load. Measured at the board's
    /// input, or the furthest-upstream sensor available on it.
    Board,
    /// At the wall — a smart PDU, an instrumented PSU, or a USB-C PD supply
    /// that reports draw. Includes the PSU's own conversion loss.
    Wall,
}

impl PowerDomain {
    /// Human-facing label, used in telemetry names and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Asic => "asic",
            Self::Board => "board",
            Self::Wall => "wall",
        }
    }
}

/// A power figure together with where it was taken and where it came from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PowerReading {
    pub watts: f32,
    pub domain: PowerDomain,
    pub provenance: PowerProvenance,
}

impl PowerReading {
    /// A figure read from a real meter at `domain`.
    pub fn measured(domain: PowerDomain, watts: f32) -> Self {
        Self {
            watts,
            domain,
            provenance: PowerProvenance::Measured,
        }
    }

    /// A figure that was modelled, apportioned, or otherwise derived.
    pub fn estimated(domain: PowerDomain, watts: f32) -> Self {
        Self {
            watts,
            domain,
            provenance: PowerProvenance::Estimated,
        }
    }

    /// Overhead between an upstream and a downstream measurement.
    ///
    /// `Board − Σ Asic` is the vampire load; `Wall − Board` is PSU conversion
    /// loss. Returns `None` if the domains are not adjacent in that order, or
    /// if the result is negative — a downstream figure exceeding its upstream
    /// means one of the two sensors is wrong, and silently reporting a negative
    /// overhead would hide that.
    ///
    /// Provenance combines: an overhead derived from any estimate is an
    /// estimate.
    pub fn overhead_over(upstream: Self, downstream: Self) -> Option<Self> {
        let ordered = matches!(
            (upstream.domain, downstream.domain),
            (PowerDomain::Board, PowerDomain::Asic) | (PowerDomain::Wall, PowerDomain::Board)
        );
        if !ordered {
            return None;
        }
        let watts = upstream.watts - downstream.watts;
        if !watts.is_finite() || watts < 0.0 {
            return None;
        }
        Some(Self {
            watts,
            domain: upstream.domain,
            provenance: upstream.provenance.combine(downstream.provenance),
        })
    }

    /// Split this reading into `fraction` of itself.
    ///
    /// Used to apportion a board-level measurement across domains by their
    /// share of the work. The result is always [`Estimated`][PowerProvenance::Estimated]
    /// however good the original measurement was: apportioning assumes the
    /// domains are identical, and they are not — that assumption is precisely
    /// what per-domain efficiency measurement exists to disprove.
    pub fn apportion(self, domain: PowerDomain, fraction: f32) -> Self {
        Self {
            watts: self.watts * fraction,
            domain,
            provenance: PowerProvenance::Estimated,
        }
    }
}

/// Energy efficiency in joules per terahash.
///
/// `W / (TH/s)` reduces to `J/TH` directly, since a watt is a joule per second.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Efficiency {
    pub joules_per_terahash: f32,
    /// Which power measurement the numerator came from. A J/TH figure is
    /// meaningless without it — silicon, board and wall efficiency are three
    /// different numbers for the same miner.
    pub domain: PowerDomain,
    pub provenance: PowerProvenance,
}

impl Efficiency {
    /// Compute efficiency from a power reading and a hashrate.
    ///
    /// Returns `None` for a zero or absent hashrate rather than an infinity: a
    /// board doing no work has no meaningful efficiency, and propagating an
    /// infinity into a control loop is worse than propagating nothing.
    ///
    /// Callers should source `hashrate` from
    /// [`HashrateEstimator::settled_hashrate`][crate::types::HashrateEstimator::settled_hashrate]
    /// — an unsettled estimate divides a real numerator by a noisy denominator
    /// and produces a confidently wrong answer.
    pub fn from_power_and_hashrate(power: PowerReading, hashrate: HashRate) -> Option<Self> {
        let terahashes_per_second = f64::from(hashrate) / HASHES_PER_TERAHASH;
        if terahashes_per_second <= 0.0 || !power.watts.is_finite() || power.watts < 0.0 {
            return None;
        }
        Some(Self {
            joules_per_terahash: (f64::from(power.watts) / terahashes_per_second) as f32,
            domain: power.domain,
            provenance: power.provenance,
        })
    }

    /// Whether a control loop may act on this figure.
    pub fn is_actionable(self) -> bool {
        self.provenance.is_actionable()
    }
}

/// Lookback windows worth reporting, shortest first.
///
/// An instantaneous J/TH is noise. Share discovery is Poisson, so the work term
/// — the denominator — carries variance that only averages out with time. Five
/// minutes is usable for a trend, an hour is decent, a day is where the number
/// stops moving. Report several rather than pretending one is authoritative.
pub const EFFICIENCY_WINDOWS: [Duration; 6] = [
    Duration::from_secs(5 * 60),
    Duration::from_secs(15 * 60),
    Duration::from_secs(30 * 60),
    Duration::from_secs(60 * 60),
    Duration::from_secs(24 * 60 * 60),
    Duration::from_secs(72 * 60 * 60),
];

/// Resolution of the history ring.
///
/// Ten seconds, chosen for the *short* windows rather than the long ones: at
/// one-minute resolution a five-minute window is only five buckets, so a single
/// partly-filled bucket at either end skews it noticeably. At ten seconds it is
/// thirty, and the edge effect stops mattering. The long windows are unaffected
/// either way — a 24-hour figure does not care about its final bucket.
///
/// The cost is bounded and small. A 72-hour history is 25,920 buckets at 40
/// bytes each, so ~1 MiB per domain and ~4 MiB for a four-ASIC board. Mujina is
/// a hosted daemon — tokio, axum, no `no_std` — so it runs on a Linux host such
/// as a Pi 5 rather than on a board MCU, and a few MiB there is free. The Pico
/// 2W on bitaxeBIRDS is the bridge this talks *to*, not a host it runs on.
const BUCKET: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
struct Bucket {
    start: Instant,
    joules: f64,
    /// Hashes completed, as counted by accepted share work.
    hashes: f64,
    /// Weakest provenance of any power sample folded into this bucket.
    provenance: PowerProvenance,
}

/// Rolling history of energy and work, from which efficiency over any lookback
/// window can be answered.
///
/// Energy is integrated from power samples rather than sampled instantaneously:
/// `∫P dt` over the same interval the work was completed in is the definition of
/// the energy that did that work. Dividing a spot wattage by a windowed hashrate
/// instead would mix a value from one instant with a rate from a different span,
/// which is only correct if power is constant — and the entire point of a tuning
/// controller is that it is not.
///
/// One bucket ring serves every window: a window is a suffix sum.
#[derive(Debug)]
pub struct EfficiencyHistory {
    domain: PowerDomain,
    buckets: VecDeque<Bucket>,
    retain: Duration,
    starts_at: Option<Instant>,
    last_sample: Option<(Instant, f32)>,
    last_hashrate: Option<(Instant, f64)>,
    work_source: Option<WorkSource>,
}

impl EfficiencyHistory {
    /// History retaining the longest window in [`EFFICIENCY_WINDOWS`].
    ///
    /// Retention runs one bucket past that window so the longest window is
    /// answerable at steady state rather than flapping on the pruning boundary.
    pub fn new(domain: PowerDomain) -> Self {
        let longest = EFFICIENCY_WINDOWS
            .iter()
            .copied()
            .max()
            .unwrap_or(Duration::from_secs(3600));
        Self::with_retention(domain, longest + BUCKET)
    }

    pub fn with_retention(domain: PowerDomain, retain: Duration) -> Self {
        Self {
            domain,
            buckets: VecDeque::new(),
            retain,
            starts_at: None,
            last_sample: None,
            last_hashrate: None,
            work_source: None,
        }
    }

    pub fn domain(&self) -> PowerDomain {
        self.domain
    }

    /// Fold a power sample into the history.
    ///
    /// Energy is accrued by trapezoidal integration against the previous sample,
    /// so an irregular sampling cadence does not bias the total. The first
    /// sample establishes a baseline and contributes no energy — there is no
    /// interval behind it to integrate over.
    pub fn record_power_at(&mut self, at: Instant, power: PowerReading) {
        self.starts_at.get_or_insert(at);
        if let Some((prev_at, prev_watts)) = self.last_sample
            && at > prev_at
        {
            let dt = at.duration_since(prev_at).as_secs_f64();
            let joules = 0.5 * (f64::from(prev_watts) + f64::from(power.watts)) * dt;
            self.bucket_at(at).joules += joules;
            let bucket = self.bucket_at(at);
            bucket.provenance = bucket.provenance.combine(power.provenance);
        }
        self.last_sample = Some((at, power.watts));
        self.prune(at);
    }

    /// Fold completed work into the history.
    ///
    /// `hashes` is the expected hash count the share represents, the same figure
    /// [`HashrateEstimator`][crate::types::HashrateEstimator] accumulates.
    ///
    /// Mutually exclusive with [`record_hashrate_at`](Self::record_hashrate_at)
    /// — see that method for why.
    pub fn record_work_at(&mut self, at: Instant, hashes: f64) {
        self.claim_work_source(WorkSource::Events);
        self.starts_at.get_or_insert(at);
        self.bucket_at(at).hashes += hashes;
        self.prune(at);
    }

    /// Fold a hashrate sample into the history.
    ///
    /// The rate analogue of [`record_power_at`](Self::record_power_at): work is
    /// accrued by trapezoidal integration between samples, by the same rule and
    /// on the same clock as the energy. A windowed J/TH is then the ratio of two
    /// integrals taken over one interval, rather than a value from one instant
    /// divided by a rate from another.
    ///
    /// Use this **or** [`record_work_at`](Self::record_work_at), never both.
    /// One integrates a rate and the other sums completed events; feeding both
    /// counts the same work twice, which *halves* the reported J/TH. A
    /// controller reading that would believe it had headroom it does not have
    /// and ramp into the ceiling. Mixing them trips a debug assertion.
    ///
    /// `hashrate` should come from
    /// [`HashrateEstimator::settled_hashrate`][crate::types::HashrateEstimator::settled_hashrate],
    /// which is itself a windowed average — so this integral inherits that
    /// window's lag, and the shortest efficiency windows trail a step change by
    /// roughly it. The hour-and-longer windows are unaffected.
    pub fn record_hashrate_at(&mut self, at: Instant, hashrate: HashRate) {
        self.claim_work_source(WorkSource::Rate);
        self.starts_at.get_or_insert(at);
        let hashes_per_second = f64::from(hashrate);
        if let Some((prev_at, prev_rate)) = self.last_hashrate
            && at > prev_at
        {
            let dt = at.duration_since(prev_at).as_secs_f64();
            self.bucket_at(at).hashes += 0.5 * (prev_rate + hashes_per_second) * dt;
        }
        self.last_hashrate = Some((at, hashes_per_second));
        self.prune(at);
    }

    /// Efficiency over the trailing `window`, or `None` if the window holds no
    /// completed work — or if the history does not yet reach back across it.
    ///
    /// Returns `None` rather than a large number when work is absent: a window
    /// in which the board found nothing has no measured efficiency, however many
    /// joules it burned. That distinction matters to a controller.
    ///
    /// It also returns `None` until the history actually spans `window`. A
    /// figure labelled "24 h" that was computed from ten minutes of data is a
    /// five-minute figure wearing a day's authority — and the entire reason for
    /// reporting several windows is that the longer ones carry less variance.
    /// A miner therefore starts reporting each window as it earns it.
    pub fn efficiency_over(&self, window: Duration, now: Instant) -> Option<Efficiency> {
        let cutoff = now.checked_sub(window)?;
        if !self.covers(cutoff) {
            return None;
        }
        let mut joules = 0.0;
        let mut hashes = 0.0;
        let mut provenance = PowerProvenance::Measured;
        let mut saw_any = false;

        for bucket in self.buckets.iter().filter(|b| b.start >= cutoff) {
            joules += bucket.joules;
            hashes += bucket.hashes;
            if bucket.joules > 0.0 {
                provenance = provenance.combine(bucket.provenance);
                saw_any = true;
            }
        }

        let terahashes = hashes / HASHES_PER_TERAHASH;
        if !saw_any || terahashes <= 0.0 || joules <= 0.0 {
            return None;
        }
        Some(Efficiency {
            joules_per_terahash: (joules / terahashes) as f32,
            domain: self.domain,
            provenance,
        })
    }

    /// Average hashrate over the trailing `window`.
    ///
    /// Reported alongside efficiency for the same reason: a short-window
    /// hashrate is dominated by share variance, and a J/TH figure is only as
    /// trustworthy as the hashrate underneath it.
    pub fn hashrate_over(&self, window: Duration, now: Instant) -> Option<HashRate> {
        let cutoff = now.checked_sub(window)?;
        if !self.covers(cutoff) {
            return None;
        }

        // A bucket holds what accrued over the interval *ending* at its start,
        // so the span the included buckets actually cover reaches back to the
        // bucket before the first one inside the window. Dividing by the
        // nominal window instead counts a leading partial bucket's work against
        // a shorter period and overstates the rate — by ~3% at five minutes,
        // which is enough to be noticed against a pool's own figure. This
        // cancels in `efficiency_over`, where the same bucket set forms both
        // the numerator and the denominator.
        let mut hashes = 0.0;
        let mut span_start = None;
        let mut preceding = None;
        for bucket in &self.buckets {
            if bucket.start >= cutoff {
                span_start.get_or_insert(preceding.unwrap_or(bucket.start));
                hashes += bucket.hashes;
            } else {
                preceding = Some(bucket.start);
            }
        }

        let secs = now.duration_since(span_start?).as_secs_f64();
        if hashes <= 0.0 || secs <= 0.0 {
            return None;
        }
        Some(HashRate::from((hashes / secs) as u64))
    }

    /// Efficiency at every window in [`EFFICIENCY_WINDOWS`] that has data.
    pub fn all_windows(&self, now: Instant) -> Vec<(Duration, Efficiency)> {
        EFFICIENCY_WINDOWS
            .iter()
            .filter_map(|&w| self.efficiency_over(w, now).map(|e| (w, e)))
            .collect()
    }

    /// When this history last received a power sample.
    ///
    /// The tracker uses it to withhold work from a domain whose sensor did not
    /// report on the current poll.
    fn last_power_sample_at(&self) -> Option<Instant> {
        self.last_sample.map(|(at, _)| at)
    }

    /// Whether recorded history reaches back to `cutoff`.
    fn covers(&self, cutoff: Instant) -> bool {
        self.starts_at.is_some_and(|start| start <= cutoff)
    }

    fn claim_work_source(&mut self, source: WorkSource) {
        debug_assert!(
            self.work_source.is_none_or(|existing| existing == source),
            "efficiency history mixed {:?} and {source:?} work: the same hashes would be counted \
             twice and J/TH would read half its true value",
            self.work_source,
        );
        self.work_source = Some(source);
    }

    fn bucket_at(&mut self, at: Instant) -> &mut Bucket {
        let needs_new = match self.buckets.back() {
            Some(last) => at.duration_since(last.start) >= BUCKET,
            None => true,
        };
        if needs_new {
            self.buckets.push_back(Bucket {
                start: at,
                joules: 0.0,
                hashes: 0.0,
                provenance: PowerProvenance::Measured,
            });
        }
        self.buckets.back_mut().expect("just ensured non-empty")
    }

    fn prune(&mut self, now: Instant) {
        let Some(cutoff) = now.checked_sub(self.retain) else {
            return;
        };
        while self.buckets.front().is_some_and(|b| b.start < cutoff) {
            self.buckets.pop_front();
            // Once the oldest data is dropped the history no longer reaches as
            // far back as it did, and the coverage test must follow it forward.
            self.starts_at = self.buckets.front().map(|bucket| bucket.start);
        }
    }
}

/// One power domain's efficiency across the windows it can answer.
#[derive(Debug, Clone, PartialEq)]
pub struct DomainEfficiency {
    pub domain: PowerDomain,
    pub samples: Vec<EfficiencySample>,
}

/// Efficiency over one lookback window, with the hashrate it was divided by.
///
/// The hashrate travels with the figure because a J/TH is only as trustworthy
/// as its denominator, and a reader cannot judge one without seeing the other.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EfficiencySample {
    pub window: Duration,
    pub efficiency: Efficiency,
    pub hashrate: Option<HashRate>,
}

/// Which side of a poll a work figure arrived from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkSource {
    /// Completed-work events, summed.
    Events,
    /// Rate samples, integrated.
    Rate,
}

/// Efficiency across every power domain a board can measure.
///
/// The work stream is shared and the power streams are not: the same hashes are
/// the denominator of silicon, board and wall efficiency alike, and only the
/// numerator differs. Keeping one tracker rather than a history per call site
/// means work is recorded once and cannot drift between domains.
///
/// Domains appear as their sensors first report. A board with no wall meter
/// simply never grows a [`Wall`][PowerDomain::Wall] history, rather than
/// carrying an empty one that reads as a measurement failure.
#[derive(Debug, Default)]
pub struct EfficiencyTracker {
    histories: Vec<EfficiencyHistory>,
}

impl EfficiencyTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold a power sample into its domain's history, creating it on first use.
    pub fn record_power_at(&mut self, at: Instant, power: PowerReading) {
        let domain = power.domain;
        self.history_mut(domain).record_power_at(at, power);
    }

    /// Fold a hashrate sample into every domain that reported power at `at`.
    ///
    /// Withholding work from a domain whose sensor did not report is the whole
    /// point. Energy only accrues where power was sampled, so crediting a
    /// silent domain with the work anyway would grow its denominator while its
    /// numerator stood still, and its J/TH would improve for exactly as long as
    /// its sensor stayed broken — a failure that reads as an optimisation.
    ///
    /// Record power for the poll before calling this.
    pub fn record_hashrate_at(&mut self, at: Instant, hashrate: HashRate) {
        for history in &mut self.histories {
            if history.last_power_sample_at() == Some(at) {
                history.record_hashrate_at(at, hashrate);
            }
        }
    }

    /// Efficiency for one domain over one window.
    pub fn efficiency_over(
        &self,
        domain: PowerDomain,
        window: Duration,
        now: Instant,
    ) -> Option<Efficiency> {
        self.history(domain)?.efficiency_over(window, now)
    }

    /// Every domain's efficiency at every window it has earned, in the order the
    /// domains first reported. Domains with nothing to say are omitted.
    pub fn report(&self, now: Instant) -> Vec<DomainEfficiency> {
        self.histories
            .iter()
            .map(|history| DomainEfficiency {
                domain: history.domain(),
                samples: history
                    .all_windows(now)
                    .into_iter()
                    .map(|(window, efficiency)| EfficiencySample {
                        window,
                        efficiency,
                        hashrate: history.hashrate_over(window, now),
                    })
                    .collect(),
            })
            .filter(|report| !report.samples.is_empty())
            .collect()
    }

    fn history(&self, domain: PowerDomain) -> Option<&EfficiencyHistory> {
        self.histories
            .iter()
            .find(|history| history.domain() == domain)
    }

    fn history_mut(&mut self, domain: PowerDomain) -> &mut EfficiencyHistory {
        if let Some(index) = self
            .histories
            .iter()
            .position(|history| history.domain() == domain)
        {
            return &mut self.histories[index];
        }
        self.histories.push(EfficiencyHistory::new(domain));
        self.histories.last_mut().expect("just pushed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joules_per_terahash_is_watts_over_terahashes_per_second() {
        // 1 TH/s drawing 20 W is 20 J/TH by definition.
        let e = Efficiency::from_power_and_hashrate(
            PowerReading::measured(PowerDomain::Board, 20.0),
            HashRate::from_terahashes(1.0),
        )
        .unwrap();
        assert!((e.joules_per_terahash - 20.0).abs() < 1e-3);

        // Doubling the hashrate at the same power halves J/TH.
        let e = Efficiency::from_power_and_hashrate(
            PowerReading::measured(PowerDomain::Board, 20.0),
            HashRate::from_terahashes(2.0),
        )
        .unwrap();
        assert!((e.joules_per_terahash - 10.0).abs() < 1e-3);
    }

    #[test]
    fn zero_hashrate_yields_none_rather_than_infinity() {
        assert!(
            Efficiency::from_power_and_hashrate(
                PowerReading::measured(PowerDomain::Board, 20.0),
                HashRate::from(0u64),
            )
            .is_none(),
            "an idle board has no efficiency; an infinity here would poison a control loop"
        );
    }

    #[test]
    fn provenance_survives_the_division() {
        let measured = Efficiency::from_power_and_hashrate(
            PowerReading::measured(PowerDomain::Board, 20.0),
            HashRate::from_terahashes(1.0),
        )
        .unwrap();
        assert!(measured.is_actionable());

        let estimated = Efficiency::from_power_and_hashrate(
            PowerReading::estimated(PowerDomain::Board, 20.0),
            HashRate::from_terahashes(1.0),
        )
        .unwrap();
        assert!(
            !estimated.is_actionable(),
            "an estimate must never present itself as actionable"
        );
        assert_eq!(
            estimated.joules_per_terahash, measured.joules_per_terahash,
            "same arithmetic, different trust"
        );
    }

    #[test]
    fn apportioning_a_measurement_downgrades_it() {
        let board = PowerReading::measured(PowerDomain::Board, 80.0);
        let per_asic = board.apportion(PowerDomain::Asic, 0.25);
        assert!((per_asic.watts - 20.0).abs() < 1e-3);
        assert_eq!(
            per_asic.provenance,
            PowerProvenance::Estimated,
            "apportioning assumes identical domains, which is the assumption per-domain \
             measurement exists to disprove"
        );
    }

    #[test]
    fn tier_deltas_are_the_diagnostics() {
        // 4 ASICs at 15 W each = 60 W of silicon.
        // Board draws 72 W  -> 12 W vampire load (MCU, radio, display, fans).
        // Wall draws  85 W  -> 13 W PSU conversion loss.
        let asics = PowerReading::measured(PowerDomain::Asic, 60.0);
        let board = PowerReading::measured(PowerDomain::Board, 72.0);
        let wall = PowerReading::measured(PowerDomain::Wall, 85.0);

        let vampire = PowerReading::overhead_over(board, asics).unwrap();
        assert!((vampire.watts - 12.0).abs() < 1e-3);
        assert_eq!(vampire.provenance, PowerProvenance::Measured);

        let psu_loss = PowerReading::overhead_over(wall, board).unwrap();
        assert!((psu_loss.watts - 13.0).abs() < 1e-3);
    }

    #[test]
    fn overhead_requires_adjacent_domains_in_order() {
        let asics = PowerReading::measured(PowerDomain::Asic, 60.0);
        let board = PowerReading::measured(PowerDomain::Board, 72.0);
        let wall = PowerReading::measured(PowerDomain::Wall, 85.0);

        // Backwards: a downstream figure cannot bound an upstream one.
        assert!(PowerReading::overhead_over(asics, board).is_none());
        // Skipping a tier would silently fold vampire load into PSU loss.
        assert!(PowerReading::overhead_over(wall, asics).is_none());
    }

    #[test]
    fn downstream_exceeding_upstream_is_rejected_not_reported_negative() {
        // ASICs cannot consume more than the board they sit on. If the numbers
        // say otherwise, a sensor is wrong — and a negative vampire load would
        // hide that behind a plausible-looking figure.
        let asics = PowerReading::measured(PowerDomain::Asic, 90.0);
        let board = PowerReading::measured(PowerDomain::Board, 72.0);
        assert!(PowerReading::overhead_over(board, asics).is_none());
    }

    #[test]
    fn an_estimated_tier_taints_the_overhead() {
        let asics = PowerReading::estimated(PowerDomain::Asic, 60.0);
        let board = PowerReading::measured(PowerDomain::Board, 72.0);
        let vampire = PowerReading::overhead_over(board, asics).unwrap();
        assert_eq!(
            vampire.provenance,
            PowerProvenance::Estimated,
            "a vampire-load figure derived from an apportioned ASIC total is not measured"
        );
    }

    #[test]
    fn the_same_miner_has_three_different_efficiencies() {
        let rate = HashRate::from_terahashes(4.0);
        let silicon = Efficiency::from_power_and_hashrate(
            PowerReading::measured(PowerDomain::Asic, 60.0),
            rate,
        )
        .unwrap();
        let board = Efficiency::from_power_and_hashrate(
            PowerReading::measured(PowerDomain::Board, 72.0),
            rate,
        )
        .unwrap();
        let wall = Efficiency::from_power_and_hashrate(
            PowerReading::measured(PowerDomain::Wall, 85.0),
            rate,
        )
        .unwrap();

        assert!((silicon.joules_per_terahash - 15.0).abs() < 1e-3);
        assert!((board.joules_per_terahash - 18.0).abs() < 1e-3);
        assert!((wall.joules_per_terahash - 21.25).abs() < 1e-3);

        // The domain is not decoration: quoting one of these as "the" J/TH
        // overstates the miner by up to 40%.
        assert_eq!(silicon.domain, PowerDomain::Asic);
        assert_eq!(wall.domain, PowerDomain::Wall);
        assert!(wall.joules_per_terahash > silicon.joules_per_terahash);
    }

    #[test]
    fn mixing_provenance_takes_the_weaker() {
        use PowerProvenance::{Estimated, Measured};
        assert_eq!(Measured.combine(Measured), Measured);
        assert_eq!(Measured.combine(Estimated), Estimated);
        assert_eq!(Estimated.combine(Measured), Estimated);
        assert_eq!(Estimated.combine(Estimated), Estimated);
    }

    #[test]
    fn windowed_efficiency_integrates_energy_over_completed_work() {
        let t0 = Instant::now();
        let mut h = EfficiencyHistory::new(PowerDomain::Board);

        // Hold 20 W for an hour while completing 3600 TH of work.
        // 20 W * 3600 s = 72,000 J over 3600 TH => 20 J/TH.
        h.record_power_at(t0, PowerReading::measured(PowerDomain::Board, 20.0));
        for minute in 1..=60 {
            let at = t0 + Duration::from_secs(minute * 60);
            h.record_power_at(at, PowerReading::measured(PowerDomain::Board, 20.0));
            h.record_work_at(at, 60.0 * HASHES_PER_TERAHASH);
        }

        let now = t0 + Duration::from_secs(3600);
        let e = h
            .efficiency_over(Duration::from_secs(3600), now)
            .expect("an hour of data should yield an efficiency");
        assert!(
            (e.joules_per_terahash - 20.0).abs() < 0.5,
            "expected ~20 J/TH, got {}",
            e.joules_per_terahash
        );
        assert!(e.is_actionable());
    }

    #[test]
    fn a_window_with_no_work_has_no_efficiency() {
        let t0 = Instant::now();
        let mut h = EfficiencyHistory::new(PowerDomain::Board);
        // Burning power while finding nothing is not "infinitely inefficient" —
        // it is unmeasured, and a controller must be able to tell the difference.
        h.record_power_at(t0, PowerReading::measured(PowerDomain::Board, 20.0));
        h.record_power_at(
            t0 + Duration::from_secs(600),
            PowerReading::measured(PowerDomain::Board, 20.0),
        );
        // The window is fully covered by recorded history, so this `None` means
        // "no work", not "not enough history yet".
        assert!(
            h.efficiency_over(Duration::from_secs(600), t0 + Duration::from_secs(600))
                .is_none()
        );
    }

    #[test]
    fn one_estimated_sample_taints_the_whole_window() {
        let t0 = Instant::now();
        let mut h = EfficiencyHistory::new(PowerDomain::Board);
        h.record_power_at(t0, PowerReading::measured(PowerDomain::Board, 20.0));
        for minute in 1..=10 {
            let at = t0 + Duration::from_secs(minute * 60);
            let p = if minute == 5 {
                PowerReading::estimated(PowerDomain::Board, 20.0)
            } else {
                PowerReading::measured(PowerDomain::Board, 20.0)
            };
            h.record_power_at(at, p);
            h.record_work_at(at, 60.0 * HASHES_PER_TERAHASH);
        }
        let e = h
            .efficiency_over(Duration::from_secs(600), t0 + Duration::from_secs(600))
            .unwrap();
        assert!(
            !e.is_actionable(),
            "a window containing any estimated power must not read as measured"
        );
    }

    #[test]
    fn history_is_bounded_regardless_of_sample_rate() {
        let t0 = Instant::now();
        let mut h =
            EfficiencyHistory::with_retention(PowerDomain::Board, Duration::from_secs(3600));
        // One sample per second for two hours: 7,200 raw samples, but retention
        // is one hour and buckets are ten seconds, so ~360 entries survive.
        for i in 0..7_200 {
            h.record_power_at(
                t0 + Duration::from_secs(i),
                PowerReading::measured(PowerDomain::Board, 20.0),
            );
        }
        let expected = 3600 / BUCKET.as_secs() as usize;
        assert!(
            h.buckets.len() <= expected + 2,
            "one hour of retention at {}s buckets should stay ~{expected} entries, got {}",
            BUCKET.as_secs(),
            h.buckets.len()
        );
    }

    #[test]
    fn footprint_of_the_longest_window_stays_small() {
        // The whole point of bucketing rather than keeping raw samples. A
        // 72-hour history must remain a few MiB on a Linux host, not a design
        // constraint. Mujina is a hosted daemon, so this is Pi-class memory.
        let buckets =
            EFFICIENCY_WINDOWS.iter().copied().max().unwrap().as_secs() / BUCKET.as_secs();
        let bytes = buckets as usize * std::mem::size_of::<Bucket>();
        assert!(
            bytes < 2 * 1024 * 1024,
            "72h history is {bytes} B ({} KiB) per domain — expected under 2 MiB",
            bytes / 1024
        );
    }

    #[test]
    fn integrating_a_rate_agrees_with_summing_the_events() {
        // The same physical run, fed both ways: 1 TH/s at 20 W for ten minutes.
        // If the two paths disagreed, the choice of feed would change the
        // answer, and the rate path exists precisely because the event path is
        // not available at the board layer.
        let t0 = Instant::now();
        let mut events = EfficiencyHistory::new(PowerDomain::Board);
        let mut rate = EfficiencyHistory::new(PowerDomain::Board);

        for step in 0..=60 {
            let at = t0 + Duration::from_secs(step * 10);
            let power = PowerReading::measured(PowerDomain::Board, 20.0);
            events.record_power_at(at, power);
            rate.record_power_at(at, power);
            rate.record_hashrate_at(at, HashRate::from_terahashes(1.0));
            if step > 0 {
                events.record_work_at(at, 10.0 * HASHES_PER_TERAHASH);
            }
        }

        let now = t0 + Duration::from_secs(600);
        let window = Duration::from_secs(600);
        let from_events = events.efficiency_over(window, now).unwrap();
        let from_rate = rate.efficiency_over(window, now).unwrap();
        assert!(
            (from_events.joules_per_terahash - 20.0).abs() < 0.1,
            "events path gave {}",
            from_events.joules_per_terahash
        );
        assert!(
            (from_rate.joules_per_terahash - from_events.joules_per_terahash).abs() < 0.1,
            "rate path gave {}, events path gave {}",
            from_rate.joules_per_terahash,
            from_events.joules_per_terahash
        );
    }

    #[test]
    fn a_domain_whose_sensor_goes_quiet_stops_accruing_work() {
        // The failure this guards against: crediting work to a domain that is
        // no longer reporting power makes its J/TH *improve* for as long as the
        // sensor stays broken. A sensor failure must not look like a win.
        let t0 = Instant::now();
        let mut tracker = EfficiencyTracker::new();

        for step in 0..=60 {
            let at = t0 + Duration::from_secs(step * 10);
            tracker.record_power_at(at, PowerReading::measured(PowerDomain::Board, 20.0));
            if step <= 30 {
                tracker.record_power_at(at, PowerReading::measured(PowerDomain::Asic, 10.0));
            }
            tracker.record_hashrate_at(at, HashRate::from_terahashes(1.0));
        }

        let now = t0 + Duration::from_secs(600);
        let window = Duration::from_secs(600);

        // Board saw every poll: 20 W over 600 s against 600 TH.
        let board = tracker
            .efficiency_over(PowerDomain::Board, window, now)
            .unwrap();
        assert!(
            (board.joules_per_terahash - 20.0).abs() < 0.1,
            "board gave {}",
            board.joules_per_terahash
        );

        // ASIC saw the first half: 10 W over 300 s against 300 TH, still 10
        // J/TH. Had the quiet half been credited, work would have doubled and
        // this would read ~5.
        let asic = tracker
            .efficiency_over(PowerDomain::Asic, window, now)
            .unwrap();
        assert!(
            (asic.joules_per_terahash - 10.0).abs() < 0.1,
            "expected ASIC efficiency to hold at its last measured value, got {}",
            asic.joules_per_terahash
        );
    }

    #[test]
    fn the_tracker_reports_every_domain_it_has_seen() {
        let t0 = Instant::now();
        let mut tracker = EfficiencyTracker::new();
        for step in 0..=60 {
            let at = t0 + Duration::from_secs(step * 10);
            tracker.record_power_at(at, PowerReading::measured(PowerDomain::Asic, 60.0));
            tracker.record_power_at(at, PowerReading::measured(PowerDomain::Board, 72.0));
            tracker.record_hashrate_at(at, HashRate::from_terahashes(4.0));
        }

        let report = tracker.report(t0 + Duration::from_secs(600));
        assert_eq!(report.len(), 2, "both measured domains should be reported");
        assert_eq!(report[0].domain, PowerDomain::Asic);
        assert!(
            report[0]
                .samples
                .iter()
                .all(|sample| sample.efficiency.domain == PowerDomain::Asic),
            "a sample must carry the domain of the history it came from"
        );
        // Ten minutes of data answers the five-minute window and nothing
        // longer: a window is reported once it is earned, not before.
        assert_eq!(report[0].samples.len(), 1);
        assert_eq!(report[0].samples[0].window, Duration::from_secs(300));
        // The denominator travels with the figure, and it is the rate that was
        // actually sustained rather than one inflated by a partial edge bucket.
        let hashrate = report[0].samples[0]
            .hashrate
            .expect("the denominator must travel with the figure");
        let terahashes = f64::from(hashrate) / HASHES_PER_TERAHASH;
        assert!(
            (terahashes - 4.0).abs() < 0.01,
            "expected the reported 4 TH/s back, got {terahashes} TH/s"
        );

        // A domain that never reported is absent, not zero.
        assert!(
            tracker
                .efficiency_over(PowerDomain::Wall, Duration::from_secs(900), t0)
                .is_none()
        );
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "counted twice")]
    fn mixing_work_events_with_rate_samples_is_caught() {
        let t0 = Instant::now();
        let mut h = EfficiencyHistory::new(PowerDomain::Board);
        h.record_hashrate_at(t0, HashRate::from_terahashes(1.0));
        h.record_work_at(t0 + Duration::from_secs(10), HASHES_PER_TERAHASH);
    }

    #[test]
    fn negative_or_nonfinite_power_is_rejected() {
        for bad in [-1.0f32, f32::NAN, f32::INFINITY] {
            assert!(
                Efficiency::from_power_and_hashrate(
                    PowerReading::measured(PowerDomain::Board, bad),
                    HashRate::from_terahashes(1.0),
                )
                .is_none(),
                "power reading {bad} should not produce an efficiency"
            );
        }
    }
}
