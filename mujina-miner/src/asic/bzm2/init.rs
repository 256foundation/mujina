//! BZM2 data-port initialization helpers.
//!
//! This module performs the board-time transport probe that happens before the
//! hashing thread takes ownership of the UART. Initialization here uses the
//! real protocol codec and returns a ready-to-use framed transport on success.

use anyhow::{Context, Result, anyhow, bail};
use futures::SinkExt;
use tokio::io::AsyncReadExt;
use tokio::time::{self, Duration};
use tokio_stream::StreamExt;
use tokio_util::codec::{FramedRead, FramedWrite};

use super::{
    Bzm2Protocol, FrameCodec, HexBytes, ReadRegData, Response,
    protocol::{DEFAULT_ASIC_ID, NOOP_STRING},
};
use crate::transport::serial::{SerialControl, SerialReader, SerialStream, SerialWriter};

/// Default BZM2 UART baud rate used by the BIRDS data port.
pub const DEFAULT_BZM2_DATA_BAUD: u32 = 5_000_000;

/// Default timeout for each initialization request/response step.
pub const DEFAULT_IO_TIMEOUT: Duration = Duration::from_secs(2);

/// Result of probing one ASIC during board initialization.
#[derive(Debug, Clone, Copy)]
pub struct ProbeResult {
    /// Logical ASIC index that was probed.
    pub logical_asic: u8,
    /// Hardware UART ID observed on the response path.
    pub asic_hw_id: u8,
    /// Raw `ASIC_ID` register value returned by the chip.
    pub asic_id: u32,
}

/// Framed BZM2 data-port transport that has already passed initialization.
pub struct InitializedDataPort {
    /// Probe metadata collected during initialization.
    pub probe: ProbeResult,
    /// Decoded response stream for subsequent hashing logic.
    pub reader: FramedRead<SerialReader, FrameCodec>,
    /// Encoded command sink for subsequent hashing logic.
    pub writer: FramedWrite<SerialWriter, FrameCodec>,
    /// Control handle associated with the serial data port.
    pub control: SerialControl,
}

fn expect_noop_response(response: Response) -> Result<u8> {
    match response {
        Response::Noop {
            asic_hw_id,
            signature,
        } if signature == *NOOP_STRING => Ok(asic_hw_id),
        Response::Noop { signature, .. } => {
            bail!("NOOP signature mismatch: got {:02x?}", signature)
        }
        other => bail!("expected NOOP response, got {:?}", other),
    }
}

fn expect_asic_id_response(expected_asic_hw_id: u8, response: Response) -> Result<u32> {
    match response {
        Response::ReadReg {
            asic_hw_id,
            data: ReadRegData::U32(asic_id),
        } if asic_hw_id == expected_asic_hw_id => Ok(asic_id),
        Response::ReadReg { asic_hw_id, data } => bail!(
            "READREG(ASIC_ID) response mismatch: expected ASIC 0x{expected_asic_hw_id:02X}, got ASIC 0x{asic_hw_id:02X} with payload {:?}",
            data
        ),
        other => bail!("expected READREG(ASIC_ID) response, got {:?}", other),
    }
}

async fn next_response(
    reader: &mut FramedRead<SerialReader, FrameCodec>,
    timeout: Duration,
    context: &str,
) -> Result<Response> {
    let response = time::timeout(timeout, reader.next())
        .await
        .with_context(|| format!("timeout waiting for {context}"))?
        .transpose()
        .with_context(|| format!("read error while waiting for {context}"))?
        .ok_or_else(|| anyhow!("BZM2 response stream closed while waiting for {context}"))?;
    Ok(response)
}

/// Open, probe, and return an initialized BZM2 data port using default
/// transport settings.
pub async fn initialize_data_port(
    serial_port: &str,
    logical_asic: u8,
) -> Result<InitializedDataPort> {
    initialize_data_port_with_options(
        serial_port,
        logical_asic,
        DEFAULT_BZM2_DATA_BAUD,
        DEFAULT_IO_TIMEOUT,
    )
    .await
}

/// Open, probe, and return an initialized BZM2 data port using explicit
/// transport settings.
pub async fn initialize_data_port_with_options(
    serial_port: &str,
    logical_asic: u8,
    baud: u32,
    timeout: Duration,
) -> Result<InitializedDataPort> {
    let protocol = Bzm2Protocol::new();
    let serial = SerialStream::new(serial_port, baud)
        .with_context(|| format!("failed to open serial port {}", serial_port))?;
    let (mut raw_reader, writer, control) = serial.split();

    // Reset/power-up can leave transient bytes on the data UART. Drain any
    // pending bytes before issuing the first command.
    drain_input_noise(&mut raw_reader).await;

    let mut reader = FramedRead::new(raw_reader, FrameCodec::default());
    let mut writer = FramedWrite::new(writer, FrameCodec::default());

    writer
        .send(protocol.noop(DEFAULT_ASIC_ID))
        .await
        .context("failed to send NOOP")?;
    let noop_response = next_response(&mut reader, timeout, "NOOP response").await?;
    let asic_hw_id = expect_noop_response(noop_response)?;

    writer
        .send(protocol.read_asic_id(asic_hw_id))
        .await
        .context("failed to send READREG(ASIC_ID)")?;
    let asic_id_response = next_response(&mut reader, timeout, "READREG(ASIC_ID) response").await?;
    let asic_id = expect_asic_id_response(asic_hw_id, asic_id_response)?;

    Ok(InitializedDataPort {
        probe: ProbeResult {
            logical_asic,
            asic_hw_id,
            asic_id,
        },
        reader,
        writer,
        control,
    })
}

async fn drain_input_noise(reader: &mut SerialReader) {
    let mut scratch = [0u8; 256];
    loop {
        match time::timeout(Duration::from_millis(20), reader.read(&mut scratch)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                tracing::debug!(
                    bytes = n,
                    rx = %HexBytes(&scratch[..n]),
                    "BZM2 init drained residual input"
                );
                continue;
            }
            Ok(Err(_)) => break,
            Err(_elapsed) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expect_noop_response_accepts_expected_signature() {
        let asic_hw_id = expect_noop_response(Response::Noop {
            asic_hw_id: DEFAULT_ASIC_ID,
            signature: *NOOP_STRING,
        })
        .unwrap();
        assert_eq!(asic_hw_id, DEFAULT_ASIC_ID);
    }

    #[test]
    fn test_expect_noop_response_rejects_non_noop_response() {
        let error = expect_noop_response(Response::ReadReg {
            asic_hw_id: DEFAULT_ASIC_ID,
            data: ReadRegData::U32(0x1234_5678),
        })
        .expect_err("non-NOOP response must fail");
        assert!(error.to_string().contains("expected NOOP response"));
    }

    #[test]
    fn test_expect_asic_id_response_accepts_matching_u32_payload() {
        let asic_id = expect_asic_id_response(
            DEFAULT_ASIC_ID,
            Response::ReadReg {
                asic_hw_id: DEFAULT_ASIC_ID,
                data: ReadRegData::U32(0x1234_5678),
            },
        )
        .unwrap();
        assert_eq!(asic_id, 0x1234_5678);
    }

    #[test]
    fn test_expect_asic_id_response_rejects_mismatched_payload_type() {
        let error = expect_asic_id_response(
            DEFAULT_ASIC_ID,
            Response::ReadReg {
                asic_hw_id: DEFAULT_ASIC_ID,
                data: ReadRegData::U8(0x12),
            },
        )
        .expect_err("unexpected payload type must fail");
        assert!(
            error
                .to_string()
                .contains("READREG(ASIC_ID) response mismatch")
        );
    }
}
