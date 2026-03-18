//! Daemon lifecycle management for mujina-miner.
//!
//! This module handles the core daemon functionality including initialization,
//! task management, signal handling, and graceful shutdown.

use std::env;

use tokio::signal::unix::{self, SignalKind};
use tokio::sync::{mpsc, watch};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::api_client::types::MinerTelemetry;
use crate::tracing::prelude::*;
use crate::{
    api::{self, ApiConfig, commands::SchedulerCommand},
    asic::hash_thread::HashThread,
    backplane::Backplane,
    cpu_miner::CpuMinerConfig,
    job_source::{
        SourceCommand, SourceEvent,
        dummy::DummySource,
        forced_rate::{ForcedRateConfig, ForcedRateSource},
        stratum_v1::StratumV1Source,
    },
    scheduler::{self, SourceRegistration},
    stratum_v1::{PoolConfig as StratumPoolConfig, TcpConnector},
    transport::{CpuDeviceInfo, TransportEvent, UsbTransport, cpu as cpu_transport},
};

/// The main daemon.
pub struct Daemon {
    shutdown: CancellationToken,
    tracker: TaskTracker,
}

impl Daemon {
    /// Create a new daemon instance.
    pub fn new() -> Self {
        Self {
            shutdown: CancellationToken::new(),
            tracker: TaskTracker::new(),
        }
    }

