use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::task::JoinHandle;

use anyhow::Result as AnyhowResult;
use async_trait::async_trait;
use tokio::sync::{mpsc, watch};

use super::{BackplaneConnector, BoardInfo, VirtualBoardDescriptor};
use crate::{
    api_client::types::{BoardTelemetry, ThreadTelemetry},
    asic::{
        bzm2::{Bzm2Thread, Bzm2ThreadConfig, Bzm2ThreadHandle},
        hash_thread::{
            HashTask, HashThread, HashThreadCapabilities, HashThreadEvent, HashThreadStatus,
        },
    },
    tracing::prelude::*,
    transport::SerialControl,
};
use calibration::Bzm2BusLayout;

use telemetry::{merge_power_readings, merge_temperature_readings};

mod abort;
pub mod board_heartbeat;
pub mod board_mcu;
pub mod board_power;
mod bringup;
mod calibration;
mod config;
pub mod fans;
mod monitor;
pub mod platform;
mod post;
mod scram;
pub mod stored_calibration;
mod telemetry;
#[cfg(all(test, unix))]
mod test_support;

pub use config::Bzm2RuntimeConfig;
use telemetry::{publish_thread_status, publish_thread_telemetry};

// Register this board type with the inventory system
inventory::submit! {
    VirtualBoardDescriptor {
        device_type: "bzm2",
        name: "BZM2",
        create_fn: || Box::pin(create_bzm2_board()),
    }
}

/// Errors raised by BZM2 board bring-up and hardware control.
#[derive(Debug)]
pub enum BoardError {
    /// Board initialization failed (bring-up, serial open, calibration).
    InitializationFailed(String),
    /// Serial or file I/O failure while talking to the board.
    Communication(std::io::Error),
    /// A hardware control operation (rails, reset, clocks) failed.
    HardwareControl(String),
}

impl std::fmt::Display for BoardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BoardError::InitializationFailed(msg) => {
                write!(f, "board initialization failed: {msg}")
            }
            BoardError::Communication(err) => write!(f, "board communication error: {err}"),
            BoardError::HardwareControl(msg) => write!(f, "hardware control error: {msg}"),
        }
    }
}

impl std::error::Error for BoardError {}

impl From<std::io::Error> for BoardError {
    fn from(err: std::io::Error) -> Self {
        BoardError::Communication(err)
    }
}

pub struct Bzm2Board {
    config: Bzm2RuntimeConfig,
    bringup_applied: bool,
    shutdown_handles: Vec<Bzm2ThreadHandle>,
    serial_controls: Vec<SerialControl>,
    bus_layouts: Arc<Mutex<Vec<Bzm2BusLayout>>>,
    telemetry_tx: watch::Sender<BoardTelemetry>,
    monitor_shutdown: Option<watch::Sender<bool>>,
    monitor_task: Option<JoinHandle<()>>,
    heartbeat_shutdown: Option<watch::Sender<bool>>,
    heartbeat_tasks: Vec<JoinHandle<()>>,
    fan_shutdown: Option<watch::Sender<bool>>,
    fan_task: Option<JoinHandle<()>>,
}

impl Bzm2Board {
    pub fn new(config: Bzm2RuntimeConfig, telemetry_tx: watch::Sender<BoardTelemetry>) -> Self {
        Self {
            config,
            bringup_applied: false,
            shutdown_handles: Vec::new(),
            serial_controls: Vec::new(),
            bus_layouts: Arc::new(Mutex::new(Vec::new())),
            telemetry_tx,
            monitor_shutdown: None,
            monitor_task: None,
            heartbeat_shutdown: None,
            heartbeat_tasks: Vec::new(),
            fan_shutdown: None,
            fan_task: None,
        }
    }

