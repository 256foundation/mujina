# BM13xx Protocol Documentation

This document describes the serial communication protocol used by the BM13xx 
family of Bitcoin mining ASICs. Since manufacturer documentation is not publicly 
available, this represents our best understanding based on analyzing open-source 
implementations and reverse engineering efforts.

**Note on Multi-Chip Chains**: Our initial implementation focuses on single-chip 
configurations (e.g., Bitaxe). Details specific to multi-chip chains may be 
incomplete or uncertain. We will refine this documentation as development 
progresses and we gain experience with multi-chip systems.

## Sources

- ESP-miner BM1370 implementation
- ESP-miner nonce space work:
  https://github.com/bitaxeorg/ESP-Miner/pull/420 and the
  experiments in https://github.com/skot/ESP-Miner/pull/167
- CGMiner driver implementations
- Emberone-miner BM1362 implementation
- BM1397 documentation: https://github.com/skot/BM1397
- Serial captures from production hardware:
  - Bitaxe Gamma (single BM1370 chip)
  - Antminer S21 Pro (65x BM1370 chips)
  - Antminer S19 J Pro (126x BM1362 chips)

## Overview

The BM13xx family (BM1362, BM1370, etc.) uses a frame-based 
serial protocol for communication between the host and mining ASICs. The 
protocol supports both command/response patterns and asynchronous nonce 
reporting.

### Chip Architecture

Different chips in the BM13xx family have varying core architectures:

- **BM1362**: Core count unknown (used in Antminer S19 J Pro)
  - Chip ID: `[0x13, 0x62]`
- **BM1370**: 128 main cores x 16 sub-cores = 2,048 total hashing units
  - Chip ID: `[0x13, 0x70]`

The core architecture affects how nonces are reported and job IDs are encoded.

## Frame Format

All frames follow this basic structure:
```
| Preamble | Type/Flags | Length | Payload | CRC |
```

### Command Frames (Host -> ASIC)
- **Preamble**: `0x55 0xAA` (2 bytes)
- **Type/Flags**: 1 byte encoding type, broadcast flag, and command
- **Length**: 1 byte total frame length
- **Payload**: Variable length data
- **CRC**: CRC5 for commands, CRC16 for jobs

### Response Frames (ASIC -> Host)
- **Preamble**: `0xAA 0x55` (2 bytes, reversed from commands)
- **Payload**: Response-specific data
- **CRC**: CRC5 in last byte (bits 0-4), with response type in bits 5-7
  - Confirmed: Response frames use CRC5 validation (verified in test cases)

## Byte Order (Endianness)

The protocol uses **little-endian** for data fields and **big-endian** for
CRC16 checksums.

### Data Fields (Little-Endian)

Multi-byte data fields transmit least significant byte first:
- **32-bit values**: nonce, nbits, ntime, version, register values
  - Example: `0x12345678` -> `[0x78, 0x56, 0x34, 0x12]`
- **16-bit values**: version field in responses
  - Example: `0x1234` -> `[0x34, 0x12]`

### Checksums (Big-Endian)

CRC16 checksums in job packets use network byte order (big-endian), transmitting
the high byte first. This follows common convention for integrity checks even in
otherwise little-endian protocols.
- **CRC16**: `0x6b18` -> `[0x6b, 0x18]`

### Special Cases

- **Hash values** (merkle_root, prev_block_hash): Convert from Bitcoin internal
  32-byte little-endian format by splitting into 8 4-byte words and reversing
  their order (word 0 with 7, 1 with 6, 2 with 5, 3 with 4).
- **chip_id in responses**: Treat as fixed byte sequence `[0x13, 0x70]` rather
  than an integer value
- **Single bytes**: No endianness applies (job_id, chip_address, etc.)

## Command Types

The Type/Flags byte (3rd byte in command frames) encodes multiple fields:

```
Bit 6: TYPE (1=register ops, 0=work)
Bit 4: BROADCAST (0=single chip, 1=all chips)
Bits 3-0: CMD value
Bits 7,5: Reserved/undefined in observed examples
```

**Implementation Note**: In our code, we use the field name `broadcast` for this
bit. Reference implementations may use different names (such as `all`), but they
all refer to the same protocol bit.

Common Type/Flags values:
- `0x40` = TYPE=1, BROADCAST=0, CMD=0 (set chip address)
- `0x41` = TYPE=1, BROADCAST=0, CMD=1 (write register to specific chip)
- `0x42` = TYPE=1, BROADCAST=0, CMD=2 (read register from specific chip)
- `0x51` = TYPE=1, BROADCAST=1, CMD=1 (write register to all chips)
- `0x52` = TYPE=1, BROADCAST=1, CMD=2 (read register from all chips - chip discovery)
- `0x53` = TYPE=1, BROADCAST=1, CMD=3 (chain inactive - prepare for addressing)
- `0x21` = TYPE=0, BROADCAST=0, CMD=1 (send work/job)

### Set Chip Address (CMD=0)
Assigns an address to a chip in the serial chain via daisy-chain forwarding.

**Request Format:**
```
| 0x55 0xAA | Type/Flags | Length | New_Addr | Reserved | CRC5 |
```
- Length: Always `0x05` (5 bytes excluding preamble)
- Type/Flags: `0x40` (NOT broadcast - uses daisy-chain forwarding)
- New_Addr: The address to assign (typically increments by 2: 0x00, 0x02, 0x04...)
- Reserved: Always `0x00` (no semantic meaning, possibly padding)
- Example: `55 AA 40 05 04 00 15` (assign address 0x04)

**How Daisy-Chain Addressing Works:**

After sending the ChainInactive command (CMD=3), all chips enter a special
addressing mode where they forward commands they don't respond to downstream:

1. Host sends ChainInactive (broadcast) - all chips enter addressing mode
2. Host sends SetChipAddress with new address (NOT broadcast)
3. First unaddressed chip in chain intercepts command and adopts that address
4. Now-addressed chip passes subsequent SetChipAddress commands downstream
5. Next unaddressed chip receives the command and adopts its address
6. Process repeats until all chips are addressed

This mechanism allows the host to sequentially address chips without knowing the
chain length beforehand. The command is NOT broadcast (BROADCAST bit = 0) because
it should only be processed by one chip at a time, but it doesn't target an
existing chip address - instead, it's intercepted by the first unaddressed chip
through forwarding.

### Chain Inactive (CMD=3)
Puts all chips into addressing mode, enabling the daisy-chain forwarding
mechanism used by SetChipAddress.

**Request Format:**
```
| 0x55 0xAA | Type/Flags | Length | Reserved | Reserved | CRC5 |
```
- Length: Always `0x05` (5 bytes excluding preamble)
- Type/Flags: `0x53` (broadcast to all chips)
- Both data bytes: Always `0x00 0x00`
- Example: `55 AA 53 05 00 00 03`

This command is broadcast to all chips before address assignment. In addressing
mode, chips that don't recognize a command (because it's not for them) will
forward it to the next chip in the chain. This enables the sequential addressing
mechanism described in SetChipAddress above.

### Read Register (CMD=2)
Reads a 4-byte register from the ASIC.

**Request Format:**
```
| 0x55 0xAA | Type/Flags | Length | Chip_Addr | Reg_Addr | CRC5 |
```
- Length: Always `0x05` (5 bytes excluding preamble)
- Type/Flags: `0x42` for specific chip, `0x52` for broadcast (chip discovery)
- Example: `55 AA 52 05 00 00 0A` (broadcast read register 0x00 - chip discovery)

### Write Register (CMD=1)
Writes a 4-byte value to a register.

**Request Format:**
```
| 0x55 0xAA | Type/Flags | Length | Chip_Addr | Reg_Addr | Data[4] | CRC5 |
```
- Length: Always `0x09` (9 bytes excluding preamble)
- Type/Flags: `0x51` for broadcast, `0x41` for specific chip
- Example: `55 AA 51 09 00 A4 90 00 FF FF 1C` (broadcast write 0xFF009090 to 
register 0xA4)

### Mining Job (TYPE=1, CMD=1)

BM13xx chips support two job formats, determined by the chip model and version 
rolling requirements:

