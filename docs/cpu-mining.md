# CPU Mining

mujina-miner can hash using your CPU instead of ASIC hardware. This is far too
slow for real mining---a few MH/s versus TH/s for ASICs---but it lets you work
with the full mining stack when you don't have hardware connected.

Use cases:

- **Development**: Test code changes without physical boards
- **Pool testing**: Verify connectivity and share submission
- **Scale testing**: Spin up many instances to stress-test infrastructure
- **Learning**: Understand mining workflows before investing in hardware

## Enabling CPU Mining

Set `MUJINA_CPUMINER_THREADS` to enable CPU mining. The value specifies how
many parallel hashing threads to run:

```bash
MUJINA_CPUMINER_THREADS=2 cargo run
```

Without this variable, the miner only looks for USB-connected ASIC hardware.

When running CPU-only, also set `MUJINA_USB_DISABLE=1` to skip USB device
discovery. This ignores any real mining boards you might have connected---they
run at vastly different hashrates and would complicate testing. It also avoids
USB-related noise on cloud systems:

```bash
MUJINA_CPUMINER_THREADS=2 \
MUJINA_USB_DISABLE=1 \
cargo run
```

## Controlling CPU Usage

By default, each mining thread hashes for 50ms then sleeps for 50ms---a 50%
duty cycle. This prevents CPU mining from starving other processes and avoids
tripping CPU usage limits on cloud instances.

Adjust with `MUJINA_CPUMINER_DUTY`:

- `100` --- Full speed, no throttling
- `50` --- Hash half the time, sleep half (default)
- `10` --- Light load, mostly idle

For testing, lower duty cycles work fine since you're not trying to maximize
hashrate.

## Testing Share Submission

Pools set share difficulty for ASIC-speed miners. A CPU running at MH/s instead
of TH/s would wait days or weeks to find a share at typical pool difficulty. To
test the share submission flow, use `MUJINA_POOL_FORCED_RATE` to artificially
lower the target:

```bash
MUJINA_CPUMINER_THREADS=2 \
MUJINA_USB_DISABLE=1 \
MUJINA_POOL_FORCED_RATE=6 \
MUJINA_POOL_URL="stratum+tcp://pool.example.com:3333" \
MUJINA_POOL_USER="your-address.worker" \
cargo run
```

The value is target shares per minute. With `MUJINA_POOL_FORCED_RATE=6`, the
miner targets one share every 10 seconds.

The forced rate wrapper intercepts jobs from the pool and replaces the share
target with one computed to achieve your target rate at current hashrate. The
pool still sets real difficulty for submitted shares; this just controls how
often your miner finds candidates to submit.

Shares found this way will likely be rejected by the pool as below-difficulty.
That's fine---you're testing connectivity and submission flow, not earning
rewards.

## Running Without a Pool

Without `MUJINA_POOL_URL`, the miner uses a dummy job source that generates
synthetic work:

```bash
MUJINA_CPUMINER_THREADS=2 \
MUJINA_USB_DISABLE=1 \
RUST_LOG=mujina_miner=debug \
cargo run
```

This exercises the full mining pipeline---job distribution, hashing, share
detection---without any external dependencies.

## Running in a Container

For deploying to cloud infrastructure or container orchestration platforms, see
[Container Image](container.md).

## Environment Reference

| Variable | Description |
|----------|-------------|
| `MUJINA_CPUMINER_THREADS` | Number of mining threads; presence enables CPU mining |
| `MUJINA_CPUMINER_DUTY` | Duty cycle percentage, 1-100 (default: 50) |
| `MUJINA_USB_DISABLE` | Set to `1` to skip USB device discovery |
| `MUJINA_POOL_FORCED_RATE` | Target share rate in shares/min |