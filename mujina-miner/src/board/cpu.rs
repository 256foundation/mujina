//! CPU hashboard implementation.
//!
//! Provides a virtual board that uses CPU cores for SHA-256 hashing.
//! See [`CpuMinerConfig`] for environment variable configuration.

use anyhow::{Result, anyhow};
use tokio::sync::watch;

use super::{BackplaneConnector, BoardInfo, VirtualBoardDescriptor};
use crate::{
    api_client::types::BoardTelemetry,
    asic::hash_thread::HashThread,
    cpu_miner::{CpuHashThread, CpuMinerConfig},
};

inventory::submit! {
    VirtualBoardDescriptor {
        device_type: "cpu_miner",
        name: "CPU Miner",
        create_fn: || Box::pin(create_cpu_board()),
    }
}

async fn create_cpu_board() -> Result<BackplaneConnector> {
    let config = CpuMinerConfig::from_env()
        .ok_or_else(|| anyhow!("cpu miner not configured (MUJINA_CPUMINER_THREADS not set)"))?;

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
    let (_telemetry_tx, telemetry_rx) = watch::channel(initial_state);

    let threads: Vec<Box<dyn HashThread>> = (0..config.thread_count)
        .map(|i| {
            Box::new(CpuHashThread::new(
                format!("CPU Core {i}"),
                config.duty_percent,
            )) as _
        })
        .collect();

    Ok(BackplaneConnector {
        info,
        threads,
        telemetry_rx,
        command_tx: None,
        shutdown: None,
    })
}
