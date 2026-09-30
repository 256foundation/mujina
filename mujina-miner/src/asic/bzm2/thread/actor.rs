use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;

use crate::asic::hash_thread::{HashTask, HashThreadEvent, HashThreadStatus, TelemetryCoalescer};
use crate::transport::serial::{SerialControl, SerialReader, SerialWriter};
use crate::types::HashRate;

use super::super::protocol::{BROADCAST_ASIC, Bzm2EngineLayout, TdmFrame, TdmFrameParser};
use super::super::uart::{
    Bzm2DtsVsConfig, configure_dts_vs_stream, confirm_on_die_protection, confirm_stream_stopped,
    disable_dts_vs_sensors, soft_reset_engines, suspend_dts_vs_stream,
};
use super::dispatch::*;
use super::engine::*;
use super::interlock::*;
use super::metrics::*;
use super::results::*;
use super::telemetry::*;
use super::*;

/// Should attaching return the chain to rest first?
///
/// On by default. Mujina reaches this point only when it has been told to drive
/// this board, and a chain it cannot get a reply from is of no use to anyone.
/// Set `MUJINA_BZM2_NO_QUIESCE=1` to attach without disturbing what is running
/// -- appropriate when something else owns the chain and is expected to keep it.
///
/// Read once, so a run cannot change its mind midway.
fn quiesce_on_attach() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            std::env::var("MUJINA_BZM2_NO_QUIESCE")
                .as_deref()
                .map(str::trim),
            Ok("1") | Ok("true") | Ok("yes") | Ok("on")
        )
    })
}

/// Ceiling on the DTS/VS bring-up write burst, which runs before the actor's
/// select loop. Bounded so a part that never accepts the configuration cannot
/// hang the hash thread at startup — telemetry is not worth blocking bring-up.
const DTS_VS_BRING_UP_TIMEOUT: Duration = Duration::from_millis(500);
/// Ceiling on the writes that quiet an inherited chain.
///
/// Bounded for the same reason bring-up is: **no startup write may be able to
/// hang the thread.** A part that will not accept a write must cost a warning
/// and a degraded run, never a daemon that starts, logs six lines and stops.
///
/// That is not hypothetical. These writes were added without a bound and a
/// handover window was lost to exactly it: Mujina applied its port settings,
/// reached the first quiesce write, and never emitted another line. The
/// pre-existing bring-up timeout had been justified in a comment directly above
/// the code I was editing, and I did not extend the same protection to writes
/// that run earlier and have the same failure mode.
pub(super) const CHAIN_QUIESCE_TIMEOUT: Duration = Duration::from_millis(750);

/// How long the chain must be silent before we consider the line drained.
const DTS_VS_DRAIN_QUIET: Duration = Duration::from_millis(120);
/// Hard cap on drained bytes, so a chain that streams continuously cannot
/// hold up attach forever. Reaching it is itself worth reporting.
const DTS_VS_DRAIN_LIMIT: usize = 64 * 1024;

/// Read and discard whatever is already on the line, stopping once it has been
/// quiet for `quiet` or once `limit` bytes have been thrown away.
///
/// Returns the number of bytes discarded. Zero means the line was already
/// idle, which is the cold-start case.
async fn drain_serial_input<R: AsyncReadExt + Unpin>(
    reader: &mut R,
    quiet: Duration,
    limit: usize,
) -> usize {
    let mut scratch = [0u8; 1024];
    let mut total = 0usize;
    loop {
        match tokio::time::timeout(quiet, reader.read(&mut scratch)).await {
            // Quiet for the whole window: the line is ours.
            Err(_) => return total,
            // EOF or error: nothing more to drain, and the caller will find
            // out soon enough through its own read.
            Ok(Ok(0)) | Ok(Err(_)) => return total,
            Ok(Ok(n)) => {
                total += n;
                if total >= limit {
                    return total;
                }
            }
        }
    }
}
/// Ceiling on the register write that takes the sensor stream off the TDM
/// path at shutdown. Best effort: a part that does not accept it is left
/// streaming, which the next open handles.
const DTS_VS_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(500);

/// THE FIRST DISPATCH SINCE A RESET SAYS WHAT IT WAS ALLOWED ON.
///
/// The interlock logs a hold and a release, but a chain whose dies were
/// already fresh logged nothing, so no run could show that it waited for them
/// (measured on hardware). Called at every site that dispatches: the first version
/// covered only the dispatch tick, and the first dispatch after a reset
/// happens in the task handlers, so an earlier version had no such line at all.
fn log_first_dispatch(interlock: &ThermalInterlock, config: &Bzm2ThreadConfig) {
    let (fresh, expected, hottest) = interlock.fresh_count();
    info!(
        path = %config.serial_path,
        fresh, expected, hottest_c = hottest,
        "First dispatch since the reset: the interlock read every guarded device fresh"
    );
}

/// How long the chain must stay silent before we believe the stream stopped.
///
/// At the rates this part streams at, a running stream produces a frame in far
/// less than this -- measured up to 19,389 frames a second -- so a window this
/// long is generous rather than tight, and the cost of being generous is paid
/// once per attach.
const DTS_VS_QUIET_CONFIRM: Duration = Duration::from_millis(300);

