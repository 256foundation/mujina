# Windows Support for Mujina Miner

This document explains how mujina-miner runs on Windows, the changes made for Windows compatibility, and setup instructions.

## Overview

Mujina-miner now has native Windows support, allowing direct communication with BitAxe miners through Windows COM ports without requiring WSL2 or Docker. This was achieved by implementing Windows-specific USB device discovery and serial port handling.

## Architecture Changes

### 1. USB Device Discovery

**Original Implementation (Linux)**:
- Used `libudev` for real-time USB hotplug events
- Monitored `/sys/bus/usb/devices/*` filesystem

**Windows Implementation**:
- Uses `tokio_serial::available_ports()` for COM port enumeration
- Polls for device changes every 2 seconds
- Groups COM ports by USB device (VID:PID:SerialNumber)
- File: [mujina-miner/src/transport/usb/windows.rs](mujina-miner/src/transport/usb/windows.rs)

**Key Features**:
- Detects USB CDC ACM devices automatically
- Handles multi-port devices (BitAxe Gamma has 2 ports: control + data)
- Tracks device connection/disconnection events
- No external dependencies (uses built-in Windows APIs via tokio-serial)

### 2. Serial Port Implementation

**Original Implementation (Unix)**:
- Used Unix file descriptors with `AsyncFd`
- Direct access to `termios` for configuration

**Windows Implementation**:
- Uses `tokio_serial::SerialStream` (wraps Windows COM port APIs)
- Provides identical API surface for cross-platform compatibility
- File: [mujina-miner/src/transport/serial_windows.rs](mujina-miner/src/transport/serial_windows.rs)

**Key Features**:
- Async I/O using Tokio runtime
- Runtime baud rate reconfiguration
- Thread-safe with `RwLock` for shared access
- Statistics tracking (bytes read/written)

### 3. Signal Handling

**Changes in [mujina-miner/src/daemon.rs](mujina-miner/src/daemon.rs)**:
- Linux: Handles `SIGINT` and `SIGTERM`
- Windows: Handles `Ctrl+C` via `tokio::signal::ctrl_c()`

### 4. Board Pattern Matching

**Changes in [mujina-miner/src/board/bitaxe.rs](mujina-miner/src/board/bitaxe.rs)**:
- Changed from manufacturer/product string matching to VID:PID matching
- Windows reports generic "Microsoft" manufacturer for CDC ACM devices
- BitAxe devices identified by `VID:PID = c0de:cafe` (bitaxe-raw firmware)

### 5. Async Board Initialization

**Changes in [mujina-miner/src/board/bitaxe.rs](mujina-miner/src/board/bitaxe.rs)**:
- Made `BitaxeBoard::new()` async to support async serial port opening
- Prevents "runtime within runtime" errors when blocking on async operations

## Platform-Specific Code Structure

```
mujina-miner/src/transport/
├── usb.rs                    # Platform-agnostic USB transport
├── usb/
│   ├── linux.rs             # Linux udev implementation
│   ├── windows.rs           # Windows COM port implementation (NEW)
│   └── macos.rs             # macOS IOKit stub
├── serial.rs                # Unix serial implementation
└── serial_windows.rs        # Windows serial implementation (NEW)
```

Conditional compilation in [mujina-miner/src/transport/mod.rs](mujina-miner/src/transport/mod.rs):
```rust
#[cfg(not(target_os = "windows"))]
pub mod serial;

#[cfg(target_os = "windows")]
#[path = "serial_windows.rs"]
pub mod serial;
```

## Setup Instructions

### Prerequisites

1. **Rust Toolchain** (required)
   - Download and install from https://rustup.rs
   - Or use winget: `winget install Rustlang.Rustup`
   - Verify installation: `rustc --version`

2. **BitAxe Hardware** (with bitaxe-raw firmware)
   - Should appear as two COM ports in Device Manager
   - Example: COM11 (control) and COM7 (data)
   - VID:PID should be `c0de:cafe`

3. **Mining Pool** (optional for testing)
   - Can run in dummy/test mode without pool connection
   - For real mining, need Stratum v1 pool URL

### Building from Source

1. **Clone the repository** (if not already done):
   ```powershell
   git clone https://github.com/your-repo/mujina-miner.git
   cd mujina-miner
   ```

2. **Build the project**:
   ```powershell
   cargo build --release
   ```

   This will compile the Windows-specific code automatically based on your platform.

3. **Binary location**:
   ```
   target\release\mujina-minerd.exe
   ```

### Configuration

Create a `.env` file in the project root (optional):

```env
# Mining pool configuration (optional - omit for dummy/test mode)
MUJINA_POOL_URL=stratum+tcp://pool.example.com:3333
MUJINA_POOL_USER=your-bitcoin-address.worker-name
MUJINA_POOL_PASS=x

# Logging level (optional)
RUST_LOG=mujina_miner=info
```

For testing without a pool, simply omit `MUJINA_POOL_URL` and the miner will run in dummy mode.

### Running

**Basic run (dummy mode)**:
```powershell
.\target\release\mujina-minerd.exe
```

**With debug logging**:
```powershell
$env:RUST_LOG="mujina_miner=debug"
.\target\release\mujina-minerd.exe
```

**With trace logging** (very verbose):
```powershell
$env:RUST_LOG="mujina_miner=trace"
.\target\release\mujina-minerd.exe
```

### Expected Output

