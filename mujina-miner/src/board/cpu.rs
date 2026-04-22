//! CPU hashboard implementation.
//!
//! Provides a virtual board that uses CPU cores for SHA-256 hashing.
//! See [`CpuMinerConfig`] for environment variable configuration.

use anyhow::Result;
use futures::future::BoxFuture;
use tokio::sync::watch;

use super::{BackplaneConnector, BoardInfo, VirtualBoardDescriptor};
use crate::{
    api_client::types::BoardTelemetry,
    asic::hash_thread::HashThread,
    cpu_miner::{CpuHashThread, CpuMinerConfig},
    transport::cpu::CpuDeviceInfo,
};

inventory::submit! {
    VirtualBoardDescriptor {
        device_type: "cpu_miner",
        name: "CPU Miner",
        create_fn: |info| Box::pin(create_cpu_board(info)),
    }
}

async fn create_cpu_board(device_info: CpuDeviceInfo) -> Result<BackplaneConnector> {
    let config = CpuMinerConfig {
        thread_count: device_info.thread_count,
        duty_percent: device_info.duty_percent,
    };

    let info = BoardInfo {
        model: "CPU Miner".into(),
        firmware_version: None,
        serial_number: Some(format!(
            "cpu-{}x{}%",
            config.thread_count, config.duty_percent
        )),
    };

    let initial_state = BoardTelemetry {
        name: info.serial_number.clone().unwrap(),
        model: info.model.clone(),
        serial: info.serial_number.clone(),
        ..Default::default()
    };
    let (telemetry_tx, telemetry_rx) = watch::channel(initial_state);

    let threads: Vec<Box<dyn HashThread>> = (0..config.thread_count)
        .map(|i| {
            Box::new(CpuHashThread::new(
                format!("CPU Core {i}"),
                config.duty_percent,
            )) as _
        })
        .collect();

    // Keep the telemetry sender alive until the board is shut down.
    // Without this, the watch channel closes immediately and the API
    // registry prunes the board as disconnected.
    let shutdown: BoxFuture<'static, ()> = Box::pin(async move {
        drop(telemetry_tx);
    });

    Ok(BackplaneConnector {
        info,
        threads,
        telemetry_rx,
        shutdown: Some(shutdown),
    })
}
