# Coding Guidelines

This document defines coding conventions and best practices for the
mujina-miner project. For formatting and style rules, see CODE_STYLE.md.

Guidelines are labeled with stable identifiers (e.g., `E.anyhow`, `L.structured`)
for easy reference in code reviews and discussions.

## General Principles

1. **Clarity over cleverness** - Write code that is easy to understand
2. **Consistency** - Follow existing patterns in the codebase
3. **Simplicity** - Prefer simple solutions over complex ones
4. **Documentation** - Document why, not what

## Error Handling

### Application Error Handling [E.anyhow](#E.anyhow)

Use `anyhow` for application code:

```rust
use anyhow::{Context, Result};

pub async fn connect_to_pool(url: &str) -> Result<PoolClient> {
    let client = PoolClient::connect(url)
        .await
        .context("Failed to connect to mining pool")?;
    Ok(client)
}
```

### Library Error Types [E.thiserror](#E.thiserror)

Use `thiserror` for library code:

```rust
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ProtocolError {
    #[error("Invalid frame: {0}")]
    InvalidFrame(String),

    #[error("CRC mismatch")]
    CrcMismatch,

    #[error("Timeout waiting for response")]
    Timeout,
}
```

### Error Context [E.context](#E.context)

Chain context to provide useful error messages:

```rust
// Good: Context explains what failed and why it matters
self.i2c
    .write(TPS546_ADDR, &[PMBus::VOUT_COMMAND, value])
    .await
    .context("Failed to set core voltage")?;

// Good: Multiple context layers for complex operations
board.send_job(job)
    .await
    .context("Failed to send job to chip")
    .context("Job distribution aborted")?;

// Bad: Generic context that doesn't add information
operation().await.context("Operation failed")?;
```

## Type Safety

### Newtype Pattern [T.newtype](#T.newtype)

Use newtypes to prevent mixing up similar values:

```rust
pub struct ChipAddress(u8);
pub struct RegisterAddress(u8);

impl ChipAddress {
    pub fn new(addr: u8) -> Option<Self> {
        (addr < 128).then_some(Self(addr))
    }
}
```

### Enums for Fixed Sets [T.enum](#T.enum)

Use enums for fixed sets of values:

```rust
// Good: Enum enforces valid values
pub enum BaudRate {
    Baud115200,
    Baud1M,
}

// Bad: Raw primitives allow invalid values
fn configure_chip(addr: u8, baud: u32) { }

// Good: Types enforce valid values
fn configure_chip(addr: ChipAddress, baud: BaudRate) { }
```

### Builder Pattern [T.builder](#T.builder)

Use builder patterns for complex construction with validation:

```rust
// Good: Builder enforces required fields and validates
let config = PllConfig::new()
    .fb_div(100)
    .ref_div(2)
    .post_div(1)
    .build()?;

// Bad: Constructor with many parameters, easy to mix up
let config = PllConfig::new(100, 2, 1, true, 0x40);
```

## Async Patterns

### Concurrent Operations [A.concurrent](#A.concurrent)

Prefer concurrent operations over sequential when possible:

```rust
// Good: Concurrent operations
let (result1, result2) = tokio::join!(
    fetch_pool_work(),
    check_board_status()
);

// Bad: Sequential when could be concurrent
let result1 = fetch_pool_work().await;
let result2 = check_board_status().await;
```

### Cancellation Tokens [A.cancel](#A.cancel)

Use cancellation tokens for coordinated graceful shutdown. This ensures
hardware resources are properly released:

```rust
use tokio_util::sync::CancellationToken;

async fn board_loop(
    mut board: Board,
    shutdown: CancellationToken,
) -> Result<()> {
    loop {
        tokio::select! {
            result = board.poll_nonces() => {
                handle_nonces(result?).await?;
            }
            _ = shutdown.cancelled() => {
                info!("Shutdown requested");
                break;
            }
        }
    }

    board.shutdown().await
}
```

## Logging Conventions

### Log Levels [L.level](#L.level)

Use appropriate log levels consistently:

- **trace**: Very detailed information for debugging specific issues
- **debug**: Information useful during development
- **info**: Normal operational messages
- **warn**: Unexpected but recoverable situations
- **error**: Error conditions that prevent operation

