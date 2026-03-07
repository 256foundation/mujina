# Blockscale / BZM2 UART And TDM Protocol Reference

## Purpose

This document summarizes the ASIC-facing UART protocol, TDM behavior, and job
programming model needed to write host software or validation tooling for the
Blockscale / BZM2 ASIC family.

It is written as a practical reference for:

- firmware developers
- board bring-up engineers
- validation engineers
- host software developers

## Link Characteristics

The vendor material and legacy host software consistently assume:

- default ASIC UART baud: `5 Mbps`
- `1.2 V` IO signaling on UART-related pads
- a reference / slow-clock environment derived from a `50 MHz` source during
  early bring-up

Practical recommendation:

- establish communication at `5 Mbps` first
- validate signal integrity there before attempting any custom timing changes

## Addressing Model

Each command carries:

- `ASIC_ID`
- opcode
- engine or group identifier
- register offset or data payload, depending on opcode

The documented system addressing model allocates:

- normal engine space for engine tiles
- top `0xFxx` engine-ID region for non-engine / local functions such as PLLs,
  sensors, and internal controller blocks

Do not assume engine IDs are contiguous or fully populated. The vendor material
explicitly allows holes due to disabled or missing engines.

## ASIC Identification

Relevant ID values used during system bring-up:

- `0xFA`: default ID before enumeration
- `0xFF`: platform-wide broadcast ID

Enumeration rule:

- writing a unique `ASIC_ID` to an ASIC previously addressed at `0xFA` also
  unlocks forwarding so the next ASIC becomes reachable

## Routing Modes

### Unicast

Use unicast when you need:

- one register or job to one engine in one ASIC
- per-ASIC tuning
- precise debug
- result ownership validation

### Broadcast

Use broadcast when you need:

- the same action on the same target across all ASICs
- coarse startup programming
- coarse frequency ramps
- fast deployment of common work or dummy work

### Multicast

Use multicast when you need:

- row-wise engine programming
- efficient fanout inside one ASIC or across ASICs
- validation flows that target equivalent engine positions

The vendor material treats multicast write as a row-group fanout mechanism and
also uses it as the basis for some broadcast-style write patterns.

## Opcode Summary

| Opcode | Name | Primary use | Typical response |
| --- | --- | --- | --- |
| `0x0` | `WRITEJOB` | Write engine job state and launch work | No immediate payload response |
| `0x1` | `READRESULT` | Poll ASIC result buffer in non-TDM mode | Result record with status |
| `0x2` | `WRITEREG` | Write engine or local registers | No immediate payload response |
| `0x3` | `READREG` | Read engine or local registers | Register data |
| `0x4` | `MULTICAST_WRITE` | Write a row-group of engines | No immediate payload response |
| `0xD` | `DTS_VS` | Read thermal and voltage sensor data | Sensor payload |
| `0xE` | `LOOPBACK` | Echo payload for transport validation | Echoed payload |
| `0xF` | `NOOP` | Link and chain liveness test | ASCII `BZ2` |

## Frame Structure

The legacy software and opcode notes use a leading length field followed by the
actual command header and payload.

### `NOOP`

Purpose:

- confirm the ASIC is synchronized with the host
- confirm chain reachability

Behavior:

- transmit a short command to a specific ASIC
- receive three ASCII bytes: `BZ2`

Practical use:

- first link test after setting baud
- chain walk after programming IDs
- low-risk sanity check during debug

### `WRITEJOB`

Purpose:

- deliver engine work over UART

Documented behavior:

- writes `42` consecutive bytes of job state
- typical transmit length is `48` bytes including framing

Payload content includes:

- `32` bytes of midstate
- merkle root residue
- start timestamp
- sequence ID
- job control

Header construction described in the opcode notes:

```text
header = (asic << 24) | (WRITEJOB << 20) | (engine << 8) | 41
```

### `READRESULT`

Purpose:

- poll the ASIC-level result buffer directly in non-TDM mode

Returned fields include:

- status
- engine ID
- sequence ID
- nonce
- timestamp

A clear valid bit is not the same as a framing error. The host is expected to
consume the full response length even when no valid result is present.

### `WRITEREG`

Purpose:

- write a target register range

Documented behavior:

- length is `7 + count`
- payload uses a byte count field encoded as `count - 1`

Common uses:

- enabling clocks
- programming nonce bounds and target
- local-register setup for TDM, clocking, or sensors

### `READREG`

Purpose:

- read a target register range

Practical uses:

- register validation during bring-up
- ASIC-local debug
- clock and sensor status inspection

### `MULTICAST_WRITE`

Purpose:

- write one register range to multiple engines identified by a row-group ID

Common uses:

- row-wise initialization
- engine-wide or board-wide debug patterns
- efficient deployment of common register state

### `LOOPBACK`

Purpose:

- validate the link by echoing a chosen payload

Use cases:

- transport validation before full job submission
- characterization of chain reliability at length
- regression tests after changing clocks, cabling, or PHY conditions

### `DTS_VS`

Purpose:

- retrieve thermal and voltage-sensor data

It can be used:

- directly as a query
- indirectly through TDM streaming when enabled

## Enhanced-Mode Job Programming

Enhanced mode is the normal software contract for engine operation.

Required setup before job submission:

- enable TCE clocks via config register `0x01`
- program `StartNonce` at `0x3C`
- program `EndNonce` at `0x40`
- program `Target` at `0x44`
- load the four midstates beginning at `0x10`

