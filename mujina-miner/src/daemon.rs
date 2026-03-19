//! Daemon lifecycle management for mujina-miner.
//!
//! This module handles the core daemon functionality including initialization,
//! task management, signal handling, and graceful shutdown.

use tokio::signal::unix::{self, SignalKind};
use tokio::sync::{mpsc, watch};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::api_client::types::MinerState;
use crate::tracing::prelude::*;
use crate::{
    api::{self, ApiConfig, commands::SchedulerCommand},
    asic::hash_thread::HashThread,
    backplane::Backplane,
    config::Config,
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
    config: Config,
    shutdown: CancellationToken,
    tracker: TaskTracker,
}

impl Daemon {
    /// Create a new daemon instance with the provided configuration.
    pub fn new(config: Config) -> Self {
        Self {
            config,
            shutdown: CancellationToken::new(),
            tracker: TaskTracker::new(),
        }
    }

    /// Return a cancellation token that triggers a clean shutdown when cancelled.
    ///
    /// Call this before [`run`] (which consumes `self`), then cancel the token
    /// from a test or management interface to stop the daemon without a signal.
    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    /// Run the daemon until shutdown is requested.
    pub async fn run(self) -> anyhow::Result<()> {
        let config = self.config;

        // Create channels for component communication
        let (transport_tx, transport_rx) = mpsc::channel::<TransportEvent>(100);
        let (thread_tx, thread_rx) = mpsc::channel::<Box<dyn HashThread>>(10);
        let (source_reg_tx, source_reg_rx) = mpsc::channel::<SourceRegistration>(10);

        // Create and start USB transport discovery
        if config.backplane.usb_enabled {
            let usb_transport = UsbTransport::new(transport_tx.clone());
            if let Err(e) = usb_transport.start_discovery(self.shutdown.clone()).await {
                error!("Failed to start USB discovery: {}", e);
            }
        } else {
            info!("USB discovery disabled (backplane.usb_enabled = false)");
        }

        // Inject CPU miner virtual device if configured
        if config.boards.cpu_miner.enabled {
            let cpu_cfg = &config.boards.cpu_miner;
            info!(
                threads = cpu_cfg.threads,
                duty = cpu_cfg.duty_percent,
                "CPU miner enabled"
            );
            let event = TransportEvent::Cpu(cpu_transport::TransportEvent::CpuDeviceConnected(
                CpuDeviceInfo {
                    device_id: format!("cpu-{}x{}%", cpu_cfg.threads, cpu_cfg.duty_percent),
                    thread_count: cpu_cfg.threads,
                    duty_percent: cpu_cfg.duty_percent,
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

        // Create job source (Stratum v1 or Dummy).
        // pool.url in the config selects Stratum v1; absent means dummy source.
        let (source_event_tx, source_event_rx) = mpsc::channel::<SourceEvent>(100);
        let (source_cmd_tx, source_cmd_rx) = mpsc::channel(10);

        if let Some(pool_url) = config.pool.url.clone() {
            // Use Stratum v1 source
            let stratum_config = StratumPoolConfig {
                url: pool_url.clone(),
                username: config.pool.user.clone(),
                password: config.pool.password.clone(),
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
            info!("Using dummy job source (set pool.url or MUJINA__POOL__URL to use Stratum v1)");

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
        let (miner_state_tx, miner_state_rx) = watch::channel(MinerState::default());

        // Command channel: API sends commands, scheduler processes them.
        let (scheduler_cmd_tx, scheduler_cmd_rx) = mpsc::channel::<SchedulerCommand>(16);

        // Start the scheduler
        self.tracker.spawn(scheduler::task(
            self.shutdown.clone(),
            thread_rx,
            source_reg_rx,
            miner_state_tx,
            scheduler_cmd_rx,
        ));

        // Start the API server
        let api_listen = config.api.listen.clone();
        self.tracker.spawn({
            let shutdown = self.shutdown.clone();
            async move {
                let api_config = ApiConfig { bind_addr: api_listen };
                if let Err(e) = api::serve(
                    api_config,
                    shutdown,
                    miner_state_rx,
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

        // Wait for shutdown signal or programmatic cancellation
        tokio::select! {
            _ = sigint.recv() => {
                info!("Received SIGINT.");
            },
            _ = sigterm.recv() => {
                info!("Received SIGTERM.");
            },
            _ = self.shutdown.cancelled() => {
                info!("Shutdown requested programmatically.");
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
        Self::new(Config::default())
    }
}
