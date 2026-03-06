use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::transport::{SerialReader, SerialWriter};

use super::protocol::{
    BROADCAST_ASIC, OPCODE_UART_LOOPBACK, OPCODE_UART_NOOP, OPCODE_UART_READREG, encode_loopback,
    encode_multicast_write, encode_noop, encode_read_register, encode_write_job,
    encode_write_register,
};

pub const NOTCH_REG: u16 = 0x0fff;
pub const BROADCAST_GROUP_ASIC: u8 = BROADCAST_ASIC;

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
}