1. **Full Format**: Used by BM1362/BM1370 - ASIC calculates midstates
2. **Midstate Format**: Used by BM1397 and others - Host pre-calculates midstates

#### Full Format (BM1362/BM1370)
The ASIC calculates SHA256 midstates internally from the provided block header 
components. This format is used by the chips mujina-miner supports.

**Request Format:**
```
| 0x55 0xAA | 0x21 | Length | Job_Data | CRC16 |
```
- **Preamble**: `0x55 0xAA` (2 bytes)
- **Type/Flags**: `0x21` = TYPE=0 (work), BROADCAST=0, CMD=1
- **Length**: `0x56` (86 decimal) = 82 bytes job_data + 2 bytes CRC16 + 2 bytes 
for type/length
- **Job_Data**: 82 bytes of mining work (see below)
- **CRC16**: 16-bit CRC calculated over type/flags + length + job_data, stored in
big-endian format (MSB first)

**Job_Data Structure (82 bytes):**
```
| job_header | num_midstates | starting_nonce[4] | nbits[4] | ntime[4] |
merkle_root[32] | prev_block_hash[32] | version[4] |
```
- **job_header** (1 byte): Container for job identification
  - Bits 6-3: 4-bit job_id field (values 0-15)
  - Bits 7, 2-0: Unused by chip (should be zero)
  - The chip extracts bits 6-3 as the job identifier
- **num_midstates** (1 byte): Number of midstates (always 0x01 for BM1370)
  - ESP-miner hardcodes this to 0x01 regardless of version rolling
  - Version rolling is actually controlled by register 0xA4 (MIDSTATE_CONFIG)
  - This field may be vestigial for chips using full format
- **starting_nonce** (4 bytes): Starting nonce value (always 0x00000000)
- **nbits** (4 bytes): Encoded difficulty target (little-endian)
  - Example: 0x170E3AB4 -> transmitted as [0xB4, 0x3A, 0x0E, 0x17]
- **ntime** (4 bytes): Block timestamp (little-endian)
  - Unix timestamp
- **merkle_root** (32 bytes): Root of transaction merkle tree
  - Convert from Bitcoin internal 32-byte little-endian format by splitting
    into 8 4-byte words and reversing their order (word 0 with 7, 1 with 6, 2
    with 5, 3 with 4)
- **prev_block_hash** (32 bytes): Hash of previous block
  - Convert from Bitcoin internal 32-byte little-endian format by splitting
    into 8 4-byte words and reversing their order (word 0 with 7, 1 with 6, 2
    with 5, 3 with 4)
- **version** (4 bytes): Block version (little-endian)
  - Example: 0x20000000 -> transmitted as [0x00, 0x00, 0x00, 0x20]
  - Lower bits may be modified if version rolling enabled

**Example Job Packet:**
```
55 AA 21 56                              # Preamble + Type + Length
18                                       # job_header: bits[6:3]=0b0011 (job_id field=3)
01                                       # num_midstates = 1
00 00 00 00                              # starting_nonce
B4 3A 0E 17                              # nbits
5C 8B 67 67                              # ntime
[32 bytes merkle_root]                   # merkle_root
[32 bytes prev_block_hash]               # prev_block_hash  
00 00 00 20                              # version
XX XX                                    # CRC16
```
Total: 88 bytes (2 preamble + 1 type + 1 length + 82 job_data + 2 CRC16)

#### Midstate Format (Not Used by mujina-miner)

Some BM13xx chips (like BM1397) require the host to pre-calculate SHA256 
midstates for version rolling. In this format:
- The host calculates different midstates for each version variation
- Job packet includes 1-4 pre-calculated midstates (32 bytes each)
- Enables more efficient version rolling on the ASIC
- Total packet size varies based on number of midstates

Since BM1362/BM1370 calculate midstates internally, mujina-miner uses 
the full format exclusively. Version rolling is controlled by register 0xA4 
(MIDSTATE_CONFIG), not by the `num_midstates` field.

## Response Types

### Read Register Response (TYPE=0)
**Format (11 bytes total):**
```
| 0xAA 0x55 | Register_Value[4] | Chip_Addr | Reg_Addr | Unknown[2] | CRC5+Type |
```

All register read responses from the BM13xx chips we support use this fixed 11-byte 
format, regardless of chip model or configuration settings.

- **Register_Value**: 4-byte value read from the register
- **Chip_Addr**: Address of the responding chip
- **Reg_Addr**: Address of the register that was read
- **Unknown**: 2 bytes of unknown purpose
- **CRC5+Type**: Last byte with CRC5 in bits 0-4 and response type (0) in bits 5-7

Example response for reading register 0x00 (CHIP_ID):
- Command: `55 AA 52 05 00 00 0A`
- Response: `AA 55 13 70 00 00 00 00 00 00 10`
  - Register_Value: `13 70 00 00` (contains BM1370 chip ID in first 2 bytes)
  - Chip_Addr: `00`
  - Reg_Addr: `00`
  - Unknown: `00 00`
  - CRC5+Type: `10`

Note: Only register 0x00 read has been captured. The purpose of the 2 unknown 
bytes is not documented.

### Nonce Response (TYPE=4)

**Format (11 bytes total):**
```
| 0xAA 0x55 | Nonce[4] | Midstate_Num | Result_Header | Version[2] | CRC5+Type |
```

**Response Length Note:**
The BM13xx family chips we support (BM1362, BM1366, BM1368, BM1370) all use 11-byte 
nonce responses that include a 2-byte version field. This is confirmed by all captured 
serial data. Documentation suggests the BM1397 (also in the BM13xx family) uses 9-byte 
responses without the version field, but we choose not to support the BM1397 in this 
implementation.

**Purpose of Core and Job ID Encoding:**
The encoding allows ASICs to:
- Match nonces back to their original work assignments
- Identify which specific core found a valid nonce (main core + sub-core)
- Support efficient work distribution across all cores

**Field Encoding by Chip Type:**

#### BM1370 (128 cores x 16 sub-cores = 2,048 units):
- **Nonce**: 32-bit nonce value (little-endian)
  - Bits 31-25: Main core ID (7 bits, values 0-127)
  - Bits 24-0: Actual nonce value
- **Midstate_Num**: Chip/core identifier (uncertain - may encode chip ID in 
multi-chip chains)
- **Result_Header**: 8-bit field containing:
  - Bits 7-4: 4-bit job_id field (0-15)
  - Bits 3-0: 4-bit subcore_id (0-15)
- **Version**: 16-bit version bits (little-endian)
  - When version rolling enabled: Contains rolled bits to be shifted left 13
positions

**Job ID Bitfield Mapping:**
The 4-bit job_id field (0-15) appears at different bit positions in sent jobs
vs. returned nonces:
- **Sent**: job_header[6:3] (encode: `job_header = job_id << 3`)
- **Returned**: result_header[7:4] (extract: `job_id = result_header >> 4`)

**Implementation Note:**
mujina-miner treats job_id as a true 4-bit field (0-15) throughout the
codebase. Reference implementations (esp-miner, emberone-miner) take a
different approach: they use full u8 values (24, 48, 72...), send them
directly, and reconstruct them from responses using `(result_header & 0xf0) >>
1`. Both approaches work, but treating job_id as 4-bit aligns more naturally
with how the chip actually operates.

Example BM1370 response: `AA 55 18 00 A6 40 02 99 22 F9 91`
- Nonce: 0x40A60018 -> Main core 32, nonce value 0x00A60018
- Result_Header: 0x99 -> job_id=9 (bits 7-4), subcore_id=9 (bits 3-0)
- Version: 0xF922 -> Version bits 0x045F2000 (after shifting)


#### BM1362:
- Similar 11-byte response format
- Different field encoding than BM1370
- Midstate_Num may encode chip ID in multi-chip configurations
- Example response: `AA 55 6D B8 8E E1 01 04 03 54 94`


### Special Response Types

Some nonce responses carry special meanings:

#### Temperature Responses
- Identified by specific job_id values (e.g., 0xB4)
- Nonce field encodes temperature data instead of mining result
- Pattern: `nonce & 0x0000FFFF == 0x00000080`
- Temperature value in upper bytes of nonce field

