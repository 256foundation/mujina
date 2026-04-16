# Mujina Miner Architecture

This document is an architectural overview of mujina-miner, Bitcoin
mining software written in Rust. It stays at the high level; for
specific files, types, and APIs, read the code.

## Overview

The miner is three subsystems working together: **Boards** do the
hashing and keep the hardware configured, powered, cooled, and
monitored; **the Mining core** schedules work from sources to chips
according to policy; and **the API** exposes telemetry and commands to
external clients.

```
     USB & hotplug                         HTTP clients
           |                                     |
           v                                     v
     +-----------+  telemetry & commands   +-----------+
     |  Boards   |<----------------------->|    API    |
     +-----------+                         +-----------+
            \                                   /
         HashThreads                telemetry & commands
                   \                     /
                    v                   v
                   +---------------------+
                   |     Mining core     |
                   +---------------------+
                              ^
                              |
                         Job sources
                        (Stratum, etc.)
```

The **backplane** glues Boards to everything else. It watches
transport events (USB hotplug today), spawns and tears down boards as
they come and go, hands their HashThreads to the scheduler, and
registers their telemetry streams with the API.

The subsystems don't call each other directly; they talk through
channels. When a board connects, the backplane registers its telemetry
stream with the API. The scheduler publishes a miner-level stream
covering every registered job source. The API reads both streams to
serve HTTP clients. Commands flow the other way, through a channel
into the scheduler. Neither the backplane nor the scheduler knows the
API exists.

The remaining sections cover each subsystem. **Boards** covers how the
hardware is organized and how drivers reach it. **Mining core** covers
how work and shares flow and how the scheduler mediates. **API**
covers what's exposed and how clients reach it. **Example
compositions** shows how the implemented boards fit it all together.

## Boards

```
                Board
                  |
          +-------+--------+
          |                |
          v                v
        ASICs        Peripherals
                     & mgmt-MCU services
          |                |
          |          hw_traits
          |          (I2C, GPIO, RGB, ...)
          |                |
          |          Mgmt protocol
          |                |
          +-------+--------+
                  |
                  v
              Transport
```

**Peripherals** are the non-ASIC chips on a board: fan controllers,
temperature sensors, power regulators. Their drivers are written
against hardware-interface traits, mainly I2C. The same driver can run
over a native Linux I2C bus (if the chip hangs off the host), an
adapter that tunnels I2C over the board's management protocol, or a
mock in tests. The EMC2101 fan controller driver runs unchanged on any
board that has one.

**Mgmt-MCU services** live on the board's management MCU itself, not
on separate chips: GPIO pins, status LEDs, ADC channels. Their drivers
still use hw_traits for testability, but the trait implementations
come from the management protocol rather than from a chip-specific
driver.

**ASICs** don't go through hw_traits. Each chip family speaks its wire
protocol directly over serial bytes. A BM1370 driver exists to produce
BM1370 frames; there's no useful "swap the I/O" story that would
justify an abstraction, just needless indirection. The abstraction
that matters for ASICs is higher up: every mining worker exposes a
`HashThread` interface to the scheduler, regardless of chip family.

**Management protocols** let a board's MCU multiplex its operations
(GPIO, LED, I2C passthrough) onto one serial link. They also supply
the hw_trait implementations that peripheral and mgmt-MCU service
drivers use.

**Transport** carries raw bytes (USB serial today) and knows nothing
about protocols.

Each board composes the above for its specific hardware and owns the
lifecycle of what it builds.

## Mining core

```
  Job source  -->  Scheduler  -->  HashThread  -->  Chip(s)
                       ^                |
                       +---- shares ----+
```

**Job sources** produce mining work. A source might be a Stratum v1
pool, a synthetic generator for testing, or anything else that
supplies jobs. Multiple sources can run at once; the scheduler
mediates.

**The scheduler** is the hub. It pulls work from registered sources,
fans it out to hashing workers, collects the shares that come back,
filters them against the pool target, and sends the pool-worthy ones
to the originating source.

**HashThreads** are how the scheduler sees hashing workers. A board
exposes one or more of them: a single-chip Bitaxe exposes one, a
multi-chip chain exposes one per chain, the CPU backend exposes one
per configured thread. Whatever shape a board takes internally, the
scheduler sees a uniform interface for handing out work and receiving
shares.

**Chips** do the actual SHA-256 hashing, beneath the HashThread.

### Share filtering

Shares are filtered in three stages on the way back:

1. **Chip hardware target**: each chip is configured with a low
   difficulty target so it produces frequent shares for health
   monitoring, typically one per second per chip.
2. **Thread processing**: the HashThread hashes each reported share
   to determine what difficulty it actually meets, then forwards every
   share to the scheduler. Since the hash has to be computed to know
   the difficulty, filtering in the thread wouldn't save any work.
   Forwarding everything also gives the scheduler ground truth for
   per-thread hashrate.
3. **Scheduler filtering**: the scheduler checks each share against
   the pool target and sends only the pool-worthy ones to the source.
   Sub-pool shares still feed internal statistics and health
   monitoring.

## API

```
     CLI & other clients
             |
             v  (HTTP REST)
         API server
             |
             +<--> scheduler
             +<--> board 1
             +<--> board 2
             +<--> ...
  (each channel: telemetry out, commands in)
```

The daemon exposes its state and a growing command surface over a REST
API. Talking to the daemon from outside goes through this API;
`mujina-cli` uses it today, but anything speaking HTTP can.

The API reads from the telemetry channels described in the overview
and dispatches commands to internal targets. There's no single command
channel; each addressable component gets its own. The scheduler has
one for pause/resume today, with richer work-distribution commands to
come. Each board has one for actions like voltage or fan control.
Managing job sources at runtime (adding, removing, reconfiguring them)
will flow through its own channel too. New capabilities get added as
new channels, not as variants on a monolithic command enum.

The API publishes an OpenAPI schema and serves a Swagger UI, so the
surface is discoverable at runtime. The `/api/v0` prefix signals that
the contract isn't stable yet.

A WebSocket channel for streaming telemetry is on deck, so UIs can
reflect live changes without polling.

The server binds to localhost by default. There's no authentication,
so binding to a public address would be unsafe; the daemon logs a
warning when started with a non-loopback bind.

## Example compositions

Three boards are implemented today, each using the abstractions
differently.

**Bitaxe Gamma**, a single-chip board:
- ASICs: 1 BM1370 (BM13xx family)
- HashThreads exposed: 1
- Peripherals: EMC2101 fan controller, TPS546 power regulator
- Management channel: bitaxe-raw over USB serial

**EmberOne00**, a multi-chip chain:
- ASICs: 12 BM1362 (BM13xx family)
- HashThreads exposed: 1 (one thread drives the chain)
- Peripherals: TMP1075 / TMP451 temperature sensors, RGB status LED
- Management channel: bitaxe-raw over USB serial

**CPU backend**, a virtual board:
- ASICs: none; software SHA-256 instead
- HashThreads exposed: N (configurable)
- Peripherals: none
- Management channel: none

All three slot into the same scheduler, API, and job sources. That's
the point of the abstractions above.
