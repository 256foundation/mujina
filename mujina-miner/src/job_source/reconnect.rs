//! Reconnect logic shared by the pool job sources.
//!
//! A source connects and retries a lost pool connection the same way
//! whatever its protocol: it waits for hash threads before the first
//! attempt, the outcome of each attempt decides whether to retry, the
//! back-off spaces the attempts, and the wait drains commands meanwhile.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::tracing::prelude::*;
use crate::types::HashRate;

use super::{SourceCommand, SourceEvent};

/// Initial delay before reconnecting after a dropped connection.
/// Doubles on each failed attempt, jittered to [0.5, 1.0] of nominal.
pub(super) const INITIAL_RECONNECT_DELAY: Duration = Duration::from_secs(1);

/// Outcome of a single connection attempt.
///
/// Returned by each source's internal `connect_and_run` method.
pub(super) enum ConnectOutcome {
    /// Graceful shutdown requested via cancellation token.
    Shutdown,
    /// Connection lost; caller should retry after back-off.
    Disconnected,
    /// Unrecoverable error (e.g. auth failure, bad config); stop retrying.
    Fatal(anyhow::Error),
}

/// Back-off state across one source's connection attempts.
///
/// A source calls [`Reconnect::attempt`] as each attempt starts and
/// [`Reconnect::wait_after_disconnect`] when one ends in a lost connection.
pub(super) struct Reconnect {
    backoff: ExponentialBackoff,
    connected_at: Instant,
}

/// Jittered exponential back-off for reconnection timing.
///
/// Starts at `initial` and doubles after each call to [`next_delay`],
/// capping at `max`.  Each returned delay is scaled to [0.5, 1.0] of the
/// nominal value to spread out reconnection attempts and avoid
/// thundering-herd pile-ups against a recovering pool.
///
/// [`next_delay`]: ExponentialBackoff::next_delay
struct ExponentialBackoff {
    current: Duration,
    initial: Duration,
    max: Duration,
    // Per-process jitter seed.  `RandomState` is seeded from OS randomness
    // at construction, so different processes produce different jitter even
    // when reconnecting at the same wall-clock instant.  This is the same
    // approach tokio uses internally for jittered timeouts.
    jitter_state: RandomState,
    jitter_step: u64,
}

impl Reconnect {
    /// Creates the state for a source that has not connected yet.
    pub(super) fn new() -> Self {
        // Ceiling for the exponential reconnect backoff.
        const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(60);

        Self {
            backoff: ExponentialBackoff::new(INITIAL_RECONNECT_DELAY, MAX_RECONNECT_DELAY),
            connected_at: Instant::now(),
        }
    }

    /// Records that a connection attempt is starting.
    pub(super) fn attempt(&mut self) {
        self.connected_at = Instant::now();
    }

    /// Clears the scheduler's work from the lost connection, then waits out
    /// the back-off.
    ///
    /// Commands that arrive during the wait are drained so the channel does
    /// not back up.  The latest hash rate is kept in `hash_rate`, and shares,
    /// which have no connection to go to, are dropped.
    ///
    /// Returns `true` if shutdown was requested during the wait.
    pub(super) async fn wait_after_disconnect(
        &mut self,
        event_tx: &mpsc::Sender<SourceEvent>,
        command_rx: &mut mpsc::Receiver<SourceCommand>,
        shutdown: &CancellationToken,
        hash_rate: &mut HashRate,
    ) -> bool {
        // If a connection was alive for at least this long, the next
        // reconnect starts at the initial backoff.  Shorter connections leave
        // the backoff untouched, preventing tight reconnection loops against a
        // flapping pool that accepts and immediately drops.
        const STABLE_CONNECTION_THRESHOLD: Duration = Duration::from_secs(60);

        if let Err(e) = event_tx.send(SourceEvent::ClearJobs).await {
            warn!(error = %e, "Failed to send ClearJobs");
        }
        if self.connected_at.elapsed() >= STABLE_CONNECTION_THRESHOLD {
            self.backoff.reset();
        }
        let delay = self.backoff.next_delay();
        info!(
            delay_secs = delay.as_secs_f64(),
            "Reconnecting after backoff"
        );

        let sleep = tokio::time::sleep(delay);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep => return false,
                Some(cmd) = command_rx.recv() => {
                    if let SourceCommand::UpdateHashRate(rate) = cmd {
                        *hash_rate = rate;
                    }
                }
                _ = shutdown.cancelled() => return true,
            }
        }
    }
}

