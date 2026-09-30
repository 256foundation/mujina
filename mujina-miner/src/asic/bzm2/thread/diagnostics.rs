use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::asic::hash_thread::{
    HashThreadError, HashThreadEvent, HashThreadStatus, HashThreadTelemetryUpdate,
    TelemetryCoalescer,
};
use crate::transport::serial::{SerialReader, SerialWriter};

use super::super::clock::{
    Bzm2ClockDebugReport, Bzm2Dll, Bzm2DllStatus, Bzm2Pll, Bzm2PllStatus, fincon_is_valid,
};
use super::super::protocol::{
    Bzm2EngineLayout, MIN_REGISTER_TRANSFER_BYTES, OPCODE_UART_LOOPBACK, OPCODE_UART_NOOP,
    OPCODE_UART_READREG, TdmDtsVsFrame, TdmFrame, TdmFrameParser, encode_loopback, encode_noop,
    encode_read_register, encode_write_register,
};
use super::super::uart::{
    Bzm2DtsVsConfig, DEFAULT_DTS_VS_QUERY_TIMEOUT, configure_dts_vs_stream, resume_dts_vs_stream,
    suspend_dts_vs_stream,
};
use super::dispatch::*;
use super::interlock::*;
use super::metrics::*;
use super::results::*;
use super::telemetry::*;
use super::*;

/// Bound for a single diagnostic UART read. The actor is one task, so a silent
/// or short-answering chip must not wedge it (including a pending `Shutdown`) on
/// an unbounded `read_exact`; on expiry the diagnostic fails instead of hanging.
const DIAGNOSTIC_READ_TIMEOUT: Duration = Duration::from_secs(2);

/// Run a synchronous UART diagnostic with the DTS/VS sensor stream suspended.
///
/// Suspend/resume lives *inside* this helper rather than at the call sites, and
/// every diagnostic goes through it, so the resume cannot be forgotten. A guard
/// type would be the usual way to express that, but Rust has no async `Drop` and
/// the resume is a register write that must be awaited — so scoping it here is
/// the closest structural equivalent.
///
/// `control_writer` is a separate handle on the same port, present purely so
/// the borrow checker permits `operation` to hold the primary reader/writer at
/// the same time. It is an `Arc`-backed view of one device and is never used
/// concurrently with them: suspend completes before `operation` starts and
/// resume runs after it ends.
pub(super) async fn run_idle_uart_diagnostic<T>(
    thread_active: bool,
    dts_vs: &mut DtsVsStream,
    stream: DiagnosticStream<'_>,
    control_writer: &mut SerialWriter,
    operation: impl AsyncFnOnce(&mut SerialReader) -> Result<T, HashThreadError>,
) -> Result<T, HashThreadError> {
    if thread_active {
        return Err(HashThreadError::DiagnosticsFailed(
            "BZM2 UART diagnostics require the thread to be idle".into(),
        ));
    }
    let DiagnosticStream {
        reader,
        parser,
        corroborator,
    } = stream;

    let suspended = if dts_vs.enabled {
        suspend_dts_vs_stream(control_writer).await.map_err(|err| {
            HashThreadError::DiagnosticsFailed(format!(
                "failed to suspend BZM2 DTS/VS streaming for diagnostics: {err}"
            ))
        })?;
        dts_vs.enabled = false;
        true
    } else {
        false
    };

    // THE SUSPEND STOPS NEW OUTPUT, NOT WHAT IS ALREADY ON ITS WAY. Frames
    // already in the chain and the host's receive buffer still arrive, and a
    // diagnostic that reads straight away reads them as its reply: measured on
    // hardware, a noop got `asic 0x40 opcode 0x99`, stream bytes, and the
    // misaligned remainder stopped the chain. So the line must go
    // quiet first, and a line that will not go quiet is not one to run a
    // diagnostic on.
    let result = match drain_until_quiet(reader, DIAGNOSTIC_QUIET, DIAGNOSTIC_DRAIN_BOUND).await {
        Ok(drained) => {
            if drained > 0 {
                debug!(
                    drained,
                    "Drained in-flight sensor bytes before a diagnostic"
                );
            }
            // Capture the outcome instead of using `?`, so a failing diagnostic
            // still reaches the resume below and cannot leave the board without
            // telemetry.
            operation(reader).await
        }
        Err(drained) => Err(HashThreadError::DiagnosticsFailed(format!(
            "the line did not go quiet within {} ms of suspending the sensor stream, or a \
             read failed ({drained} bytes drained); refusing to run a diagnostic on a shared line",
            DIAGNOSTIC_DRAIN_BOUND.as_millis()
        ))),
    };

    if suspended {
        match resume_dts_vs_stream(control_writer).await {
            Ok(()) => dts_vs.enabled = true,
            Err(err) => {
                // Leave `enabled` false: it reflects reality, and the lazy path
                // in `query_dts_vs_telemetry` will re-enable on the next query.
                warn!(error = %err, "Failed to resume BZM2 DTS/VS streaming after diagnostics");
            }
        }
    }
    after_direct_read(parser, corroborator);

    result
}

