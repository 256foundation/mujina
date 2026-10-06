use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use crate::types::LogBudget;

/// Log budgets for the two DTS/VS diagnostics that sit on the per-frame path.
///
/// Both of these warn about a *frame*, so both scale with wire traffic rather
/// than with how much is wrong. Measured on hardware: the
/// plausibility warning alone produced 118,673 records and about 38 MB inside a
/// thirty-second window, and the unit's API answered 0 of 12 probes. The
/// out-of-chain warning has the same shape and was one addressing bug away from
/// the same flood on two boards of three.
///
/// The counts are kept in full; only the number of records is bounded. See
/// [`LogBudget`].
#[derive(Debug)]
pub struct DtsVsDiagnostics {
    /// Frames asserting a fault at a temperature no die can be at.
    pub implausible_fault: LogBudget,
    /// Frames from an address the configured chain does not contain.
    pub out_of_chain: LogBudget,
    /// Every telemetry frame, at trace level.
    pub telemetry_frame: LogBudget,
    /// Fault assertions being held for corroboration.
    pub corroboration_hold: LogBudget,
}

impl Default for DtsVsDiagnostics {
    fn default() -> Self {
        // Five in full, then at most one summary a second. Five is enough to
        // tell one bad device from a whole bad chain, which is the distinction
        // that would have identified the mirrored decode immediately.
        Self {
            implausible_fault: LogBudget::new(5, Duration::from_secs(1)),
            out_of_chain: LogBudget::new(5, Duration::from_secs(1)),
            telemetry_frame: LogBudget::new(5, Duration::from_secs(1)),
            corroboration_hold: LogBudget::new(5, Duration::from_secs(1)),
        }
    }
}

/// Requires a device's fault to repeat before it stops a thread.
///
/// **Consecutive frames from the same device, not sightings in a window.** The
/// first version counted every frame a device sent -- healthy ones included --
/// inside a 250 ms window, which was wrong both ways at once, measured on
/// on hardware:
///
/// - **It believed one bad frame.** Two healthy frames from "asic 7" and one
///   misparsed frame whose voltage bytes tripped the midpoint check, inside
///   250 ms, counted as three sightings. Thread 1 stopped on a fault its wire
///   recording shows exactly once.
/// - **It could not believe a real one.** A full chain at the configured sensor
///   gap sends each device's frame about every 151 ms (246,523 frames in 373 s
///   across 100 ASICs). Three frames span ~300 ms, so a device asserting a real
///   fault on every frame reset the window before it reached three, and the
///   stream-driven fault stop could never fire in steady state.
///
/// Counting a device's own frames in a row is independent of the sensor rate:
/// a real fault asserts on every frame and is believed after
/// [`FAULT_CORROBORATION_COUNT`] of them, whatever the gap; a misparse has to
/// land on the same device that many times with no healthy frame from it in
/// between.
#[derive(Debug, Default)]
pub struct FaultCorroborator {
    /// Per device: how many of its frames in a row have asserted a fault.
    run: HashMap<u8, u8>,
}

impl FaultCorroborator {
    /// Forget every pending run. After a break in the stream, a frame from
    /// before it and one after it are not consecutive frames of anything; a
    /// real fault asserts on every frame and builds its run again at once.
    pub fn clear(&mut self) {
        self.run.clear();
    }

    /// Fold in one frame from `asic`, and whether it asserted a fault. True
    /// once that device's last [`FAULT_CORROBORATION_COUNT`] frames all did.
    pub fn observe(&mut self, asic: u8, asserted: bool) -> bool {
        if !asserted {
            self.run.remove(&asic);
            return false;
        }
        let n = self.run.entry(asic).or_insert(0);
        *n = n.saturating_add(1);
        if *n >= FAULT_CORROBORATION_COUNT {
            self.run.remove(&asic);
            return true;
        }
        false
    }
}

/// How many of one device's frames in a row must assert a fault before it is
/// believed. At the ~151 ms per-device frame period measured on a full chain,
/// three is about half a second for a genuine fault.
pub(super) const FAULT_CORROBORATION_COUNT: u8 = 3;

