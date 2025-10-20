# podman build -t quay.io/jbride2000/rust:1.90.0-trixie-tools -f .github/Dockerfile .

FROM rust:1.90.0-trixie

# Install Rust components
RUN rustup component add rustfmt clippy

# Install ARM64 cross-compilation toolchain
RUN apt-get update && apt-get install -y \
    gcc-aarch64-linux-gnu \
    g++-aarch64-linux-gnu \
    && rm -rf /var/lib/apt/lists/*

# Add ARM64 target for Rust
RUN rustup target add aarch64-unknown-linux-gnu

# Set up cross-compilation environment
ENV CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc
ENV CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++
ENV AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