#### Zero Nonces
- Nonce value 0x00000000 can be valid for non-mining responses
- Always check job_id to determine response type

## Register Map

Key registers used across BM13xx chips:

| Register | Name | Description |
|----------|------|-------------|
| 0x00 | CHIP_ID | Chip identification and configuration |
| 0x08 | PLL_DIVIDER | Frequency control registers for hash clock |
| 0x0C | CHIP_NONCE_OFFSET | Explicit per-chip nonce space placement |
| 0x10 | HASH_COUNTING_NUMBER | Nonce iterations per rolled version |
| 0x14 | TICKET_MASK | Difficulty mask for share submission |
| 0x18 | MISC_CONTROL | UART settings and GPIO pin configuration |
| 0x28 | UART_BAUD | UART baud rate configuration |
| 0x2C | UART_RELAY | UART relay configuration (multi-chip chains) |
| 0x3C | CORE_MAILBOX | Command mailbox for per-core registers |
| 0x54 | ANALOG_MUX | Analog mux control (rumored to control temp diode) |
| 0x58 | IO_DRIVER_STRENGTH | IO driver strength configuration |
| 0x68 | PLL3_PARAMETER | PLL3 configuration (multi-chip chains) |
| 0xA4 | MIDSTATE_CONFIG | Midstate generation and version rolling |
| 0xA8 | SOFT_RESET_CONTROL | Chip-internal soft resets |
| 0xB9 | MISC_SETTINGS | Miscellaneous settings (BM1370 only, value 0x00004480) |

### Register Details

#### 0x00 - CHIP_ID
Contains chip identification and configuration (4 bytes):
- **Byte 0-1**: Chip type identifier
  - BM1370: `[0x13, 0x70]`
  - BM1362: `[0x13, 0x62]`
- **Byte 2**: Core count or configuration
  - BM1362: `0x03`
  - BM1370: `0x00`
- **Byte 3**: Chip address (assigned during initialization)

Note: The chip type identifier should be treated as a byte sequence rather than
interpreted as an integer value to avoid endianness confusion.

#### 0x08 - PLL_DIVIDER (Frequency Control)
Controls the hash frequency through PLL configuration:
- Byte 0: VCO range (0x50 or 0x40)
- Byte 1: FB_DIV (feedback divider)
- Byte 2: REF_DIV (reference divider)
- Byte 3: POST_DIV flags (bit 1 = fixed to 1)

#### 0x0C - CHIP_NONCE_OFFSET
Places a chip's share of the nonce space explicitly. The wire value
sets bit 31 as an enable flag and carries a 16-bit offset in the low
bytes. Only the S21 Pro capture writes it: per chip, after the first
job, with offsets stepping by roughly 0xFFFF / 65 across the 65-chip
chain. The S19 J Pro capture never writes it despite its 126-chip
chain, so chip-address-based placement alone evidently suffices
there. Not yet modeled by mujina.

#### 0x10 - HASH_COUNTING_NUMBER
Limits how long each core sweeps nonces before the chip advances
to the next rolled versions and re-sweeps the same window. Zero
halts hashing entirely. The proper value
follows from the topology
and the hash frequency: each core's share of the nonce space,
scaled inversely by frequency. Observed stock firmwares approximate
it with per-model constants. See Search Space Distribution below
for the theory, formula, and known values.

#### 0x14 - TICKET_MASK (Nonce Reporting Filter)

Controls which nonces the chip reports over the serial link.
The chip hashes at billions of hashes per second but only a
tiny fraction are interesting. This register sets the threshold
so the chip only sends nonces whose hashes are hard enough to
be worth reporting.

**How it works:**

The chip always requires the first 32 bits of the bit-reversed
hash to be zero (hardwired, equivalent to Bitcoin difficulty 1).
The ticket mask specifies *additional* bit positions beyond
those 32 that must also be zero. Setting N extra zero bits
means only ~1 in 2^(32+N) hashes passes the filter.

For example, with `zero_bits = 8`, the chip requires 40 total
zero bits in the bit-reversed hash. At ~1 TH/s this produces
roughly 1 nonce per second -- a manageable rate for the serial
link.

Because the base 32 zero bits are already baked in, the mask
value resembles a difficulty: `2^N - 1` for N extra zero bits.
However, this is not identical to Bitcoin difficulty (see
below).

**Mask vs. target comparison:**

Bitcoin checks `hash < target`, a numerical comparison that
can express any arbitrary threshold. The chip instead checks
`hash & mask == 0`, a bitwise operation that only tests
whether specific bit positions are zero. It ignores all other
bits. The two approaches agree on average probability (N zero
bits = ~1-in-2^N either way), but they accept different sets
of hashes. Because each mask bit independently halves the pass
rate, only power-of-2 difficulty steps are possible. The mask
is a coarser, cheaper-to-implement filter that approximates
real difficulty.

**Wire encoding:**

Each byte of the 4-byte register value is bit-reversed before
transmission.

Example for difficulty 1024 (value `0x000003FF`, 10 zero bits):
- Byte 0: `0xFF` -> bit-reversed -> `0xFF`
- Byte 1: `0x03` -> bit-reversed -> `0xC0`
- Wire bytes: `[0xC0, 0xFF, 0x00, 0x00]`

**Bit-reversal justification:**

The per-byte bit-reversal is confirmed by esp-miner
(`_reverse_bits` in `common.c`), bosminer
(`reverse_bits().swap_bytes()` in `bm1387.rs`), and the
bm1397-docs register reference. The bm1397-docs speculates
this is because the ASIC compares against a bit-reversed SHA
hash, not just a byte-reversed one, so the mask must match
that representation.

#### 0x18 - MISC_CONTROL
Chip-level control bits. The layout shifts between generations
(BM1397 kept its baud divider here; later generations moved baud
configuration to register 0x28) and most bits carry only
unexplained names in the references, so the code keeps the value
opaque.

Factory firmware writes one model-specific value during
bring-up, broadcast and per chip. The low half word 0xC100,
matching the BM1366 reset value, is conserved everywhere; the
high byte varies:

- BM1362: 0xB000C100
- BM1366/68: 0xFF0FC100 broadcast, 0xF000C100 per chip
- BM1370: 0xF000C100 (S21 Pro) or 0xFF0FC100 (S21)

#### 0x2C - UART_RELAY
Turns the first and last chip of each voltage domain into a relay
for the serial lines, carrying them onward to the neighboring
domain. The word pairs two relay-enable bits, one per direction,
with a gap count; the bit layout lives on the typed register in
the code. The gap count times the relay, but what gap it counts
is not established: plausibly idle time between relayed frames,
in the usual serial sense of "gap", but neither units nor
mechanism is documented anywhere.

In the S21 Pro capture every domain-boundary chip relays both
directions, and each domain gets its own gap count, stepping by 5
per domain from 0x13 at the far end of the chain to 0x4F nearest
the host. The S19j Pro capture never writes this register;
BM1362 boards may not need the relay.

#### 0x3C - CORE_MAILBOX
Indirect access to a small register space inside each core. The
32-bit word posted to the mailbox names a core register, carries
a value, and addresses one core or all of them. The word's bit
layout lives on the typed register in the code. Every observed
command is a broadcast write; nothing in the captures reads a
core register or addresses an individual core.

Core registers written during bring-up, first broadcast, then
repeated per chip with core enable appended:

- 0x00 clock delay: 0x08 (BM1362), 0x20 (BM1366), 0x0C or 0x18
  (BM1368/70)
- 0x02 core enable: 0xAA on every model, per-chip pass only
- 0x05 clock select: 0x40 (BM1362, BM1366)
- 0x0B overlap monitor: 0x00 (BM1368/70)
- 0x0D unnamed: 0xEE (BM1370, written after mining configuration)

#### 0x54 - ANALOG_MUX
Selects which analog signal the chip routes onto its analog mux
output, rumored to feed the temperature diode. A small select
field in the low bits is the whole payload; see the typed
register in the code. Bring-up writes select 3 on BM1362 and 2 on
BM1370. What each selection connects is not documented anywhere.

