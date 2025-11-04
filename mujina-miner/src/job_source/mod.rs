//! Mining job source implementations.
//!
//! This module provides various sources for mining jobs, unifying different
//! methods of obtaining work for the mining hardware. Job sources can include:
//!
//! - **Pool Clients**: Connect to mining pools via protocols like Stratum v1/v2
//! - **Solo Mining**: Direct connection to Bitcoin nodes for solo mining
//! - **Dummy Work**: Generate synthetic jobs for power/thermal management
//!
//! # Architecture
//!
//! Job sources are **active async tasks** that push events to the scheduler via
//! message passing. Each source type implements its own `run()` loop based on
//! its event sources (timers for dummy, network for pools, RPC for solo mining).
//!
//! Sources communicate with the scheduler using the return-addressed envelope
//! pattern: messages include a `SourceHandle` that the scheduler can use to
//! route shares back to the correct source.
//!
//! ## Work Generation Hierarchy
//!
//! The mining workflow follows a three-level template hierarchy:
//!
//! 1. **[`JobTemplate`]** (from pool/source) - Contains all templates:
//!    - Version rolling mask ([`VersionTemplate`])
//!    - Extranonce2 space ([`Extranonce2Range`])
//!    - Merkle root specification ([`MerkleRootKind`])
//!
//! 2. **`HeaderTemplate`** (to hardware) - Partially instantiated:
//!    - Specific extranonce2 value selected
//!    - Nonce = 0 (hardware will roll)
//!    - Version bits may be rolled by hardware
//!
//! 3. **[`Share`]** (from hardware) - Fully instantiated:
//!    - Valid nonce found
//!    - Meets chip difficulty target
//!
//! The scheduler generates many `HeaderTemplate` instances from each
//! `JobTemplate` by allocating extranonce2 space and rolling extranonce2 and
//! ntime. Hardware then searches for valid nonces within each header template.

// Submodules
pub mod dummy;
mod extranonce2;
mod job;
mod merkle;
mod messages;
pub mod test_blocks;
mod version;

// Re-export types from submodules
pub use extranonce2::{Extranonce2, Extranonce2Error, Extranonce2Iter, Extranonce2Range};
pub use job::{JobTemplate, Share};
pub use merkle::{MerkleRootKind, MerkleRootTemplate};
pub use messages::{SourceCommand, SourceEvent, SourceHandle};
pub use version::VersionTemplate;

// TODO: Add HeaderTemplate type (Level 2 in the hierarchy)
// TODO: Implement dummy source
// TODO: Implement scheduler
