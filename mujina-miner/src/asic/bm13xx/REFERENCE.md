# BM13xx Chip Reference

This document describes the BM13xx family of Bitcoin mining ASICs: the
serial protocol, the registers, and the chip behavior behind them.
Manufacturer documentation is not publicly available. What appears here
is the Mujina project's best understanding, derived from other
open-source implementations, captures of production hardware, and
experiments (see [Sources]).

Contents:

- [Overview]
- [Conventions]
- [Searching Nonces and Versions]
    - [The Search Space]
    - [The Parallel Hierarchy]
- [The Serial Chain]
    - [Response Arbitration]
    - [BM1370]
    - [BM1362]
- [Frame Format]
    - [Command Frames]
    - [Response Frames]
    - [CRC]
    - [Byte Order]
- [Command Types]
    - [Set Chip Address]
    - [Chain Inactive]
    - [Read Register]
    - [Write Register]
    - [Mining Job]
- [Response Types]
    - [Read Register Response]
    - [Nonce Response]
- [Register Map]
    - [0x00 - CHIP_ID]
    - [0x08 - PLL_DIVIDER]
    - [0x0C - CHIP_NONCE_OFFSET]
    - [0x10 - HASH_COUNTING_NUMBER]
    - [0x14 - TICKET_MASK]
    - [0x18 - MISC_CONTROL]
    - [0x28 - UART_BAUD]
    - [0x2C - UART_RELAY]
    - [0x3C - CORE_MAILBOX]
    - [0x54 - ANALOG_MUX]
    - [0x58 - IO_DRIVER_STRENGTH]
    - [0x68 - RING_OSC_PAD_DISABLE]
    - [0xA4 - MIDSTATE_CONFIG]
    - [0xA8 - SOFT_RESET_CONTROL]
    - [On-Die Telemetry]
- [Initialization Sequence]
    - [Single-Chip Initialization]
    - [Multi-Chip Initialization]
- [Driver Guidance]
    - [Computing HASH_COUNTING_NUMBER]
    - [The Version Epoch]
    - [Distribution Across Hash Chains]
- [Sources]

## Overview

The BM13xx family (BM1362, BM1366, BM1368, and BM1370) accelerates the
brute-force work of Bitcoin mining. A chip hashes prospective block
headers, searching for one whose hash falls below a target.

The chips use a frame-based serial protocol. The host sends commands and
jobs; the chips answer register reads and report nonces asynchronously.

The chips are designed to mine in parallel, many daisy-chained together.
Commands enter the first chip and pass from chip to chip down the chain;
responses return toward the host the same way. Every chip hears
broadcast frames, so the host sends each job once and the whole chain
hashes it. Long chains group chips into voltage domains. The chips at a
domain boundary relay the serial lines across the gap (see [The
Serial Chain]).

Inside a chip, the hashing hardware is an array of cores, and each core
is a group of sub-cores. Each model has its own core and sub-core counts
(see [The Parallel Hierarchy]). How cores and sub-cores divide the work
is the subject of [Searching Nonces and Versions].

For one job, the chain sweeps the 2^32 values of the header's nonce
field, divided among its chips. With version rolling (AsicBoost)
enabled, each chip also searches rolled versions of the block header, up
to 2^16 variants of the version field on top of the 2^32 nonces.
Together the two dimensions give 2^48 candidate headers per job (see
[The Search Space]).

## Conventions

Bit numbers in this document refer to deserialized values and use the
LSB-0 convention: bit n has value 2^n, so bit 0 is the least significant
bit and bit 31 the most significant of a 32-bit value.

Byte and word numbers refer to serialized data and count transmission
order from zero: byte 0 is sent first (for a register value, that is its
most significant byte).

Bytes written as space-separated pairs (`55 AA 40 05 ...`) are
hexadecimal; elsewhere, hexadecimal values carry the 0x prefix and
unprefixed numbers are decimal.

A few terms carry fixed meanings throughout:

- **chain**: the chips daisy-chained together, driven by one host.
- **domain**: a group of adjacent chips sharing one voltage rail, one
    tier of the board's series stack.
- **core**: one of a chip's parallel hashing units, each searching its
    own share of the nonce space.
- **sub-core**: the part of a core that hashes one rolled version.
- **slice**: a chip's or a core's share of the 32-bit nonce space.
- **sweep**: one pass a core makes over its nonces, counting upward from
    its starting point.
- **window**: the stretch of nonces a sweep covers before the
    HASH_COUNTING_NUMBER deadline expires.
- **batch**: the set of rolled versions a chip hashes in parallel during
    one sweep, one version per sub-core of a core.
- **epoch**: the time a chip takes to roll through the entire
    version space.

[The Serial Chain] explains the machinery behind chains and domains; the
next section explains how the chips divide and search the space of
nonces and versions.

## Searching Nonces and Versions

Every chip in a chain hashes the same job, so each chip must sweep a
different part of that job's search space. Two chips sweeping the same
nonces would do that work twice and cover no more of the space. The host
divides the space when it first configures the chips, not when it sends
a job. A chip takes its slice from its address, or from an offset the
host writes into a register. The host then broadcasts each job to the
whole chain, and every chip works on it.

### The Search Space

For one job, a version-rolling BM13xx chip searches two dimensions:

- the 32-bit nonce field, 2^32 candidates, and
- the 16 rollable version bits (AsicBoost, [BIP320]), 2^16 variants,
    generated inside the chip once version rolling is enabled via
    MIDSTATE_CONFIG (0xA4). The upstream grants the rollable mask
    (stratum v1 negotiates it; all 16 bits is typical), and the host
    writes the grant into the chip, perhaps narrowed if it divides the
    version space among several chains (see [Distribution Across Hash
    Chains]). This section assumes the full 16.

Together that is 2^48 candidate headers per job. The chips need the
second dimension. A BM1370 at 600 MHz hashes about 1.2 TH/s and would
exhaust the bare nonce space in under 4 ms. Version rolling stretches
that to about four minutes for the full 2^48. A chain splits the nonce
dimension among its chips, so each chip's share is 2^48 divided by the
chip count, and at the same rate each of 65 chips finishes its share in
about four seconds. The host can extend the space further by rolling
ntime or the extranonce, but that happens outside the chip; this section
covers only what the chip does on its own.

A chip divides its share again among its own cores and sub-cores.

### The Parallel Hierarchy

A hashboard is a chain of chips, each chip an array of cores, each core
a group of sub-cores. The chain divides the nonce space among its chips,
and each chip divides its slice of the nonce space again, inside itself:

- **Chips sweep disjoint slices of the nonce space.** Each chip starts
    its sweep at a different point in the nonce space. The BM1362 and
    BM1366 derive that point from the chip's address. BM1370 chains take
    the point from CHIP_NONCE_OFFSET (0x0C). On every model,
    HASH_COUNTING_NUMBER sets how far the sweep runs from that point.

- **Cores split their chip's slice of the nonce space.** A fixed field
    of every nonce holds the number of the core that produced it, so
    each core sweeps only the nonces carrying its own number. The
    field's position differs by model.

- **A core's sub-cores hash the same nonces, each with its own
    version.** With version rolling enabled, the chip hands every
    sub-core its own rolled version as a precomputed midstate. The
    sub-cores of a core hash the same nonces, so one sweep tests as many
    versions as the core has sub-cores. The BM1370, with 16 sub-cores
    per core, covers all 2^16 versions in 4,096 sweeps. The BM1362 and
    BM1366, with 8 sub-cores per core, take 8,192 sweeps.[^pr420]

Version rolling needs no coordination among chips. Two chips hashing the
same version are sweeping different nonce ranges.

Every nonce names the chip and the core that found it. The BM1362 and
BM1366 stamp both into fixed nonce fields.[^bm1362nonce] The BM1370
stamps only the finding core.[^bm1370bits] The chip is implied by the
slice the nonce's value falls in. [Nonce Response] holds the per-model
layouts and worked examples.

The core and sub-core counts are per-model constants the host must know
on its own. A chip reports its model in the chip-ID register, not its
geometry, so firmware maps model to counts in
hard-coded tables.[^bm1362topo]

| Model  | Cores | Sub-cores | Separated by      | Machines              |
|--------|-------|-----------|-------------------|-----------------------|
| BM1362 | 65    | 8         | address           | S19j Pro, EmberOne00  |
| BM1366 | 112   | 8         | address           | S19 XP, S19k Pro      |
| BM1368 |       | 16        |                   | S21                   |
| BM1370 | 128   | 16        | CHIP_NONCE_OFFSET | Bitaxe Gamma, S21 Pro |

## The Serial Chain

The chain's serial signals pass from chip to chip rather than along a
bus the chips share. Commands and the clock flow away from the host.
Responses flow back, arbitrated by a busy signal.

The whole chain runs the serial link at one baud rate. A chip comes out
of reset at 115,740 baud. The host raises the rate once during bring-up,
a broadcast write, then switches its own UART to match; the captured
chains mine at 3.125 Mbaud, the Bitaxe at about 1 Mbaud (see [0x28 -
UART_BAUD] and [Initialization Sequence]).

A hashboard divides its chips into voltage domains. The chips of one
domain sit in parallel across a shared rail, and the domains stack in
series across the board supply, so each domain drops a fraction of the
supply and floats at its own DC potential. The same current flows
through every domain in the stack and divides among the chips inside a
domain. Chips in one domain share a ground reference. A signal that
crosses a domain boundary has no such shared reference. The two chips'
grounds differ by one domain's voltage drop, so the signal arrives
offset by that much relative to the receiving chip's rails.

Inside a domain the chip-to-chip links stay within one ground reference.
Two chips of each domain drive a signal across a boundary. The last chip
of a domain drives the command line and the clock into the first chip of
the next domain. The first chip of a domain drives the response line
back to the last chip of the domain before it.

Two registers configure the signals between chips. UART_RELAY (0x2C)
carries a relay enable for each serial direction and a gap count that
sets a wait. IO_DRIVER_STRENGTH (0x58) sets the drive strength of each
output pin, including the clock output.

### Response Arbitration

Every chip in a chain sends its responses over one shared path back to
the host, and a busy signal arbitrates that path. Each chip has a busy
input and a busy output, chained from chip to chip away from the host. A
transmitting chip asserts busy toward the far end of the chain, and a
chip transmits only while its busy input is inactive. No chip beyond a
transmitting chip can start a frame.[^busypins] A frame already started
by a farther chip still arrives, because a chip waits for the gap count
of UART_RELAY (0x2C) before treating the response line as free. The next
section states this interpretation.

### BM1370

The S21 Pro's 65 BM1370s divide into 13 domains of five chips. Factory
firmware assigns chip addresses 0x00 to 0x80 at an interval of 2, so a
domain contains five consecutive addresses, 0x00 through 0x08, 0x0A
through 0x12, and so on to 0x78 through 0x80.

Stock firmware writes to the IO_DRIVER_STRENGTH (0x58) of the last chip
of each domain, raising its clock output:

```text
 31              20 19     16 15     12 11      8 7       4 3       0
+------------------+---------+---------+---------+---------+---------+
| zero in capture  |  RO_DS  | CLKO_DS |NRSTO_DS |  BO_DS  |  CO_DS  |
+------------------+---------+---------+---------+---------+---------+
|        0         |    1    |    F    |    1    |    1    |    1    |
+------------------+---------+---------+---------+---------+---------+
```

- **CLKO_DS** = 0xF: the clock output at maximum drive, up from the
    strength 1 written to every chip at bring-up
- **RO_DS**, **NRSTO_DS**, **BO_DS**, **CO_DS** = 1: unchanged

The clock is the only signal whose drive strength is increased. The
command output of the last chip of a domain and the response output of
the first chip cross domains at strength 1.

Stock firmware writes UART_RELAY (0x2C) to the first and last chip of
each domain:

```text
 31              16 15                    2 1     0
+------------------+-----------------------+-----+-----+
|     gap count    | zero in every capture | rsp | cmd |
+------------------+-----------------------+-----+-----+
|    0x13-0x4F     |           0           |  1  |  1  |
+------------------+-----------------------+-----+-----+
```

- **rsp**, **cmd** = 1: both serial lines relayed, on the domain's first
    chip and its last
- **gap count**: one value per domain, written to both chips

The gap count falls from 0x4F at the domain nearest the host to 0x13 at
the far end of the chain. The count drops by one for each chip of
distance from the host, five per domain on this chain. The factory test
tool computes the count from the chain length, the chip's index in the
chain, and the domain size, plus a constant that differs between builds.
The tool skips the relay altogether on a board of nine domains or fewer.

The gap count most likely sets a wait on the response line, not the
command line. Commands come from one source, the host, which spaces them
as it chooses. A chip has no reason to insert idle between the commands
it forwards. Responses come from every chip in the chain and share one
path back to the host. The busy signal arbitrates that path, holding off
every chip beyond the one transmitting (see
[Response Arbitration]).[^gapscale]

### BM1362

The S19j Pro's 126 BM1362s divide into 42 domains of three chips. Stock
firmware configures neither boundary register. In the capture it writes
IO_DRIVER_STRENGTH once, a bring-up broadcast that leaves every output
at strength 1, and it never writes UART_RELAY. The chain mines at 3.125
Mbaud with no boundary configuration.

## Frame Format

Every frame opens with a 2-byte preamble and ends with a CRC. The fields
between differ by direction. A command frame carries explicit type and
length bytes; a response frame packs its type into the final CRC byte.

### Command Frames (Host -> Chip)

