//! Mujina protocol dissector for Saleae Logic 2 captures.

mod bm13xx;
mod capture;
mod dissect;
mod i2c;
mod output;

use anyhow::{Context, Result};
use bm13xx::{CommandStreamingParser, DecodedFrame, ParsedItem, ResponseStreamingParser};
use capture::{BaudRate, CaptureEvent, CaptureReader, Channel};
use clap::Parser;
use dissect::{dissect_decoded_frame, dissect_i2c_operation_with_context, I2cContexts};
use i2c::{group_pmbus_transactions, group_transactions, I2cAssembler};
use output::{OutputConfig, OutputEvent};
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

    /// Force color output even when not connected to a TTY
    #[arg(long)]
    force_color: bool,

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
        use_color: args.force_color || (!args.no_color && atty::is(atty::Stream::Stdout)),
    };

    // Force colored output if requested
    if args.force_color {
        colored::control::set_override(true);
    } else if args.no_color {
        colored::control::set_override(false);
    }

    // Setup streaming parsers - one for each baud rate per channel
    let mut ci_115k_parser = CommandStreamingParser::new();
    let mut ci_1m_parser = CommandStreamingParser::new();
    let mut ro_115k_parser = ResponseStreamingParser::new();
    let mut ro_1m_parser = ResponseStreamingParser::new();

    let mut i2c_assembler = I2cAssembler::new();

    // Collect all events for sorting
    let mut all_events = Vec::new();
    let mut decoded_frames = Vec::new();

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

                // Process with appropriate streaming parser based on channel and baud rate
                match (serial_event.channel, serial_event.baud_rate) {
                    (Channel::CI, BaudRate::Baud115200) => {
                        for parsed_item in ci_115k_parser.process_event(&serial_event) {
                            if let ParsedItem::ValidFrame {
                                command,
                                raw_bytes,
                                timestamps,
                            } = parsed_item
                            {
                                let frame = DecodedFrame::Command {
                                    timestamp: timestamps
                                        .last()
                                        .copied()
                                        .unwrap_or(serial_event.timestamp),
                                    command,
                                    raw_bytes,
                                    _has_errors: false,
                                    baud_rate: serial_event.baud_rate,
                                };
                                decoded_frames.push((frame, serial_event.baud_rate));
                            }
                        }
                    }
                    (Channel::CI, BaudRate::Baud1M) => {
                        for parsed_item in ci_1m_parser.process_event(&serial_event) {
                            if let ParsedItem::ValidFrame {
                                command,
                                raw_bytes,
                                timestamps,
                            } = parsed_item
                            {
                                let frame = DecodedFrame::Command {
                                    timestamp: timestamps
                                        .last()
                                        .copied()
                                        .unwrap_or(serial_event.timestamp),
                                    command,
                                    raw_bytes,
                                    _has_errors: false,
                                    baud_rate: serial_event.baud_rate,
                                };
                                decoded_frames.push((frame, serial_event.baud_rate));
                            }
                        }
                    }
                    (Channel::RO, BaudRate::Baud115200) => {
                        for parsed_item in ro_115k_parser.process_event(&serial_event) {
                            if let ParsedItem::ValidResponse {
                                response,
                                raw_bytes,
                                timestamps,
                            } = parsed_item
                            {
                                let frame = DecodedFrame::Response {
                                    timestamp: timestamps
                                        .last()
                                        .copied()
                                        .unwrap_or(serial_event.timestamp),
                                    response,
                                    raw_bytes,
                                    _has_errors: false,
                                    baud_rate: serial_event.baud_rate,
                                };
                                decoded_frames.push((frame, serial_event.baud_rate));
                            }
                        }
                    }
                    (Channel::RO, BaudRate::Baud1M) => {
                        for parsed_item in ro_1m_parser.process_event(&serial_event) {
                            if let ParsedItem::ValidResponse {
                                response,
                                raw_bytes,
                                timestamps,
                            } = parsed_item
                            {
                                let frame = DecodedFrame::Response {
                                    timestamp: timestamps
                                        .last()
                                        .copied()
                                        .unwrap_or(serial_event.timestamp),
                                    response,
                                    raw_bytes,
                                    _has_errors: false,
                                    baud_rate: serial_event.baud_rate,
                                };
                                decoded_frames.push((frame, serial_event.baud_rate));
                            }
                        }
                    }
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

    // Streaming parsers don't need explicit flushing - they process incrementally
    i2c_assembler.flush();

    // Collect serial frames - each channel decodes independently, no deduplication
    if args.protocol == "all" || args.protocol == "bm13xx" {
        for (frame, _baud_rate) in decoded_frames {
            let dissected = dissect_decoded_frame(&frame);
            all_events.push(OutputEvent::Serial(dissected));
        }
    }

    // Collect I2C transactions
    if args.protocol == "all" || args.protocol == "i2c" {
        let mut transactions = Vec::new();
        while let Some(transaction) = i2c_assembler.next_transaction() {
            transactions.push(transaction);
        }

        // Group transactions into operations with context tracking
        // Use PMBus-aware grouping for known PMBus devices, generic grouping for others
        let (pmbus_transactions, other_transactions): (Vec<_>, Vec<_>) =
            transactions.into_iter().partition(|t| t.address == 0x24); // TPS546 address

        let mut operations = Vec::new();

        // Process PMBus transactions with PMBus-aware grouping
        if !pmbus_transactions.is_empty() {
            operations.extend(group_pmbus_transactions(&pmbus_transactions));
        }

        // Process other transactions with generic grouping
        if !other_transactions.is_empty() {
            operations.extend(group_transactions(&other_transactions));
        }

        // Sort operations by timestamp
        operations.sort_by(|a, b| a.start_time.partial_cmp(&b.start_time).unwrap());

        let mut i2c_contexts = I2cContexts::default();
        for op in operations {
            let dissected = dissect_i2c_operation_with_context(&op, &mut i2c_contexts);
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
