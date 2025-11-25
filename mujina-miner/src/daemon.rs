//! Daemon lifecycle management for mujina-miner.
//!
//! This module handles the core daemon functionality including initialization,
//! task management, signal handling, and graceful shutdown.

use tokio::signal::unix::{self, SignalKind};
use tokio::sync::mpsc;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::tracing::prelude::*;
use crate::{
    api::{self, ApiConfig},
    backplane::Backplane,
    hash_thread::HashThread,
    job_source::{dummy::DummySource, stratum_v1::StratumV1Source, SourceEvent},
    scheduler::{self, SourceRegistration},
    stratum_v1::PoolConfig as StratumPoolConfig,
    transport::{TransportEvent, UsbTransport},
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
        let (thread_tx, thread_rx) = mpsc::channel::<Vec<Box<dyn HashThread>>>(10);
        let (source_reg_tx, source_reg_rx) = mpsc::channel::<SourceRegistration>(10);

        // Create and start USB transport discovery
        let usb_transport = UsbTransport::new(transport_tx.clone());
        if let Err(e) = usb_transport.start_discovery(self.shutdown.clone()).await {
            error!("Failed to start USB discovery: {}", e);
        }

        // Create and start backplane
        let mut backplane = Backplane::new(transport_rx, thread_tx);
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

        if let Ok(pool_url) = std::env::var("MUJINA_POOL_URL") {
            // Use Stratum v1 source
            let pool_user =
                std::env::var("MUJINA_POOL_USER").unwrap_or_else(|_| "mujina-testing".to_string());
            let pool_pass = std::env::var("MUJINA_POOL_PASS").unwrap_or_else(|_| "x".to_string());

            let stratum_config = StratumPoolConfig {
                url: pool_url,
                username: pool_user,
                password: pool_pass,
                user_agent: "mujina-miner/0.1.0-alpha".to_string(),
                suggested_difficulty: Some(1024),
            };

            let stratum_source = StratumV1Source::new(
                stratum_config,
                source_cmd_rx,
                source_event_tx,
                self.shutdown.clone(),
            );

            source_reg_tx
                .send(SourceRegistration {
                    name: "stratum-v1".into(),
                    event_rx: source_event_rx,
                    command_tx: source_cmd_tx,
                })
                .await?;

            self.tracker.spawn(async move {
                if let Err(e) = stratum_source.run().await {
                    error!("Stratum v1 source error: {}", e);
                }
            });
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

        // Start the scheduler
        self.tracker.spawn(scheduler::task(
            self.shutdown.clone(),
            thread_rx,
            source_reg_rx,
        ));

        // Start the API server
        self.tracker.spawn({
            let shutdown = self.shutdown.clone();
            async move {
                let config = ApiConfig::default();
                if let Err(e) = api::serve(config, shutdown).await {
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