/// What a diagnostic needs from the actor's view of the stream: the reader it
/// borrows, and the two pieces of state its reads invalidate.
pub(super) struct DiagnosticStream<'a> {
    pub(super) reader: &'a mut SerialReader,
    pub(super) parser: &'a mut TdmFrameParser,
    pub(super) corroborator: &'a mut FaultCorroborator,
}

/// How long the line must be silent to count as quiet. The stream's frames
/// on a full chain arrive about 1.5 ms apart (measured on hardware: 246,523 frames in
/// 373 s), so 50 ms without a byte is not a gap between frames.
const DIAGNOSTIC_QUIET: Duration = Duration::from_millis(50);
/// How long to wait for quiet at all before refusing the diagnostic.
const DIAGNOSTIC_DRAIN_BOUND: Duration = Duration::from_millis(500);

/// Read and discard until nothing has arrived for `quiet`. `Ok(bytes)` once
/// quiet; `Err(bytes)` if the line was still talking at `bound`, or if a read
/// failed -- an error is not silence, and the main loop treats the same error
/// as a hardware fault.
async fn drain_until_quiet(
    reader: &mut SerialReader,
    quiet: Duration,
    bound: Duration,
) -> Result<usize, usize> {
    let deadline = tokio::time::Instant::now() + bound;
    let mut drained = 0usize;
    let mut buf = [0u8; 512];
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(drained);
        }
        match tokio::time::timeout(quiet, reader.read(&mut buf)).await {
            Err(_) => return Ok(drained),
            Ok(Ok(0)) => return Err(drained),
            Ok(Ok(n)) => drained += n,
            Ok(Err(_)) => return Err(drained),
        }
    }
}

/// Something other than the actor's read loop has read the line: the parser's
/// partial frame and every pending fault sighting are from a stream that no
/// longer continues where they left it.
pub(super) fn after_direct_read(parser: &mut TdmFrameParser, corroborator: &mut FaultCorroborator) {
    let dropped = parser.discard_partial();
    if dropped > 0 {
        debug!(
            dropped,
            "Discarded a partial frame after a direct read of the line"
        );
    }
    corroborator.clear();
}

pub(super) async fn write_register(
    writer: &mut SerialWriter,
    asic: u8,
    engine_address: u16,
    offset: u8,
    value: &[u8],
) -> Result<(), HashThreadError> {
    writer
        .write_all(&encode_write_register(asic, engine_address, offset, value))
        .await
        .map_err(|err| HashThreadError::DiagnosticsFailed(err.to_string()))?;
    writer
        .flush()
        .await
        .map_err(|err| HashThreadError::DiagnosticsFailed(err.to_string()))?;
    Ok(())
}

async fn read_local_reg_u8(
    reader: &mut SerialReader,
    writer: &mut SerialWriter,
    asic: u8,
    offset: u8,
) -> Result<u8, HashThreadError> {
    let value = read_register(
        reader,
        writer,
        asic,
        crate::asic::bzm2::uart::NOTCH_REG,
        offset,
        1,
    )
    .await?;
    value
        .first()
        .copied()
        .ok_or_else(|| HashThreadError::DiagnosticsFailed("short local register response".into()))
}

async fn read_local_reg_u32(
    reader: &mut SerialReader,
    writer: &mut SerialWriter,
    asic: u8,
    offset: u8,
) -> Result<u32, HashThreadError> {
    let value = read_register(
        reader,
        writer,
        asic,
        crate::asic::bzm2::uart::NOTCH_REG,
        offset,
        4,
    )
    .await?;
    let bytes: [u8; 4] = value
        .as_slice()
        .try_into()
        .map_err(|_| HashThreadError::DiagnosticsFailed("short local register response".into()))?;
    Ok(u32::from_le_bytes(bytes))
}

