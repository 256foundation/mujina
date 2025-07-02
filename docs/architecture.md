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
├── board_manager.rs  # Board lifecycle and hotplug management
├── scheduler.rs      # Work scheduling and distribution
├── job_generator.rs  # Local job generation (testing/solo)
├── pool/             # Mining pool connectivity
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
- USB device discovery and enumeration
- Hotplug detection and events
- Opening and configuring serial ports
- Managing dual-channel devices (management + data channels)
- No protocol knowledge - just raw byte streams
- Emits `BoardConnected`/`BoardDisconnected` events

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

The board_manager responds to transport discovery events by looking up the
appropriate board type and constructing it:

```rust
// board_manager.rs - simplified
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
- Manages: ASIC chains, cooling, power delivery, monitoring

#### `asic/`
Mining ASIC drivers:
- Current: `bm13xx/` family driver with protocol documentation
- Future: Other ASIC families (BM1397, etc.)
- Handles: work distribution, nonce collection, frequency control
- Communicates through hw_trait layer for maximum flexibility

#### `board_manager.rs`
Manages board lifecycle and hotplug events:
- Listens to `transport` discovery events
- Identifies board types (USB VID/PID or probing)
- Creates/destroys board instances
- Maintains active board registry
- Routes boards to/from scheduler

#### `pool/`
Mining pool client implementations:
- `traits.rs` - `PoolClient` trait
- `stratum_v1.rs` - Stratum v1 protocol (most common)
- `stratum_v2.rs` - Stratum v2 protocol (future)
- `manager.rs` - Pool failover and switching logic
- Handles: work fetching, share submission, difficulty adjustments

#### `scheduler.rs`
Orchestrates the mining operation:
- Receives work from pools
- Distributes work to boards/chips
- Collects and routes shares
- Implements work scheduling strategies
- Manages board lifecycle

#### `job_generator.rs`
Local job generation for testing and solo mining:
- Generates valid block templates
- Updates timestamp/nonce fields
- Useful for hardware testing without pools

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
Mining Pool <--[Stratum]--> pool::PoolClient
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
USB Device ──> transport ──> BoardConnected Event ──> board_manager
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
