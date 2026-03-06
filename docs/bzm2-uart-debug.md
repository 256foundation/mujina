# BZM2 UART Debug Guide

This guide documents the direct BZM2 UART developer interface added to Mujina.

## Routing Modes

- unicast: one ASIC, one destination address
- broadcast: all ASICs on the UART bus via ASIC id `0xff`
- multicast: one ASIC, one engine-row group

## Binary

Use [bzm2-debug.rs](C:/Users/prael/Documents/Codex/bzm2_mujina/mujina-miner/src/bin/bzm2-debug.rs) through Cargo:

```text
cargo run -p mujina-miner --bin mujina-bzm2-debug -- <command> ...
```

## Unicast Examples

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

Run a NOOP sanity check:

```text
cargo run -p mujina-miner --bin mujina-bzm2-debug -- \
  uart-noop /dev/ttyUSB0 2 5000000
```

Run a loopback payload echo:

```text
cargo run -p mujina-miner --bin mujina-bzm2-debug -- \
  uart-loopback /dev/ttyUSB0 2 aabbccdd 5000000
```

## Broadcast Examples

Broadcast-enable a PLL register across every ASIC on the bus:

```text
cargo run -p mujina-miner --bin mujina-bzm2-debug -- \
  uart-write /dev/ttyUSB0 broadcast notch 0x12 01000000 5000000
```

Broadcast a full PLL program and enable sequence:

```text
cargo run -p mujina-miner --bin mujina-bzm2-debug -- \
  pll-set /dev/ttyUSB0 broadcast pll0 625 0 5000000
```

Broadcast a DLL duty-cycle configuration:

```text
cargo run -p mujina-miner --bin mujina-bzm2-debug -- \
  dll-set /dev/ttyUSB0 broadcast dll0 50 5000000
```

## Multicast Example

Set timestamp count across one ASIC row-group:

```text
cargo run -p mujina-miner --bin mujina-bzm2-debug -- \
  uart-multicast-write /dev/ttyUSB0 2 7 0x48 3c 5000000
```

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

## Scope Boundary

This interface is grounded in the legacy shipped UART path:

- register read/write
- multicast write
- noop/loopback
- PLL divider programming, enable, and lock polling
- DLL duty-cycle programming, enable, lock polling, and fincon validation

It is not a JTAG debug interface.