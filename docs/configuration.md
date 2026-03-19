*Mujina Configuration*

This document describes the configuration system for `mujina-minerd`.

- [1. Priority Order](#1-priority-order)
- [2. Config Files](#2-config-files)
  - [2.1. Default location](#21-default-location)
  - [2.2. User-specified location](#22-user-specified-location)
- [3. Environment Variables](#3-environment-variables)
  - [3.1. Migration from Legacy Environment Variables](#31-migration-from-legacy-environment-variables)
- [4. CLI Flags](#4-cli-flags)
- [5. YAML Structure](#5-yaml-structure)
  - [5.1. Full reference with defaults](#51-full-reference-with-defaults)
- [6. Testing the Priority Chain](#6-testing-the-priority-chain)

## 1. Priority Order

Configuration is resolved from multiple sources. When the same key appears in
more than one source, the **highest-priority source wins**:

| Priority | Source |
|----------|--------|
| 1 (highest) | CLI flags (`--pool-url`, `--log-level`, etc.) |
| 2 | Environment variables (`MUJINA__*`) |
| 3 | User config file (`$MUJINA_CONFIG_FILE_PATH`) |
| 4 | Default config file (`/etc/mujina/mujina.yaml`) |
| 5 (lowest) | Hard-coded defaults |

All sources are optional except the hard-coded defaults, which are always
present. A minimal deployment with no config file and no environment variables
will start with sensible defaults (dummy job source, API on localhost).

## 2. Config Files

### 2.1. Default location

`/etc/mujina/mujina.yaml`

This is the standard system-wide config file, suitable for installation by a
package manager or system administrator.

### 2.2. User-specified location

Set `MUJINA_CONFIG_FILE_PATH` to an absolute path to load a second config file
that supplements (and overrides) the default location:

```sh
MUJINA_CONFIG_FILE_PATH=/home/operator/mujina.yaml mujina-minerd
```

Keys present in the user-specified file take precedence over the same keys in
`/etc/mujina/mujina.yaml`. Keys absent from the user file fall back to the
default file, then to hard-coded defaults.

An example config file is provided at `configs/mujina.example.yaml`.

## 3. Environment Variables

Individual config keys can be overridden with environment variables. The
naming convention is:

```
MUJINA__<SECTION>__<KEY>=value
```

Nesting levels are separated by double-underscores (`__`). The prefix is
`MUJINA` (single word, no trailing underscores).

Double underscores are required because config key names themselves contain
single underscores (e.g. `cpu_miner`, `fan_min_pct`). A single-underscore
separator would make it impossible to tell whether `MUJINA_BOARDS_CPU_MINER`
means `boards.cpu_miner` (two levels) or `boards.cpu` with key `miner` (three
levels with a truncated name). Double underscores eliminate that ambiguity:
every `__` is a level boundary, every `_` is part of a name.

Examples:

```sh
# Override daemon.log_level
MUJINA__DAEMON__LOG_LEVEL=debug

# Override api.listen
MUJINA__API__LISTEN=0.0.0.0:7785

# Override pool URL
MUJINA__POOL__URL=stratum+tcp://pool.example.com:3333

# Disable USB discovery
MUJINA__BACKPLANE__USB_ENABLED=false

# Enable CPU miner
MUJINA__BOARDS__CPU_MINER__ENABLED=true
MUJINA__BOARDS__CPU_MINER__THREADS=4
```

> **Note on log filtering:** `RUST_LOG` is handled separately by the
> `tracing-subscriber` crate and controls per-module log verbosity. It is not
> part of the mujina config system but remains fully supported.
> Example: `RUST_LOG=mujina_miner=debug`

### 3.1. Migration from Legacy Environment Variables

Earlier versions used ad-hoc environment variables with single underscores.
These are superseded by the unified config system:

| Legacy variable | New config key | New env var |
|-----------------|---------------|-------------|
| `MUJINA_POOL_URL` | `pool.url` | `MUJINA__POOL__URL` |
| `MUJINA_POOL_USER` | `pool.user` | `MUJINA__POOL__USER` |
| `MUJINA_POOL_PASS` | `pool.password` | `MUJINA__POOL__PASSWORD` |
| `MUJINA_API_LISTEN` | `api.listen` | `MUJINA__API__LISTEN` |
| `MUJINA_USB_DISABLE` | `backplane.usb_enabled` | `MUJINA__BACKPLANE__USB_ENABLED` |
| `MUJINA_CPUMINER_THREADS` | `boards.cpu_miner.threads` | `MUJINA__BOARDS__CPU_MINER__THREADS` |
| `MUJINA_CPUMINER_DUTY` | `boards.cpu_miner.duty_percent` | `MUJINA__BOARDS__CPU_MINER__DUTY_PERCENT` |

`MUJINA_API_URL` (used by `mujina-cli` to locate the daemon) is not part of
the daemon config and is unchanged.

## 4. CLI Flags

CLI flags override all other sources, including environment variables. They are
intended for one-off overrides and testing, not permanent configuration.

`mujina-minerd` accepts the following flags:

```
USAGE:
    mujina-minerd [OPTIONS]

OPTIONS:
    -c, --config <PATH>         Config file path (overrides MUJINA_CONFIG_FILE_PATH)
        --log-level <LEVEL>     Log level: error, warn, info, debug, trace [default: info]
        --api-listen <ADDR>     API listen address [default: 127.0.0.1:7785]
        --pool-url <URL>        Pool URL, e.g. stratum+tcp://pool.example.com:3333
        --pool-user <USER>      Pool worker username
        --pool-pass <PASS>      Pool worker password
    -h, --help                  Print help
    -V, --version               Print version
```

## 5. YAML Structure

The config file uses YAML. The top-level keys correspond to subsystems:

```yaml
daemon:       # Process and logging settings
api:          # HTTP API server
pool:         # Mining pool connection (primary)
backplane:    # Board discovery and lifecycle
boards:       # Per-board-type hardware settings
  bitaxe:     # Bitaxe family boards (BM1370, etc.)
  cpu_miner:  # Software CPU miner (testing/development)
hash_thread:  # ASIC hash thread tuning
```

### 5.1. Full reference with defaults

See [configs/mujina.example.yaml](../configs/mujina.example.yaml) for the
annotated example file showing every key with its default value.

## 6. Testing the Priority Chain

`mujina-miner/tests/config_priority_tests.rs` contains integration tests that
exercise each layer of the priority chain end-to-end. Each test starts a real
`Daemon` instance and polls the API port to confirm the daemon bound to the
address that the winning source declared.

| Test | Layer(s) exercised |
|------|--------------------|
| `test_default_config_file` | default config file overrides hard-coded default |
| `test_user_config_override` | user config file overrides default config file |
| `test_env_var_override` | `MUJINA__*` env var overrides both config files |
| `test_command_line_arg_override` | CLI flag (direct field assignment) overrides env var and both config files |

The tests use `tempfile` to write config files in a temporary directory, so
**no root access is required** — the default config path is redirected via
`MUJINA_DEFAULT_CONFIG_PATH` during the test run.

Run only these tests with:

```sh
cargo test -p mujina-miner --test config_priority_tests
```

Because the tests mutate process-wide environment variables they are serialized
with `#[serial]` from the `serial_test` crate. Do not run them with `--test-threads > 1`.
