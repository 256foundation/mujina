//! Daemon lifecycle management for mujina-miner.
//!
//! This module handles the core daemon functionality including initialization,
//! task management, signal handling, and graceful shutdown.

use tokio::signal::unix::{self, SignalKind};
use tokio::sync::mpsc;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::tracing::prelude::*;
use crate::{
    backplane::Backplane,
    hash_thread::HashThread,
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
        let (thread_tx, thread_rx) = mpsc::channel::<Vec<Box<dyn HashThread>>>(10);

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

                // Shutdown all boards regardless of which arm completed
                backplane.shutdown_all_boards().await;
            }
        });

        // Start the scheduler with thread receiver
        self.tracker
            .spawn(scheduler::task(self.shutdown.clone(), thread_rx));
        self.tracker.close();

        info!("Started.");
        info!("For hardware debugging, set RUST_LOG=mujina_miner=trace to see all communication");

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