pub(super) async fn read_register(
    reader: &mut SerialReader,
    writer: &mut SerialWriter,
    asic: u8,
    engine_address: u16,
    offset: u8,
    count: u8,
) -> Result<Vec<u8>, HashThreadError> {
    // `count` is a u8, already within the wire's 1-256 range at its upper
    // end, but a count of 0 has no representation -- the zero-based field
    // would read it back as 1 -- so only the lower bound needs a guard here.
    if (count as usize) < MIN_REGISTER_TRANSFER_BYTES {
        return Err(HashThreadError::DiagnosticsFailed(
            "register read refused: a count of 0 has no representation on the wire \
             (the byte-count field is zero-based, so 0 reads back as 1 byte)"
                .to_string(),
        ));
    }
    let request = encode_read_register(asic, engine_address, offset, count);
    writer
        .write_all(&request)
        .await
        .map_err(|err| HashThreadError::DiagnosticsFailed(err.to_string()))?;
    writer
        .flush()
        .await
        .map_err(|err| HashThreadError::DiagnosticsFailed(err.to_string()))?;

    let expected = count as usize + 2;
    let mut response = vec![0u8; expected];
    read_exact_diagnostic(reader, &mut response).await?;
    validate_response_header(asic, OPCODE_UART_READREG, &response)?;
    Ok(response[2..].to_vec())
}

async fn read_pll_status(
    reader: &mut SerialReader,
    writer: &mut SerialWriter,
    asic: u8,
    pll: Bzm2Pll,
) -> Result<Bzm2PllStatus, HashThreadError> {
    let (_, _, enable_reg, misc_reg) = pll.register_block();
    let enable = read_local_reg_u32(reader, writer, asic, enable_reg).await?;
    let misc = read_local_reg_u32(reader, writer, asic, misc_reg).await?;
    Ok(Bzm2PllStatus {
        pll,
        enable_register: enable,
        misc_register: misc,
        enabled: (enable & 0x1) != 0,
        locked: (enable & 0x4) != 0,
    })
}

async fn read_dll_status(
    reader: &mut SerialReader,
    writer: &mut SerialWriter,
    asic: u8,
    dll: Bzm2Dll,
) -> Result<Bzm2DllStatus, HashThreadError> {
    let (control2_reg, _, _, control5_reg, coarse_reg) = dll.registers();
    let control2 = read_local_reg_u8(reader, writer, asic, control2_reg).await?;
    let control5 = read_local_reg_u8(reader, writer, asic, control5_reg).await?;
    let coarse_raw = read_local_reg_u8(reader, writer, asic, coarse_reg).await?;
    let fincon = read_local_reg_u8(reader, writer, asic, dll.fincon_register()).await?;

    Ok(Bzm2DllStatus {
        dll,
        control2,
        control5,
        coarsecon: (coarse_raw >> 5) & 0x7,
        fincon,
        freeze_valid: (control2 & 0x2) != 0,
        locked: (control5 & 0x2) != 0,
        fincon_valid: fincon_is_valid(fincon),
    })
}

pub(super) async fn query_clock_report(
    reader: &mut SerialReader,
    writer: &mut SerialWriter,
    asic: u8,
) -> Result<Bzm2ClockDebugReport, HashThreadError> {
    Ok(Bzm2ClockDebugReport {
        asic,
        pll0: read_pll_status(reader, writer, asic, Bzm2Pll::Pll0).await?,
        pll1: read_pll_status(reader, writer, asic, Bzm2Pll::Pll1).await?,
        dll0: read_dll_status(reader, writer, asic, Bzm2Dll::Dll0).await?,
        dll1: read_dll_status(reader, writer, asic, Bzm2Dll::Dll1).await?,
    })
}

pub(super) async fn query_noop(
    reader: &mut SerialReader,
    writer: &mut SerialWriter,
    asic: u8,
) -> Result<[u8; 3], HashThreadError> {
    let request = encode_noop(asic);
    writer
        .write_all(&request)
        .await
        .map_err(|err| HashThreadError::DiagnosticsFailed(err.to_string()))?;
    writer
        .flush()
        .await
        .map_err(|err| HashThreadError::DiagnosticsFailed(err.to_string()))?;

    let mut response = [0u8; 5];
    read_exact_diagnostic(reader, &mut response).await?;
    validate_response_header(asic, OPCODE_UART_NOOP, &response)?;
    Ok(response[2..5].try_into().unwrap())
}