| Field      | Size         | Description                                  |
|------------|--------------|----------------------------------------------|
| Preamble   | 2 bytes      | 55 AA                                        |
| Type/Flags | 1 byte       | encodes type, broadcast flag, and command    |
| Length     | 1 byte       | byte count after preamble (see [Mining Job]) |
| Payload    | varies       | command-specific data                        |
| CRC        | 1 or 2 bytes | CRC5 for commands, CRC16 for jobs            |

### Response Frames (Chip -> Host)

| Field    | Size    | Description                                 |
|----------|---------|---------------------------------------------|
| Preamble | 2 bytes | AA 55, reversed from commands               |
| Payload  | varies  | response-specific data                      |
| CRC      | 1 byte  | response type in bits 7-5, CRC5 in bits 4-0 |

Responses carry no length field; each response format has a fixed size
(see [Response Types]).

### CRC

A frame's CRC covers everything after the preamble, up to the CRC
itself. In responses, that includes the three type bits that share the
final byte with the checksum.

- CRC5 (commands and responses): polynomial 0x05, initial value 0x1F, no
    output XOR, no reflection. Commands send it as the final byte;
    responses pack it into bits 4-0 of the final byte.
- CRC16 (jobs): polynomial 0x1021, initial value 0xFFFF, no output XOR,
    no reflection (CRC-16-CCITT-FALSE). Sent most significant
    byte first.

### Byte Order

Byte order on the wire follows three rules:

- The chip's own multi-byte values are sent most significant byte first:
    register values in both directions, CRC16 checksums, and a nonce
    response's version field.
- Block header material is sent in the header's own little-endian
    serialization: the four header fields sent in a job frame (version,
    nbits, ntime, starting nonce), and the nonce in a response.
- The job's two hashes are an exception to the header rule: each is sent
    with its 4-byte words in reversed order (see [Mining Job]).

The nonce also has a second interpretation, most significant byte first,
that exposes the fields the chip packs into it. Those fields name the
core, and on some models the chip, that found the nonce (see [Searching
Nonces and Versions]).

## Command Types

The Type/Flags byte (the third byte of a command frame) encodes
these fields:

| Bits | Field      | Meaning                        |
|------|------------|--------------------------------|
| 7    | unobserved | always 0 in captures           |
| 6-5  | TYPE       | 2 = register ops, 1 = job      |
| 4    | BROADCAST  | 0 = single chip, 1 = all chips |
| 3-0  | CMD        | command value                  |

This document calls the bit `broadcast`; reference implementations use
other names (such as `all`) for the same protocol bit.

Common Type/Flags values:

| Value  | TYPE | BROADCAST | CMD | Command                                  |
|--------|------|-----------|-----|------------------------------------------|
|  0x40  | 2    | 0         | 0   | set chip address                         |
|  0x41  | 2    | 0         | 1   | write register                           |
|  0x42  | 2    | 0         | 2   | read register                            |
|  0x51  | 2    | 1         | 1   | write register                           |
|  0x52  | 2    | 1         | 2   | read register (chip discovery)           |
|  0x53  | 2    | 1         | 3   | chain inactive (prepare for addressing)  |
|  0x21  | 1    | 0         | 1   | send job                                 |

### Set Chip Address (CMD=0)

Assigns an address to a chip in the serial chain via
daisy-chain forwarding.

Request format and example:

| Preamble | Type/Flags | Length | New_Addr | Reserved | CRC5 |
|----------|------------|--------|----------|----------|------|
| 55 AA    | 40         | 05     | 04       | 00       | 15   |

The example row assigns address 0x04.

- **Type/Flags** (1 byte): 0x40. The broadcast bit is clear, so delivery
    relies on the forwarding mechanism below.
- **Length** (1 byte): Always 0x05 (5 bytes excluding preamble)
- **New_Addr** (1 byte): The address to assign (typically increments by
    2: 0x00, 0x02, 0x04...)
- **Reserved** (1 byte): Always 0x00 (no semantic meaning,
    possibly padding)

How daisy-chain addressing works:

After the ChainInactive command (CMD=3), every chip enters an addressing
mode and forwards downstream any command it does not answer itself:

1. The host broadcasts ChainInactive, and all chips enter
    addressing mode.
2. The host sends SetChipAddress with the first address.
3. The first unaddressed chip in the chain intercepts the command and
    adopts that address.
4. The now-addressed chip passes subsequent SetChipAddress
    commands downstream.
5. The next unaddressed chip receives the command and adopts
    its address.
6. The process repeats until all chips are addressed.

The mechanism lets the host address chips sequentially without knowing
the chain length beforehand. SetChipAddress clears the broadcast bit
because only one chip at a time should process it. The command targets
no existing address, so the first unaddressed chip intercepts it
through forwarding.

### Chain Inactive (CMD=3)

Puts all chips into addressing mode, enabling the daisy-chain forwarding
mechanism used by SetChipAddress.

Request format and example:

| Preamble | Type/Flags | Length | Reserved | Reserved | CRC5 |
|----------|------------|--------|----------|----------|------|
| 55 AA    | 53         | 05     | 00       | 00       | 03   |

- **Type/Flags** (1 byte): 0x53 (broadcast to all chips)
- **Length** (1 byte): Always 0x05 (5 bytes excluding preamble)
- **Reserved** (2 bytes): Always `00 00`

The host broadcasts ChainInactive once before address assignment. [Set
Chip Address] above describes the forwarding mechanism the addressing
mode enables.

### Read Register (CMD=2)

Reads a 4-byte value from a register.

Request format and example:

| Preamble | Type/Flags | Length | Chip_Addr | Reg_Addr | CRC5 |
|----------|------------|--------|-----------|----------|------|
| 55 AA    | 52         | 05     | 00        | 00       | 0A   |

The example row broadcasts a read of register 0x00, the chip
discovery probe.

- **Type/Flags** (1 byte): 0x42 for a specific chip, 0x52 for broadcast
    (for example, chip discovery)
- **Length** (1 byte): Always 0x05 (5 bytes excluding preamble)

### Write Register (CMD=1)

Writes a 4-byte value to a register.

Request format and example:

| Preamble | Type/Flags | Length | Chip_Addr | Reg_Addr | Data[4]     | CRC5 |
|----------|------------|--------|-----------|----------|-------------|------|
| 55 AA    | 51         | 09     | 00        | A4       | 90 00 FF FF | 1C   |

The example row broadcasts a write of 0x9000FFFF to register 0xA4.

- **Type/Flags** (1 byte): 0x41 for specific chip, 0x51 for broadcast
- **Length** (1 byte): Always 0x09 (9 bytes excluding preamble)
- **Data** (4 bytes): Register value, sent most significant byte first

### Mining Job (TYPE=1, CMD=1)

BM13xx chips take one of two job formats; which one depends on
the model:

| Format   | Chips                          | Midstates                  |
|----------|--------------------------------|----------------------------|
| Full     | BM1362, BM1366, BM1368, BM1370 | calculated by the chip     |
| Midstate | BM1397 and older generations   | pre-calculated by the host |

#### Full Format

The chip calculates SHA256 midstates itself from the job's
header fields.

Request format:

| Preamble | Type/Flags | Length | Job_Data   | CRC16     |
|----------|------------|--------|------------|-----------|
| 55 AA    | 21         | 36     | (82 bytes) | (2 bytes) |

- **Preamble** (2 bytes): `55 AA`
- **Type/Flags** (1 byte): 0x21 = TYPE=1 (job), BROADCAST=0, CMD=1
- **Length** (1 byte): 0x36 (54). The value is not the frame's
    byte count.[^joblength]
- **Job_Data** (82 bytes): mining work (see below)
- **CRC16** (2 bytes): calculated over type/flags + length + job_data,
    sent most significant byte first

Job_Data structure (82 bytes), fields in transmission order:

| Field           | Size     |
|-----------------|----------|
| job_header      | 1 byte   |
| num_midstates   | 1 byte   |
| starting_nonce  | 4 bytes  |
| nbits           | 4 bytes  |
| ntime           | 4 bytes  |
| merkle_root     | 32 bytes |
| prev_block_hash | 32 bytes |
| version         | 4 bytes  |

- **job_header** (1 byte): carries the job id
    - Bits 7-3: job_id field. The BM1370 references keep bit 7 zero and
        use 4-bit ids (0-15) in bits 6-3. In the S19j Pro capture,
        BM1362 factory firmware sends ids past 15, using the full
        five bits.
    - Bits 2-0: unused by the chip; write zero
- **num_midstates** (1 byte): Number of midstates (always 0x01).
    MIDSTATE_CONFIG (0xA4) controls version rolling, not this field, and
    in the full format the field may be vestigial.[^nummidstates]
- **starting_nonce** (4 bytes): Starting nonce value (always
    0x00000000, little-endian)[^startnonce]
- **nbits** (4 bytes): Encoded difficulty target (little-endian)
    - Example: 0x170E3AB4 -> transmitted as `B4 3A 0E 17`
- **ntime** (4 bytes): Block timestamp (little-endian)
    - Unix timestamp
- **merkle_root** (32 bytes): Root of the transaction merkle tree. The
    job's two hashes are the exception to the header-order rule in
    [Conventions]. The chip takes each hash with its eight 4-byte words
    in reversed order.
    - Convert from the header's byte order by splitting the 32 bytes
        into eight 4-byte words and reversing the word order (word 0
        with 7, 1 with 6, 2 with 5, 3 with 4); bytes within each word
        keep their places
- **prev_block_hash** (32 bytes): Hash of the previous block, sent in
    the same reversed-word order as merkle_root
- **version** (4 bytes): Block version (little-endian)
    - Example: 0x20000000 -> transmitted as `00 00 00 20`
    - The chip rolls the bits the version mask enables
        (see MIDSTATE_CONFIG).

Example job frame:

| Bytes       | Field                                   |
|-------------|-----------------------------------------|
| 55 AA 21 36 | preamble, Type/Flags, Length            |
| 18          | job_header: bits 6-3 = 0b0011, job_id 3 |
| 01          | num_midstates = 1                       |
| 00 00 00 00 | starting_nonce = 0x00000000             |
| B4 3A 0E 17 | nbits = 0x170E3AB4                      |
| 5C 8B 67 67 | ntime = 0x67678B5C                      |
| (32 bytes)  | merkle_root                             |
| (32 bytes)  | prev_block_hash                         |
| 00 00 00 20 | version = 0x20000000                    |
| XX YY       | CRC16 = 0xXXYY                          |

Total: 88 bytes (2 preamble + 1 type + 1 length + 82 job_data + 2 CRC16)

#### Midstate Format

Some BM13xx chips require the host to pre-calculate SHA256 midstates for
version rolling. The BM1397 is one, carried by the Antminer S17 and T17
families and the original Bitaxe (the Max model).[^bm1397] In
this format:

- The host calculates a midstate for each rolled version.
- The job frame carries 1-4 pre-calculated midstates, 32 bytes each.
- Frame size varies with the number of midstates.

## Response Types

In the captures the chips send two response types: TYPE=0, the
read-register response, and TYPE=4, the nonce response. Other type
values are unobserved.

Every chip in a chain sends its responses over one shared path back to
the host. [Response Arbitration], under [The Serial Chain], describes
the busy signal that arbitrates the path.

### Read Register Response (TYPE=0)

Format and example (11 bytes total):

| Preamble | Register_Value | Chip_Addr | Reg_Addr | Unknown | CRC5+Type |
|----------|----------------|-----------|----------|---------|-----------|
| AA 55    | 13 70 00 00    | 00        | 00       | 00 00   | 10        |

- **Register_Value** (4 bytes): Value read from the register, sent most
    significant byte first
- **Chip_Addr** (1 byte): Address of the responding chip
- **Reg_Addr** (1 byte): Address of the register that was read
- **Unknown** (2 bytes): Purpose unknown
- **CRC5+Type** (1 byte): Response type (0) in bits 7-5 and CRC5 in
    bits 4-0

The example row answers the discovery probe (`55 AA 52 05 00 00 0A`, a
broadcast read of register 0x00). Its register value opens with the
BM1370's chip ID `13 70`, and the chip responds from address 0x00.

A chip fresh out of reset answers in a shorter 9-byte format instead,
dropping the two unknown bytes. The version-rolling enable in
MIDSTATE_CONFIG (0xA4) switches the chip to the
11-byte format.[^resp9byte]

### Nonce Response (TYPE=4)

Format (11 bytes total, field sizes in bytes):

| Preamble | Nonce | Excess_Difficulty | Result_Header | Version | CRC5+Type |
|----------|-------|-------------------|---------------|---------|-----------|
| 2        | 4     | 1                 | 1             | 2       | 1         |

The BM1362, BM1366, BM1368, and BM1370 all send this 11-byte response,
including the 2-byte version field. The BM1397 sends a 9-byte response
without the version field.[^bm1397nonce]

The Result_Header byte's high bits carry a job id, repeating the id the
host sent in the job frame. The job id matches each returned nonce to
the job that produced it. The match is necessary because more than one
job is typically in flight. The host sends jobs continually: at least
once per version epoch, so the chips never re-hash old headers (see [The
Version Epoch]), sooner when the transaction set updates, and
immediately when the network finds a block. Chips return nonces at
unpredictable times, a job frame takes time to transmit, and cores may
still be finishing an old job when a new one arrives.

The match matters most when the network finds a new block. Work for the
old block becomes worthless immediately, and the job id lets the host
discard nonces from stale jobs while accepting those from the current
one. For example:

```text
Time 0ms:    Send Job with job_id=0 (mining block height 850,000)
Time 50ms:   Send Job with job_id=1 (same block, updated transactions)
Time 90ms:   NEW BLOCK! Send Job with job_id=2 (mining block height 850,001)
Time 95ms:   Receive nonce with job_id=0 -> Discard (old block)
Time 100ms:  Receive nonce with job_id=2 -> Valid for current block
```

