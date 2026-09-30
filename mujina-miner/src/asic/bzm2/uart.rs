use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

use crate::transport::{SerialReader, SerialWriter};

use super::protocol::{
    BROADCAST_ASIC, OPCODE_UART_LOOPBACK, OPCODE_UART_NOOP, OPCODE_UART_READREG, encode_loopback,
    encode_multicast_write, encode_noop, encode_read_register, encode_write_job,
    encode_write_register,
};

pub const NOTCH_REG: u16 = 0x0fff;
pub const BROADCAST_GROUP_ASIC: u8 = BROADCAST_ASIC;
pub const DEFAULT_ASIC_ID: u8 = 0xfa;
pub const DEFAULT_NOOP_PROBE_TIMEOUT: Duration = Duration::from_millis(100);
/// How long a local-register read may block before it is a failure.
///
/// Generous against the probe timeout because a register read may be answered
/// by a part that is busy, and mean against a hash thread's patience because
/// this read runs inside that thread's select! loop and blocking there stops
/// the chain for the life of the process.
pub const DEFAULT_REG_READ_TIMEOUT: Duration = Duration::from_millis(500);

const LOCAL_REG_ASIC_ID: u8 = 0x0b;
const LOCAL_REG_UART_TDM_CTL: u8 = 0x07;
/// Engine soft reset. Writing 0 then 1 pulses every engine on the addressed
/// ASIC back to rest; addressed to the broadcast id it quiesces a whole chain.
const LOCAL_REG_ENG_SOFT_RESET: u8 = 0x16;
/// `LOCAL_REG_UART_TX` reset value: result, register-read and no-op responses
/// enabled, the sensor stream off. Documented in the bzm2-hwref UART protocol
/// reference ("TX control register: which packets the ASIC sends"). Writing
/// it is how the stream is taken off
/// the TDM path, and it is the state a cold boot leaves the part in.
pub const UART_TX_CTRL_RESET: u32 = 0x07;

/// Low-level BZM2 UART control surface.
///
/// This controller wraps the legacy BZM2 UART framing in a small, explicit API.
/// It is intended for board bring-up, ASIC diagnostics, and developer tooling.
/// The methods are organized around the three routing modes exposed by the ASIC:
///
/// - unicast: target one ASIC and one register space address
/// - multicast: target an ASIC and one engine group row
/// - broadcast: target all ASICs on a bus via ASIC id `0xff`
///
/// Typical usage patterns:
///
/// ```rust,no_run
/// # async fn demo(mut uart: mujina_miner::asic::bzm2::Bzm2UartController) -> Result<(), Box<dyn std::error::Error>> {
/// use mujina_miner::asic::bzm2::{Bzm2UartController, NOTCH_REG};
///
/// // Unicast: write one ASIC-local register.
/// uart.write_local_reg_u32(0x02, 0x12, 1).await?;
///
/// // Broadcast: push one local register update to every ASIC on the UART bus.
/// uart.broadcast_local_reg_u32(0x07, 0x1).await?;
///
/// // Multicast: update all engines in one row group on one ASIC.
/// uart.multicast_write_reg_u8(0x02, 7, 0x49, 60).await?;
/// # Ok(()) }
/// ```
pub struct Bzm2UartController {
    reader: SerialReader,
    writer: SerialWriter,
}

