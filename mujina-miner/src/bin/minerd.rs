//! Main entry point for the mujina-miner daemon.

use mujina_miner::{daemon::Daemon, tracing};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing::init_journald_or_stdout();

    let daemon = Daemon::new();
    daemon.run().await
}
