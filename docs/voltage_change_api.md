# Voltage Change API

Allows runtime adjustment of a board's core voltage via the REST API without
restarting the miner.

## Endpoint

```
PATCH /api/v0/boards/{name}
Content-Type: application/json
```

### Path parameter

| Parameter | Description |
|-----------|-------------|
| `name` | Board name as returned by `GET /api/v0/boards` (e.g. `bitaxe-71bfd369`) |

### Request body

```json
{
  "powers": [
    { "name": "core", "voltage_v": 1.134 }
  ]
}
```

| Field | Type | Description |
|-------|------|-------------|
| `powers` | array, optional | List of power-domain patches to apply |
| `powers[].name` | string | Power domain name. Currently only `"core"` is supported on Bitaxe boards. |
| `powers[].voltage_v` | float, optional | Target output voltage in volts. Omit the field to leave the domain unchanged. |

Multiple domains can be patched in a single request; they are applied in array
order and the handler aborts on the first error.

### Response

On success (`200 OK`) the current `BoardState` snapshot is returned. Because
the stats monitor reads back voltage every 5 seconds, the `powers[].voltage_v`
field in the response may still show the previous reading — issue a `GET`
after the next stats interval to confirm the new value.

```json
{
  "name": "bitaxe-71bfd369",
  "model": "Bitaxe Gamma",
  "powers": [
    { "name": "input", "voltage_v": 5.003 },
    { "name": "core",  "voltage_v": 1.131, "current_a": 8.2, "power_w": 9.3 }
  ],
  ...
}
```

### Error responses

| Status | Cause |
|--------|-------|
| `404 Not Found` | No connected board with the given name |
| `422 Unprocessable Entity` | Unknown power domain name, or voltage outside the hardware-enforced range, or board type does not support voltage control |
| `500 Internal Server Error` | Command channel closed, or 5-second hardware timeout |

## Voltage range (Bitaxe Gamma / BM1370)

Voltage limits are enforced by the TPS546D24A driver and cannot be overridden
at runtime. Requests outside this window are rejected before any I2C write
occurs.

| Parameter | Value |
|-----------|-------|
| Minimum (`vout_min`) | **1.0 V** |
| Maximum (`vout_max`) | **2.0 V** |
| Default at boot (`vout_command`) | **1.15 V** |

The limits are defined in `mujina-miner/src/board/bitaxe.rs`
`init_power_controller()` as part of the `Tps546Config` struct.  They are
not configurable at runtime or via a config file.

## Board support

| Board | Voltage control |
|-------|----------------|
| Bitaxe Gamma | Supported (`"core"` domain) |
| CPU Miner | Not supported → `422` |
| EmberOne | Not supported → `422` |

## Example usage

```bash
# Set core voltage to 1.134 V
curl -s -X PATCH http://127.0.0.1:7785/api/v0/boards/bitaxe-71bfd369 \
  -H 'Content-Type: application/json' \
  -d '{"powers":[{"name":"core","voltage_v":1.134}]}' | jq .

# Confirm (wait up to 5 s for stats monitor to refresh)
curl -s http://127.0.0.1:7785/api/v0/boards/bitaxe-71bfd369 | jq .powers

# Unknown domain → 422
curl -s -o /dev/null -w '%{http_code}' -X PATCH \
  http://127.0.0.1:7785/api/v0/boards/bitaxe-71bfd369 \
  -H 'Content-Type: application/json' \
  -d '{"powers":[{"name":"input","voltage_v":5.0}]}'

# Out-of-range voltage → 422
curl -s -o /dev/null -w '%{http_code}' -X PATCH \
  http://127.0.0.1:7785/api/v0/boards/bitaxe-71bfd369 \
  -H 'Content-Type: application/json' \
  -d '{"powers":[{"name":"core","voltage_v":9.9}]}'
```

## Implementation overview

The feature is wired through seven files:

### `mujina-miner/src/api_client/types.rs`
Added `PowerPatch` and `BoardPatchRequest` — the serde/OpenAPI types that
define the request body shape.

### `mujina-miner/src/api/commands.rs`
Added `BoardCommand::SetVoltage { domain, voltage_v, reply }` to the existing
board command enum.  The `reply` field is a oneshot channel so the API handler
can await the hardware result synchronously.

### `mujina-miner/src/board/mod.rs`
Added `cmd_tx: Option<mpsc::Sender<BoardCommand>>` to `BoardRegistration`.
Boards that support runtime commands populate this field; boards that do not
(CPU miner, EmberOne) set it to `None`.

### `mujina-miner/src/api/registry.rs`
Added two methods to `BoardRegistry`:
- `find_cmd_tx(name)` — returns the command sender for a named board, or
  `None` if not found or unsupported.
- `contains(name)` — checks whether a board is connected, independent of
  command support (used to distinguish 404 from 422).

### `mujina-miner/src/api/v0.rs`
Added `patch_board` handler (`PATCH /boards/{name}`) and registered it
alongside the existing `get_board` route.  The handler:
1. Locks the registry briefly to resolve the board's command sender.
2. Returns `404` if the board is absent, `422` if `cmd_tx` is `None`.
3. Sends one `SetVoltage` command per `powers` entry, awaiting each reply
   with a 5-second timeout.
4. Maps `Err` replies from the board to `422`; timeout/channel errors to `500`.
5. Returns the current `BoardState` snapshot.

### `mujina-miner/src/board/bitaxe.rs`
Three changes:

1. **`BitaxeBoard` struct** — added `cmd_rx: Option<mpsc::Receiver<BoardCommand>>`.

2. **`create_from_usb` factory** — creates the `mpsc::channel`, stores
   `cmd_tx` in `BoardRegistration` and `cmd_rx` in the board struct.

3. **`spawn_stats_monitor`** — takes ownership of `cmd_rx` alongside the
   existing `state_tx`.  The monitoring loop now uses `tokio::select!` to
   interleave the 5-second telemetry tick with incoming commands:

   ```rust
   tokio::select! {
       _ = interval.tick() => { /* read sensors, publish BoardState */ }
       Some(cmd) = cmd_rx.recv() => { handle_board_command(cmd, &regulator).await; }
   }
   ```

   **Why `tokio::select!` is necessary.**  The previous loop called
   `interval.tick().await` unconditionally, which suspends the task for up
   to 5 seconds.  While suspended the task cannot read `cmd_rx`; any
   incoming command queues in the channel buffer and is ignored until the
   next tick fires.  In the worst case — a command arrives just after a tick
   — the board sleeps for ~5 s, executes the I2C write, then replies, but
   the API handler's own 5-second timeout has already expired and the caller
   receives a `500` even though the hardware operation succeeded.

   `tokio::select!` eliminates that window by racing both futures
   concurrently.  A command that arrives mid-interval is processed
   immediately (typically within a few hundred milliseconds of I2C
   round-trip time) without disturbing the telemetry cadence — the interval
   timer continues counting down and fires normally on schedule.

   `handle_board_command` validates the domain name (`"core"` only), then
   calls `regulator.lock().await.set_vout(voltage_v)` and sends the result
   back on the reply channel.  The actual range validation (`1.0 V – 2.0 V`)
   happens inside `Tps546::set_vout` in
   `mujina-miner/src/peripheral/tps546.rs`.

### `mujina-miner/src/api/server.rs`
One-line test fixture update: `BoardRegistration { state_rx, cmd_tx: None }`
to match the new struct shape.
