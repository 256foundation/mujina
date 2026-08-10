# Cargo tool installation, imported by the justfile and build container.
#
# setup-tools installs a missing tool if it is not already installed. The build
# container runs this recipe, so these versions decide what runs in CI.

COOLDOWN_VERSION := "0.3.4"
DENY_VERSION := "0.20.2"

# Install the cargo tools the recipes use
[group('setup')]
setup-tools:
    @if command -v cargo-cooldown >/dev/null; then \
        echo "cargo-cooldown already installed; using the system copy"; \
    else \
        cargo install --locked cargo-cooldown --version {{COOLDOWN_VERSION}}; \
    fi
    @if command -v cargo-deny >/dev/null; then \
        echo "cargo-deny already installed; using the system copy"; \
    else \
        cargo install --locked cargo-deny --version {{DENY_VERSION}}; \
    fi