pub(super) async fn bzm2_thread_actor(
    mut command_rx: mpsc::Receiver<ThreadCommand>,
    event_tx: mpsc::Sender<HashThreadEvent>,
    status: Arc<RwLock<HashThreadStatus>>,
    mut reader: SerialReader,
    mut writer: SerialWriter,
    control: SerialControl,
    config: Bzm2ThreadConfig,
) {
    if let Err(err) = control.set_baud_rate(config.baud_rate) {
        warn!(path = %config.serial_path, error = %err, "Failed to set BZM2 baud rate");
    }

    // Run metadata. Every result this thread reports has its nonce corrected
    // by `nonce_gap`, its ntime rebuilt from `timestamp_count`, and its
    // version looked up from the candidate set. Those are properties of the
    // run, fixed for the thread's lifetime (`config` is immutable here), and
    // any dataset built from the result log is uninterpretable without them.
    // A restart with different values starts a new run and logs a new line.
    info!(
        path = %config.serial_path,
        nonce_gap = format!("{:#x}", config.nonce_gap),
        timestamp_count = config.timestamp_count,
        version_candidates = ?VERSION_CANDIDATES,
        result_min_difficulty = config.result_min_difficulty.map(|d| d.as_f64()),
        dispatch_ms = config.dispatch_interval.as_millis(),
        "BZM2 result reconstruction parameters"
    );

    let _ = event_tx
        .send(HashThreadEvent::StatusUpdate(snapshot_status(&status)))
        .await;

    let engine_layout = Bzm2EngineLayout::default();

    let mut parser = TdmFrameParser::new(config.dts_vs_generation);

    let mut current_task: Option<HashTask> = None;
    // Recent dispatches, so a hit read after the next dispatch still finds
    // its own work. UpdateTask leaves them; ReplaceTask and GoIdle
    // invalidate them. See `DispatchRing`.
    let mut engine_dispatches = DispatchRing::default();

    // One interlock per chain, owned by the actor that drives it. It starts
    // refusing: no reading has been observed yet, and unknown reads as hot.

    let mut corroborator = FaultCorroborator::default();
    let mut dts_vs_diagnostics = DtsVsDiagnostics::default();
    let mut telemetry_coalescer = TelemetryCoalescer::new(TELEMETRY_PUBLISH_INTERVAL);

    // WHETHER WORK IS HELD BACK BY THE INTERLOCK, as opposed to absent.
    //
    // A task the interlock will not let us send yet is ACCEPTED and DEFERRED,
    // not failed. It used to be answered with Err while the thread kept it as
    // `current_task`: the scheduler, told the assignment failed, never
    // registered the task's share receiver, and the dispatch tick then sent the
    // task anyway once the dies cooled -- so every share it found went to a
    // dropped channel. On the very first dispatch, when the first job can
    // easily arrive before the sensor stream has delivered a reading, that is a
    // run that reports hashrate and delivers no shares at all.
    //
    // Tracked so `is_active` stays honest -- "being given work" -- and so the
    // deferral is logged once per transition rather than every 500 ms tick.
    let mut dispatch_deferred = false;

    let mut interlock = ThermalInterlock::new(
        config.thermal_ceiling_c,
        Duration::from_secs(config.thermal_reading_max_age_s.into()),
    )
    .expecting(&config.asic_ids);

    let mut base_sequence: u8 = 0;
    let mut dispatch_tick = tokio::time::interval(config.dispatch_interval);
    dispatch_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut ntime_tick = tokio::time::interval(Duration::from_secs(1));
    ntime_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut status_tick = tokio::time::interval(Duration::from_secs(5));
    status_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut read_buf = [0u8; 4096];
    let mut dts_vs = DtsVsStream::default();
    // A second handle on the same device, used only to suspend/resume the sensor
    // stream around a diagnostic. Separate from `writer` so the borrow checker
    // permits the diagnostic closure to hold `writer` at the same time; both are
    // `Arc`-backed views of one port and are never written concurrently.
    let mut control_writer = writer.clone();

    let mut runtime_measurements = ThreadRuntimeMeasurementState::new();

    // Bring DTS/VS streaming up here rather than lazily on the first operator
    // telemetry query. Without this a default boot has no per-ASIC die
    // temperatures at all, which makes the thermal-drift retune trigger in
    // `board/bzm2/monitor.rs` structurally dead — and on bitaxeBIRDS, which
    // carries no board temperature sensor, leaves the board with no thermal
    // source whatsoever.
    //
    // Write-only: no synchronous read here. Reading the pre-enable control value
    // is deferred to the first diagnostic that actually needs to suspend, where
    // it is done with the TDM-aware read. Doing it here instead would block the
    // actor before its select loop on a part that may not answer, and would
    // consume whatever frame arrived next — including, on a warm restart where a
    // previous run left streaming on, a sensor frame.
    // Drain the line before the first transaction.
    //
    // The comment above notes that a previous run may have left streaming on.
    // It does more than that: a chain handed over from another controller is
    // still streaming when we open the port, so our first read lands wherever
    // the stream happens to be rather than on a frame boundary. Measured on
    // hardware over four handovers, the header check then failed with a
    // DIFFERENT mismatch each time -- the signature of a phase error, not of a
    // protocol disagreement -- and the values that did parse were nonsense: a
    // reading attributed to a device outside the chain, and a die temperature
    // below ambient.
    //
    // So: read and discard until the line has been quiet briefly, bounded, and
    // only then talk. On a cold chain this costs one short timeout and finds
    // nothing, which is the common case and cheap.
    // SILENCE FIRST, then drain. Draining alone does not work.
    //
    // Measured on hardware: after taking over a chain the drain reported bytes
    // already on the line, and bring-up still failed on a framing mismatch,
    // because the stream is CONTINUOUS. There is no idle gap to wait for, so a
    // drain can only ever catch up to a moving target.
    //
    // The off value for the sensor stream is a documented constant, so we can
    // write it blind, without a read, which matters because reading requires
    // the framing we do not yet have. Silence the stream, let the last frames
    // in flight land, discard them, and only then talk.
    // STOP THE WORK FIRST. Silencing the sensor stream is not enough on its own.
    //
    // Measured after taking over a running chain: the sensor stream went quiet
    // and the line stayed saturated anyway, with 119,560 result frames in about
    // twenty seconds. The engines were still hashing whatever the previous owner
    // last gave them, and every reply to every request drowned in the results.
    //
    // Nothing Mujina has issued can be lost here: this runs once, at attach,
    // before any work of ours exists. What it can end is somebody else's mining,
    // which is the point on a handover and is why it says so loudly rather than
    // doing it quietly.
    let reset_on_attach = quiesce_on_attach();
    if reset_on_attach {
        match tokio::time::timeout(
            CHAIN_QUIESCE_TIMEOUT,
            soft_reset_engines(&mut writer, BROADCAST_ASIC),
        )
        .await
        {
            Ok(Ok(())) => info!(path = %config.serial_path,
                "Returned the chain to rest before attaching; any work it was already running has ended"),
            Ok(Err(err)) => warn!(path = %config.serial_path, error = %err,
                "Could not return the chain to rest; if it was already running, replies may be drowned by its results"),
            Err(_) => warn!(path = %config.serial_path,
                "Returning the chain to rest timed out; continuing, but a busy chain may drown replies"),
        }
    } else {
        info!(path = %config.serial_path,
            "Quiesce on attach is disabled; attaching to a chain that is already working will not succeed");
    }

    // Disable, not suspend. Suspending stops the part sending what it has and
    // leaves the sensors refilling it; we are taking the chain over and will
    // configure telemetry ourselves, so the sensors go off.
    match tokio::time::timeout(CHAIN_QUIESCE_TIMEOUT, disable_dts_vs_sensors(&mut writer)).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => warn!(path = %config.serial_path, error = %err,
            "Could not turn the sensors off before bring-up; the line may still be busy"),
        Err(_) => warn!(path = %config.serial_path,
            "Turning the sensors off timed out; continuing without it"),
    }
    let drained = drain_serial_input(&mut reader, DTS_VS_DRAIN_QUIET, DTS_VS_DRAIN_LIMIT).await;
    if drained > 0 {
        info!(path = %config.serial_path, bytes = drained,
            "Drained bytes left on the chain after silencing the stream");
    }

    // PROVE THE LINE IS ACTUALLY QUIET BEFORE TRUSTING IT.
    //
    // Everything above told the part to stop and then assumed it had. This
    // asks. The evidence is an ABSENCE -- a quiet window after a drain -- and
    // a frame arriving instead is proof the silencing did not take.
    //
    // This is the precondition the whole attach path exists to establish, and
    // its failure is the most expensive one this project has had: a stream
    // still running when the first transaction goes out is read as replies,
    // which produced a device outside the chain, a temperature below ambient,
    // and a hardware fault that never happened.
    match confirm_stream_stopped(&mut reader, config.dts_vs_generation, DTS_VS_QUIET_CONFIRM).await
    {
        Ok(drained) => info!(path = %config.serial_path, drained,
            "Chain confirmed quiet; the line is ours"),
        Err(err) => warn!(path = %config.serial_path, error = %err,
            "Could NOT confirm the chain is quiet. Anything read now may be another \
             owner's telemetry rather than our reply."),
    }

    // UNGATE THE ENGINES THE RESET ABOVE GATED, and read back that it took.
    // Here, because the readback needs the quiet line this point guarantees
    // and the stream is configured next. See `EngineGate`.
    let mut engine_gate = if reset_on_attach {
        ungate_engines_and_confirm(&mut reader, &mut writer, &engine_layout, &config).await
    } else {
        EngineGate::Inherited
    };
    match engine_gate.refusal() {
        None => info!(path = %config.serial_path, gate = ?engine_gate,
            "Engines ungated and confirmed running-ready"),
        Some(why) => error!(path = %config.serial_path, gate = ?engine_gate, reason = %why,
            "ENGINES NOT READY: work will be accepted and NOT dispatched"),
    }
    // One ERROR line per refusal episode, not one per job or per tick.
    let mut engine_refusal_logged = engine_gate.refusal().is_some();
    // Whether anything of ours may be hashing now: a stop must reset only then
    // on a chain whose previous owner's state this attach left alone.
    let mut dispatched_since_reset = false;
    // Extranonce2 consumed from the current task, so each dispatch hashes
    // values no earlier dispatch of this task did. Reset per new task.
    let mut en2_cursor: u64 = 0;

    let bring_up = tokio::time::timeout(
        DTS_VS_BRING_UP_TIMEOUT,
        configure_dts_vs_stream(&mut writer, &mut reader, &Bzm2DtsVsConfig::from_env()),
    )
    .await;

    match bring_up {
        Ok(Ok(())) => {
            dts_vs.enabled = true;
            // CONFIRM THE ON-DIE PROTECTION IS ACTUALLY ARMED.
            //
            // configure_dts_vs_stream broadcasts the thermal trip code and both
            // under-voltage shutdown thresholds to every device and proceeds.
            // That is the silicon's last-resort protection, and a broadcast
            // write is acknowledged by the bus rather than by a hundred parts.
            // The stream reports, unprompted, whether those sensors are
            // enabled -- so a frame carrying them is the device saying the
            // arming took, in a message we did not ask it for.
            match confirm_on_die_protection(
                &mut reader,
                config.dts_vs_generation,
                BROADCAST_ASIC,
                DTS_VS_BRING_UP_TIMEOUT,
            )
            .await
            {
                Ok(who) => info!(
                    path = %config.serial_path,
                    asic = who,
                    "On-die over-temperature and under-voltage protection confirmed armed"
                ),
                Err(err) => warn!(
                    path = %config.serial_path,
                    error = %err,
                    "COULD NOT CONFIRM on-die protection is armed. The thresholds were written \
                     and no device has reported them in force, so the silicon's last-resort \
                     protection is UNVERIFIED -- not known to be absent, and not known to be \
                     there."
                ),
            }
        }
        Ok(Err(err)) => {
            warn!(path = %config.serial_path, error = %err,
                "Failed to bring up BZM2 DTS/VS streaming; telemetry falls back to lazy enable");
        }
        Err(_) => {
            warn!(path = %config.serial_path, timeout_ms = DTS_VS_BRING_UP_TIMEOUT.as_millis(),
                "Timed out bringing up BZM2 DTS/VS streaming; telemetry falls back to lazy enable");
        }
    }

    // EACH ASIC'S OWN SLICE OF THE NONCE SPACE, before any work -- and AFTER
    // the stream bring-up, never before it. The silicon does not split work,
    // so without these every ASIC hashes the same nonces. They are about
    // 519 KB for a hundred-device chain, and the driver accepts bytes long
    // before the wire carries them (~1.15 s here). Written before the bring-up,
    // they put the bring-up's 500 ms register read behind that backlog: it
    // timed out, the stream never started, the interlock refused all work and
    // the monitor scrammed an idle board, measured on hardware.
    // Here only the first dispatch, write-only, waits behind them. A slice
    // write that fails leaves the ASICs overlapping, which refuses dispatch.
    if engine_gate.refusal().is_none() {
        match program_nonce_slices(&mut writer, &engine_layout, &config).await {
            Ok(()) => info!(path = %config.serial_path, asics = config.asic_ids.len(),
                "Per-ASIC nonce slices set"),
            Err(err) => {
                engine_gate =
                    EngineGate::Unmeasured(format!("the nonce slices could not be written: {err}"));
                error!(path = %config.serial_path, gate = ?engine_gate,
                    "ENGINES NOT READY: work will be accepted and NOT dispatched");
                engine_refusal_logged = true;
            }
        }
    }

    loop {
        tokio::select! {
            Some(command) = command_rx.recv() => {
                match command {
                    ThreadCommand::Configure => {
                        // Nameplate chain rate; a rough stand-in until the
                        // board layer supplies a calibration-derived figure.
                        // Scaled by chain length - a flat single-ASIC figure
                        // under-reports a multi-ASIC bus by exactly the number
                        // of chips on it.
                        let expected =
                            HashRate::from_terahashes(config.expected_chain_hashrate_ths());
                        if event_tx.send(HashThreadEvent::ExpectedHashRate(expected)).await.is_err() {
                            debug!("Event channel closed during configure");
                        }
                    }


                    ThreadCommand::UpdateTask { new_task, response_tx } => {
                        let old = current_task.replace(new_task);
                        en2_cursor = 0;
                        if let Some(ref task) = current_task {
                            // Engines first: a task for gated engines is
                            // accepted and held, like a thermal deferral.
                            if !engines_may_dispatch(
                                &mut writer,
                                &mut engine_gate,
                                &engine_layout,
                                &config,
                                &mut engine_refusal_logged,
                            )
                            .await
                            {
                                set_active(&status, false, config.expected_chain_hashrate_ths());
                                let _ = response_tx.send(Ok(old));
                                continue;
                            }
                            // ACCEPTED, DEFERRED. See `dispatch_deferred`.
                            if let Err(refusal) = interlock.check() {
                                interlock.record_refusal();
                                if !dispatch_deferred {
                                    dispatch_deferred = true;

                                    // A HOLD TAKES THE LOAD OFF. A running engine repeats its last job

                                    // until given another, so deferring dispatch alone left a hot chain

                                    // hashing through the very hold meant to cool it.

                                    if dispatched_since_reset {

                                        stop_engines(&mut writer, &config).await;

                                        engine_gate = EngineGate::Stopped;

                                        dispatched_since_reset = false;

                                    }
                                    set_active(&status, false, config.expected_chain_hashrate_ths());
                                    info!(
                                        path = %config.serial_path,
                                        reason = %refusal,
                                        "Task accepted; dispatch DEFERRED until the thermal \
                                         interlock allows it. The scheduler keeps the share \
                                         channel and the dispatch tick sends it when it is safe."
                                    );
                                }
                                let _ = response_tx.send(Ok(old));
                                continue;
                            }
                            if let Err(err) = dispatch_task_to_board(
                                &mut writer,
                                task,
                                base_sequence,
                                &engine_layout,
                                &mut engine_dispatches,
                                &config,
                                &mut interlock,
                                &engine_gate,
                                &mut en2_cursor,
                            ).await {
                                let _ = response_tx.send(Err(err));
                                continue;
                            }
                            base_sequence = base_sequence.wrapping_add(1);
                            if !dispatched_since_reset {
                                log_first_dispatch(&interlock, &config);
                            }
                            dispatched_since_reset = true;
                            dispatch_deferred = false;
                            set_active(&status, true, config.expected_chain_hashrate_ths());

                            refresh_status_hashrate(
                                &status,
                                &mut runtime_measurements,
                                config.expected_chain_hashrate_ths(),
                            );

                            let _ = event_tx.send(HashThreadEvent::StatusUpdate(snapshot_status(&status))).await;
                        }
                        let _ = response_tx.send(Ok(old));
                    }
                    ThreadCommand::ReplaceTask { new_task, response_tx } => {
                        // clean_jobs: the pool takes no share for anything
                        // dispatched before this, so nothing resolves to it.
                        engine_dispatches.invalidate();
                        let old = current_task.replace(new_task);
                        en2_cursor = 0;
                        if let Some(ref task) = current_task {
                            // Engines first: a task for gated engines is
                            // accepted and held, like a thermal deferral.
                            if !engines_may_dispatch(
                                &mut writer,
                                &mut engine_gate,
                                &engine_layout,
                                &config,
                                &mut engine_refusal_logged,
                            )
                            .await
                            {
                                set_active(&status, false, config.expected_chain_hashrate_ths());
                                let _ = response_tx.send(Ok(old));
                                continue;
                            }
                            // ACCEPTED, DEFERRED. See `dispatch_deferred`.
                            if let Err(refusal) = interlock.check() {
                                interlock.record_refusal();
                                if !dispatch_deferred {
                                    dispatch_deferred = true;

                                    // A HOLD TAKES THE LOAD OFF. A running engine repeats its last job

                                    // until given another, so deferring dispatch alone left a hot chain

                                    // hashing through the very hold meant to cool it.

                                    if dispatched_since_reset {

                                        stop_engines(&mut writer, &config).await;

                                        engine_gate = EngineGate::Stopped;

                                        dispatched_since_reset = false;

                                    }
                                    set_active(&status, false, config.expected_chain_hashrate_ths());
                                    info!(
                                        path = %config.serial_path,
                                        reason = %refusal,
                                        "Task accepted; dispatch DEFERRED until the thermal \
                                         interlock allows it. The scheduler keeps the share \
                                         channel and the dispatch tick sends it when it is safe."
                                    );
                                }
                                let _ = response_tx.send(Ok(old));
                                continue;
                            }
                            if let Err(err) = dispatch_task_to_board(
                                &mut writer,
                                task,
                                base_sequence,
                                &engine_layout,
                                &mut engine_dispatches,
                                &config,
                                &mut interlock,
                                &engine_gate,
                                &mut en2_cursor,
                            ).await {
                                let _ = response_tx.send(Err(err));
                                continue;
                            }
                            base_sequence = base_sequence.wrapping_add(1);
                            if !dispatched_since_reset {
                                log_first_dispatch(&interlock, &config);
                            }
                            dispatched_since_reset = true;
                            dispatch_deferred = false;
                            set_active(&status, true, config.expected_chain_hashrate_ths());

                            refresh_status_hashrate(
                                &status,
                                &mut runtime_measurements,
                                config.expected_chain_hashrate_ths(),
                            );

                            let _ = event_tx.send(HashThreadEvent::StatusUpdate(snapshot_status(&status))).await;
                        }
                        let _ = response_tx.send(Ok(old));
                    }
                    ThreadCommand::GoIdle { response_tx } => {
                        engine_dispatches.invalidate();
                        // Idle means the load comes off, not just that no
                        // new job is sent. See `stop_engines`.
                        if dispatched_since_reset {
                            stop_engines(&mut writer, &config).await;
                            engine_gate = EngineGate::Stopped;
                            dispatched_since_reset = false;
                        }
                        let old = current_task.take();
                        set_active(&status, false, config.expected_chain_hashrate_ths());

                        refresh_status_hashrate(
                            &status,
                            &mut runtime_measurements,
                            config.expected_chain_hashrate_ths(),
                        );

                        let _ = event_tx.send(HashThreadEvent::StatusUpdate(snapshot_status(&status))).await;
                        let _ = response_tx.send(Ok(old));
                    }

                    ThreadCommand::QueryRuntimeMetrics { response_tx } => {
                        let _ = response_tx.send(Ok(runtime_measurements.snapshot_at(Instant::now())));
                    }

                    ThreadCommand::Shutdown => break,
                }
            }
            read_result = reader.read(&mut read_buf) => {
                match read_result {
                    Ok(0) => break,
                    Ok(n) => {

                        let mut should_shutdown = false;
                        for frame in parser.push(&read_buf[..n]) {
                            match frame {

                                TdmFrame::Result(frame) => {
                                    handle_result_frame(
                                        &frame,
                                        &engine_dispatches,
                                        &engine_layout,
                                        &config,
                                        &status,
                                        &event_tx,
                                        &mut runtime_measurements,
                                    )
                                    .await;

                                }
                                TdmFrame::DtsVs(frame) => {
                                    should_shutdown = handle_dts_vs_frame(&frame, &config, &status, &event_tx, Some(&mut interlock), &mut corroborator, &mut dts_vs_diagnostics, &mut telemetry_coalescer).await;
                                    if should_shutdown {
                                        break;
                                    }
                                }
                                TdmFrame::Register(_) | TdmFrame::Noop(_) => {}
                            }
                        }
                        if should_shutdown {
                            break;
                        }
                    }
                    Err(err) => {
                        error!(path = %config.serial_path, error = %err, "BZM2 serial read failed");
                        record_hardware_error(&status);
                        break;
                    }
                }
            }

            _ = dispatch_tick.tick(), if current_task.is_some() => {
                if let Some(ref task) = current_task {
                    if !engines_may_dispatch(
                        &mut writer,
                        &mut engine_gate,
                        &engine_layout,
                        &config,
                        &mut engine_refusal_logged,
                    )
                    .await
                    {
                        continue;
                    }
                    // A THERMAL HOLD IS NOT A HARDWARE ERROR. This path used to
                    // log every interlock refusal at ERROR as "BZM2 dispatch
                    // failed" and count it with record_hardware_error, twice a
                    // second for as long as the dies stayed hot -- a deliberate
                    // safety refusal mislabelled as a fault, and a log flood.
                    if let Err(refusal) = interlock.check() {
                        interlock.record_refusal();
                        if !dispatch_deferred {
                            dispatch_deferred = true;

                            // A HOLD TAKES THE LOAD OFF. A running engine repeats its last job

                            // until given another, so deferring dispatch alone left a hot chain

                            // hashing through the very hold meant to cool it.

                            if dispatched_since_reset {

                                stop_engines(&mut writer, &config).await;

                                engine_gate = EngineGate::Stopped;

                                dispatched_since_reset = false;

                            }
                            // Not being given work, so not active: the stall
                            // watch must not read a thermal hold as a dead chain.
                            set_active(&status, false, config.expected_chain_hashrate_ths());
                            warn!(
                                path = %config.serial_path,
                                reason = %refusal,
                                "Thermal interlock is holding dispatch; the chain is idle \
                                 until it allows work again"
                            );
                        }
                        continue;
                    }
                    match dispatch_task_to_board(
                        &mut writer,
                        task,
                        base_sequence,
                        &engine_layout,
                        &mut engine_dispatches,
                        &config,
                                &mut interlock,
                                &engine_gate,
                                &mut en2_cursor,
                    ).await {
                        Ok(()) => {
                            base_sequence = base_sequence.wrapping_add(1);
                            // THE FIRST DISPATCH SINCE A RESET SAYS WHAT IT
                            // WAS ALLOWED ON. The interlock logs a hold and a
                            // release, but a chain whose dies were already
                            // fresh logged nothing, so no run could show that
                            // it waited for them, measured on hardware.
                            if !dispatched_since_reset {
                                log_first_dispatch(&interlock, &config);
                            }
                            dispatched_since_reset = true;
                            if dispatch_deferred {
                                dispatch_deferred = false;
                                set_active(&status, true, config.expected_chain_hashrate_ths());

                                refresh_status_hashrate(
                                    &status,
                                    &mut runtime_measurements,
                                    config.expected_chain_hashrate_ths(),
                                );

                                let _ = event_tx
                                    .send(HashThreadEvent::StatusUpdate(snapshot_status(&status)))
                                    .await;
                                info!(
                                    path = %config.serial_path,
                                    "Thermal interlock allows dispatch again; deferred work sent"
                                );
                            }
                        }
                        Err(err) => {
                            error!(path = %config.serial_path, error = %err, "BZM2 dispatch failed");
                            record_hardware_error(&status);
                        }
                    }
                }
            }
            _ = ntime_tick.tick(), if current_task.is_some() => {
                if let Some(ref mut task) = current_task {
                    task.ntime = task.ntime.wrapping_add(1);
                }
            }

            _ = status_tick.tick() => {
                refresh_status_hashrate(
                    &status,
                    &mut runtime_measurements,
                    config.expected_chain_hashrate_ths(),
                );
                let _ = event_tx.send(HashThreadEvent::StatusUpdate(snapshot_status(&status))).await;
            }

        }
    }

    // Our work stops before anything else is taken down, and before the
    // sensor-off frame, which stays the last thing on the bus.
    if dispatched_since_reset {
        stop_engines(&mut writer, &config).await;
    }

    // Leave the part as a cold boot would: sensor stream off. Otherwise the
    // next process to open this bus finds frames already interleaving on the
    // TDM path, a state no bring-up path expects.
    if dts_vs.enabled {
        match tokio::time::timeout(
            DTS_VS_SHUTDOWN_TIMEOUT,
            suspend_dts_vs_stream(&mut control_writer),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                warn!(path = %config.serial_path, error = %err,
                    "Failed to take BZM2 DTS/VS streaming down at shutdown");
            }
            Err(_) => {
                warn!(path = %config.serial_path,
                    "Timed out taking BZM2 DTS/VS streaming down at shutdown");
            }
        }
    }

    set_active(&status, false, config.expected_chain_hashrate_ths());

    refresh_status_hashrate(
        &status,
        &mut runtime_measurements,
        config.expected_chain_hashrate_ths(),
    );

    let _ = event_tx
        .send(HashThreadEvent::StatusUpdate(snapshot_status(&status)))
        .await;
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::super::protocol::{self, encode_write_register};
    use super::super::test_support::*;
    use super::*;
    use crate::asic::hash_thread::Share;

    use crate::transport::{SerialConfig, SerialStream};
    use bitcoin::pow::Target;
    use nix::pty::openpty;
    use std::os::unix::io::IntoRawFd;
    use tokio::io::AsyncWriteExt;
    use tokio::sync::mpsc as tokio_mpsc;
    #[tokio::test]
    async fn gen2_dts_vs_fault_shuts_down_live_thread() {
        let pty = openpty(None, None).unwrap();
        let thread_side =
            SerialStream::from_fd(pty.master.into_raw_fd(), SerialConfig::default()).unwrap();
        let host_side =
            SerialStream::from_fd(pty.slave.into_raw_fd(), SerialConfig::default()).unwrap();
        let (reader, writer, control) = thread_side.split();
        let (_host_reader, mut host_writer, _host_control) = host_side.split();

        let mut config = Bzm2ThreadConfig::new("/dev/null".into(), 5_000_000, 55.0);
        config.dts_vs_generation = protocol::DtsVsGeneration::Gen2;
        // The chain must contain the device the fault frame comes from, or the
        // address gate discards it before the fault is ever evaluated -- which
        // is the intended behaviour for a frame from a device we do not have.
        config.asic_ids = (0..=3).collect();
        let mut thread = Bzm2Thread::new("BZM2 test".into(), reader, writer, control, config);
        let mut event_rx = thread.take_event_receiver().unwrap();

        let initial = tokio::time::timeout(Duration::from_millis(250), event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(initial, HashThreadEvent::StatusUpdate(_)));

        // The actor now brings DTS/VS streaming up before entering its select
        // loop, and `configure_dts_vs_stream` read-modify-writes the bandgap
        // register — so bring-up performs a real read. This fake host never
        // answers it, so bring-up runs to its bounded timeout and gives up. Wait
        // that out before injecting: a frame written during the window is
        // consumed by the pending read rather than reaching the frame parser.
        // Includes the pre-bring-up drain, which runs first and costs one quiet
        // window before bring-up even starts. Composed from the constants
        // rather than a hand-picked number, so adding another step to attach
        // cannot silently push the injection back inside the read window.
        // Attach gained a step: after the drain it now PROVES the chain is
        // quiet, which costs its own drain plus DTS_VS_QUIET_CONFIRM. This
        // test predicted that -- "adding another step to attach cannot
        // silently push the injection back inside the read window" -- and it
        // is why the constant is added here rather than a number being nudged.
        // Attach's phases on a line that answers nothing, each at its own
        // timeout -- including the engine-state read around the ungate.
        tokio::time::sleep(
            DTS_VS_DRAIN_QUIET
                + DTS_VS_BRING_UP_TIMEOUT
                + DTS_VS_QUIET_CONFIRM
                + UNGATE_READ_TIMEOUT
                + Duration::from_millis(250),
        )
        .await;

        // A sustained fault, not a single frame. One assertion no longer
        // stops a thread: it arms the corroborator, because measurement showed
        // junk on the wire stopping the miner eight times per twenty-five
        // kilobytes and a spurious stop is only safe once. A real fault
        // asserts on every sweep, so this is what one looks like.
        // Payload in WIRE ORDER; this fixture previously ran back-to-front.
        let fault_frame = [
            0x00,
            protocol::OPCODE_UART_DTS_VS,
            0xF7,
            0xA9,
            0x96,
            0x45,
            0x12,
            0x34,
            0xAB,
            0xD5,
        ];
        for _ in 0..FAULT_CORROBORATION_COUNT {
            host_writer.write_all(&fault_frame).await.unwrap();
        }

        let mut saw_fault_status = false;
        let closed = tokio::time::timeout(Duration::from_secs(1), async {
            while let Some(event) = event_rx.recv().await {
                if let HashThreadEvent::StatusUpdate(status) = event {
                    if !status.is_active && status.hardware_errors >= 1 {
                        saw_fault_status = true;
                    }
                }
            }
        })
        .await;

        assert!(
            closed.is_ok(),
            "thread should exit after DTS/VS hardware fault"
        );
        assert!(
            saw_fault_status,
            "thread should publish a final faulted status"
        );
    }

    /// Shutdown must leave the part as a cold boot would. Before this, a
    /// clean exit left sensor frames streaming onto the bus for the next
    /// process to open, and the only "off" value the thread knew was a
    /// capture taken after the stream had already been enabled, which made
    /// every suspend a no-op on a default boot.
    #[tokio::test]
    async fn shutdown_takes_dts_vs_stream_off_the_bus() {
        let pty = openpty(None, None).unwrap();
        // THE BUS OUTLIVES THIS PROCESS'S HANDLE ON IT, so the test must too.
        //
        // Since the transport started closing its descriptor on drop, the
        // thread's shutdown really closes the pty master -- and on Linux that
        // hangs up the slave, and a tty hangup FLUSHES the input queue. The
        // frame written just before the close was discarded before the
        // emulator on the slave could read it, and this test began reporting
        // the enable frame as the last one on the bus.
        //
        // A real chain does not vanish when we close our fd: the ASICs are
        // still there and have already received the frame. Holding a second
        // reference to the master for the length of the capture models that,
        // and is the more faithful harness -- not a weaker one. If the thread
        // failed to write the off-frame, this would still catch it.
        let _bus_outlives_the_handle = rustix::io::dup(&pty.master).unwrap();
        let thread_side =
            SerialStream::from_fd(pty.master.into_raw_fd(), SerialConfig::default()).unwrap();
        let host_side =
            SerialStream::from_fd(pty.slave.into_raw_fd(), SerialConfig::default()).unwrap();
        let (reader, writer, control) = thread_side.split();
        let (mut host_reader, mut host_writer, _host_control) = host_side.split();

        let config = Bzm2ThreadConfig::new("/dev/null".into(), 5_000_000, 55.0);
        let mut thread = Bzm2Thread::new("BZM2 test".into(), reader, writer, control, config);
        let handle = thread.shutdown_handle();
        let mut event_rx = thread.take_event_receiver().unwrap();

        // A minimal part: records every length-prefixed frame the actor sends
        // and answers register reads with zeros, so bring-up's read-modify-write
        // of the bandgap register completes and the stream is really enabled.
        let collector = tokio::spawn(async move {
            let mut frames: Vec<Vec<u8>> = Vec::new();
            loop {
                let mut len = [0u8; 2];
                match tokio::time::timeout(Duration::from_secs(2), host_reader.read_exact(&mut len))
                    .await
                {
                    Ok(Ok(_)) => {}
                    _ => break,
                }
                let total = u16::from_le_bytes(len) as usize;
                let mut rest = vec![0u8; total.saturating_sub(2)];
                if host_reader.read_exact(&mut rest).await.is_err() {
                    break;
                }
                let mut frame = len.to_vec();
                frame.extend_from_slice(&rest);
                if rest.len() >= 4 && (rest[1] >> 4) == protocol::OPCODE_UART_READREG {
                    let reply = [rest[0], protocol::OPCODE_UART_READREG, 0, 0, 0, 0];
                    if host_writer.write_all(&reply).await.is_err() {
                        break;
                    }
                }
                frames.push(frame);
            }
            frames
        });

        // Let bring-up run, then shut down and wait for the event stream to close.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let _ = handle.shutdown();
        let closed = tokio::time::timeout(Duration::from_secs(3), async {
            while event_rx.recv().await.is_some() {}
        })
        .await;
        assert!(closed.is_ok(), "actor did not exit after Shutdown");

        let frames = collector.await.unwrap();
        let on = encode_write_register(
            protocol::BROADCAST_ASIC,
            crate::asic::bzm2::uart::NOTCH_REG,
            0x0a,
            &0x0fu32.to_le_bytes(),
        );
        let off = encode_write_register(
            protocol::BROADCAST_ASIC,
            crate::asic::bzm2::uart::NOTCH_REG,
            0x0a,
            &crate::asic::bzm2::uart::UART_TX_CTRL_RESET.to_le_bytes(),
        );
        assert!(
            frames.iter().any(|f| *f == on),
            "bring-up should have enabled the stream ({} frames seen)",
            frames.len()
        );
        assert_eq!(
            frames.last().map(Vec::as_slice),
            Some(off.as_slice()),
            "the last frame on the bus must take the sensor stream off"
        );
    }

    /// A part for the actor tests, modelling what was measured on hardware: engine
    /// CONFIG reads 0x77 until written 0x04, then 0x14; a reset gates it
    /// again. Once the stream is enabled it sends two die readings captured
    /// on hardware (ASICs 70 and 71), for as long as `live` is set. Every frame
    /// the actor sends comes back on the returned channel.
    fn spawn_config_modelling_part(
        mut host_reader: crate::transport::serial::SerialReader,
        mut host_writer: crate::transport::serial::SerialWriter,
        live: Arc<std::sync::atomic::AtomicBool>,
        wire_bytes_per_s: Option<u64>,
    ) -> tokio_mpsc::UnboundedReceiver<Vec<u8>> {
        let (frames_tx, frames_rx) = tokio_mpsc::unbounded_channel::<Vec<u8>>();
        // THE DRIVER TAKES BYTES FASTER THAN THE WIRE CARRIES THEM. The reader
        // accepts every frame at once, the way the chain port's driver does;
        // the part acts on each only when its bytes would have finished
        // crossing a wire of `wire_bytes_per_s`. Without this a pty drains
        // instantly and no test can see a transmit backlog -- which is how
        // 519 KB of slice writes queued ahead of a 500 ms register read
        // passed every test and failed on hardware.
        let (inbox_tx, mut inbox) = tokio_mpsc::unbounded_channel::<Vec<u8>>();
        tokio::spawn(async move {
            loop {
                let mut len = [0u8; 2];
                if host_reader.read_exact(&mut len).await.is_err() {
                    break;
                }
                let total = u16::from_le_bytes(len) as usize;
                let mut frame = len.to_vec();
                frame.resize(total.max(2), 0);
                if host_reader.read_exact(&mut frame[2..]).await.is_err() {
                    break;
                }
                if inbox_tx.send(frame).is_err() {
                    break;
                }
            }
        });
        tokio::spawn(async move {
            // Two die readings captured from hardware, ASICs 70 and 71.
            let readings: [[u8; 10]; 2] = [
                [
                    70,
                    protocol::OPCODE_UART_DTS_VS,
                    0xc8,
                    0x9b,
                    0x98,
                    0x98,
                    0x62,
                    0x50,
                    0xae,
                    0x82,
                ],
                [
                    71,
                    protocol::OPCODE_UART_DTS_VS,
                    0xc8,
                    0x93,
                    0x98,
                    0x73,
                    0x62,
                    0x49,
                    0xad,
                    0x82,
                ],
            ];
            let mut ungated = std::collections::HashSet::<u16>::new();
            let mut streaming = false;
            let mut tick = tokio::time::interval(Duration::from_millis(40));
            let mut wire_clock = tokio::time::Instant::now();
            loop {
                tokio::select! {
                    got = inbox.recv() => {
                        let Some(frame) = got else { break; };
                        if let Some(rate) = wire_bytes_per_s {
                            // Sleep only once the wire has fallen 2 ms behind:
                            // the timer's resolution is a millisecond, and one
                            // sleep per 11-byte frame ran this part ~40x slow.
                            let now = tokio::time::Instant::now();
                            wire_clock = wire_clock.max(now)
                                + Duration::from_micros(frame.len() as u64 * 1_000_000 / rate);
                            if wire_clock > now + Duration::from_millis(2) {
                                tokio::time::sleep_until(wire_clock).await;
                            }
                        }
                        let len = [frame[0], frame[1]];
                        let rest = frame[2..].to_vec();
                        let opcode = rest.get(1).map(|b| b >> 4);
                        let engine = if rest.len() >= 3 {
                            (u16::from(rest[1] & 0x0f) << 8) | u16::from(rest[2])
                        } else {
                            0
                        };
                        if opcode == Some(protocol::OPCODE_UART_WRITEREG) && rest.len() >= 6 {
                            if engine != crate::asic::bzm2::uart::NOTCH_REG && rest[3] == 0x01 && rest[5] == 0x04 {
                                ungated.insert(engine);
                            }
                            if engine == crate::asic::bzm2::uart::NOTCH_REG && rest[3] == 0x0a && rest[5] == 0x0f {
                                streaming = true;
                            }
                            // The suspend (transmit reset) stops new output;
                            // what was already on its way still arrives. One
                            // batch in flight, then silence -- the shape the
                            // hardware showed.
                            if engine == crate::asic::bzm2::uart::NOTCH_REG
                                && rest[3] == 0x0a
                                && rest[5] == crate::asic::bzm2::uart::UART_TX_CTRL_RESET as u8
                            {
                                if streaming {
                                    for r in &readings {
                                        if host_writer.write_all(r).await.is_err() { return; }
                                    }
                                }
                                streaming = false;
                            }
                            if engine == crate::asic::bzm2::uart::NOTCH_REG && rest[3] == 0x16 && rest[5] == 0x00 {
                                ungated.clear(); // a reset gates everything again
                            }
                        }
                        if opcode == Some(protocol::OPCODE_UART_NOOP) {
                            let sig = crate::asic::bzm2::uart::NOOP_SIGNATURE;
                            let reply = [rest[0], protocol::OPCODE_UART_NOOP, sig[0], sig[1], sig[2]];
                            if host_writer.write_all(&reply).await.is_err() { break; }
                        }
                        if opcode == Some(protocol::OPCODE_UART_READREG) && rest.len() >= 4 {
                            let mut reply = vec![rest[0], protocol::OPCODE_UART_READREG, 0, 0, 0, 0];
                            if engine != crate::asic::bzm2::uart::NOTCH_REG && rest[3] == 0x00 {
                                reply[3] = if ungated.contains(&engine) { 0x14 } else { 0x77 };
                            }
                            if host_writer.write_all(&reply).await.is_err() { break; }
                        }
                        let mut frame = len.to_vec();
                        frame.extend_from_slice(&rest);
                        if frames_tx.send(frame).is_err() { break; }
                    }
                    _ = tick.tick(), if streaming && live.load(std::sync::atomic::Ordering::SeqCst) => {
                        for r in &readings {
                            if host_writer.write_all(r).await.is_err() { return; }
                        }
                    }
                }
            }
        });
        frames_rx
    }

    /// THE SENSOR STREAM COMES UP ON A FULL CHAIN, AT WIRE RATE.
    ///
    /// Measured on hardware: attach wrote each of 100 ASICs'
    /// nonce slices -- about 519 KB -- and then brought the sensor stream up.
    /// The driver takes bytes long before the wire carries them, so the
    /// bring-up's 500 ms register read queued behind ~1.15 s of transmit and
    /// timed out. No stream, no die reading; the interlock refused all work,
    /// and the monitor scrammed a board that was never given any. A pty drains
    /// instantly, so every test passed. This part runs at 5 Mbaud's ~450 KB/s.
    #[tokio::test]
    async fn the_sensor_stream_comes_up_on_a_full_chain_at_wire_rate() {
        let pty = openpty(None, None).unwrap();
        let _bus_outlives_the_handle = rustix::io::dup(&pty.master).unwrap();
        let thread_side =
            SerialStream::from_fd(pty.master.into_raw_fd(), SerialConfig::default()).unwrap();
        let host_side =
            SerialStream::from_fd(pty.slave.into_raw_fd(), SerialConfig::default()).unwrap();
        let (reader, writer, control) = thread_side.split();
        let (host_reader, host_writer, _host_control) = host_side.split();

        let mut config = Bzm2ThreadConfig::new("/dev/null".into(), 5_000_000, 55.0);
        config.asic_ids = (0..100).collect();
        let thread = Bzm2Thread::new("BZM2 test".into(), reader, writer, control, config);
        let handle = thread.shutdown_handle();
        let live = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let mut frames_rx =
            spawn_config_modelling_part(host_reader, host_writer, live, Some(450_000));

        tokio::time::sleep(Duration::from_millis(3500)).await;
        let mut frames = Vec::new();
        while let Ok(f) = frames_rx.try_recv() {
            frames.push(f);
        }
        let _ = handle.shutdown();

        let stream_on = encode_write_register(
            protocol::BROADCAST_ASIC,
            crate::asic::bzm2::uart::NOTCH_REG,
            0x0a,
            &0x0fu32.to_le_bytes(),
        );
        assert!(
            frames.iter().any(|f| *f == stream_on),
            "the sensor stream was never enabled: its bring-up read timed out behind the \
                 attach's own writes ({} frames reached the part)",
            frames.len()
        );
        // And the slices still went out, for the last ASIC too.
        let (row, col) = protocol::default_engine_coordinates()[0];
        let last_slice = encode_write_register(
            99,
            protocol::logical_engine_address(row, col),
            protocol::ENGINE_REG_START_NONCE,
            &nonce_slices(100)[99].0.to_le_bytes(),
        );
        assert!(
            frames.iter().any(|f| *f == last_slice),
            "ASIC 99's nonce slice never reached the part"
        );
    }

    /// Every place that marks a dispatch logs the first one. Source-level,
    /// because the sites are three arms of one actor loop and the one that
    /// was missed was the one that runs first.
    #[test]
    fn every_dispatch_site_logs_the_first_dispatch() {
        let src = include_str!("actor.rs");
        let body = &src[..src.find("#[cfg(test)]").unwrap()];
        let lines: Vec<&str> = body.lines().collect();
        let mut sites = 0;
        for (i, l) in lines.iter().enumerate() {
            if l.trim() == "dispatched_since_reset = true;" {
                sites += 1;
                let before = lines[i.saturating_sub(4)..i].join("\n");
                assert!(
                    before.contains("log_first_dispatch("),
                    "a dispatch site at line {} does not log the first dispatch",
                    i + 1
                );
            }
        }
        assert_eq!(sites, 3, "the dispatch sites this test knows about");
    }

    /// A THERMAL HOLD TAKES THE LOAD OFF, not just the next job.
    ///
    /// A running engine repeats its last job until given another, so a hold
    /// that only deferred dispatch left a chain hashing through the very
    /// refusal meant to cool it. Here the die readings go stale mid-work; the
    /// engines must be reset with nobody calling idle.
    #[tokio::test]
    async fn a_thermal_hold_takes_the_load_off() {
        let pty = openpty(None, None).unwrap();
        let _bus_outlives_the_handle = rustix::io::dup(&pty.master).unwrap();
        let thread_side =
            SerialStream::from_fd(pty.master.into_raw_fd(), SerialConfig::default()).unwrap();
        let host_side =
            SerialStream::from_fd(pty.slave.into_raw_fd(), SerialConfig::default()).unwrap();
        let (reader, writer, control) = thread_side.split();
        let (host_reader, host_writer, _host_control) = host_side.split();

        let mut config = Bzm2ThreadConfig::new("/dev/null".into(), 5_000_000, 55.0);
        config.asic_ids = vec![70, 71];
        config.dispatch_interval = Duration::from_millis(100);
        config.thermal_reading_max_age_s = 1;
        let mut thread = Bzm2Thread::new("BZM2 test".into(), reader, writer, control, config);
        let handle = thread.shutdown_handle();
        let mut event_rx = thread.take_event_receiver().unwrap();
        tokio::spawn(async move { while event_rx.recv().await.is_some() {} });

        let live = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let mut frames_rx =
            spawn_config_modelling_part(host_reader, host_writer, live.clone(), None);

        tokio::time::sleep(Duration::from_millis(1500)).await;
        thread.update_task(test_task()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        live.store(false, std::sync::atomic::Ordering::SeqCst); // the dies go quiet
        tokio::time::sleep(Duration::from_millis(2000)).await;

        // Collected BEFORE shutdown: shutdown resets engines that have run, so
        // a reset seen after it would prove nothing about the hold. The first
        // version of this test read the frames after shutdown and passed with
        // the hold's stop removed.
        let mut frames = Vec::new();
        while let Ok(f) = frames_rx.try_recv() {
            frames.push(f);
        }
        let _ = handle.shutdown();
        let reset_low = encode_write_register(
            protocol::BROADCAST_ASIC,
            crate::asic::bzm2::uart::NOTCH_REG,
            0x16,
            &0u32.to_le_bytes(),
        );
        let is_job = |f: &Vec<u8>| f.len() == 48 && f[3] >> 4 == protocol::OPCODE_UART_WRITEJOB;
        let first_job = frames.iter().position(is_job).expect("work was dispatched");
        let last_job = frames.iter().rposition(is_job).unwrap();
        let reset_after_work = frames[first_job..]
            .iter()
            .position(|f| *f == reset_low)
            .map(|i| i + first_job)
            .expect("the hold never reset the engines: they kept hashing their last job");
        assert!(
            reset_after_work > last_job,
            "work went out after the hold had reset the engines"
        );
    }

    /// ENGINES ARE UNGATED BEFORE WORK, AND STOPPED WHEN WORK STOPS.
    ///
    /// The first test to dispatch real work through this actor. Its part
    /// models the one engine behaviour measured on hardware:
    /// CONFIG reads 0x77 (every TCE gated) until written
    /// 0x04, then 0x14. It streams two die readings captured on hardware so
    /// the thermal interlock lets work through.
    ///
    /// What it pins, in order on the bus:
    ///  1. after the attach reset, CONFIG 0x04 to every active engine, then a
    ///     readback -- measured on hardware, 125 s of work sent to gated engines;
    ///  2. work goes out;
    ///  3. idle pulses the engine reset after the last job -- a running engine
    ///     re-runs its job until given another, so stopping dispatch alone
    ///     leaves the load on;
    ///  4. the next job is preceded by the ungate again;
    ///  5. shutdown pulses the reset after the last job, and the sensor-off
    ///     frame is still the last thing on the bus.
    #[tokio::test]
    async fn engines_are_ungated_before_work_and_stopped_when_it_stops() {
        let pty = openpty(None, None).unwrap();
        let _bus_outlives_the_handle = rustix::io::dup(&pty.master).unwrap();
        let thread_side =
            SerialStream::from_fd(pty.master.into_raw_fd(), SerialConfig::default()).unwrap();
        let host_side =
            SerialStream::from_fd(pty.slave.into_raw_fd(), SerialConfig::default()).unwrap();
        let (reader, writer, control) = thread_side.split();
        let (host_reader, host_writer, _host_control) = host_side.split();

        let mut config = Bzm2ThreadConfig::new("/dev/null".into(), 5_000_000, 55.0);
        config.asic_ids = vec![70, 71];
        config.dispatch_interval = Duration::from_millis(100);
        let mut thread = Bzm2Thread::new("BZM2 test".into(), reader, writer, control, config);
        let handle = thread.shutdown_handle();
        let mut event_rx = thread.take_event_receiver().unwrap();
        tokio::spawn(async move { while event_rx.recv().await.is_some() {} });

        let live = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let mut frames_rx =
            spawn_config_modelling_part(host_reader, host_writer, live.clone(), None);

        let reset_low = encode_write_register(
            protocol::BROADCAST_ASIC,
            crate::asic::bzm2::uart::NOTCH_REG,
            0x16,
            &0u32.to_le_bytes(),
        );
        let reset_high = encode_write_register(
            protocol::BROADCAST_ASIC,
            crate::asic::bzm2::uart::NOTCH_REG,
            0x16,
            &1u32.to_le_bytes(),
        );
        let off = encode_write_register(
            protocol::BROADCAST_ASIC,
            crate::asic::bzm2::uart::NOTCH_REG,
            0x0a,
            &crate::asic::bzm2::uart::UART_TX_CTRL_RESET.to_le_bytes(),
        );
        let ungates: Vec<Vec<u8>> = protocol::default_engine_coordinates()
            .into_iter()
            .map(|(row, col)| {
                encode_write_register(
                    protocol::BROADCAST_ASIC,
                    protocol::logical_engine_address(row, col),
                    0x01,
                    &[0x04],
                )
            })
            .collect();
        let is_job = |f: &Vec<u8>| f.len() == 48 && f[3] >> 4 == protocol::OPCODE_UART_WRITEJOB;

        // Attach, then one job, then idle, then a second job, then shutdown.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        thread.update_task(test_task()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        thread.go_idle().await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        thread.update_task(test_task()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        let _ = handle.shutdown();
        tokio::time::sleep(Duration::from_millis(500)).await;

        let mut frames = Vec::new();
        while let Ok(f) = frames_rx.try_recv() {
            frames.push(f);
        }
        let pos = |want: &[u8], from: usize| {
            frames[from..]
                .iter()
                .position(|f| f.as_slice() == want)
                .map(|i| i + from)
        };

        // 1. The attach reset, then every engine ungated, then a readback.
        let attach_released = pos(&reset_high, 0).expect("attach released the engine reset");
        for u in &ungates {
            assert!(
                pos(u, attach_released).is_some(),
                "an active engine was never ungated after the attach reset"
            );
        }
        let first_job = frames
            .iter()
            .position(is_job)
            .expect("no work was ever dispatched");
        let last_ungate = ungates
            .iter()
            .filter_map(|u| pos(u, attach_released))
            .max()
            .unwrap();
        assert!(
            last_ungate < first_job,
            "work went out before the engines were ungated"
        );
        assert!(
            frames[last_ungate..first_job].iter().any(|f| f.len() >= 4
                && f[3] >> 4 == protocol::OPCODE_UART_READREG
                && f[5] == 0x00),
            "nothing read back engine state between the ungate and the first job"
        );

        // 3. Idle: a reset pulse after the first burst of jobs.
        let idle_low = pos(&reset_low, first_job).expect("idle never reset the engines");
        assert_eq!(
            pos(&reset_high, idle_low).map(|i| i > idle_low),
            Some(true),
            "idle pulsed the reset low and never released it"
        );
        // 4. The next job is preceded by the ungate again.
        let second_job = frames[idle_low..]
            .iter()
            .position(is_job)
            .map(|i| i + idle_low)
            .expect("the second task was never dispatched");
        assert!(
            ungates
                .iter()
                .all(|u| frames[idle_low..second_job].iter().any(|f| f == u)),
            "work went to engines the idle reset had gated, without ungating them"
        );
        // Each ASIC's nonce slice, unicast, before the first job and again
        // after the idle reset (which clears it) before the next.
        let slices = nonce_slices(2);
        let (row0, col0) = protocol::default_engine_coordinates()[0];
        for (&asic, &(start, _)) in [70u8, 71].iter().zip(slices.iter()) {
            let w = encode_write_register(
                asic,
                protocol::logical_engine_address(row0, col0),
                protocol::ENGINE_REG_START_NONCE,
                &start.to_le_bytes(),
            );
            assert!(
                frames[attach_released..first_job].iter().any(|f| *f == w),
                "ASIC {asic}'s nonce slice was not set before the first job"
            );
            assert!(
                frames[idle_low..second_job].iter().any(|f| *f == w),
                "the idle reset cleared ASIC {asic}'s slice and it was not set again"
            );
        }
        // 5. Shutdown: reset after the last job, sensor-off still last.
        let last_job = frames.iter().rposition(is_job).unwrap();
        let shutdown_low = pos(&reset_low, last_job).expect("shutdown never reset the engines");
        assert!(shutdown_low > last_job);
        assert_eq!(
            frames.last().map(Vec::as_slice),
            Some(off.as_slice()),
            "the sensor-off frame must stay the last thing on the bus"
        );
    }

    /// AN UPDATE KEEPS EARLIER WORK SUBMITTABLE; A REPLACE OR AN IDLE
    /// INVALIDATES IT.
    ///
    /// Stratum's `clean_jobs` is the pool saying which jobs it will still
    /// take. The scheduler sends UpdateTask for `clean_jobs=false` (earlier
    /// jobs stay valid) and ReplaceTask for `clean_jobs=true` (they do not).
    /// So a hit for an earlier task, read after an UPDATE, is a share on that
    /// task's own channel -- an earlier version lost exactly these as stale_sequence -- and
    /// a hit for anything dispatched before a REPLACE is a share on nobody's
    /// channel, not even the new task's. GoIdle stops the chain and drops
    /// its tasks, so nothing sent before it is forwarded after it either,
    /// not even the task that was live until then.
    ///
    /// Driven through the actor's real command paths, with results injected
    /// on the bus. The injection for the live task, just before the idle, is
    /// the null check: that hit must arrive, or the silences around it prove
    /// nothing.
    #[tokio::test]
    async fn an_update_keeps_earlier_work_submittable_and_a_replace_or_idle_invalidates_it() {
        let pty = openpty(None, None).unwrap();
        let _bus_outlives_the_handle = rustix::io::dup(&pty.master).unwrap();
        let inject_fd = rustix::io::dup(&pty.slave).unwrap();
        let thread_side =
            SerialStream::from_fd(pty.master.into_raw_fd(), SerialConfig::default()).unwrap();
        let host_side =
            SerialStream::from_fd(pty.slave.into_raw_fd(), SerialConfig::default()).unwrap();
        let inject_side =
            SerialStream::from_fd(inject_fd.into_raw_fd(), SerialConfig::default()).unwrap();
        let (reader, writer, control) = thread_side.split();
        let (host_reader, host_writer, _host_control) = host_side.split();
        let (_inject_reader, mut inject, _inject_control) = inject_side.split();

        let mut config = Bzm2ThreadConfig::new("/dev/null".into(), 5_000_000, 55.0);
        config.asic_ids = vec![70, 71];
        // Only the commands dispatch here (bar the interval's immediate first
        // tick); each job's sequence byte is read back off the bus anyway.
        config.dispatch_interval = Duration::from_secs(3600);
        let mut thread = Bzm2Thread::new("BZM2 test".into(), reader, writer, control, config);
        let handle = thread.shutdown_handle();
        let mut event_rx = thread.take_event_receiver().unwrap();
        tokio::spawn(async move { while event_rx.recv().await.is_some() {} });
        let live = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let mut frames_rx =
            spawn_config_modelling_part(host_reader, host_writer, live.clone(), None);

        // Every hash meets this, so WHICH channel a share reaches is the
        // whole observation. Tasks are told apart on the bus by ntime.
        let task = |ntime: u32| {
            let mut t = test_task();
            t.share_target = Target::from_be_bytes([0xff; 32]);
            t.ntime = ntime;
            let (tx, rx) = tokio_mpsc::channel(8);
            t.share_tx = tx;
            (t, rx)
        };
        let (ntime_a, ntime_b, ntime_c) = (1_700_000_000u32, 1_700_001_000, 1_700_002_000);
        let (a, mut a_rx) = task(ntime_a);
        let (b, mut b_rx) = task(ntime_b);
        let (c, mut c_rx) = task(ntime_c);

        let mut frames: Vec<Vec<u8>> = Vec::new();
        // The sequence byte of the last micro-job-0 job the bus has seen for
        // the task whose ntime starts at `base`, waiting for it to arrive.
        async fn last_sequence_for(
            frames: &mut Vec<Vec<u8>>,
            frames_rx: &mut tokio_mpsc::UnboundedReceiver<Vec<u8>>,
            base: u32,
        ) -> u8 {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            loop {
                while let Ok(f) = frames_rx.try_recv() {
                    frames.push(f);
                }
                let found = frames.iter().rev().find(|f| {
                    f.len() == 48
                        && f[3] >> 4 == protocol::OPCODE_UART_WRITEJOB
                        && f[46] & 0x3 == 0
                        && (base..base + 100)
                            .contains(&u32::from_be_bytes([f[42], f[43], f[44], f[45]]))
                });
                if let Some(f) = found {
                    return f[46];
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the task was never dispatched"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        let (row, col) = protocol::default_engine_coordinates()[0];
        let hit = |sequence_id: u8| {
            let header = (0x8u16 << 12) | protocol::logical_engine_address(row, col);
            let mut raw = vec![70, protocol::OPCODE_UART_READRESULT];
            raw.extend_from_slice(&header.to_be_bytes());
            raw.extend_from_slice(&0x1234_5678u32.to_le_bytes());
            raw.push(sequence_id);
            raw.push(DEFAULT_TIMESTAMP_COUNT);
            raw
        };
        let arrives = |rx: &mut tokio_mpsc::Receiver<Share>| {
            let got = rx.try_recv().is_ok();
            while rx.try_recv().is_ok() {}
            got
        };
        let settle = || tokio::time::sleep(Duration::from_millis(300));

        tokio::time::sleep(Duration::from_millis(1500)).await; // attach
        thread.update_task(a).await.unwrap();
        settle().await; // the interval's first tick dispatches A again
        let seq_a = last_sequence_for(&mut frames, &mut frames_rx, ntime_a).await;

        // clean_jobs=false: B dispatched, A still live at the pool.
        thread.update_task(b).await.unwrap();
        let seq_b = last_sequence_for(&mut frames, &mut frames_rx, ntime_b).await;
        inject.write_all(&hit(seq_a)).await.unwrap();
        settle().await;
        let a_after_update = arrives(&mut a_rx);
        let b_after_update = arrives(&mut b_rx);

        // clean_jobs=true: C replaces both.
        thread.replace_task(c).await.unwrap();
        let seq_c = last_sequence_for(&mut frames, &mut frames_rx, ntime_c).await;
        inject.write_all(&hit(seq_a)).await.unwrap();
        inject.write_all(&hit(seq_b)).await.unwrap();
        settle().await;
        let after_replace = [arrives(&mut a_rx), arrives(&mut b_rx), arrives(&mut c_rx)];

        // The null check.
        inject.write_all(&hit(seq_c)).await.unwrap();
        settle().await;
        let c_live = arrives(&mut c_rx);

        // Idle: the same hit for C, read after the chain was told to stop.
        thread.go_idle().await.unwrap();
        inject.write_all(&hit(seq_c)).await.unwrap();
        settle().await;
        let c_after_idle = arrives(&mut c_rx);
        let _ = handle.shutdown();

        assert!(
            c_live,
            "a hit for the live task never arrived: the injection proves nothing"
        );
        assert!(
            !c_after_idle,
            "after GoIdle, a hit for C still reached C: work dispatched before the idle was forwarded"
        );
        assert!(
            a_after_update,
            "a hit for A read after an UPDATE to B was lost; A is still live at the pool"
        );
        assert!(!b_after_update, "A's hit was credited to B");
        assert_eq!(
            after_replace,
            [false, false, false],
            "after a REPLACE, hits for A and B reached [A, B, C]: invalidated work was forwarded"
        );
    }

    /// A TASK THE INTERLOCK WILL NOT SEND YET IS ACCEPTED, NOT FAILED.
    ///
    /// The first test of task assignment through this actor at all -- before
    /// it, nothing in the tree sent a BZM2 thread a job.
    ///
    /// The case it pins is the first dispatch. The first job can easily arrive
    /// before the sensor stream has delivered a reading, and the interlock
    /// refuses on NoReading. That refusal used to be answered with Err while
    /// the thread KEPT the task: the scheduler, told the assignment failed,
    /// never registered the task's share receiver -- the task carries
    /// `share_tx` -- and the dispatch tick then sent the task once readings
    /// arrived. Every share it found went to a dropped channel. A first mining
    /// run that shows hashrate and delivers nothing to the pool.
    ///
    /// The emulator here answers the attach sequence and never streams a
    /// sensor frame, so the interlock has no reading for the whole test.
    #[tokio::test]
    async fn a_task_the_interlock_refuses_is_accepted_and_nothing_is_sent() {
        use crate::asic::hash_thread::HashThread;
        use crate::job_source::{GeneralPurposeBits, JobTemplate, MerkleRootKind, VersionTemplate};
        use bitcoin::hashes::Hash;

        let pty = openpty(None, None).unwrap();
        let _bus_outlives_the_handle = rustix::io::dup(&pty.master).unwrap();
        let thread_side =
            SerialStream::from_fd(pty.master.into_raw_fd(), SerialConfig::default()).unwrap();
        let host_side =
            SerialStream::from_fd(pty.slave.into_raw_fd(), SerialConfig::default()).unwrap();
        let (reader, writer, control) = thread_side.split();
        let (mut host_reader, mut host_writer, _host_control) = host_side.split();

        let config = Bzm2ThreadConfig::new("/dev/null".into(), 5_000_000, 55.0);
        let mut thread = Bzm2Thread::new("BZM2 test".into(), reader, writer, control, config);
        let handle = thread.shutdown_handle();
        let mut event_rx = thread.take_event_receiver().unwrap();

        let collector = tokio::spawn(async move {
            let mut frames: Vec<Vec<u8>> = Vec::new();
            loop {
                let mut len = [0u8; 2];
                match tokio::time::timeout(Duration::from_secs(2), host_reader.read_exact(&mut len))
                    .await
                {
                    Ok(Ok(_)) => {}
                    _ => break,
                }
                let total = u16::from_le_bytes(len) as usize;
                let mut rest = vec![0u8; total.saturating_sub(2)];
                if host_reader.read_exact(&mut rest).await.is_err() {
                    break;
                }
                let mut frame = len.to_vec();
                frame.extend_from_slice(&rest);
                if rest.len() >= 4 && (rest[1] >> 4) == protocol::OPCODE_UART_READREG {
                    let reply = [rest[0], protocol::OPCODE_UART_READREG, 0, 0, 0, 0];
                    if host_writer.write_all(&reply).await.is_err() {
                        break;
                    }
                }
                frames.push(frame);
            }
            frames
        });

        tokio::time::sleep(Duration::from_millis(300)).await;

        let target = bitcoin::pow::Target::MAX;
        let template = std::sync::Arc::new(JobTemplate {
            id: "deferred".into(),
            prev_blockhash: bitcoin::BlockHash::all_zeros(),
            version: VersionTemplate::new(
                bitcoin::block::Version::from_consensus(0x20000000),
                GeneralPurposeBits::none(),
            )
            .unwrap(),
            bits: bitcoin::pow::CompactTarget::from_consensus(0x1d00ffff),
            share_target: target,
            time: 1_234_567_890,
            merkle_root: MerkleRootKind::Fixed(bitcoin::TxMerkleNode::all_zeros()),
        });
        let (share_tx, _share_rx) = tokio::sync::mpsc::channel(8);
        let task = HashTask {
            template,
            en2_range: None,
            en2: None,
            share_target: target,
            ntime: 1_234_567_890,
            share_tx,
        };

        let answer = thread.update_task(task).await;
        assert!(
            answer.is_ok(),
            "a thermal refusal must be ACCEPTED and deferred: the scheduler registers the \
                 task's share receiver only on Ok, so an Err here orphans every share the task \
                 later finds. Got {answer:?}"
        );

        // Let a few dispatch ticks pass: still no reading, so still nothing sent.
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        let _ = handle.shutdown();
        let _ = tokio::time::timeout(Duration::from_secs(3), async {
            while event_rx.recv().await.is_some() {}
        })
        .await;
        let frames = collector.await.unwrap();
        let jobs = frames
            .iter()
            .filter(|f| f.len() >= 4 && (f[3] >> 4) == protocol::OPCODE_UART_WRITEJOB)
            .count();
        assert_eq!(
            jobs, 0,
            "deferred means NOTHING dispatched while the interlock has no reading"
        );
    }
}
