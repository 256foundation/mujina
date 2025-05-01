use tokio::signal::unix::{self, SignalKind};
use tokio_util::{
    sync::CancellationToken,
    task::TaskTracker,
};

use mujina_miner::scheduler;
use mujina_miner::tracing::{self, prelude::*};

#[tokio::main]
async fn main() {
    tracing::init_journald_or_stdout();

    let running = CancellationToken::new();
    let tracker = TaskTracker::new();
    tracker.spawn(scheduler::task(running.clone()));
    tracker.close();
    info!("Started.");

    let mut sigint = unix::signal(SignalKind::interrupt()).unwrap();
    let mut sigterm = unix::signal(SignalKind::terminate()).unwrap();
    tokio::select! {
        _ = sigint.recv() => {},
        _ = sigterm.recv() => {},
        // TODO: wait for crashed threads?
    }

    trace!("Shutting down.");
    running.cancel();

    tracker.wait().await;
    info!("Exiting.");
}