pub(super) async fn query_loopback(
    reader: &mut SerialReader,
    writer: &mut SerialWriter,
    asic: u8,
    payload: &[u8],
) -> Result<Vec<u8>, HashThreadError> {
    let request = encode_loopback(asic, payload);
    writer
        .write_all(&request)
        .await
        .map_err(|err| HashThreadError::DiagnosticsFailed(err.to_string()))?;
    writer
        .flush()
        .await
        .map_err(|err| HashThreadError::DiagnosticsFailed(err.to_string()))?;

    let expected = payload.len() + 2;
    let mut response = vec![0u8; expected];
    read_exact_diagnostic(reader, &mut response).await?;
    validate_response_header(asic, OPCODE_UART_LOOPBACK, &response)?;
    Ok(response[2..].to_vec())
}

async fn read_exact_diagnostic(
    reader: &mut SerialReader,
    buf: &mut [u8],
) -> Result<(), HashThreadError> {
    tokio::time::timeout(DIAGNOSTIC_READ_TIMEOUT, reader.read_exact(buf))
        .await
        .map_err(|_| {
            HashThreadError::DiagnosticsFailed(format!(
                "timed out after {} ms waiting for UART response",
                DIAGNOSTIC_READ_TIMEOUT.as_millis()
            ))
        })?
        .map_err(|err| HashThreadError::DiagnosticsFailed(err.to_string()))?;
    Ok(())
}

fn validate_response_header(
    expected_asic: u8,
    expected_opcode: u8,
    response: &[u8],
) -> Result<(), HashThreadError> {
    if response.len() < 2 {
        return Err(HashThreadError::DiagnosticsFailed(format!(
            "short UART response: expected at least 2 bytes, got {}",
            response.len()
        )));
    }
    let actual_asic = response[0];
    let actual_opcode = response[1];
    if actual_asic != expected_asic || actual_opcode != expected_opcode {
        return Err(HashThreadError::DiagnosticsFailed(format!(
            "unexpected UART response header: expected asic {expected_asic:#x} opcode {expected_opcode:#x}, got asic {actual_asic:#x} opcode {actual_opcode:#x}"
        )));
    }
    Ok(())
}

// Takes the actor's working state piecemeal; bundling it into a struct would
// just relocate the argument list without simplifying the call site.
#[allow(clippy::too_many_arguments)]
pub(super) async fn query_dts_vs_telemetry(
    asic: u8,
    reader: &mut SerialReader,
    writer: &mut SerialWriter,
    parser: &mut TdmFrameParser,
    engine_dispatches: &DispatchRing,
    engine_layout: &Bzm2EngineLayout,
    config: &Bzm2ThreadConfig,
    status: &Arc<RwLock<HashThreadStatus>>,
    event_tx: &mpsc::Sender<HashThreadEvent>,
    runtime_measurements: &mut ThreadRuntimeMeasurementState,
    dts_vs: &mut DtsVsStream,
) -> Result<HashThreadTelemetryUpdate, HashThreadError> {
    // Normally already enabled at bring-up. This still re-enables lazily for the
    // two cases that leave it down: bring-up failed and fell back to lazy, or a
    // diagnostic suspended it and the resume did not take.
    if !dts_vs.enabled {
        configure_dts_vs_stream(writer, reader, &Bzm2DtsVsConfig::default())
            .await
            .map_err(|err| HashThreadError::TelemetryQueryFailed(err.to_string()))?;
        dts_vs.enabled = true;
    }

    let deadline = tokio::time::Instant::now() + DEFAULT_DTS_VS_QUERY_TIMEOUT;
    let mut read_buf = [0u8; 512];
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(HashThreadError::TelemetryQueryFailed(format!(
                "timed out waiting for DTS/VS frame from ASIC {asic:#x}"
            )));
        }
        let remaining = deadline.saturating_duration_since(now);
        let read = tokio::time::timeout(remaining, reader.read(&mut read_buf))
            .await
            .map_err(|_| {
                HashThreadError::TelemetryQueryFailed(format!(
                    "timed out waiting for DTS/VS frame from ASIC {asic:#x}"
                ))
            })
            .and_then(|result| {
                result.map_err(|err| HashThreadError::TelemetryQueryFailed(err.to_string()))
            })?;
        if read == 0 {
            return Err(HashThreadError::TelemetryQueryFailed(
                "serial stream closed while waiting for DTS/VS data".into(),
            ));
        }

        for frame in parser.push(&read_buf[..read]) {
            match frame {
                TdmFrame::Result(frame) => {
                    handle_result_frame(
                        &frame,
                        engine_dispatches,
                        engine_layout,
                        config,
                        status,
                        event_tx,
                        runtime_measurements,
                    )
                    .await;
                }
                TdmFrame::DtsVs(frame) => {
                    let frame_asic = dts_vs_frame_asic(&frame);
                    let update =
                        build_dts_vs_telemetry_update(&frame, config).ok_or_else(|| {
                            HashThreadError::TelemetryQueryFailed(
                                "failed to build DTS/VS telemetry update".into(),
                            )
                        })?;
                    // A fault observed here is a fault. The query succeeded;
                    // the answer is bad news, and those are different things.
                    // Reporting it as a failed query told an operator that
                    // their question had failed, at the moment they most
                    // needed the answer -- and stopped nothing, while the same
                    // frame arriving on the stream would have stopped the
                    // thread.
                    let should_shutdown = handle_dts_vs_frame(
                        &frame,
                        config,
                        status,
                        event_tx,
                        None,
                        &mut FaultCorroborator::default(),
                        &mut DtsVsDiagnostics::default(),
                        // A one-shot query answers one caller now, so there is
                        // nothing to coalesce against: a zero window publishes
                        // immediately, exactly as this path did before.
                        &mut TelemetryCoalescer::new(Duration::ZERO),
                    )
                    .await;
                    if should_shutdown {
                        return Err(HashThreadError::HardwareFaultReported(
                            "a device asserted a hardware fault while answering a DTS/VS query"
                                .into(),
                        ));
                    }
                    if frame_asic == asic {
                        return Ok(update);
                    }
                }
                TdmFrame::Register(_) | TdmFrame::Noop(_) => {}
            }
        }
    }
}