The job command and the response carry the id at different bit
positions, and the returned position differs by model (see each model's
Result_Header below).

#### BM1370

- **Nonce** (4 bytes): the winning nonce, sent in the block header's
    little-endian serialization; its bytes go into the rebuilt header as
    received. When read most significant byte first into a 32-bit value
    instead, the value divides into these fields:

  ```text
   31       25 24                      9 8         0
  +-----------+-------------------------+-----------+
  |  core ID  |         counter         |  counter  |
  |           |   (starts at offset)    | (starts 0)|
  +-----------+-------------------------+-----------+
  ```

    - Bits 31-25: the ID of the core that found the nonce (7 bits), by
        the references' convention.

    - Bits 24-9: the counter's high 16 bits. Thay are initialized by the
        16-bit offset the host writes into CHIP_NONCE_OFFSET (0x0C).

    - Bits 8-0: the counter's low 9 bits, below the offset. Each sweep
        starts them at zero; they hold
        nothing identifying.[^bm1370nonce]

    The counter holds no fixed identifying field. The offset is only a
    starting value; each core counts upward from it, and the counter
    wraps past the top of its 25-bit range to zero, not back to the
    sweep's own offset. Attribution should therefore work by range
    membership, naming the chip whose offset range contains the nonce
    (see [Searching Nonces and Versions]). The BM1362 and BM1366 instead
    stamp the chip's assigned address into fixed nonce bits (see
    [BM1362][bm1362-nonce] below).

- **Excess_Difficulty** (1 byte; historically
    "Midstate_Num"[^midstatenum]): the hash's difficulty above the
    reporting threshold (TICKET_MASK, 0x14), in half-bit units on the
    log2 scale. A byte of N means the hash has N/2 more leading zero
    bits than the ticket mask requires.

    - Worked example, from a Bitaxe capture share: ticket mask
        difficulty 256 requires 40 leading zero bits, and the share's
        hash begins with 46 (difficulty 29588). Six whole bits over the
        threshold gives byte 2 x 6 = 12, the value in the
        captured response.

    - The byte brackets the hash's difficulty at threshold x 2^(byte/2)
        to within a factor of two, so a host can discard responses that
        cannot reach the share target without reconstructing the header.
        Only responses within one bracket of the target, and anything
        actually submitted, need the hash computed.

    - The low half bit depends on configuration, so the bracket is
        dependable only to whole bits. The S21 Pro's configuration takes
        the half bit from the bit after the hash's leading one; a
        measured BM1370 run under a different configuration always
        reported zero. Which configuration difference switches it on is
        not known.[^bm1370exdiff]

- **Result_Header** (1 byte):

  ```text
   7          4 3          0
  +------------+------------+
  |   job_id   | subcore_id |
  +------------+------------+
  ```

    - Bits 7-4: 4-bit job_id (0-15), repeating the job_id from the job
        frame that produced this nonce so the host can match the
        response to its job. The job frame carries the id at a different
        position (see [Mining Job]).
    - Bits 3-0: 4-bit subcore_id (0-15), naming the sub-core that found
        the nonce. Each batch hands every sub-core its own version slot,
        so the subcore_id normally equals the version-roll counter's low
        4 bits.[^bm1370subcore]

- **Version** (2 bytes): the rolled version bits, transmitted most
    significant byte first. In the rebuilt block header's version field,
    these 16 bits occupy bits 28-13; the header's other version bits
    come from the job.

    Read directly, the value is the chip's roll-progress counter. It
    restarts at zero on each new job and advances once per version
    consumed. The chip consumes versions in batches, one version per
    sub-core of a core, all hashed during one sweep (see [Searching
    Nonces and Versions]):

  ```text
   15              4 3            0
  +-----------------+--------------+
  |  batch counter  | version slot |
  +-----------------+--------------+
  ```

    - Bits 15-4: the batch counter, advancing once per batch of versions
    - Bits 3-0: the version slot within the current batch, normally the
        finding sub-core's number (the subcore_id)

Example BM1370 response: `AA 55 18 00 A6 40 02 99 22 F9 91`

- **Preamble:** `AA 55`, fixed for every response

- **Nonce:** bytes `18 00 A6 40` deserialize little-endian, unlike the
    frame's other fields, into the header nonce 0x40A60018. The same
    bytes, read most significant byte first, give 0x1800A640, which
    splits as:

  ```text
   31       25 24                      9 8         0
  +-----------+-------------------------+-----------+
  |  core ID  |         counter         |  counter  |
  |   0x0C    |         0x0053          |   0x040   |
  +-----------+-------------------------+-----------+
  ```

- **Excess_Difficulty:** 0x02, one whole bit of difficulty over the
    reporting threshold

- **Result_Header:** 0x99 splits as:

  ```text
   7          4 3          0
  +------------+------------+
  |   job_id   | subcore_id |
  |    0x9     |    0x9     |
  +------------+------------+
  ```

- **Version:** bytes `22 F9` give the roll counter 0x22F9, which
    splits as:

  ```text
   15              4 3            0
  +-----------------+--------------+
  |  batch counter  | version slot |
  |      0x22F      |     0x9      |
  +-----------------+--------------+
  ```

    Shifted into the block header's 32-bit version field, the rolled
    bits contribute 0x045F2000:

  ```text
   31      29 28               13 12         0
  +----------+-------------------+-----------+
  | from job |    rolled bits    | from job  |
  |          |      0x22F9       |           |
  +----------+-------------------+-----------+
  ```

    Note this gets serialized into the block header in little-endian
    order, `00 20 5F 04`, with the caveat that the real version
    typically has additional bits from the job OR'd in.

- **CRC5+Type:** 0x91 splits as:

  ```text
   7    5 4        0
  +------+----------+
  | type |   CRC5   |
  | 0x4  |   0x11   |
  +------+----------+
  ```

    Type 4 is a nonce response.

#### BM1362

The BM1362 sends the same 11-byte response format.

- **Nonce** (4 bytes): the winning nonce, in the block header's
    little-endian serialization as on the BM1370. Read most significant
    byte first, the value divides into these fields:

  ```text
   31       25 24        18 17                     0
  +-----------+------------+------------------------+
  |  core ID  | chip addr  |     sweep counter      |
  |  (7 bits) |  bits 7-1  |       (18 bits)        |
  +-----------+------------+------------------------+
  ```

    The address field holds the assigned chip address's bits 7-1;
    addresses step by two, so bit 0 is always zero and the seven bits
    recover the whole address.[^bm1362nonce] The core ID runs 0 through
    64 on the BM1362 and 0 through 111 on the BM1366, and the 18-bit
    counter sweeps 2^18 candidates per version batch. The BM1370
    differs: its nonce fixes no chip field, and its counter spans the
    low 25 bits.

- **Excess_Difficulty** (1 byte): the same difficulty report as
    the BM1370's.[^bm1362exdiff]

- **Result_Header** (1 byte):

  ```text
   7             3 2        0
  +---------------+----------+
  |    job_id     |subcore_id|
  +---------------+----------+
  ```

    - Bits 7-3: 5-bit job_id, a 5+3 split against the
        BM1370's 4+4.[^bm1362split]
    - Bits 2-0: 3-bit subcore_id. The subcore_id, not the version
        counter's low bits, names the finding sub-core; the two differ
        by exactly 4 in 8% of captured responses, for
        reasons unknown.[^bm1362subcore]

- **Version** (2 bytes): as on the BM1370, except the version slot is
    the roll counter's low 3 bits, matching 8 sub-cores per core, where
    the BM1370's slot is 4 bits.

Example response: `AA 55 6D B8 8E E1 01 04 03 54 94`

- **Preamble:** `AA 55`, fixed for every response

- **Nonce:** bytes `6D B8 8E E1` deserialize little-endian into the
    header nonce 0xE18EB86D. The same bytes, read most significant byte
    first, give 0x6DB88EE1, which splits as:

  ```text
   31       25 24        18 17                     0
  +-----------+------------+------------------------+
  |  core ID  |  addr 7-1  |     sweep counter      |
  |   0x36    |    0x6E    |        0x08EE1         |
  +-----------+------------+------------------------+
  ```

    Core 54. The address field 0x6E holds address bits 7-1, so the
    chip's address is 0xDC, chip 110 of the S19j Pro's 126.

- **Excess_Difficulty:** 0x01, half a bit of difficulty over the
    reporting threshold

- **Result_Header:** 0x04 splits as:

  ```text
   7             3 2        0
  +---------------+----------+
  |    job_id     |subcore_id|
  |      0x0      |   0x4    |
  +---------------+----------+
  ```

- **Version:** bytes `03 54` give the roll counter 0x0354, which
    splits as:

  ```text
   15              3 2            0
  +-----------------+--------------+
  |  batch counter  | version slot |
  |      0x6A       |     0x4      |
  +-----------------+--------------+
  ```

    The version slot matches the subcore_id. Shifted into the block
    header's 32-bit version field, the rolled bits
    contribute 0x006A8000:

  ```text
   31      29 28               13 12         0
  +----------+-------------------+-----------+
  | from job |    rolled bits    | from job  |
  |          |      0x0354       |           |
  +----------+-------------------+-----------+
  ```

- **CRC5+Type:** 0x94 splits as:

  ```text
   7    5 4        0
  +------+----------+
  | type |   CRC5   |
  | 0x4  |   0x14   |
  +------+----------+
  ```

    Type 4 is a nonce response.

## Register Map

Every register holds one 32-bit value, written and read most significant
byte first on the wire. Bit positions below are positions in that value.
The registers this document covers:

| Register | Name                 | Description                                       |
|----------|----------------------|---------------------------------------------------|
| 0x00     | CHIP_ID              | Chip model identifier and assigned address        |
| 0x08     | PLL_DIVIDER          | Hash clock frequency control (PLL dividers)       |
| 0x0C     | CHIP_NONCE_OFFSET    | Explicit per-chip sweep start                     |
| 0x10     | HASH_COUNTING_NUMBER | Nonce sweep length (deadline in crystal ticks)    |
| 0x14     | TICKET_MASK          | Difficulty threshold for nonce reporting          |
| 0x18     | MISC_CONTROL         | Nonce reporting enables (open core)               |
| 0x28     | UART_BAUD            | UART baud rate selection                          |
| 0x2C     | UART_RELAY           | Serial line relay across domain boundaries        |
| 0x3C     | CORE_MAILBOX         | Indirect access to per-core registers             |
| 0x54     | ANALOG_MUX           | Analog mux output signal select                   |
| 0x58     | IO_DRIVER_STRENGTH   | Output pin drive strength                         |
| 0x68     | RING_OSC_PAD_DISABLE | Ring-oscillator pad disable (BM1368/BM1370)       |
| 0xA4     | MIDSTATE_CONFIG      | Midstate generation and version rolling           |
| 0xA8     | SOFT_RESET_CONTROL   | Chip-internal soft resets                         |
| 0xB0     | TEMP_SENSOR_CTRL     | On-die temperature sensor control (BM1368)        |
| 0xB4     | TEMP_SENSOR_RESULT   | On-die temperature sensor result (BM1368)         |
| 0xB8     | ADC_CONFIG           | On-die ADC input select and start (BM1368/BM1370) |
| 0xB9     | ADC_CTRL1            | On-die ADC control (BM1368/BM1370)                |
| 0xBA     | -                    | On-die ADC control word (BM1368/BM1370)           |
| 0xBB     | -                    | On-die ADC control word (BM1368/BM1370)           |
| 0xBD     | ADC_RESULT           | On-die ADC result (BM1368/BM1370)                 |

### 0x00 - CHIP_ID

Chip identification, read broadcast during discovery. Value format and
example, in wire byte order:

| Chip_Type | Unknown | Chip_Addr |
|-----------|---------|-----------|
| 13 70     | 00      | 00        |

- **Chip_Type** (2 bytes): the model's identifier, `13 70` on the BM1370
    and `13 62` on the BM1362. Treat it as a byte sequence rather than
    an integer, to avoid endianness confusion.
- **Unknown** (1 byte): reads 0x00 on the BM1370 and 0x03 on
    the BM1362[^corenum]
- **Chip_Addr** (1 byte): the chip's address, assigned
    during initialization

### 0x08 - PLL_DIVIDER (Frequency Control)

Controls the hash frequency: fb_div x 25 MHz / (ref_div x post_div1
x post_div2).

```text
 31 30 29 28 27          16 15  14 13      8 7   4 3   0
+--+--+--+--+--------------+------+---------+-----+-----+
|LK|EN|BY|VS|    FB_DIV    |  0   | REF_DIV | PD1 | PD2 |
+--+--+--+--+--------------+------+---------+-----+-----+
```

- Bit 31 (LOCKED, LK): PLL lock report. Written 0; reads back 1 once
    the PLL locks.[^pllfields]
- Bit 30 (PLLEN, EN): PLL enable. 1 in every captured write.
- Bit 29 (BYPASS, BY): PLL bypass. 0 in every captured write.
- Bit 28 (VCOSEL, VS): 2400 MHz VCO select. 0 below 2.4 GHz, 1 at or
    above.[^vcosel]
- Bits 27-16: FB_DIV, a 12-bit feedback divider (top four bits zero in
    practice, fb_div staying under 256).
- Bits 15-14: reserved, zero in every captured write.
- Bits 13-8: REF_DIV, a 6-bit reference divider (2 in every
    captured write).
- Bits 7-4 and 3-0: the two post dividers, each stored minus one (three
    bits used).

The host sets and adjusts the hash frequency by rewriting this register.
Solving for dividers means searching ref_div and the post-divider pairs
(post_div1 >= post_div2), rounding fb_div toward the target, and
rejecting any combination whose fb_div or VCO frequency (fb_div x 25 MHz
/ ref_div) leaves the model's operating range. The per-model fb_div and
VCO ranges:[^pllranges]

