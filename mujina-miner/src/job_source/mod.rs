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
//!
//! ## Share Difficulty
//!
//! Sources receive share difficulty from their upstream (pool, node, etc.)
//! and report it directly in [`JobTemplate::share_target`]. We avoid
//! suggesting difficulty to pools because some (notably Ocean) disconnect
//! clients that suggest inappropriately low values.
//!
//! Rate limiting to prevent share flooding is the scheduler's responsibility.
//! Sources declare their maximum share rate at registration time, and the
//! scheduler enforces it.

// Submodules
pub mod dummy;
mod extranonce2;
pub mod forced_rate;
pub(crate) mod job;
mod merkle;
mod messages;
pub mod stratum_v1;
pub mod test_blocks;
mod version;

// Re-export types from submodules
pub use extranonce2::{Extranonce2, Extranonce2Error, Extranonce2Iter, Extranonce2Range};
pub use job::{JobTemplate, Share};
pub use merkle::{MerkleRootKind, MerkleRootTemplate};
pub use messages::{SourceCommand, SourceEvent, SourceHandle};
pub use version::{GeneralPurposeBits, VersionTemplate, VersionTemplateError};

// TODO: Add HeaderTemplate type (Level 2 in the hierarchy)
// TODO: Implement dummy source
// TODO: Implement scheduler