    /// Run the thermal fan loop for as long as this board is up.
    ///
    /// Reads the hottest die we can currently see, asks the policy for a duty,
    /// commands it, and confirms by tacho. The policy is pure and tested
    /// separately; this is only the part that has to touch hardware.
    ///
    /// ON SHUTDOWN IT COMMANDS FULL RATHER THAN STOPPING. These fans free-run
    /// slower than they run commanded, so simply ceasing to command them makes
    /// the machine cool WORSE at exactly the moment nobody is watching it.
    fn spawn_thermal_fan_control(&mut self) {
        let telemetry_rx = self.telemetry_tx.subscribe();
        let (tx, mut rx) = watch::channel(false);
        let cfg = fans::ThermalFanConfig::default();
        // THE SAME AGE LIMIT THE ABORT PATH USES, derived the same way, so the
        // two readers of one fact cannot come to different conclusions about
        // whether a die reading is still a measurement. This loop ticks every
        // 10 s; the floor keeps it sane if the telemetry poll is ever set slow.
        let die_max_age = std::cmp::max(
            self.config.telemetry.poll_interval.saturating_mul(3),
            Duration::from_secs(15),
        );
        info!(
            die_max_age_s = die_max_age.as_secs(),
            target_c = cfg.target_c,
            min_duty_pct = cfg.min_duty_pct,
            "Thermal fan control starting: unknown die temperature commands FULL, \
             and the floor is not zero because these fans free-run faster than a low duty"
        );
        self.fan_task = Some(tokio::spawn(async move {
            let fans = fans::Bzm2Fans::new(platform::DEFAULT);
            // Slower than the thermal mass it is steering. A chassis this size
            // does not change temperature in seconds, and commanding faster
            // than it responds is how a loop starts chasing its own wake.
            let mut ticker = tokio::time::interval(Duration::from_secs(10));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut last_duty: Option<u8> = None;
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        // ONE HOME FOR "WHAT COUNTS AS A USABLE DIE READING".
                        //
                        // This used to fold the max over EVERY temperature row
                        // and accept any age, which was wrong twice over. It
                        // included board and regulator sensors, so a warm rail
                        // could stand in for a die; and board state never
                        // prunes, so once any row existed the reading could
                        // never go absent -- which made the `None => FULL`
                        // branch below unreachable and the documented "unknown
                        // means full air" rule undeliverable. A die that
                        // stopped reporting two minutes ago governed the fans
                        // at whatever duty its last frame implied.
                        //
                        // The abort path already decides this question, and two
                        // consumers of one fact diverge. So it is asked once,
                        // here, through the same function.
                        let hottest = {
                            let state = telemetry_rx.borrow();
                            abort::hottest_die_at(
                                &state.temperatures,
                                std::time::Instant::now(),
                                die_max_age,
                            )
                            .map(|(_, c)| c)
                        };
                        let duty = fans::duty_for(hottest, &cfg);
                        if last_duty != Some(duty) {
                            match hottest {
                                Some(c) => info!(hottest_c = c, duty_pct = duty,
                                    "Thermal fan control adjusting"),
                                None => warn!(duty_pct = duty,
                                    "No die temperature visible; commanding FULL"),
                            }
                        }
                        for fan in 0..fans.count() {
                            if let Err(err) = fans.command(fan, duty).await {
                                warn!(fan, %err, "could not command this fan");
                            }
                        }
                        // Confirm one fan per tick rather than all four: the
                        // tacho is the only witness there is, and checking
                        // every fan every tick would cost more than it buys on
                        // a loop this slow. Over four ticks all four are seen.
                        let watch_fan = (ticker.period().as_secs() as usize)
                            .wrapping_add(last_duty.unwrap_or(0) as usize)
                            % fans.count().max(1);
                        if let Some(rpm) = fans.read_rpm(watch_fan).await
                            && duty > 0 && rpm == 0
                        {
                            error!(fan = watch_fan, duty_pct = duty,
                                "FAN COMMANDED AND NOT TURNING");
                        }
                        last_duty = Some(duty);
                    }
                    _ = rx.changed() => {
                        if *rx.borrow() {
                            // Full, not off. See the note on this function.
                            for fan in 0..fans.count() {
                                let _ = fans.command(fan, cfg.max_duty_pct).await;
                            }
                            info!("Thermal fan control stopping; fans commanded FULL");
                            return;
                        }
                    }
                }
            }
        }));
        self.fan_shutdown = Some(tx);
    }

    /// Start feeding each driven board's MCU, if the operator asked for it.
    ///
    /// One task per board, on its own adapter, so a board whose bus is
    /// wedged cannot stop the others being fed. Only boards whose chain we
    /// are actually driving are beaten: the MCU answers with its rails
    /// down, so beating an idle board would arm a countdown against a board
    /// nobody is using and buy nothing.
    fn spawn_heartbeats(&mut self) {
        if !self.config.heartbeat.enabled {
            return;
        }
        let platform = platform::DEFAULT;
        let interval = self.config.heartbeat.interval;
        let (tx, rx) = watch::channel(false);

        for serial_path in &self.config.serial_paths {
            let Some(board_index) =
                platform.board_index_for_chain(std::path::Path::new(serial_path))
            else {
                warn!(
                    serial_path,
                    platform = platform.name,
                    "No board index for this chain, so its MCU will not be fed. If this chain is \
                     real, the platform descriptor is wrong and the board will shed."
                );
                continue;
            };
            let Some(bus_path) = platform.i2c_bus_path(board_index) else {
                continue;
            };

            let bus = match crate::hw_trait::i2c::linux::LinuxI2c::open(&bus_path) {
                Ok(bus) => bus,
                Err(err) => {
                    // Refusing to start is not an option here: the board is
                    // already powered by whoever brought it up, and failing to
                    // beat it simply means it sheds. Say so loudly instead.
                    error!(
                        board_index,
                        bus = %bus_path.display(),
                        error = %err,
                        "Cannot open this board's MCU bus, so it will NOT be fed and will shed"
                    );
                    continue;
                }
            };

            let mut rx = rx.clone();
            let telemetry_tx = self.telemetry_tx.clone();
            // Announce the intent, not the outcome: nothing is armed until a
            // beat has actually landed, announced below.
            info!(
                board_index,
                bus = %bus_path.display(),
                interval_ms = interval.as_millis(),
                "Starting to feed this board's MCU. Nothing is armed until a beat lands."
            );
            self.heartbeat_tasks.push(tokio::spawn(async move {
                let mut heartbeat = board_heartbeat::Bzm2BoardHeartbeat::new(bus);
                let mut ticker = tokio::time::interval(interval);
                // Skip, never burst: a stalled task must not try to "catch up"
                // by sending several values at once. The MCU only cares that
                // the newest value is new, so a burst buys nothing and a burst
                // after a stall is exactly when the bus is least free.
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                let mut consecutive_failures = 0u32;
                // A beat returning Ok means the I2C layer took it, not that the
                // MCU was satisfied, so the rail is read periodically as an
                // independent witness -- often enough to catch a shed within a
                // few seconds, rarely enough that it does not itself become the
                // bus contention it is watching for.
                const VERIFY_EVERY_BEATS: u64 = 5;
                let mut announced = false;
                loop {
                    tokio::select! {
                        _ = ticker.tick() => {
                            match heartbeat.beat().await {
                                Ok(_) => {
                                    consecutive_failures = 0;
                                    if !announced {
                                        announced = true;
                                        info!(
                                            board_index,
                                            "First heartbeat landed. This board's shed timer is \
                                             now ARMED: it goes dark shortly after we stop \
                                             beating, which is what we want if we stop \
                                             unexpectedly."
                                        );
                                    }
                                    if heartbeat.beats() % VERIFY_EVERY_BEATS == 0 {
                                        // Publish the board's own thermals on
                                        // the same beat that reads the rail:
                                        // this MCU has one owner and this is
                                        // it. Only what was actually read is
                                        // published: a channel that did not
                                        // answer leaves its row absent rather
                                        // than writing a zero, because the
                                        // board-temperature limit reads these
                                        // rows and a fabricated cool value is
                                        // the failure this layer exists to
                                        // prevent.
                                        let (inlet, outlet) = heartbeat.read_thermals().await;
                                        if inlet.is_some() || outlet.is_some() {
                                            let now = std::time::Instant::now();
                                            let rows: Vec<_> = [
                                                (format!("board{board_index}-inlet"), inlet),
                                                (format!("board{board_index}-outlet"), outlet),
                                            ]
                                            .into_iter()
                                            .filter_map(|(name, c)| {
                                                c.map(|c| crate::api_client::types::TemperatureSensor {
                                                    name,
                                                    temperature: Some(crate::types::Temperature::from_celsius(c)),
                                                    observed_at: Some(now),
                                                })
                                            })
                                            .collect();
                                            telemetry_tx.send_modify(|state| {
                                                merge_temperature_readings(
                                                    &mut state.temperatures,
                                                    &rows,
                                                );
                                            });
                                        }
                                        match heartbeat.verify_still_energised().await {
                                            Some(false) => error!(
                                                board_index,
                                                beats = heartbeat.beats(),
                                                "BOARD DE-ENERGISED WHILE WE ARE BEATING IT. \
                                                 Every write was accepted, so the beat is being \
                                                 sent and is not being honoured. Treat this \
                                                 board as unprotected."
                                            ),
                                            None => warn!(
                                                board_index,
                                                "Cannot read this board's rail, so whether the \
                                                 heartbeat is working is UNMEASURED -- not \
                                                 confirmed."
                                            ),
                                            Some(true) => {}
                                        }
                                    }
                                }
                                Err(err) => {
                                    consecutive_failures += 1;
                                    // Loud from the first one: there is no
                                    // recovery to wait for, every missed beat
                                    // spends the shed budget, and an operator
                                    // who sees this has a bounded time to act.
                                    error!(
                                        board_index,
                                        consecutive_failures,
                                        error = %err,
                                        "Heartbeat NOT delivered; this board sheds if this continues"
                                    );
                                }
                            }
                        }
                        _ = rx.changed() => {
                            if *rx.borrow() {
                                info!(
                                    board_index,
                                    beats = heartbeat.beats(),
                                    "Stopped feeding this board's MCU"
                                );
                                return;
                            }
                        }
                    }
                }
            }));
        }
        self.heartbeat_shutdown = Some(tx);
    }
}