| Model     | fb_div    | VCO (MHz) |
|-----------|-----------|-----------|
| BM1362    | 0x10-0xFA | 2000-3200 |
| BM1366/68 | 0x90-0xEB | unknown   |
| BM1370    | 0xA0-0xEF | 1600-3200 |

### 0x0C - CHIP_NONCE_OFFSET

Sets a chip's slice of the nonce space explicitly:

```text
 31       30                   16 15              0
+--------+-----------------------+-----------------+
| enable | zero in every capture |      offset     |
+--------+-----------------------+-----------------+
```

The 16-bit offset seeds each core's 25-bit nonce counter, top-aligned,
so the sweep starts at offset << 9. HASH_COUNTING_NUMBER (0x10) bounds
how far the sweep runs before the chip rolls the next version batch and
the nonce counter resets to the offset, so every version batch re-sweeps
the same nonce window, from the offset for the configured length. With a
distinct offset per chip, the chips in a chain hash disjoint slices. The
offsets are how BM1370 chains separate the search space; the BM1362 and
BM1366 separate chips by their assigned address instead (see [Searching
Nonces and Versions]).[^cnowrites]

On a chip with the offset written, job loads also reset the counters
(nonce to the offset, version to zero). Without the offset, the start
floats across job loads.

### 0x10 - HASH_COUNTING_NUMBER

Sets the length of each core's nonce sweep. A core sweeps nonces for the
current batch of rolled versions (one version per sub-core) until the
count runs out; the chip then rolls the next batch of versions and the
core re-sweeps the same nonce window. The count is not a nonce count.
The register counts ticks of the 25 MHz reference crystal, a deadline
rather than a quota, so the correct value depends on the hash frequency.
Each core advances two nonce candidates per hash clock, so a register
value of N ticks sweeps 2 x N x (f / 25 MHz) nonces at hash frequency f.
Zero halts hashing entirely.

The count bounds the sweep on every model; the models differ only in
where the sweep starts. The BM1362 and BM1366 derive the starting point
from the chip's address. BM1370 chains set it with CHIP_NONCE_OFFSET
(0x0C). Sized to a core's slice of the nonce space, the count keeps
every sweep inside the slice; this is how chains divide the space.

[Computing HASH_COUNTING_NUMBER] (under [Driver Guidance]) gives the
full-coverage formula, the guard for the chip's measured deadline
overrun, and a divider form that reduces the arithmetic to integers. The
value is only correct for the frequency it was computed at. Recompute
and rewrite the register on every frequency change.

### 0x14 - TICKET_MASK (Nonce Reporting Filter)

Controls which nonces the chip reports over the serial link. The
register sets a difficulty threshold, and the chip reports only nonces
whose hashes meet it. The threshold keeps the report rate within what
the serial link can carry.

The chip always requires the first 32 bits of the hash to be zero
(hardwired, equivalent to Bitcoin difficulty 1). The ticket mask
requires N additional zero bits beyond those 32; only ~1 in 2^(32+N)
hashes passes the filter.

For example, N = 8 requires 40 leading zero bits in total. At ~1 TH/s
this produces roughly 1 nonce per second, a manageable rate for the
serial link.

Because the base 32 zero bits are already baked in, the mask value
resembles a difficulty, `2^N - 1` for N extra zero bits. The resemblance
is not identity. Bitcoin checks `hash <= target`, a numerical comparison
that can express any threshold. The chip instead checks `hash & mask ==
0`, a bitwise test that requires specific bit positions to be zero and
ignores all other bits. The two agree on average probability (N zero
bits pass about 1 in 2^N either way), but they accept different sets of
hashes. Because each mask bit independently halves the pass rate, only
power-of-2 difficulty steps are possible. The mask approximates real
difficulty with a coarser filter that is cheaper in hardware.

Wire encoding:

The register value is the mask of N additional required zero bits,
right-justified into a 32-bit value (2^N - 1), with each byte's bits
then reversed in place. The host sends it most significant byte first,
like any register value:

| Difficulty | Mask       | Register value | Wire bytes  |
|------------|------------|----------------|-------------|
| 256        | 0x000000FF | 0x000000FF     | 00 00 00 FF |
| 1024       | 0x000003FF | 0x0000C0FF     | 00 00 C0 FF |

The bit reversal presumably lines the mask up with whatever bit and byte
order the chip's comparison logic uses. The job's hash fields show the
same pattern with a coarser grain. The host serializes prev_block_hash
and merkle_root in the chip's internal word order rather than Bitcoin's
(see [Mining Job]).

### 0x18 - MISC_CONTROL

Gates the chip's nonce reporting.

```text
 31           28 27            16 15              0
+---------------+----------------+-----------------+
|report enables |  unexplained   |    power-on     |
+---------------+----------------+-----------------+
```

- **report enables** (bits 31-28): each bit enables nonce reporting from
    one section of the core array. On the BM1370 the sections are
    quarters: bit 28 covers cores 0-31, bit 29 cores 32-63, bit 30 cores
    64-95, and bit 31 cores 96-127. Cores in a masked quarter keep
    hashing (power draw does not move), but the chip never reports their
    nonces.[^quartermap] The quarter map does not transfer to the
    65-core BM1362 as-is. Its constant clears bit 30, yet its core 64
    reports in the S19j Pro capture.[^refind]
- **unexplained** (bits 27-16): purpose unknown. Only the BM1366/68
    broadcast constant 0xFF0FC100 sets bits in this range (27-24 and
    19-16). Bits 19-16 track the reset sequencing described under
    SOFT_RESET_CONTROL (0xA8).
- **power-on** (bits 15-0): 0xC100 out of reset, carried along unchanged
    by every observed write.

The register powers on as 0x0000C100, all enables clear, so a fresh chip
cannot report a nonce, though it answers register reads.[^miscpoweron]

Bring-up writes one model-specific constant to this register twice:
broadcast early in bring-up, then again per chip during the
configuration pass, each write immediately after a SOFT_RESET_CONTROL
(0xA8) write (the pairing is described there). Reference firmware calls
this write "open core", distinct from the core-enable word posted
through CORE_MAILBOX (0x3C). In the working model, the high bits enable
the chip's nonce reporting path section by section, and opening the core
means switching reporting on:[^opencore]

| Model     | Broadcast  | Per chip   |
|-----------|------------|------------|
| BM1362    | 0xB000C100 | 0xB000C100 |
| BM1366/68 | 0xFF0FC100 | 0xF000C100 |
| BM1370    | 0xF000C100 | 0xF000C100 |

On the BM1397 generation this register instead held the baud rate
divider and serial pin selectors, a layout some references still echo
for later models.[^misc1397]

### 0x28 - UART_BAUD

Sets the serial link's baud rate with a clock divider.

```text
 31  29 28  27           16 15       8 7        0
+------+---+---------------+----------+----------+
| zero | ? |  unexplained  | divider  |   zero   |
+------+---+---------------+----------+----------+
```

- Bit 28: set by BM1362-generation firmware and cleared by
    BM1370-generation firmware, for the same resulting rate. Its
    function is unknown.
- Bits 27-16: 0x130 in the reset value and in every observed
    write; unexplained
- Bits 15-8 (divider): the baud rate is 25 MHz / (8 x (divider + 1))

The register resets to 0x01301A00, divider 26, so a chip comes out of
reset at 115,740 baud and stays there until the host writes a smaller
divider. A chain hashes at 115,740 baud if the host never writes the
register. To raise the rate, the host broadcasts the new divider once
during bring-up, waits for the write to drain at the old rate, then
switches its own UART to match. Observed values:

| Value      | Divider | Baud      | Writer           |
|------------|---------|-----------|------------------|
| 0x01301A00 | 26      | 115,740   | reset            |
| 0x11300200 | 2       | 1,041,667 | Bitaxe capture   |
| 0x11300000 | 0       | 3,125,000 | S19j Pro capture |
| 0x01300000 | 0       | 3,125,000 | S21 Pro capture  |

### 0x2C - UART_RELAY

Configures the serial lines on the first and last chip of each voltage
domain, the two chips whose links cross to a neighboring domain:

```text
 31              16 15                    2 1     0
+------------------+-----------------------+-----+-----+
|     gap count    | zero in every capture | rsp | cmd |
+------------------+-----------------------+-----+-----+
```

- Bits 31-16: gap count, a name from the references. It sets a wait.
    Neither units nor mechanism is documented anywhere.
- Bit 1 (rsp): relay the response line, toward the host
- Bit 0 (cmd): relay the command line, toward the next chip

A set relay bit most likely makes a boundary chip re-clock the line,
sampling the incoming signal against its own clock and sending the frame
out again instead of passing the incoming edges through. That model is
inferred, not measured. The bits do not enable the forwarding itself; a
chain carries commands down and responses back with this register
unwritten. Relay is a name from the references.

[The Serial Chain] describes the voltage domains and their boundaries
and argues the response-line interpretation of the gap count from the
captured values.

### 0x3C - CORE_MAILBOX

Indirect access to a small register space inside each core. The 32-bit
word posted to the mailbox names a core register, carries a value, and
addresses one core or all of them:[^mailbox]

```text
 31    30   24 23     16 15   14   13  12    8 7       0
+-----+-------+---------+----+----+---+-------+---------+
| all |  num  | core_id | wr | rd | 0 |  reg  |  value  |
+-----+-------+---------+----+----+---+-------+---------+
```

- Bit 31 (all): address every core at once instead of the one in core_id
- Bits 30-24 (num): zero in every observation
- Bits 23-16 (core_id): the core addressed when all is clear
- Bit 15 (wr): write the value; clear on a read
- Bit 14 (rd): read done
- Bit 13: zero in every observation
- Bits 12-8 (reg): the core register addressed
- Bits 7-0 (value): the value written to or read from the core register

Core registers written during bring-up, first broadcast, then repeated
per chip with core enable appended:

- 0x00 clock delay: 0x08 (BM1362), 0x20 (BM1366), 0x0C or
    0x18 (BM1368/70)
- 0x02 core enable: 0xAA on every model, per-chip pass only
- 0x05 clock select: 0x40 (BM1362, BM1366)
- 0x0B clock select: 0x00 (BM1368/70; BM1362/66 use 0x05 above instead)
- 0x0D nonce bin overflow control: 0xEE (BM1370, written after mining
    configuration; 0xEE enables the control, 0xEF disables it)

### 0x54 - ANALOG_MUX

Selects which analog signal the chip routes onto its analog mux output.

```text
 31                                              4 3       0
+-------------------------------------------------+---------+
|              zero in every capture              | select  |
+-------------------------------------------------+---------+
```

- Bits 31-4: zero in every capture
- Bits 3-0: the select, "diode select" in the references

Observed values:

| Select | Meaning                    | Seen on        |
|--------|----------------------------|----------------|
| 2      | Temperature diode readout  | BM1370         |
| 3      | Default (no diode readout) | BM1362, BM1370 |

The factory temperature-sense routine writes select 2, paired with a
write to ADC_CTRL1 (0xB9), to route the on-die temperature diode for
readout. The BM1370 uses select 2 in production, where an external
sensor reads the diode. BM1362 bring-up and the factory test tool's
general bring-up on the BM1370 both write select 3, the setting outside
a temperature read. The layout is the same across models, but the analog
signal each select connects is not named in any source and may differ
by model.

### 0x58 - IO_DRIVER_STRENGTH

Sets the drive strength of each chip output pin. Each output has a 4-bit
field. Factory firmware writes two values, one to every chip at bring-up
and one to the last chip of each domain:

```text
 31              20 19     16 15     12 11      8 7       4 3       0
+------------------+---------+---------+---------+---------+---------+
| zero in capture  |  RO_DS  | CLKO_DS |NRSTO_DS |  BO_DS  |  CO_DS  |
+------------------+---------+---------+---------+---------+---------+
|        0         |    1    |    1    |    1    |    1    |    1    |  bring-up
+------------------+---------+---------+---------+---------+---------+
|        0         |    1    |    F    |    1    |    1    |    1    |  last chip
+------------------+---------+---------+---------+---------+---------+
```

- Bits 31-20: zero in every capture. The BM1397 documentation names
    fields here, among them a drive strength called RF_DS, but nothing
    says what they control on these models.
- Bits 19-16 (RO_DS): response output, toward the host
- Bits 15-12 (CLKO_DS): clock output, raised to maximum on the last chip
    of each domain, which drives the clock across the domain boundary
- Bits 11-8 (NRSTO_DS): reset output
- Bits 7-4 (BO_DS): busy output
- Bits 3-0 (CO_DS): command output, toward the next chip

### 0x68 - RING_OSC_PAD_DISABLE

Disables the chip's ring-oscillator pads. The register takes a fixed
guard pattern, 0x5AA55AA5, not a decodable value.

The BM1368 and BM1370 write this register at bring-up; the BM1362 does
not. The host broadcasts the write, so it applies to a single chip as
much as to a chain.

A ring oscillator is a loop of inverters that free-runs at a frequency
set by how fast the silicon switches; chips carry them as on-die speed
monitors. Measuring one grades a die's speed, which the factory uses to
bin and characterize parts (the voltage and frequency a given chip can
hold). The oscillator's signal routes to a pad for that measurement.
Disabling those pads once mining is underway is the likely purpose,
since the characterization output is unused in production. That purpose
is inferred from the register's name, not stated by the firmware.

### 0xA4 - MIDSTATE_CONFIG

Configures version rolling for AsicBoost:[^midstatecfg]