/// Why the interlock refused to hand out work.
#[derive(Debug, Clone, PartialEq)]
pub enum ThermalRefusal {
    /// No die temperature has ever been observed on this chain.
    NoReading,
    /// The last reading is older than the interlock will trust.
    Stale { age_s: f32 },
    /// The last reading is at or above the ceiling.
    OverCeiling { temperature_c: f32, ceiling_c: f32 },
    /// Some addressed devices have no fresh reading: unknown is hot.
    Unmeasured {
        fresh: usize,
        expected: usize,
        first_missing: u8,
    },
}

impl std::fmt::Display for ThermalRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoReading => write!(f, "no die temperature has been observed"),
            Self::Stale { age_s } => write!(f, "last die temperature is {age_s:.0}s old"),
            Self::OverCeiling {
                temperature_c,
                ceiling_c,
            } => {
                write!(
                    f,
                    "die temperature {temperature_c:.1}C is at or above the {ceiling_c:.1}C ceiling"
                )
            }
            Self::Unmeasured {
                fresh,
                expected,
                first_missing,
            } => write!(
                f,
                "only {fresh} of {expected} devices have a fresh die reading \
                 (first unmeasured: ASIC {first_missing})"
            ),
        }
    }
}

/// How long the interlock may refuse work before the rail comes down.
///
/// Long enough that a brief sensor gap does not cost a run, short enough that a
/// genuinely blind stack is not left powered. Escalation is the last resort, so
/// it should be rare and it should not be silent.
pub const DEFAULT_THERMAL_ESCALATION: Duration = Duration::from_secs(90);

/// Refuses to dispatch work the chain cannot be protected while doing.
///
/// This is an interlock, not a controller. It does not try to hold a
/// temperature; it decides whether handing out more work is defensible at all,
/// and it is deliberately separate from the fan loop so that a broken or
/// absent controller cannot also disable the thing that would have caught it.
///
/// **Unknown reads as hot.** No reading and a stale reading both refuse, for
/// the same reason: the failure we have already measured on this hardware is
/// telemetry that parses into nonsense, and "no number" must never be quieter
/// than "a bad number". A miner that keeps hashing because its thermometer
/// stopped answering is the exact failure this exists to prevent.
///
/// Refusing is visible, recoverable and costs throughput. Not refusing costs
/// silicon.
#[derive(Debug)]
pub struct ThermalInterlock {
    ceiling_c: f32,
    max_age: Duration,
    /// The latest reading from EACH device, not the latest reading.
    ///
    /// A single scalar here meant the ceiling was evaluated against whichever
    /// ASIC happened to speak last. A hundred devices stream at ~19,500
    /// frames/s, so that value turned over thousands of times a second and
    /// `check()` saw an essentially random 1-in-100 die — while the part that
    /// needed stopping could be twenty degrees hotter and invisible.
    ///
    /// Worse in combination with the escalation clock: `observe` cleared the
    /// refusal run whenever ITS die was below the ceiling, and with a hundred
    /// dies a cool one arrives within microseconds. One cool device
    /// continuously erased the clock a hot device had started, so the
    /// escalation stage could essentially never fire on a chain where anything
    /// was cool.
    ///
    /// Bounded by the device count of one chain, and each entry is replaced
    /// rather than appended.
    per_device: BTreeMap<u8, (Instant, f32)>,
    refusals: u64,
    /// When the current unbroken run of refusals began.
    refusing_since: Option<Instant>,
    /// How long refusal may persist before the rail should come down.
    escalate_after: Duration,
    /// The devices this interlock guards. Every one must have a fresh
    /// reading before work may go out; empty means "whatever reports".
    ///
    /// Without it one reading was enough: measured on hardware, dispatch happened a
    /// second after attach on the single frame the protection check had read,
    /// with 99 devices unmeasured, a minute before the monitor saw live rows.
    /// Unknown is hot per device, not per chain.
    expected: std::collections::BTreeSet<u8>,
}

impl ThermalInterlock {
    pub fn new(ceiling_c: f32, max_age: Duration) -> Self {
        Self::with_escalation(ceiling_c, max_age, DEFAULT_THERMAL_ESCALATION)
    }

