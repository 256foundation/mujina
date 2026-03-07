# mujina-miner

Open source Bitcoin mining software written in Rust for ASIC mining hardware.

> **Developer Preview**: This software is under heavy development and not ready
> for production use. The code is made available for developers interested in
> contributing, learning about Bitcoin mining protocols, or evaluating the
> architecture. APIs, protocols, and features are subject to change without
> notice. Documentation is incomplete and may be inaccurate. Use at your own
> risk.

## Overview

mujina-miner is a modern, async Rust implementation of Bitcoin mining software
designed to communicate with various Bitcoin mining hash boards via USB serial
interfaces. Part of the larger Mujina OS project, an open source, Debian-based
embedded Linux distribution optimized for Bitcoin mining hardware.

This repository also includes an active Rust port of the Intel BZM2 mining
stack. The goal of the port is to keep BZM2 support inside Mujina rather than
reviving the original split `cgminer` plus `bzmd` process model.

## Features

- **Heterogeneous Multi-Board Support**: Mix and match different hash board
  types in a single deployment; hot-swappable, no need to restart when adding
  or removing boards
- **Hackable & Extensible**: Clear, modular architecture with well-documented
  internals - designed for modification, experimentation, and custom extensions
- **Reference-Grade Documentation**: Thorough documentation at every layer,
  from chip protocols to system architecture, serving as both implementation
  guide and educational resource
- **API-Driven Control**: REST API for all operations---implement your own
  control strategies, automate operations, or build custom interfaces on top
- **Open-Source, Open-Contribution**: Active development with open
  contribution; not code dumps or abandonware, a living project built by
  the entire community
- **Accessible Development**: Start developing with minimal hardware; a laptop
  and a single [Bitaxe](mujina-miner/src/board/bitaxe_gamma.md) board is enough
  to contribute meaningfully
- **BZM2 Port In Progress**: Native Rust BZM2 support with direct UART work
  dispatch, result parsing, telemetry, debug tooling, and startup tuning flows

## Supported Hardware

Currently supported:
- [**Bitaxe Gamma**](mujina-miner/src/board/bitaxe_gamma.md) with BM1370 ASIC