```text
 31     30    29  28 27             16 15              0
+------+-----+------+-----------------+-----------------+
| auto | fix | gen  | zero in capture |  version mask   |
+------+-----+------+-----------------+-----------------+
```

- Bit 31 (auto): generate midstates automatically
- Bit 30 (fix): version fix; zero in every observation
- Bits 29-28 (gen): midstate generation code, the number of midstates
    the chip generates per job. On the BM1366 and later, 1 means 8, 2
    means 12, and 3 means 16; the BM1362 uses only 1 (8 midstates). The
    meaning of 0 is unobserved.
- Bits 15-0: mask of rollable version bits, applied to header version
    bits 28-13

A pool's version-rolling mask, shifted right 13 bits, is the register's
mask field. Stratum's 0x1FFFE000 becomes 0xFFFF. What version rolling
does for the search is the subject of [The Search Space], under
[Searching Nonces and Versions].

### 0xA8 - SOFT_RESET_CONTROL

Drives chip-internal soft resets. The register first appears in the
BM1362 generation (the BM1397 has no 0xA8) and its bit layout varies by
model. "Core" here means the whole hashing array as a reset domain, in
contrast to the always-on control logic that speaks UART and distributes
work; nothing in this register addresses individual cores.

Bit layout:
- **BM1362**: bit 4 CHIP_RST, bit 3 TOPCTRL_RST, bit 2 TVER_RST, bit 1
    CORE_SRST_FAST, bit 0 CORE_SRST. Resets to 0x00000000.
- **BM1366/68/70**: bits 18-16 set from power-on and preserved by every
    write; bits 8-4 set once per chip at bring-up and kept set while
    hashing; bits 3-0 runtime core soft reset. Resets to 0x00070000.

Every observed write is either the model's reset default or the default
plus reset-assert bits:

- **Broadcast during bring-up**, normalizing chip state before
    enumeration: the reset default. BM1362 0x00000000,
    BM1366/68/70 0x00070000.
- **Per chip, immediately before core configuration**, asserting the
    core reset: BM1362 0x00000002 (CORE_SRST_FAST),
    BM1366/68/70 0x000701F0.

Every host writes MISC_CONTROL (0x18) after each 0xA8 write. The two
registers cooperate during reset sequencing (MISC_CONTROL bits 19-16
move with the reset state). The register is write-only in practice; it
is not read back in any capture.

### On-Die Telemetry (0xB0 - 0xBD)

The BM1368 and BM1370 expose on-die sensors read over the serial link: a
voltage ADC, and on the BM1368 a temperature sensor. The BM1362 does not
use these registers; it reaches an equivalent ADC through a different
block (0xD0, 0xDC, 0xE4) with six channels, not yet decoded here.

#### Voltage ADC (0xB8 - 0xBD)

- 0xB8 ADC_CONFIG: input select (above bit 13; selects 1-5 pick the
    on-die supply tiers described below) plus control. Toggling one
    control bit (0x230C030D to 0x232C030D) starts a conversion.
- 0xB9 ADC_CTRL1: ADC control, written 0x3F014381 for a voltage read.
- 0xBA, 0xBB: further control, fixed values 0x00040010 and 0x03340E80.
- 0xBD ADC_RESULT: bit 31 data-valid, bits 14-0 a 15-bit code.

To read a voltage:[^adcproc]

1. Write the four config registers.
2. Toggle the start bit in 0xB8.
3. Wait about 60 ms.
4. Read 0xBD until bit 31 is set.

The front-end voltage is code / 16384 - 1.0 volts. BM1368 firmware
scales that by 0.6 (an on-die divider to the real rail); BM1370 firmware
does not, so the 0.6 is model-specific.

The five inputs form a binary-stacked power hierarchy. The chip stacks
its power delivery in a binary tree, with tiers carrying 1, 2, and 4
units of the base tier voltage and two symmetric nodes sensed at each
lower tier. These tiers are internal to one chip, distinct from the
chain-level voltage domains under [The Serial Chain]. The paired nodes
let the host watch the stack for imbalance or a sagging branch. The
power-of-two tiering fits the chip's 128-core (2^7) organization. Which
cores sit in which tier is not named in any source; the stacking is read
off the measured voltages.[^adcladder]

#### Temperature Sensor (0xB0 / 0xB4), BM1368

- 0xB0 TEMP_SENSOR_CTRL: bit 17 soft-reset release, bit 24 cload, bit 28
    run-enable, bit 31 power-down.
- 0xB4 TEMP_SENSOR_RESULT: bit 31 data-valid, bits 15-0 a code;
    temperature is code * 0.171342 - 299.5144 degrees Celsius.

To enable the sensor: write 0x00020000, then 0x01020000, then 0x10020000
to 0xB0, about 10 ms apart.[^temp1368]

On the BM1370 this sensor is inert. The registers respond, but 0xB4
reads 0x00000000 with the enable sequence above, matching the BM1370
firmware, which stubs the sensor setup and reads temperature from an
external diode instead (see [0x54 - ANALOG_MUX]).

## Initialization Sequence

This section records the factory initialization sequences as observed in
the captures, not a specification of what the chips require. No
experiment has minimized these sequences or varied their order, so any
step, ordering, or repetition below may be an artifact of the software
that performed the initialization rather than a requirement of
the hardware.

The host pulses the chain's hardware reset line (its polarity and timing
are unestablished) before the first frame, so every sequence below
starts from freshly reset chips. The steps give the order from the
captures and decode every write's value in place; each register's entry
holds the full layout and the evidence behind it.

### Single-Chip Initialization (e.g., Bitaxe)

The Bitaxe capture, ESP-Miner driving one BM1370 toward a 525 MHz
hash clock.[^bitaxeinit]

1. **Version mask**: configure version rolling.

    MIDSTATE_CONFIG (0xA4) = 0x9000FFFF, broadcast three times:

   ```text
    31     30    29  28 27             16 15              0
   +------+-----+------+-----------------+-----------------+
   | auto | fix | gen  | zero in capture |  version mask   |
   +------+-----+------+-----------------+-----------------+
   |  1   |  0  |  01  |      0x000      |     0xFFFF      |
   +------+-----+------+-----------------+-----------------+
   ```

    - **auto** = 1: the chip generates midstates automatically
    - **gen** = 1: generation code 1, selecting 8 midstates per job
    - **version mask** = 0xFFFF: all 16 rollable version bits, the pool
        mask 0x1FFFE000 shifted right 13 bits

2. **Discovery**: probe the chain.

    CHIP_ID (0x00), broadcast read. The chip answers:

   | Chip_Type | Unknown | Chip_Addr |
   |-----------|---------|-----------|
   | 13 70     | 00      | 00        |

    - **Chip_Addr** = 0x00: no address assigned yet

    MIDSTATE_CONFIG (0xA4) = 0x9000FFFF a fourth time; no source
    explains the repetition:

   ```text
    31     30    29  28 27             16 15              0
   +------+-----+------+-----------------+-----------------+
   | auto | fix | gen  | zero in capture |  version mask   |
   +------+-----+------+-----------------+-----------------+
   |  1   |  0  |  01  |      0x000      |     0xFFFF      |
   +------+-----+------+-----------------+-----------------+
   ```

3. **Reset state**: normalize chip state, broadcast.

    SOFT_RESET_CONTROL (0xA8) = 0x00070000, the BM1370 reset default:

   ```text
    31       19 18    16 15       9 8        4 3       0
   +-----------+--------+----------+----------+---------+
   |   zero    |power-on|   zero   | bring-up |core srst|
   +-----------+--------+----------+----------+---------+
   |     0     |  111   |    0     |  00000   |  0000   |
   +-----------+--------+----------+----------+---------+
   ```

    - **bring-up** = 0: not set yet; the per-chip pass sets these

    MISC_CONTROL (0x18) = 0xF000C100:

   ```text
    31           28 27            16 15              0
   +---------------+----------------+-----------------+
   |report enables |  unexplained   |    power-on     |
   +---------------+----------------+-----------------+
   |     1111      |     0x000      |     0xC100      |
   +---------------+----------------+-----------------+
   ```

4. **Addressing**: assign the chip its address.

    ChainInactive, broadcast: every chip enters addressing mode.

    SetChipAddress: the first unaddressed chip takes address 0x00. One
    chip needs one command; a chain gets a loop here (see
    [Multi-Chip Initialization]).

5. **Broadcast configuration**: chain-wide operating values.

    CORE_MAILBOX (0x3C), two writes posting clock setup to every core:

   ```text
    31    30   24 23     16 15   14   13  12    8 7       0
   +-----+-------+---------+----+----+---+-------+---------+
   | all |  num  | core_id | wr | rd | 0 |  reg  |  value  |
   +-----+-------+---------+----+----+---+-------+---------+
   |  1  |   0   |    0    |  1 |  0 | 0 | 0x0B  |  0x00   |
   +-----+-------+---------+----+----+---+-------+---------+
   |  1  |   0   |    0    |  1 |  0 | 0 | 0x00  |  0x0C   |
   +-----+-------+---------+----+----+---+-------+---------+
   ```

    - **reg** 0x0B (clock select) = 0x00
    - **reg** 0x00 (clock delay) = 0x0C

    TICKET_MASK (0x14) = 0x000000FF: eight required zero bits beyond the
    hardwired 32, so the chip reports only nonces at difficulty 256 or
    better, about one report per second near 1 TH/s.

    IO_DRIVER_STRENGTH (0x58) = 0x00011111:

   ```text
    31              20 19     16 15     12 11      8 7       4 3       0
   +------------------+---------+---------+---------+---------+---------+
   | zero in capture  |  RO_DS  | CLKO_DS |NRSTO_DS |  BO_DS  |  CO_DS  |
   +------------------+---------+---------+---------+---------+---------+
   |        0         |    1    |    1    |    1    |    1    |    1    |
   +------------------+---------+---------+---------+---------+---------+
   ```

6. **Per-chip pass**: per-chip state, ending in core enable. One chip,
    so the pass runs once, addressed to chip 0x00.

    SOFT_RESET_CONTROL (0xA8) = 0x000701F0, the core-reset assert that
    precedes core configuration:

   ```text
    31       19 18    16 15       9 8        4 3       0
   +-----------+--------+----------+----------+---------+
   |   zero    |power-on|   zero   | bring-up |core srst|
   +-----------+--------+----------+----------+---------+
   |     0     |  111   |    0     |  11111   |  0000   |
   +-----------+--------+----------+----------+---------+
   ```

    MISC_CONTROL (0x18) = 0xF000C100, the open-core value, now per chip:

   ```text
    31           28 27            16 15              0
   +---------------+----------------+-----------------+
   |report enables |  unexplained   |    power-on     |
   +---------------+----------------+-----------------+
   |     1111      |     0x000      |     0xC100      |
   +---------------+----------------+-----------------+
   ```

    CORE_MAILBOX (0x3C), three writes: the two core-clock words from
    step 5 repeated, then the core enable:

   ```text
    31    30   24 23     16 15   14   13  12    8 7       0
   +-----+-------+---------+----+----+---+-------+---------+
   | all |  num  | core_id | wr | rd | 0 |  reg  |  value  |
   +-----+-------+---------+----+----+---+-------+---------+
   |  1  |   0   |    0    |  1 |  0 | 0 | 0x0B  |  0x00   |
   +-----+-------+---------+----+----+---+-------+---------+
   |  1  |   0   |    0    |  1 |  0 | 0 | 0x00  |  0x0C   |
   +-----+-------+---------+----+----+---+-------+---------+
   |  1  |   0   |    0    |  1 |  0 | 0 | 0x02  |  0xAA   |
   +-----+-------+---------+----+----+---+-------+---------+
   ```

7. **Analog setup**: route the temperature diode, broadcast.

    ADC_CTRL1 (0xB9) = 0x00004480: analog front-end control paired with
    the mux select below; the value is undecoded and differs from the
    one a voltage read uses (see [On-Die Telemetry]).

    ANALOG_MUX (0x54) = 0x00000002: select 2 routes the temperature
    diode to the analog mux output, where the Bitaxe's external sensor
    reads it.

    ADC_CTRL1 (0xB9) = 0x00004480 again, after the mux select.

    CORE_MAILBOX (0x3C) = 0x80008DEE:

   ```text
    31    30   24 23     16 15   14   13  12    8 7       0
   +-----+-------+---------+----+----+---+-------+---------+
   | all |  num  | core_id | wr | rd | 0 |  reg  |  value  |
   +-----+-------+---------+----+----+---+-------+---------+
   |  1  |   0   |    0    |  1 |  0 | 0 | 0x0D  |  0xEE   |
   +-----+-------+---------+----+----+---+-------+---------+
   ```

    - **reg** 0x0D (nonce bin overflow control) = 0xEE: enables
        the control

8. **Frequency ramp**: step the hash clock to its target.

    PLL_DIVIDER (0x08), broadcast about every 100 ms: the requested
    frequency rises 6.25 MHz per write, from 62.5 MHz to the 525 MHz
    target, each write carrying the nearest divider solution. The first
    and last writes:

   ```text
    31    28 27          16 15  14 13      8 7   4 3   0
   +-------+--------------+------+---------+-----+-----+
   | flag  |   FB_DIV     |  0   | REF_DIV | PD1 | PD2 |
   +-------+--------------+------+---------+-----+-----+
   |  0x5  |    0x0D2     |  0   |    2    |  6  |  5  |  62.5 MHz
   +-------+--------------+------+---------+-----+-----+
   |  0x5  |    0x0D2     |  0   |    2    |  4  |  0  |  525 MHz
   +-------+--------------+------+---------+-----+-----+
   ```

    - **flag** = 0x5: PLLEN (bit 30) and VCOSEL (bit 28) set; both
        endpoints run the VCO in the high range at 2625 MHz (FB_DIV x
        25 MHz / REF_DIV)
    - **FB_DIV** = 0x0D2 (210), **REF_DIV** = 2: shared by the
        endpoints; mid-ramp solutions vary them too
    - **PD1**, **PD2**: stored minus one, so the post dividers take the
        2625 MHz VCO to 2625 / (7 x 6) = 62.5 MHz and 2625 / (5 x 1) =
        525 MHz