impl Bzm2Board {
    fn board_info(&self) -> BoardInfo {
        BoardInfo {
            model: "BZM2".into(),
            firmware_version: None,
            serial_number: Some(self.config.device_id()),
        }
    }

    /// Power-on self test: compare this machine against what its variant says
    /// it should be, and let the DECLARATION decide whether to start.
    ///
    /// This used to decide its own policy: absent fan nodes warned, present
    /// ones that would not turn refused. That heuristic was invented here, and
    /// it could not express the case that matters — an immersion chassis has
    /// no fans *by design*, and from a probe that is identical to an
    /// air-cooled machine with four dead ones. The difference is not
    /// observable; it is declared. See `board/bzm2/post.rs`.
    ///
    /// No retry. A fan that did not turn when told to is a mechanical or
    /// control fault, and a retry converts a hard fault into an intermittent
    /// one and loses the evidence.
    async fn power_on_self_test(&self) -> AnyhowResult<()> {
        let def =
            match post::PlatformDef::parse(include_str!("../../../../platforms/rds-dvt2.json")) {
                Ok(def) => def,
                Err(err) => {
                    // The definition is compiled in, so this is a build-time
                    // mistake reaching runtime. Refuse: a POST that cannot read
                    // its own contract has no verdict to give, and proceeding
                    // would be proceeding unjudged.
                    return Err(anyhow::anyhow!(
                        "platform definition did not parse, so this machine cannot be judged \
                     against anything: {err}"
                    ));
                }
            };
        let variant = std::env::var("MUJINA_BZM2_VARIANT")
            .unwrap_or_else(|_| def.default_variant().to_string());

        let fans = fans::Bzm2Fans::new(platform::DEFAULT);
        // EXPORT BEFORE PROBING. Without this the fan paths do not exist on a
        // freshly booted unit, every fan reads absent, and the POST blocks a
        // healthy machine for a reason that is ours rather than the rig's.
        fans.ensure_exported().await;
        let mut observed = Vec::new();

        // Fans, commanded to FULL and confirmed by TACHO -- the only witness
        // available, because the gate node is write-only and the duty node
        // echoes writes whether or not they drive anything.
        if fans.count() > 0 && fans.any_present().await {
            info!(
                fans = fans.count(),
                expect_rpm = platform::DEFAULT.fan_rpm_at_full / 2,
                settle_s = fans::SETTLE.as_secs(),
                %variant,
                "POST: commanding every fan to FULL and reading the tacho back"
            );
            // COMMANDED FULL MEANS CONFIRMED NEAR FULL.
            //
            // The floor used here was DEFAULT_MIN_FAN_RPM, 300 -- a runtime
            // stall threshold, and the wrong question for a preflight that
            // commands 100 %. Measured 2026-09-22: a fan whose PWM channel was
            // never enabled sat at 1,020 rpm under a 100 % duty and PASSED,
            // because 1,020 clears 300 comfortably. It was delivering 15 % of
            // what it had been asked for.
            //
            // Half of the measured full speed: generous enough for fan-to-fan
            // spread and a warm chassis, tight enough that a channel which is
            // not actually driving cannot pass.
            let expect_rpm = platform::DEFAULT.fan_rpm_at_full / 2;
            for outcome in fans.command_all_and_measure(100).await {
                let (state, detail) = match outcome.measured_rpm {
                    Some(rpm) if outcome.confirmed(expect_rpm) => {
                        (post::State::Present, format!("{rpm} rpm"))
                    }
                    Some(rpm) => (
                        post::State::Failed,
                        format!(
                            "{rpm} rpm at 100% duty, under the {expect_rpm} rpm this chassis \
                             should reach; the channel may not be driving"
                        ),
                    ),
                    None => (post::State::Failed, "tacho UNREADABLE".to_string()),
                };
                observed.push(post::Observation {
                    kind: "fan".into(),
                    index: outcome.index,
                    parent_index: None,
                    state,
                    detail,
                });
            }
        } else {
            // Nothing answered. Whether that is a fault or the shape of the
            // machine is the declaration's call, not ours.
            for index in 0..fans.count() {
                observed.push(post::Observation {
                    kind: "fan".into(),
                    index,
                    parent_index: None,
                    state: post::State::Absent,
                    detail: "no fan node on this host".into(),
                });
            }
        }

        info!(
            class = %def.class,
            %variant,
            variant_title = def.variant_title(&variant).unwrap_or("UNKNOWN"),
            platform = %def.title,
            "POST: judging this machine against its declared fitment"
        );
        let findings = def.evaluate(&variant, &observed)?;
        for f in &findings {
            let line = format!(
                "POST {}: {} {} is {} — {} ({})",
                f.verdict.label(),
                f.kind,
                f.index,
                f.state.label(),
                f.detail,
                f.why
            );
            match f.verdict {
                post::Verdict::Block => error!(variant = %variant, "{line}"),
                post::Verdict::Warn => warn!(variant = %variant, "{line}"),
                post::Verdict::Note => info!(variant = %variant, "{line}"),
            }
        }
        match post::worst(&findings) {
            Some(post::Verdict::Block) => Err(anyhow::anyhow!(
                "POST BLOCKED the start: {} finding(s) against variant {variant} of {}, \
                 {} of them blocking. Change the variant if this machine is a different \
                 configuration; do not retry.",
                findings.len(),
                def.class,
                findings
                    .iter()
                    .filter(|f| f.verdict == post::Verdict::Block)
                    .count(),
            )),
            other => {
                info!(
                    variant = %variant,
                    class = %def.class,
                    findings = findings.len(),
                    worst = other.map(|v| v.label()).unwrap_or("clean"),
                    "POST passed"
                );
                Ok(())
            }
        }
    }

