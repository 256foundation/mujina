//! The two-stage ladder between "stop dispatching" and "de-energise".
//!
//! A safety trip used to do exactly one thing: stop handing out work, and stop
//! watching. That leaves a board that is still energised, still drawing
//! leakage heat, with nothing left running to notice -- and on this platform it
//! is worse than that, because the MCU heartbeat keeps beating and actively
//! prevents the board's own shed from firing. The trip therefore removed the
//! last mechanism that would have made the part safe.
//!
//! The other extreme is worse in the other direction. De-energising on the
//! first trip means one dropped sensor read, one transient over-limit sample,
//! costs a cold start on a 3 kW machine -- and the cold start is itself a
//! thermal and electrical event, so a jumpy trip does not converge on safety.
//!
//! So the response is a ladder, and it escalates on evidence:
//!
//! 1. **Arm.** The first trip stops dispatch. Cheap, immediate, and it removes
//!    the heat source without removing the ability to watch what happens next.
//! 2. **Scram.** While armed, either of two things escalates:
//!    - the condition is *still* present after [`escalate_after`] -- an
//!      extended failure, which stopping dispatch did not fix; or
//!    - the condition cleared and came *back* -- a second failure, which says
//!      the machine is unstable rather than that one sample was bad.
//! 3. **Disarm.** If the condition clears and stays clear for
//!    [`disarm_after`], the rails stay up. Dispatch does not come back -- this
//!    process has already stopped its threads and cannot restart them -- but a
//!    board that is stopped, cooling, and reading within limits does not need
//!    to be de-energised to be safe, and de-energising it anyway would be
//!    acting on evidence we no longer have.
//!
//! The same shape already existed one layer down, in
//! [`ThermalInterlock`][crate::asic::bzm2::thread::ThermalInterlock], which has
//! carried an `escalate_after` and a `should_drop_rails()` since it was
//! written -- and which nothing in production ever called. This is that
//! decision, made where there is something able to act on it.
//!
//! **This type decides. It does not actuate.** Keeping the ladder pure is what
//! makes it testable at all: the actuation end of a scram is three I2C
//! transactions and a watch channel on a live 3 kW board, and none of that can
//! be exercised in a unit test.

use std::time::{Duration, Instant};

use super::abort::{AbortCondition, AbortSeverity};

/// How long an armed condition may persist before the rails come down.
///
/// One home for this number: it is the same question
/// [`ThermalInterlock`][crate::asic::bzm2::thread::ThermalInterlock] asks --
/// how long may we refuse and watch before refusing is not enough -- and two
/// copies of one policy diverge.
pub(super) use crate::asic::bzm2::thread::DEFAULT_THERMAL_ESCALATION as DEFAULT_SCRAM_ESCALATION;

/// How long the condition must stay clear before the ladder forgets it.
///
/// Five minutes, because the thing being distinguished is "one bad sample" from
/// "an unstable machine", and a fault that recurs inside five minutes is the
/// second of those. Shorter and a slow oscillation disarms between every peak,
/// which is precisely the case the second-failure rung exists to catch.
pub(super) const DEFAULT_SCRAM_DISARM: Duration = Duration::from_secs(300);

/// What the monitor should do about this poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ScramAction {
    /// Nothing changed and nothing is pending.
    Nothing,
    /// First failure. Stop dispatch and start the escalation clock.
    Arm { reason: String },
    /// Armed, and neither escalating nor disarming yet.
    Hold,
    /// De-energise the board.
    Scram { reason: String },
    /// The condition cleared and stayed clear. The rails stay up.
    Disarm { armed_for: Duration },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Clear,
    /// `cleared_at` is `Some` once the condition stopped being reported. The
    /// ladder stays armed through that window: a fault that comes back before
    /// the window closes is the second failure, not a fresh first one.
    Armed {
        since: Instant,
        cleared_at: Option<Instant>,
    },
    Scrammed,
}

/// Tracks a board's position on the ladder.
#[derive(Debug)]
pub(super) struct ScramLadder {
    stage: Stage,
    escalate_after: Duration,
    disarm_after: Duration,
}

impl ScramLadder {
    pub(super) fn new() -> Self {
        Self::with_timings(DEFAULT_SCRAM_ESCALATION, DEFAULT_SCRAM_DISARM)
    }

    pub(super) fn with_timings(escalate_after: Duration, disarm_after: Duration) -> Self {
        Self {
            stage: Stage::Clear,
            escalate_after,
            disarm_after,
        }
    }

    pub(super) fn is_armed(&self) -> bool {
        matches!(self.stage, Stage::Armed { .. })
    }

