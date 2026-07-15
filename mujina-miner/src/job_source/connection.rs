//! Shared connection-lifecycle utilities for pool job sources.
//!
//! Both the Stratum V1 and V2 job sources share the same connection-lifecycle
//! primitives.  Keeping them here ensures the behaviour stays in sync and
//! avoids copy-paste drift.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Outcome of a single connection attempt, returned by each source's internal
/// `connect_and_run` method.
pub(super) enum ConnectOutcome {
    /// Graceful shutdown requested via cancellation token.
    Shutdown,
    /// Connection lost; caller should retry after back-off.
    Disconnected,
    /// Unrecoverable error (e.g. auth failure, bad config); stop retrying.
    Fatal(anyhow::Error),
}

/// Jittered exponential back-off for reconnection timing.
///
/// Starts at `initial` and doubles after each call to [`next_delay`],
/// capping at `max`.  Each returned delay is scaled to [0.5, 1.0] of the
/// nominal value to spread out reconnection attempts and avoid
/// thundering-herd pile-ups against a recovering pool.
///
/// [`next_delay`]: ExponentialBackoff::next_delay
pub(super) struct ExponentialBackoff {
    current: Duration,
    initial: Duration,
    max: Duration,
    // Per-process jitter seed.  `RandomState` is seeded from OS randomness
    // at construction, so different processes produce different jitter even
    // when reconnecting at the same wall-clock instant — the same approach
    // tokio uses internally for jittered timeouts.
    jitter_state: RandomState,
    jitter_step: u64,
}

impl ExponentialBackoff {
    pub(super) fn new(initial: Duration, max: Duration) -> Self {
        Self {
            current: initial,
            initial,
            max,
            jitter_state: RandomState::new(),
            jitter_step: 0,
        }
    }

    /// Return the next back-off delay (with jitter) and advance the state.
    ///
    /// The nominal delay (1 s, 2 s, 4 s, …) is scaled by a jitter factor in
    /// [0.5, 1.0] to spread reconnection attempts across concurrent miners.
    pub(super) fn next_delay(&mut self) -> Duration {
        let nominal = self.current;
        self.current = (self.current * 2).min(self.max);

        let mut hasher = self.jitter_state.build_hasher();
        hasher.write_u64(self.jitter_step);
        self.jitter_step = self.jitter_step.wrapping_add(1);
        let hash = hasher.finish();
        let jitter = 0.5 + (hash as f64 / u64::MAX as f64) * 0.5;

        nominal.mul_f64(jitter)
    }

    /// Reset back-off to the initial delay (call after a stable connection).
    pub(super) fn reset(&mut self) {
        self.current = self.initial;
    }
}

/// Drain `command_rx` while sleeping for `delay`, returning `true` if shutdown
/// was requested before the sleep expired.
///
/// Keeps the command channel drained so it does not back up during reconnect
/// waits.
pub(super) async fn backoff_wait<C>(
    delay: Duration,
    command_rx: &mut mpsc::Receiver<C>,
    shutdown: &CancellationToken,
) -> bool {
    let sleep = tokio::time::sleep(delay);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            _ = &mut sleep => return false,
            Some(_) = command_rx.recv() => {},
            _ = shutdown.cancelled() => return true,
        }
    }
}