/// The three payload bytes a healthy BZM2 returns to a NOOP, in wire order.
///
/// This is a known-answer probe, and the only one the part offers: it is the
/// cheapest possible check that the link is framed correctly, because a
/// mis-clocked or mis-aligned link cannot produce these three bytes by
/// accident. Enumeration depends on it, so a wrong value here reads as an
/// empty chain rather than as a bug.
///
/// The bytes are the part name reversed. That is the trap: the obvious
/// spelling is the wrong one, and it is wrong in a way that looks right in
/// review.
///
/// Provenance, in two parts, because they carry different weight.
///
/// MEASURED ON OUR OWN WIRE, once: a capture
/// carries `00 0f 32 5a 42` -- asic 0x00,
/// [`OPCODE_UART_NOOP`], then these three bytes. That is a complete,
/// frame-aligned NOOP reply, not three bytes that happened to land together.
/// Across all seven wire captures we hold, this spelling occurs once and the
/// reversed one occurs ZERO times, so the evidence is one-directional even
/// though it is thin.
///
/// AGREED BY THREE IMPLEMENTATIONS -- two reference implementations and the
/// third-party port at https://github.com/johnny9/ESP-Miner-Bonanza
/// (components/asic/bzm_transport.c). Those may share an origin, so agreement
/// among them is not independent evidence; it is corroboration of the reading
/// above rather than a substitute for it.
///
/// n=1 is still n=1. A follow-on measurement
/// measures it.
pub const NOOP_SIGNATURE: [u8; 3] = *b"2ZB";

impl Bzm2UartController {
    pub fn new(reader: SerialReader, writer: SerialWriter) -> Self {
        Self { reader, writer }
    }

    pub async fn write_register(
        &mut self,
        asic: u8,
        engine_address: u16,
        offset: u8,
        value: &[u8],
    ) -> Result<(), Bzm2UartError> {
        self.writer
            .write_all(&encode_write_register(asic, engine_address, offset, value))
            .await?;
        self.writer.flush().await?;
        Ok(())
    }

    pub async fn write_register_u8(
        &mut self,
        asic: u8,
        engine_address: u16,
        offset: u8,
        value: u8,
    ) -> Result<(), Bzm2UartError> {
        self.write_register(asic, engine_address, offset, &[value])
            .await
    }

    pub async fn write_register_u32(
        &mut self,
        asic: u8,
        engine_address: u16,
        offset: u8,
        value: u32,
    ) -> Result<(), Bzm2UartError> {
        self.write_register(asic, engine_address, offset, &value.to_le_bytes())
            .await
    }

    pub async fn write_local_reg_u8(
        &mut self,
        asic: u8,
        offset: u8,
        value: u8,
    ) -> Result<(), Bzm2UartError> {
        self.write_register_u8(asic, NOTCH_REG, offset, value).await
    }

    pub async fn write_local_reg_u32(
        &mut self,
        asic: u8,
        offset: u8,
        value: u32,
    ) -> Result<(), Bzm2UartError> {
        self.write_register_u32(asic, NOTCH_REG, offset, value)
            .await
    }

    pub async fn broadcast_local_reg_u8(
        &mut self,
        offset: u8,
        value: u8,
    ) -> Result<(), Bzm2UartError> {
        self.write_local_reg_u8(BROADCAST_ASIC, offset, value).await
    }

    pub async fn broadcast_local_reg_u32(
        &mut self,
        offset: u8,
        value: u32,
    ) -> Result<(), Bzm2UartError> {
        self.write_local_reg_u32(BROADCAST_ASIC, offset, value)
            .await
    }

    /// Program the next ASIC still responding on the default chain id.
    pub async fn assign_default_asic_id(&mut self, new_id: u8) -> Result<(), Bzm2UartError> {
        self.write_local_reg_u32(DEFAULT_ASIC_ID, LOCAL_REG_ASIC_ID, new_id as u32)
            .await
    }

    pub async fn multicast_write_register(
        &mut self,
        asic: u8,
        group: u16,
        offset: u8,
        value: &[u8],
    ) -> Result<(), Bzm2UartError> {
        self.writer
            .write_all(&encode_multicast_write(asic, group, offset, value))
            .await?;
        self.writer.flush().await?;
        Ok(())
    }

    pub async fn multicast_write_reg_u8(
        &mut self,
        asic: u8,
        group: u16,
        offset: u8,
        value: u8,
    ) -> Result<(), Bzm2UartError> {
        self.multicast_write_register(asic, group, offset, &[value])
            .await
    }

