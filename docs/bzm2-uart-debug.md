# BZM2 UART Debug Guide

This guide documents the direct BZM2 UART and silicon-validation interface added to Mujina.

## Intent

The current CLI folds in the portable parts of the legacy `silicon validation` BZM2 validation surface:

- chain enumeration and `ASIC_ID` assignment from the default bus id
- ASIC discovery and liveness scans
- loopback data-path validation
- explicit TDM enable and disable control
- live TDM result, register, noop, and DTS/VS observation
- broadcast TDM register-read observation across many ASICs
- engine presence probing and full physical engine-map discovery
- engine-wide timestamp, target, and leading-zero programming
- deterministic grid-job exercisers for single-phase and back-to-back job dispatch
- existing PLL and DLL diagnostics and bring-up helpers

What is deliberately not ported here:

- legacy board-reset and board-count orchestration
- BCH-vector and nonce-golden-data regression harnesses
- opaque manufacturing hooks and carrier-specific glue

## Routing Modes

- unicast: one ASIC, one destination address
- broadcast: all ASICs on the UART bus via ASIC id `0xff`
- multicast: one ASIC, one engine-row group

## Binary

Use [bzm2-debug.rs](C:/Users/prael/Documents/Codex/bzm2_mujina/mujina-miner/src/bin/bzm2-debug.rs) through Cargo:

```text
cargo run -p mujina-miner --bin mujina-bzm2-debug -- <command> ...
```

## Legacy Test Mapping

The most useful `silicon validation` tests map to the following commands:

- `test_noop_all_asic` -> `noop-scan`
- `test_loopback` -> `loopback-scan`
- `test_effbst_tdm_selectable_leadingzeros` -> `engine-zeros-all` plus `job-grid-watch`
- `test_effbst_tdm_jobsubmit_with_writejobcmd` -> `job-grid` or `job-grid-watch`
- `test_effbst_tdm_continuous_b2b_jobs` -> `job-grid-2phase-watch`
- `uart_command_broadcast_readreg_tdm_async` flows -> `tdm-broadcast-read-watch`
- general TDM callback validation -> `tdm-watch`
- historical engine-detect flow -> `engine-probe` or `discover-engine-map`

The old multicasted-EFFBST and auto-clock-gating tests depended on the legacy BCH vector library and runtime board harness. The new CLI does not claim golden nonce validation there; it provides deterministic packet generation and live observation so developers can still exercise the same ASIC paths when debugging hardware.

## Direct UART Examples

Read one ASIC-local register:

```text
cargo run -p mujina-miner --bin mujina-bzm2-debug -- \
  uart-read /dev/ttyUSB0 2 notch 0x12 4 5000000
```

Write one ASIC-local register:

```text
cargo run -p mujina-miner --bin mujina-bzm2-debug -- \
  uart-write /dev/ttyUSB0 2 notch 0x12 01000000 5000000
```

Run a NOOP sanity check across an ASIC range:

```text
cargo run -p mujina-miner --bin mujina-bzm2-debug -- \
  noop-scan /dev/ttyUSB0 0 15 5000000
```

Enumerate a powered chain from the default `ASIC_ID` (`0xFA`) and assign
incrementing ids starting at `0`:

```text
cargo run -p mujina-miner --bin mujina-bzm2-debug -- \
  enumerate-chain /dev/ttyUSB0 32 0 5000000
```

This is the first generic bring-up step for a fresh chain where every ASIC is
still on the default id.

The chain walk now uses a bounded `NOOP` probe internally, so the command stops
cleanly when no additional default-id ASIC is present instead of hanging at the
end of the bus.

The same discovery flow can be enabled during Mujina board startup with:

- `MUJINA_BZM2_ENUMERATE_CHAIN=true`
- optional `MUJINA_BZM2_ENUM_START_ID`
- optional `MUJINA_BZM2_ENUM_MAX_ASICS_PER_BUS`

Run deterministic loopback validation across an ASIC range:

```text
cargo run -p mujina-miner --bin mujina-bzm2-debug -- \
  loopback-scan /dev/ttyUSB0 0 15 8 5000000
```

Read one synchronous result frame from one ASIC:

```text
cargo run -p mujina-miner --bin mujina-bzm2-debug -- \
  uart-read-result /dev/ttyUSB0 2 gen2 5000000
```

## TDM Control and Observation

The legacy `enable_tdm()` path is grounded in a local-register write to `LOCAL_REG_UART_TDM_CTL` (`0x07`) with control word:

```text
(prediv_raw << 9) | (tdm_counter << 1) | enable_bit
```

Enable TDM explicitly:

```text
cargo run -p mujina-miner --bin mujina-bzm2-debug -- \
  tdm-enable /dev/ttyUSB0 0x12 16 5000000
```

Disable TDM explicitly:

```text
cargo run -p mujina-miner --bin mujina-bzm2-debug -- \
  tdm-disable /dev/ttyUSB0 0x12 16 5000000
```

Watch the mixed TDM stream for five seconds:

```text
cargo run -p mujina-miner --bin mujina-bzm2-debug -- \
  tdm-watch /dev/ttyUSB0 gen2 5 5000000
```

Broadcast a register read and collect returned TDM read frames from ASICs `0` through `15`:

```text
cargo run -p mujina-miner --bin mujina-bzm2-debug -- \
  tdm-broadcast-read-watch /dev/ttyUSB0 gen2 0 15 notch 0x12 4 5000000
```

Probe one physical engine coordinate using the historical TDM-sync
`ENGINE_REG_END_NONCE` detection rule:

```text
cargo run -p mujina-miner --bin mujina-bzm2-debug -- \
  engine-probe /dev/ttyUSB0 2 0 0 0x0f 16 100 5000000
```

Scan the full `20 x 12` physical grid and report the discovered hole map for
one ASIC:

```text
cargo run -p mujina-miner --bin mujina-bzm2-debug -- \
  discover-engine-map /dev/ttyUSB0 2 0x0f 16 100 5000000
```

Notes:

- these commands intentionally scan all physical coordinates, not the legacy
  default engine list
- discovery uses the source-grounded rule from the historical C path:
  `ENGINE_REG_END_NONCE == 0xfffffffe` means the engine is present
- the CLI reports whether the discovered missing coordinates still match the
  default four-hole BZM2 map or whether the ASIC diverges from that assumption

## Engine-Wide Programming Helpers

Set the target register across every engine row on one ASIC or the whole bus:

```text
cargo run -p mujina-miner --bin mujina-bzm2-debug -- \
  engine-target-all /dev/ttyUSB0 2 0x1705ffff 5000000
```

Set timestamp count across all engine rows:

```text
cargo run -p mujina-miner --bin mujina-bzm2-debug -- \
  engine-timestamp-all /dev/ttyUSB0 2 60 5000000
```

Program selectable leading zeros across all engine rows:

```text
cargo run -p mujina-miner --bin mujina-bzm2-debug -- \
  engine-zeros-all /dev/ttyUSB0 2 48 5000000
```

`engine-zeros-all` accepts `32..=64`, matching the legacy validation flow. The actual register value written is `zeros_to_find - 32`, which is what the C source used.

## Job Exercisers

These commands intentionally generate deterministic synthetic job material. They are designed to exercise the ASIC job path and TDM-result machinery without pretending to replace the legacy golden-vector regression harness.

Dispatch one full grid of `WRITEJOB` packets:

```text
cargo run -p mujina-miner --bin mujina-bzm2-debug -- \
  job-grid /dev/ttyUSB0 2 0 1700000000 60 5000000
```

Dispatch one full grid and watch live TDM responses:

```text
cargo run -p mujina-miner --bin mujina-bzm2-debug -- \
  job-grid-watch /dev/ttyUSB0 2 0 1700000000 60 5 gen2 5000000
```

Dispatch back-to-back grids with the second phase offset by the legacy `MAX_EFFBST_SUBJOBS` sequence spacing:

```text
cargo run -p mujina-miner --bin mujina-bzm2-debug -- \
  job-grid-2phase-watch /dev/ttyUSB0 2 0 1700000000 60 5 gen2 5000000
```

## Broadcast, Multicast, and Unicast Reference

Use unicast when you need one ASIC-local or one engine-local change:

```text
cargo run -p mujina-miner --bin mujina-bzm2-debug -- \
  uart-write /dev/ttyUSB0 2 notch 0x12 01000000 5000000
```

Use broadcast when every ASIC on the bus should receive the same packet:

```text
cargo run -p mujina-miner --bin mujina-bzm2-debug -- \
  uart-write /dev/ttyUSB0 broadcast notch 0x12 01000000 5000000
```

Use multicast when one ASIC needs the same engine-register update across a full row group:

```text
cargo run -p mujina-miner --bin mujina-bzm2-debug -- \
  uart-multicast-write /dev/ttyUSB0 2 7 0x48 3c 5000000
```

The higher-level helpers such as `engine-timestamp-all`, `engine-zeros-all`, and `job-grid` are implemented on top of this same routing model so developers can either stay at the helper level or drop down to raw packet control when needed.

## Clock Diagnostics

Read PLL and DLL status for one ASIC:

```text
cargo run -p mujina-miner --bin mujina-bzm2-debug -- \
  clock-report /dev/ttyUSB0 2 5000000
```

Program and lock one PLL on one ASIC:

```text
cargo run -p mujina-miner --bin mujina-bzm2-debug -- \
  pll-set /dev/ttyUSB0 2 pll1 625 0 5000000
```

Program, enable, and validate one DLL on one ASIC:

```text
cargo run -p mujina-miner --bin mujina-bzm2-debug -- \
  dll-set /dev/ttyUSB0 2 dll1 55 5000000
```

Broadcast one PLL program and verify lock across ASIC ids `0` through `15`:

```text
cargo run -p mujina-miner --bin mujina-bzm2-debug -- \
  pll-broadcast-lock-check /dev/ttyUSB0 pll0 625 0 0 15 5000000
```

Broadcast one DLL program and verify lock and `fincon` validity across ASIC ids `0` through `15`:

```text
cargo run -p mujina-miner --bin mujina-bzm2-debug -- \
  dll-broadcast-lock-check /dev/ttyUSB0 dll0 50 0 15 5000000
```

## Scope Boundary

This interface is grounded in the legacy shipped UART path and the `silicon validation` validation source. It is not a JTAG debug interface and it is not a full BCH-vector regression harness.