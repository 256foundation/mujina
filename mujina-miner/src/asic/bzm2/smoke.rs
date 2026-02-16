//! BZM2 UART smoke test helpers.
//!
//! Used for early bring-up to verify basic command/response on the ASIC UART.

use anyhow::{Context, Result, bail};
use futures::SinkExt;
use tokio::io::AsyncReadExt;
use tokio::time::{self, Duration};
use tokio_util::codec::FramedWrite;

use super::{
    Command, FrameCodec,
    protocol::{DEFAULT_ASIC_ID, NOOP_STRING, NOTCH_REG, local_reg},
};
use crate::transport::serial::SerialStream;

/// Default BZM2 UART baud rate used by BIRDS data port.
pub const DEFAULT_BZM2_DATA_BAUD: u32 = 5_000_000;

/// Default timeout for each request/response step.
pub const DEFAULT_IO_TIMEOUT: Duration = Duration::from_secs(2);

/// Output from the smoke test.
#[derive(Debug, Clone, Copy)]
pub struct SmokeResult {
    pub logical_asic: u8,
    pub asic_hw_id: u8,
    pub asic_id: u32,
}

/// Run NOOP + READREG(ASIC_ID) smoke test on a BZM2 UART port.
pub async fn run_smoke(serial_port: &str, logical_asic: u8) -> Result<SmokeResult> {
    run_smoke_with_options(
        serial_port,
        logical_asic,
        DEFAULT_BZM2_DATA_BAUD,
        DEFAULT_IO_TIMEOUT,
    )
    .await
}

/// Run smoke test with explicit baud and timeout.
pub async fn run_smoke_with_options(
    serial_port: &str,
    logical_asic: u8,
    baud: u32,
    timeout: Duration,
) -> Result<SmokeResult> {
    // Initial bring-up uses BZM2 default ASIC ID (0xFA) before ID assignment.
    let asic_hw_id = DEFAULT_ASIC_ID;

    let serial = SerialStream::new(serial_port, baud)
        .with_context(|| format!("failed to open serial port {}", serial_port))?;
    let (mut reader, writer, _control) = serial.split();
    let mut tx = FramedWrite::new(
        writer,
        FrameCodec::new(4).context("failed to construct BZM2 codec")?,
    );

    // Reset/power-up can leave transient bytes on the data UART. Drain any
    // pending bytes before issuing the first command.
    drain_input_noise(&mut reader).await;

    // Step 1: NOOP
    tx.send(Command::Noop { asic_hw_id })
        .await
        .context("failed to send NOOP")?;

    let mut noop_raw = [0u8; 5];
    time::timeout(timeout, reader.read_exact(&mut noop_raw))
        .await
        .context("timeout waiting for NOOP response")?
        .context("read error while waiting for NOOP response")?;

    let mut signature = [0u8; 3];
    signature.copy_from_slice(&noop_raw[2..5]);
    if signature != *NOOP_STRING {
        bail!(
            "NOOP signature mismatch: got {:02x?} (raw={:02x?})",
            signature,
            noop_raw
        );
    }

    // Step 2: READREG NOTCH_REG:LOCAL_REG_ASIC_ID
    tx.send(Command::read_reg_u32(
        asic_hw_id,
        NOTCH_REG,
        local_reg::ASIC_ID,
    ))
    .await
    .context("failed to send READREG(ASIC_ID)")?;

    let mut readreg_raw = [0u8; 6];
    time::timeout(timeout, reader.read_exact(&mut readreg_raw))
        .await
        .context("timeout waiting for READREG response")?
        .context("read error while waiting for READREG response")?;

    let asic_id = u32::from_le_bytes(
        readreg_raw[2..6]
            .try_into()
            .expect("slice is exactly 4 bytes"),
    );

    Ok(SmokeResult {
        logical_asic,
        asic_hw_id,
        asic_id,
    })
}

async fn drain_input_noise(reader: &mut crate::transport::serial::SerialReader) {
    let mut scratch = [0u8; 256];
    loop {
        match time::timeout(Duration::from_millis(20), reader.read(&mut scratch)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(_n)) => continue,
            Ok(Err(_)) => break,
            Err(_elapsed) => break,
        }
    }
}