    /// Run the daemon until shutdown is requested.
    pub async fn run(self) -> anyhow::Result<()> {
        // Create channels for component communication
        let (transport_tx, transport_rx) = mpsc::channel::<TransportEvent>(100);
        let (thread_tx, thread_rx) = mpsc::channel::<Box<dyn HashThread>>(10);
        let (source_reg_tx, source_reg_rx) = mpsc::channel::<SourceRegistration>(10);

        // Create and start USB transport discovery
        if std::env::var("MUJINA_USB_DISABLE").is_err() {
            let usb_transport = UsbTransport::new(transport_tx.clone());
            if let Err(e) = usb_transport.start_discovery(self.shutdown.clone()).await {
                error!("Failed to start USB discovery: {}", e);
            }
        } else {
            info!("USB discovery disabled (MUJINA_USB_DISABLE set)");
        }

        // Inject CPU miner virtual device if configured
        if let Some(config) = CpuMinerConfig::from_env() {
            info!(
                threads = config.thread_count,
                duty = config.duty_percent,
                "CPU miner enabled"
            );
            let event = TransportEvent::Cpu(cpu_transport::TransportEvent::CpuDeviceConnected(
                CpuDeviceInfo {
                    device_id: format!("cpu-{}x{}%", config.thread_count, config.duty_percent),
                    thread_count: config.thread_count,
                    duty_percent: config.duty_percent,
                },
            ));
            if let Err(e) = transport_tx.send(event).await {
                error!("Failed to send CPU miner event: {}", e);
            }
        }

        // Board registration channel: backplane forwards board
        // registrations here, the API server collects and serves them.
        let (board_reg_tx, board_reg_rx) = mpsc::channel(10);

        // Create and start backplane
        let mut backplane = Backplane::new(transport_rx, thread_tx, board_reg_tx);
        self.tracker.spawn({
            let shutdown = self.shutdown.clone();
            async move {
                tokio::select! {
                    result = backplane.run() => {
                        if let Err(e) = result {
                            error!("Backplane error: {}", e);
                        }
                    }
                    _ = shutdown.cancelled() => {}
                }

                backplane.shutdown_all_boards().await;
            }
        });

        // Create job source (Stratum v1 or Dummy)
        // Controlled by environment variables:
        // - MUJINA_POOL_URL: Pool address (e.g., stratum+tcp://localhost:3333)
        // - MUJINA_POOL_USER: Worker username (optional, defaults to "mujina-testing")
        // - MUJINA_POOL_PASS: Worker password (optional, defaults to "x")
        let (source_event_tx, source_event_rx) = mpsc::channel::<SourceEvent>(100);
        let (source_cmd_tx, source_cmd_rx) = mpsc::channel(10);

        if let Ok(pool_url) = env::var("MUJINA_POOL_URL") {
            // Use Stratum v1 source
            let pool_user =
                env::var("MUJINA_POOL_USER").unwrap_or_else(|_| "mujina-testing".to_string());
            let pool_pass = env::var("MUJINA_POOL_PASS").unwrap_or_else(|_| "x".to_string());

            let stratum_config = StratumPoolConfig {
                url: pool_url.clone(),
                username: pool_user,
                password: pool_pass,
                user_agent: "mujina-miner/0.1.0-alpha".to_string(),
            };

            // Optionally wrap with ForcedRateSource for testing
            if let Some(forced_rate_config) = ForcedRateConfig::from_env() {
                info!(
                    rate = %forced_rate_config.target_rate,
                    "Forced share rate wrapper enabled"
                );

                // Create inner channels (stratum <-> wrapper)
                let (inner_event_tx, inner_event_rx) = mpsc::channel::<SourceEvent>(100);
                let (inner_cmd_tx, inner_cmd_rx) = mpsc::channel::<SourceCommand>(10);

                let stratum_source = StratumV1Source::new(
                    stratum_config,
                    inner_cmd_rx,
                    inner_event_tx,
                    self.shutdown.clone(),
                    Box::new(TcpConnector::new(pool_url.clone())),
                );
                let stratum_name = stratum_source.name();

                // Spawn stratum source
                self.tracker.spawn(async move {
                    if let Err(e) = stratum_source.run().await {
                        error!("Stratum v1 source error: {}", e);
                    }
                });

                // Create and spawn wrapper (uses outer channels from above)
                let forced_rate = ForcedRateSource::new(
                    forced_rate_config,
                    inner_event_rx,
                    source_event_tx,
                    inner_cmd_tx,
                    source_cmd_rx,
                    self.shutdown.clone(),
                );

                source_reg_tx
                    .send(SourceRegistration {
                        name: format!("{} (forced-rate)", stratum_name),
                        url: Some(pool_url.clone()),
                        event_rx: source_event_rx,
                        command_tx: source_cmd_tx,
                    })
                    .await?;

                self.tracker.spawn(async move {
                    if let Err(e) = forced_rate.run().await {
                        error!("Forced rate wrapper error: {}", e);
                    }
                });
            } else {
                // Direct stratum source (no wrapper)
                let stratum_source = StratumV1Source::new(
                    stratum_config,
                    source_cmd_rx,
                    source_event_tx,
                    self.shutdown.clone(),
                    Box::new(TcpConnector::new(pool_url.clone())),
                );

                source_reg_tx
                    .send(SourceRegistration {
                        name: stratum_source.name(),
                        url: Some(pool_url),
                        event_rx: source_event_rx,
                        command_tx: source_cmd_tx,
                    })
                    .await?;

                self.tracker.spawn(async move {
                    if let Err(e) = stratum_source.run().await {
                        error!("Stratum v1 source error: {}", e);
                    }
                });
            }
        } else {
            // Use DummySource
            info!("Using dummy job source (set MUJINA_POOL_URL to use Stratum v1)");

            let dummy_source = DummySource::new(
                source_cmd_rx,
                source_event_tx,
                self.shutdown.clone(),
                tokio::time::Duration::from_secs(30),
            )?;

            source_reg_tx
                .send(SourceRegistration {
                    name: "dummy".into(),
                    url: None,
                    event_rx: source_event_rx,
                    command_tx: source_cmd_tx,
                })
                .await?;

            self.tracker.spawn(async move {
                if let Err(e) = dummy_source.run().await {
                    error!("DummySource error: {}", e);
                }
            });
        }

        // Miner state channel: scheduler publishes snapshots, API serves them.
        let (miner_telemetry_tx, miner_telemetry_rx) = watch::channel(MinerTelemetry::default());

        // Command channel: API sends commands, scheduler processes them.
        let (scheduler_cmd_tx, scheduler_cmd_rx) = mpsc::channel::<SchedulerCommand>(16);

        // Start the scheduler
        self.tracker.spawn(scheduler::task(
            self.shutdown.clone(),
            thread_rx,
            source_reg_rx,
            miner_telemetry_tx,
            scheduler_cmd_rx,
        ));

        // Start the API server
        self.tracker.spawn({
            let shutdown = self.shutdown.clone();
            async move {
                // ASCII 'M' (77) + 'U' (85) = 7785
                const API_PORT: u16 = 7785;

                let bind_addr = match env::var("MUJINA_API_LISTEN") {
                    Ok(addr) if addr.contains(':') => addr,
                    Ok(addr) => format!("{addr}:{API_PORT}"),
                    Err(_) => format!("127.0.0.1:{API_PORT}"),
                };
                let config = ApiConfig { bind_addr };
                if let Err(e) = api::serve(
                    config,
                    shutdown,
                    miner_telemetry_rx,
                    board_reg_rx,
                    scheduler_cmd_tx,
                )
                .await
                {
                    error!("API server error: {}", e);
                }
            }
        });

        self.tracker.close();

        info!("Started.");
        info!("For debugging, set RUST_LOG=mujina_miner=debug or trace.");

        // Install signal handlers
        let mut sigint = unix::signal(SignalKind::interrupt())?;
        let mut sigterm = unix::signal(SignalKind::terminate())?;

        // Wait for shutdown signal
        tokio::select! {
            _ = sigint.recv() => {
                info!("Received SIGINT.");
            },
            _ = sigterm.recv() => {
                info!("Received SIGTERM.");
            },
        }

        // Initiate shutdown
        self.shutdown.cancel();

        // Wait for all tasks to complete
        self.tracker.wait().await;
        info!("Exiting.");

        Ok(())
    }
}

impl Default for Daemon {
    fn default() -> Self {
        Self::new()
    }
}