impl ExponentialBackoff {
    fn new(initial: Duration, max: Duration) -> Self {
        Self {
            current: initial,
            initial,
            max,
            jitter_state: RandomState::new(),
            jitter_step: 0,
        }
    }

    /// Returns the next back-off delay and advances the state.
    ///
    /// The nominal delay (1 s, 2 s, 4 s, ...) is scaled by a jitter factor in
    /// [0.5, 1.0] to spread reconnection attempts across concurrent miners.
    fn next_delay(&mut self) -> Duration {
        let nominal = self.current;
        self.current = (self.current * 2).min(self.max);

        let mut hasher = self.jitter_state.build_hasher();
        hasher.write_u64(self.jitter_step);
        self.jitter_step = self.jitter_step.wrapping_add(1);
        let hash = hasher.finish();
        let jitter = 0.5 + (hash as f64 / u64::MAX as f64) * 0.5;

        nominal.mul_f64(jitter)
    }

    /// Resets the back-off to the initial delay.
    ///
    /// Call this after a connection has proved stable.
    fn reset(&mut self) {
        self.current = self.initial;
    }
}

/// Waits until the scheduler reports a positive hash rate.
///
/// A source holds off connecting until then, since there is no point
/// fetching jobs with no hash threads to work them.  Shares that arrive
/// meanwhile have no connection to go to and are dropped.
///
/// Returns `None` if shutdown was requested first.
pub(super) async fn wait_for_hash_rate(
    command_rx: &mut mpsc::Receiver<SourceCommand>,
    shutdown: &CancellationToken,
) -> Option<HashRate> {
    loop {
        tokio::select! {
            Some(cmd) = command_rx.recv() => match cmd {
                SourceCommand::UpdateHashRate(rate) if !rate.is_zero() => return Some(rate),
                SourceCommand::UpdateHashRate(_) | SourceCommand::SubmitShare(_) => {}
            },
            _ = shutdown.cancelled() => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_each_step() {
        let mut backoff = ExponentialBackoff::new(Duration::from_secs(1), Duration::from_secs(60));

        let d1 = backoff.next_delay();
        let d2 = backoff.next_delay();
        let d3 = backoff.next_delay();

        // Nominal sequence is 1s, 2s, 4s. With jitter in [0.5, 1.0],
        // each delay is at least half the nominal value.
        assert!(d1 >= Duration::from_millis(500), "d1={d1:?}");
        assert!(d1 < Duration::from_secs(1), "d1={d1:?}");

        assert!(d2 >= Duration::from_secs(1), "d2={d2:?}");
        assert!(d2 < Duration::from_secs(2), "d2={d2:?}");

        assert!(d3 >= Duration::from_secs(2), "d3={d3:?}");
        assert!(d3 < Duration::from_secs(4), "d3={d3:?}");
    }

    #[test]
    fn backoff_caps_at_max() {
        let mut backoff = ExponentialBackoff::new(Duration::from_secs(32), Duration::from_secs(60));

        let _d1 = backoff.next_delay(); // consumes 32s nominal
        let d2 = backoff.next_delay(); // nominal capped at 60s

        assert!(d2 >= Duration::from_secs(30), "d2={d2:?}");
        assert!(d2 < Duration::from_secs(60), "d2={d2:?}");

        // Further calls stay at max
        let d3 = backoff.next_delay();
        assert!(d3 >= Duration::from_secs(30), "d3={d3:?}");
        assert!(d3 < Duration::from_secs(60), "d3={d3:?}");
    }

    #[test]
    fn backoff_reset_restores_initial() {
        let mut backoff = ExponentialBackoff::new(Duration::from_secs(1), Duration::from_secs(60));

        // Advance past initial
        let _ = backoff.next_delay();
        let _ = backoff.next_delay();
        let _ = backoff.next_delay();

        backoff.reset();

        let d = backoff.next_delay();
        assert!(d >= Duration::from_millis(500), "d={d:?}");
        assert!(d < Duration::from_secs(1), "d={d:?}");
    }
}
