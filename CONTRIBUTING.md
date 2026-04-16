# Contributing to Mujina Miner

This document explains how to contribute effectively and get your
changes merged quickly. The workflow is designed to make contributing
easier, not harder.

## Quick Guide

### I have a bug or something isn't working

1. Search [discussions] and the [issue tracker] for similar
   problems. Tip: also search [closed issues] and [closed
   discussions]---your issue might have already been fixed!
2. If your issue hasn't been reported, open an ["Issue Triage"
   discussion] and fill in the template completely. Use this
   category only for bug reports.

[discussions]: https://github.com/256foundation/mujina/discussions
[issue tracker]: https://github.com/256foundation/mujina/issues
[closed issues]: https://github.com/256foundation/mujina/issues?q=is%3Aissue+state%3Aclosed
[closed discussions]: https://github.com/256foundation/mujina/discussions?discussions_q=is%3Aclosed
["Issue Triage" discussion]: https://github.com/256foundation/mujina/discussions/new?category=issue-triage

### I have an idea for a feature

Open a discussion in the ["Ideas" category] to propose and discuss
the feature before implementation.

["Ideas" category]: https://github.com/256foundation/mujina/discussions/new?category=ideas

### I've implemented a feature

1. If there's an issue for the feature, open a pull request.
2. If there's no issue, open a discussion first and link to your
   branch. Getting feedback before the PR makes the review process
   smoother.

### I'd like to contribute

All [issues][issue tracker] are actionable---pick one and start
working on it. If you need help or guidance, comment on the issue.
Issues tagged with ["good first issue"] are extra friendly to new
contributors.

["good first issue"]: https://github.com/256foundation/mujina/issues?q=is%3Aissue+is%3Aopen+label%3A%22good+first+issue%22

### I have a question

Open a [Q&A discussion] or ask in our [Telegram group].

[Q&A discussion]: https://github.com/256foundation/mujina/discussions/new?category=q-a
[Telegram group]: https://t.me/the256foundation

## Workflow: Discussion, Issue, Pull Request

The path from idea to merged code has three steps:

1. **Discussion.** Propose ideas, report bugs, or ask questions in
   GitHub [discussions]. This is where design conversations happen.
2. **Issue.** Once a discussion produces a well-understood,
   actionable item, it moves to the [issue tracker]. Every issue in
   the tracker is ready to be worked on.
3. **Pull request.** Implement the issue and open a PR.

This sequence matters. Without prior discussion, you risk spending
time on work that conflicts with planned changes, doesn't fit the
project direction, or solves a problem in a way that won't be
merged. A quick discussion up front saves everyone time.

Issues tagged with "feature" or "enhancement" represent accepted,
well-scoped work. If you implement an issue as described, your pull
request will be accepted with a high degree of certainty.

**Exception:** Trivial improvements don't need discussion or an
issue. This includes typo fixes, wording improvements, spelling
corrections, log level adjustments, comment improvements, and
documentation clarifications. Use your judgment---if the change is
obviously correct and self-contained, just open the PR.

The above workflow is adapted from [Ghostty]. Thanks to Mitchell
Hashimoto and contributors for the pattern!

[Ghostty]: https://github.com/ghostty-org/ghostty

## Getting Started

### Prerequisites

