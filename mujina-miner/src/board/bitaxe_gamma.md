# Bitaxe Gamma Board Support

This document describes the Bitaxe Gamma board architecture and how to
interface with it from a software perspective.

## Overview

The Bitaxe Gamma is an open-source Bitcoin mining board featuring:
- Single BM1370 ASIC chip (from Antminer S21 Pro)
- ESP32-S3 microcontroller running bitaxe-raw firmware
- USB CDC ACM serial interface (dual port)
- Integrated power management and thermal control

## Communication Architecture

### USB Serial Ports
The board presents two USB CDC ACM serial ports:
- `/dev/ttyACM0` - Control channel (board management)
- `/dev/ttyACM1` - Data channel (ASIC communication)

Both ports operate at 115200 baud.

### Control Protocol (bitaxe-raw)

The control channel uses a custom packet-based protocol for managing board
peripherals. This protocol tunnels I2C, GPIO, and ADC operations over USB.

#### Packet Format
```
Request:  [Length:2 LE] [ID:1] [Bus:1] [Page:1] [Command:1] [Data:N]
Response: [Length:2 LE] [ID:1] [Data:N]
```

**Important**: The length field in responses contains only the data size, not
the total packet size. Total packet size = length + 3.

#### Bus Types
- `0x00` - GPIO operations
- `0x01` - I2C operations  
- `0x02` - ADC operations

#### GPIO Page Commands
The command byte for GPIO operations is the pin number itself.
- Write: `[pin_number] [0x00 or 0x01]` (low/high)
- Read: `[pin_number]` → Response: `[0x00 or 0x01]`

#### I2C Operations
Standard I2C operations with 7-bit addressing:
- Write: `[i2c_addr] [reg_addr] [data...]`
- Read: `[i2c_addr] [reg_addr] [read_len]` → Response: `[data...]`
- Write-Read: `[i2c_addr] [write_data...] [read_len]` → Response: `[data...]`

## Hardware Components

### BM1370 ASIC
- Single chip configuration at address 0
- Connected via `/dev/ttyACM1` (data channel)
- 1280 cores capable of ~640 GH/s at 500 MHz
- Supports version rolling (mask 0xFFFF)
- Reset controlled via GPIO 0 (active low)

### Power Management - TPS546D24A
- I2C address: `0x24`
- Texas Instruments digital buck converter
- PMBus protocol for control and monitoring
- Functions:
  - Core voltage adjustment for ASIC power
  - Input voltage monitoring (5V USB power)
  - Output current monitoring
  - Temperature monitoring
  - Fault protection (OV, OC, OT)
- Configured for Bitaxe Gamma's single BM1370 ASIC requirements
- See `Tps546Config::bitaxe_gamma()` in the code for specific values

### Thermal Management - EMC2101
- I2C address: `0x4C`
- Product ID: `0x28` (EMC2101-R variant with EEPROM support)
- PWM fan controller with integrated temperature sensor
- External temperature diode connected to ASIC
- TACH input for RPM monitoring (5.6kΩ pull-up)
- Requires TACH input enabled in CONFIG register
- See EMC2101 driver implementation for RPM calculation details

### Reset Control
- GPIO 0: ASIC reset (active low)
  - Assert low for 100ms to reset
  - Must be high for normal operation
  - Used during initialization and shutdown

## Initialization Sequence

1. Open both serial ports at 115200 baud
2. Initialize control channel for packet communication
3. Release ASIC from reset (GPIO 0 = high)
4. Initialize EMC2101 fan controller
   - Enable TACH input (CONFIG register bit 2)
   - Set initial fan speed (e.g., 50%)
5. Enable version rolling on ASIC
6. Discover chips on data channel
7. Initialize TPS546D24A for core voltage control
8. Start monitoring tasks (temperature, fan, power)

## Operational Considerations

### Thermal Management
- Monitor ASIC temperature via EMC2101 external sensor
- Adjust fan speed based on temperature
- Typical operating range: 50-85°C

### Power Management
- Monitor input voltage (5V nominal)
- Monitor core voltage and current via TPS546
- Adjust core voltage for efficiency optimization
- Typical power consumption: 15-25W

### Reset and Shutdown
- Always hold ASIC in reset during shutdown
- Send chain inactive command before reset
- Allow time for capacitors to discharge

## Protocol Quirks and Workarounds

1. **Packet Length Field**: The bitaxe-raw firmware uses a non-standard length
   field that only includes data bytes, not the ID byte.

2. **TACH Configuration**: The EMC2101 requires explicit TACH enable via CONFIG
   register, which is not always documented clearly.

3. **Error Responses**: Protocol errors return specific error codes:
   - `0x00` - Success
   - `0x01` - Wrong bus
   - `0x02` - Wrong page  
   - `0x03` - Wrong ID
   - `0x04` - Packet overflow
   - `0x05` - Unknown error

## Testing and Debugging

- Use `RUST_LOG=mujina_miner=trace` to see all serial communication
- Monitor board statistics logged every 30 seconds
- Check TACH reading for fan operation verification
- Verify temperature readings are reasonable (not 0°C or >100°C)

## References

- [Bitaxe Gamma Hardware](https://github.com/bitaxeorg/bitaxeGamma)
- [bitaxe-raw Firmware](https://github.com/skot/bitaxe-raw)
- [BM1370 Protocol Documentation](../../../asic/bm13xx/PROTOCOL.md)

## Components

- EMC2101 - Fan controller and temperature sensor
- TPS546D24A - Power management controller
