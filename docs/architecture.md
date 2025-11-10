# Mujina Miner Architecture

This document describes the high-level architecture of mujina-miner,
Bitcoin mining software built in Rust.

## Overview

Mujina-miner is organized as a single Rust crate with well-defined modules
that separate concerns while maintaining simplicity. The architecture is
fully async using Tokio for concurrent I/O operations.

### Key Dependencies

- **tokio**: Async runtime for concurrent I/O operations
- **tokio-serial**: Async serial port communication
- **rust-bitcoin**: Core Bitcoin types and utilities
- **axum**: HTTP server framework
- **tracing**: Structured logging and diagnostics

## Module Structure

```
src/
├── bin/              # Binary entry points
│   ├── minerd.rs     # mujina-minerd - Main daemon
│   ├── cli.rs        # mujina-cli - Command line interface
│   └── tui.rs        # mujina-tui - Terminal UI
├── lib.rs            # Library root
├── error.rs          # Common error types
├── types.rs          # Core types (Job, Share, Nonce, etc.)
├── config.rs         # Configuration loading and validation
├── daemon.rs         # Daemon lifecycle management
├── board/            # Hash board implementations
├── transport/        # Physical transport layer
├── mgmt_protocol/    # Board management protocols
├── hw_trait/         # Hardware interface traits (I2C, SPI, GPIO, Serial)
├── peripheral/       # Peripheral chip drivers
├── asic/             # Mining ASIC drivers
├── backplane.rs      # Backplane: board communication and lifecycle
├── scheduler.rs      # Work scheduling and distribution
├── job_generator.rs  # Local job generation (testing/solo) [deprecated]
├── pool/             # Mining pool connectivity [deprecated]
├── job_source/       # Unified mining job sources (pools, solo, testing)
├── api/              # HTTP API and WebSocket
├── api_client/       # Shared API client library
│   ├── mod.rs        # Client implementation
│   └── types.rs      # API DTOs and models
└── tracing.rs        # Logging and observability
```

## Module Descriptions

### Core Modules

#### `bin/minerd.rs`
The main daemon binary entry point. Handles:
- Signal handling (SIGINT/SIGTERM)
- Tokio runtime initialization
- Graceful shutdown coordination
- Top-level task spawning

#### `bin/cli.rs`
Command-line interface for controlling the miner:
- Uses the `api_client` module to communicate with the daemon
- Provides commands for status, configuration, pool management
- Suitable for scripting and automation

#### `bin/tui.rs`
Terminal user interface for interactive monitoring:
- Uses the `api_client` module to communicate with the daemon
- Built with ratatui for rich terminal graphics
- Real-time hashrate graphs and statistics
- Keyboard-driven interface for operators
- Connects via API WebSocket for live updates

#### `error.rs` (new)
Centralized error types using `thiserror`. Provides a unified `Error` enum
for the entire crate with conversions from underlying error types.

#### `types.rs` (new)
Core domain types shared across modules. This module re-exports commonly
used types from rust-bitcoin and defines mining-specific types. Using
rust-bitcoin provides battle-tested implementations of Bitcoin primitives
while avoiding reinventing fundamental types.

#### `config.rs` (new)
Configuration management:
- TOML file parsing with serde
- Config validation
- Hot-reload support via file watching
- Default values and config merging

#### `daemon.rs` (new)
Daemon lifecycle management:
- systemd notification support
- PID file handling
- Resource cleanup
- Health monitoring

### Hardware Communication Layer

The hardware communication layer is organized in distinct levels, each
with a specific responsibility. This design enables maximum code reuse and
testability.

