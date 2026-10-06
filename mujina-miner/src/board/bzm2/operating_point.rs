//! Operating points indexed by die temperature.
//!
//! A single stored operating point is only valid at the temperature it was
//! learned at. The functional voltage window slides with junction temperature,
//! so a point learned on a cold bench is a different point in a warm room —
//! cold-start and warm-running are genuinely separate rows, not one row plus a
//! correction.
//!
//! Indexing by temperature is also what makes a warm restart fast: look up the
//! row for the die temperature we are actually at and apply it, instead of
//! re-running a search that has already been run at this temperature before.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::tuning::calibration_planner::Bzm2SavedOperatingPoint;

/// Temperature within which two observations describe the same operating point.
///
/// The window slides about a millivolt per degree, so two degrees is roughly
/// two millivolts of a forty-to-fifty millivolt window — fine enough that
/// merging inside it loses nothing, coarse enough that the table does not grow
/// a row per sample.
const ROW_MERGE_RADIUS_C: f32 = 2.0;

/// How far past the coldest or hottest learned row a lookup may still reach.
///
/// Interpolating *between* two learned rows is defensible: the behaviour
/// between two measured points is bounded by them. Extrapolating past the end
/// of the table is not — there is no measurement out there, and walking a
/// voltage out of the functional window is exactly the failure this table is
/// meant to prevent. Five degrees of edge tolerance is about five millivolts,
/// a tenth of the window, and beyond that a lookup returns nothing and the
/// caller must search.
const EDGE_TOLERANCE_C: f32 = 5.0;

/// Ceiling on stored rows.
///
/// The merge radius already bounds this in practice — a realistic operating
/// span holds a few dozen rows — so this only exists so a stuck or wildly
/// noisy temperature sensor cannot grow the profile without limit.
const MAX_ROWS: usize = 64;

/// Weight given to a new observation when folding it into an existing row.
///
/// Rows converge rather than jumping to the latest sample, so one noisy
/// measurement cannot displace a well-observed point.
const OBSERVATION_WEIGHT: f32 = 0.25;

/// A learned operating point, and the die temperature it was learned at.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub(super) struct Bzm2OperatingPointRow {
    pub(super) die_temp_c: f32,
    pub(super) board_voltage_mv: u32,
    #[serde(default)]
    pub(super) per_domain_voltage_mv: BTreeMap<u16, u32>,
    #[serde(default)]
    pub(super) per_asic_pll_mhz: BTreeMap<u16, [f32; 2]>,
    /// How many observations back this row.
    ///
    /// Borrowed from the confidence field on Braiins' efficiency curve, and for
    /// the same reason: a table that treats one noisy sample as fact degrades
    /// far worse than one that knows which of its rows it trusts.
    #[serde(default)]
    pub(super) observations: u32,
    /// Throughput actually observed at this row, not a planned figure.
    ///
    /// `None` until the board has run here long enough to measure it. The
    /// distinction matters: validating a profile against a modelled baseline
    /// compares it to a number that was never observed.
    #[serde(default)]
    pub(super) measured_throughput_ths: Option<f32>,
    /// Efficiency actually observed at this row, from metered power.
    #[serde(default)]
    pub(super) measured_efficiency_j_th: Option<f32>,
}

impl Bzm2OperatingPointRow {
    /// A row describing `point`, as learned at `die_temp_c`.
    pub(super) fn observed(
        die_temp_c: f32,
        point: &Bzm2SavedOperatingPoint,
        measured_throughput_ths: Option<f32>,
        measured_efficiency_j_th: Option<f32>,
    ) -> Self {
        Self {
            die_temp_c,
            board_voltage_mv: point.board_voltage_mv,
            per_domain_voltage_mv: point.per_domain_voltage_mv.clone(),
            per_asic_pll_mhz: point.per_asic_pll_mhz.clone(),
            observations: 1,
            measured_throughput_ths,
            measured_efficiency_j_th,
        }
    }

    /// This row as an applicable operating point.
    ///
    /// The engine topology comes from `template` rather than the row: which
    /// engines exist is a property of the board, not of the temperature it is
    /// running at, so storing it per row would duplicate it once per row and
    /// invite them to disagree.
    pub(super) fn to_saved_operating_point(
        &self,
        template: &Bzm2SavedOperatingPoint,
    ) -> Bzm2SavedOperatingPoint {
        Bzm2SavedOperatingPoint {
            board_voltage_mv: self.board_voltage_mv,
            // Prefer what was measured here over what was planned elsewhere.
            board_throughput_ths: self
                .measured_throughput_ths
                .unwrap_or(template.board_throughput_ths),
            per_domain_voltage_mv: self.per_domain_voltage_mv.clone(),
            per_asic_engine_topology: template.per_asic_engine_topology.clone(),
            per_asic_pll_mhz: self.per_asic_pll_mhz.clone(),
        }
    }
}

