*Mujina Configuration*

This document describes the configuration system for `mujina-minerd`.

- [1. Priority Order](#1-priority-order)
- [2. Config Files](#2-config-files)
  - [2.1. Default location](#21-default-location)
  - [2.2. Specifying a config file](#22-specifying-a-config-file)
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
| 1 (highest) | `--set key=value` CLI overrides |
| 2 | Environment variables (`MUJINA__*`) |
| 3 | Config file specified via `--config` |
| 4 | Default config file (`/etc/mujina/mujina.yaml`) |
| 5 (lowest) | Hard-coded defaults |

All sources are optional except the hard-coded defaults, which are always
present. A minimal deployment with no config file and no environment variables
will start with sensible defaults (dummy job source, API on localhost).

## 2. Config Files

### 2.1. Default location

`/etc/mujina/mujina.yaml`

This is the standard system-wide config file, suitable for installation by a
package manager or system administrator. It is optional — if absent, Mujina
starts with hard-coded defaults.

### 2.2. Specifying a config file

Use the `--config` flag to load a config file from any path:

```sh
mujina-minerd --config /home/operator/mujina.yaml
```

Keys in the specified file take precedence over `/etc/mujina/mujina.yaml`.
Keys absent from the file fall back to the default file, then to hard-coded
defaults.

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
| `MUJINA_POOL_FORCED_RATE` | `pool.forced_rate` | `MUJINA__POOL__FORCED_RATE` |
| `MUJINA_API_LISTEN` | `api.listen` | `MUJINA__API__LISTEN` |
| `MUJINA_USB_DISABLE` | `backplane.usb_enabled` | `MUJINA__BACKPLANE__USB_ENABLED` |
| `MUJINA_CPUMINER_THREADS` | `boards.cpu_miner.threads` | `MUJINA__BOARDS__CPU_MINER__THREADS` |
| `MUJINA_CPUMINER_DUTY` | `boards.cpu_miner.duty_percent` | `MUJINA__BOARDS__CPU_MINER__DUTY_PERCENT` |

`MUJINA_API_URL` (used by `mujina-cli` to locate the daemon) is not part of
the daemon config and is unchanged.

## 4. CLI Flags

CLI flags override all other sources and are intended for one-off overrides and
testing, not permanent configuration.

```
USAGE:
    mujina-minerd [OPTIONS]

OPTIONS:
    -c, --config <PATH>           Config file path (overrides /etc/mujina/mujina.yaml)
        --set <KEY=VALUE>         Override a config key (may be repeated)
    -h, --help                    Print help
    -V, --version                 Print version
```

`--set` uses the same dot-path namespace as the YAML file, so any key from
the YAML structure can be overridden without a dedicated flag:

```sh
mujina-minerd \
  --set pool.url=stratum+tcp://pool.example.com:3333 \
  --set pool.user=bc1q....worker \
  --set api.listen=0.0.0.0:7785 \
  --set boards.cpu_miner.enabled=true
```

Multiple `--set` flags are applied in order; later values win if the same key
appears more than once.

## 5. YAML Structure

The config file uses YAML. The top-level keys correspond to subsystems:

```yaml
daemon:       # Process and logging settings
api:          # HTTP API server
pool:         # Mining pool connection (primary)
backplane:    # Board discovery and lifecycle
boards:       # Per-board-type hardware settings
  cpu_miner:  # Software CPU miner (testing/development)
```

### 5.1. Full reference with defaults

See [configs/mujina.example.yaml](../configs/mujina.example.yaml) for the
annotated example file showing every key with its default value.

## 6. Testing the Priority Chain

`mujina-miner/tests/daemon_integration_tests.rs` contains integration tests that
exercise each layer of the priority chain end-to-end. Each test starts a real
`Daemon` instance and polls the API port to confirm the daemon bound to the
address that the winning source declared.

| Test | Layer(s) exercised |
|------|--------------------|
| `test_cli_config_file_is_read` | `--config` file overrides hard-coded default |
| `test_env_var_overrides_config_file` | `MUJINA__*` env var overrides `--config` file |
| `test_command_line_arg_override` | `--set` override wins over env var and `--config` file |
| `test_cpu_miner_starts_from_config` | CPU miner board starts from `--config` file |

The tests use `tempfile` to write config files in a temporary directory, so
**no root access is required**.

Run only these tests with:

```sh
cargo test -p mujina-miner --test daemon_integration_tests
```

Because some tests mutate process-wide environment variables they are serialized
with `#[serial]` from the `serial_test` crate. Do not run them with `--test-threads > 1`.