    pub async fn read_register(
        &mut self,
        asic: u8,
        engine_address: u16,
        offset: u8,
        count: u8,
    ) -> Result<Vec<u8>, Bzm2UartError> {
        let request = encode_read_register(asic, engine_address, offset, count);
        self.writer.write_all(&request).await?;
        self.writer.flush().await?;

        let expected = count as usize + 2;
        let mut response = vec![0u8; expected];
        self.reader.read_exact(&mut response).await?;
        validate_response_header(asic, OPCODE_UART_READREG, &response)?;
        Ok(response[2..].to_vec())
    }

    pub async fn read_register_u8(
        &mut self,
        asic: u8,
        engine_address: u16,
        offset: u8,
    ) -> Result<u8, Bzm2UartError> {
        Ok(self.read_register(asic, engine_address, offset, 1).await?[0])
    }

    pub async fn read_register_u32(
        &mut self,
        asic: u8,
        engine_address: u16,
        offset: u8,
    ) -> Result<u32, Bzm2UartError> {
        let data = self.read_register(asic, engine_address, offset, 4).await?;
        Ok(u32::from_le_bytes(data.try_into().unwrap()))
    }

    pub async fn read_local_reg_u8(&mut self, asic: u8, offset: u8) -> Result<u8, Bzm2UartError> {
        self.read_register_u8(asic, NOTCH_REG, offset).await
    }

    pub async fn read_local_reg_u32(&mut self, asic: u8, offset: u8) -> Result<u32, Bzm2UartError> {
        self.read_register_u32(asic, NOTCH_REG, offset).await
    }

    pub async fn noop(&mut self, asic: u8) -> Result<[u8; 3], Bzm2UartError> {
        let request = encode_noop(asic);
        self.writer.write_all(&request).await?;
        self.writer.flush().await?;

        let mut response = [0u8; 5];
        self.reader.read_exact(&mut response).await?;
        validate_response_header(asic, OPCODE_UART_NOOP, &response)?;
        Ok(response[2..5].try_into().unwrap())
    }

    pub async fn noop_with_timeout(
        &mut self,
        asic: u8,
        wait: Duration,
    ) -> Result<[u8; 3], Bzm2UartError> {
        let request = encode_noop(asic);
        self.writer.write_all(&request).await?;
        self.writer.flush().await?;

        let mut response = [0u8; 5];
        timeout(wait, self.reader.read_exact(&mut response))
            .await
            .map_err(|_| Bzm2UartError::NoopTimeout {
                asic,
                timeout_ms: wait.as_millis().min(u128::from(u64::MAX)) as u64,
            })??;
        validate_response_header(asic, OPCODE_UART_NOOP, &response)?;
        Ok(response[2..5].try_into().unwrap())
    }

    pub async fn verify_noop_signature(&mut self, asic: u8) -> Result<(), Bzm2UartError> {
        let data = self.noop(asic).await?;
        if data == NOOP_SIGNATURE {
            Ok(())
        } else {
            Err(Bzm2UartError::UnexpectedNoopPayload { asic, data })
        }
    }

    pub async fn verify_noop_signature_with_timeout(
        &mut self,
        asic: u8,
        wait: Duration,
    ) -> Result<(), Bzm2UartError> {
        let data = self.noop_with_timeout(asic, wait).await?;
        if data == NOOP_SIGNATURE {
            Ok(())
        } else {
            Err(Bzm2UartError::UnexpectedNoopPayload { asic, data })
        }
    }

    /// Enumerate a fresh chain by assigning ids to devices that still answer on
    /// the documented default ASIC id `0xFA`.
    pub async fn enumerate_chain(
        &mut self,
        max_asics: u8,
        start_id: u8,
    ) -> Result<Vec<u8>, Bzm2UartError> {
        self.enumerate_chain_with_timeout(max_asics, start_id, DEFAULT_NOOP_PROBE_TIMEOUT)
            .await
    }