9. **Sweep length**: set the nonce sweep deadline.

    HASH_COUNTING_NUMBER (0x10) = 0x00001EB5: the BM1370 constant
    inherited from the S21 Pro and its 65-chip chain (see [Factory
    Values]). On one chip, each version batch sweeps about 1% of each
    core's slice; the version epoch stays the donor's 1.29 s (see [The
    Version Epoch]).

10. **Baud raise**: speed up the serial link.

    UART_BAUD (0x28) = 0x11300200: divider 2 in bits 15-8 selects 25 MHz
    / (8 x 3), about 1.04 Mbaud; the host then switches its own UART to
    1 Mbaud.

    The step is optional. A chip left at the reset divider hashes the
    same, at 115,740 baud.

11. **Mining**: rewrite the version mask, then send jobs.

    MIDSTATE_CONFIG (0xA4) = 0x9000FFFF once more:

    ```text
     31     30    29  28 27             16 15              0
    +------+-----+------+-----------------+-----------------+
    | auto | fix | gen  | zero in capture |  version mask   |
    +------+-----+------+-----------------+-----------------+
    |  1   |  0  |  01  |      0x000      |     0xFFFF      |
    +------+-----+------+-----------------+-----------------+
    ```

### Multi-Chip Initialization (e.g., S21 Pro, S19j Pro)

In both chain captures, 65 BM1370s (S21 Pro) and 126 BM1362s (S19j Pro),
stock firmware runs the same backbone through the baud
change.[^chaininit] Where a value depends on the chip model, both
models' values appear.

1. **Version mask**: configure version rolling.

    MIDSTATE_CONFIG (0xA4) = 0x9000FFFF, broadcast three times in total
    on both chains; at least one write precedes discovery (the S21 Pro
    sends all three before it, the S19j Pro one before and two after):

   ```text
    31     30    29  28 27             16 15              0
   +------+-----+------+-----------------+-----------------+
   | auto | fix | gen  | zero in capture |  version mask   |
   +------+-----+------+-----------------+-----------------+
   |  1   |  0  |  01  |      0x000      |     0xFFFF      |
   +------+-----+------+-----------------+-----------------+
   ```

    - **auto** = 1: the chip generates midstates automatically
    - **gen** = 1: generation code 1, selecting 8 midstates per job
    - **version mask** = 0xFFFF: all 16 rollable version bits, the pool
        mask 0x1FFFE000 shifted right 13 bits

2. **Discovery**: probe the chain.

    CHIP_ID (0x00), broadcast read. Every chip answers, and the host
    counts the responses:

   | Chain  | Chip_Type | Unknown | Chip_Addr |
   |--------|-----------|---------|-----------|
   | BM1370 | 13 70     | 00      | 00        |
   | BM1362 | 13 62     | 03      | 00        |

    - **Chip_Addr** = 0x00: no address assigned yet

3. **Reset state**: normalize chip state, broadcast.

    SOFT_RESET_CONTROL (0xA8), each model's reset default. The BM1370
    chain writes 0x00070000:

   ```text
    31       19 18    16 15       9 8        4 3       0
   +-----------+--------+----------+----------+---------+
   |   zero    |power-on|   zero   | bring-up |core srst|
   +-----------+--------+----------+----------+---------+
   |     0     |  111   |    0     |  00000   |  0000   |
   +-----------+--------+----------+----------+---------+
   ```

    - **bring-up** = 0: not set yet; the per-chip pass sets these

    The BM1362 chain writes 0x00000000, its all-zero reset default:

   ```text
    31                          5  4    3    2    1    0
   +-----------------------------+----+----+----+----+----+
   |            zero             |chip|top |tver|fast|srst|
   +-----------------------------+----+----+----+----+----+
   |              0              |  0 |  0 |  0 |  0 |  0 |
   +-----------------------------+----+----+----+----+----+
   ```

    MISC_CONTROL (0x18), the model's open-core constant, 0xF000C100 on
    the BM1370 and 0xB000C100 on the BM1362:

   ```text
    31           28 27            16 15              0
   +---------------+----------------+-----------------+
   |report enables |  unexplained   |    power-on     |
   +---------------+----------------+-----------------+
   |     1111      |     0x000      |     0xC100      |  BM1370
   +---------------+----------------+-----------------+
   |     1011      |     0x000      |     0xC100      |  BM1362
   +---------------+----------------+-----------------+
   ```

4. **Addressing**: cover the address space.

    ChainInactive, broadcast: every chip enters addressing mode.

    SetChipAddress, one command per address slot: the firmware sends 256
    / interval commands at the chain's address interval, regardless of
    chain length. At the captured interval of 2, that is 128 commands,
    addresses 0x00 through 0xFE. Commands for slots past the end of the
    chain (65 and 126 chips here) go unclaimed.

5. **Broadcast configuration**: chain-wide operating values.

    CORE_MAILBOX (0x3C), two writes posting clock setup to every core;
    the core registers and values differ by model:

   ```text
    31    30   24 23     16 15   14   13  12    8 7       0
   +-----+-------+---------+----+----+---+-------+---------+
   | all |  num  | core_id | wr | rd | 0 |  reg  |  value  |
   +-----+-------+---------+----+----+---+-------+---------+
   |  1  |   0   |    0    |  1 |  0 | 0 | 0x0B  |  0x00   |  BM1370
   +-----+-------+---------+----+----+---+-------+---------+
   |  1  |   0   |    0    |  1 |  0 | 0 | 0x00  |  0x0C   |  BM1370
   +-----+-------+---------+----+----+---+-------+---------+
   |  1  |   0   |    0    |  1 |  0 | 0 | 0x05  |  0x40   |  BM1362
   +-----+-------+---------+----+----+---+-------+---------+
   |  1  |   0   |    0    |  1 |  0 | 0 | 0x00  |  0x08   |  BM1362
   +-----+-------+---------+----+----+---+-------+---------+
   ```

    - **reg** 0x0B (clock select) = 0x00 on the BM1370; the BM1362
        writes its clock select through core register 0x05, as 0x40
    - **reg** 0x00 (clock delay) = 0x0C on the BM1370, 0x08 on
        the BM1362

    TICKET_MASK (0x14) = 0x000000FF, on both chains: eight required zero
    bits beyond the hardwired 32, so each chip reports only nonces at
    difficulty 256 or better, the same threshold the Bitaxe uses; the
    chain's report rate scales with its hashrate.

    ANALOG_MUX (0x54) = 0x00000003, the BM1362 chain only: select 3, the
    setting outside a temperature read. The BM1370 chain writes its mux
    during analog setup after the baud change.

    IO_DRIVER_STRENGTH (0x58) = 0x00011111, on both chains:

   ```text
    31              20 19     16 15     12 11      8 7       4 3       0
   +------------------+---------+---------+---------+---------+---------+
   | zero in capture  |  RO_DS  | CLKO_DS |NRSTO_DS |  BO_DS  |  CO_DS  |
   +------------------+---------+---------+---------+---------+---------+
   |        0         |    1    |    1    |    1    |    1    |    1    |
   +------------------+---------+---------+---------+---------+---------+
   ```

6. **Domain configuration**, BM1370 only: prepare the domain boundaries
    (see [The Serial Chain]).

    IO_DRIVER_STRENGTH (0x58) = 0x0001F111, addressed to the last chip
    of each of the chain's 13 five-chip domains:

   ```text
    31              20 19     16 15     12 11      8 7       4 3       0
   +------------------+---------+---------+---------+---------+---------+
   | zero in capture  |  RO_DS  | CLKO_DS |NRSTO_DS |  BO_DS  |  CO_DS  |
   +------------------+---------+---------+---------+---------+---------+
   |        0         |    1    |    F    |    1    |    1    |    1    |
   +------------------+---------+---------+---------+---------+---------+
   ```

    RING_OSC_PAD_DISABLE (0x68) = 0x5AA55AA5, broadcast: the fixed guard
    pattern, not a decodable value; disables the ring-oscillator pads.

    UART_RELAY (0x2C), addressed to the first and last chip of
    each domain:

   ```text
    31              16 15                    2 1     0
   +------------------+-----------------------+-----+-----+
   |     gap count    | zero in every capture | rsp | cmd |
   +------------------+-----------------------+-----+-----+
   |    0x13-0x4F     |           0           |  1  |  1  |
   +------------------+-----------------------+-----+-----+
   ```

    - **gap count**: one value per domain, from 0x13 at the far end of
        the chain, stepping by 5, to 0x4F nearest the host

7. **Baud change**: speed up the serial link.

    UART_BAUD (0x28) = 0x11300000 on the BM1362 and 0x01300000 on the
    BM1370. Divider 0 in bits 15-8 selects 25 MHz / 8, or 3.125 Mbaud
    (nominally "3 Mbaud"). Bit 28 differs by firmware generation and its
    function is unknown (see [0x28 - UART_BAUD] for the decode). The
    host waits for the write to drain, then switches its own UART.

After the baud change, stock firmware on the two machines orders the
remaining steps differently.

#### BM1362 chain, after the baud change

8. **Frequency ramp**: step the hash clock to its target, broadcast.

    PLL_DIVIDER (0x08): the requested frequency rises 6.25 MHz per
    write, from 56.25 MHz to the 525 MHz target, each write a fresh
    divider solution. The first and last writes:

   ```text
    31    28 27          16 15  14 13      8 7   4 3   0
   +-------+--------------+------+---------+-----+-----+
   | flag  |   FB_DIV     |  0   | REF_DIV | PD1 | PD2 |
   +-------+--------------+------+---------+-----+-----+
   |  0x4  |    0x0A2     |  0   |    2    |  5  |  5  |  56.25 MHz
   +-------+--------------+------+---------+-----+-----+
   |  0x4  |    0x0A8     |  0   |    2    |  3  |  0  |  525 MHz
   +-------+--------------+------+---------+-----+-----+
   ```

    - **flag** = 0x4: the low-VCO select; the VCO (FB_DIV x 25 MHz /
        REF_DIV) stays below 2.4 GHz at both endpoints
    - **PD1**, **PD2**: stored minus one, so post dividers 6 x 6 at
        56.25 MHz and 4 x 1 at 525 MHz

9. **Per-chip pass**: per-chip state, ending in core enable; to every
    chip in address order, 0x00 through 0xFA. Every chip gets the same
    bytes except its address.

    SOFT_RESET_CONTROL (0xA8) = 0x00000002:

   ```text
    31                          5  4    3    2    1    0
   +-----------------------------+----+----+----+----+----+
   |            zero             |chip|top |tver|fast|srst|
   +-----------------------------+----+----+----+----+----+
   |              0              |  0 |  0 |  0 |  1 |  0 |
   +-----------------------------+----+----+----+----+----+
   ```

    - **fast** = 1: CORE_SRST_FAST, the fast core soft reset, asserted
        ahead of core configuration

    MISC_CONTROL (0x18) = 0xB000C100, the open-core value, now per chip:

   ```text
    31           28 27            16 15              0
   +---------------+----------------+-----------------+
   |report enables |  unexplained   |    power-on     |
   +---------------+----------------+-----------------+
   |     1011      |     0x000      |     0xC100      |
   +---------------+----------------+-----------------+
   ```

    CORE_MAILBOX (0x3C), three writes: the two core-clock words again,
    then the core enable:

   ```text
    31    30   24 23     16 15   14   13  12    8 7       0
   +-----+-------+---------+----+----+---+-------+---------+
   | all |  num  | core_id | wr | rd | 0 |  reg  |  value  |
   +-----+-------+---------+----+----+---+-------+---------+
   |  1  |   0   |    0    |  1 |  0 | 0 | 0x05  |  0x40   |
   +-----+-------+---------+----+----+---+-------+---------+
   |  1  |   0   |    0    |  1 |  0 | 0 | 0x00  |  0x08   |
   +-----+-------+---------+----+----+---+-------+---------+
   |  1  |   0   |    0    |  1 |  0 | 0 | 0x02  |  0xAA   |
   +-----+-------+---------+----+----+---+-------+---------+
   ```

10. **Mining**: set the deadline, rewrite the version mask, send jobs.

    HASH_COUNTING_NUMBER (0x10) = 0x00001381: the S19j Pro factory
    constant, deliberately short of full coverage for 126 chips at 525
    MHz (see [Computing HASH_COUNTING_NUMBER]); it sets a 1.64 s version
    epoch (see [The Version Epoch]).

    MIDSTATE_CONFIG (0xA4) = 0x9000FFFF once more:

    ```text
     31     30    29  28 27             16 15              0
    +------+-----+------+-----------------+-----------------+
    | auto | fix | gen  | zero in capture |  version mask   |
    +------+-----+------+-----------------+-----------------+
    |  1   |  0  |  01  |      0x000      |     0xFFFF      |
    +------+-----+------+-----------------+-----------------+
    ```

#### BM1370 chain, after the baud change

