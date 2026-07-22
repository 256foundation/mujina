# BZM2 Opcode And JTAG Grounding

## Scope

This note captures only behavior that is grounded in material available to this port:

- the shipped Rust implementation in
  [protocol.rs](../../mujina-miner/src/asic/bzm2/protocol.rs) and
  [uart.rs](../../mujina-miner/src/asic/bzm2/uart.rs), including their
  legacy wire-format tests
- the public Blockscale UART/TDM protocol reference in
  [bzm2-hwref](https://github.com/Blockscale-Solutions/bzm2-hwref/blob/main/references/blockscale-uart-protocol-reference.md)

The behavior below was originally derived from the legacy `bzmd` C source
(`uart.h`, `uart.c`, and `tests/test.c`). That source is not vendored in this
repository, so any claim that traces only to it is explicitly marked below as
unverified against the legacy source.

Anything not evidenced by these sources is intentionally excluded from the Mujina port.

## What The Legacy Source Proved

The legacy `bzmd` source gives a concrete UART wire contract for these opcodes:

- `WRITEJOB`
- `READRESULT`
- `WRITEREG`
- `READREG`
- `MULTICAST_WRITE`
- `DTS_VS`
- `LOOPBACK`
- `NOOP`

Grounded request/response behavior, originally from the legacy `uart.c` and now verified against the shipped encoders and the public protocol reference:

- `WRITEREG`: request is `len(2 LE) + header(4 BE) + count_minus_one + payload`
- `MULTICAST_WRITE`: same framing as `WRITEREG`, but opcode `0x4`
- `READREG`: request is fixed-length `8` byte frame with terminal target byte; direct response is `asic + opcode + payload`
- `READRESULT`: in TDM mode, result frame is `asic + opcode + 8-byte payload`
- `NOOP`: request is a 4-byte frame; response payload is 3 bytes
- `LOOPBACK`: request is `len + header + count_minus_one + payload`; response echoes `asic + opcode + payload`
- `DTS_VS`: in TDM mode, payload is 4 bytes for gen1 and 8 bytes for gen2

Grounded concurrency and parser behavior, originally from the legacy `uart.h`, `uart.c`, and `test.c`:

- TDM parsing is byte-stream oriented and must resynchronize after unknown prefixes
- TDM `READREG` response size is caller-driven and tracked per ASIC
- one outstanding TDM register read per ASIC is the supported model
- one outstanding TDM noop per ASIC was reportedly the legacy model (unverified
  against the legacy source); the Mujina parser treats TDM `NOOP` frames as
  fixed-length and stateless, so the legacy restriction is not load-bearing
- broadcast register writes use `WRITEREG` with ASIC `0xFF`, not a separate broadcast opcode
- broadcast TDM register reads use no distinct opcode; the legacy mechanism of
  layering them on top of `READREG` is unverified against the legacy source,
  and the current controller exposes no broadcast TDM read helper

## What Mujina Now Grounds

Current Mujina BZM2 support in [protocol.rs](../../mujina-miner/src/asic/bzm2/protocol.rs) and [uart.rs](../../mujina-miner/src/asic/bzm2/uart.rs) is now explicitly locked to the legacy-tested UART behavior for:

- `WRITEREG`, `READREG`, `WRITEJOB`, `MULTICAST_WRITE`, `READRESULT`, `NOOP`, `LOOPBACK`, `DTS_VS`
- gen1 and gen2 DTS/VS payload decoding
- partial-frame buffering and resynchronization after unknown byte prefixes
- legacy wire-format invariants for register, noop, and loopback command encoders

## Deliberate Exclusions

Not implemented from the docs side:

- JTAG command transport
- JTAG IR/DR scan helpers
- any opcode semantics that cannot be traced to shipped UART code or tests

PLL debug readout is no longer excluded: UART-register-based PLL and DLL
configuration, lock polling, and status readback are implemented in
`clock.rs` and surfaced through the `thread.rs` diagnostics path.

Reason:

- the shipped Rust implementation and its wire-format tests, plus the public
  protocol reference, prove the UART mining/control path
- the repository-visible sources do not provide enough packet-level JTAG detail to implement anything defensible