    pub fn with_escalation(ceiling_c: f32, max_age: Duration, escalate_after: Duration) -> Self {
        Self {
            ceiling_c,
            max_age,
            per_device: BTreeMap::new(),
            refusals: 0,
            refusing_since: None,
            escalate_after,
            expected: std::collections::BTreeSet::new(),
        }
    }

    /// Require a fresh reading from each of these devices before work.
    pub fn expecting(mut self, devices: &[u8]) -> Self {
        self.expected = devices.iter().copied().collect();
        self
    }

    /// Record one device's die temperature. Only called with readings that
    /// have already passed the frame's own validity checks.
    ///
    /// Keyed by device, so a hot part cannot be displaced by a cool neighbour
    /// arriving a microsecond later.
    pub fn observe(&mut self, asic: u8, temperature_c: f32) {
        self.per_device
            .insert(asic, (Instant::now(), temperature_c));
        // THE CLOCK CLEARS ON THE HOTTEST, NOT ON THIS ONE. Clearing whenever
        // the device just seen was cool is what made the escalation stage
        // unreachable: any cool die erased the run a hot die had started.
        if let Some((_, hottest)) = self.hottest_fresh()
            && hottest < self.ceiling_c
        {
            self.clear_refusal_run();
        }
    }

    /// The hottest reading still inside the staleness window, with its device.
    ///
    /// HOTTEST, never mean and never latest: cooling is decided by the worst
    /// part, and an average of a hundred dies hides exactly the one that needs
    /// the work to stop. Stale entries are excluded rather than deleted, so a
    /// device that goes quiet stops counting without being forgotten.
    fn hottest_fresh(&self) -> Option<(u8, f32)> {
        let now = Instant::now();
        self.per_device
            .iter()
            .filter(|(_, (at, _))| now.saturating_duration_since(*at) <= self.max_age)
            .map(|(asic, (_, t))| (*asic, *t))
            .max_by(|a, b| a.1.total_cmp(&b.1))
    }

    /// May work be dispatched right now?
    pub fn check(&self) -> Result<(), ThermalRefusal> {
        if self.per_device.is_empty() {
            return Err(ThermalRefusal::NoReading);
        }
        let now = Instant::now();
        let Some((asic, hottest)) = self.hottest_fresh() else {
            // Everything we have is older than the window. Report the age of
            // the FRESHEST one, because that is how long we have been blind.
            let youngest = self
                .per_device
                .values()
                .map(|(at, _)| now.saturating_duration_since(*at))
                .min()
                .unwrap_or(self.max_age);
            return Err(ThermalRefusal::Stale {
                age_s: youngest.as_secs_f32(),
            });
        };
        let _ = asic;
        if !self.expected.is_empty() {
            let fresh = |id: &u8| {
                self.per_device
                    .get(id)
                    .is_some_and(|(at, _)| now.saturating_duration_since(*at) <= self.max_age)
            };
            if let Some(&first_missing) = self.expected.iter().find(|id| !fresh(id)) {
                return Err(ThermalRefusal::Unmeasured {
                    fresh: self.expected.iter().filter(|id| fresh(id)).count(),
                    expected: self.expected.len(),
                    first_missing,
                });
            }
        }
        if hottest >= self.ceiling_c {
            return Err(ThermalRefusal::OverCeiling {
                temperature_c: hottest,
                ceiling_c: self.ceiling_c,
            });
        }
        Ok(())
    }

    /// How many of the guarded devices have a fresh reading, of how many,
    /// and the hottest fresh one: what a passing `check()` was based on,
    /// said out loud so a run's evidence can show it (the ladder's P4).
    pub fn fresh_count(&self) -> (usize, usize, Option<f32>) {
        let now = Instant::now();
        let fresh = |id: &u8| {
            self.per_device
                .get(id)
                .is_some_and(|(at, _)| now.saturating_duration_since(*at) <= self.max_age)
        };
        let expected = if self.expected.is_empty() {
            self.per_device.len()
        } else {
            self.expected.len()
        };
        let counted = if self.expected.is_empty() {
            self.per_device.keys().filter(|id| fresh(id)).count()
        } else {
            self.expected.iter().filter(|id| fresh(id)).count()
        };
        (counted, expected, self.hottest_fresh().map(|(_, t)| t))
    }

