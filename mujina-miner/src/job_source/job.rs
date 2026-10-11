//! Mining job template and share types.

use bitcoin::block::Version;
use bitcoin::hash_types::BlockHash;
use bitcoin::pow::{CompactTarget, Target};

use super::{Extranonce, MerkleRootKind, VersionTemplate};

/// Template for mining jobs from any source.
///
/// A job template contains all the information needed to generate block headers
/// for mining. It includes templates for version rolling, extranonce rolling,
/// and merkle root computation. The scheduler uses this template to generate
/// many `HeaderTemplate` instances for distribution to hardware.
///
/// Job templates may come from pools (Stratum v1/v2), solo mining, or dummy
/// sources for testing. Depending on the protocol and mode, the merkle root may
/// be fixed or computed dynamically from coinbase transaction parts.
#[derive(Debug, Clone)]
pub struct JobTemplate {
    /// Identifier for this job assigned by the source
    pub id: String,

    /// Previous block hash
    pub prev_blockhash: BlockHash,

    /// Block version with optional rolling capability
    pub version: VersionTemplate,

    /// Encoded network difficulty target (for block header)
    pub bits: CompactTarget,

    /// Share submission threshold.
    ///
    /// Shares are submitted if hash meets this target. Set by Stratum's
    /// mining.set_difficulty (converted to Target). Independent from network
    /// difficulty (bits field).
    pub share_target: Target,

    /// Block timestamp
    pub time: u32,

    /// Specifies how to obtain the merkle root for this job.
    pub merkle_root: MerkleRootKind,
}

impl JobTemplate {
    /// Get the target difficulty as a Target type.
    pub fn target(&self) -> Target {
        Target::from(self.bits)
    }

    /// Compute merkle root for the given extranonce.
    ///
    /// Returns an error if this is a fixed merkle root (header-only job)
    /// or if merkle computation fails.
    pub fn compute_merkle_root(
        &self,
        extranonce: &Extranonce,
    ) -> anyhow::Result<bitcoin::hash_types::TxMerkleNode> {
        match &self.merkle_root {
            MerkleRootKind::Computed(template) => template.compute_merkle_root(extranonce),
            MerkleRootKind::Fixed(_) => {
                anyhow::bail!("Cannot compute merkle root for header-only job")
            }
        }
    }
}

/// Represents a share submission (solved work).
#[derive(Debug, Clone)]
pub struct Share {
    /// Job ID this share is for
    pub job_id: String,

    /// Nonce that solves the work
    pub nonce: u32,

    /// Block timestamp
    pub time: u32,

    /// Version bits
    pub version: Version,

    /// Extranonce
    pub extranonce: Option<Extranonce>,
}
