# BZM2 PnP Calibration In Mujina

This note captures the current BZM2 PnP state in Mujina, what the legacy `bzmd` implementation did, and what is now implemented in the Rust port.

## Current Gap

Before this change, Mujina's BZM2 support had:

- UART work dispatch
- result parsing
- thermal and power safety shutdowns
- UART register access
- PLL and DLL control

What it did not have was a native Mujina tuning planner for BZM2:

- operating-class and performance-mode target selection
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

- derive voltage, clock, and acceptance targets from operating class and performance mode
- derive initial voltage and clock from site thermal conditions
- broadcast a starting frequency
- sweep upward while respecting power and thermal guard rails
- tune back down on individual ASICs or stacks when pass rate falls outside the target window
- invalidate saved operating point when throughput regresses materially

## Mujina Ported Behavior

The new Rust module at
`mujina-miner/src/tuning/blockscale.rs`
implements the reusable planner without pulling board-MCU or PSU glue into the ASIC layer.

Implemented:

- operating-class target tables for:
  - generic
  - EarlyValidation
  - ProductionValidation
  - StackTunedA
  - StackTunedB
  - ExtendedHeadroom
  - ExtendedHeadroomB
- search-space generation corresponding to the historical C sweep helper
- site-temperature-aware initial voltage and clock planning corresponding to the historical C startup helper
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
- persist applied calibration results as a saved operating point profile
- replay a compatible saved operating point profile directly on restart before falling back to retune
- collect live runtime tuning measurements during mining for:
  - board throughput
  - per-ASIC throughput
  - per-ASIC average pass rate
  - per-PLL throughput and pass rate
  - per-domain measured rail voltage and power
- normalize saved-throughput comparisons and planned board hashrate against
  actual active-engine capacity instead of assuming every ASIC still has the
  default full map
- run the same planner against live runtime measurements during mining so the
  board can continuously evaluate whether the current operating point is still
  valid
- automatically promote saved operating point state from `pending` to
  `validated` after clean runtime sampling
- automatically invalidate saved operating point profiles when persistent
  runtime retune triggers fire, so restart replay will not reuse a known-bad
  operating point

Engine-capacity inputs now come from, in order:

- live pre-calibration engine discovery when enabled
- saved per-ASIC topology embedded in the saved operating point profile
- default BZM2 hole-map fallback when no better topology data is available

That means tuning decisions can now distinguish between:

- a slow ASIC that still has full engine capacity
- an ASIC that is throughput-limited because it has permanently missing engines

What still remains outside the ASIC planner layer:

- board-specific PSU ramp policy
- reimplementation of the legacy CSV/database layer

Those pieces still belong above the ASIC planner, in board or daemon integration layers.
