# mujina-miner

Open source Bitcoin mining software written in Rust for ASIC mining hardware.

## Overview

mujina-miner is a modern, async Rust implementation of Bitcoin mining software
designed to communicate with various Bitcoin mining hash boards via USB serial
interfaces. It's part of the larger Mujina OS project, an open source,
Debian-based embedded Linux distribution optimized for Bitcoin mining hardware.

## Features

- **Modular Architecture**: Clean separation between transport, board control,
  chip communication, and mining logic
- **Async I/O**: Built on Tokio for efficient concurrent operations
- **Multi-Board Support**: Extensible design for different ASIC boards and chips
- **Hardware Abstraction**: Generic traits for GPIO, I2C, and ADC operations
- **Protocol Documentation**: Detailed documentation of mining protocols

## Documentation

### Project Documentation

- [Architecture Overview](docs/architecture.md) - System design and component
  interaction
- [Contributing Guide](CONTRIBUTING.md) - How to contribute to the project
- [Code Style Guide](CODE_STYLE.md) - Formatting and style rules
- [Coding Guidelines](CODING_GUIDELINES.md) - Best practices and design
  patterns

### Protocol Documentation

- [BM13xx ASIC Protocol](mujina-miner/src/asic/bm13xx/PROTOCOL.md) - Serial
  protocol for BM13xx series mining chips
- [Bitaxe-Raw Control Protocol](mujina-miner/src/mgmt_protocol/bitaxe_raw/PROTOCOL.md) -
  Management protocol for Bitaxe board peripherals

### Hardware Documentation

- [Bitaxe Gamma Board Guide](mujina-miner/src/board/bitaxe_gamma.md) - Hardware
  and software interface documentation for Bitaxe Gamma

## Supported Hardware

Currently supported:
- **Bitaxe Gamma** with BM1370 ASIC (via bitaxe-raw firmware)

Planned support:
- **EmberOne** with BM1362 ASIC
- Additional Bitaxe variants
- Antminer hash boards
- Other ASIC mining hardware

## Building

```bash
# Build the workspace
cargo build

# Run tests
cargo test

# Run the miner daemon (requires connected hardware)
cargo run --bin mujina-minerd
```

## Running

Ensure your Bitaxe device is connected via USB and appears as `/dev/ttyACM0`
and `/dev/ttyACM1`.

```bash
# Run with default logging
cargo run --bin mujina-minerd

# Run with debug logging
RUST_LOG=mujina_miner=debug cargo run --bin mujina-minerd

# Run with trace logging (shows all serial communication)
RUST_LOG=mujina_miner=trace cargo run --bin mujina-minerd
```

## Protocol Analysis Tool

The `mujina-dissect` tool analyzes captured communication between the host and
mining hardware, providing detailed protocol-level insights.

### Features

- **BM13xx ASIC Protocol**: Dissects serial commands and responses with CRC
  validation
- **PMBus/I2C Analysis**: Decodes power management operations with Linear11/
  Linear16 format conversion
- **TPS546 Power Controller**: Context-aware state tracking including
  VOUT_MODE interpretation
- **Human-Readable Output**: Color-coded, formatted display of protocol
  transactions

### Usage

The dissector reads CSV files exported from Saleae Logic analyzers:

```bash
# Analyze a capture file
cargo run --bin mujina-dissect -- path/to/capture.csv

# Filter by protocol
cargo run --bin mujina-dissect -- path/to/capture.csv -p bm13xx
cargo run --bin mujina-dissect -- path/to/capture.csv -f I2C

# Show hexdump alongside decoded output
cargo run --bin mujina-dissect -- path/to/capture.csv -x
```

The tool reuses the same protocol parsing code as the miner itself, ensuring
consistency between analysis and runtime behavior.

## Development Status

This is an active development project. Current focus areas:
- [x] Basic board initialization and chip discovery
- [x] GPIO control for ASIC reset
- [x] I2C peripheral control implementation
- [x] EMC2101 fan controller with temperature monitoring
- [x] TPS546 power management controller
- [ ] Additional temperature sensors (TMP75)
- [ ] Power monitoring (INA260)
- [ ] Stratum pool communication
- [ ] Production-ready error handling and recovery

## License

This project is licensed under the GNU General Public License v3.0 or later -
see the [LICENSE](LICENSE) file for details.

## Contributing

We welcome contributions! Please see our [Contributing Guide](CONTRIBUTING.md)
for details on how to get started.

## Related Projects

- [Mujina OS](https://github.com/mujina-os/mujina-os) - The parent project
- [bitaxe-raw](https://github.com/bitaxeorg/bitaxe-raw) - Firmware for Bitaxe
  boards