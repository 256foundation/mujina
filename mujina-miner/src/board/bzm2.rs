use std::env;
use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::watch;

use super::{Board, BoardError, BoardInfo, VirtualBoardDescriptor};
use crate::{
    api_client::types::BoardState,
    asic::{
        bzm2::{Bzm2Thread, Bzm2ThreadConfig, Bzm2ThreadHandle},
        hash_thread::HashThread,
    },
    transport::SerialStream,
};

const DEFAULT_BAUD_RATE: u32 = 5_000_000;
const DEFAULT_DISPATCH_INTERVAL_MS: u64 = 500;
const DEFAULT_NOMINAL_HASHRATE_THS: f64 = 40.0;

#[derive(Debug, Clone)]
pub struct Bzm2VirtualDeviceConfig {
    pub serial_paths: Vec<String>,
    pub baud_rate: u32,
    pub timestamp_count: u8,
    pub nonce_gap: u32,
    pub dispatch_interval: Duration,
    pub nominal_hashrate_ths: f64,
}

impl Bzm2VirtualDeviceConfig {
    pub fn from_env() -> Option<Self> {
        let raw_paths = env::var("MUJINA_BZM2_SERIAL")
            .ok()
            .or_else(|| env::var("MUJINA_BZM2_SERIAL_PATHS").ok())?;

        let serial_paths: Vec<String> = raw_paths
            .split(',')
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(ToOwned::to_owned)
            .collect();
        if serial_paths.is_empty() {
            return None;
        }

        let baud_rate = env::var("MUJINA_BZM2_BAUD")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(DEFAULT_BAUD_RATE);
        let timestamp_count = env::var("MUJINA_BZM2_TIMESTAMP_COUNT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(crate::asic::bzm2::protocol::DEFAULT_TIMESTAMP_COUNT);
        let nonce_gap = env::var("MUJINA_BZM2_NONCE_GAP")
            .ok()
            .and_then(|value| {
                let trimmed = value.trim();
                if let Some(hex) = trimmed.strip_prefix("0x") {
                    u32::from_str_radix(hex, 16).ok()
                } else {
                    trimmed.parse().ok()
                }
            })
            .unwrap_or(crate::asic::bzm2::protocol::DEFAULT_NONCE_GAP);
        let dispatch_interval = Duration::from_millis(
            env::var("MUJINA_BZM2_DISPATCH_MS")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(DEFAULT_DISPATCH_INTERVAL_MS),
        );
        let nominal_hashrate_ths = env::var("MUJINA_BZM2_HASHRATE_THS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(DEFAULT_NOMINAL_HASHRATE_THS);

        Some(Self {
            serial_paths,
            baud_rate,
            timestamp_count,
            nonce_gap,
            dispatch_interval,
            nominal_hashrate_ths,
        })
    }

    pub fn device_id(&self) -> String {
        let suffix = self
            .serial_paths
            .iter()
            .map(|path| {
                Path::new(path)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(path)
                    .chars()
                    .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("-");
        format!("bzm2-{}", suffix)
    }
}

pub struct Bzm2Board {
    config: Bzm2VirtualDeviceConfig,
    shutdown_handles: Vec<Bzm2ThreadHandle>,
    state_tx: watch::Sender<BoardState>,
}

impl Bzm2Board {
    pub fn new(config: Bzm2VirtualDeviceConfig, state_tx: watch::Sender<BoardState>) -> Self {
        Self {
            config,
            shutdown_handles: Vec::new(),
            state_tx,
        }
    }
}

#[async_trait]
impl Board for Bzm2Board {
    fn board_info(&self) -> BoardInfo {
        BoardInfo {
            model: "BZM2".into(),
            firmware_version: None,
            serial_number: Some(self.config.device_id()),
        }
    }

    async fn shutdown(&mut self) -> Result<(), BoardError> {
        for handle in &self.shutdown_handles {
            handle.shutdown();
        }
        self.shutdown_handles.clear();
        let _ = self.state_tx.send_modify(|state| {
            for thread in &mut state.threads {
                thread.is_active = false;
                thread.hashrate = 0;
            }
        });
        Ok(())
    }

    async fn create_hash_threads(&mut self) -> Result<Vec<Box<dyn HashThread>>, BoardError> {
        let mut threads: Vec<Box<dyn HashThread>> = Vec::new();
        let mut thread_states = Vec::new();

        for (index, serial_path) in self.config.serial_paths.iter().enumerate() {
            let stream = SerialStream::new(serial_path, self.config.baud_rate).map_err(|err| {
                BoardError::InitializationFailed(format!(
                    "Failed to open BZM2 serial transport {}: {}",
                    serial_path, err
                ))
            })?;
            let (reader, writer, control) = stream.split();
            let thread_name = format!("BZM2 UART {}", index);
            let mut config = Bzm2ThreadConfig::new(serial_path.clone(), self.config.baud_rate);
            config.timestamp_count = self.config.timestamp_count;
            config.nonce_gap = self.config.nonce_gap;
            config.dispatch_interval = self.config.dispatch_interval;
            config.nominal_hashrate_ths = self.config.nominal_hashrate_ths;

            let thread = Bzm2Thread::new(thread_name.clone(), reader, writer, control, config);
            self.shutdown_handles.push(thread.shutdown_handle());
            thread_states.push(crate::api_client::types::ThreadState {
                name: thread_name,
                hashrate: 0,
                is_active: false,
            });
            threads.push(Box::new(thread));
        }

        let _ = self.state_tx.send_modify(|state| {
            state.threads = thread_states.clone();
        });

        Ok(threads)
    }
}

async fn create_bzm2_board()
-> crate::error::Result<(Box<dyn Board + Send>, super::BoardRegistration)> {
    let config = Bzm2VirtualDeviceConfig::from_env().ok_or_else(|| {
        crate::error::Error::Config("BZM2 not configured (MUJINA_BZM2_SERIAL not set)".into())
    })?;

    let serial = config.device_id();
    let initial_state = BoardState {
        name: serial.clone(),
        model: "BZM2".into(),
        serial: Some(serial),
        ..Default::default()
    };
    let (state_tx, state_rx) = watch::channel(initial_state);

    let board = Bzm2Board::new(config, state_tx);
    let registration = super::BoardRegistration { state_rx };
    Ok((Box::new(board), registration))
}

inventory::submit! {
    VirtualBoardDescriptor {
        device_type: "bzm2",
        name: "BZM2",
        create_fn: || Box::pin(create_bzm2_board()),
    }
}



