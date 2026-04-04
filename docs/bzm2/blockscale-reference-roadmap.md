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

## Generic Breakout Targets

The long-term goal is not just a strong BZM2 port, but a reusable mining-board
reference architecture inside Mujina.

These pieces should remain shaped so they can become general-purpose facilities:

- `board::power`
  - rail sequencing
  - reset-line orchestration
  - reusable rail telemetry adapters
- `asic::hash_thread`
  - thread telemetry events
  - idle-only diagnostics gatekeeping
  - generic scheduler-facing lifecycle hooks
- `tuning`
  - board-independent tuning inputs and outputs
  - reusable search-space generation
  - reusable replay / pending / validated operating-point states
- board command plumbing
  - generic board-side diagnostics RPC pattern
  - per-board command enums only for truly ASIC-specific operations
- protocol documentation pattern
  - keep wire-level references vendor-specific
  - keep planning, telemetry, and power-control descriptions reusable

The pieces that should stay BZM2-specific are:

- UART opcode packing and TDM parsing
- BZM2 engine topology model
- BZM2 PLL/DLL register programming
- BZM2 DTS/VS payload decoding

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

1. Wire `VoltageStackBringupPlan` into `Bzm2Board`
2. Add a concrete board-facing rail bundle abstraction:
   - one or more rails
   - optional reset line
   - optional rail telemetry
3. Apply safe startup and shutdown sequencing through the board runtime
4. Expose rail telemetry into board state where available

Status:

- completed: `VoltageStackBringupPlan` is now wired into `Bzm2Board` startup and
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

Status:

- completed: TDM-sync engine probe helpers now detect physical engine presence
  by reading `ENGINE_REG_END_NONCE`, matching the historical C detection path
- completed: the debug CLI now supports:
  - `engine-probe`
  - `discover-engine-map`
- completed: discovered per-ASIC engine maps can now be pushed into live
  `BoardState.asics` through:
  - `Bzm2Board` command handling
  - the live BZM2 thread actor
  - `POST /api/v0/boards/{name}/bzm2/discover-engines`
- completed: successful discovery scans now update the live BZM2 runtime engine
  layout used by:
  - work dispatch fanout
  - result reconstruction
  - share validation helpers
- completed: calibration input now consumes active-engine counts and missing
  coordinates through:
  - live pre-calibration engine discovery when enabled
  - saved operating-point topology replay
  - default-map fallback when no topology data exists
- completed: saved operating-point reuse and planned hashrate estimation now
  normalize against real engine capacity instead of assuming every ASIC has the
  default full map
- next: Phase 5, closed-loop calibration and retune

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

Status:

- completed: live BZM2 threads now maintain work-based runtime throughput
  estimators for:
  - whole-thread throughput
  - per-ASIC throughput
  - per-PLL throughput using the documented row 0-9 / row 10-19 stack split
- completed: `Bzm2Board` now samples and stores runtime tuning measurements
  into live board state and an internal cache, including:
  - board throughput
  - per-ASIC throughput
  - per-ASIC average pass rate
  - per-PLL pass rate and throughput
  - per-domain measured voltage and power
- completed: the board runtime now feeds those live measurements back into the
  existing tuning planner and publishes the current planner decision through
  board state, including:
  - reuse-saved-operating-point decision
  - needs-retune decision
  - desired voltage / clock / accept-ratio targets
  - planner notes
- completed: runtime retune triggers are now promoted only after configurable
  persistence across monitor polls for:
  - throughput regression
  - thermal drift
  - persistent voltage imbalance
- completed: saved operating point profiles now carry runtime validation state
  and are automatically:
  - marked `validated` after clean runtime sampling
  - marked `invalidated` when persistent retune triggers fire
  - excluded from direct replay and planner seeding on later restarts once
    invalidated
- next: Phase 6, diagnostics and API parity

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

Status:

- completed: board/API parity now covers live BZM2 thread-routed commands for:
  - `NOOP`
  - loopback
  - register read/write
- completed: those diagnostics are exposed through HTTP endpoints that preserve
  UART ownership by routing through the live BZM2 thread actor
- completed: the board/API surface now exposes `clock-report` parity through
  the same live thread actor, so operators can inspect PLL/DLL lock state and
  clock-control registers without dropping to the standalone CLI
- completed: the board/API surface now exposes a chain-summary view with:
  - current per-bus serial path
  - global ASIC ranges
  - total discovered/configured ASIC count
  - the startup path that produced the active operating point
  - saved operating point validation state
- completed: Phase 6 exit criteria are now met for the planned board/API
  diagnostics slice
- next:
  - surface more of the same diagnostics through board state where useful
  - only add broader manufacturing parity if there is a clear operator need

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