#### 0x58 - IO_DRIVER_STRENGTH
Sets the drive strength of each chip output pin. Each output has a
4-bit field:

| Bits  | Field    | Output                        |
|-------|----------|-------------------------------|
| 0-3   | CO_DS    | Command output (to next chip) |
| 4-7   | BO_DS    | Busy output                   |
| 8-11  | NRSTO_DS | Reset output                  |
| 12-15 | CLKO_DS  | Clock output                  |
| 16-19 | RO_DS    | Response output (to host)     |
| 20-27 | (varies) | Relay enables, RF strength    |

Note: this register's value travels big-endian on the wire, unlike
most register data. Value 0x0001F111 appears as bytes `00 01 F1 11`.

Values observed in factory captures:
- All chips at init: 0x00011111 (every output at strength 1)
- Last chip of each domain: 0x0001F111 (clock output raised to
  maximum; the boundary chip drives the clock across the domain gap)

#### 0x68 - PLL3_PARAMETER
PLL3 configuration for multi-chip chains:
- Value: 0x5AA55AA5 (appears to be a magic pattern)
- Only used in multi-chip configurations

#### 0xA4 - MIDSTATE_CONFIG
Configures version rolling for AsicBoost: a 16-bit mask of rollable
version bits, a midstate generation code, and a flag for automatic
midstate generation. The bit layout and the model-specific meaning
of the generation code live on the typed register in the code.
Every capture writes `0x9000FFFF` (full mask, generation code 1,
automatic generation on). A pool's version-rolling mask maps to the
register's mask field shifted right by 13 bits, so Stratum's
`0x1FFFE000` becomes a register mask of `0xFFFF`.

#### 0xA8 - SOFT_RESET_CONTROL
Drives chip-internal soft resets. The register first appears in the
BM1362 generation (BM1397 has no 0xA8) and its bit layout varies by
model. "Core" here means the whole hashing array as a
reset domain, in contrast to the always-on control logic that speaks
UART and distributes work; nothing in this register addresses
individual cores.

Bit layout:
- **BM1362**: bit 0 CORE_SRST, bit 1 CORE_SRST_FAST, bit 2 TVER_RST,
  bit 3 TOPCTRL_RST, bit 4 CHIP_RST. Resets to `0x00000000`.
- **BM1366/68/70**: bits 0-3 runtime core soft reset; bits 4-8 set
  once per chip at bring-up and kept set while hashing; bits 16-18
  set from power-on and preserved by every write. Resets to
  `0x00070000`.

Every observed write is either the model's reset default or the
default plus reset-assert bits:
- **Broadcast during bring-up**, normalizing chip state before
  enumeration: the reset default. BM1362 `0x00000000`,
  BM1366/68/70 `0x00070000`.
- **Per chip, immediately before core configuration**, asserting the
  core reset: BM1362 `0x00000002` (CORE_SRST_FAST),
  BM1366/68/70 `0x000701F0`.

Each 0xA8 write is followed by a MISC_CONTROL (0x18) write; the two
registers cooperate during reset sequencing (MISC_CONTROL bits 16-19
move with the reset state). The register is write-only in practice:
no capture reads it back.

#### 0xB9 - MISC_SETTINGS (BM1370 only)
Undocumented miscellaneous settings register:
- Value: 0x00004480
- Written twice during BM1370 initialization
- Not used in other BM13xx variants
- Purpose unknown

## Initialization Sequence

### Single-Chip Initialization (e.g., Bitaxe)

1. **Chip Detection**
   - Write 0x9000FFFF to register 0xA4 (enable and set version mask)
   - Read register 0x00 to get chip_id
   - Verify chip type

2. **Basic Configuration**
   - Write register 0xA8 with 0x00070000
   - Write register 0x18 with 0x0000C1F0 (UART/misc control)
   - Configure register 0x3C with chip-specific sequence

3. **Mining Configuration**
   - Set difficulty via register 0x14
   - Configure IO driver strength (0x58)
   - Write register 0xB9 (BM1370 only)
   - Configure analog mux (0x54)

4. **Frequency Ramping**
   - Start at low frequency
   - Gradually increase to target
   - Use register 0x08 for PLL control

5. **Start Mining**
   - Write HASH_COUNTING_NUMBER (0x10); the value follows from the
     topology and final frequency (see Search Space Distribution)
   - Enable version rolling (0xA4)
   - Send the first job

### Multi-Chip Initialization (e.g., S21 Pro, S19 J Pro)

1. **Chain Reset and Discovery**
   - Write 0x9000FFFF to register 0xA4 three times
   - Broadcast read register 0x00 (command 0x52)
   - Count responding chips

2. **Initial Configuration**
   - Write register 0xA8 (chip-specific value)
   - Write register 0x18 (UART control)
   - Send chain inactive command (0x53)

3. **Address Assignment**
   - Chain inactive command (0x53) puts chips in addressing mode
   - Send SetChipAddress commands (0x40) with addresses incrementing by 2
   - Each command assigns address to first unaddressed chip via daisy-chain
     forwarding (see SetChipAddress command documentation for details)
   - Typically send 128 address commands regardless of actual chip count
   - Example sequence: 0x00, 0x02, 0x04, 0x06... up to 0xFE

4. **Domain Configuration** (BM1370 chains)
   - Configure IO driver strength on domain-end chips
   - Set UART relay registers on domain boundaries
   - Write PLL3 parameter (0x68)

5. **Per-Chip Configuration**
   - Configure each chip individually with registers 0xA8, 0x18, 0x3C
   - Different sequence for first vs. subsequent chips

6. **Baud Rate Change**
   - Configure register 0x28 for higher baud rate
   - BM1370: 3Mbaud (0x00003001)
   - BM1362: Different rate (0x00003011)

7. **Frequency Ramp and Start Mining**
   - Ramp frequency in steps via register 0x08
   - Write HASH_COUNTING_NUMBER (0x10); the value follows from the
     topology and final frequency (see Search Space Distribution)
   - Enable version rolling (0xA4)
   - Send the first job

Both captures place the HASH_COUNTING_NUMBER write after the
frequency ramp and immediately before the version-rolling enable.
That position follows from what the register does: it paces version
rolling, and its value depends on the topology and the final
frequency.

## Domain Management in Multi-Chip Chains

Large chip chains are divided into domains for signal integrity:

### Domain Structure
- Chips grouped into domains (typically 5-7 chips per domain)
- Special configuration for first and last chip in each domain
- Stronger IO drivers on domain boundaries

### Domain-Specific Registers

**IO Driver Strength (0x58):**
- Normal chips: 0x00011111
- Domain-end chips: 0x0001F111

**UART Relay (0x2C):**
- Configured on domain boundary chips
- Values encode domain position and relay settings

### Example: S21 Pro Domain Configuration
```
Domain 0: Chips 0x00-0x08 (relay: 0x004F0003)
Domain 1: Chips 0x0A-0x12 (relay: 0x004A0003)
Domain 2: Chips 0x14-0x1C (relay: 0x00450003)
...
```

## Key Implementation Details

### Search Space Distribution

A job is broadcast once and every chip in the chain works on it.
Nothing in the job assigns ranges; the job's starting-nonce field is
always zero. The chips carve up the search space themselves, using
their identities and a handful of registers. This section explains
that machinery level by level: first the space itself, then the
hierarchy that parallelizes it, then the registers that place and
pace the sweep.

#### The Search Space

For one job, a version-rolling BM13xx chip searches two dimensions:

- the 32-bit nonce field, 2^32 candidates, and
- the 16 rollable version bits (AsicBoost, BIP320), 2^16 variants,
  generated inside the chip once version rolling is enabled via
  MIDSTATE_CONFIG (0xA4).

Together that is 2^48 candidate headers per job. The chips need the
second dimension: a BM1370 at 600 MHz hashes about 1.2 TH/s and
would exhaust the bare nonce space in under 4 ms. Version rolling
stretches that to about four minutes for the full 2^48, which is
what lets a chip stay busy on one job while the host prepares the
next. The host can extend the space further by rolling ntime or the
extranonce, but that happens outside the chip; this section covers
only what the chip does on its own.

