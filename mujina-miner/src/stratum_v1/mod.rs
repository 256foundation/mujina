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
//! use stratum_v1::{StratumV1Client, ClientEvent, StratumV1PoolConfig};
//!
//! let (event_tx, mut event_rx) = mpsc::channel(100);
//! let config = StratumV1PoolConfig {
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

pub use client::{StratumV1Client, StratumV1PoolConfig};
pub use connection::{Connector, TcpConnector, Transport};
#[cfg(test)]
pub(crate) use connection::{MockConnector, MockTransport, MockTransportHandle};
pub use error::{StratumError, StratumResult};
#[cfg(test)]
pub(crate) use messages::JsonRpcMessage;
pub use messages::{ClientCommand, ClientEvent, JobNotification, SubmitParams};
