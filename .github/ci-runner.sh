#!/bin/bash

# CI runner script that can be used both locally and in GitHub Actions
# Supports hybrid approach: Podman containers + ARM SSH deployment
set -e

# Default values
WORKSPACE_DIR="${WORKSPACE_DIR:-$(pwd)}"
RUST_IMAGE="${RUST_IMAGE:-quay.io/jbride2000/rust:1.90.0-trixie-tools}"
CACHE_MOUNTS="${CACHE_MOUNTS:-true}"
TARGET_ARCH="${TARGET_ARCH:-}"
HYBRID_MODE="${HYBRID_MODE:-false}"
ARM_HOST="${ARM_HOST:-}"
ARM_USER="${ARM_USER:-}"
SKIP_CLIPPY="${SKIP_CLIPPY:-false}"

# Function to run a command in the container
run_in_container() {
    local cmd="$1"
    local description="$2"
    
    echo "Running: $description"
    
    # Add target architecture if specified
    local target_cmd="$cmd"
    if [ -n "$TARGET_ARCH" ]; then
        # Set pkg-config for ARM64 cross-compilation
        if [ "$TARGET_ARCH" = "aarch64-unknown-linux-gnu" ]; then
            target_cmd="export PKG_CONFIG_ALLOW_CROSS=1 PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig PKG_CONFIG_LIBDIR=/usr/lib/aarch64-linux-gnu/pkgconfig && rustup target add $TARGET_ARCH && $cmd --target $TARGET_ARCH"
        else
            target_cmd="rustup target add $TARGET_ARCH && $cmd --target $TARGET_ARCH"
        fi
    fi
    
    if [ "$CACHE_MOUNTS" = "true" ]; then
        podman run --rm \
            -v "$WORKSPACE_DIR":/workspace:Z \
            -w /workspace \
            --mount=type=volume,source=cargo-registry,target=/usr/local/cargo/registry \
            --mount=type=volume,source=cargo-git,target=/usr/local/cargo/git \
            --mount=type=volume,source=cargo-target,target=/workspace/target \
            "$RUST_IMAGE" \
            bash -c "$target_cmd"
    else
        podman run --rm \
            -v "$WORKSPACE_DIR":/workspace:Z \
            -w /workspace \
            "$RUST_IMAGE" \
            bash -c "$target_cmd"
    fi
}

# Function to deploy to ARM hardware
deploy_to_arm() {
    local binary_path="$1"
    
    if [ "$HYBRID_MODE" = "false" ] || [ -z "$ARM_HOST" ] || [ -z "$ARM_USER" ]; then
        echo "Skipping ARM deployment (not in hybrid mode or missing ARM config)"
        return 0
    fi
    
    echo "🚀 Deploying to ARM hardware..."
    
    # Check if deployment script exists
    if [ ! -f "scripts/local_ci/deploy-to-arm.sh" ]; then
        echo "❌ ARM deployment script not found"
        return 1
    fi
    
    # Run ARM deployment
    chmod +x scripts/local_ci/deploy-to-arm.sh
    ./scripts/local_ci/deploy-to-arm.sh \
        --host "$ARM_HOST" \
        --user "$ARM_USER" \
        --binary "$binary_path" \
        --test-mode
}

# Parse command line arguments
case "${1:-all}" in
    "fmt")
        run_in_container "cargo fmt --all -- --check" "formatting check"
        ;;
    "clippy")
        if [ "$SKIP_CLIPPY" = "true" ]; then
            echo "⏭️  Skipping clippy check (SKIP_CLIPPY=true)"
        else
            run_in_container "cargo clippy --all-targets --all-features -- -D warnings" "clippy check"
        fi
        ;;
    "build")
        run_in_container "cargo build --verbose" "build"
        ;;
    "build-release")
        run_in_container "cargo build --release --verbose" "release build"
        ;;
    "test")
        run_in_container "cargo test --verbose" "tests"
        ;;
    "test-release")
        run_in_container "cargo test --release --verbose" "release tests"
        ;;
    "all")
        run_in_container "cargo fmt --all -- --check" "formatting check"
        if [ "$SKIP_CLIPPY" = "true" ]; then
            echo "⏭️  Skipping clippy check (SKIP_CLIPPY=true)"
        else
            run_in_container "cargo clippy --all-targets --all-features -- -D warnings" "clippy check"
        fi
        run_in_container "cargo build --verbose" "build"
        run_in_container "cargo test --verbose" "tests"
        ;;
    "hybrid")
        echo "🔄 Running hybrid CI (containers + ARM deployment)..."

        # Step 1: Container-based checks
        run_in_container "cargo fmt --all -- --check" "formatting check"
        if [ "$SKIP_CLIPPY" = "true" ]; then
            echo "⏭️  Skipping clippy check (SKIP_CLIPPY=true)"
        else
            run_in_container "cargo clippy --all-targets --all-features -- -D warnings" "clippy check"
        fi
        
        # Step 2: Cross-compile for ARM64
        TARGET_ARCH=aarch64-unknown-linux-gnu
        run_in_container "cargo build --release --verbose" "ARM64 release build"
        
        # Step 3: Deploy to ARM hardware
        if [ -n "$TARGET_ARCH" ]; then
            binary_path="$WORKSPACE_DIR/target/$TARGET_ARCH/release/mujina-minerd"
        else
            binary_path="$WORKSPACE_DIR/target/release/mujina-minerd"
        fi
        
        deploy_to_arm "$binary_path"
        ;;
    *)
        echo "Usage: $0 [fmt|clippy|build|build-release|test|test-release|all|hybrid]"
        echo ""
        echo "Commands:"
        echo "  fmt           Check formatting"
        echo "  clippy        Run clippy lints"
        echo "  build         Build debug version"
        echo "  build-release Build release version"
        echo "  test          Run tests"
        echo "  test-release  Run release tests"
        echo "  all           Run all checks"
        echo "  hybrid        Run hybrid CI (containers + ARM deployment)"
        echo ""
        echo "Environment variables:"
        echo "  WORKSPACE_DIR: Directory to mount (default: current directory)"
        echo "  RUST_IMAGE: Rust container image (default: quay.io/jbride2000/rust:1.90.0-trixie-tools)"
        echo "  TARGET_ARCH: Target architecture (e.g., aarch64-unknown-linux-gnu)"
        echo "  CACHE_MOUNTS: Enable cache mounts (default: true)"
        echo "  SKIP_CLIPPY: Skip clippy lints (default: false)"
        echo "  HYBRID_MODE: Enable ARM deployment (default: false)"
        echo "  ARM_HOST: ARM64 host IP address"
        echo "  ARM_USER: SSH username for ARM host"
        exit 1
        ;;
esac
