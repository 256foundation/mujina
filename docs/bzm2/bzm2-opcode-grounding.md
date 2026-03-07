# BZM2 Opcode And JTAG Grounding

## Scope

This note captures only behavior that is grounded in material available in this workspace:

- top-level PDFs in `../docs`
- legacy UART implementation in [uart.h](../bzm2_cgminer/feeds/mining_src/bzmd/uart.h) and [uart.c](../bzm2_cgminer/feeds/mining_src/bzmd/uart.c)
- legacy exercised behavior in [test.c](../bzm2_cgminer/feeds/mining_src/bzmd/tests/test.c)

Anything not evidenced there is intentionally excluded from the Mujina port.

## What The PDFs Proved

The top-level PDFs are present:

- the vendor JTAG usage guide
- the vendor opcode explanation guide

From this shell, the PDFs are not extractable into reliable body text. The only recoverable textual evidence was:

- JTAG PDF bookmark text: `PLL0/PLL1 debug signals`
- opcode PDF outline text: `1 Document Revision History`

That is not enough to justify implementing a JTAG protocol layer or inventing undocumented opcode semantics.

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

Grounded request/response behavior from [uart.c](../bzm2_cgminer/feeds/mining_src/bzmd/uart.c):

- `WRITEREG`: request is `len(2 LE) + header(4 BE) + count_minus_one + payload`
- `MULTICAST_WRITE`: same framing as `WRITEREG`, but opcode `0x4`
- `READREG`: request is fixed-length `8` byte frame with terminal target byte; direct response is `asic + opcode + payload`
- `READRESULT`: in TDM mode, result frame is `asic + opcode + 8-byte payload`
- `NOOP`: request is a 4-byte frame; response payload is 3 bytes
- `LOOPBACK`: request is `len + header + count_minus_one + payload`; response echoes `asic + opcode + payload`
- `DTS_VS`: in TDM mode, payload is 4 bytes for gen1 and 8 bytes for gen2

Grounded concurrency and parser behavior from [uart.h](../bzm2_cgminer/feeds/mining_src/bzmd/uart.h), [uart.c](../bzm2_cgminer/feeds/mining_src/bzmd/uart.c), and [test.c](../bzm2_cgminer/feeds/mining_src/bzmd/tests/test.c):

- TDM parsing is byte-stream oriented and must resynchronize after unknown prefixes
- TDM `READREG` response size is caller-driven and tracked per ASIC
- one outstanding TDM register read per ASIC is the supported model
- one outstanding TDM noop per ASIC is the supported model
- broadcast register writes use `WRITEREG` with ASIC `0xFF`, not a separate broadcast opcode
- broadcast TDM register reads are layered on top of `READREG`, not a distinct opcode

## What Mujina Now Grounds

Current Mujina BZM2 support in [protocol.rs](./mujina-miner/src/asic/bzm2/protocol.rs) is now explicitly locked to the legacy-tested UART behavior for:

- `WRITEREG`, `READREG`, `WRITEJOB`, `MULTICAST_WRITE`, `READRESULT`, `NOOP`, `LOOPBACK`, `DTS_VS`
- gen1 and gen2 DTS/VS payload decoding
- partial-frame buffering and resynchronization after unknown byte prefixes
- legacy wire-format invariants for register, noop, and loopback command encoders

## Deliberate Exclusions

Not implemented from the docs side:

- JTAG command transport
- JTAG IR/DR scan helpers
- PLL debug readout sequences
- any opcode semantics that cannot be traced to shipped UART code or tests

Reason:

- the available source in this workspace proves the UART mining/control path
- the available PDF extraction on this machine does not provide enough packet-level JTAG detail to implement anything defensible