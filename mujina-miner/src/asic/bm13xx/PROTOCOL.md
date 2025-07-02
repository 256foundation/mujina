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
- CGMiner driver implementations
- Emberone-miner BM1362 implementation
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
- **BM1370**: 80 main cores × 16 sub-cores = 1,280 total hashing units
  - Chip ID: `[0x13, 0x70]`

The core architecture affects how nonces are reported and job IDs are encoded.

## Frame Format

All frames follow this basic structure:
```
| Preamble | Type/Flags | Length | Payload | CRC |
```

### Command Frames (Host → ASIC)
- **Preamble**: `0x55 0xAA` (2 bytes)
- **Type/Flags**: 1 byte encoding type, broadcast flag, and command
- **Length**: 1 byte total frame length
- **Payload**: Variable length data
- **CRC**: CRC5 for commands, CRC16 for jobs

### Response Frames (ASIC → Host)
- **Preamble**: `0xAA 0x55` (2 bytes, reversed from commands)
- **Payload**: Response-specific data
- **CRC**: CRC5 in last byte (bits 0-4), with response type in bits 5-7

## Byte Order (Endianness)

**All multi-byte values in the BM13xx protocol use little-endian byte order.**

This means for multi-byte values:
- The least significant byte (LSB) is transmitted first
- The most significant byte (MSB) is transmitted last

Examples:
- 16-bit value `0x1234` → transmitted as `[0x34, 0x12]`
- 32-bit value `0x12345678` → transmitted as `[0x78, 0x56, 0x34, 0x12]`

Affected fields:
- **16-bit values**: version, chip_id, CRC16
- **32-bit values**: nonce, nbits, ntime, register values

Special cases:
- **chip_id in responses**: The 2-byte chip_id field that appears in all read 
register responses should be treated as a fixed byte sequence `[0x13, 0x70]` 
rather than as an integer value
- **Hash values** (merkle_root, prev_block_hash): These are byte arrays that 
should be transmitted as-is without endianness conversion
- **Single bytes**: No endianness applies (job_id, midstate_num, etc.)

## Command Types

The Type/Flags byte (3rd byte in command frames) encodes multiple fields:

```
Bit 7: TYPE (0=command, 1=work)  
Bit 6: BROADCAST (0=single chip, 1=all chips)
Bit 5: Always 0
Bits 4-0: CMD value
```

Common Type/Flags values:
- `0x40` = TYPE=0, BROADCAST=0, CMD=0 (set chip address)
- `0x41` = TYPE=0, BROADCAST=0, CMD=1 (write register to specific chip)
- `0x42` = TYPE=0, BROADCAST=0, CMD=2 (read register from specific chip)
- `0x51` = TYPE=0, BROADCAST=1, CMD=1 (write register to all chips)
- `0x52` = TYPE=0, BROADCAST=1, CMD=2 (read register from all chips - chip discovery)
- `0x53` = TYPE=0, BROADCAST=1, CMD=3 (chain inactive - prepare for addressing)
- `0x21` = TYPE=1, BROADCAST=0, CMD=1 (send work/job)

### Set Chip Address (CMD=0)
Assigns an address to a chip in the serial chain.

**Request Format:**
```
| 0x55 0xAA | Type/Flags | Length | Chip_Addr | Reg_Addr | CRC5 |
```
- Length: Always `0x05` (5 bytes excluding preamble)
- Type/Flags: `0x40` (single chip addressing)
- Chip_Addr: The address to assign (typically increments by 2: 0x00, 0x02, 0x04...)
- Reg_Addr: Always `0x00`
- Example: `55 AA 40 05 04 00 15` (assign address 0x04)

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
- **Type/Flags**: `0x21` = TYPE=1 (work), BROADCAST=0, CMD=1
- **Length**: `0x56` (86 decimal) = 82 bytes job_data + 2 bytes CRC16 + 2 bytes 
for type/length
- **Job_Data**: 82 bytes of mining work (see below)
- **CRC16**: 16-bit CRC calculated over type/flags + length + job_data

**Job_Data Structure (82 bytes):**
```
| job_header | num_midstates | starting_nonce[4] | nbits[4] | ntime[4] | 
merkle_root[32] | prev_block_hash[32] | version[4] |
```
- **job_header** (1 byte): Identifies this job for nonce responses
  - Contains 4-bit job_id in bits 6-3
