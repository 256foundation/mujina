# BZM2 Mujina Port

## Architecture

This port keeps BZM2 support inside Mujina rather than reviving the original split `cgminer` + `bzmd` process model.

The legacy split looked like this:

- `cgminer` handled scheduling, pool interaction, and IPC to `bzmd`
- `bzmd` owned UART transport, job fanout, result validation, and board-management glue

In Mujina, those responsibilities map cleanly onto existing abstractions:

- `Daemon` injects a virtual `bzm2` board when configured
- `Backplane` instantiates the virtual board through `inventory`
- `board::bzm2::Bzm2Board` opens serial transports and creates hash threads
- `asic::bzm2::Bzm2Thread` performs direct UART job dispatch, telemetry parsing, and share validation
- `asic::bzm2::control` provides reusable GPIO-reset and PMBus/I2C rail sequencing primitives

A standalone Rust daemon is therefore not required for the mining path.

## Bring-Up And Shutdown

`Bzm2Board` now supports optional board-level power and reset sequencing through
the existing `Bzm2BringupPlan`.

The current generic integration path uses file-backed adapters:

- rail setpoint files for coarse voltage application
- optional rail enable files for regulator enable or precharge control
- an optional reset file for ASIC reset assertion and release

This keeps the implementation generic across custom Linux-based carriers without
hard-coding one vendor board layout or management MCU protocol.

## Implemented Behavior

The BZM2 Mujina thread now reimplements the core legacy data path and the generally reusable portions of the control path:

- 20 x 12 logical engine grid with the four excluded engines from legacy code
- enhanced-mode 4-midstate dispatch per logical engine
- version-rolling micro-jobs in slots `0, 2, 4, 8`
- UART register writes for target bits, leading-zero threshold, and timestamp count
- TDM result parsing with sequence parity matching
- nonce correction via enhanced-mode nonce gap
- in-thread Bitcoin header reconstruction and share validation before scheduler submission
- UART opcode coverage for:
  - `WRITEJOB`
  - `WRITEREG`
  - `READREG`
  - `MULTICAST_WRITE`
  - `READRESULT`
  - `NOOP`
  - `LOOPBACK`
  - `DTS_VS`
- DTS/VS generation 1 and generation 2 frame decoding
- live DTS/VS gen2 hardware-fault handling that shuts down the hash thread on thermal or voltage fault indications
- reusable GPIO reset-line control through `AsicEnable`
- reusable TPS546 PMBus rail control through `VoltageRegulator`
- reusable multi-rail bring-up and shutdown sequencing for single-rail, small-stack, and larger multi-stack designs
- UART-register-based PLL diagnostic/control flow for divider programming, enable/disable, lock polling, and readback
- UART-register-based DLL diagnostic/control flow for duty-cycle programming, enable/disable, lock polling, and fincon validation
- developer-facing UART debug CLI documented in [bzm2-uart-debug.md](C:/Users/prael/Documents/Codex/bzm2_mujina/docs/bzm2-uart-debug.md) with unicast, multicast, and broadcast examples
- domain-aware PnP calibration planner documented in [bzm2-pnp.md](C:/Users/prael/Documents/Codex/bzm2_mujina/docs/bzm2-pnp.md) for strategy/bin target selection, parameter sweeps, and per-domain plus per-ASIC tuning

## Configuration

Enable BZM2 by setting `MUJINA_BZM2_SERIAL` to one or more comma-separated serial device paths.

Supported environment variables:

- `MUJINA_BZM2_SERIAL`: required, comma-separated serial device paths
- `MUJINA_BZM2_SERIAL_PATHS`: alternate name for the same setting
- `MUJINA_BZM2_BAUD`: UART baud rate, default `5000000`
- `MUJINA_BZM2_TIMESTAMP_COUNT`: default `60`
- `MUJINA_BZM2_NONCE_GAP`: default `0x28`
- `MUJINA_BZM2_DISPATCH_MS`: redispatch interval in milliseconds, default `500`
- `MUJINA_BZM2_HASHRATE_THS`: nominal per-thread hashrate estimate, default `40`
- `MUJINA_BZM2_DTS_VS_GEN`: DTS/VS payload generation, `1` or `2`, default `2`
- `MUJINA_BZM2_ENUMERATE_CHAIN`: enable startup chain enumeration from the
  documented default `ASIC_ID`
- `MUJINA_BZM2_AUTO_ENUMERATE`: alternate name for the same setting
- `MUJINA_BZM2_ENUM_START_ID`: first assigned runtime `ASIC_ID`, default `0`
- `MUJINA_BZM2_ENUM_MAX_ASICS_PER_BUS`: comma-separated per-bus enumeration
  ceilings, default `100` per bus unless calibration topology already provides
  a larger configured count
- `MUJINA_BZM2_ENABLE_BRINGUP`: enable startup and shutdown rail/reset
  sequencing
- `MUJINA_BZM2_BRINGUP_ENABLE`: alternate name for the same setting
- `MUJINA_BZM2_RAIL_SET_PATHS`: comma-separated rail-control file paths
- `MUJINA_BZM2_RAIL_TARGET_VOLTS`: comma-separated target rail voltages for the
  bring-up plan
- `MUJINA_BZM2_RAIL_WRITE_SCALES`: optional comma-separated scale factors used
  when converting volts into the raw file value, for example `1000` for mV or
  `1000000` for uV
- `MUJINA_BZM2_RAIL_ENABLE_PATHS`: optional comma-separated enable/control file
  paths paired with the rail list
- `MUJINA_BZM2_RAIL_ENABLE_VALUES`: optional comma-separated values written to
  the rail enable paths during rail initialization
