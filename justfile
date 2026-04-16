_default:
    @just --list --unsorted

[group('dev')]
fmt *args:
    cargo fmt {{args}}

[group('dev')]
lint:
    cargo clippy --release -- -D warnings

[group('dev')]
test:
    cargo test

# Run all checks (before commit, push, merge, release)
[group('dev')]
@checks: (fmt "--check") lint test

[group('dev')]
run:
    cargo run --bin mujina-minerd

BUILD_IMAGE := "mujina-build"
# Tag with a content hash of the Containerfile so we can detect
# staleness without rebuilding. This matters in CI where podman
# save/load doesn't preserve layer cache---podman build would
# rebuild from scratch even with a loaded image. The content-hash
# tag lets `podman image exists` skip the build entirely.
BUILD_TAG := `sha256sum build.Containerfile | cut -c1-12`

# Build the build toolchain image (skips if unchanged)
[group('container')]
build-image:
    podman image exists {{BUILD_IMAGE}}:{{BUILD_TAG}} || \
        podman build -t {{BUILD_IMAGE}}:{{BUILD_TAG}} -f build.Containerfile .

# Remove stale build toolchain images
[group('container')]
build-image-clean:
    podman images --format '{{{{.Repository}}:{{{{.Tag}}' \
        | grep '^{{BUILD_IMAGE}}:' \
        | grep -v ':{{BUILD_TAG}}$' \
        | xargs -r podman rmi

# Run a just recipe inside the build toolchain image
[group('container')]
in-container *args: build-image
    mkdir -p .cache/cargo-registry .cache/cargo-git
    podman run --rm \
        -v "$(pwd)":/workspace:Z \
        -v "$(pwd)/.cache/cargo-registry":/usr/local/cargo/registry \
        -v "$(pwd)/.cache/cargo-git":/usr/local/cargo/git \
        -w /workspace \
        {{BUILD_IMAGE}}:{{BUILD_TAG}} \
        just {{args}}

# The CI pipeline. This is what GitHub Actions runs.
[group('ci')]
ci: (in-container "checks")

[group('container')]
container-build tag=`git rev-parse --abbrev-ref HEAD`:
    podman build -t mujina-minerd:{{tag}} -f Containerfile .

[group('container')]
container-push tag=`git rev-parse --abbrev-ref HEAD`:
    podman tag mujina-minerd:{{tag}} ghcr.io/256foundation/mujina-minerd:{{tag}}
    podman push ghcr.io/256foundation/mujina-minerd:{{tag}}

# Configure git to use the project's .githooks directory
[group('setup')]
setup-hooks:
    git config core.hooksPath .githooks
    @echo "Git hooks configured to use .githooks/"
    @ls .githooks/ | sed 's/^/  - /'