Experimental support in this repository:
- **BZM2 boards** via Mujina's native Rust BZM2 path
  - [Satoshi Starter](https://github.com/Blockscale-Solutions/SatoshiStarter)
  - [bitaxeBIRDS](https://github.com/bitaxeorg/bitaxeBIRDS)
  - direct UART mining path
  - PLL and DLL diagnostics
  - DTS/VS telemetry through the API
  - on-demand DTS/VS query support through CLI and HTTP API
  - silicon-validation helpers adapted from the legacy silicon validation stack

Planned support:
- **EmberOne** with BM1362 ASIC
- Antminer S19j Pro hash boards
- Any and all ASIC mining hardware

The BZM2 work is functional and test-covered, but still not presented as
production-ready hardware support. The remaining gap is mostly board-specific
bring-up and manufacturing/operations integration rather than the core UART
ASIC path.

## Documentation

### Project Documentation

- [Architecture Overview](docs/architecture.md) - System design and component
  interaction
- [REST API](docs/api.md) - API contract, conventions, and endpoints
- [BZM2 Port Note](docs/bzm2/bzm2-port.md) - Architecture, implemented behavior,
  telemetry, and current scope boundaries for the BZM2 port
- [BZM2 UART Debug Guide](docs/bzm2/bzm2-uart-debug.md) - CLI usage for UART,
  telemetry queries, TDM observation, clock diagnostics, and validation flows
- [BZM2 Tuning Planner](docs/bzm2/bzm2-pnp.md) - Tuning-planner behavior and current
  calibration scope
- [BZM2 Opcode Grounding](docs/bzm2/bzm2-opcode-grounding.md) - Source-grounded UART
  opcode behavior and the current JTAG evidence boundary
- [Blockscale ASIC Integration Guide](docs/bzm2/blockscale-asic-integration-guide.md) -
  Generic hardware design guidance for building a custom solution around the
  Blockscale / BZM2 ASIC
- [Blockscale UART And TDM Reference](docs/bzm2/blockscale-uart-protocol-reference.md) -
  ASIC-facing UART, TDM, opcode, and job-programming reference
- [Blockscale Reference Roadmap](docs/bzm2/blockscale-reference-roadmap.md) -
  Ordered implementation plan for closing the remaining bring-up, tuning, and
  diagnostics gaps
- [CPU Mining](docs/cpu-mining.md) - Run without hardware for development and
  testing
- [Container Image](docs/container.md) - Build and run as a container
- [Contribution Guide](CONTRIBUTING.md) - How to contribute to the project
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

## Build Requirements

### Linux

On Debian/Ubuntu systems:

```bash
sudo apt-get install libudev-dev libssl-dev
```

### macOS

macOS support is planned, but USB discovery using IOKit is not yet implemented.

## Building

A [justfile](https://github.com/casey/just) provides common development tasks:

```bash
just test      # Run unit tests (no hardware required)
just run       # Build and run the miner
just checks    # Run all checks (fmt, lint, test)
```

Or use cargo directly:

```bash
cargo build
cargo test
```

## Running

At this point in development, configuration is done via environment variables.
Once configuration storage and API functionality are more complete, persistent
configuration will be available through the REST API and CLI tools.

### Pool Configuration

Connect to a Stratum v1 mining pool:

```bash
MUJINA_POOL_URL="stratum+tcp://localhost:3333" \
MUJINA_POOL_USER="bc1qce93hy5rhg02s6aeu7mfdvxg76x66pqqtrvzs3.mujina" \
MUJINA_POOL_PASS="custom-password" \
cargo run
```

The password defaults to "x" if not specified.

Without `MUJINA_POOL_URL`, the miner runs with a dummy job source that
generates synthetic mining work, which is useful for testing hardware without a
pool connection.

### BZM2 Quick Start

Enable the BZM2 path by pointing Mujina at one or more serial devices:

```bash
MUJINA_BZM2_SERIAL="/dev/ttyUSB0" \
MUJINA_BZM2_BAUD="5000000" \
MUJINA_BZM2_DTS_VS_GEN="2" \
cargo run -p mujina-miner --bin minerd
```

Useful companion tooling:

```bash
# Query one ASIC's DTS/VS telemetry directly
cargo run -p mujina-miner --bin mujina-bzm2-debug -- \
  dts-vs-query /dev/ttyUSB0 2 gen2 1500 5000000

# Read refreshed board telemetry over HTTP
curl -X POST http://127.0.0.1:7785/api/v0/boards/bzm2-0/bzm2/dts-vs-query \
  -H "Content-Type: application/json" \
  -d '{"thread_index":0,"asic":2}'
```

See [BZM2 UART Debug Guide](docs/bzm2/bzm2-uart-debug.md) for the full command
surface.

### API Server

The REST API listens on `127.0.0.1:7785` by default. To listen
on all interfaces:

```bash
MUJINA_API_LISTEN="0.0.0.0" cargo run
```

See [REST API](docs/api.md) for endpoints and details.

### Running Without Hardware

For development and testing without physical mining hardware, the miner
includes a CPU mining backend. See [CPU Mining](docs/cpu-mining.md) for
details.

A container image is available for deploying to cloud infrastructure or
Kubernetes for pool and miner testing. See [Container Image](docs/container.md).

### Log Levels

Control output verbosity with `RUST_LOG`:

```bash
# Info level (default) -- shows pool connection, shares, errors
cargo run

# Debug level -- adds job distribution, hardware state changes
RUST_LOG=mujina_miner=debug cargo run

# Trace level -- shows all protocol traffic (serial, network, I2C)
RUST_LOG=mujina_miner=trace cargo run
```

Target specific modules for focused debugging:

```bash
# Trace just the Stratum v1 client
RUST_LOG=mujina_miner::stratum_v1=trace cargo run

# Debug Stratum v1, trace BM13xx protocol
RUST_LOG=mujina_miner::stratum_v1=debug,mujina_miner::asic::bm13xx=trace cargo run
```

Combine pool configuration with logging as needed:

```bash
RUST_LOG=mujina_miner=debug \
MUJINA_POOL_URL="stratum+tcp://localhost:3333" \
MUJINA_POOL_USER="your-address.worker" \
cargo run
```

## Protocol Analysis Tool

The `mujina-dissect` tool analyzes captured communication between the host and
mining hardware, providing detailed protocol-level insights for BM13xx serial
commands, PMBus/I2C power management, and fan control.

See [tools/mujina-dissect/README.md](tools/mujina-dissect/README.md) for
detailed usage and documentation.

## Validation Status

As of the current BZM2 porting work, the full Linux-side `mujina-miner` test
suite passes in WSL with:

- `327 passed`
- `0 failed`
- `5 ignored`

That validation includes the BZM2-specific protocol, thread, board, telemetry,
API, and debug-tooling coverage added in this repository.

## License

This project is licensed under the GNU General Public License v3.0 or later.
See the [LICENSE](LICENSE) file for details.

## Contributing

We welcome contributions! Whether you're fixing bugs, adding features, improving
documentation, or simply exploring the codebase to learn about Bitcoin mining
protocols and hardware, your involvement is valued.

Please see our [Contribution Guide](CONTRIBUTING.md) for details on how to get
started.

## Related Projects

- [Bitaxe](https://github.com/bitaxeorg) - Open-source Bitcoin mining
  hardware designs
- [bitaxe-raw](https://github.com/bitaxeorg/bitaxe-raw) - Firmware for Bitaxe
  boards
- [EmberOne](https://github.com/256foundation/emberone00-pcb) - Open-source
  Bitcoin mining hashboard
- [emberone-usbserial-fw](https://github.com/256foundation/emberone-usbserial-fw) -
  Firmware for EmberOne boards
- [Satoshi Starter](https://github.com/Blockscale-Solutions/SatoshiStarter) -
  BZM2-based open hardware carrier from Blockscale Solutions
- [bitaxeBIRDS](https://github.com/bitaxeorg/bitaxeBIRDS) -
  BZM2-based Bitaxe-family board design
