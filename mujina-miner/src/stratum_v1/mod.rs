//! Stratum v1 mining protocol client.
//!
//! This module provides a reusable Stratum v1 client for connecting to mining
//! pools. The protocol uses JSON-RPC over TCP with newline-delimited messages.
//!
//! # Protocol Overview
//!
//! Stratum v1 is a bidirectional, event-driven protocol:
//!
//! - **Client requests**: subscribe, authorize, submit, suggest_difficulty
//! - **Server notifications**: mining.notify (new work), mining.set_difficulty,
//!   mining.set_version_mask
//! - **Server responses**: Results for client requests (boolean or error array)
//!
//! # Architecture
//!
//! The client is designed as an active async task that manages the TCP
//! connection and pushes events to a consumer via channels. This fits naturally
//! with the job_source abstraction and tokio's async patterns.
//!
//! # Usage
//!
//! ```rust,ignore
//! use stratum_v1::{StratumV1Client, ClientEvent, PoolConfig};
//!
//! let (event_tx, mut event_rx) = mpsc::channel(100);
//! let config = PoolConfig {
//!     url: "stratum+tcp://pool.example.com:3333".to_string(),
//!     username: "worker".to_string(),
//!     password: "x".to_string(),
//! };
//!
//! let client = StratumV1Client::new(config, event_tx, shutdown_token);
//! tokio::spawn(client.run());
//!
//! while let Some(event) = event_rx.recv().await {
//!     match event {
//!         ClientEvent::NewJob(job) => { /* handle new work */ }
//!         ClientEvent::DifficultyChanged(diff) => { /* update difficulty */ }
//!         // ...
//!     }
//! }
//! ```

mod client;
mod connection;
mod error;
mod messages;

use crate::types::ShareRate;
use std::time::Duration;

pub use client::{PoolConfig, StratumV1Client};
pub use error::{StratumError, StratumResult};
pub use messages::{ClientCommand, ClientEvent, JobNotification, SubmitParams};

/// Safety cap to prevent flooding pools during startup or misconfiguration.
///
/// # Purpose
///
/// This cap exists solely for flood prevention---it's a safety net for
/// pathological cases like:
/// - Pool starts us at difficulty 1 with high-hashrate hardware
/// - Misconfigured pool sends absurdly low difficulty
/// - Bug in difficulty handling
///
/// It is NOT intended to interact with or assist pool vardiff algorithms.
/// The goal is to be permissive enough that normal vardiff operation is
/// unaffected while still catching catastrophic misconfiguration.
///
/// # Why not set it at the pool's target rate?
///
/// It's tempting to set this at a typical pool's target share rate (e.g.,
/// ckpool targets ~3.33 sec/share). However, this would defeat vardiff:
///
/// Vardiff algorithms like ckpool's work by measuring actual share arrival
/// rate and adjusting difficulty to converge on a target rate. If we
/// client-side rate limit at exactly the target rate, the pool sees the
/// capped rate and thinks difficulty is correct---it can never increase
/// difficulty because it never sees the "natural" share flood that would
/// indicate difficulty is too low.
///
/// Example: ckpool's vardiff triggers after 72 shares or 240 seconds and
/// adjusts based on the ratio of actual vs target share rate. With client-
/// side capping at the target rate, the pool would see perfect behavior
/// and never raise difficulty, leaving us doing unnecessary local work
/// filtering shares.
///
/// # Why 10 shares/second?
///
/// - **Server load**: 10 req/sec from a single client is modest. Pools
///   routinely handle many concurrent miners, so per-client rates at this
///   level shouldn't cause concern.
/// - **Headroom for vardiff**: At 10/sec, a pool targeting 0.3/sec (3.33 sec
///   interval) sees ~33x higher than desired rate, clearly signaling that
///   difficulty needs to increase dramatically. Vardiff algorithms converge
///   quickly under such clear signal.
/// - **Still prevents real floods**: Without any cap, very low difficulty
///   combined with high hashrate could overwhelm both our CPU (verifying
///   hashes) and the pool. 10/sec provides a hard ceiling.
///
/// # Interaction with ckpool vardiff
///
/// ckpool's vardiff (from `src/stratifier.c`):
/// - Target rate: ~3.33 seconds per share (`optimal = dsps * 3.33`)
/// - Evaluation window: 72 shares OR 240 seconds, whichever comes first
/// - Acceptable ratio range: 0.15 to 0.4 (raises if too fast, lowers if too slow)
///
/// With our 10/sec cap vs ckpool's ~0.3/sec target:
/// - We're ~33x faster than target, well outside the 0.15-0.4 acceptable range
/// - ckpool will aggressively raise difficulty each evaluation period
/// - After a few adjustment cycles, difficulty will be high enough that natural
///   share rate falls below 10/sec and the cap becomes inactive
pub const FLOOD_PREVENTION_CAP: ShareRate = ShareRate::from_interval(Duration::from_millis(100));
