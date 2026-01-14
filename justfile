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

[group('container')]
container-build tag=`git rev-parse --abbrev-ref HEAD`:
    podman build -t mujina-minerd:{{tag}} -f Containerfile .

[group('container')]
container-push tag=`git rev-parse --abbrev-ref HEAD`:
    podman tag mujina-minerd:{{tag}} ghcr.io/256foundation/mujina-minerd:{{tag}}
    podman push ghcr.io/256foundation/mujina-minerd:{{tag}}

[group('setup')]
hooks:
    ./scripts/setup-hooks.sh