fn dts_vs_frame_asic(frame: &TdmDtsVsFrame) -> u8 {
    match frame {
        TdmDtsVsFrame::Gen1(frame) => frame.asic,
        TdmDtsVsFrame::Gen2(frame) => frame.asic,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::super::protocol;

    use super::*;

    use crate::transport::{SerialConfig, SerialStream};

    use nix::pty::openpty;
    use std::os::unix::io::IntoRawFd;

    /// Reported by @j-kon in review of #117: a read count of 0 encodes as
    /// "1 byte" on the wire (the count field is zero-based), desyncing the
    /// reader from the ASIC's actual reply. Refused before the request is
    /// encoded or sent.
    #[tokio::test]
    async fn read_register_refuses_a_zero_count_before_the_wire() {
        let pty = openpty(None, None).unwrap();
        let thread_side =
            SerialStream::from_fd(pty.master.into_raw_fd(), SerialConfig::default()).unwrap();
        let (mut reader, mut writer, _control) = thread_side.split();

        let err = read_register(&mut reader, &mut writer, 2, 0x0345, 0x67, 0)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, HashThreadError::DiagnosticsFailed(reason) if reason.contains("count")),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn a_reported_fault_is_not_a_failed_query() {
        // The distinction the API and any operator depend on: the query
        // worked, and the chain said something alarming. Collapsing the two
        // is how "your question failed" gets shown to somebody who was asking
        // because they already suspected a problem.
        let failed = HashThreadError::TelemetryQueryFailed("timed out".into());
        let fault = HashThreadError::HardwareFaultReported(
            "a device asserted a hardware fault while answering a DTS/VS query".into(),
        );
        assert!(!matches!(fault, HashThreadError::TelemetryQueryFailed(_)));
        let text = format!("{fault}");
        assert!(
            text.contains("fault"),
            "the message must name a fault: {text}"
        );
        assert!(
            !text.contains("query failed"),
            "and must not read as a failed question: {text}"
        );
        assert!(format!("{failed}").contains("query failed"));
    }

    /// A failing diagnostic must still leave DTS/VS streaming running.
    ///
    /// Previously diagnostics were *refused* whenever streaming was active, so
    /// this could not arise. Now they suspend and resume around the operation —
    /// and if the resume were skipped on the error path, one failed diagnostic
    /// would silently cost the board every per-ASIC die temperature until
    /// restart. On bitaxeBIRDS, which has no board temperature sensor, that
    /// means all thermal visibility.
    #[tokio::test]
    async fn failing_diagnostic_still_resumes_dts_vs_streaming() {
        let pty = openpty(None, None).unwrap();
        let thread_side =
            SerialStream::from_fd(pty.master.into_raw_fd(), SerialConfig::default()).unwrap();
        let host_side =
            SerialStream::from_fd(pty.slave.into_raw_fd(), SerialConfig::default()).unwrap();
        let (mut reader, mut writer, _control) = thread_side.split();
        let (mut host_reader, _host_writer, _host_control) = host_side.split();

        // Streaming already up, so the helper takes the suspend path without
        // needing bring-up emulated.
        let mut dts_vs = DtsVsStream { enabled: true };
        let mut parser = TdmFrameParser::default();
        let mut corroborator = FaultCorroborator::default();

        let result: Result<(), HashThreadError> = run_idle_uart_diagnostic(
            false,
            &mut dts_vs,
            DiagnosticStream {
                reader: &mut reader,
                parser: &mut parser,
                corroborator: &mut corroborator,
            },
            &mut writer,
            async |_reader| {
                Err(HashThreadError::DiagnosticsFailed(
                    "deliberate failure".into(),
                ))
            },
        )
        .await;

        assert!(result.is_err(), "the diagnostic itself should have failed");
        assert!(
            dts_vs.enabled,
            "streaming must be back on after a failing diagnostic"
        );

        // Both register writes must have reached the wire, in order: suspend
        // writes the documented reset value (sensor bit clear), resume
        // re-enables the sensor stream.
        let suspend = encode_write_register(
            protocol::BROADCAST_ASIC,
            crate::asic::bzm2::uart::NOTCH_REG,
            0x0a,
            &crate::asic::bzm2::uart::UART_TX_CTRL_RESET.to_le_bytes(),
        );
        let resume = encode_write_register(
            protocol::BROADCAST_ASIC,
            crate::asic::bzm2::uart::NOTCH_REG,
            0x0a,
            &0x0fu32.to_le_bytes(),
        );
        let mut seen = vec![0u8; suspend.len() + resume.len()];
        tokio::time::timeout(Duration::from_secs(1), host_reader.read_exact(&mut seen))
            .await
            .expect("host should observe both register writes")
            .expect("read should succeed");

        assert_eq!(&seen[..suspend.len()], suspend.as_slice(), "suspend write");
        assert_eq!(&seen[suspend.len()..], resume.as_slice(), "resume write");
    }

    /// NOTHING FROM BEFORE A DIAGNOSTIC SURVIVES IT.
    ///
    /// The diagnostic's reads take bytes the parser never sees, so a partial
    /// frame held from before is the front of a frame whose back is gone, and
    /// pending fault sightings are from a stream that no longer continues.
    /// Measured on hardware: one thread needed three sightings of "asic 7" in 250 ms; its
    /// recording shows one.
    #[tokio::test]
    async fn a_diagnostic_leaves_no_half_frame_and_no_sighting_behind() {
        let pty = openpty(None, None).unwrap();
        let thread_side =
            SerialStream::from_fd(pty.master.into_raw_fd(), SerialConfig::default()).unwrap();
        let host_side =
            SerialStream::from_fd(pty.slave.into_raw_fd(), SerialConfig::default()).unwrap();
        let (mut reader, mut writer, _control) = thread_side.split();
        let (_host_reader, _host_writer, _host_control) = host_side.split();

        let mut parser = TdmFrameParser::default();
        assert!(
            parser
                .push(&[70, protocol::OPCODE_UART_DTS_VS, 0xc8, 0x9b])
                .is_empty(),
            "half a frame parses to nothing yet"
        );
        let mut corroborator = FaultCorroborator::default();
        assert!(!corroborator.observe(7, true));
        assert!(!corroborator.observe(7, true));
        let mut dts_vs = DtsVsStream { enabled: false };

        run_idle_uart_diagnostic(
            false,
            &mut dts_vs,
            DiagnosticStream {
                reader: &mut reader,
                parser: &mut parser,
                corroborator: &mut corroborator,
            },
            &mut writer,
            async |_reader| Ok(()),
        )
        .await
        .unwrap();

        assert_eq!(
            parser.discard_partial(),
            0,
            "the half frame outlived the diagnostic"
        );
        assert!(
            !corroborator.observe(7, true),
            "one sighting after a diagnostic must not complete two from before it"
        );
    }
}