    /// Fold one poll into the ladder.
    ///
    /// `condition` is the trip reason this poll produced, or `None` if the
    /// board read within every armed limit. Time is passed in rather than read
    /// so the ladder can be tested at all -- an escalation that only happens
    /// after ninety wall-clock seconds is an escalation nobody tests.
    pub(super) fn observe(
        &mut self,
        now: Instant,
        condition: Option<&AbortCondition>,
    ) -> ScramAction {
        // SOME CONDITIONS HAVE NO ARM RUNG. Stopping dispatch is the cheap
        // reversible lever because for a thermal condition the load IS the
        // cause. For a voltage fault it is not: the part is reporting a rail
        // it cannot survive, and giving it less work changes nothing about
        // that. Arming would spend the escalation budget waiting for an
        // improvement that cannot arrive.
        if let Some(condition) = condition
            && condition.severity == AbortSeverity::Scram
            && !matches!(self.stage, Stage::Scrammed)
        {
            self.stage = Stage::Scrammed;
            return ScramAction::Scram {
                reason: format!(
                    "{} -- escalating immediately: stopping work cannot fix this class of fault.",
                    condition.reason
                ),
            };
        }
        let condition = condition.map(|c| c.reason.as_str());
        match (self.stage, condition) {
            (Stage::Scrammed, _) => ScramAction::Nothing,

            (Stage::Clear, None) => ScramAction::Nothing,

            (Stage::Clear, Some(reason)) => {
                self.stage = Stage::Armed {
                    since: now,
                    cleared_at: None,
                };
                ScramAction::Arm {
                    reason: reason.to_string(),
                }
            }

            // Still failing, unbroken. Escalate once it has gone on long
            // enough that stopping dispatch is demonstrably not the fix.
            (
                Stage::Armed {
                    since,
                    cleared_at: None,
                },
                Some(reason),
            ) => {
                let armed_for = now.saturating_duration_since(since);
                if armed_for >= self.escalate_after {
                    self.stage = Stage::Scrammed;
                    ScramAction::Scram {
                        reason: format!(
                            "{reason} -- still present {:.0}s after work was stopped, past the \
                             {:.0}s escalation budget. Stopping dispatch did not fix it, so the \
                             rail is the answer.",
                            armed_for.as_secs_f32(),
                            self.escalate_after.as_secs_f32(),
                        ),
                    }
                } else {
                    ScramAction::Hold
                }
            }

            // It cleared, and it is back. Second failure: escalate now rather
            // than waiting out a fresh clock, because the evidence has changed
            // from "one bad sample" to "this recurs".
            (
                Stage::Armed {
                    since,
                    cleared_at: Some(cleared_at),
                },
                Some(reason),
            ) => {
                self.stage = Stage::Scrammed;
                ScramAction::Scram {
                    reason: format!(
                        "{reason} -- this is the SECOND trip: the first was {:.0}s ago and it \
                         cleared {:.0}s ago. A fault that returns is not a transient.",
                        now.saturating_duration_since(since).as_secs_f32(),
                        now.saturating_duration_since(cleared_at).as_secs_f32(),
                    ),
                }
            }

            // First clean poll since arming: start the disarm window, but stay
            // armed. Clearing is not the same as being well.
            (
                Stage::Armed {
                    since,
                    cleared_at: None,
                },
                None,
            ) => {
                self.stage = Stage::Armed {
                    since,
                    cleared_at: Some(now),
                };
                ScramAction::Hold
            }

            (
                Stage::Armed {
                    since,
                    cleared_at: Some(cleared_at),
                },
                None,
            ) => {
                if now.saturating_duration_since(cleared_at) >= self.disarm_after {
                    let armed_for = now.saturating_duration_since(since);
                    self.stage = Stage::Clear;
                    ScramAction::Disarm { armed_for }
                } else {
                    ScramAction::Hold
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arm(reason: &str) -> AbortCondition {
        AbortCondition::arm(reason)
    }

    const ESCALATE: Duration = Duration::from_secs(90);
    const DISARM: Duration = Duration::from_secs(300);

    fn ladder() -> ScramLadder {
        ScramLadder::with_timings(ESCALATE, DISARM)
    }

    fn at(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    #[test]
    fn clean_polls_do_nothing() {
        let t0 = Instant::now();
        let mut l = ladder();
        assert_eq!(l.observe(t0, None), ScramAction::Nothing);
        assert_eq!(l.observe(at(t0, 600), None), ScramAction::Nothing);
        assert!(!l.is_armed());
    }

    /// THE REGRESSION THIS WHOLE FILE EXISTS FOR. Before the ladder, this
    /// first trip was the entire response: dispatch stopped, the monitor
    /// stopped, and the board stayed energised with the heartbeat still
    /// feeding it. A first trip must arm and MUST NOT de-energise.
    #[test]
    fn first_trip_arms_and_does_not_scram() {
        let t0 = Instant::now();
        let mut l = ladder();
        assert_eq!(
            l.observe(
                t0,
                Some(&arm("ASIC temperature 96.0C exceeded limit 95.0C"))
            ),
            ScramAction::Arm {
                reason: "ASIC temperature 96.0C exceeded limit 95.0C".into()
            }
        );
        assert!(l.is_armed());
    }

    /// A genuine transient: one bad poll, then clean for longer than the
    /// disarm window. The board is stopped and cooling; it does not need the
    /// rails pulled, and pulling them would be acting on evidence we no
    /// longer have.
    #[test]
    fn transient_hiccup_disarms_without_scram() {
        let t0 = Instant::now();
        let mut l = ladder();
        assert!(matches!(
            l.observe(t0, Some(&arm("hot"))),
            ScramAction::Arm { .. }
        ));
        assert_eq!(l.observe(at(t0, 5), None), ScramAction::Hold);
        assert_eq!(l.observe(at(t0, 200), None), ScramAction::Hold);
        assert_eq!(
            l.observe(at(t0, 5 + 300), None),
            ScramAction::Disarm {
                armed_for: Duration::from_secs(305)
            }
        );
        assert!(!l.is_armed());
    }

    /// The staged ladder's second rung: a hiccup arms, and a SECOND failure escalates.
    /// It must escalate on the second trip itself, not wait out a fresh
    /// escalation clock -- by then the evidence has already changed.
    #[test]
    fn second_trip_inside_the_disarm_window_scrams_immediately() {
        let t0 = Instant::now();
        let mut l = ladder();
        assert!(matches!(
            l.observe(t0, Some(&arm("hot"))),
            ScramAction::Arm { .. }
        ));
        assert_eq!(l.observe(at(t0, 10), None), ScramAction::Hold);
        let action = l.observe(at(t0, 20), Some(&arm("hot")));
        let ScramAction::Scram { reason } = action else {
            panic!("second trip must scram, got {action:?}");
        };
        assert!(reason.contains("SECOND trip"), "{reason}");
    }

    /// The staged ladder's first rung: an extended failure. Stopping dispatch did not
    /// fix it, which is the case where refusing work is provably not enough.
    #[test]
    fn unbroken_condition_scrams_at_the_escalation_budget() {
        let t0 = Instant::now();
        let mut l = ladder();
        assert!(matches!(
            l.observe(t0, Some(&arm("no reading for ASIC temperature"))),
            ScramAction::Arm { .. }
        ));
        assert_eq!(
            l.observe(at(t0, 89), Some(&arm("no reading for ASIC temperature"))),
            ScramAction::Hold
        );
        let action = l.observe(at(t0, 90), Some(&arm("no reading for ASIC temperature")));
        let ScramAction::Scram { reason } = action else {
            panic!("an extended failure must scram, got {action:?}");
        };
        assert!(reason.contains("still present"), "{reason}");
    }

    /// Two unrelated hiccups hours apart are two first failures, not a second
    /// one. Without the disarm transition the ladder would scram on the
    /// second of any two trips in the life of the process, however far apart.
    #[test]
    fn a_trip_after_a_full_disarm_is_a_first_trip_again() {
        let t0 = Instant::now();
        let mut l = ladder();
        assert!(matches!(
            l.observe(t0, Some(&arm("hot"))),
            ScramAction::Arm { .. }
        ));
        assert_eq!(l.observe(at(t0, 1), None), ScramAction::Hold);
        assert!(matches!(
            l.observe(at(t0, 400), None),
            ScramAction::Disarm { .. }
        ));
        assert_eq!(
            l.observe(at(t0, 4000), Some(&arm("hot"))),
            ScramAction::Arm {
                reason: "hot".into()
            }
        );
    }

    /// Once scrammed, the board is off. The ladder must not keep issuing
    /// actuation for a board that has already been de-energised.
    #[test]
    fn scram_is_terminal() {
        let t0 = Instant::now();
        let mut l = ladder();
        l.observe(t0, Some(&arm("hot")));
        assert!(matches!(
            l.observe(at(t0, 200), Some(&arm("hot"))),
            ScramAction::Scram { .. }
        ));
        assert_eq!(
            l.observe(at(t0, 201), Some(&arm("hot"))),
            ScramAction::Nothing
        );
        assert_eq!(l.observe(at(t0, 900), None), ScramAction::Nothing);
    }

    /// The staged ladder has a bypass: some faults have no arm rung, because
    /// stopping work is not related to their cause. A voltage fault must
    /// de-energise on its FIRST appearance, without spending the escalation
    /// budget waiting for an improvement that cannot arrive.
    #[test]
    fn a_scram_severity_condition_bypasses_the_arm_rung() {
        let t0 = Instant::now();
        let mut l = ladder();
        let fault = AbortCondition::scram("ASIC 3 reports a voltage shutdown");
        let action = l.observe(t0, Some(&fault));
        let ScramAction::Scram { reason } = action else {
            panic!("a scram-severity condition must not arm, got {action:?}");
        };
        assert!(reason.contains("voltage shutdown"), "{reason}");
        // And it is still terminal.
        assert_eq!(l.observe(at(t0, 1), Some(&fault)), ScramAction::Nothing);
    }
}