    /// Enumerate a fresh chain using a bounded NOOP probe so the walk can stop
    /// cleanly when the last default-id device has been assigned.
    pub async fn enumerate_chain_with_timeout(
        &mut self,
        max_asics: u8,
        start_id: u8,
        probe_timeout: Duration,
    ) -> Result<Vec<u8>, Bzm2UartError> {
        let mut assigned = Vec::new();
        for offset in 0..max_asics {
            let next_id = start_id.saturating_add(offset);
            if self
                .verify_noop_signature_with_timeout(DEFAULT_ASIC_ID, probe_timeout)
                .await
                .is_err()
            {
                break;
            }
            self.assign_default_asic_id(next_id).await?;
            // Bound the post-assignment verify with the same probe timeout: a
            // device that passed the timed probe on 0xfa but then fails to echo
            // on its new id (it died, an id collision garbled the reply, or the
            // assignment half-took) must not wedge enumeration — and board init
            // with it — on an unbounded read_exact.
            self.verify_noop_signature_with_timeout(next_id, probe_timeout)
                .await?;
            assigned.push(next_id);
        }
        Ok(assigned)
    }

    pub async fn set_tdm_enabled(
        &mut self,
        prediv_raw: u32,
        counter: u8,
        enable: bool,
    ) -> Result<(), Bzm2UartError> {
        set_tdm_enabled_stream(&mut self.writer, prediv_raw, counter, enable).await
    }

    pub async fn enable_tdm(&mut self, prediv_raw: u32, counter: u8) -> Result<(), Bzm2UartError> {
        self.set_tdm_enabled(prediv_raw, counter, true).await
    }

    pub async fn disable_tdm(&mut self, prediv_raw: u32, counter: u8) -> Result<(), Bzm2UartError> {
        self.set_tdm_enabled(prediv_raw, counter, false).await
    }

    pub async fn loopback(&mut self, asic: u8, payload: &[u8]) -> Result<Vec<u8>, Bzm2UartError> {
        let request = encode_loopback(asic, payload);
        self.writer.write_all(&request).await?;
        self.writer.flush().await?;

        let expected = payload.len() + 2;
        let mut response = vec![0u8; expected];
        self.reader.read_exact(&mut response).await?;
        validate_response_header(asic, OPCODE_UART_LOOPBACK, &response)?;
        Ok(response[2..].to_vec())
    }

    // Mirrors the write-job wire format field for field; a parameter struct
    // would obscure the correspondence with the opcode layout.
    #[allow(clippy::too_many_arguments)]
    pub async fn write_job(
        &mut self,
        asic: u8,
        engine_address: u16,
        midstate: &[u8; 32],
        merkle_root_residue: u32,
        ntime: u32,
        sequence_id: u8,
        job_control: u8,
    ) -> Result<(), Bzm2UartError> {
        self.writer
            .write_all(&encode_write_job(
                asic,
                engine_address,
                midstate,
                merkle_root_residue,
                ntime,
                sequence_id,
                job_control,
            ))
            .await?;
        self.writer.flush().await?;
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Bzm2UartError {
    #[error("serial I/O failed: {0}")]
    Io(#[from] std::io::Error),

    #[error("short UART response: expected {expected} bytes, got {actual}")]
    ShortResponse { expected: usize, actual: usize },

    #[error(
        "unexpected UART response header: expected asic {expected_asic:#x} opcode {expected_opcode:#x}, got asic {actual_asic:#x} opcode {actual_opcode:#x}"
    )]
    UnexpectedHeader {
        expected_asic: u8,
        expected_opcode: u8,
        actual_asic: u8,
        actual_opcode: u8,
    },

    #[error("unexpected NOOP payload from ASIC {asic:#x}: {data:02x?}")]
    UnexpectedNoopPayload { asic: u8, data: [u8; 3] },

    #[error("timed out waiting for NOOP response from ASIC {asic:#x} after {timeout_ms} ms")]
    NoopTimeout { asic: u8, timeout_ms: u64 },

    #[error(
        "the DTS/VS stream is still running after being told to stop: ASIC {asic:#x} \
         is still sending sensor frames, so any diagnostic issued now would read \
         its telemetry as a reply"
    )]
    StreamStillRunning { asic: u8 },

    #[error(
        "ASIC {asic:#x} reports its on-die protection NOT armed after configuration \
         (thermal_enabled={thermal}, voltage_enabled={voltage}); the trip code and \
         shutdown thresholds were broadcast but the part is not reporting them in force"
    )]
    ProtectionNotArmed {
        asic: u8,
        thermal: bool,
        voltage: bool,
    },

    #[error("timed out waiting for DTS/VS frame from ASIC {asic:#x}")]
    DtsVsTimeout { asic: u8 },

    #[error(
        "timed out waiting for TDM register response from ASIC {asic:#x} engine {engine_address:#05x} offset {offset:#04x}"
    )]
    TdmRegisterTimeout {
        asic: u8,
        engine_address: u16,
        offset: u8,
    },
}

