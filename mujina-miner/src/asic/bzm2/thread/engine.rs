use std::time::Duration;

use tokio::io::AsyncWriteExt;

use crate::transport::serial::{SerialReader, SerialWriter};

use super::super::protocol::{
    self, BROADCAST_ASIC, Bzm2EngineLayout, encode_write_register, logical_engine_address,
};
use super::super::uart::soft_reset_engines;
use super::actor::*;
use super::diagnostics::*;
use super::dispatch::*;
use super::*;

/// Patience for each engine-state read around the attach ungate. A chain
/// answers these in milliseconds; a line that does not answer in
/// this long will not answer the next one either, and waiting the full
/// diagnostic timeout per read would stall attach by seconds.
pub(super) const UNGATE_READ_TIMEOUT: Duration = Duration::from_millis(250);

/// Whether the engines can hash the work they are sent.
///
/// # Why dispatch waits on this
///
/// An engine soft reset gates every TCE clock, and a gated engine accepts work
/// and never hashes it. Measured on hardware: dispatching for 125 s into exactly
/// that produced no error anywhere, no result frame, idle power. Reading
/// the state directly, CONFIG is 0x77 after the reset and 0x14 once written 0x04.
/// Nothing about the dispatch path can see the difference, so it is decided
/// here, once, and dispatch refuses until it is decided well.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum EngineGate {
    /// Ungated at attach, and read back ungated.
    Ready,
    /// Ungated again after a stop, write-only; the readback at attach proved
    /// the write path.
    Rearmed,
    /// Reset by a stop; ungated again before the next dispatch.
    Stopped,
    /// This attach did not reset the engines (quiesce disabled), so the
    /// previous owner's configuration stands and was not touched.
    Inherited,
    /// Read back with a TCE gate set, or not in run-all mode.
    Gated {
        asic: u8,
        engine_address: u16,
        config: u8,
    },
    /// The readback could not be taken. Not a pass.
    Unmeasured(String),
}

impl EngineGate {
    pub(super) fn refusal(&self) -> Option<String> {
        match self {
            EngineGate::Ready | EngineGate::Rearmed | EngineGate::Inherited => None,
            EngineGate::Stopped => {
                Some("the engines were reset by a stop and have not been ungated".into())
            }
            EngineGate::Gated {
                asic,
                engine_address,
                config,
            } => Some(format!(
                "ASIC {asic} engine {engine_address:#05x} reads CONFIG {config:#04x}: TCE clocks \
                 gated, so work sent there would never hash"
            )),
            EngineGate::Unmeasured(why) => Some(format!(
                "engine CONFIG could not be read back ({why}); not dispatching to engines that \
                 may be gated"
            )),
        }
    }
}

/// Write CONFIG = run-all-TCE to every active engine, on every ASIC at once.
///
/// Broadcast ASIC, one write per engine address: the addressing dispatch
/// already uses for every other per-engine register. Our hwref lists enabling
/// the TCE clocks through CONFIG as required before job submission.
async fn write_engine_ungate(
    writer: &mut SerialWriter,
    engine_layout: &Bzm2EngineLayout,
) -> std::io::Result<()> {
    for &(row, col) in engine_layout.active_coordinates() {
        writer
            .write_all(&encode_write_register(
                BROADCAST_ASIC,
                logical_engine_address(row, col),
                protocol::ENGINE_REG_CONFIG,
                &[protocol::ENGINE_CONFIG_RUN_ALL_TCE],
            ))
            .await?;
    }
    writer.flush().await
}

/// Before any dispatch: re-arm engines a stop has reset, and say whether
/// dispatch may proceed.
///
/// Logs ONE error when dispatch starts being refused and one info line when
/// it stops being refused -- never one per job or per tick, which on this
/// path would be two a second.
pub(super) async fn engines_may_dispatch(
    writer: &mut SerialWriter,
    engine_gate: &mut EngineGate,
    engine_layout: &Bzm2EngineLayout,
    config: &Bzm2ThreadConfig,
    refusal_logged: &mut bool,
) -> bool {
    if *engine_gate == EngineGate::Stopped {
        // A reset clears CONFIG and the nonce slices alike.
        let rearm = async {
            write_engine_ungate(writer, engine_layout).await?;
            program_nonce_slices(writer, engine_layout, config).await
        };
        *engine_gate = match rearm.await {
            Ok(()) => EngineGate::Rearmed,
            Err(err) => EngineGate::Unmeasured(format!("the re-arm write failed: {err}")),
        };
    }
    match engine_gate.refusal() {
        None => {
            if *refusal_logged {
                *refusal_logged = false;
                info!(path = %config.serial_path, gate = ?engine_gate,
                    "Engines ready; dispatch proceeds");
            }
            true
        }
        Some(why) => {
            if !*refusal_logged {
                *refusal_logged = true;
                error!(path = %config.serial_path, reason = %why,
                    "Work accepted and NOT dispatched: the engines are not ready");
            }
            false
        }
    }
}