8. **Per-chip pass**: per-chip state, ending in core enable; to every
    chip in address order, 0x00 through 0x80. Every chip gets the same
    bytes except its address.

    SOFT_RESET_CONTROL (0xA8) = 0x000701F0, the core-reset assert that
    precedes core configuration:

   ```text
    31       19 18    16 15       9 8        4 3       0
   +-----------+--------+----------+----------+---------+
   |   zero    |power-on|   zero   | bring-up |core srst|
   +-----------+--------+----------+----------+---------+
   |     0     |  111   |    0     |  11111   |  0000   |
   +-----------+--------+----------+----------+---------+
   ```

    MISC_CONTROL (0x18) = 0xF000C100, the open-core value, now per chip:

   ```text
    31           28 27            16 15              0
   +---------------+----------------+-----------------+
   |report enables |  unexplained   |    power-on     |
   +---------------+----------------+-----------------+
   |     1111      |     0x000      |     0xC100      |
   +---------------+----------------+-----------------+
   ```

    CORE_MAILBOX (0x3C), three writes: the two core-clock words again,
    then the core enable:

   ```text
    31    30   24 23     16 15   14   13  12    8 7       0
   +-----+-------+---------+----+----+---+-------+---------+
   | all |  num  | core_id | wr | rd | 0 |  reg  |  value  |
   +-----+-------+---------+----+----+---+-------+---------+
   |  1  |   0   |    0    |  1 |  0 | 0 | 0x0B  |  0x00   |
   +-----+-------+---------+----+----+---+-------+---------+
   |  1  |   0   |    0    |  1 |  0 | 0 | 0x00  |  0x0C   |
   +-----+-------+---------+----+----+---+-------+---------+
   |  1  |   0   |    0    |  1 |  0 | 0 | 0x02  |  0xAA   |
   +-----+-------+---------+----+----+---+-------+---------+
   ```

9. **Analog setup**: route the temperature diode, broadcast.

    ADC_CTRL1 (0xB9) = 0x00004480: analog front-end control paired with
    the mux select below; the value is undecoded and differs from the
    one a voltage read uses (see [On-Die Telemetry]).

    ANALOG_MUX (0x54) = 0x00000002: select 2 routes the temperature
    diode to the analog mux output for external sensing.

    ADC_CTRL1 (0xB9) = 0x00004480 again, after the mux select.

    CORE_MAILBOX (0x3C) = 0x80008DEE:

   ```text
    31    30   24 23     16 15   14   13  12    8 7       0
   +-----+-------+---------+----+----+---+-------+---------+
   | all |  num  | core_id | wr | rd | 0 |  reg  |  value  |
   +-----+-------+---------+----+----+---+-------+---------+
   |  1  |   0   |    0    |  1 |  0 | 0 | 0x0D  |  0xEE   |
   +-----+-------+---------+----+----+---+-------+---------+
   ```

    - **reg** 0x0D (nonce bin overflow control) = 0xEE: enables
        the control

10. **Frequency ramp**: step the hash clock to its target, broadcast.

    PLL_DIVIDER (0x08): the requested frequency rises 6.25 MHz per
    write, from 56.25 MHz to the 600 MHz target, each write a fresh
    divider solution. The first and last writes:

    ```text
     31    28 27          16 15  14 13      8 7   4 3   0
    +-------+--------------+------+---------+-----+-----+
    | flag  |   FB_DIV     |  0   | REF_DIV | PD1 | PD2 |
    +-------+--------------+------+---------+-----+-----+
    |  0x4  |    0x0A2     |  0   |    2    |  5  |  5  |  56.25 MHz
    +-------+--------------+------+---------+-----+-----+
    |  0x5  |    0x0C0     |  0   |    2    |  3  |  0  |  600 MHz
    +-------+--------------+------+---------+-----+-----+
    ```

    - **flag**: 0x4 at the start; 0x5 at the target, whose VCO (FB_DIV x
        25 MHz / REF_DIV) is exactly 2.4 GHz, the high-VCO
        select threshold
    - **PD1**, **PD2**: stored minus one, so post dividers 6 x 6 at
        56.25 MHz and 4 x 1 at 600 MHz

11. **Nonce offsets**: send one job, then give each chip its slice.

    CHIP_NONCE_OFFSET (0x0C), addressed to every chip in address order:

    ```text
     31       30                   16 15              0
    +--------+-----------------------+-----------------+
    | enable | zero in every capture |      offset     |
    +--------+-----------------------+-----------------+
    |   1    |           0           |  0x0000-0xFC10  |
    +--------+-----------------------+-----------------+
    ```

    - **offset**: per chip, dividing the 16-bit space evenly across the
        chain, 0x0000 for the first chip, 0x03F1 for the second, on up
        to 0xFC10 for the 65th, so the chips sweep disjoint slices (see
        [Searching Nonces and Versions])

12. **Mining**: set the deadline, rewrite the version mask, send jobs.

    HASH_COUNTING_NUMBER (0x10) = 0x00001EB5: the S21 Pro factory
    constant, the donor of the Bitaxe's value, deliberately short of
    full coverage (see [Computing HASH_COUNTING_NUMBER]); it sets a 1.29
    s version epoch (see [The Version Epoch]).

    MIDSTATE_CONFIG (0xA4) = 0x9000FFFF once more:

    ```text
     31     30    29  28 27             16 15              0
    +------+-----+------+-----------------+-----------------+
    | auto | fix | gen  | zero in capture |  version mask   |
    +------+-----+------+-----------------+-----------------+
    |  1   |  0  |  01  |      0x000      |     0xFFFF      |
    +------+-----+------+-----------------+-----------------+
    ```

Factory firmware paces the sequence: about 10 ms between consecutive
configuration writes (the address loop, the per-chip pass, and between
the SOFT_RESET_CONTROL and MISC_CONTROL writes of a pair), settles of 30
to 200 ms after milestone steps, and about 60 ms after the baud-change
write before the host switches rate.

Every host writes HASH_COUNTING_NUMBER after the frequency ramp, among
the final configuration writes, and rewrites the version mask as the
last write before steady job flow. A HASH_COUNTING_NUMBER value is
correct only for the frequency it was computed at, which is why the
write follows the ramp.

## Driver Guidance

### Computing HASH_COUNTING_NUMBER

A core sweeps only the window the HASH_COUNTING_NUMBER (0x10) deadline
allows, and every version batch re-sweeps that same window. A window
smaller than the core's slice leaves the rest of the slice permanently
unswept; a larger window wraps and duplicates work within each batch.
Correct distribution needs a window that just spans the slice.

ESP-Miner computes the register value for full nonce coverage[^pr420]
(since ported to NerdQAxePlus[^pr546]):

```text
cores_up = next_power_of_two(cores_per_chip)
chips_up = next_power_of_two(chain_length)
slice    = 2^32 / cores_up / chips_up
hcn      = slice * (25 / freq_mhz) * 0.5
```

Per-model core and sub-core counts are in [The Parallel Hierarchy].

Three rules govern the formula's use:

1. **Round down.** Rounding up spills the sweep into the neighboring
    slice, while rounding down leaves only a sub-tick sliver unswept
    (one crystal tick spans a couple dozen nonce iterations at 600 MHz).
2. **Subtract a guard.** The sweep overruns its deadline (below).
3. **Recompute on every frequency change.** The value is correct only
    for the hash frequency it was computed at. Slower cores need a
    longer deadline to finish the same slice.

Better, compute from the PLL dividers instead of a frequency in
megahertz. The hash clock comes from the same 25 MHz crystal through the
PLL, so `25 / freq_mhz` is exactly `refdiv * postdiv1 * postdiv2 /
fbdiv`, and the whole value reduces to integer arithmetic with a
single floor:

```text
hcn = floor(slice * refdiv * postdiv1 * postdiv2 / (2 * fbdiv))
```

The divider form avoids both errors of the frequency form: rounding
inside the megahertz arithmetic can push the value one high, and
computing from a requested frequency that the PLL ends up exceeding
recreates exactly the overshoot the floor avoids.

The sweep overruns its deadline by about 130 crystal ticks on every
batch, constant in ticks across clocks.[^overrun] A 200-tick guard
covers the overrun with margin, negligible against a single chip's
window and under 4% of a chain-sized window.

Stock firmware's values stop well short of full coverage. The S19j Pro's
factory value covers 80% of each core's slice, and the S21 Pro's
covers 73%.

### The Version Epoch

The version epoch is how long a chip takes to roll through the entire
2^16 version space. In one epoch the chip tries everything it will ever
try against a job, because every version batch re-sweeps the same nonce
window. Past one epoch, the chip re-hashes the same headers until the
job changes, and it gives no "work complete" signal.[^cadence] The host
should therefore replace a chip's work within one epoch.

Each batch lasts HASH_COUNTING_NUMBER crystal ticks plus the overrun of
about 130 ticks, and each batch consumes one version per sub-core of a
core. Ignoring the overrun:

```text
epoch = (2^16 / sub_cores_per_core) * hcn / 25 MHz
```

#### Factory Values

The factory values all set an epoch of about one second.[^rollrate]

| Machine           | Chip   | Sub-cores | hcn    | Epoch  |
|-------------------|--------|-----------|--------|--------|
| Antminer S21      | BM1368 | 16        | 0x15A4 | 0.91 s |
| Antminer S21 Pro  | BM1370 | 16        | 0x1EB5 | 1.29 s |
| Antminer S19k Pro | BM1366 | 8         | 0x115A | 1.46 s |
| Antminer S19j Pro | BM1362 | 8         | 0x1381 | 1.64 s |
| Antminer S19 XP   | BM1366 | 8         | 0x151C | 1.77 s |


### Distribution Across Hash Chains

A mining system may drive several chains of chips, usually one chain per
hash board. The host must prevent duplicate work across the chains.
Chains can be separated along any header dimension the upstream leaves
to the miner, or by giving each chain its own upstream:

1. **Extranonce.** The host can derive each chain's jobs from a disjoint
    slice of the extranonce2 space, so that every chain hashes a
    different merkle root. This needs the upstream to grant enough
    extranonce2 room. The room is usually ample under Stratum v1, but
    proxies that subdivide the extranonce between downstream miners can
    leave little, and SV2 header-only mining has no extranonce at all,
    which forces a different solution.

2. **Ntime.** The host can send each chain work at a different ntime
    offset (chain 0 at ntime, chain 1 at ntime + 1, and so on), so that
    no two chains hash the same header.

3. **Connection.** The host can open a separate upstream connection for
    each chain. The upstream assigns each connection its own
    extranonce1, so the chains build different merkle roots without
    further coordination. Separate SV2 channels serve the same role.

4. **Nonce.** The host can extend the chip-level division across chains,
    continuing the address or CHIP_NONCE_OFFSET assignments across chain
    boundaries and sizing HASH_COUNTING_NUMBER to the system-wide chip
    count, so all chains sweep one shared nonce space as a single
    long chain.

5. **Version.** The host could partition the version space by fixing
    some version bits per chain. Within [BIP320]'s 16 rollable bits, the
    fix would narrow each chain's rolled-version mask. The draft
    [BIP323] widens the rollable room to 24 bits (5-28). The eight added
    bits (5-12) sit below the chips' rolled range (13-28), so the host
    could fix them per chain while every chip still rolls its full
    2^16 versions.

## Sources

