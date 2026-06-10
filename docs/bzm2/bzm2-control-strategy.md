# BZM2 Runtime Control Strategy

## Purpose

This note captures the recommended runtime control strategy for the BZM2
implementation in Mujina.

The goal is to move from:

- startup calibration
- saved operating-point replay
- runtime measurement and retune detection

to:

- active runtime operating-point control
- user-selectable control objectives
- safe rollback to the last validated operating point

## Current State

The current implementation already has:

- startup calibration planning
- per-domain voltage application during startup
- per-ASIC / per-PLL clock application during startup
- saved operating-point replay
- runtime measurement collection
- runtime retune detection and state reporting

The current implementation does not yet have:

- a runtime retune executor
- a closed-loop voltage / frequency controller
- user-selectable runtime control modes

## Recommended Control Modes

### 1. Target Temperature

The user specifies a desired ASIC operating temperature in Celsius.

The controller should:

- use domain voltage as the primary control variable
- reduce voltage first when thermal headroom shrinks
- reduce clock only after voltage reaches a floor or thermal recovery is too slow
- interpret the user target as a temperature band, not a single exact switching point
- apply hysteresis so the controller does not chatter between two adjacent operating points

This is the best first control mode because:

- the thermal signal is relatively smooth
- the loop is easier to stabilize than hashrate tracking
- the necessary telemetry is already available

### 2. Target Power

The user specifies a board or domain power limit.

The controller should:

- use domain voltage as the primary control variable
- reduce clock only if voltage reaches its minimum safe point and power is still too high

This is a strong second control mode because:

- rail power telemetry already exists
- the objective is operationally useful
- the loop is simpler than pure hashrate hold

### 3. Target Hashrate

The user specifies a desired hashrate.

The controller should:

- raise or lower voltage within safe limits first
- use clock as a secondary control variable
- apply stronger smoothing and hysteresis than the thermal/power modes

This should come after the first two modes because the signal is noisier and
depends on share-derived runtime estimates.

## Control Architecture

### Safety Layer

The safety layer always overrides any target objective.

Hard limits include:

- maximum ASIC temperature
- maximum board / domain power
- minimum and maximum voltage
- PLL lock failure
- DTS/VS fault state
- missing or stale telemetry

If any hard limit is violated:

- move immediately to a safer operating point
- or fall back to the last validated operating point
- or idle / shut down if no safe state is available

### Operating-Point Controller

The operating-point controller chooses how to move the hardware toward the user
objective.

Recommended priority:

1. adjust voltage in small bounded domain-local steps
2. wait for electrical and thermal settling
3. re-evaluate
4. adjust clock only if voltage is insufficient or constrained

This should be done:

- per domain first
- per PLL second when split-stack behavior is enabled

### Stability Layer

The stability layer prevents oscillation.

It should include:

- persistence thresholds
- hysteresis bands
- moving averages / EWMA smoothing
- minimum dwell time between adjustments

For temperature management specifically:

- a target such as `72 C` should be treated as an operating band rather than a
  literal single-value threshold
- for example, a controller may hold a nominal band around the target and only
  step downward once the upper edge is exceeded for a sustained period
- likewise, it should only step upward again after the lower edge has been
  crossed and the cooldown/recovery period has held long enough

That prevents:

- rapid toggling between two neighboring voltage or clock setpoints
- unnecessary PLL or rail churn
- control instability caused by normal telemetry noise or brief share-rate
  variation

## Why Not Pure PID Everywhere

Mining hardware is not a perfectly smooth control system.

Practical issues:

- hashrate measurements are noisy
- share-derived throughput is bursty
- temperature reacts slowly
- clock changes are coarse
- voltage changes need settle time

Recommended approach:

- PID-like control for temperature
- bounded step controller for power
- stepwise optimizer with smoothing for hashrate

For the temperature controller, the “PID-like” part should be wrapped in:

- explicit hysteresis bands
- minimum time-at-setting requirements
- range-based targets instead of exact-value lock points

## Recommended Actuation Order

### Voltage First

Use voltage as the primary runtime control variable because it directly affects:

- thermal output
- efficiency
- stability margin

Small voltage steps can often recover a target without a disruptive frequency
change.

### Clock Second

Use clock as the secondary control variable:

- when voltage is already at a safe minimum or maximum
- when thermal or power error remains after voltage settling
- when the user target cannot be met by voltage adjustment alone

## Runtime Metrics To Use

The runtime controller should consume:

- board throughput
- per-ASIC throughput
- per-PLL throughput where available
- ASIC temperature
- board / regulator temperature
- per-domain voltage
- per-domain power
- scheduler share counts
- current saved operating-point status

Useful derived metrics:

- watts per terahash
- joules per terahash
- temperature margin to limit
- voltage error against planned target
- normalized throughput per active engine

## Required New Components

### 1. Runtime Retune Executor

This executor should:

- consume retune requests
- build a new operating-point plan
- apply voltage and clock changes safely
- verify the result
- mark the result validated, pending, or failed

### 2. Generic Diagnostics Surface

The runtime controller should not own raw device access directly.

Instead, it should build on a generic diagnostics / control surface that can:

- read telemetry
- query current operating point
- apply domain voltage
- apply per-PLL or per-ASIC clock changes
- verify lock / health status

### 3. Rollback Path

The controller must be able to:

- revert to the last validated operating point
- preserve that operating point until a replacement is proven good

This is especially important because runtime retune is inherently riskier than
startup calibration.

## Suggested Configuration Surface

Potential user-facing control settings:

- `control_mode`
- `target_hashrate_ths`
- `target_temp_c`
- `target_power_w`
- `voltage_step_mv`
- `clock_step_mhz`
- `settle_delay_ms`
- `retune_persistence_polls`
- `retune_hysteresis_temp_c`
- `retune_hysteresis_power_w`

The exact configuration surface does not need to be finalized before the
executor exists, but the design should assume these concepts.

## Recommended Implementation Order

1. add the runtime retune executor framework
2. implement `target_temperature` with domain-voltage control
3. implement rollback to last validated operating point
4. implement `target_power`
5. implement `target_hashrate`
6. add per-PLL refinements for split-stack boards

## Best First Mode

The best first runtime control mode is:

- `target_temperature`

Reason:

- easiest to stabilize
- directly useful to operators
- uses telemetry already present in the BZM2 implementation
- safer than chasing exact hashrate with noisy share-derived measurements

## Summary

The runtime control design should evolve from advisory tuning state into an
active control system that:

- respects safety limits first
- adjusts domain voltage before clock
- uses smoothing and hysteresis
- preserves the last validated operating point until a replacement is proven
- supports user-selectable operating objectives

The main implementation gap is no longer planning logic. It is the executor
that safely applies and verifies runtime operating-point changes.
