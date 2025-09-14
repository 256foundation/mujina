//! Mujina protocol dissector for Saleae Logic 2 captures.

mod capture;
mod dissect;
mod i2c;
mod output;
mod serial;

use anyhow::{Context, Result};
use capture::{BaudRate, CaptureEvent, CaptureReader, Channel};
use clap::Parser;
use dissect::{dissect_i2c_operation, dissect_serial_frame};
use i2c::{group_transactions, I2cAssembler};
use output::{OutputConfig, OutputEvent};
use serial::{Direction, FrameAssembler, SerialFrame};
use std::path::PathBuf;

/// Protocol dissector for Bitcoin mining hardware captures
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to Saleae Logic 2 CSV export file
    input: PathBuf,

    /// Show raw hex data for each frame
    #[arg(short = 'x', long)]
    hex: bool,

    /// Use absolute timestamps instead of relative (seconds from start)
    #[arg(short = 'a', long)]
    absolute_time: bool,

    /// Filter by channel (CI, RO, I2C)
    #[arg(short = 'f', long)]
    filter_channel: Option<String>,

    /// Filter by protocol (bm13xx, i2c, all)
    #[arg(short = 'p', long, default_value = "all")]
    protocol: String,

    /// Output file (default: stdout)
    #[arg(short = 'o', long)]
    output: Option<PathBuf>,

    /// Disable colored output
    #[arg(long)]
    no_color: bool,

    /// Enable debug logging
    #[arg(short = 'd', long)]
    debug: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    // Setup logging if requested
    if args.debug {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::from_default_env()
                    .add_directive("mujina_dissect=debug".parse()?),
            )
            .init();
    }

    // Open capture file
    let mut reader = CaptureReader::open(&args.input)
        .with_context(|| format!("Failed to open capture file: {:?}", args.input))?;

    // Setup output configuration
    let mut output_config = OutputConfig {
        show_raw_hex: args.hex,
        use_relative_time: !args.absolute_time,
        start_time: None,
        use_color: !args.no_color && atty::is(atty::Stream::Stdout),
    };

    // Setup assemblers - one for each baud rate per channel
    let mut ci_115k_assembler = FrameAssembler::new(Direction::HostToChip);
    let mut ci_1m_assembler = FrameAssembler::new(Direction::HostToChip);
    let mut ro_115k_assembler = FrameAssembler::new(Direction::ChipToHost);
    let mut ro_1m_assembler = FrameAssembler::new(Direction::ChipToHost);
    let mut i2c_assembler = I2cAssembler::new();

    // Collect all events for sorting
    let mut all_events = Vec::new();
    let mut candidate_frames = Vec::new();

    // Process capture events
    for event_result in reader.events() {
        let event = event_result?;

        match event {
            CaptureEvent::Serial(serial_event) => {
                // Filter by channel if requested
                if let Some(ref filter) = args.filter_channel {
                    let channel_name = format!("{:?}", serial_event.channel);
                    if !filter.eq_ignore_ascii_case(&channel_name) {
                        continue;
                    }
                }

                // Process with appropriate assembler based on channel and baud rate
                let assembler = match (serial_event.channel, serial_event.baud_rate) {
                    (Channel::CI, BaudRate::Baud115200) => &mut ci_115k_assembler,
                    (Channel::CI, BaudRate::Baud1M) => &mut ci_1m_assembler,
                    (Channel::RO, BaudRate::Baud115200) => &mut ro_115k_assembler,
                    (Channel::RO, BaudRate::Baud1M) => &mut ro_1m_assembler,
                };

                if let Some(frame) = assembler.process(&serial_event) {
                    candidate_frames.push((frame, serial_event.baud_rate));
                }
            }
            CaptureEvent::I2c(i2c_event) => {
                // Filter by channel if requested
                if let Some(ref filter) = args.filter_channel {
                    if !filter.eq_ignore_ascii_case("i2c") {
                        continue;
                    }
                }

                i2c_assembler.process(&i2c_event);
            }
        }
    }

    // Flush any pending frames from all assemblers
    if let Some(frame) = ci_115k_assembler.flush() {
        candidate_frames.push((frame, BaudRate::Baud115200));
    }
    if let Some(frame) = ci_1m_assembler.flush() {
        candidate_frames.push((frame, BaudRate::Baud1M));
    }
    if let Some(frame) = ro_115k_assembler.flush() {
        candidate_frames.push((frame, BaudRate::Baud115200));
    }
    if let Some(frame) = ro_1m_assembler.flush() {
        candidate_frames.push((frame, BaudRate::Baud1M));
    }
    i2c_assembler.flush();

    // Deduplicate frames at frame level
    let deduplicated_frames = deduplicate_frames(candidate_frames);

    // Collect serial frames
    if args.protocol == "all" || args.protocol == "bm13xx" {
        for frame in deduplicated_frames {
            let dissected = dissect_serial_frame(&frame);
            all_events.push(OutputEvent::Serial(dissected));
        }
    }

    // Collect I2C transactions
    if args.protocol == "all" || args.protocol == "i2c" {
        let mut transactions = Vec::new();
        while let Some(transaction) = i2c_assembler.next_transaction() {
            transactions.push(transaction);
        }

        // Group transactions into operations
        let operations = group_transactions(&transactions);
        for op in operations {
            let dissected = dissect_i2c_operation(&op);
            all_events.push(OutputEvent::I2c(dissected));
        }
    }

    // Sort events by timestamp
    all_events.sort_by(|a, b| a.timestamp().partial_cmp(&b.timestamp()).unwrap());

    // Set start time for relative timestamps (default behavior)
    if !args.absolute_time && !all_events.is_empty() {
        output_config.start_time = Some(all_events[0].timestamp());
    }

    // Output results
    if let Some(output_path) = args.output {
        use std::io::Write;
        let mut file = std::fs::File::create(&output_path)
            .with_context(|| format!("Failed to create output file: {:?}", output_path))?;

        for event in all_events {
            writeln!(file, "{}", event.format(&output_config))?;
        }
    } else {
        for event in all_events {
            println!("{}", event.format(&output_config));
        }
    }

    Ok(())
}

