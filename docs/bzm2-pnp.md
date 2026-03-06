# BZM2 PnP Calibration In Mujina

This note captures the current BZM2 PnP state in Mujina, what the legacy `bzmd` implementation did, and what is now implemented in the Rust port.

## Current Gap

Before this change, Mujina's BZM2 support had:

- UART work dispatch
- result parsing
- thermal and power safety shutdowns
- UART register access
- PLL and DLL control

What it did not have was the legacy PnP calibration planner:

- strategy and bin-specific target selection
- parameter sweep generation
- initial voltage and frequency selection from site temperature
- saved operating point reuse checks
- retune decisions when measured throughput regresses
- domain-aware planning for hardware with multiple voltage domains
- per-ASIC or per-stack frequency fine-tuning around a target pass-rate window

## Legacy `pnp.c` Behavior

The original C implementation mixed:

- calibration search policy
- board and PSU policy
- persisted board calibration profiles
- per-ASIC telemetry accumulation
- per-engine pass-rate accounting
- platform-specific data collection and file I/O

The reusable algorithmic parts are:

- derive target voltage, frequency, and pass rate from operating class and performance mode
- derive initial voltage and frequency from site thermal conditions
- broadcast a starting frequency
- sweep upward while respecting power and thermal guard rails
- tune back down on individual ASICs or stacks when pass rate falls outside the target window
- invalidate saved operating point when throughput regresses materially

## Mujina Ported Behavior

The new Rust module at
[C:\Users\prael\Documents\Codex\bzm2_mujina\mujina-miner\src\asic\bzm2\pnp.rs](C:\Users\prael\Documents\Codex\bzm2_mujina\mujina-miner\src\asic\bzm2\pnp.rs)
implements the reusable planner without pulling board-MCU or PSU glue into the ASIC layer.

Implemented:

- performance mode targets for:
  - generic
  - EarlyValidation
  - ProductionValidation
  - StackTunedA
  - StackTunedB
  - ExtendedHeadroom
  - ExtendedHeadroomB
- parameter sweep generation analogous to legacy `pnp_create_paramters_vector()`
- ambient-aware initial voltage and frequency planning analogous to `pnp_set_initial_voltage_frequency()`
- saved operating point reuse vs. full retune decisions
- domain-aware voltage planning using explicit voltage-domain offsets and guards
- per-domain frequency planning using aggregated pass-rate, thermal, and power data
- per-ASIC fine-tuning with optional per-stack / per-PLL behavior

## Efficiency Model

The planner is structured to scale cleanly from a single ASIC to large chains:

- one pass to aggregate domain-level metrics
- one pass to emit per-domain plans
- one pass to emit per-ASIC adjustments

That keeps the planning work effectively linear in ASIC count for normal use.

For larger systems with multiple voltage domains, the planner prefers:

- domain-level voltage decisions first
- domain-average frequency targets next
- per-ASIC or per-PLL corrections only where pass-rate or thermal data requires it

That is materially more scalable than treating a 100-ASIC machine as 100 independent full-search problems.

## Scope Boundary

The planner is now wired into `Bzm2Board` startup so Mujina can:

- execute a live pre-thread calibration phase
- persist applied calibration results as a stored profile
- replay a compatible stored profile directly on restart before falling back to retune

What still remains outside the ASIC planner layer:

- board-specific PSU ramp policy
- dynamic runtime retune from live pass-rate or throughput feedback
- reimplementation of the legacy CSV/database layer

Those pieces still belong above the ASIC planner, in board or daemon integration layers.

