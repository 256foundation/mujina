//! The scheduler module manages the distribution of mining jobs to hash boards
//! and ASIC chips.
//!
//! This is a work-in-progress. It's currently the main and initial place where
//! functionality is added, after which the functionality is refactored out to
//! where it belongs.

use futures::sink::SinkExt;
use tokio::io::AsyncWriteExt;
use tokio::time::{self, Duration};
use tokio_serial::{self, SerialPortBuilderExt};
use tokio_util::codec::FramedWrite;
use tokio_util::sync::CancellationToken;

use crate::chip::bm13xx;
use crate::board::bitaxe;
use crate::tracing::prelude::*;

pub async fn task(running: CancellationToken) {
    trace!("Task started.");

    let data_port = tokio_serial::new(bitaxe::DATA_SERIAL, 115200)
        .open_native_async()
        .expect("failed to open data serial port");

    let mut framed = FramedWrite::new(data_port, bm13xx::FrameCodec);

    let mut control_port = tokio_serial::new(bitaxe::CONTROL_SERIAL, 115200)
        .open_native_async()
        .expect("failed to open control serial port");
    const RSTN_HI: &[u8] = &[0x07, 0x00, 0x00, 0x00, 0x06, 0x00, 0x01];
    control_port.write_all(&RSTN_HI).await.unwrap();
    control_port.flush().await.unwrap();

    while !running.is_cancelled() {
        let read_address = bm13xx::Command::ReadRegister {
            all: true,
            address: 0,
            register: bm13xx::Register::ChipAddress,
        };

        trace!("Writing to port.");
        if let Err(e) = framed.send(read_address).await {
            error!("Error {e} writing to port.");
        }

        // Sleep to avoid busy loop
        time::sleep(Duration::from_secs(1)).await;
    }

    trace!("Task stopped.");
}

#[cfg(test)]
mod tests {
    #[test]
    fn hello_world() {
        assert_eq!("Hello, world!", "Hello, world!");
    }
}
