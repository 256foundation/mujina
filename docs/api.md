# REST API

Mujina exposes an HTTP API on port 7785 (ASCII "MU") for
monitoring and control. It binds to localhost by default. Set
`MUJINA_API_LISTEN` to override the listen address:

```bash
MUJINA_API_LISTEN="0.0.0.0" cargo run
```

The port defaults to 7785 if not specified, or you can
override it with `MUJINA_API_LISTEN="0.0.0.0:9000"`.

The API currently has no authentication or encryption, so
binding to a non-localhost address exposes it to the network
without access control. These will be addressed later.

An OpenAPI spec is served at `/api/v0/openapi.json`. A Swagger
UI is available at `/swagger-ui` for interactive browsing.

## Versioning

All endpoints live under `/api/v0/`. The v0 prefix signals an
unstable API: breaking changes are expected until Mujina
matures. Once the API stabilizes it will move to v1, and
backwards-incompatible changes will increment the version
number from there.

## Data model

The API models the miner as a single state tree. `GET /miner`
returns the complete snapshot; endpoints like `/boards` and
`/boards/{name}` return subtrees of the same structure.
Writable fields are typically updated with PATCH, which applies
a partial update to the relevant part of the tree.

## Conventions

### Null values

Sensor readings (`rpm`, `temperature_c`, `voltage_v`, etc.) are
nullable. A null value means the hardware read failed or the
sensor is not present. Clients should treat null as "unknown,"
not zero.

Target fields (`target_percent`) are also nullable. Null means
no override has been set through the API; the miner is managing
the value on its own (e.g. a startup default or a control
algorithm). Once a client sets a value, the field reflects that
value until changed again.

### Units

All values are in raw SI-ish units. Clients are responsible for
formatting and unit conversion.

| Field suffix          | Unit                   |
|-----------------------|------------------------|
| `_secs`               | seconds                |
| `_c`                  | degrees Celsius        |
| `_v`                  | volts                  |
| `_a`                  | amperes                |
| `_w`                  | watts                  |
| `rpm`                 | revolutions per minute |
| `hashrate`            | hashes per second      |
| `joules_per_terahash` | joules per terahash    |

Percentage fields (`percent`, `target_percent`) are integers
0--100.

### Efficiency

A board reports `efficiency` as one entry per power domain, each
holding several lookback windows. There is no single J/TH for a
miner: `asic` is what the silicon consumes, `board` adds the
housekeeping load the board draws alongside it, and `wall` adds
the power supply's conversion loss. The differences between them
are the useful diagnostics, so a client should say which domain
it is showing rather than picking one and calling it "the"
efficiency.

An absent window means the miner has not run long enough to fill
it, not that the value is zero or unknown. Windows appear as they
are earned, shortest first, so a board reports its five-minute
figure long before its 72-hour one. A window that is present but
whose domain is missing entirely means that domain has no sensor.

Each window carries a `provenance` of `measured` or `estimated`.
`estimated` means at least one power sample in that window was
modelled or apportioned rather than metered. Display it freely;
do not close a control loop on it.

The `hashrate` on a window is the denominator that J/TH was
computed against, over the same span. It is reported so the
figure can be judged rather than taken on trust — a short-window
J/TH rests on a hashrate that share variance still dominates.

### Naming

Boards and sources are identified by a URL-friendly `name` field
(e.g. `bitaxe-e2f56f9b`). These names appear in URL paths for
single-resource endpoints like `/api/v0/boards/{name}`.

## Endpoints

The OpenAPI spec is the authoritative endpoint reference. This
table is a summary and may not be exhaustive.

### Miner (aggregate)

| Method | Path         | Description                    |
|--------|--------------|--------------------------------|
| GET    | `/miner`     | Full state snapshot            |
| PATCH  | `/miner`     | Update miner config (e.g. pause) |

### Boards

| Method | Path              | Description           |
|--------|-------------------|-----------------------|
| GET    | `/boards`         | List connected boards |
| GET    | `/boards/{name}`  | Single board detail   |

### Sources

| Method | Path              | Description          |
|--------|-------------------|----------------------|
| GET    | `/sources`        | List job sources     |
| GET    | `/sources/{name}` | Single source detail |

### Health

| Method | Path      | Description          |
|--------|-----------|----------------------|
| GET    | `/health` | Returns "OK"         |

All paths are relative to `/api/v0`.

## Types

The request and response types are defined in Rust in the
`api_client::types` module
(`mujina-miner/src/api_client/types.rs`). These types are the
shared contract between the server and its clients (CLI, TUI).
The OpenAPI schema is derived from them automatically.
