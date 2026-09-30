use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::asic::hash_thread::HashThreadError;
use crate::transport::serial::{SerialReader, SerialWriter};

use super::super::protocol::{OPCODE_UART_READREG, encode_read_register};

/// Bound for a single diagnostic UART read. The actor is one task, so a silent
/// or short-answering chip must not wedge it (including a pending `Shutdown`) on
/// an unbounded `read_exact`; on expiry the diagnostic fails instead of hanging.
const DIAGNOSTIC_READ_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) async fn read_register(
    reader: &mut SerialReader,
    writer: &mut SerialWriter,
    asic: u8,
    engine_address: u16,
    offset: u8,
    count: u8,
) -> Result<Vec<u8>, HashThreadError> {
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
