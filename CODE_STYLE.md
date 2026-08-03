# Code Style Guide

This document defines the formatting and mechanical style rules for the
mujina-miner project. For best practices and design patterns, see
CODING_GUIDELINES.md.

Rules are labeled with stable identifiers (e.g., `S.fmt`, `D.md.wrap`) for
easy reference in code reviews and discussions.

## Rust Code Style

### Formatting [S.fmt](#S.fmt)

We use `rustfmt` with default settings. Always run before committing:
```bash
cargo fmt
```

### Linting [S.lint](#S.lint)

We use `clippy` to catch common mistakes. Fix all warnings:
```bash
cargo clippy -- -D warnings
```

### Naming Conventions [S.name](#S.name)

Follow Rust naming conventions:
- Types use `UpperCamelCase` (e.g., `BoardConfig`, `ChipType`)
- Functions/Methods use `snake_case` (e.g., `send_work`, `get_status`)
- Variables use `snake_case` (e.g., `hash_rate`, `temp_sensor`)
- Constants use `SCREAMING_SNAKE_CASE` (e.g., `MAX_CHIPS`, `DEFAULT_FREQ`)
- Modules use `snake_case` (e.g., `board`, `chip`, `pool`)

### Module Organization [S.mod](#S.mod)

Organize module contents in this order:

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

### Top-Down Ordering [S.topdown](#S.topdown)

Order a module's items top down: public interface first,
implementation after. A file then reads from its most important code
to its least.

This is a stylistic choice. Technically either order works, but Rust
lets us do something we couldn't do in C and C++: order a file by
importance instead of by dependency. A C function must be declared
before it is called, so definitions come ahead of their callers unless
the file repeats them as prototypes. Rust resolves items wherever they
appear.

- Public items come before private ones, in the types group and in
  the functions group alike.
- The module's main type comes before the supporting types that
  appear in its signatures.
- Among private items, callers come before callees.

```rust
pub struct Client { /* the subject of the module */ }

impl Client {
    pub fn new(/* ... */) -> Self { /* ... */ }
    pub async fn run(/* ... */) { /* calls step() */ }

    async fn step(/* ... */) { /* private, below its caller */ }
}

struct Pending { /* private supporting type */ }

fn decode(/* ... */) { /* private helper, below what calls it */ }
```

S.mod fixes the order of the groups; this rule orders the items
inside each one.

### Module Layout [S.modlayout](#S.modlayout)

Choose between the two module-with-submodules layouts based on
where the real code lives:

- **`foo/mod.rs`** -- when the root file is mostly re-exports and
  wiring. The directory *is* the module (e.g., `peripheral/mod.rs`
  declaring driver submodules).
- **`foo.rs` + `foo/`** (2024 edition style) -- when the root file
  has substantial code and submodules are supporting detail (e.g.,
  `temperature.rs` with a `temperature/conversions.rs` submodule).

### Lint Attributes [S.expect](#S.expect)

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

## Documentation Format

### Markdown Files [D.md](#D.md)

- Wrap prose at 72 characters (enforced by .editorconfig), the
  same width CONTRIBUTING.md sets for commit message bodies.
  Typography puts a comfortable reading line at 45--75
  characters, and 72 stays under 80 columns in a diff, under
  git log's indent, and when quoted in review
- Indent nested and continuation lines by 4 spaces
- Tables, ASCII diagrams, fenced code blocks, and link definitions
  may run wider; keep their alignment instead
- Use hard line breaks, not soft wrapping
- Use proper heading hierarchy (don't skip levels)
- Include code examples where helpful
- Nest code blocks using more backticks in outer block than inner block

### Code Documentation [D.code](#D.code)

Use standard Rust documentation format:
- Module-level documentation with `//!`
- Item documentation with `///`
- Include examples for complex functionality
- Document panics, errors, and safety requirements

See CODING_GUIDELINES.md for guidance on what to document and comment
style.

### Commits [D.commit](#D.commit)

See CONTRIBUTING.md for commit message format, conventional commit
types, and atomic commit guidelines (revertability, bisectability,
reviewability).
