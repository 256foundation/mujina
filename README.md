# mujina-miner

Open-source Bitcoin mining firmware.

## Why Mujina

You bought the hardware, but someone else controls the software. Whether
you have thousands of machines in a data center or one in your basement,
the firmware running them is closed. It comes from the manufacturer or a
third-party vendor, and you can't read it, audit it, or change it.

Mujina is here to change that: one open-source codebase to run any
hashboard from any vendor on any control board, written by hardware
engineers, protocol authors, and mining operators from across the industry.
Read every line, modify it without permission, control it through a
documented API, and pay no dev fee. Own your firmware.

## Current Status

Mujina is under active development. Today's supported hardware is a
starting point:

**Working now**

- **[Bitaxe Gamma](mujina-miner/src/board/bitaxe_gamma.md)** (single
  BM1370 ASIC): an open-source single-chip miner. Good for developers
  and advanced users who want to run Mujina on real hardware today.
- **CPU backend**: software SHA-256 hashing, no hardware required.
  Useful for exercising Mujina itself, testing pool and other server
  software against a working miner client, and teaching the full mining
  pipeline. See [CPU Mining](docs/cpu-mining.md).

**Landing now**

- **[EmberOne00](https://github.com/256foundation/emberone00-pcb)**
  (twelve BM1362 ASICs): a sister project from the 256 Foundation. An
  open-source hashboard designed to be driven by open firmware.

**Near-term targets**

- Installable images for the Antminer S19 series
- The 256 Foundation's forthcoming
  [Libreboard](https://github.com/256foundation/libreboard) control
  board
- Broader support for commercial mining machines

APIs are still moving and parts of the docs lag the code.

## Quick Start

Build Mujina and watch it run end to end, no mining hardware required.
On Debian or Ubuntu:

```bash
git clone https://github.com/256foundation/mujina.git
cd mujina
sudo apt-get install libudev-dev libssl-dev
MUJINA_CPUMINER_THREADS=1 MUJINA_CPUMINER_DUTY=50 MUJINA_USB_DISABLE=1 \
  cargo run --bin mujina-minerd
```

In this example, the CPU backend hashes in software against a dummy job
source, exercising the full pipeline: job distribution, hashing, share
detection, logging, and the API. When you're ready to mine for real,
continue below.

## Build Requirements

Mujina builds with the current stable
[Rust toolchain](https://rustup.rs). Install the additional packages
below for your platform.

### Linux

On Debian or Ubuntu:

```bash
sudo apt-get install libudev-dev libssl-dev
```

Other distributions need their equivalents of the udev and openssl
development packages.

### macOS

macOS is supported. Install Xcode Command Line Tools alongside the Rust
toolchain. A build failure on `openssl-sys` usually means the build
can't find openssl; see the
[openssl crate's macOS notes](https://docs.rs/openssl/latest/openssl/#automatic)
for the supported installation and environment options.

## Building

mujina-miner is a cargo workspace. Build and test it the usual way:

```bash
cargo build
cargo test
```

The workspace contains several binaries: `mujina-minerd` (the daemon),
`mujina-cli`, and others. Running requires picking one:

```bash
cargo run --bin mujina-minerd
```

If you'll be working in the repo regularly, install
[just](https://github.com/casey/just) (`cargo install just`) for
shorter aliases that avoid retyping the `--bin` flag:

```bash
just run        # same as cargo run --bin mujina-minerd
just test       # same as cargo test
just checks     # fmt, lint, and test in one step
```

Examples in the rest of this README use plain cargo so they work
without `just` installed.

## Running

Mujina is currently configured through environment variables.
Persistent configuration via the REST API and CLI will follow as those
interfaces mature.

### Connecting to a job source

Point Mujina at a Stratum v1 mining pool:

```bash
MUJINA_POOL_URL="stratum+tcp://pool.example.com:3333" \
MUJINA_POOL_USER="your-address.worker" \
cargo run --bin mujina-minerd
```

`MUJINA_POOL_USER` defaults to `mujina-testing` and `MUJINA_POOL_PASS`
defaults to `x`, so only `MUJINA_POOL_URL` is strictly required.

### Testing without a pool

Omit `MUJINA_POOL_URL` to use a dummy job source that generates
synthetic mining work. Useful for development without a network
connection.

```bash
cargo run --bin mujina-minerd
```

### Controlling log output

The default filter emits Mujina log entries at info level and
third-party crates at warn. `RUST_LOG` directives are additive: set
per-module levels to dig into specific subsystems without flooding the
rest of the log.

```bash
# Default: info for Mujina, warn for third-party crates
cargo run --bin mujina-minerd

# Trace the Stratum v1 client, everything else unchanged
RUST_LOG=mujina_miner::stratum_v1=trace cargo run --bin mujina-minerd

# Debug Stratum v1 and trace BM13xx at the same time
RUST_LOG=mujina_miner::stratum_v1=debug,mujina_miner::asic::bm13xx=trace \
  cargo run --bin mujina-minerd
```

Debug shows logical stages and summaries: chip initialization, jobs
received from the pool, shares submitted. Trace adds step-by-step
execution detail: individual serial frames, I2C transactions, and USB
device events.

### Using the REST API

Mujina logs the API bind address at startup. By default it's
`127.0.0.1:7785`; set `MUJINA_API_LISTEN` to change it:

```bash
# All interfaces, default port
MUJINA_API_LISTEN="0.0.0.0" cargo run --bin mujina-minerd

# All interfaces, custom port
MUJINA_API_LISTEN="0.0.0.0:9000" cargo run --bin mujina-minerd
```

See [REST API](docs/api.md) for endpoints and conventions. The
`/api/v0/` prefix signals the API is still in flux. Authentication
is on the roadmap.

## Contributing

We welcome contributions! Whether you're fixing bugs, adding features,
improving documentation, or simply exploring the codebase to learn
about Bitcoin mining protocols and hardware, your involvement is
valued.

Please see our [Contribution Guide](CONTRIBUTING.md) for details on
how to get started. For user-oriented discussion and support, visit
the [Mujina forum](https://forum.256foundation.org/c/mujina/7). For
real-time chat, join our [Telegram group](https://t.me/the256foundation)---note
that Telegram is ephemeral; decisions and important context belong on
GitHub.

## Further Reading

### Design and operation

- [Architecture Overview](docs/architecture.md): system design and
  component interaction
- [REST API](docs/api.md): endpoints, conventions, and OpenAPI spec
- [CPU Mining](docs/cpu-mining.md): the CPU backend in detail
- [Container Image](docs/container.md): build and run Mujina as a
  container

### Protocols

- [BM13xx ASIC Protocol](mujina-miner/src/asic/bm13xx/PROTOCOL.md):
  serial protocol for the BM13xx mining-chip family
- [Bitaxe-Raw Control Protocol](mujina-miner/src/mgmt_protocol/bitaxe_raw/PROTOCOL.md):
  management protocol for Bitaxe board peripherals

### Hardware

- [Bitaxe Gamma Board Guide](mujina-miner/src/board/bitaxe_gamma.md):
  board hardware, firmware flashing, and Mujina integration

### Contributor reference

- [Contribution Guide](CONTRIBUTING.md): process and requirements
- [Code Style Guide](CODE_STYLE.md): formatting and mechanical style
- [Coding Guidelines](CODING_GUIDELINES.md): design patterns and best
  practices

## Related Projects

- [Bitaxe](https://github.com/bitaxeorg): open-source Bitcoin mining
  hardware
- [bitaxe-raw](https://github.com/bitaxeorg/bitaxe-raw): pass-through firmware for
  Bitaxe boards required for use by Mujina
- [EmberOne00](https://github.com/256foundation/emberone00-pcb): 256
  Foundation's first open-source Bitcoin mining hashboard
- [Libreboard](https://github.com/256foundation/libreboard): 256
  Foundation's open-source mining control board

## License

This project is licensed under the GNU General Public License v3.0 or
later. See the [LICENSE](LICENSE) file for details.