```
┌──────────────────────────────────────────────────────────────┐
│                     Board Implementation                     │
│   orchestrates all components for a specific board model     │
└──────────────────────────────────────────────────────────────┘
               │                               │                
               │                               │                
┌─────────────────────────────┐ ┌──────────────────────────────┐
│         Peripheral          │ │            ASICs             │
│     peripheral drivers      │ │   ┌──────────────────────┐   │
│ ┌──────┐ ┌───────┐┌───────┐ │ │   │     BM13xx Family    │   │
│ │ TMP75│ │INA260 ││EMC2101│ │ │   │  ┌──────┐ ┌──────┐   │   │
│ └───┬──┘ └───┬───┘└───┬───┘ │ │   │  │BM1370│ │BM1362│   │   │
└─────────────────────────────┘ │   │  └───┬──┘ └───┬──┘   │   │
      │        │        │       │   └──────────────────────┘   │
      └────────┼────────┘       └──────────────────────────────┘
               │                           └────┬───┘           
               └───────────────┬────────────────┘               
                               │                                
┌──────────────────────────────────────────────────────────────┐
│            Hardware Abstraction Layer (hw_trait)             │
│          I2C, SPI, GPIO, Serial trait definitions            │
└──────────────────────────────────────────────────────────────┘
                               │                                
┌──────────────────────────────────────────────────────────────┐
│              Management Protocols (mgmt_protocol)            │
│           Implement hw_traits over board protocols           │
└──────────────────────────────────────────────────────────────┘
                               │                                
┌──────────────────────────────────────────────────────────────┐
│                    Transport Layers                          │
│              Physical connections to boards                  │
│            USB Serial, PCIe, Ethernet (future)               │
└──────────────────────────────────────────────────────────────┘
```

#### `transport/`
Physical connections to hash boards. This layer handles:
- USB device discovery and enumeration via libudev (Linux)
- Real-time hotplug detection and events
- Opening and configuring serial ports
- Managing dual-channel devices (management + data channels)
- No protocol knowledge - just raw byte streams
- Emits `BoardConnected`/`BoardDisconnected` events

Platform support:
- **Linux**: Uses libudev for USB device discovery and monitoring
- **macOS**: Planned (will use IOKit framework)

The transport layer discovers devices based on VID/PID and associated serial
ports, then emits events to the backplane for board initialization.