    /// Empty each board's MCU fault queue and record what was in it.
    ///
    /// NEVER FAILS THE BRING-UP. A fault the board queued before we arrived is
    /// history, not a verdict on this run: the machine may have been power
    /// cycled, reseated, or simply left with a stale entry. Refusing to start
    /// on it would make a board unusable until someone cleared a queue by hand.
    /// It is reported loudly and in full, and the POST is what decides whether
    /// to start.
    ///
    /// Popping is destructive -- each entry exists nowhere else once read --
    /// which is why `drain_faults` hands back what it took alongside why it
    /// stopped, and why everything it took is logged even when it stopped for
    /// a bad reason.
    async fn drain_board_faults_before_bringup(&mut self) {
        // Bounded against an MCU queueing faults faster than we drain. Hitting
        // it is itself a finding and is reported as one.
        const LIMIT: usize = 32;

        let platform = platform::DEFAULT;
        for board_index in self.driven_board_indices() {
            let Some(bus_path) = platform.i2c_bus_path(board_index) else {
                continue;
            };
            let bus = match crate::hw_trait::i2c::linux::LinuxI2c::open(&bus_path) {
                Ok(bus) => bus,
                Err(err) => {
                    warn!(
                        board_index, bus = %bus_path.display(), %err,
                        "Cannot open this board's MCU to read its fault history or identity; \
                         starting without knowing what it had queued or which board it is"
                    );
                    continue;
                }
            };
            let mut mcu = board_mcu::Bzm2BoardMcu::new(bus);

            // IDENTITY IN THE SAME PASS. One bus open, two facts, and this is
            // the only window in which the MCU has no other owner -- the
            // heartbeat takes it later in this function. Reading the serial
            // here is what lets a stored calibration be checked against the
            // board actually in the slot, rather than against a slot number.
            match mcu.read_identity().await {
                Ok(presence) => {
                    let serial = presence.present().and_then(|id| id.serial);
                    match &serial {
                        Some(sn) => info!(board_index, serial = %sn, "board identified"),
                        None => warn!(
                            board_index,
                            "MCU answered but its serial field is blank, which is what an \
                             unprogrammed board looks like; calibration cannot be bound to \
                             this board's identity"
                        ),
                    }
                }
                Err(err) => {
                    warn!(
                        board_index, %err,
                        "Could not read this board's identity; a stored calibration cannot \
                         be confirmed to belong to it"
                    );
                }
            }

            let drain = mcu.drain_faults(LIMIT).await;
            for fault in &drain.faults {
                warn!(
                    board_index,
                    ?fault,
                    "MCU had a fault queued BEFORE this bring-up: it predates anything we \
                     did and is history, not a verdict on this run"
                );
            }
            match &drain.stopped {
                board_mcu::DrainStop::Empty => info!(
                    board_index,
                    drained = drain.faults.len(),
                    "MCU fault queue read and empty"
                ),
                board_mcu::DrainStop::Absent => info!(
                    board_index,
                    drained = drain.faults.len(),
                    "MCU reports no board in this slot"
                ),
                board_mcu::DrainStop::Limit => error!(
                    board_index,
                    drained = drain.faults.len(),
                    limit = LIMIT,
                    "MCU fault queue did not empty within the limit -- entries REMAIN, and \
                     the queue is producing faster than a bounded drain can read it"
                ),
                board_mcu::DrainStop::Failed(err) => error!(
                    board_index,
                    drained = drain.faults.len(),
                    %err,
                    "MCU fault queue read FAILED part-way; the entries above were taken and \
                     exist nowhere else, and the rest are unread"
                ),
            }
        }
    }

    async fn shutdown(&mut self) -> AnyhowResult<()> {
        // Stop beating first. The MCU's shed is a backstop for an unexpected
        // stop; a deliberate one takes the rails down itself below, and
        // leaving beats going while that happens would have two mechanisms
        // acting on the same board.
        if let Some(tx) = self.fan_shutdown.take() {
            let _ = tx.send(true);
        }
        if let Some(handle) = self.fan_task.take() {
            let _ = handle.await;
        }
        if let Some(tx) = self.heartbeat_shutdown.take() {
            let _ = tx.send(true);
        }
        for handle in self.heartbeat_tasks.drain(..) {
            let _ = handle.await;
        }
        if let Some(tx) = self.monitor_shutdown.take() {
            let _ = tx.send(true);
        }
        if let Some(handle) = self.monitor_task.take() {
            let _ = handle.await;
        }
        // A deliberate shutdown, so a refused stop is still worth saying: it
        // means a thread was never asked, and the rails are about to come down
        // underneath it.
        for handle in &self.shutdown_handles {
            // Only a live-but-not-listening thread is worth a warning here.
            // A deliberate shutdown reaches threads that have already stopped
            // as a matter of course.
            if handle.shutdown().still_running() {
                warn!(
                    "a hash thread is alive and would not accept its stop; shutting down around it"
                );
            }
        }
        self.shutdown_handles.clear();
        self.serial_controls.clear();
        self.telemetry_tx.send_modify(|state| {
            for thread in &mut state.threads {
                thread.is_active = false;
                thread.hashrate = 0;
            }
        });
        self.apply_shutdown_sequence().await?;
        Ok(())
    }

