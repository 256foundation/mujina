# Build toolchain image for running checks in a reproducible environment.
#
# The Rust compiler and just are pinned to exact versions. The base
# image is pinned by digest. Apt packages come from the Debian
# bookworm repos bundled in the base image; their exact versions can
# drift across builds but are stable system libraries that don't
# affect build output.

FROM docker.io/library/rust:1.94-bookworm@sha256:b2fe2c0f26e0e1759752b6b2eb93b119d30a80b91304f5b18069b31ea73eaee8

# These are pinned to the exact toolchain release by the base image digest.
RUN rustup component add rustfmt clippy

RUN apt-get update && apt-get install -y --no-install-recommends \
    libudev-dev \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

RUN cargo install just --version 1.40.0

WORKDIR /workspace
