# Bitaxe-Raw Control Protocol

This document describes our understanding of the bitaxe-raw control protocol
based on source code analysis and runtime observations.

## Overview

The bitaxe-raw firmware provides a packet-based control protocol over USB CDC
ACM serial for managing board peripherals (GPIO, I2C, ADC). Communication is
request-response based with packet framing.

## Packet Format

### Request (Host → Device)

```
+--------+--------+--------+--------+--------+--------+----------+
| Length | ID     | Bus    | Page   | Command| Data             |
| (2B LE)| (1B)   | (1B)   | (1B)   | (1B)   | (variable)       |
+--------+--------+--------+--------+--------+--------+----------+
```

- **Length**: Total packet size including this field (little-endian u16)
- **ID**: Packet identifier, echoed in response (0-255)
- **Bus**: Always 0x00 in current implementation
- **Page**: Command category (0x05=I2C, 0x06=GPIO, 0x07=ADC)
- **Command**: Page-specific command byte
- **Data**: Command-specific payload (practically limited by 4KB USB buffer)

### Response (Device → Host)

```
+--------+--------+----------+
| Length | ID     | Data     |
| (2B LE)| (1B)   | (0-256B) |
+--------+--------+----------+
```

- **Length**: Size of data field only, NOT including length or ID fields
- **ID**: Echo of request packet ID
- **Data**: Response payload (max 256 bytes) or error indication

### Error Response

```
+--------+--------+--------+--------+----------+
| Length | ID     | 0xFF   | Error  | Message  |
| (2B LE)| (1B)   | (1B)   | (1B)   | (optional) |
+--------+--------+--------+--------+----------+
```

- **0xFF**: Error marker (always present in error responses)
- **Error**: Error code (0x10=Timeout, 0x11=Invalid, 0x12=Overflow, 0xFF=Custom)
- **Message**: Error description string (only present when Error=0xFF, length > 2 indicates message bytes follow)

## GPIO Commands (Page 0x06)

For GPIO operations, the command byte represents the pin number.

### Set GPIO
- Command: Pin number (e.g., 0x00 for pin 0)
- Data: [level] where level is 0x00 (low) or 0x01 (high)
- Response: [level] echo of the set level

### Get GPIO
- Command: Pin number
- Data: Empty
- Response: [level] current pin level

## Important Notes

1. The length field in responses contains ONLY the data payload size, not the
   total packet size. This differs from some protocol documentation that
   suggests the length includes itself.

2. Minimum response packet is 3 bytes (2 length + 1 ID) even for empty
   responses.

3. The protocol uses little-endian byte ordering for multi-byte values.

4. GPIO pin 0 is used for ASIC reset control on Bitaxe boards (active low).