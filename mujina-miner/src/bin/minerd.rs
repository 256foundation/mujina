//! Main entry point for the mujina-miner daemon.

use std::path::PathBuf;

use clap::Parser;
use mujina_miner::{config::Config, daemon::Daemon, tracing};

/// Mujina Bitcoin mining daemon.
#[derive(Parser)]
#[command(name = "mujina-minerd", version)]
struct Cli {
    /// Config file path (overrides /etc/mujina/mujina.yaml).
    #[arg(short = 'c', long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Override individual config keys (may be repeated).
    /// Format: KEY=VALUE using dot-path notation, e.g. --set pool.url=stratum+tcp://...
    /// Mirrors the YAML structure: pool.url, api.listen, boards.cpu_miner.threads, etc.
    /// Takes precedence over env vars and config files.
    #[arg(long = "set", value_name = "KEY=VALUE")]
    set: Vec<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing::init();

    let cli = Cli::parse();

    let overrides = parse_set_flags(&cli.set)?;
    let config = Config::load_with(cli.config, &overrides)?;

    let daemon = Daemon::new(config);
    daemon.run().await
}

fn parse_set_flags(flags: &[String]) -> anyhow::Result<Vec<(String, String)>> {
    flags
        .iter()
        .map(|s| {
            s.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .ok_or_else(|| anyhow::anyhow!("--set value must be KEY=VALUE, got: {s:?}"))
        })
        .collect()
}