/// Return every engine on the addressed ASIC to rest.
///
/// # Why this exists
///
/// Mujina could silence the *sensor* stream but had no way to stop *work*. On a
/// chain it did not start -- a handover, a restart, a crash recovery, a
/// developer attaching to a rig someone left running -- the engines keep hashing
/// whatever they were last given and keep returning results. Measured on an RDS
/// after taking over from the vendor stack: **119,560 result frames in about
/// twenty seconds**, which drowns every reply to every request.
///
/// # The sequence, and why the first write is not redundant
///
/// Write 0, pause, write 1, pause. The zero looks superfluous and is not: the
/// register **latches**, and the hardware does not return it to zero by itself.
/// If it is already 1 -- which it will be on any chain that has run -- writing 1
/// again is a no-op and nothing resets. The zero is what guarantees the edge.
///
/// Corroborated across two independent implementations, which agree on the
/// register, the order and the roughly one millisecond spacing.
///
/// # Addressing
///
/// Pass [`BROADCAST_ASIC`] to quiesce every ASIC on the bus at once -- a single
/// pair of writes rather than a sweep of the 944 engines each part carries.
pub async fn soft_reset_engines(writer: &mut SerialWriter, asic: u8) -> Result<(), Bzm2UartError> {
    write_local_reg_u32_raw(writer, asic, LOCAL_REG_ENG_SOFT_RESET, 0).await?;
    tokio::time::sleep(ENGINE_SOFT_RESET_SETTLE).await;
    write_local_reg_u32_raw(writer, asic, LOCAL_REG_ENG_SOFT_RESET, 1).await?;
    tokio::time::sleep(ENGINE_SOFT_RESET_SETTLE).await;
    Ok(())
}

/// Spacing either side of the soft-reset edge. Both reference implementations
/// use about a millisecond; this is not a measured minimum.
pub const ENGINE_SOFT_RESET_SETTLE: Duration = Duration::from_millis(2);

/// Bit periods each device is given per TDM slot while a chain is mining.
///
/// The slot has to hold the longest frame a part sends unprompted, and it is
/// the value a working hundred-device RDS chain has been measured running in
/// (see [`Bzm2TdmControl::operating`]). A shorter slot is a different mode, not
/// a faster one.
pub const OPERATING_TDM_PREDIV_RAW: u32 = 0x7f;

/// One chain-wide TDM configuration: slot width, slot count, and whether TDM
/// is on at all.
///
/// # Why this is a type and not three arguments
///
/// Everything this driver reads unprompted -- results and the sensor stream --
/// arrives in TDM slots. Turn TDM off and a chain goes silent without any
/// error: the parts are fine, they are just no longer allowed to talk. So the
/// operating configuration is a fact the driver has to be able to PUT BACK,
/// and a fact that is only ever spelled as loose arguments at each call site
/// is one that a diagnostic will eventually overwrite and not restore.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bzm2TdmControl {
    pub prediv_raw: u32,
    pub counter: u8,
    pub enabled: bool,
}