    async fn create_hash_threads(&mut self) -> AnyhowResult<Vec<Box<dyn HashThread>>> {
        let mut threads: Vec<Box<dyn HashThread>> = Vec::new();
        let mut thread_states = Vec::new();
        // JUDGE THE MACHINE AGAINST ITS DECLARATION BEFORE MAKING HEAT.
        //
        // Runs before the rails, because a fan that will not turn is a reason
        // not to energise at all -- and because this is the one moment the
        // check is free: nothing is hot yet, so a refusal costs a cold start
        // rather than a run.
        //
        // Commanded and CONFIRMED BY TACHO, not by the write returning Ok.
        // The gate node is write-only and the duty node echoes whatever was
        // written whether or not it drives anything, so the tachometer is the
        // only witness available.
        // NEVER WALK AWAY FROM AN ENERGISED BOARD.
        //
        // In a handover the vendor stack has already powered the boards and is
        // gone, and nothing is beating their MCUs yet. A POST refusal used to
        // return straight out of here: no heartbeat, no monitor, no ladder, and
        // fans left at the POST's 100 % -- the board kept energised only until
        // its MCU shed it 74-80 s later, unwatched the whole time.
        //
        // So a refusal de-energises, through the same path the scram uses --
        // heartbeat first (there is none yet), then opcode 14 and a rail read
        // from the MCU's ADC -- proved on hardware. On a
        // cold start the boards are already dark and it reports so. A false
        // refusal costs nothing extra: without a heartbeat the board was going
        // to shed anyway, and this only removes the unmonitored interval.
        if let Err(post_refusal) = self.power_on_self_test().await {
            error!(
                board = %self.config.device_id(),
                error = %post_refusal,
                "POST refused the start. De-energising now rather than leaving the boards \
                 to shed on their own, unmonitored, 74-80 s from the last heartbeat."
            );
            monitor::scram(
                &self.config.device_id(),
                &None,
                &self.driven_board_indices(),
            )
            .await;
            return Err(post_refusal);
        }
        // READ THE BOARD'S FAULT HISTORY BEFORE WE CAUSE ANY OF IT.
        //
        // The stock stack fetches MCU error and resets the MCU to init during
        // prepare-power-on, and reads it AGAIN as its own step before enabling
        // trip protection. The ordering is the point: a fault read before the
        // ramp stays distinguishable from anything the ramp itself causes.
        // Read it afterwards and the two are one pile.
        //
        // Here for two more reasons. The MCU answers with the rails down -- it
        // runs from its own supply -- so nothing has to be energised first.
        // And the fault queue demands an EXCLUSIVE reader: a concurrent query
        // shifts the MCU's counter parity, after which this handle discards a
        // real fault and returns a stale word as the queue's answer. The
        // heartbeat becomes that MCU's sole owner later in this function, so
        // before it starts is the only safe window.
        self.drain_board_faults_before_bringup().await;
        self.apply_bringup_sequence().await?;
        // Arm the silicon's own thermal/voltage protection before the long
        // bring-up stress, not after: the sensors come up powered down and
        // carry no thresholds, so a part cannot assert its own trip or
        // shutdown until firmware arms it.
        //
        // Only when there is a ramp and a sweep to protect. With bring-up
        // disabled -- a handover, or an observation run -- there is no ramp
        // and no calibration for this to precede, and the chain-attach path
        // arms the sensors exactly as it always has. Opening the chain port
        // here anyway buys nothing and costs a close-then-reopen of the same
        // tty, which some drivers punish with a refusal that latches until
        // reboot -- measured on hardware.
        if self.config.bringup.enabled {
            self.arm_on_die_protection_before_calibration().await;
        } else {
            info!(
                "Bring-up disabled, so no ramp or calibration to protect: on-die protection \
                 will be armed by the chain attach, not by a separate open of the port"
            );
        }
        let bus_layouts = self.resolve_bus_layouts().await?;
        *self.bus_layouts.lock().unwrap_or_else(|e| e.into_inner()) = bus_layouts.clone();
        let initial_snapshot = self.config.telemetry.snapshot();
        let initial_rail_snapshot = self.config.bringup.snapshot_telemetry();
        self.telemetry_tx.send_modify(|state| {
            state.fans = initial_snapshot.fans.clone();
            merge_temperature_readings(&mut state.temperatures, &initial_snapshot.temperatures);
            merge_power_readings(&mut state.powers, &initial_snapshot.powers);
            merge_temperature_readings(
                &mut state.temperatures,
                &initial_rail_snapshot.temperatures,
            );
            merge_power_readings(&mut state.powers, &initial_rail_snapshot.powers);
        });

        self.execute_live_calibration(&bus_layouts).await?;
        let post_calibration_rail_snapshot = self.config.bringup.snapshot_telemetry();
        self.telemetry_tx.send_modify(|state| {
            merge_temperature_readings(
                &mut state.temperatures,
                &post_calibration_rail_snapshot.temperatures,
            );
            merge_power_readings(&mut state.powers, &post_calibration_rail_snapshot.powers);
        });

        for (index, serial_path) in self.config.serial_paths.iter().enumerate() {
            let stream = crate::transport::serial::open_with_platform_cflag(
                serial_path,
                self.config.baud_rate,
            )
            .map_err(|err| {
                BoardError::InitializationFailed(format!(
                    "Failed to open BZM2 serial transport {}: {}",
                    serial_path, err
                ))
            })?;
            let (reader, writer, control) = config::split_chain_port(stream);
            let thread_name = format!("BZM2 UART {}", index);
            let mut config = Bzm2ThreadConfig::new(
                serial_path.clone(),
                self.config.baud_rate,
                self.config.nominal_hashrate_ths,
            );
            config.timestamp_count = self.config.timestamp_count;
            config.nonce_gap = self.config.nonce_gap;
            config.result_min_difficulty = self.config.result_min_difficulty;
            config.dispatch_interval = self.config.dispatch_interval;
            config.dts_vs_generation = self.config.dts_vs_generation;
            // Hand the thread the chain it is actually addressing. Without this
            // it cannot key result bookkeeping by (asic, engine), and it reports
            // a single-ASIC nameplate for the whole bus.
            // WIRE ids, not the global ones the layout carries. The thread
            // compares these against addresses that arrive on the chain, and a
            // chain is addressed from `start_id` on every bus regardless of
            // where the bus sits in the machine-wide numbering.
            config.asic_ids = bus_layouts
                .iter()
                .find(|layout| layout.serial_path == *serial_path)
                .map(|layout| layout.wire_asic_ids(self.config.enumeration.start_id))
                .filter(|ids| !ids.is_empty())
                .unwrap_or_else(|| vec![self.config.enumeration.start_id]);

            self.serial_controls.push(control.clone());
            let thread = Bzm2Thread::new(thread_name.clone(), reader, writer, control, config);
            self.shutdown_handles.push(thread.shutdown_handle());
            thread_states.push(ThreadTelemetry {
                name: thread_name,
                hashrate: 0,
                is_active: false,
            });
            threads.push(Box::new(Bzm2ManagedThread::new(
                Box::new(thread),
                self.telemetry_tx.clone(),
                index,
            )));
        }

        self.telemetry_tx.send_modify(|state| {
            state.threads = thread_states.clone();
        });

        self.spawn_heartbeats();
        self.spawn_thermal_fan_control();
        self.spawn_monitor();
        Ok(threads)
    }
}

