# Container Image

A container image lets you deploy mujina-minerd to cloud infrastructure,
Kubernetes clusters, or anywhere containers run. Since real mining requires
specialized hardware, the container is primarily a testing tool:

- **Pool development**: Spin up many CPU miners to stress-test pool software
- **Mujina development**: Test changes without a local Rust toolchain
- **CI/CD**: Automated testing for pool or miner software, or integration
  testing with actual mining hardware

Examples below use Podman. Docker commands are identical---substitute `docker`
for `podman`.

## Quick Start

Pull and run from GitHub Container Registry:

```bash
podman run --rm -it \
  -e MUJINA_USB_DISABLE=1 \
  -e MUJINA_CPUMINER_THREADS=2 \
  -e MUJINA_POOL_URL="stratum+tcp://pool.example.com:3333" \
  -e MUJINA_POOL_USER="your-address.worker" \
  ghcr.io/256foundation/mujina-minerd:latest
```

This starts a 2-thread CPU miner connected to your pool. See
[CPU Mining](cpu-mining.md) for all environment variables.

## Building the Image

Build locally with the justfile:

```bash
just container-build
```

Or directly with Podman:

```bash
podman build -t mujina-minerd:latest -f Containerfile .
```

The Containerfile uses a multi-stage build: full Rust toolchain for
compilation, then debian:bookworm-slim for runtime. The final image is around
100MB.

By default, `just container-build` tags with your current git branch. Override
with:

```bash
just container-build v0.1.0
```

## Accessing the API

The REST API listens on port 7785. To access it from the host:

```bash
podman run --rm -it \
  -p 7785:7785 \
  -e MUJINA_USB_DISABLE=1 \
  -e MUJINA_CPUMINER_THREADS=2 \
  mujina-minerd:latest
```

Then query via `http://localhost:7785/`.

## Image Details

| Property | Value |
|----------|-------|
| Base image | debian:bookworm-slim |
| Runtime dependencies | libudev1, ca-certificates |
| User | mujina (non-root) |
| Exposed port | 7785 |
| Size | ~100MB |