#### `mgmt_protocol/`
Protocol implementations for hash board management. This layer:
- Implements specific packet formats (e.g., bitaxe-raw's 7-byte header)
- Provides protocol operations: GPIO control, ADC readings, I2C passthrough
- Handles command/response sequencing and error checking
- Translates high-level operations into protocol packets
- Provides adapters that implement `hw_trait` interfaces over protocols

#### `hw_trait/`
Hardware interface traits and native implementations. This layer:
- Defines traits like `I2c`, `Spi`, `Gpio`, `Serial` that drivers use
- Allows the same driver to work with, e.g., Linux I2C or I2C-over-protocol
- Provides native Linux implementations for local buses

#### `peripheral/`
Reusable drivers for peripheral chips (not mining ASICs). These drivers:
- Are generic over `hw_trait` interfaces (e.g, work with any `I2c` implementation)
- Can be tested with mock implementations
- Used on various board types

#### `asic/` (Mining ASIC drivers)
Mining ASIC drivers - the heart of mining operations:
- Implements drivers for different ASIC families (BM13xx, etc.)
- Manages chip initialization, frequency control, and status
- Communicates via hw_trait::Serial (which may be a direct passthrough or
  tunneled through mgmt_protocol)
- Handles chip-specific protocols and command sequences

### Example: Board Implementation Layering

Here's how the architectural layers compose in practice:

```rust
// board/bitaxe_gamma.rs
use crate::mgmt_protocol::bitaxe_raw::BitaxeRawProtocol;
use crate::peripheral::{TMP75, INA260};
use crate::asic::bm13xx::{BM1370, ChipChain};
use crate::board::Board;

pub struct BitaxeGammaBoard {
    protocol: BitaxeRawProtocol,
    asic_chain: ChipChain<BM1370>,
    temp_sensor: TMP75<BitaxeRawI2c>,
    power_monitor: INA260<BitaxeRawI2c>,
}

impl BitaxeGammaBoard {
    // Each board type knows its exact construction requirements
    pub async fn new(mgmt_serial: impl Serial, data_serial: impl Serial) -> Result<Self> {
        // Create protocol handler
        let mut protocol = BitaxeRawProtocol::new(mgmt_serial);
        
        // Initialize hardware via protocol
        protocol.set_gpio(3, false).await?;  // Reset ASIC (GPIO 3)
        tokio::time::sleep(Duration::from_millis(100)).await;
        protocol.set_gpio(3, true).await?;   // Release reset
        
        // Create ASIC chain with single BM1370
        let mut asic_chain = ChipChain::<BM1370>::new(data_serial);
        asic_chain.enumerate_chips().await?;
        asic_chain.set_frequency(500.0).await?;
        
        // Create I2C adapter from protocol
        let i2c = protocol.i2c_adapter();
        
        // Create peripheral drivers
        let temp_sensor = TMP75::new(i2c.clone(), 0x48);
        let power_monitor = INA260::new(i2c, 0x40);
        
        Ok(Self {
            protocol,
            asic_chain,
            temp_sensor,
            power_monitor,
        })
    }
}
```

The backplane responds to transport discovery events by looking up the
appropriate board type and constructing it:

```rust
// backplane.rs - simplified
async fn handle_transport_connected(&mut self, event: TransportEvent) {
    // Look up board type in registry based on transport properties
    let board_type = match &event {
        TransportEvent::UsbConnected { vid, pid, .. } => {
            self.board_registry.find_by_usb(*vid, *pid)?
        }
        TransportEvent::PcieConnected { device_id, .. } => {
            self.board_registry.find_by_pcie(*device_id)?
        }
    };
    
    // Each board type knows how to construct itself from transport
    let board: Box<dyn Board> = match board_type {
        BoardType::BitaxeGamma => {
            // Extract dual serial ports from transport event
            let (mgmt_port, data_port) = event.into_dual_serial()?;
            
            // Board constructor handles all protocol and driver setup
            Box::new(BitaxeGammaBoard::new(mgmt_port, data_port).await?)
        }
        
        BoardType::S19Pro => {
            // S19 uses a single multiplexed serial connection
            let serial = event.into_serial()?;
            
            // S19 board handles its own protocol complexity
            Box::new(S19ProBoard::new(serial).await?)
        }
        
        BoardType::Avalon1366 => {
            // Some boards might use multiple transports
            let (control, chain1, chain2, chain3) = event.into_quad_serial()?;
            
            Box::new(AvalonBoard::new(control, [chain1, chain2, chain3]).await?)
        }
    };
    
    // Register board with scheduler
    self.scheduler.add_board(board);
}
```

This architecture achieves several key objectives:

1. **Driver Reusability**: The same peripheral driver (e.g., TMP75 temp sensor) works on:
   - Different boards (Bitaxe, S19, Avalon)
   - Different I2C implementations (Linux native, USB-tunneled, protocol-based)
   - Test environments with mock I2C
2. **ASIC Driver Portability**: BM13xx drivers work with any Serial implementation:
   - Direct serial port on development boards
   - Protocol-multiplexed chains on production miners
   - Mock serial for testing
3. **Clean Abstractions**: Drivers depend only on hw_trait interfaces, not specific hardware
4. **Testability**: Every driver can be tested in isolation with trait mocks
5. **Composability**: Boards compose existing drivers rather than reimplementing:
   - Pick the ASIC driver for your chips
   - Pick the peripheral drivers for your sensors
   - Wire them together with your board's specific transport

### Mining Logic

#### `board/`
Hash board implementations that compose all hardware elements:
- `Board` trait defining the interface for all hash boards
- `bitaxe.rs` - Original Bitaxe board implementation
- `ember_one.rs` - EmberOne board using layered architecture
- `registry.rs` - Board type registry for dynamic instantiation

Board responsibilities:
- Hardware initialization and lifecycle management
- Creating hash threads for work assignment
- Providing threads with chip communication mechanisms (implementation-specific)
- Optionally sharing peripheral access to enable thread autonomy
- Monitoring hardware health (temperature, power, faults)
- Coordinating shutdown and cleanup

The board-thread relationship is intentionally board-specific to allow diverse
hardware designs. Some boards may give threads direct hardware access, while
others may mediate all hardware operations.

#### `asic/`
Mining ASIC drivers:
- Current: `bm13xx/` family driver with protocol documentation
- Future: Other ASIC families (BM1397, etc.)
- Handles: work distribution, nonce collection, frequency control
- Communicates through hw_trait layer for maximum flexibility

#### Hash Threads

Hash threads are the scheduler's abstraction for mining work assignment. The
`HashThread` trait defines only the minimal interface the scheduler needs:
- Work assignment methods (`update_work()`, `replace_work()`, `go_idle()`)
- Event reporting channel for shares and status updates
- Capability and status queries
- Future: Scheduler will control thread hashrate for power management

Everything else---chip communication, peripheral access, internal
architecture---is board-specific implementation detail. This design allows
radically different board architectures:

**Communication with chips**: Boards may transfer serial I/O ownership to
threads, use message passing, share a communication bus, or any other pattern
that suits the hardware.

**Peripheral access**: Threads may need direct access to board peripherals for
real-time optimization (voltage tuning, thermal throttling, fault recovery).
Boards may provide this access via shared references (e.g., Arc-wrapped
peripherals), message-based requests, or keep all peripheral control
board-mediated. The choice depends on the hardware design and optimization
requirements.

**Shutdown coordination**: Board-to-thread shutdown signaling is
implementation-specific, using watch channels, cancellation tokens, or custom
mechanisms as appropriate.

This flexibility enables diverse hardware designs without requiring scheduler
changes. The scheduler sees only a uniform HashThread interface, while boards
and threads collaborate in hardware-appropriate ways.

#### `backplane.rs`
Communication substrate between boards and scheduler:
- Listens to `transport` discovery events
- Identifies board types (USB VID/PID or probing)
- Creates/destroys board instances
- Maintains active board registry
- Extracts hash threads from boards and routes to scheduler
- Boards remain active for hardware lifecycle management
- Coordinates emergency shutdowns and hotplug

#### `job_source/`
Unified interface for all mining job sources:
- `traits.rs` - `JobSource` trait for all sources
- `stratum_v1.rs` - Stratum v1 pool client
- `stratum_v2.rs` - Stratum v2 pool client (next-gen protocol)
- `solo.rs` - Direct Bitcoin node connection for solo mining
- `dummy.rs` - Synthetic job generator for power/thermal load management
- Provides consistent interface for scheduler regardless of job origin

#### `pool/` (Deprecated - Moving to job_source)
Mining pool client implementations:
- Being replaced by `job_source/stratum_v1` and `job_source/stratum_v2`
- Retained temporarily for backward compatibility

#### `scheduler.rs`
Orchestrates the mining operation:
- Receives work from any `JobSource` implementation
- Distributes work to boards/chips
- Collects and routes shares
- Implements work scheduling strategies
- Manages board lifecycle

#### `job_generator.rs` (Deprecated - Moving to job_source)
Local job generation for testing and solo mining:
- Being replaced by `job_source/dummy` and `job_source/solo`
- Retained temporarily for backward compatibility

### API and Observability

#### `api/`
HTTP API server (new):
- Built on Axum (async web framework)
- RESTful endpoints for status, control, configuration
- WebSocket support for real-time updates
- OpenTelemetry integration
- Prometheus metrics endpoint

#### `tracing.rs`
Structured logging and observability:
- tracing subscriber setup
- journald or stdout output
- Log level configuration
- Performance tracing spans

## Data Flow

```
Mining Pool <--[Stratum]--> job_source::StratumV1
Bitcoin Node <--[RPC]-----> job_source::Solo
Dummy Work Generator -----> job_source::Dummy
                                   |
                                   v
                            scheduler::Scheduler
                                   |
                    +--------------+--------------+
                    |                             |
                    v                             v
             board::Board                  board::Board
                    |                             |
                    v                             v
          asic::BM13xxChip              asic::BM13xxChip
                    |                             |
                    v                             v
       transport::UsbSerial        transport::UsbSerial


Hotplug Flow:
USB Device ──> transport ──> BoardConnected Event ──> backplane
                                                           |
                                                           v
                                                    Creates Board
                                                           |
                                                           v
                                                  Registers with Scheduler
                                                           |
                                                           v
                                              Scheduler talks directly to Board
```

## Share Processing Flow

Shares flow through three filtering stages from hardware to pool:

1. **Chip Hardware Target**: ASICs are configured with a low difficulty target
   (e.g., diff 100-1000) to generate frequent shares for health monitoring.
   At diff 100, a 500 GH/s chip produces ~1 share/second.

2. **Thread Processing**: HashThreads receive ALL chip shares and compute the
   hash for each one (required to determine what difficulty it meets). Since
   the hash computation cost is already paid, threads forward all shares to
   the scheduler rather than filtering them. This keeps threads simple and
   gives the scheduler ground truth for per-thread hashrate measurement.

3. **Scheduler Filtering**: The scheduler filters shares against the job target
   before forwarding to the JobSource. Only pool-worthy shares are submitted.
   The scheduler uses all shares for internal statistics and health monitoring.

**Message Volume**: Even at aggressive chip targets, message volume is
manageable. A 12-chip board at diff 100 produces ~10-15 shares/second,
well within mpsc channel capacity.

**Design Rationale**: Forwarding all shares centralizes filtering logic in the
scheduler, provides accurate per-thread monitoring, and simplifies thread
implementation. The alternative (filtering in threads) would require duplicating
target comparison logic and prevent the scheduler from measuring actual hardware
performance.

## Async Patterns

All I/O operations are async using Tokio:
- Serial communication uses `tokio-serial`
- HTTP server uses `axum` (built on Tokio)
- Background tasks use `tokio::spawn`
- Graceful shutdown via `CancellationToken`
- Concurrent operations via `TaskTracker`

## Extension Points

The architecture supports extension through several mechanisms:

1. **New Board Types**: Implement the `Board` trait
2. **New ASIC Families**: Add modules under `asic/`
3. **New Pool Protocols**: Implement `PoolClient` trait
4. **New Management Protocols**: Add under `mgmt_protocol/`
5. **Custom Schedulers**: Pluggable scheduling strategies
6. **Additional Peripheral Chips**: Add drivers to `peripheral/`
7. **New Connection Types**: Extend `transport/` (PCIe, Ethernet)

## Configuration

Configuration is managed through TOML files with hot-reload support:
- `/etc/mujina/mujina.toml` - System configuration
- Board-specific settings
- Pool credentials and priorities
- Temperature limits and safety settings
- API server configuration

## Security Considerations

- No hardcoded credentials
- TLS support for API endpoints
- Privilege dropping after startup
- Isolated board control (no direct chip access from API)
- Rate limiting on API endpoints

## User Interfaces

Mujina-miner provides multiple interfaces for different use cases:

### Web Application (Separate Repository)
The primary user interface is a modern web application that lives in a
separate repository (`mujina-web`):
- Built with modern web technologies (React/Vue/Svelte)
- Communicates exclusively through the HTTP API
- Provides rich visualizations and easy configuration
- Suitable for remote management
- Can be served by any web server or CDN

**Repository**: `github.com/mujina/mujina-web` (example)

### Command Line Interface (CLI)
Included in this repository as `mujina-cli`:
- Direct API client for automation and scripting
- Supports all daemon operations
- JSON output mode for parsing
- Configuration file management

### Terminal User Interface (TUI)
Included in this repository as `mujina-tui`:
- Interactive terminal dashboard
- Real-time monitoring without web browser
- Ideal for SSH sessions
- Keyboard shortcuts for common operations

### API Client Library
The `api_client` module provides:
- Rust types for all API requests/responses
- Async HTTP client using reqwest
- WebSocket support for real-time data
- Shared between CLI and TUI
- Could be published as separate crate for third-party tools

## Repository Structure

This repository contains the core miner daemon and terminal-based tools:
```
mujina-miner/
├── Cargo.toml
├── README.md
├── docs/
│   ├── architecture.md    # This file
│   ├── api.md            # API documentation
│   └── deployment.md     # Installation guide
├── configs/
│   └── example.toml      # Example configuration
├── src/                  # Rust source code
├── systemd/
│   └── mujina-minerd.service
└── debian/               # Debian packaging
```

The web interface lives in a separate repository to allow:
- Independent development cycles
- Different programming languages
- Separate CI/CD pipelines
- Alternative web UIs from the community