- **num_midstates** (1 byte): Number of midstates (always 0x01 for BM1370)
  - ESP-miner hardcodes this to 0x01 regardless of version rolling
  - Version rolling is actually controlled by register 0xA4 (VERSION_MASK)
  - This field may be vestigial for chips using full format
- **starting_nonce** (4 bytes): Starting nonce value (always 0x00000000)
- **nbits** (4 bytes): Encoded difficulty target (little-endian)
  - Example: 0x170E3AB4 → transmitted as [0xB4, 0x3A, 0x0E, 0x17]
- **ntime** (4 bytes): Block timestamp (little-endian)
  - Unix timestamp
- **merkle_root** (32 bytes): Root of transaction merkle tree
  - SHA256 hash, transmitted as-is (no endianness conversion)
- **prev_block_hash** (32 bytes): Hash of previous block
  - SHA256 hash, transmitted as-is (no endianness conversion)
- **version** (4 bytes): Block version (little-endian)
  - Example: 0x20000000 → transmitted as [0x00, 0x00, 0x00, 0x20]
  - Lower bits may be modified if version rolling enabled

**Example Job Packet:**
```
55 AA 21 56                              # Preamble + Type + Length
18                                       # job_header (job_id = 3)
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
(VERSION_MASK), not by the `num_midstates` field.

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
- Run multiple jobs concurrently (up to 128 different jobs)
- Identify which specific core found a valid nonce (main core + sub-core)
- Match nonces back to their original work assignments
- Support efficient work distribution across all cores

**Field Encoding by Chip Type:**

#### BM1370 (80 cores × 16 sub-cores = 1,280 units):
- **Nonce**: 32-bit nonce value (little-endian)
  - Bits 31-25: Main core ID (7 bits, values 0-79)
  - Bits 24-0: Actual nonce value
- **Midstate_Num**: Chip/core identifier (uncertain - may encode chip ID in 
multi-chip chains)
- **Result_Header**: 8-bit field containing:
  - Bits 7-4: 4-bit job_id (0-15) 
  - Bits 3-0: 4-bit subcore_id (0-15)
- **Version**: 16-bit version bits (little-endian)
  - When version rolling enabled: Contains rolled bits to be shifted left 13 
positions

Example BM1370 response: `AA 55 18 00 A6 40 02 99 22 F9 91`
- Nonce: 0x40A60018 → Main core 12, nonce value 0x00A60018
- Result_Header: 0x99 → job_id=9 (bits 7-4), subcore_id=9 (bits 3-0)
- Version: 0xF922 → Version bits 0x045F2000 (after shifting)


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
| 0x10 | NONCE_RANGE | Controls nonce search range per core |
| 0x14 | TICKET_MASK | Difficulty mask for share submission |
| 0x18 | MISC_CONTROL | UART settings and miscellaneous control |
| 0x28 | UART_BAUD | UART baud rate configuration |
| 0x2C | UART_RELAY | UART relay configuration (multi-chip chains) |
| 0x3C | CORE_REGISTER | Core configuration and control |
| 0x54 | ANALOG_MUX | Analog mux control (rumored to control temp diode) |
| 0x58 | IO_DRIVER_STRENGTH | IO driver strength configuration |
| 0x68 | PLL3_PARAMETER | PLL3 configuration (multi-chip chains) |
| 0xA4 | VERSION_MASK | Version rolling mask configuration |
| 0xA8 | INIT_CONTROL | Initialization control register |
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

#### 0x10 - NONCE_RANGE
Controls nonce search space distribution (format not fully documented):
- Affects how chips divide the 32-bit nonce space
- Different values used for different chip counts
- Mechanism remains partially understood through empirical testing

#### 0x14 - TICKET_MASK (Difficulty)
Sets the difficulty mask (4 bytes, little-endian):
- Each byte is bit-reversed
- Example: difficulty 256 = 0xFF000000 → transmitted as [0xFF, 0x00, 0x00, 
0x00]

#### 0x2C - UART_RELAY
Controls UART signal relay in multi-chip chains (4 bytes):
- Used on first and last chips in each domain
- Format appears to encode domain boundaries
- Example values from S21 Pro: 0x00130003, 0x00180003, etc.

#### 0x3C - CORE_REGISTER
Requires multiple writes during initialization. Values differ by chip type:

**BM1362 sequence:**
1. Write 0x80008540
2. Write 0x80008008

**BM1370 sequence:**
1. Write 0x80008B00
2. Write 0x8000800C
3. Write 0x800082AA (per chip configuration)

#### 0x54 - ANALOG_MUX
Controls analog multiplexer, possibly for temperature sensing:
- BM1370: Write value 0x00000002
- BM1362: Write value 0x00000003
- Purpose not fully documented by manufacturer

#### 0x58 - IO_DRIVER_STRENGTH
Controls IO signal driver strength (4 bytes):
- Normal chips: 0x00011111
- Domain-end chips: 0x0001F111 (stronger drive for signal integrity)
- Configured differently for last chip in each domain

#### 0x68 - PLL3_PARAMETER
PLL3 configuration for multi-chip chains:
- Value: 0x5AA55AA5 (appears to be a magic pattern)
- Only used in multi-chip configurations

#### 0xA4 - VERSION_MASK
Controls which bits of the version field can be rolled:
- Lower 16 bits typically enabled for rolling
- Set via Stratum configuration (e.g., 0x1FFFE000)
- Initial enable: 0xFFFF0090 (from captures)

#### 0xA8 - INIT_CONTROL
Initialization control register with chip-specific values:
- BM1362: 0x00000000
- BM1370 single-chip: 0x00070000
- BM1370 multi-chip: 0x00070000 initially, then 0xF0010700 per chip

#### 0xB9 - MISC_SETTINGS (BM1370 only)
Undocumented miscellaneous settings register:
- Value: 0x00004480
- Written twice during BM1370 initialization
- Not used in other BM13xx variants
- Purpose unknown

## Initialization Sequence

### Single-Chip Initialization (e.g., Bitaxe)

1. **Chip Detection**
   - Write 0xFFFF0090 to register 0xA4 (enable and set version mask)
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

### Multi-Chip Initialization (e.g., S21 Pro, S19 J Pro)

1. **Chain Reset and Discovery**
   - Write 0xFFFF0090 to register 0xA4 three times
   - Broadcast read register 0x00 (command 0x52)
   - Count responding chips

2. **Initial Configuration**
   - Write register 0xA8 (chip-specific value)
   - Write register 0x18 (UART control)
   - Send chain inactive command (0x53)

3. **Address Assignment**
   - Assign addresses incrementing by 2 (0x00, 0x02, 0x04...)
   - Use command 0x40 for each address
   - Typically assign 128 addresses regardless of actual chip count

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

7. **Final Configuration**
   - Set NONCE_RANGE (0x10) based on chip count
   - Configure remaining registers
   - Begin frequency ramping

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

### Job Distribution Across Multiple Chips

In multi-chip mining systems, job distribution works as follows:

#### Chip Addressing
- Each chip in a chain is assigned a unique 8-bit address during initialization
- Addresses are typically spaced evenly (e.g., 0, 4, 8, 12... for a 64-chip 
chain)
- The address determines which portion of the nonce space each chip searches

#### Job Broadcasting
- **The same job is sent to ALL chips in the chain**
- Single broadcast command propagates through the entire chain
- Each chip automatically works on a different portion of the nonce space

#### Nonce Space Partitioning
The 32-bit nonce space (4.3 billion values) is automatically divided:

1. **Between Chips**: Based on chip address and NONCE_RANGE register
   - Chip address influences which nonces are searched
   - NONCE_RANGE register (0x10) further controls distribution
   - No explicit range assignment needed from software

2. **Between Cores**: Within each chip
   - Core ID encoded in upper nonce bits (typically bits 24-31)
   - Each core searches ~33.5 million nonces (4.3B / 128 cores)

3. **Example**: BM1370 with 80 cores × 16 sub-cores
   - Bits 31-25: Main core ID (80 cores)
   - Bits 24-0: Actual nonce value searched
   - Total: 1,280 parallel searches per chip

#### NONCE_RANGE Register Configuration

The NONCE_RANGE register (0x10) uses empirically-determined values to optimize 
nonce distribution. See discussion at: https://github.com/bitaxeorg/ESP-Miner/pull/167

**Known Values (4-byte little-endian):**
- 1 chip: `0x00001EB5` (Bitaxe single BM1370)
- 65 chips: `0x00001EB5` (S21 Pro - same as single chip!)
- 77 chips: `0x0000115A` (S19k Pro - from ESP-miner)
- 110 chips: `0x0000141C` (S19XP Stock - from ESP-miner)
- 110 chips: `0x00001446` (S19XP Luxos - from ESP-miner)
- 126 chips: `0x00001381` (S19 J Pro BM1362)
- Full range: `0x000F0000` (experimental, searches full 32-bit space?)

**How It Likely Works:**
While the exact mechanism is undocumented, analysis suggests:
- The value may define a stride/increment for nonce searching
- Combined with chip address to ensure non-overlapping ranges
- Smaller values for more chips ensure better coverage
- Values appear carefully chosen to minimize gaps in search space

**Example Theory:**
With register value 0x00001EB5 (7,861 decimal):
- Chip might test nonces at intervals of 7,861
- Starting offset based on chip address
- Ensures even distribution without collision

Note: The ESP-miner source notes this register is "still a bit of a mystery" 
and values are determined through empirical testing rather than documentation.
Multi-chip configurations may require different values than those listed.

#### Starting Nonce Field
- Always set to 0x00000000 in practice
- Hardware automatically offsets based on chip/core addressing
- Software doesn't need to manually partition the nonce space

#### Practical Example: 4-Chip Chain
Consider a 4-chip BM1370 chain mining a block:
1. **Job sent**: Same job broadcast to all 4 chips
2. **Chip addresses**: 0x00, 0x40, 0x80, 0xC0 (64 apart)
3. **Nonce space division**:
   - Chip 0: Searches nonces where certain bits = 0x00
   - Chip 1: Searches nonces where certain bits = 0x40
   - Chip 2: Searches nonces where certain bits = 0x80
   - Chip 3: Searches nonces where certain bits = 0xC0
4. **Total parallel operations**: 4 chips × 1,280 cores = 5,120 simultaneous 
searches

#### Multiple Hash Board Distribution
When a mining system has multiple hash boards, the software MUST prevent 
duplicate work:

1. **Time-Based Work Distribution** (most common):
   - Each board receives work with a different `ntime` offset
   - Board 0: ntime + 0
   - Board 1: ntime + 1
   - Board 2: ntime + 2
   - This ensures each board searches a unique block variation

2. **Work Registry**:
   - Software maintains a registry tracking which work is on which board
   - Each work assignment has a unique ID
   - Nonce responses are matched back to the correct work/board

3. **Example**: Antminer S19 with 3 hash boards
   - Board 0: Works on block with ntime=X
   - Board 1: Works on block with ntime=X+1
   - Board 2: Works on block with ntime=X+2
   - Total: 3 boards × 76 chips × ~100 cores = ~23,000 parallel searches
   - Each searching a DIFFERENT block variation

4. **No Wasted Work**:
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
Time 0ms:    Send Job 0x00 (mining block height 850,000)
Time 50ms:   Send Job 0x18 (same block, updated transactions)
Time 90ms:   NEW BLOCK! Send Job 0x30 (mining block height 850,001)
Time 95ms:   Receive nonce with Job ID 0x00 → Discard (old block)
Time 100ms:  Receive nonce with Job ID 0x30 → Valid for current block
```