    pub fn record_refusal(&mut self) {
        self.refusals = self.refusals.saturating_add(1);
        self.refusing_since.get_or_insert_with(Instant::now);
    }

    /// Should the core rail be brought down?
    ///
    /// **Refusing work is not the same as making the part safe**, and on some
    /// carriers it is barely related. A powered stack draws leakage heat with no
    /// jobs in it at all, so withholding work bounds how much heat is *added*
    /// while doing nothing about the heat already being produced.
    ///
    /// Whether that gap matters depends on the board, and the difference is
    /// large:
    ///
    /// - The RDS arms per-ASIC trip codes in hardware during bring-up, so
    ///   refusing dispatch is a layer above a hardware backstop that will act
    ///   whatever software does.
    /// - A carrier whose over-temperature output reaches only an indicator, and
    ///   not the rail enable, has **no backstop at all**. Software is the entire
    ///   protection, and the only lever that removes heat is the rail.
    ///
    /// So the interlock escalates. Refusal comes first, because it is cheap and
    /// reversible. If the condition persists, the rail is the answer.
    ///
    /// The trigger is deliberately **loss of visibility**, not over-temperature.
    /// A reading above the ceiling and no reading at all are the same signal
    /// here: in both cases we cannot show the part is safe, and a stale sensor
    /// on a hot stack looks exactly like a cool one. Shutting down only on a hot
    /// reading trusts a channel that has already stopped reporting.
    pub fn should_drop_rails(&self) -> bool {
        match self.refusing_since {
            Some(since) => since.elapsed() >= self.escalate_after,
            None => false,
        }
    }

    /// A good reading clears the escalation clock; refusals start it again.
    pub fn clear_refusal_run(&mut self) {
        self.refusing_since = None;
    }

