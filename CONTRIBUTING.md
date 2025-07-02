# Contributing to Mujina Miner

Thank you for your interest in contributing to mujina-miner! This document
provides guidelines and instructions for contributing to the project.

## Code of Conduct

This project adheres to a Code of Conduct that all contributors are expected
to follow. Please be respectful and constructive in all interactions.

## Getting Started

### Prerequisites

- Rust toolchain (stable)
- Linux development environment
- Git
- Optional: Hardware for testing (Bitaxe boards, etc.)

### Setting Up Your Development Environment

1. Fork the repository on GitHub
2. Clone your fork:
   ```bash
   git clone https://github.com/YOUR_USERNAME/mujina-miner.git
   cd mujina-miner
   ```
3. Add the upstream repository:
   ```bash
   git remote add upstream https://github.com/mujina/mujina-miner.git
   ```
4. Create a feature branch:
   ```bash
   git checkout -b feature/your-feature-name
   ```

## Development Process

### Before You Start

1. Check existing issues and pull requests to avoid duplicate work
2. For significant changes, open an issue first to discuss the approach
3. Read the architecture documentation in `docs/architecture.md`
4. Familiarize yourself with the code style guide in `CODE_STYLE.md`

### Making Changes

1. Write clean, idiomatic Rust code
2. Follow the project's module structure
3. Add tests for new functionality
4. Update documentation as needed
5. Ensure all tests pass: `cargo test`
6. Run clippy: `cargo clippy -- -D warnings`
7. Format your code: `cargo fmt`

### Testing

- **Unit tests**: Required for all new functionality
- **Integration tests**: For cross-module functionality
- **Hardware tests**: Mark with `#[ignore]` and document requirements
- **Protocol tests**: Use captured data when possible

Example test structure:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_functionality() {
        // Your test here
    }

    #[test]
    #[ignore] // Requires hardware
    fn test_with_hardware() {
        // Hardware-dependent test
    }
}
```

### Documentation

- Add doc comments to all public items
- Update module-level documentation
- Include examples in doc comments where helpful
- Keep markdown files wrapped at 79 characters
- Update architecture docs for significant changes
- Use asciiflow.com for creating ASCII diagrams in documentation

### Commit Messages

Write proper commit messages following these guidelines (adapted from the
Linux kernel contribution standards):

#### The Seven Rules of a Great Commit Message

1. **Separate subject from body with a blank line**
2. **Limit the subject line to 50 characters**
3. **Capitalize the subject line**
4. **Do not end the subject line with a period**
5. **Use the imperative mood in the subject line**
6. **Wrap the body at 72 characters**
7. **Use the body to explain what and why vs. how**

#### Format

```
type(scope): Subject in imperative mood

Longer explanation of what this commit does and why it was necessary.
The body should provide context for the change and explain what problem
it solves.

Wrap the body at 72 characters. Use the body to explain what changed
and why, not how (the code shows how).

Further paragraphs come after blank lines.

- Bullet points are okay too
- Use a hyphen or asterisk for bullets

Fixes: #123
Closes: #456
See-also: #789
```

#### Write Atomic Commits

Each commit should be a single logical change. Don't make several logical
changes in one commit. For example, if a patch fixes a bug and optimizes
the performance of a feature, split it into two separate commits.

**Good commit separation:**
- Commit 1: Fix buffer overflow in protocol parser
- Commit 2: Optimize protocol parser performance

**Bad commit (does too much):**
- Fix buffer overflow and optimize parser performance

#### Use Imperative Mood

Write your commit message as if you're giving orders to the codebase:
- ✅ "Add temperature monitoring to board controller"
- ✅ "Fix race condition in share submission"
- ✅ "Refactor protocol handler to use async/await"
- ❌ "Added temperature monitoring"
- ❌ "Fixes race condition"
- ❌ "Refactoring protocol handler"

A properly formed Git commit subject should complete this sentence:
"If applied, this commit will _your subject here_"

#### Types
- `feat`: Add a new feature
- `fix`: Fix a bug
- `docs`: Change documentation only
- `style`: Change code style (formatting, missing semicolons, etc.)
- `refactor`: Refactor code without changing functionality
- `perf`: Improve performance
- `test`: Add or correct tests
- `chore`: Update build process, dependencies, etc.

#### Examples

**Good commit message:**
```
fix(board): prevent double-free in shutdown sequence

The board shutdown sequence could trigger a double-free when called
multiple times due to missing state check. This adds a proper state
machine to track shutdown progress and prevent multiple cleanup attempts.

The issue was discovered during stress testing with rapid board
connect/disconnect cycles.

Fixes: #234
```

**Another good example:**
```
feat(scheduler): implement work-stealing algorithm

Replace the simple round-robin scheduler with a work-stealing algorithm
that better balances load across multiple boards. Idle boards now steal
work from busy boards' queues.

Performance testing shows 15% improvement in share submission rate with
heterogeneous board configurations.
```

## Pull Request Process

1. Update your branch with the latest upstream changes:
   ```bash
   git fetch upstream
   git rebase upstream/main
   ```

2. Push your branch to your fork:
   ```bash
   git push origin feature/your-feature-name
   ```

3. Create a pull request on GitHub with:
   - Clear title describing the change
   - Description of what changed and why
   - Reference to any related issues
   - Screenshots/logs if applicable

4. Address review feedback promptly

5. Once approved, your PR will be merged

## Areas for Contribution

### Good First Issues

Look for issues labeled `good first issue` for beginner-friendly tasks:
- Documentation improvements
- Test coverage additions
- Small bug fixes
- Code cleanup

### High-Priority Areas

- Pool protocol implementations (Stratum v2)
- Additional ASIC chip support
- Hardware monitoring and safety features
- API endpoint implementations
- Performance optimizations

### Feature Requests

Check the roadmap in `CLAUDE.md` and GitHub issues for planned features.
Always discuss major features before implementation.

## Hardware Testing

If you have mining hardware:
1. Test changes thoroughly before submitting
2. Document hardware-specific behavior
3. Provide debug logs with hardware interactions
4. Note any hardware limitations or quirks

## Questions and Support

- Open an issue for bugs or feature requests
- Use discussions for general questions
- Join our community chat (if available)

## Recognition

Contributors will be recognized in:
- The project's contributor list
- Release notes for significant contributions
- Special thanks for major features

Thank you for contributing to mujina-miner!