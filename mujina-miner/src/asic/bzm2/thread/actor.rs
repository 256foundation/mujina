use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;

use crate::asic::hash_thread::{HashThreadEvent, HashThreadStatus, TelemetryCoalescer};
use crate::transport::serial::{SerialControl, SerialReader, SerialWriter};
use crate::types::HashRate;

use super::super::protocol::{BROADCAST_ASIC, TdmFrame, TdmFrameParser};
use super::super::uart::{
    Bzm2DtsVsConfig, configure_dts_vs_stream, confirm_on_die_protection, confirm_stream_stopped,
    disable_dts_vs_sensors, soft_reset_engines, suspend_dts_vs_stream,
};
use super::interlock::*;
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

    let _ = event_tx
        .send(HashThreadEvent::StatusUpdate(snapshot_status(&status)))
        .await;

    let mut parser = TdmFrameParser::new(config.dts_vs_generation);

    // One interlock per chain, owned by the actor that drives it. It starts
    // refusing: no reading has been observed yet, and unknown reads as hot.

    let mut corroborator = FaultCorroborator::default();
    let mut dts_vs_diagnostics = DtsVsDiagnostics::default();
    let mut telemetry_coalescer = TelemetryCoalescer::new(TELEMETRY_PUBLISH_INTERVAL);

    let mut interlock = ThermalInterlock::new(
        config.thermal_ceiling_c,
        Duration::from_secs(config.thermal_reading_max_age_s.into()),
    )
    .expecting(&config.asic_ids);

    let mut read_buf = [0u8; 4096];
    let mut dts_vs = DtsVsStream::default();
    // A second handle on the same device, used only to suspend/resume the sensor
    // stream around a diagnostic. Separate from `writer` so the borrow checker
    // permits the diagnostic closure to hold `writer` at the same time; both are
    // `Arc`-backed views of one port and are never written concurrently.
    let mut control_writer = writer.clone();

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
                                TdmFrame::Result(_) => {}
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

        }
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

    let _ = event_tx
        .send(HashThreadEvent::StatusUpdate(snapshot_status(&status)))
        .await;
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::super::protocol::{self, encode_write_register};
    use super::*;

    use crate::transport::{SerialConfig, SerialStream};

    use nix::pty::openpty;
    use std::os::unix::io::IntoRawFd;
    use tokio::io::AsyncWriteExt;

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
}
