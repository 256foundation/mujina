use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{Instant, timeout};

use crate::transport::{SerialReader, SerialWriter};

use super::protocol::{
    BROADCAST_ASIC, DtsVsGeneration, ENGINE_REG_END_NONCE, OPCODE_UART_LOOPBACK, OPCODE_UART_NOOP,
    OPCODE_UART_READREG, TdmDtsVsFrame, TdmFrame, TdmFrameParser, encode_loopback,
    encode_multicast_write, encode_noop, encode_read_register, encode_write_job,
    encode_write_register, logical_engine_address, physical_engine_coordinates,
};

pub const NOTCH_REG: u16 = 0x0fff;
pub const BROADCAST_GROUP_ASIC: u8 = BROADCAST_ASIC;
pub const DEFAULT_DTS_VS_QUERY_TIMEOUT: Duration = Duration::from_secs(2);
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
const LOCAL_REG_SLOW_CLK_DIV: u8 = 0x08;
const LOCAL_REG_UART_TX: u8 = 0x0a;
/// Engine soft reset. Writing 0 then 1 pulses every engine on the addressed
/// ASIC back to rest; addressed to the broadcast id it quiesces a whole chain.
const LOCAL_REG_ENG_SOFT_RESET: u8 = 0x16;
/// `LOCAL_REG_UART_TX` reset value: result, register-read and no-op responses
/// enabled, the sensor stream off. Documented in the bzm2-hwref UART protocol
/// reference ("TX control register: which packets the ASIC sends"). Writing
/// it is how the stream is taken off
/// the TDM path, and it is the state a cold boot leaves the part in.
pub const UART_TX_CTRL_RESET: u32 = 0x07;
/// `LOCAL_REG_UART_TX` value that puts DTS/VS sensor messages on the TDM path.
///
/// There is no documented "streaming off" counterpart, so the disable path
/// restores a value read back from the part rather than writing a constant.
const DTS_VS_UART_TX_ENABLE: u32 = 0x0f;
const LOCAL_REG_SENS_TDM_GAP_CNT: u8 = 0x2d;
const LOCAL_REG_DTS_SRST_PD: u8 = 0x2e;
const LOCAL_REG_DTS_CFG: u8 = 0x2f;
const LOCAL_REG_TEMPSENSOR_TUNE_CODE: u8 = 0x30;
const LOCAL_REG_SENSOR_THRS_CNT: u8 = 0x3c;
const LOCAL_REG_SENSOR_CLK_DIV: u8 = 0x3d;
const LOCAL_REG_VSENSOR_SRST_PD: u8 = 0x3e;
const LOCAL_REG_VSENSOR_CFG: u8 = 0x3f;
const LOCAL_REG_VOLTAGE_SENSOR_ENABLE: u8 = 0x40;
const LOCAL_REG_BANDGAP: u8 = 0x45;

