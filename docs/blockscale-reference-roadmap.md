# Blockscale / BZM2 Reference Implementation Roadmap

## Goal

Close the remaining gap between:

- a strong ASIC-facing Rust port with solid debug tooling

and

- a comprehensive, reusable reference implementation for custom Blockscale /
  BZM2 hardware.

This roadmap is ordered by dependency and practical value.

## Scope

In scope:

- generic ASIC bring-up
- generic chain discovery
- reusable domain-aware power and tuning control
- board/API diagnostics
- runtime retune

Out of scope for this plan:

- vendor reference-board reproduction
- carrier-specific MCU protocols unless a target board actually needs them
- Gen1 telemetry completion
- speculative JTAG implementation not grounded in concrete protocol evidence

## Current Gap Summary

The current repo already has:

- UART opcode support
- TDM parsing
- mining dispatch and result handling
- PLL and DLL diagnostics
- DTS/VS telemetry and query tooling
- startup tuning planning and saved operating-point replay
- a strong silicon-validation CLI

The biggest missing pieces are:

1. runtime engine/topology discovery instead of fixed assumptions
2. closed-loop calibration and retune
3. board/API diagnostics parity with the CLI

## Phase 1: Discoverable Bring-Up

Objective:

- eliminate the assumption that ASIC count and identity are fully preconfigured

Deliverables:

1. Add low-level UART helpers for:
   - writing `ASIC_ID`
   - enumerating a chain starting from default `0xFA`
   - verifying assigned IDs with `NOOP`
2. Add debug CLI support for:
   - chain enumeration
   - ID assignment validation
3. Add optional board startup enumeration mode so `Bzm2Board` can populate bus
   layout from hardware rather than only from `MUJINA_BZM2_ASICS_PER_BUS`

Status:

- completed: low-level default-`ASIC_ID` enumeration helpers
- completed: `enumerate-chain` CLI support
- completed: opt-in `Bzm2Board` startup enumeration with fallback to
  configured topology when no default-id ASICs are present
- next: Phase 2, applied rail and reset control

Exit criteria:

- a powered chain can be discovered from software with no hard-coded ASIC count
- the discovered count can seed board topology and saved operating-point
  compatibility checks

## Phase 2: Applied Rail And Reset Control

Objective:

- move the existing control abstractions from library-only status into real
  board startup and shutdown flows

Deliverables:

1. Wire `Bzm2BringupPlan` into `Bzm2Board`
2. Add a concrete board-facing rail bundle abstraction:
   - one or more rails
   - optional reset line
   - optional rail telemetry
3. Apply safe startup and shutdown sequencing through the board runtime
4. Expose rail telemetry into board state where available

Status:

- completed: `Bzm2BringupPlan` is now wired into `Bzm2Board` startup and
  shutdown through generic file-backed rail and reset adapters
- completed: optional file-backed rail telemetry now flows into `BoardState`
- next: map planned domain voltages onto those startup/shutdown hooks

Exit criteria:

- board startup can perform reset and rail sequencing without external manual
  steps
- board shutdown returns the hardware to a safe state

## Phase 3: Domain Voltage Application

Objective:

- make the tuning planner’s voltage-domain outputs real rather than advisory

Deliverables:

1. Map planned domain voltages onto configured rails
2. Apply coarse domain voltages before clock ramp
3. Use rail telemetry and ASIC `DTS_VS` readings to verify applied state
4. Persist replay metadata that distinguishes:
   - clock-only replay
   - full voltage-plus-clock replay

Exit criteria:

- `Bzm2Board` can apply multi-domain operating points, not just PLL maps

Status:

- completed: planner-generated per-domain voltages are now mapped onto the
  configured rail-control path before PLL ramp
- completed: saved operating-point replay now reapplies persisted per-domain
  voltages before clock replay
- completed: live calibration persists per-domain rail targets for restart
  replay
- next: Phase 4, topology and defect discovery

## Phase 4: Topology And Defect Discovery

Objective:

- stop assuming the default logical engine map is always the real map

Deliverables:

1. Add engine/topology probing helpers
2. Detect unavailable or disabled engines per ASIC
3. Feed the discovered engine map into:
   - work dispatch
   - validation helpers
   - tuning calculations

Exit criteria:

- systems with missing or disabled engines do not need a code rebuild or static
  exclusion map edit

## Phase 5: Closed-Loop Calibration And Retune

Objective:

- turn the startup planner into a true operating-point controller

Deliverables:

1. Measure and store real:
   - pass rate
   - throughput
   - per-PLL behavior
   - per-domain power
2. Feed those measurements back into the tuning planner
3. Add runtime retune triggers for:
   - throughput regression
   - thermal drift
   - persistent voltage imbalance
4. Revalidate or invalidate saved operating points automatically

Exit criteria:

- tuning decisions are based on measured runtime behavior, not just startup
  heuristics and persisted estimates

## Phase 6: Diagnostics And API Parity

Objective:

- expose the most useful silicon-validation operations without requiring the
  standalone CLI

Deliverables:

1. Board/API commands for:
   - `NOOP`
   - loopback
   - register read/write
   - clock report
   - chain enumeration summary
2. Board-state visibility for:
   - discovered ASIC count
   - discovered engine count / disabled-engine map
   - saved operating-point replay path
   - current calibration and safety status

Exit criteria:

- operators can perform high-value diagnostics through the board/API surface
  without dropping to raw serial tooling

## Phase 7: JTAG, Only If Grounded

Objective:

- add JTAG only if enough packet-level evidence exists to do it correctly

Deliverables:

1. protocol-evidence review
2. minimal IR/DR helpers only if grounded
3. debug-only tooling for validated use cases

Exit criteria:

- no guessed JTAG semantics enter the codebase

## Immediate Execution Order

The next concrete work items should be:

1. implement generic chain enumeration and `ASIC_ID` assignment helpers
2. add a debug CLI command that enumerates a live chain
3. add optional board startup auto-enumeration using that helper
4. then wire generic rail/reset sequencing into `Bzm2Board`

## Current Execution Status

Started:

- Phase 1 step 1 and step 2

Reason:

- enumeration removes a major assumption from the current board runtime
- it is ASIC-generic
- it is directly grounded in documented and legacy UART behavior
