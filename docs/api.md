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

| Field suffix | Unit                   |
|--------------|------------------------|
| `_secs`      | seconds                |
| `_c`         | degrees Celsius        |
| `_v`         | volts                  |
| `_a`         | amperes                |
| `_w`         | watts                  |
| `rpm`        | revolutions per minute |
| `hashrate`   | hashes per second      |

Percentage fields (`percent`, `target_percent`) are integers
0--100.

### Naming

Boards and sources are identified by a URL-friendly `name`
field (e.g. `bitaxe-e2f56f9b`). These names appear in URL paths
for single-resource endpoints like `/api/v0/boards/{name}`.

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

### Config

| Method | Path      | Description             |
|--------|-----------|--------------------------|
| GET    | `/config` | Full configuration tree |

Includes each pool's password in plaintext -- there is no
redaction. The API has no authentication, so anyone who can
reach it can read it.

### Sources

| Method | Path              | Description                  |
|--------|-------------------|-------------------------------|
| GET    | `/sources`        | List configured job sources  |
| GET    | `/sources/{name}` | Single source detail         |

Same data as the `sources` field of `/config`, addressable by
`name`. Each entry carries a `kind` field identifying what kind
of source it is; `"stratum_v1"` is the only kind today, with
fields matching a Stratum pool (`url`, `username`, `password`).
"Source" is the term the rest of the codebase already uses for
this concept, and it covers more than pool connections -- the
dummy source used when none is configured, and, over time,
other protocols (Stratum v2 and beyond). `kind` exists so a
client can tell sources apart, and so adding a source kind
later is additive rather than a breaking change to this
endpoint.

A source's `name` is assigned automatically today (`a`, `b`,
`c`, ... by position) since there's no way to name one
explicitly yet.

### Health

| Method | Path      | Description          |
|--------|-----------|----------------------|
| GET    | `/health` | Returns "OK"         |

All paths are relative to `/api/v0`.

## Types

Most request and response types are defined in Rust in the
`api_client::types` module
(`mujina-miner/src/api_client/types.rs`). These types are the
shared contract between the server and its clients (CLI, TUI).
The OpenAPI schema is derived from them automatically.

`/config` and `/sources` are the exception: they serialize
`config::Config`, `config::SourceConfig`
(`mujina-miner/src/config.rs`), and
`stratum_v1::StratumV1PoolConfig`
(`mujina-miner/src/stratum_v1/client.rs`) directly, rather than a
separate view type.
