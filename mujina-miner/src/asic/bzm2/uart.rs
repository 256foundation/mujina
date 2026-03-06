use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::transport::{SerialReader, SerialWriter};

use super::protocol::{
    BROADCAST_ASIC, DtsVsGeneration, OPCODE_UART_LOOPBACK, OPCODE_UART_NOOP, OPCODE_UART_READREG,
    TdmDtsVsFrame, TdmFrame, TdmFrameParser, encode_loopback, encode_multicast_write, encode_noop,
    encode_read_register, encode_write_job, encode_write_register,
};

pub const NOTCH_REG: u16 = 0x0fff;
pub const BROADCAST_GROUP_ASIC: u8 = BROADCAST_ASIC;
pub const DEFAULT_DTS_VS_QUERY_TIMEOUT: Duration = Duration::from_secs(2);

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

    #[error("timed out waiting for DTS/VS frame from ASIC {asic:#x}")]
    DtsVsTimeout { asic: u8 },
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
}