/// Learned operating points, ordered by die temperature.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub(super) struct Bzm2OperatingPointTable {
    #[serde(default)]
    rows: Vec<Bzm2OperatingPointRow>,
}

impl Bzm2OperatingPointTable {
    /// The operating point to use at `die_temp_c`, or `None` to search.
    ///
    /// Between two learned rows the result is interpolated. Outside them it is
    /// the nearest row, but only within [`EDGE_TOLERANCE_C`] — past that the
    /// table declines to guess. An empty answer is not a failure; it is the
    /// table saying this temperature has never been characterised, which is
    /// exactly when a search is the right thing to do.
    pub(super) fn lookup(&self, die_temp_c: f32) -> Option<Bzm2OperatingPointRow> {
        if !die_temp_c.is_finite() || self.rows.is_empty() {
            return None;
        }

        let upper = self
            .rows
            .iter()
            .position(|row| row.die_temp_c >= die_temp_c);
        match upper {
            // Colder than everything learned.
            Some(0) => self.edge_row(0, die_temp_c),
            // Bracketed: the useful case.
            Some(index) => Some(interpolate(
                &self.rows[index - 1],
                &self.rows[index],
                die_temp_c,
            )),
            // Warmer than everything learned.
            None => self.edge_row(self.rows.len() - 1, die_temp_c),
        }
    }

