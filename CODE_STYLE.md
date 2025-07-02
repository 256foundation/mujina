# Code Style Guide

This document defines the coding standards for the mujina-miner project.
Following these guidelines ensures consistency and maintainability across
the codebase.

## General Principles

1. **Clarity over cleverness** - Write code that is easy to understand
2. **Consistency** - Follow existing patterns in the codebase
3. **Simplicity** - Prefer simple solutions over complex ones
4. **Documentation** - Document why, not what

## Rust Code Style

### Formatting

We use `rustfmt` with default settings. Always run before committing:
```bash
cargo fmt
```

### Linting

We use `clippy` to catch common mistakes. Fix all warnings:
```bash
cargo clippy -- -D warnings
```

### Naming Conventions

Follow Rust naming conventions:
- **Types**: `UpperCamelCase` (e.g., `BoardConfig`, `ChipType`)
- **Functions/Methods**: `snake_case` (e.g., `send_work`, `get_status`)
- **Variables**: `snake_case` (e.g., `hash_rate`, `temp_sensor`)
- **Constants**: `SCREAMING_SNAKE_CASE` (e.g., `MAX_CHIPS`, `DEFAULT_FREQ`)
- **Modules**: `snake_case` (e.g., `board`, `chip`, `pool`)

### Module Organization

```rust
// 1. Module documentation
//! Brief module description.
//! 
//! Longer explanation if needed.

// 2. Imports (grouped and sorted)
use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::Mutex;

use crate::types::{Job, Share};

// 3. Constants
const BUFFER_SIZE: usize = 1024;

// 4. Types (structs, enums)
pub struct BoardManager {
    boards: HashMap<String, Board>,
}

// 5. Implementations
impl BoardManager {
    pub fn new() -> Self {
        Self {
            boards: HashMap::new(),
        }
    }
}

// 6. Functions
pub async fn discover_boards() -> Result<Vec<Board>> {
    // Implementation
}

// 7. Tests
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_board_discovery() {
        // Test implementation
    }
}
```

### Error Handling

Use appropriate error handling patterns:

```rust
// Application code: use anyhow
use anyhow::{Context, Result};

pub async fn connect_to_pool(url: &str) -> Result<PoolClient> {
    let client = PoolClient::connect(url)
        .await
        .context("Failed to connect to mining pool")?;
    Ok(client)
}

// Library code: use thiserror
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

### Lint Attributes

Use `#[expect(...)]` instead of `#[allow(...)]` for intentional lint suppressions.
This makes the intent explicit and will warn if the suppression becomes unnecessary:

```rust
// Good: Use expect with a reason
#[expect(dead_code, reason = "Will be used when pool support is implemented")]
struct PoolConnection {
    url: String,
}

// Good: For unused parameters in trait implementations
impl Handler for MyHandler {
    fn handle(&self, #[expect(unused_variables)] _ctx: Context) {
        // Implementation doesn't need context yet
    }
}

// Bad: Don't use allow
#[allow(dead_code)]  // Avoid this
struct TempStruct {
    field: String,
}
```

The `expect` attribute requires a reason, making code reviews easier and helping
future maintainers understand why the suppression exists. When the code changes
and the suppression is no longer needed, the compiler will warn about it.

### Async Code

Follow Tokio best practices:

```rust
// Good: Concurrent operations
let (result1, result2) = tokio::join!(
    fetch_pool_work(),
    check_board_status()
);

// Good: Proper cancellation
async fn long_running_task(shutdown: CancellationToken) {
    tokio::select! {
        _ = do_work() => {},
        _ = shutdown.cancelled() => {
            info!("Task cancelled");
        }
    }
}

// Bad: Sequential when could be concurrent
let result1 = fetch_pool_work().await;
let result2 = check_board_status().await;
```

### Comments and Documentation

```rust
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
```

### Testing

Write comprehensive tests:

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

    // Test with fixtures
    #[test]
    fn test_frame_parsing() {
        let frame_data = include_bytes!("../test_data/valid_frame.bin");
        let frame = Frame::parse(frame_data).unwrap();
        assert_eq!(frame.command, Command::ReadReg);
    }
}
```

## Documentation Style

### Markdown Files

- Wrap lines at 79 characters (enforced by .editorconfig)
- Use hard line breaks, not soft wrapping
- Use proper heading hierarchy (don't skip levels)
- Include code examples where helpful

### Code Documentation

Document all public APIs:
- Module-level documentation with `//!`
- Item documentation with `///`
- Include examples for complex functionality
- Document panics, errors, and safety requirements

### Commit Messages

Follow conventional commits:
```
feat(board): add temperature monitoring

Implements continuous temperature monitoring for all connected boards.
Readings are cached and updated every 5 seconds.

- Add TemperatureMonitor struct
- Integrate with board lifecycle
- Expose readings via API

Closes #45
```

## Project-Specific Conventions

### Hardware Interaction

```rust
// Always trace hardware communication
trace!("Sending to chip: {:02x?}", data);

// Use timeouts for hardware operations
timeout(Duration::from_secs(5), chip.send_work(job))
    .await
    .context("Timeout sending work to chip")?;

// Check hardware state before operations
if !board.is_ready() {
    return Err(anyhow!("Board not ready"));
}
```

### Protocol Implementation

```rust
// Define protocol constants clearly
pub mod constants {
    pub const FRAME_HEADER: u8 = 0xAA;
    pub const FRAME_TAIL: u8 = 0x55;
    pub const MAX_FRAME_SIZE: usize = 256;
}

// Use builders for complex protocol messages
let command = CommandBuilder::new()
    .with_register(Register::Frequency)
    .with_value(600_000_000)
    .build();
```

### Safety and Resource Management

```rust
// Always implement Drop for hardware resources
impl Drop for Board {
    fn drop(&mut self) {
        if let Err(e) = self.shutdown() {
            error!("Failed to shutdown board: {}", e);
        }
    }
}

// Use RAII patterns
let _guard = board.lock_communication().await;
// Communication automatically unlocked when guard drops
```

## Continuous Improvement

This style guide is a living document. If you find patterns that work well
or identify areas for improvement, please propose changes through the
normal contribution process.

Remember: the goal is to make the code easy to understand, maintain, and
extend. When in doubt, favor clarity and consistency.