- [Rust](https://rustup.rs/) toolchain (stable)
- [just](https://just.systems/) command runner
- Git
- Optional: [Podman](https://podman.io/) for reproducing CI locally
- Optional: Hardware for testing (Bitaxe boards, etc.)

### Setting Up Your Development Environment

1. Fork the repository on GitHub
2. Clone your fork:
   ```bash
   git clone https://github.com/YOUR_USERNAME/mujina.git
   cd mujina
   ```
3. Add the upstream repository:
   ```bash
   git remote add upstream \
       https://github.com/256foundation/mujina.git
   ```
4. **Setup git hooks** (highly recommended):
   ```bash
   just setup-hooks
   ```
   The pre-commit hook checks whitespace and Rust formatting
   before each commit. If you need to bypass it (e.g., committing
   a work-in-progress), use:
   ```bash
   git commit --no-verify
   ```
5. Create a branch:
   ```bash
   git checkout -b fix-double-free-on-shutdown
   ```

Before diving into the code, read
[`CODE_STYLE.md`](CODE_STYLE.md) and
[`CODING_GUIDELINES.md`](CODING_GUIDELINES.md). These define the
project's standards.

## Development

### Running Checks

Run all checks before committing:

```bash
just checks
```

This runs `cargo fmt --check`, `cargo clippy`, and `cargo test`
using your local Rust toolchain.

Pull requests are gated on CI that runs inside a Podman container
with a pinned Rust toolchain. If `just checks` passes locally but
CI fails (or vice versa), reproduce the CI environment with:

```bash
just ci
```

This builds a toolchain container from `build.Containerfile` and
runs `just checks` inside it. Podman is required for this step but
not for regular development.

### Documenting Known Bugs with `#[should_panic]`

When a bug is found in already-pushed code, we sometimes add a
test that asserts the correct behavior and mark it
`#[should_panic]` with a brief comment. This documents the bug and
keeps CI green. The fix then removes the `#[should_panic]`
annotation in a separate commit, turning the test into a normal
regression test.

```rust
#[test]
#[should_panic] // known bug: brief description
fn descriptive_name() {
    // Assert the *correct* behavior here.
    // The test "passes" because the bug causes a panic.
}
```

### Commits

#### Atomic Commits

Each commit should be exactly one logical change, and the tree
should build and pass tests after every commit. This matters for
three reasons:

- **Revertability.** If a commit introduces a regression, `git
  revert` should cleanly undo that one change without dragging
  unrelated work along with it.
- **Bisectability.** `git bisect` needs every commit to be a
  working state. Mixed commits force you to debug multiple changes
  at once.
- **Reviewability.** Small, focused commits are easier to review
  and reason about than large ones that do several things.

A good test: if you need "and" in the subject line, you probably
have two commits.

**Good (two separate commits):**
- `fix(protocol): prevent buffer overflow in parser`
- `perf(protocol): optimize parser performance`

**Bad (two unrelated changes lumped together):**
- `fix(protocol): prevent buffer overflow and optimize performance`

**Good (feature split into buildable steps):**
- `feat(board): add temperature sensor reading`
- `feat(board): add overheat shutdown using temperature sensor`

**Bad (entire feature in one commit):**
- `feat(board): add temperature monitoring with overheat shutdown`

#### Message Format

We use [conventional commits].

[conventional commits]: https://www.conventionalcommits.org/

```
type(scope): subject in imperative mood

Explain what this commit does and why. The code shows how.

Wrap the body at 72 characters.

Fixes: #123
```

The subject should complete the sentence "if applied, this commit
will ___." Use imperative mood ("add", "fix", "refactor"), not
past tense or gerunds:

- GOOD: `feat(board): add temperature monitoring`
- GOOD: `fix(scheduler): prevent race in share submission`
- BAD: `feat(board): added temperature monitoring`
- BAD: `fix(scheduler): fixes race condition`

#### Types

`feat` and `fix` signal behavioral changes and are the most common
types. The remaining types are for changes that don't alter
behavior (refactoring, documentation, tests, etc.).

- `feat`: Add a new feature
- `fix`: Fix a bug
- `docs`: Change documentation only
- `style`: Change code style (formatting, missing semicolons, etc.)
- `refactor`: Refactor code without changing functionality
- `perf`: Improve performance
- `test`: Add or correct tests
- `chore`: Update build process, dependencies, etc.

#### Example

```
fix(board): prevent double-free in shutdown sequence

The board shutdown sequence could trigger a double-free when called
multiple times due to missing state check. Add a state machine to
track shutdown progress and prevent multiple cleanup attempts.

The issue was discovered during stress testing with rapid board
connect/disconnect cycles.

Fixes: #234
```

## Pull Requests

1. Update your branch with the latest upstream changes:
   ```bash
   git fetch upstream
   git rebase upstream/main
   ```

2. Run checks to make sure everything still passes:
   ```bash
   just checks
   ```

3. Push your branch to your fork:
   ```bash
   git push origin fix-double-free-on-shutdown
   ```

4. Create a pull request on GitHub with:
   - Title in conventional commit format (e.g.,
     `feat(board): add temperature monitoring`)
   - Reference to the issue being implemented (if applicable)
   - Link to the discussion that led to the issue (helpful
     context)
   - The commit messages should describe individual changes;
     use the PR body for big-picture context that ties the
     commits together or doesn't belong in any single commit
   - Relevant logs if applicable

5. If the work isn't finished but you want early feedback, open
   the PR as a **draft**. Mark it ready for review when the code
   is complete. This keeps the review queue clear so reviewers can
   trust that non-draft PRs represent finished work they can act
   on.

6. Address review feedback promptly.

7. Once approved, your PR will be merged.