- `MUJINA_BZM2_RESET_PATH`: optional reset-control file path
- `MUJINA_BZM2_RESET_ACTIVE_LOW`: whether the reset path is active-low, default
  `true`
- `MUJINA_BZM2_BRINGUP_PRE_POWER_MS`: delay before rail initialization, default
  `10`
- `MUJINA_BZM2_BRINGUP_POST_POWER_MS`: delay after the configured rail steps,
  default `25`
- `MUJINA_BZM2_BRINGUP_RELEASE_RESET_MS`: delay after reset release, default
  `25`

Startup enumeration notes:

- this mode is intended for fresh chains where ASICs still answer on the
  default `ASIC_ID`
- enumeration uses a bounded `NOOP` probe so the chain walk terminates cleanly
  at the end of the bus
- if no default-id ASIC responds on startup, Mujina falls back to the configured
  `MUJINA_BZM2_ASICS_PER_BUS` topology so warm-restart cases do not collapse to
  zero ASICs

<<<<<<< HEAD
Bring-up notes:

- if bring-up is enabled, `Bzm2Board` applies the configured rail/reset
  sequence before chain discovery, calibration, and hash-thread creation
- on board shutdown, the same plan is used in reverse order to assert reset and
  drive the configured rails back to `0`
- the current implementation is intentionally coarse-grained: it provides real
  board lifecycle sequencing now, while richer rail telemetry and domain-aware
  voltage application remain the next steps

## API Telemetry

When Gen2 `DTS_VS` frames are present on the UART path, Mujina now surfaces ASIC-internal telemetry through the normal board API state:

- `BoardState.temperatures`
- `BoardState.powers`

The values are named per serial bus and per ASIC so they can coexist with host-side sensor files:

- temperature: `ttyUSB0-asic-2-dts`
- voltage channels: `ttyUSB0-asic-2-vs-ch0`, `ttyUSB0-asic-2-vs-ch1`, `ttyUSB0-asic-2-vs-ch2`

Example JSON fragment:

```json
{
  "temperatures": [
    { "name": "ttyUSB0-asic-2-dts", "temperature_c": 64.5 }
  ],
  "powers": [
    { "name": "ttyUSB0-asic-2-vs-ch0", "voltage_v": 0.78, "current_a": null, "power_w": null },
    { "name": "ttyUSB0-asic-2-vs-ch1", "voltage_v": 0.79, "current_a": null, "power_w": null },
    { "name": "ttyUSB0-asic-2-vs-ch2", "voltage_v": 0.77, "current_a": null, "power_w": null }
  ]
}
```

Notes:

- these ASIC-originated entries are merged into board state and do not replace host-file telemetry
- Celsius and voltage scaling follow the legacy `bzmd` DTS/VS conversion formulas
- Gen1 currently exposes voltage through this path, but not a Celsius temperature reading

## On-Demand ASIC Sensor Queries

Mujina now supports explicit DTS/VS query operations in addition to passive frame reporting.

This is useful when:

- the miner is idle and no passive DTS/VS traffic is arriving
- one ASIC is misbehaving and needs targeted inspection
- developers want a direct sensor read without enabling a full TDM watch session

Two access paths are implemented:

- CLI: `mujina-bzm2-debug dts-vs-query` and `mujina-bzm2-debug dts-vs-scan`
- HTTP API: `POST /api/v0/boards/{name}/bzm2/dts-vs-query`

The query path runs through the live BZM2 hash-thread actor so UART ownership remains correct. Queried frames are converted through the same telemetry code path used for passive DTS/VS reporting, so the returned values land in normal `BoardState` telemetry.

Example HTTP request:

```bash
curl -X POST http://127.0.0.1:3000/api/v0/boards/bzm2-0/bzm2/dts-vs-query \
  -H "Content-Type: application/json" \
  -d '{"thread_index":0,"asic":2}'
```

See [bzm2-uart-debug.md](bzm2-uart-debug.md) for CLI usage examples and expected output shape.
## Design Boundary

The legacy `bzmd` board-power path mixes three different concerns:

- genuinely reusable sequencing concepts
- generic peripheral protocols like PMBus/I2C regulators and reset GPIOs
- highly board-specific MCU command sets, sysfs GPIO numbering, CAN PSU control, and platform wiring assumptions

Only the first two belong in a generally applicable Mujina BZM2 implementation.

Ported into Mujina:

- generic reset assertion/deassertion
- generic ordered rail bring-up and shutdown
- generic PMBus/TPS546 voltage control and telemetry adapters
- ASIC-originated DTS/VS telemetry and fault handling

Intentionally not ported verbatim:

- Intel board MCU command protocol from `mcu.c`
- hard-coded board GPIO numbering and sysfs reset pulses from `util.c` / `daemon.c`
- platform CAN PSU control from `psu.c`
- board-specific fan and ambient-sensor plumbing that depends on the original platform layout

Those pieces should only be added behind a concrete Mujina board implementation when the target hardware actually uses them.

## Current Limits

Still not implemented from the broader legacy stack:

- JTAG workflows from the standalone platform documents
- JTAG-only PLL debug sequences that are not represented in the shipped UART code
- calibration and autotuning state machines
- manufacturing and diagnostics RPC surface
- any board-MCU protocol that is specific to one carrier or backplane design

The top-level `docs` PDFs reference additional JTAG and opcode material, but this port currently implements the opcode surface that is evidenced in the legacy shipping UART path and not an inferred JTAG control plane.


See also:

- docs/bzm2-opcode-grounding.md for the source-grounded opcode matrix and the current JTAG evidence boundary
