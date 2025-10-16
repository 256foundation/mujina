//! Block version template with rolling capability.

use bitcoin::block::Version;

/// Template for block version with optional rolling capability.
///
/// Mining hardware can search additional nonce space by modifying bits in the
/// block version field, a technique called "version rolling" (BIP320). In Stratum v1,
/// the pool specifies which bits miners may modify via the `version_mask` parameter in
/// the `mining.configure` response.
///
/// Version rolling expands the search space without requiring new jobs from the
/// pool. For example, if a chip can roll 10 version bits along with the 32-bit
/// nonce, it can search 2^42 combinations per job instead of just 2^32.
///
/// The mask uses bit positions where 1 means "may be modified" and 0 means
/// "must be preserved". If no mask is provided, version rolling is disabled.
#[derive(Debug, Clone)]
pub struct VersionTemplate {
    /// Block version number and soft-fork signaling field.
    pub version: Version,

    /// Bitmask indicating which version bits may be rolled.
    ///
    /// Each bit set to 1 indicates that bit position may be modified during
    /// mining. None indicates version rolling is not permitted for this job.
    pub mask: Option<u32>,
}
