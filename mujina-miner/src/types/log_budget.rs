//! A budget for log records that repeat at wire speed.
//!
//! The failure this exists to prevent, measured rather than imagined: on
//! hardware the BZM2 DTS/VS plausibility gate emitted one WARN
//! per rejected frame. With the Gen2 decode mirrored, *every* frame was
//! rejected, so 118,673 identical warnings -- 237,346 of the log's 237,392
//! lines, about 38 MB -- were written to the control board's flash inside a
//! thirty-second window. The HTTP API on that unit answered 0 of 12 probes,
//! and the case was recorded as a telemetry failure, measured on hardware.
//!
//! The decode is fixed, which removes that particular flood. This type exists
//! because the flood was never really about the decode: any chain that
//! desyncs reproduces it exactly, and a diagnostic that disables the daemon it
//! is diagnosing is worse than no diagnostic.
//!
//! **Nothing is discarded.** Every occurrence is counted, and the count is the
//! single home for "how often did this happen" -- the log is a *view* of that
//! counter, never a second copy of the fact. What the budget bounds is how
//! many bytes that view costs: the first few occurrences are emitted in full,
//! so the shape of the problem is visible, and everything after is folded into
//! a periodic summary that carries the exact number suppressed and the span it
//! covers. A reader can always recover the true rate; they simply do not pay
//! one write per event to do it.

use std::time::{Duration, Instant};

/// What the caller should do with one occurrence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogVerdict {
    /// Emit this occurrence in full.
    Emit,
    /// Counted, nothing to say. The caller must not log.
    Count,
    /// Emit one summary standing in for the folded occurrences.
    Summarise {
        /// Occurrences folded into this summary, including the one that
        /// triggered it. Never zero.
        folded: u64,
        /// Wall-clock span those occurrences arrived over.
        span: Duration,
        /// Occurrences since this budget was created, all time.
        total: u64,
    },
}

/// Bounds how often a repeating log record is written, without losing the count.
#[derive(Debug)]
pub struct LogBudget {
    total: u64,
    folded: u64,
    verbatim_remaining: u32,
    window_start: Option<Instant>,
    interval: Duration,
}

impl LogBudget {
    /// `verbatim` occurrences are emitted in full before folding begins;
    /// thereafter a summary is emitted at most once per `interval`.
    ///
    /// `verbatim` should be small but not one: a single example shows that
    /// something happened, while a handful shows whether it is one device or
    /// the whole chain -- which is the difference between a broken part and a
    /// broken decode, and is exactly the distinction that mattered in our captures.
    pub fn new(verbatim: u32, interval: Duration) -> Self {
        Self {
            total: 0,
            folded: 0,
            verbatim_remaining: verbatim,
            window_start: None,
            interval,
        }
    }

    /// Occurrences since creation, whether or not they were ever logged.
    ///
    /// This is the number to assert on, export, or compare between boards.
    pub fn total(&self) -> u64 {
        self.total
    }

    /// Record one occurrence and decide what the caller may print.
    pub fn admit(&mut self, now: Instant) -> LogVerdict {
        self.total = self.total.saturating_add(1);

        if self.verbatim_remaining > 0 {
            self.verbatim_remaining -= 1;
            // The window opens when folding starts, not at construction: a
            // budget created long before the first occurrence would otherwise
            // report its first summary as covering that idle time.
            self.window_start = Some(now);
            return LogVerdict::Emit;
        }

        self.folded = self.folded.saturating_add(1);
        let started = *self.window_start.get_or_insert(now);
        let span = now.saturating_duration_since(started);
        if span < self.interval {
            return LogVerdict::Count;
        }

        let folded = self.folded;
        self.folded = 0;
        self.window_start = Some(now);
        LogVerdict::Summarise {
            folded,
            span,
            total: self.total,
        }
    }