### CRC Calculation
- **CRC5**: Used for command/response frames
  - Polynomial: 0x05
  - Init: 0x1F
  - Calculated over all bytes after preamble
- **CRC16**: Used for job packets only
  - Polynomial: 0x1021 (CRC-16-CCITT-FALSE)
  - Init: 0xFFFF
  - Calculated over all bytes after preamble, before CRC

### Version Rolling and Midstates

Version rolling allows ASICs to expand their search space beyond the 32-bit 
nonce range by modifying the block version field.

#### How Version Rolling Works

1. **Search Order**: The ASIC searches in this sequence:
   - First: All nonces in the chip's range, using current version
   - Then: Increment version and search all nonces again
   - Continues until all allowed version values are exhausted

2. **Version Rolling Control**:
   - Version rolling is enabled via register 0xA4 (VERSION_MASK)
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
   - With 16-bit version rolling: 2^32 × 2^16 = 2^48 hashes per job
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
   - Total search space: (nonces per chip) × (chips) × (version values)
   - Example: 1B nonces × 4 chips × 65K versions = 2^50 unique hashes

4. **Timing Considerations**:
   - All chips roll versions at different times
   - Faster chips may reach version 2 while others still on version 1
   - This is fine---no coordination needed between chips
   - Each chip's nonce+version combination remains unique

### Chip Summary

| Chip | Chip ID | Cores | Sub-cores | Job ID Bits | Used In |
|------|---------|-------|-----------|-------------|----------|
| BM1362 | 0x1362 | Unknown | Unknown | Unknown | Antminer S19 J Pro |
| BM1370 | 0x1370 | 80 | 16 | 4+4 | Bitaxe Gamma, S21 Pro |

