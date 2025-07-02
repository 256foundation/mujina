//! Daemon lifecycle management for mujina-miner.
//!
//! This module handles the core daemon functionality including initialization,
//! task management, signal handling, and graceful shutdown.

use tokio::signal::unix::{self, SignalKind};
use tokio::sync::mpsc;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::tracing::prelude::*;
use crate::{
    board::Board,
    board_manager::BoardManager,
    scheduler,
    transport::{TransportEvent, UsbTransport},
};

/// The main daemon that coordinates all mining operations.
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
        let (board_tx, board_rx) = mpsc::channel::<Box<dyn Board + Send>>(10);

        // Create and start USB transport discovery
        let usb_transport = UsbTransport::new(transport_tx.clone());
        self.tracker.spawn({
            let shutdown = self.shutdown.clone();
            async move {
                if let Err(e) = usb_transport.start_discovery().await {
                    error!("USB discovery failed: {}", e);
                }
                // Keep the transport task alive until shutdown
                shutdown.cancelled().await;
            }
        });

        // Create and start board manager
        let mut board_manager = BoardManager::new(transport_rx, board_tx);
        self.tracker.spawn({
            let shutdown = self.shutdown.clone();
            async move {
                tokio::select! {
                    result = board_manager.run() => {
                        if let Err(e) = result {
                            error!("Board manager error: {}", e);
                        }
                    }
                    _ = shutdown.cancelled() => {
                        debug!("Board manager shutting down");
                    }
                }
            }
        });

        // Start the scheduler with board receiver
        self.tracker
            .spawn(scheduler::task(self.shutdown.clone(), board_rx));
        self.tracker.close();

        info!("Started.");
        info!(
            "For hardware debugging, set RUST_LOG=mujina_miner=trace to see \
             all serial communication"
        );

        // Install signal handlers
        let mut sigint = unix::signal(SignalKind::interrupt())?;
        let mut sigterm = unix::signal(SignalKind::terminate())?;

        // Wait for shutdown signal
        tokio::select! {
            _ = sigint.recv() => {
                info!("Received SIGINT");
            },
            _ = sigterm.recv() => {
                info!("Received SIGTERM");
            },
        }

        // Initiate shutdown
        trace!("Shutting down.");
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