    /// Report any occurrences folded since the last summary, if there are any.
    ///
    /// Without this the final partial window is never accounted for in the log
    /// stream -- `total()` still has it, but a reader working from the records
    /// alone would undercount, and the end of a run is exactly when a flood
    /// matters most. Call it when the stream it guards is shut down.
    pub fn drain(&mut self, now: Instant) -> Option<LogVerdict> {
        if self.folded == 0 {
            return None;
        }
        let folded = self.folded;
        self.folded = 0;
        let started = self.window_start.unwrap_or(now);
        self.window_start = Some(now);
        Some(LogVerdict::Summarise {
            folded,
            span: now.saturating_duration_since(started),
            total: self.total,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flood_that_starved_the_api_costs_a_bounded_number_of_records() {
        // The real shape, replayed: 118,673 rejected frames arriving over the
        // thirty-second handover window, measured on hardware.
        const OCCURRENCES: u64 = 118_673;
        const WINDOW: Duration = Duration::from_secs(30);

        let mut budget = LogBudget::new(5, Duration::from_secs(1));
        let start = Instant::now();
        let step = WINDOW / OCCURRENCES as u32;

        let mut emitted = 0u64;
        let mut summarised = 0u64;
        let mut accounted = 0u64;

        for i in 0..OCCURRENCES {
            let now = start + step * i as u32;
            match budget.admit(now) {
                LogVerdict::Emit => {
                    emitted += 1;
                    accounted += 1;
                }
                LogVerdict::Count => {}
                LogVerdict::Summarise { folded, total, .. } => {
                    summarised += 1;
                    accounted += folded;
                    // The summary always reports the true running total, so a
                    // reader never has to add up the records to learn the rate.
                    assert_eq!(total, i + 1);
                }
            }
        }

        // The tail: whatever was folded after the last summary must still be
        // accounted for, or a reader working from records alone undercounts.
        if let Some(LogVerdict::Summarise { folded, .. }) = budget.drain(start + WINDOW) {
            summarised += 1;
            accounted += folded;
        }

        let records = emitted + summarised;

        // The behaviour under test. Unbudgeted this was 118,673 records and
        // about 38 MB; the whole point is that it is now bounded by elapsed
        // time rather than by event count.
        assert!(
            records <= 40,
            "{records} records for {OCCURRENCES} occurrences is not bounded"
        );
        assert_eq!(emitted, 5, "the first few must still be shown in full");

        // Nothing is lost: the counter is exact, and every occurrence is
        // either shown or folded into a summary that names it.
        assert_eq!(budget.total(), OCCURRENCES);
        assert_eq!(
            accounted, OCCURRENCES,
            "occurrences were neither emitted nor folded into a summary"
        );

        // And the reduction is the thing that buys the API its I/O back.
        assert!(
            OCCURRENCES / records >= 1000,
            "expected a thousandfold reduction, got {}x",
            OCCURRENCES / records
        );
    }

    #[test]
    fn a_rare_event_is_never_folded() {
        // A budget must not turn an occasional, genuinely interesting warning
        // into a summary nobody reads. Below the verbatim allowance every
        // occurrence is emitted, however long the gaps.
        let mut budget = LogBudget::new(5, Duration::from_secs(1));
        let start = Instant::now();
        for i in 0..5 {
            let now = start + Duration::from_secs(3600) * i;
            assert_eq!(budget.admit(now), LogVerdict::Emit);
        }
        assert_eq!(budget.total(), 5);
    }

    #[test]
    fn the_first_summary_covers_only_the_folding_period() {
        // Regression: if the window opened at construction rather than at the
        // first occurrence, a budget built at start-up would report its first
        // summary as spanning the whole uptime, overstating the quiet period
        // and understating the rate.
        let mut budget = LogBudget::new(1, Duration::from_secs(1));
        let start = Instant::now();
        let first = start + Duration::from_secs(600);
        assert_eq!(budget.admit(first), LogVerdict::Emit);

        assert_eq!(
            budget.admit(first + Duration::from_millis(100)),
            LogVerdict::Count
        );
        match budget.admit(first + Duration::from_millis(1100)) {
            LogVerdict::Summarise {
                folded,
                span,
                total,
            } => {
                assert_eq!(folded, 2);
                assert_eq!(total, 3);
                assert!(
                    span < Duration::from_secs(2),
                    "span {span:?} leaked the idle period before the first event"
                );
            }
            other => panic!("expected a summary, got {other:?}"),
        }
    }
}
