//! Main entry point for the mujina-miner daemon.

use mujina_miner::{daemon::Daemon, tracing};

fn print_help() {
    println!("mujina-minerd - Bitcoin mining daemon for Mujina Mining Firmware");
    println!();
    println!("USAGE:");
    println!("    mujina-minerd [OPTIONS]");
    println!();
    println!("OPTIONS:");
    println!("    --help     Print this help message");
    println!();
    println!("DESCRIPTION:");
    println!("    A high-performance open-source Bitcoin mining daemon");
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Check for command-line arguments
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        match args[1].as_str() {
            "--help" => {
                print_help();
                return Ok(());
            }
            _ => {
                eprintln!("Unknown option: {}", args[1]);
                eprintln!("Use --help for usage information");
                std::process::exit(1);
            }
        }
    }

    tracing::init_journald_or_stdout();

    let daemon = Daemon::new();
    daemon.run().await
}
