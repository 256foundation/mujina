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

const LOCAL_REG_ASIC_ID: u8 = 0x0b;
const LOCAL_REG_UART_TDM_CTL: u8 = 0x07;
const LOCAL_REG_SLOW_CLK_DIV: u8 = 0x08;
const LOCAL_REG_UART_TX: u8 = 0x0a;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bzm2DtsVsConfig {
    pub tdm_interval: u8,
    pub thermal_trip_c: i32,
    pub voltage_ch0_shutdown_mv: u32,
    pub voltage_ch1_shutdown_mv: u32,
}

impl Default for Bzm2DtsVsConfig {
    fn default() -> Self {
        Self {
            tdm_interval: 1,
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

    pub async fn verify_noop_bz2(&mut self, asic: u8) -> Result<(), Bzm2UartError> {
        let data = self.noop(asic).await?;
        if data == *b"BZ2" {
            Ok(())
        } else {
            Err(Bzm2UartError::UnexpectedNoopPayload { asic, data })
        }
    }

    pub async fn verify_noop_bz2_with_timeout(
        &mut self,
        asic: u8,
        wait: Duration,
    ) -> Result<(), Bzm2UartError> {
        let data = self.noop_with_timeout(asic, wait).await?;
        if data == *b"BZ2" {
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
                .verify_noop_bz2_with_timeout(DEFAULT_ASIC_ID, probe_timeout)
                .await
                .is_err()
            {
                break;
            }
            self.assign_default_asic_id(next_id).await?;
            self.verify_noop_bz2(next_id).await?;
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
        wait: Duration,
    ) -> Result<Bzm2DiscoveredEngineMap, Bzm2UartError> {
        discover_engine_map_stream(
            &mut self.reader,
            &mut self.writer,
            asic,
            tdm_prediv_raw,
            tdm_counter,
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

pub async fn configure_dts_vs_stream(
    writer: &mut SerialWriter,
    reader: &mut SerialReader,
    config: &Bzm2DtsVsConfig,
) -> Result<(), Bzm2UartError> {
    // Enable thermal and voltage sensor messages on the UART TDM path.
    write_local_reg_u32_raw(writer, BROADCAST_ASIC, LOCAL_REG_UART_TX, 0x0f).await?;

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
        config.tdm_interval as u32,
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

    Ok(())
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
            if let TdmFrame::Register(frame) = frame {
                if frame.asic == asic {
                    return Ok(frame.data);
                }
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

    let disable_result = set_tdm_enabled_stream(writer, tdm_prediv_raw, tdm_counter, false).await;
    match (result, disable_result) {
        (Ok(map), Ok(())) => Ok(map),
        (Err(err), _) => Err(err),
        (Ok(_), Err(err)) => Err(err),
    }
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
                if frame_asic == asic {
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

async fn read_local_reg_u32_raw(
    reader: &mut SerialReader,
    writer: &mut SerialWriter,
    asic: u8,
    offset: u8,
) -> Result<u32, Bzm2UartError> {
    let request = encode_read_register(asic, NOTCH_REG, offset, 4);
    writer.write_all(&request).await?;
    writer.flush().await?;

    let mut response = [0u8; 6];
    reader.read_exact(&mut response).await?;
    validate_response_header(asic, OPCODE_UART_READREG, &response)?;
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

#[cfg(test)]
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

            let expected_disable = encode_write_register(
                BROADCAST_ASIC,
                NOTCH_REG,
                LOCAL_REG_UART_TDM_CTL,
                &encode_tdm_control(prediv, counter, false).to_le_bytes(),
            );
            let mut disable_request = vec![0u8; expected_disable.len()];
            file.read_exact(&mut disable_request).unwrap();
            assert_eq!(disable_request, expected_disable);
            std::thread::sleep(Duration::from_millis(20));
        });

        let stream = SerialStream::new(&serial_path, 5_000_000).unwrap();
        let (reader, writer, _control) = stream.split();
        let mut uart = Bzm2UartController::new(reader, writer);
        let discovery = uart
            .discover_engine_map(1, prediv, counter, Duration::from_millis(100))
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
}
