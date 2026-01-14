#!/bin/bash

# Local hybrid CI script - combines Podman containers with SSH ARM deployment
# Usage: ./scripts/hybrid-ci-local.sh [--arm-host <ip>] [--arm-user <user>] [--skip-arm]

set -e

# Default values
ARM_HOST=""
ARM_USER=""
SKIP_ARM=false
RUST_IMAGE="quay.io/jbride2000/rust:1.90.0-trixie-tools"

# Parse command line arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        --arm-host)
            ARM_HOST="$2"
            shift 2
            ;;
        --arm-user)
            ARM_USER="$2"
            shift 2
            ;;
        --skip-arm)
            SKIP_ARM=true
            shift
            ;;
        --rust-image)
            RUST_IMAGE="$2"
            shift 2
            ;;
        --help)
            echo "Usage: $0 [--arm-host <ip>] [--arm-user <user>] [--skip-arm] [--rust-image <image>]"
            echo ""
            echo "Options:"
            echo "  --arm-host     ARM64 host IP address for deployment"
            echo "  --arm-user     SSH username for ARM host"
            echo "  --skip-arm     Skip ARM deployment (containers only)"
            echo "  --rust-image   Rust container image (default: quay.io/jbride2000/rust:1.90.0-trixie-tools)"
            echo "  --help         Show this help"
            exit 0
            ;;
        *)
            echo "Unknown option: $1"
            exit 1
            ;;
    esac
done

echo "🔄 Starting hybrid CI (Podman + ARM SSH)..."
echo "   Rust Image: $RUST_IMAGE"
echo "   ARM Host: ${ARM_HOST:-'Not configured'}"
echo "   ARM User: ${ARM_USER:-'Not configured'}"
echo "   Skip ARM: $SKIP_ARM"
echo ""

# Check if Podman is available
if ! command -v podman &> /dev/null; then
    echo "❌ Podman is not installed. Please install Podman first."
    exit 1
fi

# Check if ARM deployment is configured
if [ "$SKIP_ARM" = "false" ] && [ -z "$ARM_HOST" ]; then
    echo "⚠️  ARM host not configured. Running container-only CI."
    echo "   Use --arm-host <ip> and --arm-user <user> to enable ARM deployment"
    echo "   Or use --skip-arm to skip ARM deployment entirely"
    echo ""
    SKIP_ARM=true
fi

# Make scripts executable
chmod +x .github/ci-runner.sh

echo "📦 Step 1: Running container-based CI checks..."
export RUST_IMAGE="$RUST_IMAGE"
export WORKSPACE_DIR="$(pwd)"
export CACHE_MOUNTS="true"
export SKIP_CLIPPY="${SKIP_CLIPPY:-false}"

# Run all container-based checks
./.github/ci-runner.sh all

echo ""
echo "✅ Container-based CI completed successfully!"
echo ""

# Cross-compile for ARM64
echo "🔨 Step 2: Cross-compiling for ARM64..."
export TARGET_ARCH="aarch64-unknown-linux-gnu"
./.github/ci-runner.sh build-release

echo ""
echo "✅ ARM64 cross-compilation completed!"
echo ""

# ARM deployment (if configured)
if [ "$SKIP_ARM" = "false" ]; then
    echo "🚀 Step 3: Deploying to ARM64 hardware..."
    
    # Set up environment for ARM deployment
    export HYBRID_MODE="true"
    export ARM_HOST="$ARM_HOST"
    export ARM_USER="$ARM_USER"
    
    # Find the ARM64 binary
    ARM_BINARY="target/aarch64-unknown-linux-gnu/release/mujina-minerd"
    
    if [ ! -f "$ARM_BINARY" ]; then
        echo "❌ ARM64 binary not found at $ARM_BINARY"
        exit 1
    fi
    
    # Ensure deploy script is executable
    chmod +x scripts/local_ci/deploy-to-arm.sh

    # Deploy to ARM hardware
    ./scripts/local_ci/deploy-to-arm.sh \
        --host "$ARM_HOST" \
        --user "$ARM_USER" \
        --binary "$ARM_BINARY" \
        --test-mode
    
    echo ""
    echo "✅ ARM64 deployment and testing completed!"
else
    echo "⏭️  Step 3: Skipping ARM deployment"
    echo "   ARM64 binary available at: target/aarch64-unknown-linux-gnu/release/mujina-minerd"
fi

echo ""
echo "🎉 Hybrid CI completed successfully!"
echo ""
echo "📊 Summary:"
echo "   ✅ Container-based CI: PASSED"
echo "   ✅ ARM64 cross-compilation: PASSED"
if [ "$SKIP_ARM" = "false" ]; then
    echo "   ✅ ARM64 hardware deployment: PASSED"
else
    echo "   ⏭️  ARM64 hardware deployment: SKIPPED"
fi
echo ""
echo "🚀 Ready for deployment!"
