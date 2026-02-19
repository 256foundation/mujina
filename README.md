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

## Supported Hardware

Currently supported:
- [**Bitaxe Gamma**](mujina-miner/src/board/bitaxe_gamma.md) with BM1370 ASIC

Planned support:
- **EmberOne** with BM1362 ASIC
- **EmberOne** with Intel BZM2 ASICs
- Antminer S19j Pro hash boards
- Any and all ASIC mining hardware

## Documentation

### Project Documentation

- [Architecture Overview](docs/architecture.md) - System design and component
  interaction
- [REST API](docs/api.md) - API contract, conventions, and endpoints
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

macOS is supported. USB discovery uses IOKit, which is part of the system
frameworks and requires no additional dependencies.

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