impl Bzm2TdmControl {
    /// TDM on, one slot for every id up to the highest on the chain.
    ///
    /// The counter is the number of slots in a frame and a device transmits
    /// only when the slot number equals its id, so it has to cover the
    /// HIGHEST id rather than count the devices. An empty chain gets one slot.
    ///
    /// Derived rather than configured, and pinned by test to the one value we
    /// have measured a working chain in: 0xfec9 for ids 0..=99, logged by the
    /// vendor daemon, measured on hardware.
    pub fn operating(wire_asic_ids: &[u8]) -> Self {
        let counter = wire_asic_ids
            .iter()
            .max()
            .map_or(1, |highest| highest.saturating_add(1));
        Self {
            prediv_raw: OPERATING_TDM_PREDIV_RAW,
            counter,
            enabled: true,
        }
    }

    pub fn encode(self) -> u32 {
        encode_tdm_control(self.prediv_raw, self.counter, self.enabled)
    }
}

/// Write one chain-wide TDM configuration.
pub async fn set_tdm_control_stream(
    writer: &mut SerialWriter,
    control: Bzm2TdmControl,
) -> Result<(), Bzm2UartError> {
    set_tdm_enabled_stream(writer, control.prediv_raw, control.counter, control.enabled).await
}

pub async fn set_tdm_enabled_stream(
    writer: &mut SerialWriter,
    prediv_raw: u32,
    counter: u8,
    enable: bool,
) -> Result<(), Bzm2UartError> {
    write_local_reg_u32_raw(
        writer,
        BROADCAST_ASIC,
        LOCAL_REG_UART_TDM_CTL,
        encode_tdm_control(prediv_raw, counter, enable),
    )
    .await
}

async fn write_local_reg_u32_raw(
    writer: &mut SerialWriter,
    asic: u8,
    offset: u8,
    value: u32,
) -> Result<(), Bzm2UartError> {
    writer
        .write_all(&encode_write_register(
            asic,
            NOTCH_REG,
            offset,
            &value.to_le_bytes(),
        ))
        .await?;
    writer.flush().await?;
    Ok(())
}

fn encode_tdm_control(prediv_raw: u32, counter: u8, enable: bool) -> u32 {
    (prediv_raw << 9) | ((counter as u32) << 1) | u32::from(enable)
}

