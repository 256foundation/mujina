//! Chassis fan control, and the confirmation without which it is not control.
//!
//! # Commanding a fan here takes TWO writes, and neither one is sufficient
//!
//! A duty sets the level; a separate gate decides whether that level drives
//! anything. Measured on the RDS 2026-09-21: a duty of 20000 written with the
//! gate shut read back as 20000 and moved no air at all.
//!
//! # And the command cannot be verified by reading it back
//!
//! The gate is **write-only** — reading it returns an I/O error — so there is
//! no way to ask whether control is even active. The duty node echoes whatever
//! was written to it regardless. **The tacho is therefore not a nicer
//! confirmation, it is the only one**, which makes this the clearest case on
//! the machine for the rule that a command is not done until something which
//! is not the command says so.
//!
//! # Free-run is not maximum
//!
//! With the gate shut the fans free-run at about 5,430 rpm. Commanded to 100%
//! they reach about 6,800. So a machine left alone cools *less* well than one
//! commanded to full, which inverts the usual assumption about safe defaults:
//! doing nothing is not the safest thing this module can do.
//!
//! Measured points, all on the RDS: gate shut ≈ 5,430 rpm; 25% ≈ 1,050 rpm;
//! 100% ≈ 6,800 rpm.
//!
//! # The tacho latches when the gate is shut
//!
//! A fan commanded to 25% and then un-gated kept reporting 1,050 rpm and did
//! not move. So a reading taken without commanding first can be minutes stale.
//! Command, settle, then read — in that order, always.

use std::time::Duration;

use tokio::fs;

use super::platform::{Bzm2Platform, FanPaths};

/// How long a fan needs to reach a commanded speed.
///
/// Shorter than this and a slow spin-up is indistinguishable from a stalled
/// fan, which is the one mistake this module must not make: it would either
/// stop a healthy run or pass a dead fan.
pub const SETTLE: Duration = Duration::from_secs(15);

/// What one fan did when it was told to do something.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FanOutcome {
    pub index: usize,
    pub commanded_pct: u8,
    /// `None` when the tacho could not be read at all, which is UNMEASURED
    /// rather than stopped.
    pub measured_rpm: Option<u32>,
}

impl FanOutcome {
    /// Whether this fan confirmed it is turning at least `min_rpm`.
    ///
    /// An unreadable tacho is NOT a pass. A fan we cannot see is a fan we
    /// cannot vouch for, and the caller is usually about to make heat.
    pub fn confirmed(&self, min_rpm: u32) -> bool {
        matches!(self.measured_rpm, Some(rpm) if rpm >= min_rpm)
    }
}

/// The chassis fans of one machine.
pub struct Bzm2Fans {
    platform: Bzm2Platform,
}

impl Bzm2Fans {
    pub fn new(platform: Bzm2Platform) -> Self {
        Self { platform }
    }

    pub fn count(&self) -> usize {
        self.platform.fan_count
    }

    fn paths(&self, fan: usize) -> Option<FanPaths> {
        self.platform.fan_paths(fan)
    }