```rust
trace!(data = ?data, "Sending to chip");
debug!(chip_id = %chip_id, freq_mhz = %freq, "Chip initialized");
info!(board = %board.name(), "Board connected");
warn!(temp_c = %temp, threshold = %TEMP_WARN, "Chip temperature high");
error!(error = %err, "Failed to initialize board");
```

### Structured Logging [L.structured](#L.structured)

Prefer structured logging fields over string interpolation:

```rust
// Good: Structured fields enable querying and filtering
info!(
    chip_id = %id,
    freq_mhz = %freq,
    voltage = %voltage,
    "Chip configured"
);

// Acceptable: Simple messages
info!("Chip {} configured: {}MHz, {}V", id, freq, voltage);
```

Structured logging provides the same human-readable terminal output but
stores data as queryable fields. This enables powerful log analysis when
using log aggregation systems (ELK, Loki, etc.): you can filter by specific
field values, compute statistics, and correlate events across components.
Even if not using aggregation now, structured logs make future integration
easier and cost nothing at runtime.

Use `%` for Display formatting and `?` for Debug formatting:
```rust
// Display formatting with %
info!(voltage = %volts, "Voltage set");

// Debug formatting with ?
trace!(register = ?reg, "Register read");
```

### RUST_LOG Filtering [L.filter](#L.filter)

The `RUST_LOG` environment variable provides powerful runtime filtering,
which means you can log liberally without performance concerns:

```bash
# Show only errors from all modules
RUST_LOG=error cargo run

# Show info and above for entire application
RUST_LOG=info cargo run

# Show trace logging for specific module, info for rest
RUST_LOG=mujina_miner::asic::bm13xx=trace,info cargo run

# Debug one module, trace another, warn for everything else
RUST_LOG=mujina_miner::board::bitaxe=debug,mujina_miner::peripheral::tps546=trace,warn cargo run

# Multiple specific modules at trace level
RUST_LOG=mujina_miner::asic::bm13xx=trace,mujina_miner::board=trace cargo run
```

Disabled log statements have **minimal runtime cost**---just a branch check
to see if the log level is enabled. The expensive parts (string formatting,
allocation, I/O) are skipped when filtered out. This means:

- Add trace!() liberally for protocol debugging
- Add debug!() for state transitions and important operations
- Don't worry about performance impact of verbose logging

Users can enable exactly the verbosity they need for debugging specific
issues without recompiling. When investigating a protocol problem, they can
enable trace logging for just that module while keeping the rest at info
level.

### Logging vs Comments [L.comment](#L.comment)

Well-placed log statements often eliminate the need for comments:

```rust
// Good: Log statement explains what's happening
debug!("Waiting for chip to stabilize after voltage change");
tokio::time::sleep(Duration::from_millis(100)).await;

// Bad: Comment duplicates what log already says
// Wait for chip to stabilize
debug!("Waiting for chip to stabilize after voltage change");
tokio::time::sleep(Duration::from_millis(100)).await;

// Bad: Too much logging clutters the code
trace!("Entering initialization function");
debug!("Setting up GPIO pins");
let gpio = setup_gpio()?;
debug!("GPIO pins set up successfully");
trace!("Configuring I2C bus");
let i2c = setup_i2c()?;
trace!("I2C bus configured");
// ... this gets hard to read
```

Log at meaningful boundaries and state changes, not every statement. The
code should remain readable without having to mentally filter out log
statements.

## Comments and Documentation

### Public API Documentation [C.public](#C.public)

Document all public APIs:

````rust
/// Sends a job to the specified chip.
///
/// # Arguments
///
/// * `chip_id` - The target chip identifier
/// * `job` - The mining job to send
///
/// # Returns
///
/// Returns `Ok(())` if the job was sent successfully, or an error if
/// communication failed.
///
/// # Example
///
/// ```
/// let job = Job::new(block_header, target);
/// board.send_job(0, job).await?;
/// ```
pub async fn send_job(&mut self, chip_id: u8, job: Job) -> Result<()> {
    // Implementation
}
````

### Explain Why, Not What [C.why](#C.why)

