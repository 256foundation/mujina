# Board Support Module

This module contains board-specific implementations for various Bitcoin mining
hardware platforms. Each board implementation provides the necessary logic to
initialize, control, and communicate with specific mining hardware.

## Board Implementations

- [`bitaxe.rs`](bitaxe.rs) - Implementation for Bitaxe Gamma boards
  - See [Bitaxe Gamma Documentation](bitaxe_gamma.md) for detailed hardware
    information

## Board Trait

All board implementations must implement the `Board` trait defined in `mod.rs`,
which provides a common interface for:

- Hardware initialization and reset
- Chip discovery and enumeration
- Mining job distribution
- Result collection
- Peripheral management (fans, temperature, power)
- Graceful shutdown

## Adding New Board Support

To add support for a new board:

1. Create a new implementation file (e.g., `myboard.rs`)
2. Implement the `Board` trait for your board type
3. Register the board with the inventory system using the appropriate VID/PID
4. Create documentation similar to `bitaxe_gamma.md` for your board
5. Add your board to the supported hardware list in the main README

## Common Patterns

Most mining boards share similar components:
- ASIC chips for hashing (BM13xx, BM17xx, etc.)
- Power management (voltage regulators, current monitoring)
- Thermal management (temperature sensors, fan controllers)
- Communication interfaces (UART, I2C, SPI)

The board implementations abstract these hardware-specific details behind the
common `Board` trait interface.