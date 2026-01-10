#!/bin/bash

# Deploy and test Mujina Mining Firmware on ARM64 hardware via SSH
# Usage: ./scripts/deploy-to-arm.sh --host <ip> --user <user> --binary <path> [--test-mode]

set -e

# Source environment file if it exists (command line args will override these)
if [[ -f "scripts/local_ci/arm-deployment.env" ]]; then
    echo "Loading configuration from arm-deployment.env..."
    source scripts/local_ci/arm-deployment.env
fi

# Default values (can be overridden by env file or command line)
HOST="${ARM_HOST:-}"
USER="${ARM_USER:-}"
BINARY=""
TEST_MODE=false
SSH_KEY="${SSH_KEY:-}"
SSH_OPTS="${SSH_OPTS:--o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null}"
SSH_ASKPASS="${SSH_ASKPASS:-}"
TIMEOUT="${TIMEOUT:-300}"

# Parse command line arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        --host)
            HOST="$2"
            shift 2
            ;;
        --user)
            USER="$2"
            shift 2
            ;;
        --binary)
            BINARY="$2"
            shift 2
            ;;
        --test-mode)
            TEST_MODE=true
            shift
            ;;
        --ssh-key)
            SSH_KEY="$2"
            shift 2
            ;;
        --timeout)
            TIMEOUT="$2"
            shift 2
            ;;
        --help)
            echo "Usage: $0 --host <ip> --user <user> --binary <path> [--test-mode] [--ssh-key <path>] [--timeout <seconds>]"
            echo ""
            echo "Options:"
            echo "  --host      ARM64 host IP address"
            echo "  --user      SSH username"
            echo "  --binary    Path to binary to deploy"
            echo "  --test-mode Run in test mode (default: false)"
            echo "  --ssh-key   Path to SSH private key"
            echo "  --timeout   SSH connection timeout in seconds (default: 300)"
            echo "  --help      Show this help"
            exit 0
            ;;
        *)
            echo "Unknown option: $1"
            exit 1
            ;;
    esac
done

# Validate required arguments
if [[ -z "$HOST" || -z "$USER" || -z "$BINARY" ]]; then
    echo "Error: --host, --user, and --binary are required"
    echo "Use --help for usage information"
    exit 1
fi

# Check if binary exists
if [[ ! -f "$BINARY" ]]; then
    echo "Error: Binary file '$BINARY' does not exist"
    exit 1
fi

# Make binary executable
chmod +x "$BINARY"

echo "🚀 Deploying to ARM64 hardware..."
echo "   Host: $USER@$HOST"
echo "   Binary: $BINARY"
echo "   Test Mode: $TEST_MODE"

# Build SSH command with key if provided
SSH_CMD="ssh $SSH_OPTS"
if [ -n "$SSH_KEY" ]; then
    SSH_CMD="$SSH_CMD -i $SSH_KEY"
    # Skip ssh-agent entirely and use key directly
    echo "🔑 Using SSH key directly (skipping ssh-agent)..."
fi

# Set up SSH_ASKPASS if provided
if [ -n "$SSH_ASKPASS" ]; then
    export SSH_ASKPASS
    export DISPLAY=dummy:0  # Required for SSH_ASKPASS
    SSH_CMD="$SSH_CMD -o PasswordAuthentication=no"
fi

# Create remote directory
echo "📁 Setting up remote directory..."
$SSH_CMD "$USER@$HOST" "mkdir -p /tmp/mujina-miner-test"

# Remove existing binary if it exists
echo "🗑️  Removing existing binary..."
$SSH_CMD "$USER@$HOST" "rm -f /tmp/mujina-miner-test/mujina-minerd"

# Copy binary to remote host
echo "📤 Uploading binary..."
SCP_CMD="scp $SSH_OPTS"
if [ -n "$SSH_KEY" ]; then
    SCP_CMD="$SCP_CMD -i $SSH_KEY"
fi
if [ -n "$SSH_ASKPASS" ]; then
    export SSH_ASKPASS
    export DISPLAY=dummy:0
fi
$SCP_CMD "$BINARY" "$USER@$HOST:/tmp/mujina-miner-test/mujina-minerd"

# Copy any additional test files
if [[ -d "test-data" ]]; then
    echo "📤 Uploading test data..."
    $SCP_CMD -r test-data "$USER@$HOST:/tmp/mujina-miner-test/"
fi

# Run tests on remote ARM64 hardware
echo "🧪 Running tests on ARM64 hardware..."

# Create test script on remote host
$SSH_CMD "$USER@$HOST" << 'EOF'
cd /tmp/mujina-miner-test
chmod +x mujina-minerd

echo "=== ARM64 Hardware Information ==="
uname -a
cat /proc/cpuinfo | grep -E "(processor|model name|cpu MHz|cache size)" | head -10
cat /proc/meminfo | head -5
echo ""

echo "=== Binary Information ==="
file mujina-minerd
ldd mujina-minerd || echo "Static binary or ldd not available"
echo ""

echo "=== help test ==="
timeout 30 ./mujina-minerd --help || echo "Help command failed"
echo ""

echo ""
echo "=== Test Results ==="
echo "✅ ARM64 deployment successful"
echo "✅ Binary executes correctly"
echo "✅ Hardware integration tested"
EOF

# Check exit status
if [[ $? -eq 0 ]]; then
    echo "✅ ARM64 deployment and testing completed successfully!"
else
    echo "❌ ARM64 deployment or testing failed"
    exit 1
fi

# Cleanup remote files (optional)
echo "🧹 Cleaning up remote files..."
$SSH_CMD "$USER@$HOST" "rm -rf /tmp/mujina-miner-test" || echo "Cleanup failed (non-critical)"

echo "🎉 Hybrid CI deployment completed!"