    pub fn refusals(&self) -> u64 {
        self.refusals
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A HUNDRED DEVICES, ONE CEILING, AND THE CEILING MUST SEE THE WORST.
    ///
    /// The interlock held a single scalar, overwritten by whichever ASIC spoke
    /// last. On a real chain that turned over ~19,500 times a second, so the
    /// 85 C dispatch ceiling was evaluated against a random 1-in-100 die while
    /// the part that needed stopping could be twenty degrees hotter.
    #[test]
    fn one_hot_die_is_not_hidden_by_ninety_nine_cool_ones() {
        let mut lock = ThermalInterlock::new(85.0, Duration::from_secs(30));
        for asic in 0..99u8 {
            lock.observe(asic, 60.0);
        }
        lock.observe(99, 96.0);
        // ...and then every cool device reports again, which is what the wire
        // actually does. The hot one must still decide.
        for asic in 0..99u8 {
            lock.observe(asic, 60.0);
        }
        let err = lock.check().expect_err("96 C on any die must refuse work");
        assert!(
            matches!(err, ThermalRefusal::OverCeiling { temperature_c, .. } if temperature_c > 95.0),
            "got {err:?}"
        );
    }

    /// THE ESCALATION CLOCK MUST NOT BE ERASED BY A COOL NEIGHBOUR.
    ///
    /// `observe` cleared the refusal run whenever ITS die was below the
    /// ceiling. With a hundred dies streaming, a cool one arrives within
    /// microseconds of a hot one, so the clock the hot die started was wiped
    /// continuously and the escalation stage could never be reached on a chain
    /// where anything was cool.
    #[test]
    fn a_cool_device_does_not_clear_a_hot_devices_escalation_clock() {
        let mut lock = ThermalInterlock::with_escalation(
            85.0,
            Duration::from_secs(30),
            Duration::from_millis(60),
        );
        lock.observe(7, 96.0);
        assert!(lock.check().is_err());
        lock.record_refusal();
        // The rest of the chain keeps reporting, cool, exactly as it does live.
        for _ in 0..50 {
            for asic in 0..20u8 {
                if asic != 7 {
                    lock.observe(asic, 55.0);
                }
            }
        }
        assert!(
            lock.check().is_err(),
            "the hot die is still hot; a cool neighbour is not evidence about it"
        );
        std::thread::sleep(Duration::from_millis(80));
        assert!(
            lock.should_drop_rails(),
            "the escalation clock must survive cool neighbours, or it can never fire"
        );
    }

    /// And a chain that genuinely cools DOES clear it — the escalation is a
    /// last resort, not a latch.
    #[test]
    fn a_chain_that_cools_clears_the_escalation_clock() {
        let mut lock = ThermalInterlock::with_escalation(
            85.0,
            Duration::from_secs(30),
            Duration::from_millis(60),
        );
        lock.observe(7, 96.0);
        lock.record_refusal();
        lock.observe(7, 60.0);
        assert!(lock.check().is_ok());
        std::thread::sleep(Duration::from_millis(80));
        assert!(
            !lock.should_drop_rails(),
            "everything is cool; nothing to escalate"
        );
    }

    /// Stale readings from every device is blindness, and blindness refuses.
    /// The reported age is the FRESHEST one — that is how long we have been
    /// blind, not how long the oldest device has been quiet.
    #[test]
    fn every_device_going_quiet_is_blindness_not_safety() {
        let mut lock = ThermalInterlock::new(85.0, Duration::from_millis(40));
        lock.observe(0, 60.0);
        lock.observe(1, 61.0);
        std::thread::sleep(Duration::from_millis(60));
        let err = lock.check().expect_err("all readings stale must refuse");
        assert!(matches!(err, ThermalRefusal::Stale { .. }), "got {err:?}");
    }

    /// One device going quiet must not blind the chain: the rest are still
    /// answering and the hottest of THEM decides.
    #[test]
    fn one_quiet_device_does_not_blind_the_others() {
        let mut lock = ThermalInterlock::new(85.0, Duration::from_millis(60));
        lock.observe(3, 96.0);
        std::thread::sleep(Duration::from_millis(80));
        lock.observe(4, 60.0);
        assert!(
            lock.check().is_ok(),
            "asic 3's reading has aged out; asic 4 is fresh and cool"
        );
    }

    #[test]
    fn interlock_refuses_before_any_reading() {
        // The startup case, and the one that matters most: a chain that has
        // never reported a temperature has not been shown to be safe, and
        // "not yet measured" must not be quieter than "measured and hot".
        let lock = ThermalInterlock::new(85.0, Duration::from_secs(30));
        assert_eq!(lock.check(), Err(ThermalRefusal::NoReading));
    }

    #[test]
    fn interlock_allows_a_fresh_cool_reading() {
        let mut lock = ThermalInterlock::new(85.0, Duration::from_secs(30));
        lock.observe(0, 62.0);
        assert_eq!(lock.check(), Ok(()));
    }

    #[test]
    fn interlock_refuses_at_and_above_the_ceiling() {
        let mut lock = ThermalInterlock::new(85.0, Duration::from_secs(30));
        lock.observe(0, 84.9);
        assert_eq!(lock.check(), Ok(()), "just below the ceiling still runs");
        lock.observe(0, 85.0);
        assert!(
            matches!(lock.check(), Err(ThermalRefusal::OverCeiling { .. })),
            "the ceiling is inclusive: at the limit is not under it"
        );
        lock.observe(0, 120.0);
        assert!(matches!(
            lock.check(),
            Err(ThermalRefusal::OverCeiling { .. })
        ));
    }

    #[test]
    fn interlock_refuses_a_stale_reading() {
        // A sensor that stops answering must stop the miner. The failure this
        // guards against is a reading that was true once and is quoted
        // forever, which looks identical to a healthy chain.
        let mut lock = ThermalInterlock::new(85.0, Duration::from_millis(50));
        lock.observe(0, 40.0);
        assert_eq!(lock.check(), Ok(()));
        std::thread::sleep(Duration::from_millis(80));
        assert!(
            matches!(lock.check(), Err(ThermalRefusal::Stale { .. })),
            "an old reading is not a reading"
        );
    }

    #[test]
    fn interlock_recovers_when_a_fresh_reading_arrives() {
        // Refusal is a state of the world, not a latch. A chain that cools
        // down may be given work again; requiring a restart would push
        // operators toward disabling the interlock.
        let mut lock = ThermalInterlock::new(85.0, Duration::from_secs(30));
        lock.observe(0, 90.0);
        assert!(lock.check().is_err());
        lock.observe(0, 70.0);
        assert_eq!(lock.check(), Ok(()));
    }

    #[test]
    fn the_interlock_says_how_many_devices_it_read_fresh() {
        let mut lock = ThermalInterlock::new(85.0, Duration::from_millis(60)).expecting(&[0, 1, 2]);
        assert_eq!(lock.fresh_count(), (0, 3, None));
        lock.observe(0, 50.0);
        lock.observe(2, 61.5);
        assert_eq!(lock.fresh_count(), (2, 3, Some(61.5)));
        lock.observe(1, 40.0);
        assert!(lock.check().is_ok());
        assert_eq!(lock.fresh_count(), (3, 3, Some(61.5)));
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(lock.fresh_count().0, 0, "stale readings are not fresh");
    }

    /// EVERY ADDRESSED DEVICE MUST BE MEASURED BEFORE WORK GOES OUT.
    ///
    /// One fresh reading used to be enough: measured on hardware, dispatch happened on the
    /// single frame its protection check had read, with 99 devices unknown.
    #[test]
    fn the_interlock_refuses_until_every_addressed_device_is_measured() {
        let mut lock = ThermalInterlock::new(85.0, Duration::from_millis(60)).expecting(&[0, 1, 2]);
        lock.observe(1, 50.0);
        match lock.check() {
            Err(ThermalRefusal::Unmeasured {
                fresh: 1,
                expected: 3,
                first_missing: 0,
            }) => {}
            other => panic!("one of three measured must refuse, naming the gap: {other:?}"),
        }
        lock.observe(0, 50.0);
        lock.observe(2, 50.0);
        assert!(lock.check().is_ok(), "all three measured and cool");

        // Device 2 goes quiet; the others keep reporting.
        std::thread::sleep(Duration::from_millis(40));
        lock.observe(0, 50.0);
        lock.observe(1, 50.0);
        std::thread::sleep(Duration::from_millis(30));
        assert!(
            matches!(
                lock.check(),
                Err(ThermalRefusal::Unmeasured {
                    first_missing: 2,
                    ..
                })
            ),
            "a device gone stale is unmeasured again"
        );
    }

    /// Refusing work is not the same as making the part safe. A stack that has
    /// stopped reporting temperature is not cooler for having no jobs in it, so
    /// refusal escalates to the rail rather than continuing indefinitely.
    #[test]
    fn interlock_escalates_to_the_rail_when_refusal_persists() {
        let mut lock = ThermalInterlock::with_escalation(
            85.0,
            Duration::from_secs(30),
            Duration::from_millis(40),
        );
        assert!(!lock.should_drop_rails(), "nothing has been refused yet");

        lock.record_refusal();
        assert!(
            !lock.should_drop_rails(),
            "one refusal must not drop a rail; brief sensor gaps are normal"
        );

        std::thread::sleep(Duration::from_millis(60));
        assert!(
            lock.should_drop_rails(),
            "refusal persisted past the escalation window and the rail stayed up"
        );
    }

    /// A good reading clears the clock. Otherwise a single early gap would arm
    /// escalation forever and the rail would come down during healthy running.
    #[test]
    fn a_healthy_reading_clears_the_escalation_clock() {
        let mut lock = ThermalInterlock::with_escalation(
            85.0,
            Duration::from_secs(30),
            Duration::from_millis(40),
        );
        lock.record_refusal();
        lock.observe(0, 60.0);
        std::thread::sleep(Duration::from_millis(60));
        assert!(
            !lock.should_drop_rails(),
            "a healthy reading did not clear the escalation clock"
        );
    }

    /// The trigger is loss of visibility, not heat. A reading above the ceiling
    /// does not clear the clock -- only a reading that shows the part is safe
    /// does, and "hot" and "unknown" must escalate alike.
    #[test]
    fn an_over_ceiling_reading_does_not_count_as_visibility() {
        let mut lock = ThermalInterlock::with_escalation(
            85.0,
            Duration::from_secs(30),
            Duration::from_millis(40),
        );
        lock.record_refusal();
        lock.observe(0, 97.0);
        std::thread::sleep(Duration::from_millis(60));
        assert!(
            lock.should_drop_rails(),
            "a reading above the ceiling was treated as if it made the part safe"
        );
    }
}
