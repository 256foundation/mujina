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

1. If there is a discussion for the feature, open a **draft** pull
   request and link to it. Mark it ready for review when the code
   is complete and ready to merge.
2. If there is no discussion yet, open one first and link to your
   branch. Getting alignment before the PR makes the review
   process smoother.

### I'd like to contribute

All [issues][issue tracker] are actionable---pick one and start
working on it. If you need help or guidance, comment on the issue.
Issues tagged with ["good first issue"] are extra friendly to new
contributors.

["good first issue"]: https://github.com/256foundation/mujina/issues?q=is%3Aissue+is%3Aopen+label%3A%22good+first+issue%22

### I have a question

For user support and general discussion, visit the [Mujina forum].
For developer questions, open a [Q&A discussion] on GitHub. For
real-time chat, join our [Telegram group]---note that Telegram is
ephemeral; decisions and important context belong on GitHub.

[Mujina forum]: https://forum.256foundation.org/c/mujina/7
[Q&A discussion]: https://github.com/256foundation/mujina/discussions/new?category=q-a
[Telegram group]: https://t.me/the256foundation

## Workflow

The path to merged code depends on the type of change:

**Bug reports** start in an ["Issue Triage" discussion]. The
maintainers triage the report and, if confirmed, promote it to the
[issue tracker]. Every issue in the tracker is ready to be worked on.

**Features** start in the ["Ideas" category]. Once there is consensus
on the approach, open a draft PR linked to the discussion and develop
there. Keep the PR as a draft until the work is complete and ready to
merge.

**Simple fixes** go straight to a PR---no discussion or issue needed.
This covers typo fixes, wording improvements, spelling corrections,
log level adjustments, comment improvements, documentation
clarifications, and clear bug fixes where the problem and solution are
both obvious. Use your judgment; if the change is obviously correct
and self-contained, just open the PR.

The first two paths exist for a reason. Without prior alignment, you
risk spending time on work that conflicts with planned changes, doesn't
fit the project direction, or solves a problem in a way that won't be
merged. A quick discussion up front saves everyone time.

Issues tagged with "feature" or "enhancement" represent accepted,
well-scoped work. If you implement an issue as described, your pull
request will be accepted with a high degree of certainty.

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
with a pinned Rust toolchain, and it checks every commit on the
branch individually, not just the branch tip (see Atomic Commits
below for why). Run the same thing locally with:

```bash
just ci
```

This checks out each commit after the upstream default branch in
turn, runs `just checks` inside a toolchain container built from
`build.Containerfile`, and returns to your original branch when
done. Podman is required for this step but not for regular
development.

To check only the working tree in the CI container, for example
when `just checks` passes locally but CI fails:

```bash
just in-container checks
```

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
should pass `just checks` after every commit. This matters for
three reasons:

- **Revertability.** If a commit introduces a regression, `git
  revert` should cleanly undo that one change without dragging
  unrelated work along with it.
- **Bisectability.** `git bisect` needs every commit to be a
  working state. Mixed commits force you to debug multiple changes
  at once.
- **Reviewability.** Small, focused commits are easier to review
  and reason about than large ones that do several things.

CI enforces this: pull request CI runs the full pipeline on each
commit in the PR, so an intermediate commit that fails checks
fails the PR even when the branch tip passes.

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

2. Check that every commit still passes, since rebasing can break
   an intermediate commit even when the branch tip is fine:
   ```bash
   just ci
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

5. Open feature PRs as a **draft** while work is in progress.
   Mark it ready for review only when the code is complete and
   ready to merge. This keeps the review queue clear so reviewers
   can trust that non-draft PRs represent finished work they can
   act on.

6. Address review feedback promptly.

7. Once approved, your PR will be merged.