/// Stop every engine by pulsing the soft reset: the one action measured to
/// take the load off (measured on hardware: both show the drop at
/// attach).
///
/// Needed because a running engine does not stop by itself. Jobs are sent
/// with TIMESTAMP_COUNT's repeat bit set, so an engine re-runs its last job
/// until given another: the vendor's own SIGKILL left this board drawing
/// ~1.5 kW for 23 s with nobody attending, until Mujina's reset. Stopping
/// dispatch alone, which is all a stop did before, removes no load.
pub(super) async fn stop_engines(writer: &mut SerialWriter, config: &Bzm2ThreadConfig) {
    match tokio::time::timeout(
        CHAIN_QUIESCE_TIMEOUT,
        soft_reset_engines(writer, BROADCAST_ASIC),
    )
    .await
    {
        Ok(Ok(())) => info!(path = %config.serial_path,
            "Engines reset: our work on this chain has stopped"),
        Ok(Err(err)) => error!(path = %config.serial_path, error = %err,
            "COULD NOT RESET THE ENGINES: they may still be hashing our last job"),
        Err(_) => error!(path = %config.serial_path,
            "Resetting the engines timed out: they may still be hashing our last job"),
    }
}

/// Is this CONFIG value one an engine hashes in?
fn engine_config_runs(config: u8) -> bool {
    config & protocol::ENGINE_CONFIG_TCE_GATES == 0
        && config & protocol::ENGINE_CONFIG_RUN_ALL_TCE != 0
}

/// Read an engine's STATUS and CONFIG within `UNGATE_READ_TIMEOUT`.
async fn read_engine_state(
    reader: &mut SerialReader,
    writer: &mut SerialWriter,
    asic: u8,
    engine_address: u16,
) -> Result<Vec<u8>, String> {
    let read = read_register(
        reader,
        writer,
        asic,
        engine_address,
        protocol::ENGINE_REG_STATUS,
        4,
    );
    match tokio::time::timeout(UNGATE_READ_TIMEOUT, read).await {
        Ok(Ok(bytes)) if bytes.len() >= 2 => Ok(bytes),
        Ok(Ok(bytes)) => Err(format!("{} bytes, expected 4", bytes.len())),
        Ok(Err(err)) => Err(err.to_string()),
        Err(_) => Err(format!("no answer within {UNGATE_READ_TIMEOUT:?}")),
    }
}

/// Ungate every engine after the attach reset, and read back that it took.
///
/// Needs a quiet line: runs after the chain is confirmed silent and before
/// the sensor stream is configured. Reads the first and last engine of the
/// first and last ASIC; any one still gated refuses dispatch, and a read that
/// cannot be taken is `Unmeasured`, never `Ready`.
pub(super) async fn ungate_engines_and_confirm(
    reader: &mut SerialReader,
    writer: &mut SerialWriter,
    engine_layout: &Bzm2EngineLayout,
    config: &Bzm2ThreadConfig,
) -> EngineGate {
    let coords = engine_layout.active_coordinates();
    let (Some(&first), Some(&last)) = (coords.first(), coords.last()) else {
        return EngineGate::Unmeasured("no active engines in the layout".into());
    };
    let first_asic = config.asic_ids.first().copied().unwrap_or(0);
    let last_asic = config.asic_ids.last().copied().unwrap_or(first_asic);
    let first_address = logical_engine_address(first.0, first.1);
    let last_address = logical_engine_address(last.0, last.1);

    // The state the reset left, logged every run: a reading taken
    // took once by hand, now evidence in every window.
    let before = read_engine_state(reader, writer, first_asic, first_address).await;
    match &before {
        Ok(bytes) => info!(
            path = %config.serial_path, asic = first_asic, engine = first_address,
            status = format!("{:#04x}", bytes[0]), config = format!("{:#04x}", bytes[1]),
            "Engine state after the attach reset, before ungating"
        ),
        Err(why) => warn!(path = %config.serial_path, reason = %why,
            "Engine state before ungating could not be read"),
    }

    // Written whatever the read said: ungating is right after a reset, and
    // a line that did not answer may still take writes.
    if let Err(err) = write_engine_ungate(writer, engine_layout).await {
        return EngineGate::Unmeasured(format!("the ungate write failed: {err}"));
    }
    if let Err(why) = before {
        return EngineGate::Unmeasured(format!(
            "the chain did not answer the engine state read ({why}), so the ungate cannot be \
             confirmed"
        ));
    }

    let mut samples = vec![(first_asic, first_address), (first_asic, last_address)];
    if last_asic != first_asic {
        samples.extend([(last_asic, first_address), (last_asic, last_address)]);
    }
    for (asic, engine_address) in samples {
        match read_engine_state(reader, writer, asic, engine_address).await {
            Ok(bytes) => {
                if !engine_config_runs(bytes[1]) {
                    return EngineGate::Gated {
                        asic,
                        engine_address,
                        config: bytes[1],
                    };
                }
            }
            Err(why) => {
                return EngineGate::Unmeasured(format!(
                    "ASIC {asic} engine {engine_address:#05x}: {why}"
                ));
            }
        }
    }
    EngineGate::Ready
}