    /// Fold an observation into the table.
    ///
    /// An observation within [`ROW_MERGE_RADIUS_C`] of an existing row updates
    /// it and raises its confidence; anything further away becomes a new row.
    pub(super) fn observe(&mut self, observation: Bzm2OperatingPointRow) {
        if !observation.die_temp_c.is_finite() {
            return;
        }
        match self.nearest_within(observation.die_temp_c, ROW_MERGE_RADIUS_C) {
            Some(index) => merge_into(&mut self.rows[index], observation),
            None => {
                self.rows.push(Bzm2OperatingPointRow {
                    observations: observation.observations.max(1),
                    ..observation
                });
                self.rows
                    .sort_by(|a, b| a.die_temp_c.total_cmp(&b.die_temp_c));
                self.evict_least_observed();
            }
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub(super) fn rows(&self) -> &[Bzm2OperatingPointRow] {
        &self.rows
    }

    /// The row at `index`, if `die_temp_c` is within edge tolerance of it.
    fn edge_row(&self, index: usize, die_temp_c: f32) -> Option<Bzm2OperatingPointRow> {
        let row = self.rows.get(index)?;
        ((row.die_temp_c - die_temp_c).abs() <= EDGE_TOLERANCE_C).then(|| row.clone())
    }

    fn nearest_within(&self, die_temp_c: f32, radius_c: f32) -> Option<usize> {
        self.rows
            .iter()
            .enumerate()
            .map(|(index, row)| (index, (row.die_temp_c - die_temp_c).abs()))
            .filter(|&(_, distance)| distance <= radius_c)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(index, _)| index)
    }

    /// Drop the least-observed row once the table is full.
    ///
    /// Evicting by confidence rather than by age keeps the rows the board has
    /// actually spent time at, which are the ones a restart is most likely to
    /// land on.
    fn evict_least_observed(&mut self) {
        while self.rows.len() > MAX_ROWS {
            let Some(index) = self
                .rows
                .iter()
                .enumerate()
                .min_by_key(|(_, row)| row.observations)
                .map(|(index, _)| index)
            else {
                return;
            };
            self.rows.remove(index);
        }
    }
}

/// Linear interpolation between two bracketing rows.
///
/// Position between the rows is set by temperature alone, never weighted by
/// their confidence. Skewing toward the better-observed row would move the
/// operating point away from where the temperature says it belongs — trading a
/// physical quantity for a statistical one. Confidence instead flows into the
/// *result*: an interpolated row is only as trustworthy as the weaker of the
/// two it came from, and a caller that needs certainty can check it.
fn interpolate(
    lower: &Bzm2OperatingPointRow,
    upper: &Bzm2OperatingPointRow,
    die_temp_c: f32,
) -> Bzm2OperatingPointRow {
    let span = upper.die_temp_c - lower.die_temp_c;
    let t = if span.abs() < f32::EPSILON {
        0.0
    } else {
        ((die_temp_c - lower.die_temp_c) / span).clamp(0.0, 1.0)
    };

    Bzm2OperatingPointRow {
        die_temp_c,
        board_voltage_mv: lerp(
            lower.board_voltage_mv as f32,
            upper.board_voltage_mv as f32,
            t,
        )
        .round() as u32,
        per_domain_voltage_mv: lower
            .per_domain_voltage_mv
            .iter()
            .map(|(&domain, &low)| {
                let high = upper
                    .per_domain_voltage_mv
                    .get(&domain)
                    .copied()
                    .unwrap_or(low);
                (domain, lerp(low as f32, high as f32, t).round() as u32)
            })
            .collect(),
        per_asic_pll_mhz: lower
            .per_asic_pll_mhz
            .iter()
            .map(|(&asic, low)| {
                let high = upper.per_asic_pll_mhz.get(&asic).unwrap_or(low);
                (asic, [lerp(low[0], high[0], t), lerp(low[1], high[1], t)])
            })
            .collect(),
        observations: lower.observations.min(upper.observations),
        measured_throughput_ths: interpolate_optional(
            lower.measured_throughput_ths,
            upper.measured_throughput_ths,
            t,
        ),
        measured_efficiency_j_th: interpolate_optional(
            lower.measured_efficiency_j_th,
            upper.measured_efficiency_j_th,
            t,
        ),
    }
}

/// Fold `observation` into `row`, converging rather than replacing.
fn merge_into(row: &mut Bzm2OperatingPointRow, observation: Bzm2OperatingPointRow) {
    let w = OBSERVATION_WEIGHT;
    row.die_temp_c = lerp(row.die_temp_c, observation.die_temp_c, w);
    row.board_voltage_mv = lerp(
        row.board_voltage_mv as f32,
        observation.board_voltage_mv as f32,
        w,
    )
    .round() as u32;

    for (domain, voltage) in observation.per_domain_voltage_mv {
        let entry = row.per_domain_voltage_mv.entry(domain).or_insert(voltage);
        *entry = lerp(*entry as f32, voltage as f32, w).round() as u32;
    }
    for (asic, frequencies) in observation.per_asic_pll_mhz {
        let entry = row.per_asic_pll_mhz.entry(asic).or_insert(frequencies);
        entry[0] = lerp(entry[0], frequencies[0], w);
        entry[1] = lerp(entry[1], frequencies[1], w);
    }

    row.measured_throughput_ths = converge_optional(
        row.measured_throughput_ths,
        observation.measured_throughput_ths,
        w,
    );
    row.measured_efficiency_j_th = converge_optional(
        row.measured_efficiency_j_th,
        observation.measured_efficiency_j_th,
        w,
    );
    row.observations = row.observations.saturating_add(1);
}

/// Interpolate two optional measurements, yielding `None` unless both exist.
///
/// Substituting whichever side happens to be present would silently report one
/// row's measurement as though it held across the span to the other.
fn interpolate_optional(lower: Option<f32>, upper: Option<f32>, t: f32) -> Option<f32> {
    Some(lerp(lower?, upper?, t))
}

/// Fold a new optional measurement into a stored one.
///
/// A fresh measurement seeds an empty slot; an absent one leaves the stored
/// value alone rather than erasing it, since "not measured this time" is not
/// evidence against what was measured last time.
fn converge_optional(stored: Option<f32>, observed: Option<f32>, weight: f32) -> Option<f32> {
    match (stored, observed) {
        (Some(stored), Some(observed)) => Some(lerp(stored, observed, weight)),
        (stored, None) => stored,
        (None, observed) => observed,
    }
}

fn lerp(from: f32, to: f32, t: f32) -> f32 {
    from + (to - from) * t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(die_temp_c: f32, voltage_mv: u32, mhz: f32) -> Bzm2OperatingPointRow {
        Bzm2OperatingPointRow {
            die_temp_c,
            board_voltage_mv: voltage_mv,
            per_domain_voltage_mv: BTreeMap::from([(0, voltage_mv)]),
            per_asic_pll_mhz: BTreeMap::from([(0, [mhz, mhz])]),
            observations: 1,
            measured_throughput_ths: None,
            measured_efficiency_j_th: None,
        }
    }

    fn two_row_table() -> Bzm2OperatingPointTable {
        let mut table = Bzm2OperatingPointTable::default();
        table.observe(row(40.0, 17_400, 1_100.0));
        table.observe(row(80.0, 17_440, 1_060.0));
        table
    }

    #[test]
    fn a_bracketed_temperature_interpolates() {
        // Halfway between 40 C and 80 C: halfway between the two rows.
        let found = two_row_table().lookup(60.0).unwrap();
        assert_eq!(found.board_voltage_mv, 17_420);
        assert!((found.per_asic_pll_mhz[&0][0] - 1_080.0).abs() < 0.01);
    }

    #[test]
    fn the_slide_is_about_a_millivolt_per_degree() {
        // The premise of indexing by temperature at all: 40 degrees of swing
        // moved the point 40 mV, which is a whole functional window.
        let table = two_row_table();
        let cold = table.lookup(40.0).unwrap().board_voltage_mv;
        let hot = table.lookup(80.0).unwrap().board_voltage_mv;
        assert_eq!(hot - cold, 40);
    }

    #[test]
    fn just_past_the_edge_uses_the_edge_row() {
        let table = two_row_table();
        // Three degrees colder than the coldest row is within tolerance.
        let found = table.lookup(37.0).unwrap();
        assert_eq!(found.board_voltage_mv, 17_400);
    }

    #[test]
    fn far_past_the_edge_refuses_rather_than_extrapolating() {
        let table = two_row_table();
        // A cold start twenty degrees below anything learned. Extrapolating
        // here would walk the voltage out of the functional window, which is
        // the failure this table exists to prevent.
        assert!(table.lookup(20.0).is_none());
        assert!(table.lookup(100.0).is_none());
        assert!(Bzm2OperatingPointTable::default().lookup(50.0).is_none());
    }

    #[test]
    fn nearby_observations_merge_instead_of_multiplying_rows() {
        let mut table = Bzm2OperatingPointTable::default();
        for _ in 0..10 {
            table.observe(row(50.0, 17_400, 1_100.0));
            table.observe(row(51.0, 17_402, 1_100.0));
        }
        assert_eq!(table.rows().len(), 1, "{:?}", table.rows());
        assert!(table.rows()[0].observations >= 20);
    }

    #[test]
    fn a_row_converges_rather_than_chasing_the_latest_sample() {
        let mut table = Bzm2OperatingPointTable::default();
        table.observe(row(50.0, 17_400, 1_100.0));
        // One wild sample must not displace a learned row.
        table.observe(row(50.0, 18_400, 1_100.0));
        let voltage = table.rows()[0].board_voltage_mv;
        assert!(
            (17_400..17_700).contains(&voltage),
            "expected the row to move a little, not jump: {voltage}"
        );
    }

    #[test]
    fn interpolated_confidence_takes_the_weaker_row() {
        let mut table = Bzm2OperatingPointTable::default();
        table.observe(row(40.0, 17_400, 1_100.0));
        for _ in 0..5 {
            table.observe(row(80.0, 17_440, 1_060.0));
        }
        let found = table.lookup(60.0).unwrap();
        assert_eq!(
            found.observations, 1,
            "a point interpolated across a barely-observed row is barely observed"
        );
    }

    #[test]
    fn a_half_measured_span_reports_no_measurement() {
        let mut table = Bzm2OperatingPointTable::default();
        let mut cold = row(40.0, 17_400, 1_100.0);
        cold.measured_throughput_ths = Some(80.0);
        table.observe(cold);
        table.observe(row(80.0, 17_440, 1_060.0));

        let found = table.lookup(60.0).unwrap();
        assert!(
            found.measured_throughput_ths.is_none(),
            "one end's measurement must not be reported as holding across the span"
        );
    }

    #[test]
    fn an_absent_measurement_does_not_erase_a_stored_one() {
        let mut table = Bzm2OperatingPointTable::default();
        let mut measured = row(50.0, 17_400, 1_100.0);
        measured.measured_throughput_ths = Some(80.0);
        table.observe(measured);
        // A later observation with nothing measured: "not measured this time"
        // is not evidence against last time.
        table.observe(row(50.0, 17_400, 1_100.0));
        assert!(table.rows()[0].measured_throughput_ths.is_some());
    }

    #[test]
    fn the_table_stays_bounded_under_a_noisy_sensor() {
        let mut table = Bzm2OperatingPointTable::default();
        for step in 0..1_000 {
            table.observe(row(step as f32 * 0.5, 17_400, 1_100.0));
        }
        assert!(table.rows().len() <= MAX_ROWS, "{}", table.rows().len());
    }

    #[test]
    fn a_nonfinite_temperature_is_ignored_rather_than_stored() {
        let mut table = two_row_table();
        table.observe(row(f32::NAN, 17_400, 1_100.0));
        assert_eq!(table.rows().len(), 2);
        assert!(table.lookup(f32::NAN).is_none());
    }
}