On successful startup, you should see:
```
INFO daemon: Using dummy job source (set MUJINA_POOL_URL to use Stratum v1)
INFO daemon: Started.
INFO transport::usb::windows: Starting Windows COM port USB discovery
DEBUG transport::usb::windows: Initial enumeration found 1 USB devices
DEBUG transport::usb::windows: Found USB device: c0de:cafe:12345678 with ports: ["COM11", "COM7"]
INFO board::bitaxe: BitAxe Gamma initialized successfully
INFO board::bitaxe: EMC2101 fan controller initialized (speed: 100%)
INFO board::bitaxe: TPS546 voltage regulator set to 1.15V
INFO board::bitaxe: BM1370 chip discovered at address 0
INFO scheduler: Hash thread registered
INFO api: API server listening. url=http://127.0.0.1:7785
```

## Device Detection

The miner automatically detects BitAxe devices by:

1. **USB Enumeration**: Scans all COM ports for USB serial devices
2. **VID:PID Matching**: Identifies BitAxe by `c0de:cafe` (bitaxe-raw firmware)
3. **Port Grouping**: Groups multiple COM ports belonging to the same USB device
4. **Board Creation**: Instantiates BitAxe Gamma board with control and data ports

No manual port specification is required - the miner finds devices automatically.

## Troubleshooting

### Device Not Detected

**Symptoms**: No "USB device connected" logs

**Solutions**:
1. Check Device Manager for COM ports with VID:PID `c0de:cafe`
2. Ensure bitaxe-raw firmware is installed (not stock ESP32 firmware)
3. Try unplugging and replugging the USB cable
4. Run with `RUST_LOG=mujina_miner=debug` to see enumeration details

### Access Denied on COM Ports

**Symptoms**: `Serial port error: Access is denied. (os error 5)`

**Solutions**:
1. Close any other applications using the COM ports (Arduino IDE, PuTTY, etc.)
2. Check if another mujina-minerd.exe instance is running
3. Restart the device and wait 5 seconds before running mujina

### Build Errors

**Symptoms**: `failed to remove file mujina-minerd.exe: Access is denied`

**Solutions**:
1. Stop any running mujina-minerd.exe processes
2. Close terminals/shells that may be locking the binary
3. Use Task Manager to end the process if needed

### Slow Hash Rate or No Shares

**Symptoms**: Hash rate shows `--` or very low values

**Solutions**:
1. Check fan is spinning (EMC2101 should report 100%)
2. Verify voltage regulator is set correctly (TPS546 should report ~1.15V)
3. Check ASIC chip detection logs (should see "BM1370 chip discovered")
4. Ensure sufficient USB power delivery (use powered USB hub if needed)

## API Access

The miner provides a REST API on `http://127.0.0.1:7785` for monitoring:

- `/api/stats` - Mining statistics (hash rate, shares, uptime)
- `/api/boards` - Connected board information
- `/api/pools` - Pool connection status

Access via browser, `curl`, or PowerShell:
```powershell
Invoke-WebRequest -Uri http://127.0.0.1:7785/api/stats | Select-Object -Expand Content
```

## Technical Details

### Thread Safety

Windows serial implementation uses:
- `RwLock<TokioSerialStream>` for synchronized access
- Atomic types for statistics counters
- Safe interior mutability pattern

Marked as `Send + Sync` with safety justification:
```rust
// SAFETY: SerialInner is Send because:
// - TokioSerialStream is always accessed through RwLock
// - All atomic fields are inherently Send
unsafe impl Send for SerialInner {}

// SAFETY: SerialInner is Sync because:
// - All access to TokioSerialStream goes through RwLock
// - Atomic fields are inherently Sync
unsafe impl Sync for SerialInner {}
```

### Polling vs. Hotplug Events

Unlike Linux's real-time udev events, Windows implementation uses polling (2-second interval). This is acceptable because:
- USB device connection/disconnection is infrequent
- 2-second detection latency is negligible for mining operations
- Simpler implementation without Windows API hooks
- Lower CPU overhead than continuous monitoring

### COM Port Path Format

Windows uses `COMx` format (e.g., `COM11`, `COM7`), while Linux uses `/dev/ttyACMx`. The serial implementation abstracts this difference, but internally:
- Windows paths are passed directly to `tokio_serial`
- Extended format `\\.\COMx` used automatically for ports > COM9

## Contributing

When making changes that affect Windows support:

1. Test on Windows with actual BitAxe hardware
2. Ensure conditional compilation remains correct (`#[cfg(target_os = "windows")]`)
3. Maintain API parity between Unix and Windows implementations
4. Update this documentation if adding Windows-specific features

## Known Limitations

1. **Polling-based detection**: 2-second latency for new device detection (vs. instant on Linux)
2. **No Windows Device Manager integration**: Changes in Device Manager require application restart
3. **COM port numbering**: High COM port numbers (>256) may require special handling

## Files Modified for Windows Support

Core changes:
- [mujina-miner/src/transport/usb/windows.rs](mujina-miner/src/transport/usb/windows.rs) - Windows USB discovery (NEW)
- [mujina-miner/src/transport/serial_windows.rs](mujina-miner/src/transport/serial_windows.rs) - Windows serial implementation (NEW)
- [mujina-miner/src/transport/usb.rs](mujina-miner/src/transport/usb.rs) - Platform abstraction and Clone impl
- [mujina-miner/src/transport/mod.rs](mujina-miner/src/transport/mod.rs) - Conditional compilation
- [mujina-miner/src/board/bitaxe.rs](mujina-miner/src/board/bitaxe.rs) - VID:PID matching, async init
- [mujina-miner/src/daemon.rs](mujina-miner/src/daemon.rs) - Windows signal handling

## License

Same as the main project.
