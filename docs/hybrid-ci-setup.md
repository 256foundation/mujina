**Hybrid CI Setup Guide**

This guide explains how to set up and use the hybrid CI system that uses 

## 1. Overview

The hybrid CI system provides:

1. **Container-based CI**: Fast, reproducible builds using Podman containers
2. **ARM64 cross-compilation**: Build ARM64 binaries on x86_64 runners
3. **Real hardware testing**: Deploy and test on actual ARM hardware via SSH
4. **Flexible deployment**: Works with GitHub Actions and local development


## 2. Github Actions

This project includes a _workflow_ called [hybrid-ci.yml](../.github/workflows/hybrid-ci.yml) that gets automatically triggered in Github with every commit.
The workflow also allows for manual trigger via the Github UI.

![GitHub Actions Manual Trigger](images/gh_actions_1.png)


Notice in the `hybrid-cli.yml`, configuration exists for specifying the branches that github actions will automatically run on:


```
name: Hybrid CI (Podman + Native ARM64)
      
on:     
  push:
    branches: [ main, master, preview, gh_actions]
  pull_request:
    branches: [ main, master, preview, gh_actions ]
```

Modify the list of branches to support your development efforts as necessary.

### 2.1. Skipping Clippy Tests

You can skip clippy lints when manually triggering the workflow via the GitHub UI.
(Manual triggering is available only on the `main` branch.
Check the "Skip clippy lints" option when running the workflow manually. By default, clippy is enabled.

![Clippy Skip](images/clippy_skip.png)

## 3. Local testing

This project also includes a [Makefile](../scripts/local_ci/Makefile.ci) that can be run in your local dev environment.
These Makefile commands are for local use only and are not used by GitHub Actions.

On a x86_64 based development environment, you can use the Makefile for the following:

1. Full CI checks in containers:
    * Formatting check (cargo fmt --all -- --check)
    * Clippy lints (cargo clippy --all-targets --all-features -- -D warnings)
    * Debug build (cargo build --verbose)
    * Tests (cargo test --verbose)

2. ARM64 cross-compilation

All compilation and tests occur in a linux container using podman.

### 3.1. Prerequisites

- **Podman**

### 3.2. Execute

1. OPTIONAL: To skip clippy lints locally, set the `SKIP_CLIPPY` environment variable:
    ```bash
    export SKIP_CLIPPY=true
    ```


2. Execute:
    ```bash
    make -f scripts/local_ci/Makefile.ci ci-containers-arm64-cross-compilation
    ```

## 4. Local Setup (ARM64 deploy)

The CI functionality also allows for deploying and testing cross-compiled mujina to an ARM64 target environment.

### 4.1. Prerequisites

- **Podman** installed on your system
- **SSH access** to ARM64 hardware (ie: Raspberry Pi 4)

### 4.2. Configuration

Create your ARM deployment configuration:

```bash
# Copy the template
cp scripts/local_ci/arm-deployment.template scripts/local_ci/arm-deployment.env

# Edit with your details
vi scripts/local_ci/arm-deployment.env
```

### 4.3. Usage

```bash
make -f scripts/local_ci/Makefile.ci deploy-arm
```

### 4.4. Volume Management

#### 4.4.1. Manual Volume Cleanup

If you encounter issues with mixed architectures or need to clean up build artifacts, you can manually manage the Podman volumes:

```bash
# List all volumes
podman volume ls

# Remove specific CI volumes
podman volume rm cargo-registry cargo-git cargo-target

# Remove all unused volumes
podman volume prune

# Force remove all volumes (use with caution)
podman volume rm --all
```

#### 4.4.2. Volume Locations

The CI system uses these volumes:
- `cargo-registry`: Rust package registry cache
- `cargo-git`: Git dependencies cache  
- `cargo-target`: Build artifacts cache