A chip rarely gets that space to itself, though. Outside of
single-chip boards like the Bitaxe, chips share a serial bus in
chains of dozens or more, every one of them hearing the same
broadcast job. The space has to be divided among the chips, and
then again inside each chip among its cores and sub-cores. That
division is the machinery of the rest of this section.

#### The Parallel Hierarchy

A hashboard is a chain of chips, each chip an array of cores, each
core a group of sub-cores (some references say "big cores" and
"small cores"). The BM1370 has 128 cores of 16 sub-cores, 2,048
hashing units. Each level of the hierarchy takes a dimension of the
search space:

- **Sub-cores share their core's nonce range and differ by
  version.** With version rolling enabled, the chip hands each of a
  core's sub-cores its own rolled version as a precomputed midstate,
  so one pass over the core's nonce range tests 16 versions at once
  on a BM1370. Working through all 2^16 versions takes 4,096 such
  passes in series
  (https://github.com/bitaxeorg/ESP-Miner/pull/420).
- **Cores partition the nonce space by its top bits.** Each core
  owns the slice of nonces whose top bits equal its core ID and
  counts through the remaining bits. The width of that ID field
  follows the model's core count: the BM1370's 128 cores make it
  7 bits wide (bits 31-25), leaving each core a 2^25 slice. The
  arrangement shows in the results: every nonce a core reports
  carries its ID in those top bits (see Nonce Response).
- **Chips offset where their search starts.** The chip address
  seeds each chip's placement within the nonce space, below the
  core ID bits. Newer stock firmware can also place a chip
  explicitly with CHIP_NONCE_OFFSET (0x0C). The next section works
  through the details.

#### The Nonce as Bit Fields

At the top of the nonce the layout is exact across the family: the
core ID occupies the top bits, and the field's width follows the
model's core count. The widths below are the BM1370's:

```
 31        25 24                          0
+------------+----------------------------+
|  core ID   |  per-core nonce counter    |
|  (7 bits)  |  (25 bits, 2^25 values)    |
+------------+----------------------------+
```

For example, the response documented under Nonce Response carries
nonce `0x40A60018`: its top seven bits are `0100000`, core 32, and
the remaining 25 bits, `0x00A60018`, are where that core's counter
stood. Every nonce core 32 ever returns carries those same top
bits.

On a model with a different core count, only the widths move. A
BM1366 has 112 cores, and seven bits is still the smallest ID
field that can tell 112 cores apart. But a 7-bit field has 128
possible values, and only 112 of them name a real core. Nonces
whose top bits spell one of the 16 missing IDs have no core that
begins its sweep there. This is also why the coverage arithmetic
later in this section divides by 128 rather than 112: the hardware
partitions the space by bit pattern, so each core's slice is 1/128
of the space whether or not every pattern has a core behind it.

The core ID field explains how the cores of one chip divide the
space among themselves. The next question is what keeps separate
chips apart: where in the nonce space does each chip start
counting? Two mechanisms set the starting point:

- **The chip address seeds it.** Each chip begins its sweep at a
  position derived from its address, so the chips of a chain start
  spread across the nonce space. Every capture we have assigns
  addresses at interval two, for 65-, 77-, and 126-chip chains
  alike, so the interval evidently does not derive from the chain
  length.
- **CHIP_NONCE_OFFSET (0x0C) sets it explicitly.** Newer stock
  firmware writes each chip a 16-bit offset instead of relying on
  the address alone. The S21 Pro does this per chip after the
  first job: its 65 chips receive offsets stepping by about
  0xFFFF / 65 (chip 0 at 0x0000, the next at 0x03F1, up to 0xFC10
  at the far end), spacing their starting points evenly.

From its starting point, each core counts nonces upward.
HASH_COUNTING_NUMBER (next section) sets how long it counts: when
the deadline expires, the chip rolls the next batch of versions
and the core sweeps the same span again under them. The register
is neither a ceiling on the nonce's value nor a count of version
rolls; it fixes the length of the sweep window that every version
batch repeats. Two experimental facts pin down this
restart-per-batch reading jointly with the register's units.
First, values above a threshold produce duplicated nonces:
exactly the behavior of a sweep running past the end of its slice
and wrapping within one batch, and inexplicable for a counter
that carried on across batches, which would revisit nothing until
it had covered everything, at any value. Second, the
trial-and-error full-range value (0x000F0000) is, read as a
deadline in crystal ticks, just long enough for a BM1366 core to
sweep its entire slice once per batch at that chip's operating
frequencies; read as a raw nonce count it would cover under a
tenth of a slice and the full-range observation would be
impossible. Together the two observations favor tick units and
restart-per-batch. A chip's coverage is therefore a start and a
window: the address or offset sets where the sweep begins, and
HASH_COUNTING_NUMBER sets how far every batch's sweep extends,
permanently. What a too-small value leaves unswept stays unswept.

**Address-seeded placement, concretely.** The S19 J Pro leans on
addresses alone: 126 BM1362 chips, addresses 0x00 through 0xFA at
interval two, and not one CHIP_NONCE_OFFSET write in its capture.
An address is 8 bits, and with only even addresses assigned, its
bit 0 is always zero. That leaves 7 meaningful bits, and
experiments on a single BM1366
(https://github.com/skot/ESP-Miner/pull/167) show they play two
roles:

```
 chip address
   7   6 5 4 3 2 1   0
 +---+-------------+---+
 | P |  position   | 0 |
 +---+-------------+---+
```

P, the address's top bit, becomes bit 0 of every nonce the chip
produces: chips with P = 0 return only even nonces, chips with
P = 1 only odd ones. The six position bits select one of 64
further placements within that parity, for 128 distinct placements
in all.

Where do the position bits land? A tempting model is that the
address becomes a fixed field of the nonce, just as the core ID
does at the top: mirrored into the bottom bits (address bit 7 at
nonce bit 0), it would explain the parity split, and each chip's
nonces would then be congruent, modulo 128, to its mirrored
address.

Our captures refute that model, and with it any model in which a
chip holds a fixed 7-bit pattern in the nonce's low bits. The
S19 J Pro capture contains 6,662 nonce responses, and their low 7
bits take all 128 possible values, essentially uniformly. A
126-chip chain of fixed low-bit fields would leave exactly two
7-bit patterns unused, whatever the address-to-pattern mapping;
the two patterns the mirror model forbids appear 107 times, right
at the uniform-random rate (about 104 expected). The S21 Pro's
7,902 responses show the same uniform low bits. The counters
plainly run through the low bits, so the address must place a
chip some other way, perhaps as an arithmetic starting offset
rather than a preserved bit pattern.

Nor does the address hide anywhere else in the nonce, for example
in a field adjacent to the core ID. A 126-chip fixed field must
leave exactly two patterns of its bit window unused, and a scan of
every window of the S19 J Pro's nonces finds no window with that
signature. The scan did surface real structure, but of a different
kind: BM1362 nonces almost never set bit 7 (1% of responses), and
bits 9-8 take the values 0, 1, and 2 in equal measure but 3 at a
quarter rate, independent of the version field, the response
header, and the neighboring bits. The BM1370's nonces show nothing
similar (every bit unbiased, every window full). Whatever walks
the BM1362's counters through the space skips most of the space's
bit-7 half-blocks; the mechanism is unexplained, and since it
appears uniformly across the whole chain it reflects chip
architecture, not chain placement.

Two tempting explanations for that structure fail against the
data. Bit 7 pinned at zero looks like the mirror image of address
bit 0, which is zero on every assigned address; but the BM1366
chain dumps have equally even addresses and their bit 7 runs at
42%, so the mirror is not the mechanism. A sweep truncated by job
replacement (each new job cutting the walk short partway through
the bits 9-8 cycle) predicts that the bits 9-8 value grows with a
nonce's position within its job; measured against the capture's
105 job boundaries, it is flat. The structure does grade across
the family, though: the BM1366 chains show a milder version of
the same depression (bits 7 through 9 all at 38-43%), the BM1362
the extreme form, and the BM1370 none at all.

One address bit does survive in the nonce, and it scales to whole
chains. Two further chain dumps from the same experiment thread
(77 and 110 BM1366 chips, address-placed, no explicit offsets)
put nonce parity exactly where address-fixed parity predicts:
17.3% odd against a predicted 16.9% (13 of 77 chips addressed at
or above 0x80), and 42.0% against 41.8% (46 of 110). The
S19 J Pro fits too: 49.0% odd against 49.2% predicted (62 of
126). Numerically that means the counting steps in twos, but the
hardware needs no adder that skips: picture the nonce assembled
from fields, with bit 0 wired from the address's top bit and the
counter occupying other bits. A counter never carries into a bit
that is not part of the counter, so the transplanted bit holds
with no special-casing at all. The working model is therefore a
hybrid: the address's top bit is transplanted into nonce bit 0
and held fixed, while the remaining address bits set an
arithmetic starting offset for the counting. The S21 Pro breaks
the pattern in a telling way: its parity is free (49.7% odd,
where address-fixed parity would predict 1.5%, only one of its 65
chips sitting at or above 0x80), which says CHIP_NONCE_OFFSET
does not merely supplement address placement but replaces it.

The arithmetic-offset reading also explains the one observation
every bit-field model failed: an odd address overlapping both of
its even neighbors. Sweeps that grow from starting points spaced
K apart do exactly that when the sweep length lies between K and
2K: even neighbors, 2K apart, stay disjoint, while an odd
address, K from each, overlaps both. It even suggests why the
S21 Pro bothers with explicit offsets at all: 65 chips at
addresses 0x00 through 0x80 would crowd their implicit starting
points into the lower half of the space, so the firmware
respaces them evenly across it, while the S19 J Pro's 126 chips
nearly fill the address range and its firmware writes no offsets.

Chip attribution, however, stays out of reach: beyond that single
parity bit, which chip found a nonce is not recoverable from the
nonce. The response's midstate-number byte does not look like the
chip identity either: across the capture its values halve in
frequency with each increment, the signature of a difficulty
count rather than an identifier (see Nonce Response).

Reading a few chips of the S19 J Pro chain under the two-role
scheme:

| Chip | Address | P | Position | Placement                |
|------|---------|---|----------|--------------------------|
| 0    | 0x00    | 0 | 0        | even nonces, position 0  |
| 1    | 0x02    | 0 | 1        | even nonces, position 1  |
| 63   | 0x7E    | 0 | 63       | even nonces, position 63 |
| 64   | 0x80    | 1 | 0        | odd nonces, position 0   |
| 125  | 0xFA    | 1 | 61       | odd nonces, position 61  |

The first 64 chips fill every even-nonce position; the remaining
62 fill all but two odd-nonce positions (the unassigned addresses
0xFC and 0xFE would be odd positions 62 and 63).

**Explicit placement, concretely.** The S21 Pro's 65 BM1370 chips
(128 cores each) finish their bring-up ramp at 593.75 MHz, and the
capture then shows every chip receiving its own CHIP_NONCE_OFFSET,
stepping by about 0xFFFF / 65:

| Chip | Address | CHIP_NONCE_OFFSET | Sweep starts  |
|------|---------|-------------------|---------------|
| 0    | 0x00    | 0x0000            | at the bottom |
| 1    | 0x02    | 0x03F1            | 1/65th in     |
| 63   | 0x7E    | 0xF820            | 63/65ths in   |
| 64   | 0x80    | 0xFC10            | 64/65ths in   |

By the power-of-two arithmetic above, 65 chips round up to 128, so
128 cores x 128 chips cut the space into 2^14 slices of
2^32 / 2^14 = 2^18 nonces each. HASH_COUNTING_NUMBER then sets how
much of its slice each core visits per batch of versions:

| HASH_COUNTING_NUMBER | At 593.75 MHz                         |
|----------------------|---------------------------------------|
| 0                    | no hashing at all                     |
| 0x158E (computed)    | the full 2^18 slice per version batch |
| 0x1EB5 (factory)     | 1.4x the computed full value          |

The computed row is the full-coverage formula from the next
section: `2^32 / 128 / 128 * (25 / 593.75) * 0.5 = 0x158E`. That
formula divides the space among 128 chip slots, 65 rounded up to
a power of two. But this machine does not place its chips in
power-of-two slots: its explicit offsets space the 65 chips
evenly, at 0xFFFF / 65. Recompute full coverage with 65 as the
divisor and it comes to 0x2A73 (10,867), of which the factory
0x1EB5 covers 72%. Against the power-of-two basis the factory
value would exceed full coverage by 42%, meaning duplicated work;
the sensible reading is that explicitly placed chains slice the
space by actual chip count.

The cost of treating the value as a constant shows on the Bitaxe
instead. Its firmware writes the same 0x1EB5, a value calibrated
for a 65-chip machine, to a chain of one. With no chain to share
it, each of the single chip's 128 cores owns a full 2^25 slice,
and full coverage at the Bitaxe's roughly 500 MHz computes to
about 0xC0000 (786,000 nonces per version batch). The inherited
7,861 advances each version batch by about 1% of that, so the
chip spends its time rolling versions over a sliver of the nonce
space it could be sweeping.

Fine print on the evidence behind all of this:

- Only even chip addresses produce disjoint search ranges; an odd
  address overlaps both of its even neighbors.
- The nonce cap is real: with a factory HASH_COUNTING_NUMBER value
  a chip looped its bounded slice and never wandered the full
  range, while the experimental full-range value swept all of 2^32
  from a single chip regardless of its address.
- Our captures do not show how the explicit 16-bit offset maps
  onto the per-core counter; a search for the S21 Pro's offset
  stride across the nonce bit windows of its responses found no
  alignment.
- The parity finding is confirmed at chain scale on two BM1366
  chains (77 and 110 chips) and is consistent with the BM1362
  aggregate; the disjointness finding remains single-chip BM1366
  evidence, and no per-chip experiment has run on a BM1362.

#### HASH_COUNTING_NUMBER: Pacing the Sweep

One question remains: how much of its nonce slice does a core sweep
before the chip moves to the next batch of versions? That is what
HASH_COUNTING_NUMBER sets. Each core sweeps nonces until the
deadline expires; then the chip rolls the next versions and the
core sweeps the same window again (the counter model in the
placement discussion above).

The consequences, measured on hardware in the experiments above:

- Zero halts hashing: a zero-length window means no work at all.
- Small values roll versions quickly but re-sweep the same short
  window under every batch, leaving the rest of each slice
  permanently unvisited.
- 0x000F0000 covered the full 32-bit range on a single BM1366,
  found by trial and error: the window that just spans a whole
  slice.
- Larger values still produce duplicated nonces: the sweep wraps
  its slice within a single batch.

Coverage matters when the job's search space is finite from the
chip's point of view. Under Stratum v1 the host rolls the
extranonce, so unswept nonce space costs nothing: every hash is
still a fresh header. The host can also roll ntime, gaining a
fresh space for each elapsed second. Under SV2 header-only mining
there is no extranonce, so the nonce and version space, plus what
ntime rolling the clock allows, is the entire per-job space;
partial nonce coverage shrinks it below 2^48 and forces faster
job turnover.

#### Computing the Value

ESP-Miner computes the register value for full nonce coverage
(https://github.com/bitaxeorg/ESP-Miner/pull/420, merged; ported to
NerdQAxePlus in
https://github.com/shufps/ESP-Miner-NerdQAxePlus/pull/546):

```
cores_up = next_power_of_two(cores_per_chip)
chips_up = next_power_of_two(chain_length)
hcn      = (2^32 / cores_up / chips_up) * (25 / freq_mhz) * 0.5
```

The shape of the formula makes sense if the register is a deadline
measured in crystal ticks. Each core owns a slice of the nonce
space; that is the first factor, the space divided by the bit-field
slots for cores and chips, rounded up to powers of two as a
bit-field partition demands. The sweep of that slice runs on the
hash clock, but the roll trigger apparently counts on the 25 MHz
reference crystal, which is where `25 / freq_mhz` comes from: the
same slice takes more crystal ticks to sweep when the hash clock
is slower. Read that way,

```
slice      = 2^32 / cores_up / chips_up     nonces per core
sweep time = (slice / 2) / hash clock       one full pass
hcn        = sweep time * 25 MHz            the deadline, in ticks
```

and the formula's three factors fall out. The factor of a half is
then the parity structure from the placement discussion: the
counters step in twos, covering one parity only, so a complete
pass over a slice takes `slice / 2` iterations. (An alternative
reading puts the half in the pipeline, two hashes per clock; the
arithmetic cannot distinguish them, and no source says.) All of
this is inference; what is certain is the practical consequence
of the frequency term: the value is only correct for the
frequency it was computed at, and must be rewritten whenever the
frequency changes.

Rounding matters, and it must go down. The deadline has to fire
before a core crosses the end of its slice: a value rounded up
lets the sweep spill into the neighboring slice before the roll,
duplicating work, while a value rounded down only leaves a
sub-tick sliver unswept (one crystal tick spans a couple dozen
nonce iterations at 600 MHz). The merged code floors by way of
its integer cast.

No intermediate factor needs rounding of its own, because nothing
in the computation is irrational: the hash clock comes from the
same crystal through the PLL, so `25 / freq_mhz` is exactly
`refdiv * postdiv1 * postdiv2 / fbdiv`, and the whole value
reduces to integer arithmetic with a single floor at the end:

```
hcn = floor(slice * refdiv * postdiv1 * postdiv2 / (2 * fbdiv))
```

Computing from the divider values costs nothing, floors exactly
once, and uses the frequency the PLL actually achieves by
construction, where a requested target can differ by a percent or
so and, if the achieved clock lands faster, recreates exactly the
overshoot the floor avoids. Float arithmetic on a megahertz value
is also where stray off-by-ones come from: the S21 Pro's computed
value is 0x158F if an intermediate division is rounded, 0x158E
(5,518) computed exactly.

Could these effects be what the correction terms in the wild are
compensating? For the large scale factors, no: rounding error is
a tick or two and PLL quantization a fraction of a percent, while
the factory values imply factors of tens of percent (40% and 72%
at the two capture points); those look like deliberate coverage
policy. The BM1370's subtracted 268 is subtler. At the four-chip
machine where it was calibrated, the full-coverage value is about
171,000 ticks, and a typical PLL quantization error of 0.1% fast
needs a margin of about 170 ticks, the same order as the
constant, so a frequency-gap reading is plausible there. A
frequency error demands a margin proportional to the value,
though, and 268 is a constant. That is no strike against it in
its home: the firmwares carrying it target machines of one to
eight chips, never long chains, and within that envelope a
constant tuned on one machine can be a serviceable empirical
patch. (The one tension inside the envelope is the single-chip
case, where 0.1% of the value is about a thousand ticks; either
those machines' operating points quantize slow or exact, or they
quietly duplicate a little.) The lesson is about transplanting
it: the constant encodes one machine's measured exposure, not a
law, and says nothing about long chains, where it would amount to
five percent of the value, fifty times any plausible clock error.
Computing exactly from the divider values retires the
frequency-gap exposure entirely; whatever margin still proves
necessary after that is a true per-roll cost, the version-roll
latency reading of the erratum comment. Distinguishing the two
takes a hardware experiment: compute the exact value, subtract
nothing, and watch for duplicates.

The deadline reading also explains why this is a register at all
rather than a constant baked into the silicon: the right deadline
depends on the chain length and on the ratio of hash clock to
crystal, neither of which a chip can know by itself. The host
knows both, computes the deadline, and programs it.

Worked through for machines in this document:

- **S21 Pro**: 65 chips of 128 cores at 593.75 MHz. Both counts
  round to 128, so `slice = 2^32 / 128 / 128 = 2^18`, and
  `2^18 * (25 / 593.75) * 0.5 = 0x158E`: the computed row of the
  earlier table.
- **A 12-chip BM1362 board at 525 MHz** (the EmberOne00's shape),
  taking the fit-consistent 64 cores: 12 chips round to 16, so
  `slice = 2^32 / 64 / 16 = 2^22`, and
  `2^22 * (25 / 525) * 0.5 = 0x18618` (99,864). An order of
  magnitude above every factory constant in the table below:
  short chains need large values, which is exactly what reusing a
  long-chain donor constant gets wrong.
- **The same board at half the clock**, 262.5 MHz, needs double:
  0x30C30 (199,728). Slower cores need a longer deadline to
  finish the same slice.

The BM1370 subtracts an empirically found error term of 2 x 134
before use (a hardware erratum; duplicates appear otherwise).

NerdQAxePlus previously drove this register as a "version rolling
frequency" targeting a 25 kHz roll rate. Both views describe the
same mechanism, since a nonce count per version at a given clock is
a version roll rate. Notably, their 25 kHz value (about 7,864)
lands within 0.04% of the S21 Pro factory default (7,861), which
suggests the factory values encode a fixed version-roll rate.

#### The Version Epoch

The deadline reading yields a second derived quantity: the version
epoch, how long a chip takes to roll through the entire 2^16
version space. Each batch lasts exactly HASH_COUNTING_NUMBER
crystal ticks of wall time, and each batch consumes one version
per sub-core, so:

```
epoch = (2^16 / sub_cores_per_core) * hcn / 25 MHz
```

Frequency enters only through the programmed value, so the
formula reads differently depending on how that value is managed.
Operated correctly, with the value recomputed as 1/frequency per
the full-coverage policy, a faster hash clock shortens every
batch and the wheel turns proportionally faster while sweeping
the same slice per batch. Only a stale register value pins the
epoch while coverage drifts with the clock; that is a
misconfiguration, not a mode, though it is exactly the state of a
firmware that writes a constant and then lets the user change
frequency. The factory epochs below therefore describe each
machine at its designed operating point. Worked for the machines
in this document, with sub-core counts from the ESP-Miner device
tables (BM1366: 8 per core, BM1368 and BM1370: 16 per core; the
BM1362 is hedged as 8, like its generation-mate):

| Machine | Batches | Value | Epoch |
|--------------------|-------|--------|--------|
| Antminer S21       | 4,096 | 0x15A4 | 0.91 s |
| Antminer S21 Pro   | 4,096 | 0x1EB5 | 1.29 s |
| Antminer S19k Pro  | 8,192 | 0x115A | 1.46 s |
| Antminer S19 J Pro | 8,192 | 0x1381 | 1.64 s |
| Antminer S19 XP    | 8,192 | 0x151C | 1.77 s |

The factory values, whatever their exact derivation, all set a
version wheel that turns in about a second: near-constant epochs,
with per-batch nonce coverage left to be whatever the machine's
topology and clock make of it. That is the version-roll-rate
reading of the factory constants made concrete, though the
epochs cluster around a second rather than matching exactly.

Two more consequences fall out. The Bitaxe, inheriting the S21
Pro's value, rolls its versions on the same 1.29 s wheel; its
deficit is coverage, not pace. And with the computed
full-coverage value (838,860 at 500 MHz) its epoch stretches to
4,096 * 838,860 / 25 MHz = 137 seconds, which is the same number
The Search Space section reached by the other road: 2^47
visitable header candidates (the parity structure halves the
nominal 2^48) at 1.02 TH/s is 138 seconds. At full coverage, the
epoch IS the time to try everything.

Because every batch re-sweeps the same window (the counter model
in the placement discussion), one epoch exhausts everything a
chip will ever try against a job: after the wheel wraps, it
re-treads identical headers until the job changes. The
second-scale factory epochs are therefore a requirement, not a
curiosity: firmware running factory values must refresh work at
least that often or its chips duplicate their own work. The
near-match between the factory epochs and a typical job-refresh
cadence may be exactly the design rule the constants encode.

#### Factory Values

All observed factory values, from our captures and the catalog
retained in the ESP-Miner sources:

| Machine | Chip | Chain | Value |
|---------|------|-------|-------|
| Bitaxe Gamma (capture) | BM1370 | 1 | 0x1EB5 |
| Antminer S21 Pro (capture) | BM1370 | 65 | 0x1EB5 |
| Antminer S21 stock | BM1368 | | 0x15A4 |
| Antminer S19 XP stock | BM1366 | 110 | 0x151C |
| Antminer S19 XP Luxos | BM1366 | 110 | 0x1446 |
| Antminer S19k Pro | BM1366 | 77 | 0x115A |
| Antminer S19 J Pro (capture) | BM1362 | 126 | 0x1381 |
| Full range (experiment) | BM1366 | 1 | 0x000F0000 |

The Bitaxe writes the S21 Pro value because both carry the BM1370:
in these firmwares the value is a per-model constant inherited from
the donor machine's capture, not derived from the actual chain.

#### What the Captures Determine

Two capture points fix chain length, final frequency, and value all
at once, and they adjudicate the formula:

- **S19 J Pro**: 126 chips, final ramp 525 MHz, value 0x1381
  (4,993). The value equals
  `2^32 / 128 / 64 / 525 * 25 * 0.2 = 4,993.2` exactly, where 128
  is the next power of two of 126 chips and 64 is a core count
  consistent with the fit. In the formula's terms, stock covers
  40% of full.
- **S21 Pro**: 65 chips, final ramp 593.75 MHz, value 0x1EB5
  (7,861). Divided among 128 power-of-two chip slots, the
  formula's full-coverage value is 5,518, which the factory value
  exceeds by 42%, an overshoot that would mean duplicated work.
  Divided among the actual 65 chips it is 10,867, of which the
  factory value is 72%. Since this machine spaces its chips
  explicitly at 0xFFFF / 65, the actual-count basis is the
  consistent one. The coherent reading of both capture points,
  interpretation rather than proof: address-placed chains slice
  the space by implicit power-of-two address slots, explicitly
  placed chains by actual chip count.

#### Open Questions

- The register's true units: raw nonce count or reference-clock
  ticks.
- Stock firmware's exact arithmetic. The S19 J Pro's stock miner
  binary computes this value in no reachable code, so the captures
  are the ground truth for what stock machines write.
- How chip address, CHIP_NONCE_OFFSET, and HASH_COUNTING_NUMBER
  compose per model, and in particular how the BM1362 partitions a
  126-chip chain when its stock firmware never writes
  CHIP_NONCE_OFFSET.
- Whether rewriting the value while work is in flight glitches the
  search. References rewrite it only at mining-start boundaries and
  on frequency changes.

#### Multiple Hash Board Distribution
When a mining system has multiple hash boards, the software must
prevent duplicate work across them. Boards can be separated along
any header dimension the pool leaves to the miner:

1. **Extranonce Distribution**:
   - Each board derives its jobs from a disjoint slice of the
     extranonce2 space, so every board hashes a different merkle
     root (this is mujina's approach)
   - Needs the pool to grant enough extranonce2 room. Usually
     ample under Stratum v1, but proxies that subdivide the
     extranonce between downstream miners can leave little, and
     SV2 header-only mining has no extranonce at all, which
     forces one of the other dimensions

2. **Time-Based Work Distribution**:
   - Each board receives work with a different `ntime` offset
   - Board 0: ntime + 0
   - Board 1: ntime + 1
   - Board 2: ntime + 2
   - This ensures each board searches a unique block variation

3. **Work Registry**:
   - Software maintains a registry tracking which work is on which board
   - Each work assignment has a unique ID
   - Nonce responses are matched back to the correct work/board

4. **Example**: Antminer S19 with 3 hash boards
   - Board 0: Works on block with ntime=X
   - Board 1: Works on block with ntime=X+1
   - Board 2: Works on block with ntime=X+2
   - Total: 3 boards x 76 chips x ~100 cores = ~23,000 parallel searches
   - Each searching a DIFFERENT block variation

5. **No Wasted Work**:
   - Every hash calculation is unique across all boards
   - Software actively manages work distribution
   - Hardware (chips/cores) handle nonce space division within each board

### Job ID Management

#### Purpose of Job IDs
Job IDs are critical for mining operation even though work is broadcast to all
chips.

1. **Asynchronous Nonce Returns**: Chips find and return nonces at 
unpredictable times
2. **Pipeline Overlap**: Multiple jobs can be "in flight" simultaneously:
   - Commands propagate serially through chip chains (milliseconds for 64+ 
chips)
   - Cores may still be processing old jobs when new ones arrive
   - Typically 2-3 jobs overlap during normal operation
3. **Work Identification**: When a nonce arrives, the job ID identifies which 
block template it belongs to
4. **Critical for Block Changes**: When a new block is found on the network:
   - Old work becomes invalid immediately
   - Nonces for old jobs must be discarded

#### Example Timeline
```
Time 0ms:    Send Job with job_id=0 (mining block height 850,000)
Time 50ms:   Send Job with job_id=1 (same block, updated transactions)
Time 90ms:   NEW BLOCK! Send Job with job_id=2 (mining block height 850,001)
Time 95ms:   Receive nonce with job_id=0 -> Discard (old block)
Time 100ms:  Receive nonce with job_id=2 -> Valid for current block
```

### CRC Calculation
- **CRC5**: Used for command/response frames
  - Polynomial: 0x05
  - Init: 0x1F
  - Calculated over all bytes after preamble
  - Transmitted as single byte
- **CRC16**: Used for job packets only
  - Polynomial: 0x1021 (CRC-16-CCITT-FALSE)
  - Init: 0xFFFF
  - Calculated over all bytes after preamble, before CRC
  - Transmitted in big-endian byte order (see Byte Order section above)

### Version Rolling and Midstates

Version rolling allows ASICs to expand their search space beyond the 32-bit 
nonce range by modifying the block version field.

#### How Version Rolling Works

1. **Search Order**: The ASIC searches in this sequence:
   - First: All nonces in the chip's range, using current version
   - Then: Increment version and search all nonces again
   - Continues until all allowed version values are exhausted

2. **Version Rolling Control**:
   - Version rolling is enabled via register 0xA4 (MIDSTATE_CONFIG)
   - The chip internally modifies version bits as allowed by the mask
   - For BM1370, ESP-miner always sets `num_midstates = 1`
   - AsicBoost optimization happens internally in the chip

3. **Version Mask Configuration**:
   - Set via register 0xA4 (e.g., 0x1FFFE000 enables bits 13-28)
   - ASICs can only modify bits enabled in the mask
   - The rolled bits are returned in the nonce response
   - Reconstructed version: `original_version | (response.version << 13)`

4. **Search Space Multiplication**:
   - Without version rolling: 2^32 hashes per job
   - With 16-bit version rolling: 2^32 x 2^16 = 2^48 hashes per job
   - At 1 TH/s, exhausting 2^48 hashes would take ~78 hours

5. **Job Exhaustion**:
   - No explicit "work complete" signal from the ASIC
   - Mining software must send new jobs before exhaustion

#### Version Rolling in Multi-Chip Chains

In a multi-chip chain, version rolling works seamlessly with automatic nonce 
space partitioning:

1. **Each Chip's Search Pattern**:
   - Chip searches its assigned nonce range (based on chip address)
   - After exhausting its nonce range, increments version
   - Searches the same nonce range again with new version
   - The chip address ensures no overlap between chips

3. **No Duplication**:
   - Chip address bits embedded in nonce ensure unique ranges
   - Version rolling multiplies each chip's search space equally
   - Total search space: (nonces per chip) x (chips) x (version values)
   - Example: 1B nonces x 4 chips x 65K versions = 2^50 unique hashes

4. **Timing Considerations**:
   - All chips roll versions at different times
   - Faster chips may reach version 2 while others still on version 1
   - This is fine---no coordination needed between chips
   - Each chip's nonce+version combination remains unique

### Chip Summary

| Chip | Chip ID | Cores | Sub-cores | Job ID Bits | Used In |
|------|---------|-------|-----------|-------------|----------|
| BM1362 | 0x1362 | Unknown | Unknown | Unknown | Antminer S19 J Pro |
| BM1370 | 0x1370 | 128 | ~16 | 4+4 | Bitaxe Gamma, S21 Pro |