Write comments that explain "why" not "what" - the code shows what,
comments explain reasoning:

```rust
// Bad: Increment counter by 1
counter += 1;

// Good: Track retry attempts for exponential backoff
counter += 1;

// Bad: This function processes data
fn process_data() { ... }

// Good: Preserves original timestamps during migration to avoid breaking
// dependent services that rely on creation_date ordering
fn process_data() { ... }
```

### Use Comments Sparingly [C.sparse](#C.sparse)

Use inline comments sparingly, only for non-obvious logic. Keep comments
concise - if it needs multiple lines, consider refactoring the code. Update
comments when code changes - stale comments are worse than no comments.

## Testing

### Unit Tests [TEST.unit](#TEST.unit)

Write comprehensive unit tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    // Unit test
    #[test]
    fn test_crc_calculation() {
        let data = b"test data";
        let crc = calculate_crc(data);
        assert_eq!(crc, 0x1234);
    }

    // Async test
    #[tokio::test]
    async fn test_board_connection() {
        let board = Board::connect("/dev/ttyUSB0").await.unwrap();
        assert!(board.is_connected());
    }
}
```

### Use Known-Good Captures [TEST.fixture](#TEST.fixture)

Use known-good frame captures in tests where possible:

```rust
#[test]
fn test_frame_parsing() {
    let frame_data = include_bytes!("../test_data/valid_frame.bin");
    let frame = Frame::parse(frame_data).unwrap();
    assert_eq!(frame.command, Command::ReadReg);
}
```

### Round-Trip Tests [TEST.roundtrip](#TEST.roundtrip)

Write round-trip tests for protocol encoding/decoding to ensure consistency.

### Mock Hardware [TEST.mock](#TEST.mock)

Mock hardware interfaces for unit testing. Physical hardware is required
for integration testing. Always ask before running on hardware.

## Hardware Interaction

### Trace Hardware Communication [H.trace](#H.trace)

Always trace hardware communication:

```rust
trace!("Sending to chip: {:02x?}", data);
```

### Use Timeouts [H.timeout](#H.timeout)

Use timeouts for hardware operations:

```rust
timeout(Duration::from_secs(5), chip.send_work(job))
    .await
    .context("Timeout sending work to chip")?;
```

### Check Hardware State [H.state](#H.state)

Check hardware state before operations:

```rust
if !board.is_ready() {
    return Err(anyhow!("Board not ready"));
}
```

Document hardware protocols and timing requirements. Ensure hardware
resources are properly released on shutdown.

## Protocol Implementation

### Protocol Constants [P.const](#P.const)

Define protocol constants clearly:

```rust
pub mod constants {
    pub const FRAME_HEADER: u8 = 0xAA;
    pub const FRAME_TAIL: u8 = 0x55;
    pub const MAX_FRAME_SIZE: usize = 256;
}
```

### Protocol Message Builders [P.builder](#P.builder)

Use builders for complex protocol messages:

```rust
let command = CommandBuilder::new()
    .with_register(Register::Frequency)
    .with_value(600_000_000)
    .build();
```

## Resource Management

### Implement Drop for Resources [R.drop](#R.drop)

Always implement Drop for hardware resources:

```rust
impl Drop for Board {
    fn drop(&mut self) {
        if let Err(e) = self.shutdown() {
            error!("Failed to shutdown board: {}", e);
        }
    }
}
```

### Use RAII Patterns [R.raii](#R.raii)

Use RAII patterns for automatic cleanup:

```rust
let _guard = board.lock_communication().await;
// Communication automatically unlocked when guard drops
```

## Dependencies

### Evaluate Dependencies Carefully [DEP.eval](#DEP.eval)

Evaluate new dependencies carefully:

- Is the crate well-maintained and widely used?
- Does it bring significant value over implementing ourselves?
- What is the transitive dependency cost?
- Does it have a compatible license?
- Is it stable (1.0+) or are breaking changes expected?

Justify dependency additions in commit messages and pull requests.

## Continuous Improvement

This document is a living document. If you find patterns that work well or
identify areas for improvement, please propose changes through the normal
contribution process.

Remember: the goal is to make the code easy to understand, maintain, and
extend. When in doubt, favor clarity and consistency.