- ESP-Miner BM1370 implementation: [bitaxeorg/ESP-Miner]
- ESP-Miner nonce space work: [bitaxeorg/ESP-Miner#420] and the
    experiments in [skot/ESP-Miner#167]
- cgminer driver implementations: [ckolivas/cgminer]
- emberone-miner BM1362 implementation: [skot/emberone-miner]
- BM1397 documentation: [skot/BM1397]
- Bitmain datasheets for earlier chips: BM1380, BM1382, BM1384,
    and BM1385
- Factory firmware for the BM1362: the S19j Pro's stock miner, bmminer
- Factory test firmware for the BM1370: the S21 Pro's single_board_test
- Experiments run with mujina on live hardware, and the traces and logs
    they produce

Serial captures from production hardware:

| Machine           | Model  | Chips | Firmware  | Covers              |
|-------------------|--------|-------|-----------|---------------------|
| Bitaxe Gamma      | BM1370 | 1     | ESP-Miner | bring-up and mining |
| Antminer S21 Pro  | BM1370 | 65    | stock     | bring-up and mining |
| Antminer S19j Pro | BM1362 | 126   | stock     | bring-up and mining |
| Antminer S19k Pro | BM1366 | 77    | stock     | nonce traffic       |
| Antminer S19 XP   | BM1366 | 110   | stock     | nonce traffic       |

The BM1366 rows are hexdumps attached to the ESP-Miner nonce space
work above.

[^busypins]: The BM1397 documentation lists the BI and BO pins in its
    pinout (https://github.com/skot/BM1397/blob/master/readme.md). Its
    protocol page states "BI (Busy Input) signal must be pulled-down in
    order to let the BM1397 communicate"
    (https://github.com/skot/BM1397/blob/master/protocol.md). Bitmain's
    BM1380, BM1382, BM1384, and BM1385 datasheets name the pins Respond
    Busy Input and Respond Busy Output. The chaining direction comes
    from board wiring.
[^gapscale]: The captured counts fit the interpretation. A busy
    assertion reaches the far end of the chain one chip at a time, and a
    frame that a distant chip started before the assertion arrived comes
    back the same way, so the idle a chip requires should scale with the
    number of chips beyond it. The one-per-chip step matches that
    scaling, and the base value at the far domain matches a domain with
    no chip beyond it and nothing in flight to wait for.
[^joblength]: Factory firmware sends 0x36 on both the BM1362 and BM1370
    chains. ESP-Miner sends the true count 0x56 (86, everything after
    the preamble). The BM1370 accepts both values. A single-midstate job
    frame is exactly 54 bytes on the wire, so factory firmware evidently
    declares the full format's length as if the frame were in
    midstate format.
[^nummidstates]: ESP-Miner hardcodes num_midstates to 0x01 regardless of
    version rolling. Factory firmware sends 0x01 in all 227 jobs of the
    S21 Pro capture and all 105 of the S19j Pro capture.
[^startnonce]: The little-endian order follows the reference
    implementations. Every captured job sends zero, which leaves the
    byte order unexercised.
[^resp9byte]: A read of MISC_CONTROL before any configuration write
    returned the 9-byte `AA 55 00 00 C1 00 00 18 02` from a BM1370.
    Configured chips answer in the 11-byte format in every capture.
    Every bring-up writes the version-rolling enable before its first
    read. The nonce response's version field accounts for the two added
    bytes. The BM1397, with no version rolling, sends only the
    short format.
[^vcosel]: Factory firmware for the BM1362 and BM1370 confirms the 2400
    MHz threshold. The captures verify it at the boundary, where fb_div
    0xC0 with ref_div 2 is exactly 2.4 GHz and carries 0x50.
[^pllfields]: The single-bit field names (LOCKED, PLLEN, BYPASS,
    VCOSEL) come from the BM1362 firmware. The LOCKED readback is
    measured on a Bitaxe BM1370: after the frequency ramp, a read
    answers the written word with bit 31 set (flag bits 0x4 written,
    0xC read) and every divider as written.
[^pllranges]: The BM1362 fb_div and VCO ranges come from its firmware's
    PLL solver, which caps the VCO at 3125 MHz when ref_div is 1. The
    BM1370 VCO range comes from its firmware's solver. The BM1366/68 and
    BM1370 fb_div ranges come from ESP-Miner's per-model code. No
    reference gives a BM1366/68 VCO range.
[^cnowrites]: In the captures, only the S21 Pro's firmware (a BM1370
    chain, 65 chips) writes CHIP_NONCE_OFFSET. It writes each chip
    separately, after the first job, with offsets that divide the 16-bit
    space evenly across the chain. The S19j Pro's firmware (a BM1362
    chain, 126 chips) never writes the register.
[^quartermap]: The quarter map is measured on a live BM1370.
[^miscpoweron]: The power-on value is read from a BM1370. The references
    give the same value for the BM1366.
[^opencore]: The BM1362 and BM1370 table rows match the S19j Pro, S21
    Pro, and Bitaxe captures. The BM1366/68 row comes from reference
    driver code. In an S21 capture, a BM1370 chain receives 0xFF0FC100
    instead. Factory firmware builds its value by ORing the high bits
    over the register's current contents.
[^refind]: Masking also has an unexplained side effect, measured on a
    live BM1370 fed recycled identical headers. With some quarters
    masked and no offset written, re-sent identical work re-finds
    identical results from the cores still reporting. Each job load
    evidently restarts every core's sweep from the same point. An offset
    in CHIP_NONCE_OFFSET produces the same restart. With the upper half
    masked, the chip re-found 26 results exactly twice. With three
    quarters masked, the chip re-found every result three or four times
    (15 unique among 54). With all quarters enabled, the chip found 200
    results, all unique. Runs with unique headers and the same masks
    produced no duplicates. The mechanism is unknown.
[^misc1397]: The BM1397 documentation lays out 0x18 with the baud
    divider and serial pin selectors
    (https://github.com/skot/BM1397/blob/master/registers.md). Its
    BM1366 page carries the same layout forward
    (https://github.com/skot/BM1397/blob/master/bm1366_registers.md).
[^mailbox]: The field names follow reference driver code. In the
    captures the host sends only broadcast writes to the mailbox.
    Nothing in the captures reads a core register or addresses an
    individual core.
[^midstatecfg]: The field names and the generation-code values follow
    reference driver code. Every host writes 0x9000FFFF (full mask,
    generation code 1, automatic generation on).
[^adcproc]: The procedure, its pacing, and the front-end formula come
    from factory test firmware. A live BM1370 confirms all three.
[^adcladder]: On a live BM1370, selects 4 and 5 read a base unit (about
    0.14 V), selects 1 and 2 twice that, and select 3 four times. Each
    higher tier is the series sum of the two below it (select 3 = select
    1 + select 2, select 1 = select 4 + select 5). Sweeping the core
    supply moved all five in proportion and held the ratio, so these are
    live nodes tapped off the core, not fixed references.
[^temp1368]: The bit assignments, enable sequence, pacing, and
    conversion formula come from factory test firmware. They are
    unchecked on live hardware.
[^bitaxeinit]: The published ESP-Miner driver reproduces the capture
    write for write, so the sequence is source- and capture-verified.
[^chaininit]: Stock firmware for the BM1362 runs the same order: baud
    change, frequency, then the per-chip pass. The emberone-miner driver
    reproduces the sequence minus the baud change. The S21 Pro sends all
    three version-mask writes before discovery. The S19j Pro sends one
    write before discovery and two after. No source explains why the
    hosts write the mask three times. The write before discovery
    switches chips to the 11-byte response format (see [Read
    Register Response]).
[^bm1397]: The BM1397 documentation records the midstate job format and,
    in its variants table, the Antminer models that carry the chip:
    https://github.com/skot/BM1397/blob/master/protocol.md and
    https://github.com/skot/BM1397/blob/master/readme.md. ESP-Miner's
    BM1397 driver builds these midstate jobs for the original Bitaxe.
[^bm1397nonce]: The BM1397 documentation's nonce diagram shows the
    9-byte response
    (https://github.com/skot/BM1397/blob/master/protocol.md), and
    ESP-Miner's BM1397 driver reads it as a packed struct of 9 bytes
    with no version field.
[^bm1370nonce]: For the core ID, the interpretation follows the
    references, corroborated twice: first-party BM1370 firmware decodes
    bits 31-25 as the core ID, byte-identical to the BM1368, and the
    same bits are the capture-verified core ID on the BM1362 and BM1366
    (see [Searching Nonces and Versions]). For the offset, an experiment
    on a single BM1370 measured the mapping, the sweep starting at
    offset << 9, and the S21 Pro's firmware writes a per-chip offset for
    every chip on its chain. Searches of the S21 Pro capture found no
    counter bit that holds still, and range-membership attribution is
    unverified on long chains (see [The Parallel Hierarchy]).
[^midstatenum]: Reference code calls the byte Midstate_Num, a holdover
    from the midstate job format, where it named which host-supplied
    midstate produced the winning hash; here the byte
    identifies nothing.
[^bm1370exdiff]: The always-zero half bit read zero in 537 of 537 hashes
    of a measured run. The worked example decides nothing between the
    configurations: the bit following its hash's leading one is 0, so
    the share reports 12 either way.
[^bm1370subcore]: The subcore_id matches the version counter's low 4
    bits in all 7,902 captured S21 Pro responses.
[^bm1362nonce]: The nonce fields are capture-verified across chains of
    77, 110, and 126 chips; the S19j Pro's stock miner decodes the
    same positions.
[^bm1362exdiff]: Capture statistics step in whole bits (each value half
    as frequent as the last); the field is unchecked against
    reconstructed hashes.
[^bm1362split]: The 5+3 split follows emberone-miner (job id mask 0xF8,
    ids encoded shifted left 3) and the S19j Pro capture, whose factory
    job ids exceed 15: a captured result header of 0xF2 decodes as job
    id 30 with subcore_id 2, which a 4+4 packing cannot express.
[^bm1362subcore]: The runt core (every chip's core 64, three of its
    eight sub-cores dead) never reports a dead sub-core. When the
    version counter's low bits point at a dead sub-core, the subcore_id
    names the live sub-core that hashed in its place. The runt's forced
    substitutions are understood; the 8% background rate on whole cores
    is not.
[^corenum]: The BM1397 register documentation reads the chip-ID
    register's third byte as a core count
    (https://github.com/skot/BM1397/blob/master/registers.md), but the
    value matches no plausible count on either model. Its BM1366 page
    already calls the byte irrelevant:
    https://github.com/skot/BM1397/blob/master/bm1366_registers.md
[^bm1362topo]: The S19j Pro's stock firmware records the BM1362's counts
    in a topology config. The config counts 514 hashing units, not the
    520 of 65 full cores. Three of the six missing units are the dead
    sub-cores of the runt core, every chip's core 64. The other three
    presumably sit in the other cores; one missing sub-core lowers a
    core's nonce rate too little to show in the captures.
[^pr420]: https://github.com/bitaxeorg/ESP-Miner/pull/420
[^bm1370bits]: The S21 Pro capture (65 chips, per-chip offsets stepping
    evenly across the 16-bit range) shows no fixed per-chip bit and none
    of the BM1362/BM1366 low-bit structure. Capture statistics cannot
    verify the core ID, because a 128-core chip fills the 7-bit field
    exactly. The references decode bits 31-25 as the core ID, and
    first-party firmware does too, with a decoder byte-identical to the
    BM1368's. The offset << 9 mapping and the counter wrap are measured
    on a single BM1370.
[^pr546]: https://github.com/shufps/ESP-Miner-NerdQAxePlus/pull/546
[^overrun]: Measured on a single BM1370 at 525 and 262.5 MHz; the BM1362
    is unmeasured. Window-edge fits across four deadlines and two clocks
    put the overrun at 109-132 ticks, constant in ticks while halving in
    nonces with the clock. The wrap experiment (see [The Parallel
    Hierarchy]) agrees, its wrapped fraction and its deepest wrapped
    value giving 127 and 131 ticks; the wrapped values fall uniformly
    over the overrun, so the overrun is constant batch to batch. The
    NerdQAxePlus duplicate rates[^pr546] fit the same decomposition, a
    frequency term matching their solver's PLL quantization error plus a
    fixed ~96 ticks.
[^rollrate]: NerdQAxePlus once drove the register as a "version rolling
    frequency" targeting a 25 kHz roll rate; its value (about 7,864)
    comes within 0.04% of the S21 Pro factory default (7,861).
[^cadence]: The version field is the roll counter, restarting at zero on
    each job, so it gauges epoch progress (see [Nonce Response]). On the
    S19j Pro the per-job maximum averages 64% of the version space,
    about one second of rolling against the machine's 1.64 s epoch; on
    the S21 Pro, 18%, about a quarter second against 1.29 s. Both stock
    firmwares replace work comfortably inside their epochs, so the roll
    never completes and no header repeats.

[bitaxeorg/ESP-Miner]: https://github.com/bitaxeorg/ESP-Miner
[bitaxeorg/ESP-Miner#420]: https://github.com/bitaxeorg/ESP-Miner/pull/420
[skot/ESP-Miner#167]: https://github.com/skot/ESP-Miner/pull/167
[ckolivas/cgminer]: https://github.com/ckolivas/cgminer
[skot/emberone-miner]: https://github.com/skot/emberone-miner
[skot/BM1397]: https://github.com/skot/BM1397
[BIP320]: https://github.com/bitcoin/bips/blob/master/bip-0320.mediawiki
[BIP323]: https://github.com/bitcoin/bips/blob/master/bip-0323.mediawiki

[0x00 - CHIP_ID]: #0x00---chip_id
[0x08 - PLL_DIVIDER]: #0x08---pll_divider-frequency-control
[0x0C - CHIP_NONCE_OFFSET]: #0x0c---chip_nonce_offset
[0x10 - HASH_COUNTING_NUMBER]: #0x10---hash_counting_number
[0x14 - TICKET_MASK]: #0x14---ticket_mask-nonce-reporting-filter
[0x18 - MISC_CONTROL]: #0x18---misc_control
[0x28 - UART_BAUD]: #0x28---uart_baud
[0x2C - UART_RELAY]: #0x2c---uart_relay
[0x3C - CORE_MAILBOX]: #0x3c---core_mailbox
[0x54 - ANALOG_MUX]: #0x54---analog_mux
[0x58 - IO_DRIVER_STRENGTH]: #0x58---io_driver_strength
[0x68 - RING_OSC_PAD_DISABLE]: #0x68---ring_osc_pad_disable
[0xA4 - MIDSTATE_CONFIG]: #0xa4---midstate_config
[0xA8 - SOFT_RESET_CONTROL]: #0xa8---soft_reset_control
[BM1362]: #bm1362
[bm1362-nonce]: #bm1362-1
[BM1370]: #bm1370
[Byte Order]: #byte-order
[Chain Inactive]: #chain-inactive-cmd3
[Command Frames]: #command-frames-host---chip
[Command Types]: #command-types
[Computing HASH_COUNTING_NUMBER]: #computing-hash_counting_number
[Conventions]: #conventions
[CRC]: #crc
[Distribution Across Hash Chains]: #distribution-across-hash-chains
[Driver Guidance]: #driver-guidance
[Factory Values]: #factory-values
[Frame Format]: #frame-format
[Initialization Sequence]: #initialization-sequence
[Mining Job]: #mining-job-type1-cmd1
[Multi-Chip Initialization]: #multi-chip-initialization-eg-s21-pro-s19j-pro
[Nonce Response]: #nonce-response-type4
[On-Die Telemetry]: #on-die-telemetry-0xb0---0xbd
[Overview]: #overview
[Read Register]: #read-register-cmd2
[Read Register Response]: #read-register-response-type0
[Register Map]: #register-map
[Response Arbitration]: #response-arbitration
[Response Frames]: #response-frames-chip---host
[Response Types]: #response-types
[Searching Nonces and Versions]: #searching-nonces-and-versions
[Set Chip Address]: #set-chip-address-cmd0
[Single-Chip Initialization]: #single-chip-initialization-eg-bitaxe
[Sources]: #sources
[The Parallel Hierarchy]: #the-parallel-hierarchy
[The Search Space]: #the-search-space
[The Serial Chain]: #the-serial-chain
[The Version Epoch]: #the-version-epoch
[Write Register]: #write-register-cmd1
