//! Main entry point for the mujina-miner daemon.

use clap::Command;

use mujina_miner::{daemon::Daemon, env_help, tracing};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // The daemon takes no positional arguments; clap is here to handle
    // --help (documenting the control environment variables) and --version.
    Command::new("mujina-minerd")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Bitcoin ASIC mining daemon")
        .after_help(env_help::help_text())
        .get_matches();

    tracing::init();

    let daemon = Daemon::new();
    daemon.run().await
}