    /// Read one fan's speed, or `None` if the tacho will not answer.
    /// Read one fan's tachometer, in rpm.
    ///
    /// SAMPLED, NOT READ ONCE. This counter is documented misbehaving in our
    /// own watchdog: it refreshes about every 2 s against a 1 s tick and
    /// SHORT-COUNTS FOR 2-5 SECONDS AFTER EVERY FAN-ENABLE WRITE -- 35
    /// sub-floor readings across three runs, every one inside such a window
    /// and none anywhere else in 12,436 samples.
    ///
    /// Measured 2026-09-22: a single read taken 15 s after commanding all four
    /// fans to full returned UNREADABLE on all four, and the POST blocked a
    /// machine whose fans were at 6,750 rpm a second later. One failed read of
    /// this node is not evidence about a fan.
    ///
    /// So it is retried. `None` now means every attempt failed, which is a
    /// genuine inability to see the fan rather than a bad moment to have
    /// looked -- and that distinction is the whole reason the POST may refuse
    /// to energise on it.
    pub async fn read_rpm(&self, fan: usize) -> Option<u32> {
        const ATTEMPTS: usize = 5;
        const GAP: Duration = Duration::from_millis(600);

        let paths = self.paths(fan)?;
        // KEEP WHY EACH ATTEMPT FAILED. The first version discarded it, and
        // three hypotheses about this node were then tested and falsified on
        // the rig -- a timing quirk after an enable write, the gate write
        // itself, and padding bytes -- while the one fact that would have
        // settled it was being thrown away on every read. A shell `cat` of the
        // same path succeeds; this read did not, five times out of five. The
        // difference is in the reader, and the reader has to say what it saw.
        let mut last_failure = String::new();
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(GAP).await;
            }
            let raw = match fs::read_to_string(&paths.tacho).await {
                Ok(raw) => raw,
                Err(err) => {
                    last_failure = format!("read failed: {err} (kind {:?})", err.kind());
                    continue;
                }
            };
            // The shared parser: this node is NUL-terminated, and a local
            // `trim().parse()` is exactly the reader that could never see it.
            match super::telemetry::parse_sysfs_number::<u32>(&raw).ok_or(()) {
                Ok(counts) => {
                    if attempt > 0 {
                        tracing::debug!(fan, attempt, counts, "tacho answered on a retry");
                    }
                    return Some(counts * self.platform.fan_rpm_per_count);
                }
                Err(()) => {
                    last_failure = format!("unparseable; raw bytes {:?}", raw.as_bytes());
                }
            }
        }
        tracing::warn!(
            fan,
            attempts = ATTEMPTS,
            path = %paths.tacho.display(),
            last_failure = %last_failure,
            "tachometer did not answer on any attempt"
        );
        None
    }

    /// Command one fan, without waiting or confirming.
    ///
    /// Both writes, in this order: the level first, then the gate. A gate
    /// opened onto a stale duty would run the fan at whatever the last caller
    /// wanted for as long as it takes the next write to land.
    pub async fn command(&self, fan: usize, duty_pct: u8) -> anyhow::Result<()> {
        let paths = self
            .paths(fan)
            .ok_or_else(|| anyhow::anyhow!("fan {fan} is not on this platform"))?;
        let period: u64 = fs::read_to_string(&paths.period)
            .await?
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("fan {fan} period is not a number"))?;
        let duty = period.saturating_mul(u64::from(duty_pct.min(100))) / 100;
        fs::write(&paths.duty, duty.to_string()).await?;
        fs::write(&paths.gate, "1").await?;
        Ok(())
    }

    /// Command every fan, wait for them to settle, then report what each one
    /// actually did.
    ///
    /// Returns outcomes rather than a verdict: whether 2,000 rpm is acceptable
    /// depends on what the caller is about to do, and this module does not
    /// know that.
    /// Command ONE fan and report what its tachometer then said.
    ///
    /// Separate from [`Self::command_all_and_measure`] because they are
    /// different requests and conflating them was a real defect: the API's
    /// per-fan `SetFanTarget` called the all-fans version, so asking for one
    /// fan at 20 % quietly set all four to 20 % and then reported the one the
    /// caller had named. Nothing in the reply said the other three had moved.
    /// Bring the PWM channels into existence, and give them a period.
    ///
    /// TWO STEPS, AND NEITHER IS OPTIONAL. Measured on a freshly booted unit,
    /// 2026-09-22: all four chips report `npwm=1` and none are exported, so
    /// `pwmchip{n}/pwm0/` -- the path every other fan operation here assumes
    /// -- does not exist. And a channel that has just been exported reads
    /// `period=0`; a duty cycle may not exceed its period, so until a period
    /// is written no duty can be set and the fan cannot be commanded at all.
    ///
    /// Every run we had ever taken inherited both from the vendor stack, which
    /// is why neither was ever missing. The first boot without that stack has
    /// no fan control and nothing to write to.
    ///
    /// Idempotent by design: re-exporting an already-exported channel is
    /// refused by the kernel, and that refusal is SUCCESS here -- the channel
    /// exists, which is all this promises. An existing non-zero period is left
    /// alone rather than overwritten, because something already running may
    /// have chosen it.
    ///
    /// Never fails the caller. A chip that will not export is reported and
    /// then judged by the POST against what the platform variant declares,
    /// which is where "no fans" is decided to be a fault or the shape of the
    /// machine.
    pub async fn ensure_exported(&self) {
        for fan in 0..self.count() {
            let Some(paths) = self.paths(fan) else {
                continue;
            };
            if fs::metadata(&paths.duty).await.is_err() {
                let export = self
                    .platform
                    .fan_pwm_export_pattern
                    .replace("{}", &fan.to_string());
                // The channel index WITHIN the chip, not the fan index: each
                // chip here carries exactly one channel, so it is always 0.
                match fs::write(&export, b"0").await {
                    Ok(()) => tracing::info!(fan, path = %export, "exported PWM channel"),
                    Err(err) => {
                        tracing::warn!(
                            fan, path = %export, %err,
                            "could not export this PWM channel; it will read as an absent fan"
                        );
                        continue;
                    }
                }
            }
            match fs::read_to_string(&paths.period).await {
                Ok(text) if text.trim().parse::<u64>().unwrap_or(0) == 0 => {
                    let ns = self.platform.fan_pwm_period_ns;
                    match fs::write(&paths.period, ns.to_string()).await {
                        Ok(()) => tracing::info!(fan, period_ns = ns, "set PWM period"),
                        Err(err) => tracing::warn!(
                            fan, period_ns = ns, %err,
                            "could not set this PWM period; no duty can be written without one"
                        ),
                    }
                }
                Ok(_) => {}
                Err(err) => tracing::warn!(fan, %err, "could not read this PWM period"),
            }
            // ENABLE THE CHANNEL, AND ONLY AFTER THE PERIOD EXISTS. A period
            // of zero is not a configuration a channel can be enabled into,
            // so the order here is export, period, enable -- not the order
            // this was first written in.
            //
            // Exporting creates it and the period makes a
            // duty writable, but neither starts the fan: a channel with duty
            // at 100 % and enable at 0 turned at 15 % of full speed on this
            // chassis. Written unconditionally rather than only when it reads
            // 0, because writing 1 to an already-enabled channel is a no-op
            // and reading first would be a second way to get this wrong.
            let enable = self
                .platform
                .fan_pwm_enable_pattern
                .replace("{}", &fan.to_string());
            if let Err(err) = fs::write(&enable, b"1").await {
                tracing::warn!(
                    fan, path = %enable, %err,
                    "could not enable this PWM channel; its duty will be written into a \
                     channel that is not driving"
                );
            }
        }
    }

    /// Does this machine actually have the fans the platform claims?
    ///
    /// The descriptor is a statement about a chassis; the filesystem is the
    /// machine in front of us. They disagree on any host that is not an RDS,
    /// and "no fan nodes at all" is a different fact from "fans that will not
    /// turn" -- the first says this descriptor does not describe this machine,
    /// the second is a mechanical fault. Only the second may refuse a bring-up.
    pub async fn any_present(&self) -> bool {
        for fan in 0..self.count() {
            if let Some(paths) = self.paths(fan)
                && fs::metadata(&paths.duty).await.is_ok()
            {
                return true;
            }
        }
        false
    }

    pub async fn command_and_measure(
        &self,
        index: usize,
        duty_pct: u8,
    ) -> anyhow::Result<FanOutcome> {
        self.command(index, duty_pct).await?;
        tokio::time::sleep(SETTLE).await;
        Ok(FanOutcome {
            index,
            commanded_pct: duty_pct,
            measured_rpm: self.read_rpm(index).await,
        })
    }

    pub async fn command_all_and_measure(&self, duty_pct: u8) -> Vec<FanOutcome> {
        for fan in 0..self.count() {
            if let Err(err) = self.command(fan, duty_pct).await {
                tracing::warn!(fan, %err, "could not command this fan");
            }
        }
        tokio::time::sleep(SETTLE).await;
        let mut out = Vec::with_capacity(self.count());
        for index in 0..self.count() {
            out.push(FanOutcome {
                index,
                commanded_pct: duty_pct,
                measured_rpm: self.read_rpm(index).await,
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A NUMBER IS NOT A CONFIRMATION, and this is the case that shipped: a
    /// fan commanded to 80 % whose tacho says 0 was reported as success by the
    /// API, because the handler accepted any `Some(rpm)`. `confirmed` existed
    /// for exactly this and had no caller.
    #[test]
    fn a_stopped_fan_does_not_confirm_a_command() {
        let dead = FanOutcome {
            index: 0,
            commanded_pct: 80,
            measured_rpm: Some(0),
        };
        assert!(!dead.confirmed(300), "0 rpm cannot confirm 80% duty");

        let turning = FanOutcome {
            index: 0,
            commanded_pct: 80,
            measured_rpm: Some(4200),
        };
        assert!(turning.confirmed(300));
    }

    /// Unreadable is not a pass. A fan we cannot see is a fan we cannot vouch
    /// for, and the caller is usually about to make heat.
    #[test]
    fn an_unreadable_tacho_does_not_confirm() {
        let unmeasured = FanOutcome {
            index: 2,
            commanded_pct: 100,
            measured_rpm: None,
        };
        assert!(
            !unmeasured.confirmed(0),
            "even a zero floor cannot pass an unread tacho"
        );
    }

    /// A fan that is turning but below the floor is failing, not passing --
    /// the distinction the floor exists to draw.
    #[test]
    fn a_fan_below_the_floor_does_not_confirm() {
        let slow = FanOutcome {
            index: 1,
            commanded_pct: 60,
            measured_rpm: Some(120),
        };
        assert!(!slow.confirmed(300));
        assert!(
            slow.confirmed(100),
            "and it does confirm against a lower floor"
        );
    }
    use crate::board::bzm2::platform::RDS_2_0;

    #[test]
    fn every_fan_has_four_distinct_nodes() {
        // The gate and the duty are different files, and conflating them is
        // how a command becomes a no-op that reads back correctly.
        for fan in 0..RDS_2_0.fan_count {
            let p = RDS_2_0.fan_paths(fan).expect("fan is on this platform");
            let all = [&p.duty, &p.period, &p.gate, &p.tacho];
            for (i, a) in all.iter().enumerate() {
                for b in all.iter().skip(i + 1) {
                    assert_ne!(a, b, "fan {fan} reuses a node for two purposes");
                }
            }
            assert!(p.duty.to_string_lossy().contains(&fan.to_string()));
            assert!(p.tacho.to_string_lossy().contains(&fan.to_string()));
        }
        assert!(RDS_2_0.fan_paths(RDS_2_0.fan_count).is_none());
    }

    #[test]
    fn an_unreadable_tacho_is_never_a_pass() {
        // UNMEASURED is not confirmed. A fan we cannot see is one we cannot
        // vouch for, and the caller is usually about to make heat.
        let unseen = FanOutcome {
            index: 0,
            commanded_pct: 100,
            measured_rpm: None,
        };
        assert!(!unseen.confirmed(1));
        assert!(!unseen.confirmed(0), "even a floor of zero must not pass");
    }

    #[test]
    fn the_measured_operating_points_bracket_the_floor_we_use() {
        // Guards the numbers this module's callers reason with. Measured on
        // the RDS: gate shut about 5,430 rpm, 25% about 1,050, 100% about
        // 6,800. A floor of 4,000 therefore passes a commanded-full fan and
        // fails one stuck at a quarter -- which is the discrimination wanted.
        const FLOOR: u32 = 4_000;
        assert!(FLOOR < 6_800, "a healthy commanded fan must pass");
        assert!(FLOOR > 1_050, "a fan stuck at 25% must fail");
        // And the tacho conversion those numbers came through.
        assert_eq!(181 * RDS_2_0.fan_rpm_per_count, 5_430);
    }
}

/// What the fan loop is trying to hold, and the bounds it may not leave.
#[derive(Debug, Clone, Copy)]
pub struct ThermalFanConfig {
    /// Die temperature the loop aims to hold.
    pub target_c: f32,
    /// Below `target_c - band_c` the loop may ease off; above `target_c` it
    /// pushes. The band exists so the fans are not hunting either side of a
    /// single number.
    pub band_c: f32,
    /// Never command below this.
    ///
    /// NOT ZERO, and not a comfort setting. With the gate shut these fans
    /// free-run at about 5,430 rpm while 25% duty gives about 1,050 — so a low
    /// commanded duty is *worse* than no control at all, and a floor is what
    /// stops the loop being actively harmful at idle.
    pub min_duty_pct: u8,
    pub max_duty_pct: u8,
}

impl Default for ThermalFanConfig {
    fn default() -> Self {
        Self {
            // Conservative against the 115 °C the on-die trip is armed at, and
            // against the 59-80 °C the vendor stack runs these dies at under
            // load. Sits below both with room to respond.
            target_c: 75.0,
            band_c: 8.0,
            min_duty_pct: 40,
            max_duty_pct: 100,
        }
    }
}

/// Choose a fan duty from the hottest die we can see.
///
/// PURE, so the policy can be argued with in tests rather than on a 3 kW
/// machine. Proportional with clamps, deliberately NOT a PID: an integral term
/// needs the thermal time constants of this chassis and we have not measured
/// them, and an integrator tuned by guesswork on a machine with a ten-minute
/// thermal mass is how you build an oscillator. We now log per-ASIC die
/// temperature at 1 Hz, so those constants are measurable and this can become
/// a PID once they are.
///
/// **`None` means full.** Not the last value, not the floor — full. The fans
/// free-run slower than they run commanded, so "we cannot see the dies" must
/// not resolve to "leave things as they are".
pub fn duty_for(hottest_c: Option<f32>, cfg: &ThermalFanConfig) -> u8 {
    let Some(hot) = hottest_c else {
        return cfg.max_duty_pct;
    };
    if !hot.is_finite() {
        return cfg.max_duty_pct;
    }
    let span = cfg.band_c.max(0.1);
    // 0.0 at the bottom of the band, 1.0 at the target and beyond.
    let demand = ((hot - (cfg.target_c - span)) / span).clamp(0.0, 1.0);
    let lo = f32::from(cfg.min_duty_pct);
    let hi = f32::from(cfg.max_duty_pct.max(cfg.min_duty_pct));
    (lo + demand * (hi - lo)).round().clamp(0.0, 100.0) as u8
}

#[cfg(test)]
mod thermal_tests {
    use super::*;

    #[test]
    fn unknown_temperature_means_full_not_last_and_not_floor() {
        // THE SAFETY PROPERTY. These fans free-run slower than they run
        // commanded, so "we cannot see the dies" must never resolve to
        // "leave things alone" or to the floor.
        let cfg = ThermalFanConfig::default();
        assert_eq!(duty_for(None, &cfg), cfg.max_duty_pct);
        assert_eq!(duty_for(Some(f32::NAN), &cfg), cfg.max_duty_pct);
        assert_eq!(duty_for(Some(f32::INFINITY), &cfg), cfg.max_duty_pct);
    }

    #[test]
    fn it_never_commands_below_the_floor_however_cold() {
        // A low duty is worse than no control: 25% gives about 1,050 rpm
        // against roughly 5,430 free-running.
        let cfg = ThermalFanConfig::default();
        for c in [-40.0, 0.0, 20.0, 50.0] {
            assert_eq!(duty_for(Some(c), &cfg), cfg.min_duty_pct, "at {c} C");
        }
    }

    #[test]
    fn it_reaches_full_at_and_above_target() {
        let cfg = ThermalFanConfig::default();
        assert_eq!(duty_for(Some(cfg.target_c), &cfg), cfg.max_duty_pct);
        assert_eq!(duty_for(Some(cfg.target_c + 30.0), &cfg), cfg.max_duty_pct);
        // And well before the on-die trip at 115 C, which must never be the
        // thing that saves us.
        assert_eq!(duty_for(Some(110.0), &cfg), cfg.max_duty_pct);
    }

    #[test]
    fn it_rises_monotonically_across_the_band() {
        // No dips: a hotter die must never ask for less air than a cooler one.
        let cfg = ThermalFanConfig::default();
        let mut last = 0u8;
        let mut c = cfg.target_c - cfg.band_c - 5.0;
        while c <= cfg.target_c + 5.0 {
            let d = duty_for(Some(c), &cfg);
            assert!(d >= last, "duty fell from {last} to {d} at {c} C");
            last = d;
            c += 0.5;
        }
        assert_eq!(last, cfg.max_duty_pct);
    }
}