const THERMAL_SENSOR_RESOLUTION: u8 = 12;
const THERMAL_SENSOR_MODE: u8 = 0;
const VOLTAGE_SENSOR_RESOLUTION: u8 = 14;
const VOLTAGE_SENSOR_CONVERSION_MODE: u8 = 1;
const VOLTAGE_SENSOR_MODE: u8 = 0;
const DISCOVERED_ENGINE_END_NONCE: u32 = 0xffff_fffe;

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
/// use mujina_miner::asic::bzm2::{Bzm2Pll, Bzm2UartController, NOTCH_REG};
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

    pub async fn read_register_tdm_sync(
        &mut self,
        asic: u8,
        engine_address: u16,
        offset: u8,
        count: u8,
        wait: Duration,
    ) -> Result<Vec<u8>, Bzm2UartError> {
        read_register_tdm_sync_stream(
            &mut self.reader,
            &mut self.writer,
            asic,
            engine_address,
            offset,
            count,
            wait,
        )
        .await
    }

    pub async fn detect_engine(
        &mut self,
        asic: u8,
        row: u8,
        col: u8,
        wait: Duration,
    ) -> Result<bool, Bzm2UartError> {
        detect_engine_stream(&mut self.reader, &mut self.writer, asic, row, col, wait).await
    }

    pub async fn discover_engine_map(
        &mut self,
        asic: u8,
        tdm_prediv_raw: u32,
        tdm_counter: u8,
        restore: Bzm2TdmControl,
        wait: Duration,
    ) -> Result<Bzm2DiscoveredEngineMap, Bzm2UartError> {
        discover_engine_map_stream(
            &mut self.reader,
            &mut self.writer,
            asic,
            tdm_prediv_raw,
            tdm_counter,
            restore,
            wait,
        )
        .await
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

    /// Enable DTS/VS reporting using the legacy local-register sequence.
    pub async fn enable_dts_vs(&mut self, config: Bzm2DtsVsConfig) -> Result<(), Bzm2UartError> {
        configure_dts_vs_stream(&mut self.writer, &mut self.reader, &config).await
    }

    /// Read the next DTS/VS frame for a specific ASIC after ensuring DTS/VS is enabled.
    pub async fn query_dts_vs(
        &mut self,
        asic: u8,
        generation: DtsVsGeneration,
        config: Bzm2DtsVsConfig,
        timeout: Duration,
    ) -> Result<TdmDtsVsFrame, Bzm2UartError> {
        self.enable_dts_vs(config).await?;
        read_dts_vs_frame_stream(&mut self.reader, generation, asic, timeout).await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bzm2DtsVsConfig {
    /// Gap between sensor TDM slots. Larger is slower.
    ///
    /// Widened from `u8` because the register takes a full word and the
    /// narrower type capped the achievable gap at 255 for no reason in the
    /// silicon.
    pub tdm_interval: u32,
    pub thermal_trip_c: i32,
    pub voltage_ch0_shutdown_mv: u32,
    pub voltage_ch1_shutdown_mv: u32,
}

/// Environment override for the sensor stream rate.
///
/// The right value is a property of the CONTROLLER'S CPU BUDGET, not of the
/// silicon, so it is tunable without a rebuild.
pub const DTS_VS_GAP_ENV: &str = "MUJINA_BZM2_DTS_VS_GAP";

impl Bzm2DtsVsConfig {
    /// Defaults, with the stream rate overridable from the environment.
    pub fn from_env() -> Self {
        let mut config = Self::default();
        if let Some(gap) = std::env::var(DTS_VS_GAP_ENV)
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
        {
            // Zero would ask for no gap at all; the fastest meaningful setting
            // is 1, which is what this used to be fixed at.
            config.tdm_interval = gap.max(1);
        }
        config
    }
}

impl Default for Bzm2DtsVsConfig {
    fn default() -> Self {
        Self {
            // MEASURED, not chosen for neatness. At a gap of 1 -- which this
            // was fixed at -- a 100-device chain streams 19,389 sensor frames
            // a second, 194 KB/s, measured over 28 s once bring-up started
            // working (measured on hardware:
            // 542,900 frames, zero desyncs, 100% valid). The frames were
            // perfect and there were simply far too many of them: parsing them
            // saturates the two-core control board, and the board never
            // finished registering inside the window.
            //
            // Nothing needs that rate. The thermal interlock only requires a
            // reading inside `thermal_reading_max_age_s`, which is tens of
            // seconds, and no API consumer polls faster than about 1 Hz. A gap
            // of 100 targets roughly 2 Hz per device, which is still two
            // orders of magnitude more often than anything reads it.
            tdm_interval: 100,
            thermal_trip_c: 115,
            voltage_ch0_shutdown_mv: 500,
            voltage_ch1_shutdown_mv: 500,
        }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Bzm2EngineCoordinate {
    pub row: u8,
    pub col: u8,
    pub engine_address: u16,
}

impl Bzm2EngineCoordinate {
    pub fn new(row: u8, col: u8) -> Self {
        Self {
            row,
            col,
            engine_address: logical_engine_address(row, col),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bzm2DiscoveredEngineMap {
    pub asic: u8,
    pub present: Vec<Bzm2EngineCoordinate>,
    pub missing: Vec<Bzm2EngineCoordinate>,
}

impl Bzm2DiscoveredEngineMap {
    pub fn present_count(&self) -> usize {
        self.present.len()
    }

    pub fn missing_count(&self) -> usize {
        self.missing.len()
    }
}

pub async fn configure_dts_vs_stream(
    writer: &mut SerialWriter,
    reader: &mut SerialReader,
    config: &Bzm2DtsVsConfig,
) -> Result<(), Bzm2UartError> {
    // CONFIGURE FIRST, ENABLE LAST. The TX-enable write that starts the
    // sensor stream is at the END of this function, and it must stay there.
    //
    // It used to be the FIRST thing done here, several writes before the
    // BANDGAP read below. That read then had to land on a line this function
    // had just deliberately filled with sensor traffic, and it could not: it
    // failed with `expected asic 0xff opcode 0x3, got asic 0x3f opcode 0x3`
    // on every handover we have ever run, in both dry-run and write-enabled
    // modes.
    //
    // The consequences were worse than a failed bring-up. The enable had
    // already succeeded, and so had the TDM gap write below that sets the
    // stream to its fastest interval -- so the function returned Err, the
    // caller left `dts_vs.enabled` false and fell back to lazy enable, and
    // the part streamed at 13,850 frames/s with the daemon believing
    // telemetry was off. That flood is what silenced the control board's API
    // for an entire handover window on 2026-09-18
    // (measured on hardware).
    //
    // A read on a line you have started streaming on is not a read.

    // Legacy reference clock setup: 50 MHz reference, 6.25 MHz sensor clocks.
    let slow_clk_div = 2u32;
    let sensor_clk_div = 8u32;
    write_local_reg_u32_raw(writer, BROADCAST_ASIC, LOCAL_REG_SLOW_CLK_DIV, slow_clk_div).await?;
    write_local_reg_u32_raw(
        writer,
        BROADCAST_ASIC,
        LOCAL_REG_SENSOR_CLK_DIV,
        (sensor_clk_div << 5) | sensor_clk_div,
    )
    .await?;
    write_local_reg_u32_raw(writer, BROADCAST_ASIC, LOCAL_REG_DTS_SRST_PD, 1 << 8).await?;
    write_local_reg_u32_raw(
        writer,
        BROADCAST_ASIC,
        LOCAL_REG_SENS_TDM_GAP_CNT,
        config.tdm_interval,
    )
    .await?;

    let cfg0_ts_resolution = match THERMAL_SENSOR_RESOLUTION {
        10 => 1,
        8 => 2,
        _ => 0,
    };
    write_local_reg_u32_raw(
        writer,
        BROADCAST_ASIC,
        LOCAL_REG_DTS_CFG,
        ((cfg0_ts_resolution as u32) << 5) | THERMAL_SENSOR_MODE as u32,
    )
    .await?;

    let thermal_threshold_cnt = 10u32;
    let voltage_ch0_threshold_cnt = 10u32;
    write_local_reg_u32_raw(
        writer,
        BROADCAST_ASIC,
        LOCAL_REG_SENSOR_THRS_CNT,
        (thermal_threshold_cnt << 16) | voltage_ch0_threshold_cnt,
    )
    .await?;

    let thermal_trip_code = legacy_temperature_c_to_tune_code(config.thermal_trip_c);
    write_local_reg_u32_raw(
        writer,
        BROADCAST_ASIC,
        LOCAL_REG_TEMPSENSOR_TUNE_CODE,
        0x8001 | (thermal_trip_code << 1),
    )
    .await?;

    let bandgap = read_local_reg_u32_raw(reader, writer, BROADCAST_ASIC, LOCAL_REG_BANDGAP).await?;
    write_local_reg_u32_raw(
        writer,
        BROADCAST_ASIC,
        LOCAL_REG_BANDGAP,
        (bandgap & !0x0f) | 0x03,
    )
    .await?;

    write_local_reg_u32_raw(writer, BROADCAST_ASIC, LOCAL_REG_VSENSOR_SRST_PD, 1 << 8).await?;

    let cfg0_vs_resolution = match VOLTAGE_SENSOR_RESOLUTION {
        12 => 1,
        10 => 2,
        8 => 3,
        _ => 0,
    };
    let gap_cnt = 8u32;
    write_local_reg_u32_raw(
        writer,
        BROADCAST_ASIC,
        LOCAL_REG_VSENSOR_CFG,
        (gap_cnt << 28)
            | ((VOLTAGE_SENSOR_CONVERSION_MODE as u32) << 24)
            | ((cfg0_vs_resolution as u32) << 5)
            | VOLTAGE_SENSOR_MODE as u32,
    )
    .await?;

    write_local_reg_u32_raw(
        writer,
        BROADCAST_ASIC,
        LOCAL_REG_VOLTAGE_SENSOR_ENABLE,
        (legacy_voltage_mv_to_tune_code(config.voltage_ch1_shutdown_mv) << 16)
            | (legacy_voltage_mv_to_tune_code(config.voltage_ch0_shutdown_mv) << 1)
            | 1,
    )
    .await?;

    // Now, and only now, put the sensor messages on the TDM path. Everything
    // this function needs to read has been read; from here the line belongs to
    // the stream. See the note at the top of this function.
    write_local_reg_u32_raw(
        writer,
        BROADCAST_ASIC,
        LOCAL_REG_UART_TX,
        DTS_VS_UART_TX_ENABLE,
    )
    .await?;

    Ok(())
}

/// Take the DTS/VS sensor stream off the TDM path by writing the register's
/// documented reset value ([`UART_TX_CTRL_RESET`]).
///
/// Only the TX-enable register is touched. Sensor clocks, resolution, TDM gap
/// and threshold configuration written by [`configure_dts_vs_stream`] all
/// survive, so resuming is a single register write rather than a reconfigure.
/// Nothing needs capturing first: the off value is a documented constant, and
/// capturing the live value after streaming was enabled would record the
/// enable pattern and make the suspend a no-op.
/// Confirm a stream that was told to stop has actually stopped.
///
/// # The confirmation here is an ABSENCE, and that changes how it must be done
///
/// Suspending and disabling both write a register and return. Nothing checked
/// that the part obeyed, and the consequence is not theoretical: a diagnostic
/// that runs while the stream is still going reads sensor frames as its own
/// replies. That is exactly the failure mode that cost this project a week --
/// mis-framed telemetry decoding to a device outside the chain, a temperature
/// below ambient, and a hardware fault that never happened.
///
/// Because the evidence is silence, buffered bytes already in flight would
/// look like a stream that never stopped. So this drains first, then listens.
/// A frame arriving after the drain is proof the write did not take; a quiet
/// window is the confirmation.
///
/// Returns the number of bytes drained, which is worth having: a large drain
/// says the part had a lot still queued, and a caller doing this repeatedly can
/// watch that number fall.
pub async fn confirm_stream_stopped(
    reader: &mut SerialReader,
    generation: DtsVsGeneration,
    quiet: Duration,
) -> Result<usize, Bzm2UartError> {
    // Whatever was already on the wire is not evidence about the write. Drain
    // until the line goes quiet or the budget is spent. Bounded both ways: a
    // part that never stops would otherwise be drained forever, and the caller
    // is entitled to an answer.
    const DRAIN_QUIET: Duration = Duration::from_millis(50);
    const DRAIN_LIMIT: usize = 64 * 1024;
    let mut drained = 0usize;
    let mut scratch = [0u8; 1024];
    while drained < DRAIN_LIMIT {
        match tokio::time::timeout(DRAIN_QUIET, reader.read(&mut scratch)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => drained += n,
            // Quiet for a whole window: nothing more is in flight.
            Err(_) => break,
            Ok(Err(err)) => return Err(Bzm2UartError::Io(err)),
        }
    }
    match read_dts_vs_frame_stream(reader, generation, BROADCAST_ASIC, quiet).await {
        // A timeout is SUCCESS here: nothing spoke, so the stream is silent.
        // It must be the DTS/VS timeout specifically -- the variant this reader
        // actually returns. Matching the wrong one made every quiet window
        // report "could not confirm" instead of "confirmed quiet".
        Err(Bzm2UartError::DtsVsTimeout { .. }) => Ok(drained),
        Ok(frame) => {
            let asic = match &frame {
                TdmDtsVsFrame::Gen1(f) => f.asic,
                TdmDtsVsFrame::Gen2(f) => f.asic,
            };
            Err(Bzm2UartError::StreamStillRunning { asic })
        }
        // Any other error is a failure to observe, not an observation.
        Err(other) => Err(other),
    }
}

pub async fn suspend_dts_vs_stream(writer: &mut SerialWriter) -> Result<(), Bzm2UartError> {
    write_local_reg_u32_raw(
        writer,
        BROADCAST_ASIC,
        LOCAL_REG_UART_TX,
        UART_TX_CTRL_RESET,
    )
    .await
}

/// Turn the sensors off outright, rather than merely stopping their output.
///
/// # Not the same operation as [`suspend_dts_vs_stream`], and the difference matters
///
/// Suspend resets the transmit path: the part stops sending what it has, the
/// sensors keep producing, and [`resume_dts_vs_stream`] puts output back. That
/// pairing is right around a diagnostic, where telemetry must survive.
///
/// This is for the other case — **taking over a chain somebody else configured**.
/// There, stopping output is not enough: measured on an RDS after inheriting a
/// chain from the vendor stack, suspend alone left 134 KB/s arriving
/// indefinitely and 118,673 sensor frames in a thirty second window, because
/// the sensors were still running and refilling what had just been flushed.
/// Every request timed out underneath it.
///
/// **There is no matching resume, deliberately.** Whoever disables the sensors
/// is taking ownership and is expected to configure them for itself. A resume
/// would have to restore a trip code and thresholds it does not know, and
/// guessing them is worse than leaving the sensors off.
///
/// The sequence is the reference implementation's own disable path rather than
/// an inversion of its enable path — inverting an enable is a guess; this is
/// the operation it actually performs. Corroborated register-for-register
/// against the JTAG validation library, a third implementation.
pub async fn disable_dts_vs_sensors(writer: &mut SerialWriter) -> Result<(), Bzm2UartError> {
    write_local_reg_u32_raw(
        writer,
        BROADCAST_ASIC,
        LOCAL_REG_UART_TX,
        UART_TX_CTRL_RESET,
    )
    .await?;
    write_local_reg_u32_raw(writer, BROADCAST_ASIC, LOCAL_REG_TEMPSENSOR_TUNE_CODE, 0).await?;
    write_local_reg_u32_raw(writer, BROADCAST_ASIC, LOCAL_REG_VOLTAGE_SENSOR_ENABLE, 0).await
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

/// Put the sensor stream back on the TDM path after [`suspend_dts_vs_stream`].
pub async fn resume_dts_vs_stream(writer: &mut SerialWriter) -> Result<(), Bzm2UartError> {
    write_local_reg_u32_raw(
        writer,
        BROADCAST_ASIC,
        LOCAL_REG_UART_TX,
        DTS_VS_UART_TX_ENABLE,
    )
    .await
}

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

pub async fn read_register_tdm_sync_stream(
    reader: &mut SerialReader,
    writer: &mut SerialWriter,
    asic: u8,
    engine_address: u16,
    offset: u8,
    count: u8,
    wait: Duration,
) -> Result<Vec<u8>, Bzm2UartError> {
    let request = encode_read_register(asic, engine_address, offset, count);
    writer.write_all(&request).await?;
    writer.flush().await?;

    let deadline = Instant::now() + wait;
    let mut parser = TdmFrameParser::default();
    parser.expect_read_register_bytes(asic, count as usize);
    let mut read_buf = [0u8; 256];

    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err(Bzm2UartError::TdmRegisterTimeout {
                asic,
                engine_address,
                offset,
            });
        }
        let remaining = deadline.saturating_duration_since(now);
        let read = timeout(remaining, reader.read(&mut read_buf))
            .await
            .map_err(|_| Bzm2UartError::TdmRegisterTimeout {
                asic,
                engine_address,
                offset,
            })??;
        if read == 0 {
            return Err(Bzm2UartError::ShortResponse {
                expected: 1,
                actual: 0,
            });
        }
        for frame in parser.push(&read_buf[..read]) {
            if let TdmFrame::Register(frame) = frame
                && frame.asic == asic
            {
                return Ok(frame.data);
            }
        }
    }
}

pub async fn detect_engine_stream(
    reader: &mut SerialReader,
    writer: &mut SerialWriter,
    asic: u8,
    row: u8,
    col: u8,
    wait: Duration,
) -> Result<bool, Bzm2UartError> {
    let engine_address = logical_engine_address(row, col);
    let data = read_register_tdm_sync_stream(
        reader,
        writer,
        asic,
        engine_address,
        ENGINE_REG_END_NONCE,
        4,
        wait,
    )
    .await?;
    let end_nonce = u32::from_le_bytes(data.try_into().unwrap());
    Ok(end_nonce == DISCOVERED_ENGINE_END_NONCE)
}

pub async fn discover_engine_map_stream(
    reader: &mut SerialReader,
    writer: &mut SerialWriter,
    asic: u8,
    tdm_prediv_raw: u32,
    tdm_counter: u8,
    restore: Bzm2TdmControl,
    wait: Duration,
) -> Result<Bzm2DiscoveredEngineMap, Bzm2UartError> {
    set_tdm_enabled_stream(writer, tdm_prediv_raw, tdm_counter, true).await?;

    let result = async {
        let mut present = Vec::new();
        let mut missing = Vec::new();

        for (row, col) in physical_engine_coordinates() {
            let coordinate = Bzm2EngineCoordinate::new(row, col);
            if detect_engine_stream(reader, writer, asic, row, col, wait).await? {
                present.push(coordinate);
            } else {
                missing.push(coordinate);
            }
        }

        Ok(Bzm2DiscoveredEngineMap {
            asic,
            present,
            missing,
        })
    }
    .await;

    // PUT THE CHAIN BACK THE WAY IT RUNS, on success and on failure alike.
    //
    // This used to write the sweep's own slot settings with TDM OFF. On a
    // chain that mines in TDM that is not "tidying up": results and the sensor
    // stream both ride TDM slots, so the chain went silent with no error, the
    // die rows aged out, and the monitor scrammed the board, measured on hardware.
    // The error path matters most here -- that
    // run's discovery had FAILED, and failure is the case nobody re-checks.
    let restore_result = set_tdm_control_stream(writer, restore).await;
    match (result, restore_result) {
        (Ok(map), Ok(())) => Ok(map),
        (Err(err), _) => Err(err),
        (Ok(_), Err(err)) => Err(err),
    }
}

/// Confirm the part's ON-DIE protection is actually armed, from its own stream.
///
/// # Why a register read-back would not settle this
///
/// `configure_dts_vs_stream` broadcasts the thermal trip code and both
/// under-voltage shutdown thresholds to every device on the chain and then
/// proceeds. That is the silicon's own last-resort protection — the thing that
/// acts when software is already too late — and until now nothing confirmed a
/// single device had taken it. A broadcast write is acknowledged by the bus,
/// not by a hundred parts.
///
/// Reading the registers back would ask the same synchronous register path
/// that wrote them. The sensor STREAM is a different mechanism: the part
/// reports, asynchronously and unprompted, whether its thermal and voltage
/// sensors are enabled. Those bits are set by the configuration this is
/// checking, so a frame carrying them is the device saying the arming took, in
/// a message we did not ask it for.
///
/// Returns the ASIC that answered, so a caller can say WHICH device confirmed
/// rather than implying all hundred did.
pub async fn confirm_on_die_protection(
    reader: &mut SerialReader,
    generation: DtsVsGeneration,
    asic: u8,
    timeout: Duration,
) -> Result<u8, Bzm2UartError> {
    let frame = read_dts_vs_frame_stream(reader, generation, asic, timeout).await?;
    let (who, thermal, voltage) = match &frame {
        TdmDtsVsFrame::Gen2(f) => (f.asic, f.thermal_enabled, f.voltage_enabled),
        // The shorter generation carries a thermal enable and no voltage one,
        // so it can only ever confirm half of this. Saying so beats implying
        // the other half was checked.
        TdmDtsVsFrame::Gen1(f) => (f.asic, f.thermal_enabled, f.voltage_enabled),
    };
    if thermal && voltage {
        return Ok(who);
    }
    Err(Bzm2UartError::ProtectionNotArmed {
        asic: who,
        thermal,
        voltage,
    })
}

pub async fn read_dts_vs_frame_stream(
    reader: &mut SerialReader,
    generation: DtsVsGeneration,
    asic: u8,
    timeout: Duration,
) -> Result<TdmDtsVsFrame, Bzm2UartError> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut parser = TdmFrameParser::new(generation);
    let mut read_buf = [0u8; 256];

    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(Bzm2UartError::DtsVsTimeout { asic });
        }
        let remaining = deadline.saturating_duration_since(now);
        let read = tokio::time::timeout(remaining, reader.read(&mut read_buf))
            .await
            .map_err(|_| Bzm2UartError::DtsVsTimeout { asic })??;
        if read == 0 {
            return Err(Bzm2UartError::ShortResponse {
                expected: 1,
                actual: 0,
            });
        }
        for frame in parser.push(&read_buf[..read]) {
            if let TdmFrame::DtsVs(frame) = frame {
                let frame_asic = match frame {
                    TdmDtsVsFrame::Gen1(gen1) => gen1.asic,
                    TdmDtsVsFrame::Gen2(gen2) => gen2.asic,
                };
                // BROADCAST MEANS "WHOEVER ANSWERS", NOT "A DEVICE CALLED 0xFF".
                //
                // No frame ever carries the broadcast address: every device
                // reports under its OWN id. Matching `asic` literally against
                // BROADCAST_ASIC therefore never matches, and a caller asking
                // "has anything spoken?" would wait out its whole timeout with
                // a healthy chain streaming past it.
                //
                // This is the second time this exact assumption has been wrong
                // here -- a broadcast REGISTER READ is likewise answered by
                // every device under its own id, which is why DTS/VS bring-up
                // failed for the life of this project.
                if asic == BROADCAST_ASIC || frame_asic == asic {
                    return Ok(frame);
                }
            }
        }
    }
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

/// How long to wait for another device's answer to a broadcast read before
/// calling the chain done. Short: the siblings arrive back to back at 5 Mbaud,
/// so a gap this long means the round is over.
const BROADCAST_SIBLING_QUIET: Duration = Duration::from_millis(20);

async fn read_local_reg_u32_raw(
    reader: &mut SerialReader,
    writer: &mut SerialWriter,
    asic: u8,
    offset: u8,
) -> Result<u32, Bzm2UartError> {
    let request = encode_read_register(asic, NOTCH_REG, offset, 4);
    writer.write_all(&request).await?;
    writer.flush().await?;

    // BOUNDED AT THE READER, not at whichever caller remembered.
    //
    // This was a bare `read_exact`, and it is addressed to the broadcast id --
    // a request no single part owns the reply to. It sits under the hash
    // thread's select! loop, so awaiting here stops that thread entirely for
    // the life of the process: no dispatch, no result frames, no trip
    // detection, no shutdown. The daemon stays up and other chains keep
    // running, so the symptom is one silently dead chain rather than a dead
    // miner, which is the harder kind to notice.
    //
    // The rule already existed -- the enumeration probe and the diagnostic
    // handlers were bounded for exactly this failure shape -- and this path was
    // the gap in it. A later commit bounded a NEW call site and left the reader
    // unbounded, which is the same mistake a second time: the bound belongs
    // where the blocking happens, because that is the only place every caller
    // passes through.
    let mut response = [0u8; 6];
    // A REGISTER READ TIMES OUT AS A REGISTER READ. This reported NoopTimeout,
    // so the log said "timed out waiting for NOOP response"
    // when the bring-up's bandgap register read had timed out behind a
    // transmit backlog -- and the diagnosis started by looking for a NOOP.
    timeout(DEFAULT_REG_READ_TIMEOUT, reader.read_exact(&mut response))
        .await
        .map_err(|_| Bzm2UartError::TdmRegisterTimeout {
            asic,
            engine_address: NOTCH_REG,
            offset,
        })??;
    // A BROADCAST READ IS ANSWERED BY EVERY DEVICE, EACH UNDER ITS OWN ID.
    //
    // Measured 2026-09-21 on a quiet line, after the chain had been reset and
    // drained: one broadcast register read drew answers from 99 distinct ASIC
    // ids, and NOT ONE of them carried 0xff
    // (measured on hardware).
    //
    // Requiring the reply header to echo the broadcast address was therefore a
    // condition no chain could ever satisfy, and it failed every DTS/VS
    // bring-up we have ever run. It read as a framing fault because the id that
    // happened to arrive first varied with timing -- 0x3f, 0x47, 0x7e, 0x18,
    // 0x22, 0xd3 across runs -- which is exactly what "line noise" looks like,
    // and is why this was chased as a desync for a week.
    //
    // The opcode is still checked, and for a directed read the id still must
    // match. What is relaxed is only the case where we deliberately addressed
    // everyone.
    if asic == BROADCAST_ASIC {
        if response[1] != OPCODE_UART_READREG {
            return Err(Bzm2UartError::UnexpectedHeader {
                expected_asic: asic,
                expected_opcode: OPCODE_UART_READREG,
                actual_asic: response[0],
                actual_opcode: response[1],
            });
        }
        // Every other device is still answering the same question. Leave the
        // line clean rather than making the next transaction step over the
        // siblings -- that is the condition this whole attach path exists to
        // establish. Bounded, and a short read just means they are done.
        let mut sibling = [0u8; 6];
        while timeout(BROADCAST_SIBLING_QUIET, reader.read_exact(&mut sibling))
            .await
            .is_ok_and(|r| r.is_ok())
        {}
    } else {
        validate_response_header(asic, OPCODE_UART_READREG, &response)?;
    }
    Ok(u32::from_le_bytes(response[2..6].try_into().unwrap()))
}

fn legacy_temperature_c_to_tune_code(temperature_c: i32) -> u32 {
    let resolution_power = match THERMAL_SENSOR_RESOLUTION {
        10 => 1024.0_f32,
        8 => 256.0_f32,
        _ => 4096.0_f32,
    };
    (2048.0 / resolution_power + 4096.0 * (temperature_c as f32 + 293.8) / 631.8) as u32
}

fn legacy_voltage_mv_to_tune_code(voltage_mv: u32) -> u32 {
    let resolution_power = match VOLTAGE_SENSOR_RESOLUTION {
        12 => 4096.0_f32,
        10 => 1024.0_f32,
        8 => 256.0_f32,
        _ => 16384.0_f32,
    };
    ((16384.0 / 6.0) * (2.5 * voltage_mv as f32 / 706.7 + 3.0 / resolution_power + 1.0)) as u32
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
    fn legacy_temperature_query_threshold_matches_legacy_formula_family() {
        assert_eq!(legacy_temperature_c_to_tune_code(115), 2650);
    }

    #[test]
    fn legacy_voltage_query_threshold_matches_legacy_formula_family() {
        assert_eq!(legacy_voltage_mv_to_tune_code(500), 7561);
    }

    #[test]
    fn default_asic_id_matches_legacy_value() {
        assert_eq!(DEFAULT_ASIC_ID, 0xfa);
    }

    /// A register read must not be able to block forever.
    ///
    /// The bound belongs at the reader, because that is the only place every
    /// caller passes through -- a bound added at one call site left the next
    /// one unprotected, twice. Nothing answers this PTY: the request goes out
    /// and no reply comes back, which is exactly what a broadcast-addressed
    /// read does, since no single part owns the reply to it.
    #[tokio::test]
    async fn a_register_read_that_is_never_answered_times_out() {
        let pty = openpty(None, None).unwrap();
        let serial_path = fs::read_link(format!("/proc/self/fd/{}", pty.slave.as_raw_fd()))
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let stream = SerialStream::new(&serial_path, 5_000_000).unwrap();
        let (mut reader, mut writer, _control) = stream.split();

        let started = std::time::Instant::now();
        let result = read_local_reg_u32_raw(&mut reader, &mut writer, BROADCAST_ASIC, 0x00).await;
        let elapsed = started.elapsed();

        assert!(
            result.is_err(),
            "an unanswered register read must not succeed"
        );
        assert!(
            elapsed < DEFAULT_REG_READ_TIMEOUT * 4,
            "returned after {elapsed:?}, which is not bounded by {DEFAULT_REG_READ_TIMEOUT:?}"
        );
        drop(pty);
    }

    /// A broadcast register read is answered by every device, under its OWN id.
    ///
    /// Bytes lifted verbatim from a capture taken on a quiet line, after the
    /// chain had been reset and drained:
    /// captured on hardware
    ///
    /// One broadcast read drew answers from 99 distinct ASIC ids there, and not
    /// one carried 0xff. Requiring the reply header to echo the broadcast
    /// address was a condition no chain could satisfy, and it failed every
    /// DTS/VS bring-up we ever ran. It read as a framing fault because the id
    /// arriving first varied with timing -- 0x3f, 0x47, 0x7e, 0x18, 0x22, 0xd3
    /// across runs -- which is what line noise looks like, and is why this was
    /// chased as a desync for a week.
    #[tokio::test]
    async fn a_broadcast_register_read_accepts_the_device_that_answers() {
        use tokio::io::AsyncWriteExt as _;

        let pty = openpty(None, None).unwrap();
        let serial_path = fs::read_link(format!("/proc/self/fd/{}", pty.slave.as_raw_fd()))
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let stream = SerialStream::new(&serial_path, 5_000_000).unwrap();
        let (mut reader, mut writer, _control) = stream.split();

        // Three consecutive devices answering one broadcast read. They report
        // the SAME value, which is why reading one and writing the result back
        // to all of them was the right operation all along -- only the header
        // check was wrong.
        let chain_reply: [u8; 18] = [
            0x48, 0x03, 0xf3, 0x02, 0x00, 0x00, // asic 72
            0x49, 0x03, 0xf3, 0x02, 0x00, 0x00, // asic 73
            0x4a, 0x03, 0xf3, 0x02, 0x00, 0x00, // asic 74
        ];
        let mut master = tokio::fs::File::from_std(unsafe {
            <std::fs::File as std::os::unix::io::FromRawFd>::from_raw_fd(pty.master.as_raw_fd())
        });
        master.write_all(&chain_reply).await.unwrap();
        master.flush().await.unwrap();

        let value =
            read_local_reg_u32_raw(&mut reader, &mut writer, BROADCAST_ASIC, LOCAL_REG_BANDGAP)
                .await
                .expect("a broadcast read must accept the device that answers");

        // The value the part actually reported, not one this test computed.
        assert_eq!(value, 0x0000_02f3);

        std::mem::forget(master);
        drop(pty);
    }

    /// A bring-up that FAILS must not leave the part streaming.
    ///
    /// This is the defect that silenced the control board's API for a whole
    /// handover window on 2026-09-18. `configure_dts_vs_stream` enabled the
    /// sensor stream as its FIRST action, then several writes later tried to
    /// read LOCAL_REG_BANDGAP -- on the line it had just filled with sensor
    /// traffic. The read failed every time we have ever run a handover
    /// (`expected asic 0xff opcode 0x3, got asic 0x3f opcode 0x3`), so the
    /// function returned Err and the caller fell back to lazy enable, while
    /// the hardware went on streaming at 13,850 frames/s with the daemon
    /// believing telemetry was off.
    ///
    /// Evidence: measured on hardware
    /// (397,516 frames in 28.7 s, zero HTTP responses in the same window).
    ///
    /// Nothing answers the read here, so bring-up fails -- which is the case
    /// under test. The enable must not have been written.
    #[test]
    fn broadcast_means_whoever_answers() {
        // A REGRESSION GUARD FOR AN ASSUMPTION THAT HAS NOW BEEN WRONG TWICE.
        //
        // No frame ever carries the broadcast address; every device reports
        // under its own id. A waiter that matched BROADCAST_ASIC literally
        // would sit out its whole timeout with a healthy chain streaming past
        // it -- which is exactly what happened: two confirmations added to
        // attach could only ever report "could not confirm", on hardware, with
        // a hundred devices talking.
        //
        // The same assumption broke a broadcast REGISTER READ, where every
        // device likewise answers under its own id, and that one cost the
        // whole life of DTS/VS bring-up.
        //
        // This asserts the property directly: the broadcast address is not a
        // device address, so nothing may be matched against it by equality.
        for id in [0u8, 1, 42, 70, 99] {
            assert_ne!(
                id, BROADCAST_ASIC,
                "a real device id must never equal the broadcast address"
            );
        }
        assert_eq!(BROADCAST_ASIC, 0xff);
    }

    #[tokio::test]
    async fn a_failed_bring_up_does_not_leave_the_stream_running() {
        use std::io::Read;
        use std::os::unix::io::FromRawFd;

        let pty = openpty(None, None).unwrap();
        let serial_path = fs::read_link(format!("/proc/self/fd/{}", pty.slave.as_raw_fd()))
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let stream = SerialStream::new(&serial_path, 5_000_000).unwrap();
        let (mut reader, mut writer, _control) = stream.split();

        let result =
            configure_dts_vs_stream(&mut writer, &mut reader, &Bzm2DtsVsConfig::default()).await;
        assert!(
            result.is_err(),
            "nothing answered the register read, so bring-up must fail"
        );

        // Everything the function put on the wire. Non-blocking, so the read
        // loop ends at WouldBlock rather than hanging on an idle pty.
        let master_fd = pty.master.as_raw_fd();
        nix::fcntl::fcntl(
            master_fd,
            nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
        )
        .unwrap();
        let mut master = unsafe { std::fs::File::from_raw_fd(master_fd) };
        let mut written = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match master.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => written.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
            if written.len() > 65536 {
                break;
            }
        }
        std::mem::forget(master);

        let enable = encode_write_register(
            BROADCAST_ASIC,
            NOTCH_REG,
            LOCAL_REG_UART_TX,
            &DTS_VS_UART_TX_ENABLE.to_le_bytes(),
        );
        let bandgap_read = encode_read_register(BROADCAST_ASIC, NOTCH_REG, LOCAL_REG_BANDGAP, 4);

        let find = |needle: &[u8]| written.windows(needle.len()).position(|w| w == needle);

        assert!(
            find(&bandgap_read).is_some(),
            "the bandgap read should have been attempted; the sequence changed"
        );
        assert!(
            find(&enable).is_none(),
            "THE BUG: bring-up failed but the TX-enable had already been written, \
             so the part is streaming while the daemon believes telemetry is off"
        );

        drop(pty);
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

    #[test]
    fn discovered_engine_map_counts_entries() {
        let map = Bzm2DiscoveredEngineMap {
            asic: 2,
            present: vec![
                Bzm2EngineCoordinate::new(0, 0),
                Bzm2EngineCoordinate::new(1, 0),
            ],
            missing: vec![Bzm2EngineCoordinate::new(0, 4)],
        };

        assert_eq!(map.present_count(), 2);
        assert_eq!(map.missing_count(), 1);
    }

    #[tokio::test]
    async fn read_register_tdm_sync_decodes_engine_response() {
        let pty = openpty(None, None).unwrap();
        let master = pty.master;
        let slave = pty.slave;
        let serial_path = fs::read_link(format!("/proc/self/fd/{}", slave.as_raw_fd()))
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let engine_address = logical_engine_address(3, 4);

        let emulator = std::thread::spawn(move || {
            let mut file = fs::File::from(master);
            let mut request = [0u8; 8];
            file.read_exact(&mut request).unwrap();
            assert_eq!(
                request.to_vec(),
                encode_read_register(2, engine_address, ENGINE_REG_END_NONCE, 4)
            );
            file.write_all(&[2, OPCODE_UART_READREG, 0xfe, 0xff, 0xff, 0xff])
                .unwrap();
            file.flush().unwrap();
            std::thread::sleep(Duration::from_millis(20));
        });

        let stream = SerialStream::new(&serial_path, 5_000_000).unwrap();
        let (reader, writer, _control) = stream.split();
        let mut uart = Bzm2UartController::new(reader, writer);
        let data = uart
            .read_register_tdm_sync(
                2,
                engine_address,
                ENGINE_REG_END_NONCE,
                4,
                Duration::from_millis(100),
            )
            .await
            .unwrap();
        assert_eq!(data, vec![0xfe, 0xff, 0xff, 0xff]);

        emulator.join().unwrap();
    }

    #[tokio::test]
    async fn discover_engine_map_scans_physical_coordinates() {
        let pty = openpty(None, None).unwrap();
        let master = pty.master;
        let slave = pty.slave;
        let serial_path = fs::read_link(format!("/proc/self/fd/{}", slave.as_raw_fd()))
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let present = std::collections::BTreeSet::from([(0u8, 0u8), (19u8, 10u8)]);
        let prediv = 0x0f;
        let counter = 16;

        let emulator = std::thread::spawn(move || {
            let mut file = fs::File::from(master);
            let expected_enable = encode_write_register(
                BROADCAST_ASIC,
                NOTCH_REG,
                LOCAL_REG_UART_TDM_CTL,
                &encode_tdm_control(prediv, counter, true).to_le_bytes(),
            );
            let mut enable_request = vec![0u8; expected_enable.len()];
            file.read_exact(&mut enable_request).unwrap();
            assert_eq!(enable_request, expected_enable);

            for (row, col) in physical_engine_coordinates() {
                let mut request = [0u8; 8];
                file.read_exact(&mut request).unwrap();
                assert_eq!(
                    request.to_vec(),
                    encode_read_register(
                        1,
                        logical_engine_address(row, col),
                        ENGINE_REG_END_NONCE,
                        4
                    )
                );
                let value = if present.contains(&(row, col)) {
                    DISCOVERED_ENGINE_END_NONCE
                } else {
                    0
                };
                let mut response = vec![1, OPCODE_UART_READREG];
                response.extend_from_slice(&value.to_le_bytes());
                file.write_all(&response).unwrap();
                file.flush().unwrap();
            }

            let expected_restore = encode_write_register(
                BROADCAST_ASIC,
                NOTCH_REG,
                LOCAL_REG_UART_TDM_CTL,
                &Bzm2TdmControl::operating(&[0, 1]).encode().to_le_bytes(),
            );
            let mut restore_request = vec![0u8; expected_restore.len()];
            file.read_exact(&mut restore_request).unwrap();
            assert_eq!(restore_request, expected_restore);
            std::thread::sleep(Duration::from_millis(20));
        });

        let stream = SerialStream::new(&serial_path, 5_000_000).unwrap();
        let (reader, writer, _control) = stream.split();
        let mut uart = Bzm2UartController::new(reader, writer);
        let discovery = uart
            .discover_engine_map(
                1,
                prediv,
                counter,
                Bzm2TdmControl::operating(&[0, 1]),
                Duration::from_millis(100),
            )
            .await
            .unwrap();

        assert_eq!(discovery.present_count(), 2);
        assert_eq!(
            discovery.missing_count(),
            physical_engine_coordinates().len() - 2
        );
        assert_eq!(
            discovery.present,
            vec![
                Bzm2EngineCoordinate::new(0, 0),
                Bzm2EngineCoordinate::new(19, 10)
            ]
        );

        emulator.join().unwrap();
    }

    /// A discovery that FAILS must still hand the chain back in the TDM state
    /// it runs in, because the error path is the one nobody watches.
    ///
    /// Measured on hardware: the chain was
    /// streaming in TDM at 0xfec9 -- the value the vendor daemon logged setting --
    /// when a discovery request failed after 1.28 s.
    /// Its tail wrote TDM OFF, the sensor stream stopped on the wire within a
    /// second, and every die row went stale until the monitor scrammed the
    /// board. Nothing in the error was about temperature; the stream was simply
    /// never turned back on.
    #[tokio::test]
    async fn a_failed_discovery_still_returns_the_chain_to_its_operating_tdm() {
        let pty = openpty(None, None).unwrap();
        let master = pty.master;
        let slave = pty.slave;
        let serial_path = fs::read_link(format!("/proc/self/fd/{}", slave.as_raw_fd()))
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let (prediv, counter) = (0x0f, 16);
        let chain: Vec<u8> = (0..100).collect();
        let operating = Bzm2TdmControl::operating(&chain);

        let emulator = std::thread::spawn(move || {
            let mut file = fs::File::from(master);
            let expected_enable = encode_write_register(
                BROADCAST_ASIC,
                NOTCH_REG,
                LOCAL_REG_UART_TDM_CTL,
                &encode_tdm_control(prediv, counter, true).to_le_bytes(),
            );
            let mut enable = vec![0u8; expected_enable.len()];
            file.read_exact(&mut enable).unwrap();
            assert_eq!(enable, expected_enable);
            // The first engine read goes unanswered, so discovery times out.
            let mut first_read = [0u8; 8];
            file.read_exact(&mut first_read).unwrap();

            let expected_restore = encode_write_register(
                BROADCAST_ASIC,
                NOTCH_REG,
                LOCAL_REG_UART_TDM_CTL,
                &operating.encode().to_le_bytes(),
            );
            let mut tail = vec![0u8; expected_restore.len()];
            file.read_exact(&mut tail).unwrap();
            assert_eq!(
                tail, expected_restore,
                "discovery's last write must put the chain back in its operating TDM"
            );
            std::thread::sleep(Duration::from_millis(20));
        });

        let stream = SerialStream::new(&serial_path, 5_000_000).unwrap();
        let (reader, writer, _control) = stream.split();
        let mut uart = Bzm2UartController::new(reader, writer);
        let result = uart
            .discover_engine_map(1, prediv, counter, operating, Duration::from_millis(20))
            .await;

        assert!(
            matches!(result, Err(Bzm2UartError::TdmRegisterTimeout { .. })),
            "the unanswered read must surface as the failure, not be masked by the restore: {result:?}"
        );
        emulator.join().unwrap();
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