struct Bzm2ManagedThread {
    inner: Box<dyn HashThread>,
    telemetry_tx: watch::Sender<BoardTelemetry>,
    thread_index: usize,
}

impl Bzm2ManagedThread {
    fn new(
        inner: Box<dyn HashThread>,
        telemetry_tx: watch::Sender<BoardTelemetry>,
        thread_index: usize,
    ) -> Self {
        Self {
            inner,
            telemetry_tx,
            thread_index,
        }
    }

    fn publish_status(&self, status: &HashThreadStatus) {
        publish_thread_status(&self.telemetry_tx, self.thread_index, status);
    }
}

#[async_trait]
impl HashThread for Bzm2ManagedThread {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn capabilities(&self) -> &HashThreadCapabilities {
        self.inner.capabilities()
    }

    async fn configure(&mut self) -> AnyhowResult<()> {
        self.inner.configure().await
    }

    async fn update_task(&mut self, new_task: HashTask) -> AnyhowResult<Option<HashTask>> {
        let result = self.inner.update_task(new_task).await;
        self.publish_status(&self.inner.status());
        result
    }

    async fn replace_task(&mut self, new_task: HashTask) -> AnyhowResult<Option<HashTask>> {
        let result = self.inner.replace_task(new_task).await;
        self.publish_status(&self.inner.status());
        result
    }

    async fn go_idle(&mut self) -> AnyhowResult<Option<HashTask>> {
        let result = self.inner.go_idle().await;
        self.publish_status(&self.inner.status());
        result
    }

    fn take_event_receiver(&mut self) -> Option<mpsc::Receiver<HashThreadEvent>> {
        let inner_rx = self.inner.take_event_receiver()?;
        let (event_tx, event_rx) = mpsc::channel(64);
        tokio::spawn(forward_thread_events(
            inner_rx,
            event_tx,
            self.telemetry_tx.clone(),
            self.thread_index,
        ));
        Some(event_rx)
    }

    fn status(&self) -> HashThreadStatus {
        self.inner.status()
    }
}
/// Relay one chain's events: its telemetry into the board's own state, and
/// what the scheduler acts on to the scheduler.
///
/// **TELEMETRY STOPS HERE.** Every event used to be forwarded, and the
/// scheduler does nothing with a telemetry update but trace it. A chain on a
/// full stream sends about a hundred of them every 200 ms, into two 64-slot
/// queues that only the scheduler's loop drains -- and that loop awaits each
/// chain's `update_task` in turn. While it waits on one chain, another's
/// queues fill, that chain's actor blocks sending telemetry, and it can then
/// never answer the `update_task` the scheduler sends it next: a deadlock
/// from the second chain onward (the pre-mining review, from the channel
/// sizes; one chain cannot reach it, which is why a single-chain run did not).
///
/// A status update is latest-wins and is already in the board state when
/// this forwards it, so one that finds the scheduler's queue full is dropped
/// and counted rather than awaited. Everything else is awaited as before.
async fn forward_thread_events(
    mut inner_rx: mpsc::Receiver<HashThreadEvent>,
    event_tx: mpsc::Sender<HashThreadEvent>,
    telemetry_tx: watch::Sender<BoardTelemetry>,
    thread_index: usize,
) {
    let mut dropped_status: u64 = 0;
    while let Some(event) = inner_rx.recv().await {
        match &event {
            HashThreadEvent::TelemetryUpdate(update) => {
                publish_thread_telemetry(&telemetry_tx, thread_index, update);
                continue;
            }
            HashThreadEvent::StatusUpdate(status) => {
                publish_thread_status(&telemetry_tx, thread_index, status);
                match event_tx.try_send(event) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        dropped_status += 1;
                        if dropped_status == 1 || dropped_status.is_multiple_of(1000) {
                            debug!(
                                thread_index,
                                dropped_status,
                                "Scheduler queue full: status update dropped (the board state has it)"
                            );
                        }
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => break,
                }
                continue;
            }
            _ => {}
        }
        if event_tx.send(event).await.is_err() {
            break;
        }
    }
}

async fn create_bzm2_board() -> AnyhowResult<BackplaneConnector> {
    let config = Bzm2RuntimeConfig::from_env()
        .ok_or_else(|| anyhow::anyhow!("BZM2 not configured (MUJINA_BZM2_SERIAL not set)"))?;

    let serial = config.device_id();
    let initial_state = BoardTelemetry {
        name: serial.clone(),
        model: "BZM2".into(),
        serial: Some(serial),
        ..Default::default()
    };
    let (telemetry_tx, telemetry_rx) = watch::channel(initial_state);

    let mut board = Bzm2Board::new(config, telemetry_tx);
    let info = board.board_info();

    // Bring-up, enumeration, calibration, and the monitor/command loops
    // all happen here; the returned threads are ready for the scheduler.
    let threads = board.create_hash_threads().await?;

    let shutdown = Box::pin(async move {
        if let Err(err) = board.shutdown().await {
            warn!(error = %err, "BZM2 board shutdown reported an error");
        }
    });

    Ok(BackplaneConnector {
        info,
        threads,
        telemetry_rx,
        command_tx: None,
        shutdown: Some(shutdown),
    })
}

#[cfg(all(test, unix))]
mod tests {

    use super::bringup::Bzm2BringupConfig;
    use super::config::{
        Bzm2CalibrationConfig, Bzm2EnumerationConfig, DEFAULT_BAUD_RATE,
        DEFAULT_NOMINAL_HASHRATE_THS,
    };
    use super::telemetry::{Bzm2TelemetryConfig, SensorSpec};
    use super::*;
    use crate::board::power::{VoltageStackBringupPlan, VoltageStackStep};
    use crate::types::Temperature;