fn validate_response_header(
    expected_asic: u8,
    expected_opcode: u8,
    response: &[u8],
) -> Result<(), Bzm2UartError> {
    if response.len() < 2 {
        return Err(Bzm2UartError::ShortResponse {
            expected: 2,
            actual: response.len(),
        });
    }

    let actual_asic = response[0];
    let actual_opcode = response[1];
    if actual_asic != expected_asic || actual_opcode != expected_opcode {
        return Err(Bzm2UartError::UnexpectedHeader {
            expected_asic,
            expected_opcode,
            actual_asic,
            actual_opcode,
        });
    }

    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;

    use nix::pty::openpty;

    use crate::transport::SerialStream;

    #[test]
    fn response_header_validation_accepts_matching_unicast_response() {
        validate_response_header(0x12, OPCODE_UART_READREG, &[0x12, OPCODE_UART_READREG]).unwrap();
    }

    #[test]
    fn response_header_validation_rejects_mismatched_response() {
        let err = validate_response_header(0x12, OPCODE_UART_NOOP, &[0x13, OPCODE_UART_LOOPBACK])
            .unwrap_err();
        assert!(matches!(
            err,
            Bzm2UartError::UnexpectedHeader {
                expected_asic: 0x12,
                expected_opcode: OPCODE_UART_NOOP,
                actual_asic: 0x13,
                actual_opcode: OPCODE_UART_LOOPBACK,
            }
        ));
    }

    #[test]
    fn default_asic_id_matches_legacy_value() {
        assert_eq!(DEFAULT_ASIC_ID, 0xfa);
    }

    #[test]
    fn a_register_read_timeout_is_defined_and_finite() {
        assert!(DEFAULT_REG_READ_TIMEOUT > Duration::from_millis(0));
        assert!(DEFAULT_REG_READ_TIMEOUT < Duration::from_secs(5));
    }

    /// The signature is the spelling we have SEEN, not the one that reads right.
    ///
    /// Across seven wire captures the bytes `32 5a 42` occur once, frame-aligned
    /// behind asic 0x00 and OPCODE_UART_NOOP, and the reversed spelling occurs
    /// zero times. This pins both halves: the value, and the fact that the
    /// plausible alternative is not what the part sends. Enumeration reads a
    /// wrong value here as an EMPTY CHAIN rather than as a bug, which is why it
    /// is worth a test that names the wrong answer explicitly.
    #[test]
    fn the_noop_signature_is_the_spelling_observed_on_our_wire() {
        assert_eq!(
            NOOP_SIGNATURE,
            [0x32, 0x5a, 0x42],
            "the bytes seen at wire.log:714"
        );
        assert_eq!(&NOOP_SIGNATURE, b"2ZB");
        // The obvious spelling -- the part name forwards -- is the trap, and it
        // has never appeared on our wire.
        assert_ne!(
            NOOP_SIGNATURE,
            [0x42, 0x5a, 0x32],
            "BZ2 is the plausible wrong answer and must stay refused"
        );
    }

    #[test]
    fn default_noop_probe_timeout_is_bounded() {
        assert_eq!(DEFAULT_NOOP_PROBE_TIMEOUT, Duration::from_millis(100));
    }

    /// The operating TDM control is DERIVED from the chain, not typed in. This
    /// pins the derivation to the one value we have measured a working chain
    /// in: 0xfec9 on a hundred-device RDS chain, logged by the vendor daemon
    /// measured on hardware.
    #[test]
    fn operating_tdm_for_a_hundred_device_chain_is_the_measured_value() {
        let chain: Vec<u8> = (0..100).collect();
        assert_eq!(Bzm2TdmControl::operating(&chain).encode(), 0xfec9);
    }

    /// The counter must cover the HIGHEST id on the chain, not its length: a
    /// device whose id is at or past the counter never gets a slot, and a
    /// device with no slot sends nothing -- no results and no temperatures.
    #[test]
    fn operating_tdm_counter_covers_the_highest_id_not_the_count() {
        assert_eq!(Bzm2TdmControl::operating(&[16, 17, 18, 19]).counter, 20);
        assert_eq!(Bzm2TdmControl::operating(&[]).counter, 1);
        assert_eq!(Bzm2TdmControl::operating(&[255]).counter, 255);
    }

    #[tokio::test]
    async fn enumerate_chain_bounds_post_assignment_verify() {
        let pty = openpty(None, None).unwrap();
        let master = pty.master;
        let slave = pty.slave;
        let serial_path = fs::read_link(format!("/proc/self/fd/{}", slave.as_raw_fd()))
            .unwrap()
            .to_string_lossy()
            .into_owned();

        let emulator = std::thread::spawn(move || {
            let mut file = fs::File::from(master);

            // First timed NOOP probe on the default id -> answer the signature.
            let mut probe = [0u8; 4];
            file.read_exact(&mut probe).unwrap();
            assert_eq!(probe.to_vec(), encode_noop(DEFAULT_ASIC_ID));
            file.write_all(&[
                DEFAULT_ASIC_ID,
                OPCODE_UART_NOOP,
                NOOP_SIGNATURE[0],
                NOOP_SIGNATURE[1],
                NOOP_SIGNATURE[2],
            ])
            .unwrap();
            file.flush().unwrap();

            // Accept the assign-id write...
            let assign_len = encode_write_register(
                DEFAULT_ASIC_ID,
                NOTCH_REG,
                LOCAL_REG_ASIC_ID,
                &7u32.to_le_bytes(),
            )
            .len();
            let mut assign = vec![0u8; assign_len];
            file.read_exact(&mut assign).unwrap();

            // ...then go silent for the post-assignment verify probe.
            let mut verify = [0u8; 4];
            let _ = file.read_exact(&mut verify);
            std::thread::sleep(Duration::from_millis(300));
        });

        let stream = SerialStream::new(&serial_path, 5_000_000).unwrap();
        let (reader, writer, _control) = stream.split();
        let mut uart = Bzm2UartController::new(reader, writer);

        // Without the post-assign timeout this call never returns; assert it
        // completes with a bounded timeout error well inside the wall clock.
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            uart.enumerate_chain_with_timeout(4, 7, Duration::from_millis(100)),
        )
        .await;
        assert!(
            result.is_ok(),
            "enumerate_chain hung on the post-assign verify"
        );
        assert!(matches!(
            result.unwrap(),
            Err(Bzm2UartError::NoopTimeout { asic: 7, .. })
        ));

        emulator.join().unwrap();
    }

    /// The signature is an external fact about the silicon, so it is pinned to
    /// its literal here rather than only being referenced. Every other test
    /// feeds the constant, which means they would all still pass if this value
    /// were changed to something the part never sends -- this is the one test
    /// that would fail, and the provenance for why lives on the constant.
    ///
    /// Note what this test does NOT do: it cannot tell us the value is right.
    /// Only the wire can, and until a follow-on measurement runs, no test in this
    /// repository has ever seen the part answer.
    #[test]
    fn noop_signature_is_the_part_name_reversed() {
        assert_eq!(&NOOP_SIGNATURE, b"2ZB");
    }

    /// The spelling that reads correctly is the one the part does not send.
    /// It is a real vendor constant, for an unrelated purpose, which is how it
    /// came to be used here; enumeration silently found no devices. A probe
    /// that accepts it is not a probe.
    #[test]
    fn noop_signature_rejects_the_plausible_reversal() {
        assert_ne!(&NOOP_SIGNATURE, b"BZ2");
    }

    /// The zero write is the whole point and the easy thing to optimise away.
    /// The register latches and the hardware never clears it, so on any chain
    /// that has run it already reads 1 -- writing 1 again resets nothing. This
    /// test fails if someone "simplifies" the sequence to a single write.
    #[tokio::test]
    #[cfg_attr(
        feature = "skip-pty-tests",
        ignore = "PTY-based test, skipped in this environment"
    )]
    async fn soft_reset_pulses_low_then_high() {
        use nix::pty::openpty;
        use std::io::Read;

        let Ok(pty) = openpty(None, None) else { return };
        let serial_path = match fs::read_link(format!("/proc/self/fd/{}", pty.slave.as_raw_fd())) {
            Ok(p) => p.to_string_lossy().into_owned(),
            Err(_) => return,
        };
        let stream = SerialStream::new(&serial_path, 115_200).expect("open the pty slave");
        let (_r, mut writer, _c) = stream.split();

        soft_reset_engines(&mut writer, BROADCAST_ASIC)
            .await
            .unwrap();

        let mut master = fs::File::from(pty.master);
        let mut seen = vec![0u8; 512];
        let n = master.read(&mut seen).unwrap_or(0);
        let wrote = &seen[..n];

        let frame = |v: u32| {
            encode_write_register(
                BROADCAST_ASIC,
                NOTCH_REG,
                LOCAL_REG_ENG_SOFT_RESET,
                &v.to_le_bytes(),
            )
        };
        let find = |needle: Vec<u8>| wrote.windows(needle.len()).position(|w| w == needle);
        let lo = find(frame(0)).expect("the deassert write never reached the wire");
        let hi = find(frame(1)).expect("the assert write never reached the wire");
        assert!(
            lo < hi,
            "soft reset wrote 1 before 0; the register latches, so that resets nothing"
        );
    }
}
