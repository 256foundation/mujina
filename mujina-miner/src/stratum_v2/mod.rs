//! Stratum V2 protocol client module.
//!
//! Provides a Noise-encrypted Stratum V2 client using Extended Channels.
//! Extended Channels supply `coinbase_tx_prefix`, `merkle_path`, and related
//! fields to the miner, enabling the existing `MerkleRootKind::Computed` path
//! without requiring ASIC-level `nVersion` bit rolling — see
//! [Mining Protocol > Extended Channel][sv2-ec].
//!
//! Networking (Noise_NX handshake, encrypted frame I/O, DNS resolution, TCP
//! timeouts) is delegated to the [`stratum_apps`] library.
//!
//! # Architecture
//!
//! The [`StratumV2Client`] is a single-connection protocol session:
//!
//! 1. **DNS resolve** → TCP connect → Noise NX handshake (via
//!    [`stratum_apps::network_helpers`])
//! 2. **SetupConnection** — negotiate protocol version 2
//! 3. **OpenExtendedMiningChannel** — establish the mining channel
//! 4. **Event loop** — `select!` over incoming frames, commands, and shutdown
//!
//! Read and write halves are decoupled: a spawned reader task does blocking
//! `read_frame()` calls (which are not cancellation-safe), forwarding decoded
//! messages to the main event loop via a channel. The main loop `select!`s
//! over the reader channel, command channel, and shutdown token.
//!
//! # References
//!
//! - [Protocol Security > URL Scheme and Pool Authority Key][sv2-url]
//! - [Mining Protocol > Extended Channel][sv2-ec]
//!
//! [sv2-url]: https://github.com/stratum-mining/sv2-spec/blob/main/04-Protocol-Security.md#47-url-scheme-and-pool-authority-key
//! [sv2-ec]: https://github.com/stratum-mining/sv2-spec/blob/main/05-Mining-Protocol.md#522-extended-channel

mod client;
mod error;

pub use client::constants;
pub use client::{ClientCommand, ClientEvent, ClientOutcome, PoolConfig, StratumV2Client};
pub use error::{StratumV2Error, StratumV2Result};
