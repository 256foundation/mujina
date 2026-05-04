//! CPU hashboard implementation.
//!
//! Provides a virtual board that uses CPU cores for SHA-256 hashing.
//! See [`CpuMinerConfig`] for environment variable configuration.

use std::time::Duration;

use anyhow::{Result, anyhow};
use tokio::sync::watch;

use super::{BackplaneConnector, BoardInfo, VirtualBoardDescriptor};
use crate::{
    api_client::types::{BoardTelemetry, ThreadTelemetry},
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
        .ok_or_else(|| anyhow!("cpu miner not configured (MUJINA_CPU_MINER not set)"))?;

    let info = BoardInfo {
        model: "CPU Miner".into(),
        firmware_version: None,
        serial_number: Some(format!(
            "cpu-{}x{}%",
            config.thread_count, config.duty_percent
        )),
    };

    let cpu_threads: Vec<CpuHashThread> = (0..config.thread_count)
        .map(|i| CpuHashThread::new(format!("CPU Core {i}"), config.duty_percent))
        .collect();

    // Snapshot the thread names + live status handles before handing
    // ownership of the threads to the scheduler. The board's telemetry
    // task reads these to publish per-thread hashrate.
    let thread_handles: Vec<(String, _)> = cpu_threads
        .iter()
        .map(|t| (t.name().to_string(), t.status_handle()))
        .collect();

    let initial_threads: Vec<ThreadTelemetry> = thread_handles
        .iter()
        .map(|(name, _)| ThreadTelemetry {
            name: name.clone(),
            hashrate: 0,
            is_active: false,
        })
        .collect();

    let initial_state = BoardTelemetry {
        name: info.serial_number.clone().unwrap(),
        model: info.model.clone(),
        serial: info.serial_number.clone(),
        threads: initial_threads,
        ..Default::default()
    };
    let (telemetry_tx, telemetry_rx) = watch::channel(initial_state);

    // Periodically refresh BoardTelemetry from each thread's status.
    // The task owns telemetry_tx so the channel stays alive until the
    // board is shut down; the registry then evicts the board normally.
    let publisher = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let threads = thread_handles
                .iter()
                .map(|(name, status)| {
                    let s = status.read().unwrap();
                    ThreadTelemetry {
                        name: name.clone(),
                        hashrate: u64::from(s.hashrate),
                        is_active: s.is_active,
                    }
                })
                .collect();
            telemetry_tx.send_modify(|t| t.threads = threads);
        }
    });

    let threads: Vec<Box<dyn HashThread>> = cpu_threads
        .into_iter()
        .map(|t| Box::new(t) as Box<dyn HashThread>)
        .collect();

    Ok(BackplaneConnector {
        info,
        threads,
        telemetry_rx,
        shutdown: Some(Box::pin(async move { publisher.abort() })),
    })
}
