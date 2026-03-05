//! Main entry point for the mujina-miner daemon.

use std::path::PathBuf;

use clap::Parser;
use mujina_miner::{config::Config, daemon::Daemon, tracing};

/// Mujina Bitcoin mining daemon.
#[derive(Parser)]
#[command(name = "mujina-minerd", version)]
struct Cli {
    /// Config file path (overrides MUJINA_CONFIG_FILE_PATH and the default
    /// /etc/mujina/mujina.yaml location).
    #[arg(short = 'c', long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Log level: error | warn | info | debug | trace
    #[arg(long, value_name = "LEVEL")]
    log_level: Option<String>,

    /// API listen address, e.g. 0.0.0.0:7785
    #[arg(long, value_name = "ADDR")]
    api_listen: Option<String>,

    /// Pool URL, e.g. stratum+tcp://pool.example.com:3333
    #[arg(long, value_name = "URL")]
    pool_url: Option<String>,

    /// Pool worker username
    #[arg(long, value_name = "USER")]
    pool_user: Option<String>,

    /// Pool worker password
    #[arg(long, value_name = "PASS")]
    pool_pass: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing::init_journald_or_stdout();

    let cli = Cli::parse();

    // Load config through the standard hierarchy (files + env vars), then
    // apply CLI flag overrides on top as the highest-priority source.
    let mut config = Config::load_with(cli.config)?;

    if let Some(level) = cli.log_level {
        config.daemon.log_level = level;
    }
    if let Some(listen) = cli.api_listen {
        config.api.listen = listen;
    }
    if let Some(url) = cli.pool_url {
        config.pool.url = Some(url);
    }
    if let Some(user) = cli.pool_user {
        config.pool.user = user;
    }
    if let Some(pass) = cli.pool_pass {
        config.pool.password = pass;
    }

    let daemon = Daemon::new(config);
    daemon.run().await
}