    use nix::pty::openpty;
    use std::fs;
    use std::os::fd::AsRawFd;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// A chain streaming telemetry must never wait on a scheduler that is not
    /// reading: while the scheduler awaits another chain's `update_task`,
    /// nothing drains this chain's queue, and a chain blocked on telemetry
    /// cannot answer the scheduler when it is asked next.
    #[tokio::test]
    async fn a_chain_never_waits_on_the_scheduler_to_publish_telemetry() {
        let (inner_tx, inner_rx) = mpsc::channel(64);
        let (event_tx, mut event_rx) = mpsc::channel(64);
        let (telemetry_tx, _telemetry_rx) = watch::channel(BoardTelemetry::default());
        tokio::spawn(forward_thread_events(inner_rx, event_tx, telemetry_tx, 0));

        // Ten seconds of one chain's telemetry, and its status updates, with
        // the scheduler reading nothing.
        let streamed = tokio::time::timeout(Duration::from_secs(5), async {
            for _ in 0..5_000 {
                inner_tx
                    .send(HashThreadEvent::TelemetryUpdate(
                        crate::asic::hash_thread::HashThreadTelemetryUpdate {
                            temperatures: Vec::new(),
                            powers: Vec::new(),
                            asic: None,
                        },
                    ))
                    .await
                    .unwrap();
            }
            for _ in 0..500 {
                inner_tx
                    .send(HashThreadEvent::StatusUpdate(HashThreadStatus::default()))
                    .await
                    .unwrap();
            }
        })
        .await;
        assert!(
            streamed.is_ok(),
            "the chain blocked on a scheduler that was not reading"
        );
        assert!(
            event_rx.len() <= 64,
            "the scheduler's queue grew past its bound"
        );

        // An event the scheduler acts on still arrives once it reads again, and
        // nothing on the way to it is telemetry.
        inner_tx
            .send(HashThreadEvent::ExpectedHashRate(
                crate::types::HashRate::from_terahashes(1.0),
            ))
            .await
            .unwrap();
        let mut status_seen = 0;
        let arrived = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match event_rx.recv().await {
                    Some(HashThreadEvent::ExpectedHashRate(_)) => return true,
                    Some(HashThreadEvent::TelemetryUpdate(_)) => {
                        panic!("telemetry reached the scheduler")
                    }
                    Some(_) => status_seen += 1,
                    None => return false,
                }
            }
        })
        .await;
        assert_eq!(arrived, Ok(true), "the rate report never arrived");
        let _ = status_seen;
    }

    #[tokio::test]
    async fn create_hash_threads_applies_bringup_and_shutdown_sequences() {
        // THIS IS A BENCH, AND THE VARIANT HAS TO SAY SO.
        //
        // The POST judges against rds-dvt2's default variant, air-3b, which
        // declares four fans that must turn. A workstation has none, and the
        // whole point of post.rs is that absent-by-design and absent-by-fault
        // are indistinguishable from a probe -- so a test host declares
        // itself rather than being guessed at.
        unsafe { std::env::set_var("MUJINA_BZM2_VARIANT", "bench") };
        let pty = openpty(None, None).unwrap();
        let serial_path = fs::read_link(format!("/proc/self/fd/{}", pty.slave.as_raw_fd()))
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let rail0_path = std::env::temp_dir().join(format!("bzm2-rail0-{unique}.txt"));
        let rail1_path = std::env::temp_dir().join(format!("bzm2-rail1-{unique}.txt"));
        let enable0_path = std::env::temp_dir().join(format!("bzm2-enable0-{unique}.txt"));
        let enable1_path = std::env::temp_dir().join(format!("bzm2-enable1-{unique}.txt"));
        let reset_path = std::env::temp_dir().join(format!("bzm2-reset-{unique}.txt"));

        let config = Bzm2RuntimeConfig {
            serial_paths: vec![serial_path],
            baud_rate: DEFAULT_BAUD_RATE,
            timestamp_count: crate::asic::bzm2::protocol::DEFAULT_TIMESTAMP_COUNT,
            nonce_gap: crate::asic::bzm2::protocol::DEFAULT_NONCE_GAP,
            result_min_difficulty: None,
            dispatch_interval: Duration::from_millis(50),
            nominal_hashrate_ths: DEFAULT_NOMINAL_HASHRATE_THS,
            dts_vs_generation: crate::asic::bzm2::protocol::DtsVsGeneration::Gen2,
            telemetry: Bzm2TelemetryConfig::default(),
            enumeration: Bzm2EnumerationConfig::default(),
            bringup: Bzm2BringupConfig {
                enabled: true,
                rail_set_paths: vec![
                    rail0_path.to_string_lossy().into_owned(),
                    rail1_path.to_string_lossy().into_owned(),
                ],
                rail_write_scales: vec![1000.0, 1000.0],
                domain_rail_indices: Vec::new(),
                rail_enable_paths: vec![
                    enable0_path.to_string_lossy().into_owned(),
                    enable1_path.to_string_lossy().into_owned(),
                ],
                rail_enable_values: vec!["EN".into(), "ON".into()],
                rail_vin: Vec::new(),
                rail_vout: Vec::new(),
                rail_current: Vec::new(),
                rail_power: Vec::new(),
                rail_temperature: Vec::new(),
                reset_path: Some(reset_path.to_string_lossy().into_owned()),
                reset_active_low: true,
                plan: VoltageStackBringupPlan {
                    pre_power_delay: Duration::ZERO,
                    post_power_delay: Duration::ZERO,
                    release_reset_delay: Duration::ZERO,
                    steps: vec![
                        VoltageStackStep {
                            rail_index: 0,
                            voltage: 1.1,
                            settle_for: Duration::ZERO,
                        },
                        VoltageStackStep {
                            rail_index: 1,
                            voltage: 1.25,
                            settle_for: Duration::ZERO,
                        },
                    ],
                    ..Default::default()
                },
            },
            heartbeat: Default::default(),
            stored_calibration: None,
            calibration: Bzm2CalibrationConfig::default(),
        };
        let (telemetry_tx, _telemetry_rx) = watch::channel(BoardTelemetry {
            name: "bzm2-test".into(),
            model: "BZM2".into(),
            serial: Some("bzm2-test".into()),
            ..Default::default()
        });
        let mut board = Bzm2Board::new(config, telemetry_tx);

        let _threads = board.create_hash_threads().await.unwrap();

        assert_eq!(fs::read_to_string(&rail0_path).unwrap(), "1100");
        assert_eq!(fs::read_to_string(&rail1_path).unwrap(), "1250");
        assert_eq!(fs::read_to_string(&enable0_path).unwrap(), "EN");
        assert_eq!(fs::read_to_string(&enable1_path).unwrap(), "ON");
        assert_eq!(fs::read_to_string(&reset_path).unwrap(), "1");

        board.shutdown().await.unwrap();

        assert_eq!(fs::read_to_string(&rail0_path).unwrap(), "0");
        assert_eq!(fs::read_to_string(&rail1_path).unwrap(), "0");
        assert_eq!(fs::read_to_string(&reset_path).unwrap(), "0");

        // THE ENABLE NODES, which this test created, wrote, asserted after
        // bring-up, and then deleted after shutdown without ever looking at
        // again. It held the evidence in its hand. A rail whose setpoint is
        // zero but whose enable is still asserted is not off -- it is an
        // enabled rail commanded to zero volts, a different electrical state
        // and not the one `shutdown()` is asked for.
        assert_eq!(
            fs::read_to_string(&enable0_path).unwrap(),
            "0",
            "shutdown must de-assert the enable node, not only zero the setpoint"
        );
        assert_eq!(
            fs::read_to_string(&enable1_path).unwrap(),
            "0",
            "shutdown must de-assert the enable node, not only zero the setpoint"
        );

        let _ = fs::remove_file(rail0_path);
        let _ = fs::remove_file(rail1_path);
        let _ = fs::remove_file(enable0_path);
        let _ = fs::remove_file(enable1_path);
        let _ = fs::remove_file(reset_path);
        drop(pty);
    }

    #[tokio::test]
    async fn create_hash_threads_publishes_rail_telemetry() {
        // THIS IS A BENCH, AND THE VARIANT HAS TO SAY SO.
        //
        // The POST judges against rds-dvt2's default variant, air-3b, which
        // declares four fans that must turn. A workstation has none, and the
        // whole point of post.rs is that absent-by-design and absent-by-fault
        // are indistinguishable from a probe -- so a test host declares
        // itself rather than being guessed at.
        unsafe { std::env::set_var("MUJINA_BZM2_VARIANT", "bench") };
        let pty = openpty(None, None).unwrap();
        let serial_path = fs::read_link(format!("/proc/self/fd/{}", pty.slave.as_raw_fd()))
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let vin_path = std::env::temp_dir().join(format!("bzm2-vin-{unique}.txt"));
        let vout_path = std::env::temp_dir().join(format!("bzm2-vout-{unique}.txt"));
        let current_path = std::env::temp_dir().join(format!("bzm2-current-{unique}.txt"));
        let power_path = std::env::temp_dir().join(format!("bzm2-power-{unique}.txt"));
        let temp_path = std::env::temp_dir().join(format!("bzm2-temp-{unique}.txt"));
        fs::write(&vin_path, "12000\n").unwrap();
        fs::write(&vout_path, "850\n").unwrap();
        fs::write(&current_path, "1500\n").unwrap();
        fs::write(&power_path, "1275\n").unwrap();
        fs::write(&temp_path, "47000\n").unwrap();

        let config = Bzm2RuntimeConfig {
            serial_paths: vec![serial_path],
            baud_rate: DEFAULT_BAUD_RATE,
            timestamp_count: crate::asic::bzm2::protocol::DEFAULT_TIMESTAMP_COUNT,
            nonce_gap: crate::asic::bzm2::protocol::DEFAULT_NONCE_GAP,
            result_min_difficulty: None,
            dispatch_interval: Duration::from_millis(50),
            nominal_hashrate_ths: DEFAULT_NOMINAL_HASHRATE_THS,
            dts_vs_generation: crate::asic::bzm2::protocol::DtsVsGeneration::Gen2,
            telemetry: Bzm2TelemetryConfig::default(),
            enumeration: Bzm2EnumerationConfig::default(),
            bringup: Bzm2BringupConfig {
                rail_vin: vec![SensorSpec {
                    path: vin_path.to_string_lossy().into_owned(),
                    scale: 0.001,
                }],
                rail_vout: vec![SensorSpec {
                    path: vout_path.to_string_lossy().into_owned(),
                    scale: 0.001,
                }],
                rail_current: vec![SensorSpec {
                    path: current_path.to_string_lossy().into_owned(),
                    scale: 0.001,
                }],
                rail_power: vec![SensorSpec {
                    path: power_path.to_string_lossy().into_owned(),
                    scale: 0.001,
                }],
                rail_temperature: vec![SensorSpec {
                    path: temp_path.to_string_lossy().into_owned(),
                    scale: 0.001,
                }],
                ..Default::default()
            },
            heartbeat: Default::default(),
            stored_calibration: None,
            calibration: Bzm2CalibrationConfig::default(),
        };
        let (telemetry_tx, telemetry_rx) = watch::channel(BoardTelemetry {
            name: "bzm2-test".into(),
            model: "BZM2".into(),
            serial: Some("bzm2-test".into()),
            ..Default::default()
        });
        let mut board = Bzm2Board::new(config, telemetry_tx);

        let _threads = board.create_hash_threads().await.unwrap();
        let state = telemetry_rx.borrow().clone();
        assert!(state.temperatures.iter().any(|sensor| {
            sensor.name == "rail0-regulator"
                && sensor
                    .temperature
                    .map(Temperature::as_degrees_c)
                    .is_some_and(|value| (value - 47.0).abs() < 0.001)
        }));
        assert!(state.powers.iter().any(|power| {
            power.name == "rail0-input"
                && power
                    .voltage_v
                    .is_some_and(|value| (value - 12.0).abs() < 0.001)
        }));
        assert!(state.powers.iter().any(|power| {
            power.name == "rail0-output"
                && power
                    .voltage_v
                    .is_some_and(|value| (value - 0.85).abs() < 0.001)
                && power
                    .current_a
                    .is_some_and(|value| (value - 1.5).abs() < 0.001)
                && power
                    .power_w
                    .is_some_and(|value| (value - 1.275).abs() < 0.001)
        }));

        board.shutdown().await.unwrap();

        let _ = fs::remove_file(vin_path);
        let _ = fs::remove_file(vout_path);
        let _ = fs::remove_file(current_path);
        let _ = fs::remove_file(power_path);
        let _ = fs::remove_file(temp_path);
        drop(pty);
    }
}