/// Deduplicate frames by preferring 115k baud over 1M when both exist at similar times
fn deduplicate_frames(mut candidates: Vec<(SerialFrame, BaudRate)>) -> Vec<SerialFrame> {
    // Sort by timestamp
    candidates.sort_by(|a, b| a.0.start_time.partial_cmp(&b.0.start_time).unwrap());

    let mut deduplicated = Vec::new();
    let mut i = 0;

    while i < candidates.len() {
        let (frame, baud) = &candidates[i];

        // Look for frames from the same direction at similar times
        let mut j = i + 1;
        let mut found_115k = baud == &BaudRate::Baud115200;
        let mut best_idx = i;

        while j < candidates.len() {
            let (other_frame, other_baud) = &candidates[j];

            // If frames are from same direction and within 10ms, consider them duplicates
            if other_frame.direction == frame.direction
                && (other_frame.start_time - frame.start_time).abs() < 0.01
            {
                // Prefer 115k over 1M, or frame with fewer errors
                if other_baud == &BaudRate::Baud115200 && !found_115k {
                    found_115k = true;
                    best_idx = j;
                } else if other_baud == baud && !other_frame.has_errors && frame.has_errors {
                    best_idx = j;
                }

                j += 1;
            } else {
                break; // Frames too far apart in time
            }
        }

        // Add the best frame
        deduplicated.push(candidates[best_idx].0.clone());

        // Skip all the duplicate frames
        i = j;
    }

    deduplicated
}

// Check if output is a terminal (for color support)
mod atty {
    pub enum Stream {
        Stdout,
    }

    pub fn is(_stream: Stream) -> bool {
        // Simple check for terminal
        std::env::var("TERM").is_ok()
    }
}