### Four-write sequence

| Write | `0x30` MR residue | `0x34` start timestamp | `0x38` sequence ID | `0x39` job control |
| --- | --- | --- | --- | --- |
| `WriteJob_0` | same value | same value | `0b00` | `0x0` |
| `WriteJob_1` | same value | same value | `0b01` | `0x0` |
| `WriteJob_2` | same value | same value | `0b10` | `0x0` |
| `WriteJob_3` | same value | same value | `0b11` | `0x1` or `0x3` |

Key rule:

- only the fourth write should launch execution

If `JobControl` is asserted too early, the job can be launched in an incomplete
state.

## Job Control Semantics

The documented values are:

- `0x0`: clear pending only
- `0x1`: mark pending ready
- `0x2`: clear current and pending, return to idle
- `0x3`: abort current and immediately promote pending

Practical guidance:

- use `0x1` for orderly sequencing
- use `0x3` for preemption when the current search space is stale
- use `0x2` to recover from invalid or hung engine state before resuming work

## Partial, Incomplete, And Invalid Programming

The hardware documentation is explicit that bad sequences are not harmless.

### Dummy jobs

When writing dummy jobs:

- zero the midstate and merkle-residue content for the unused lanes

Reason:

- residual values can otherwise generate nonces that do not belong to the host
  work model

### Disabled TCEs

If some TCEs are disabled:

- software still has to maintain valid sequence structure
- disabled lanes should receive zeroed dummy content
- valid lanes can still carry production work

### Partial write-job hazards

If a write-job is incomplete:

- missing bytes can be consumed from the next write
- unintended nonce generation can occur

Do not treat UART framing problems as soft performance issues. They are
functional correctness problems.

## TDM Overview

TDM lets ASICs stream data back to the host in time slots associated with ASIC
IDs.

The extracted datasheet text describes:

- an ASIC owns TX opportunity when the slot matches its `ASIC_ID`
- the ASIC begins transmission after a programmed TDM delay
- TDM can carry multiple response classes including register responses, results,
  `NOOP`, and thermal / voltage data

The software guide gives a practical example:

- with `128` bit-time slots at `5 MHz`, a TDM frame is approximately `2.5 ms`

That matters because it bounds result and telemetry latency across a long chain.

## Result Aggregation Model

The software guide describes a two-stage buffering model:

- each engine tile has an `8`-deep local result FIFO
- the ASIC notch block has a `16`-deep ASIC-level result FIFO

Host-visible implications:

- local tile FIFOs are scanned about every `300 us`
- results are aggregated before entering the TDM slot stream
- under expected mining conditions, FIFO overflow should be rare
- overflow bits still need to be handled because overflow means lost nonces

## Sensor Streaming And Querying

### TDM sensor behavior

The datasheet describes thermal and voltage telemetry as a TDM payload source.
The voltage / thermal packet is described as the fourth packet class in TDM
operation.

### Direct query behavior

`DTS_VS` can also be used as a direct query opcode when explicit on-demand
sensor retrieval is needed.

This is the model now exposed in the Rust debug CLI and HTTP API in this
repository.

## DTS / VS Payload Layout

The vendor material describes an 8-byte sensor payload. At a practical level,
the host needs to extract:

- thermal tune code
- thermal validity / enable bits
- thermal-trip status
- voltage enable bit
- voltage shutdown / fault status
- voltage raw codes for `ch0`, `ch1`, and `ch2`
- PLL lock state bits when present in the response generation

### Voltage channels

The three channels represent:

- `ch0`: bottom stack voltage
- `ch1`: top stack voltage
- `ch2`: differential between stacks

These raw values should be converted into engineering units before being used
for protection or calibration decisions.

## Sensor Conversion Equations

### Temperature

```text
T = K + Y * (N - 2^11 / 2^R) / 2^12
```

Where:

- `T` = Celsius
- `N` = raw tune code
- `R` = resolution
- `Y = 631.8`
- `K = -293.8`

### Voltage

```text
V = 1000 * (2 / 5) * VREF * (6 * N / 2^14 - 3 / 2^R - 1)
```

Where:

- `V` = mV
- `N` = raw voltage code
- `R` = resolution
- `VREF = 0.7067`

## `NOOP` Timing Caution

The datasheet text includes a specific warning for `NOOP`:

- do not issue back-to-back `NOOP` commands with only one stop bit of spacing
- in non-TDM mode, maintain at least a three-byte gap between consecutive
  `NOOP`s

Treat this as a real transport rule during low-level validation.

## Practical Host Strategy

For production-capable software, a good division of labor is:

- use broadcast or multicast for common initialization
- use unicast for identity assignment and final trim
- use TDM for steady-state results and telemetry
- use direct `READREG`, `READRESULT`, `NOOP`, `LOOPBACK`, and `DTS_VS` for
  debug and validation

That is also how the Rust tooling in this repository is structured.

## Practical Validation With This Repository

Relevant follow-on references in this repository:

- [ASIC Integration Guide](blockscale-asic-integration-guide.md)
- [BZM2 UART Debug Guide](bzm2-uart-debug.md)
- [BZM2 Port Note](bzm2-port.md)

The Rust debug CLI already exposes:

- `NOOP` scans
- loopback scans
- TDM watch mode
- direct `DTS_VS` queries
- result polling
- PLL and DLL diagnostics
- broadcast and multicast helpers
- silicon-validation flows
