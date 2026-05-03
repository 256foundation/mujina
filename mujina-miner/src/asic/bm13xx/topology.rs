//! Static specification of chip wiring on a BM13xx hash board.
//!
//! For each chain position, identifies the voltage domain the chip
//! belongs to and whether the board needs domain boundary configuration.
//! TopologySpec is the input to `chain::Chain`, which is the live runtime
//! model populated and updated during a mining session.

use std::collections::HashSet;

use super::chain::{ChipId, DomainId};

/// Describes the physical wiring of chips on a board.
///
/// For each chain position, specifies which voltage domain the chip belongs
/// to. Also indicates whether the board needs domain boundary configuration
/// (IO driver strength, UART relay).
#[derive(Debug, Clone)]
pub struct TopologySpec {
    /// For each chain position, which domain does that chip belong to?
    chip_domains: Vec<DomainId>,
    /// Whether this board needs domain boundary configuration
    needs_domain_config: bool,
}

impl TopologySpec {
    /// Uniform domains with equal chip counts (S21 Pro style).
    ///
    /// Chain visits all chips in domain 0, then domain 1, etc.
    /// Each domain has the same number of chips.
    ///
    /// Example: `uniform_domains(13, 5, true)` creates 65 chips in 13 domains of 5.
    pub fn uniform_domains(
        domain_count: usize,
        chips_per_domain: usize,
        needs_domain_config: bool,
    ) -> Self {
        let chip_domains = (0..domain_count * chips_per_domain)
            .map(|i| DomainId(i / chips_per_domain))
            .collect();
        Self {
            chip_domains,
            needs_domain_config,
        }
    }

    /// Individual domains, one per chip (EmberOne style).
    ///
    /// Each chip is its own voltage domain. Useful for boards where each
    /// chip has independent power regulation.
    pub fn individual_domains(chip_count: usize, needs_domain_config: bool) -> Self {
        let chip_domains = (0..chip_count).map(DomainId).collect();
        Self {
            chip_domains,
            needs_domain_config,
        }
    }

    /// Single domain for all chips (Bitaxe, simple boards).
    ///
    /// All chips share one voltage domain. No domain boundary configuration
    /// is needed.
    pub fn single_domain(chip_count: usize) -> Self {
        Self {
            chip_domains: vec![DomainId(0); chip_count],
            needs_domain_config: false,
        }
    }

    /// Explicit mapping for complex or non-standard routing.
    ///
    /// Domain IDs must be contiguous starting from 0 (e.g., `[0, 0, 1, 1, 2, 2]`).
    /// Returns error if domains are sparse (e.g., `[0, 2, 3]` skipping 1).
    pub fn custom(
        chip_domains: Vec<usize>,
        needs_domain_config: bool,
    ) -> Result<Self, NonContiguousDomainsError> {
        if chip_domains.is_empty() {
            return Ok(Self {
                chip_domains: Vec::new(),
                needs_domain_config,
            });
        }

        let max_domain = *chip_domains.iter().max().unwrap();
        let unique_domains: HashSet<usize> = chip_domains.iter().copied().collect();

        let expected: HashSet<usize> = (0..=max_domain).collect();
        if unique_domains != expected {
            return Err(NonContiguousDomainsError {
                found: unique_domains.into_iter().collect(),
            });
        }

        Ok(Self {
            chip_domains: chip_domains.into_iter().map(DomainId).collect(),
            needs_domain_config,
        })
    }

    /// Number of chips in the topology.
    pub fn expected_chip_count(&self) -> usize {
        self.chip_domains.len()
    }

    /// Number of domains in the topology.
    pub fn domain_count(&self) -> usize {
        self.chip_domains
            .iter()
            .map(|d| d.index())
            .max()
            .map(|m| m + 1)
            .unwrap_or(0)
    }

    /// Which domain does the chip at this chain position belong to?
    pub fn domain_for(&self, id: ChipId) -> DomainId {
        self.chip_domains[id.index()]
    }

    /// Whether this board needs domain boundary configuration.
    pub fn needs_domain_config(&self) -> bool {
        self.needs_domain_config
    }
}

/// Error when domain IDs are not contiguous.
#[derive(Debug, Clone)]
pub struct NonContiguousDomainsError {
    pub found: Vec<usize>,
}

impl std::fmt::Display for NonContiguousDomainsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "domain IDs must be contiguous from 0, found: {:?}",
            self.found
        )
    }
}

impl std::error::Error for NonContiguousDomainsError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uniform_domains_creates_correct_chip_count() {
        let spec = TopologySpec::uniform_domains(13, 5, true);
        assert_eq!(spec.expected_chip_count(), 65);
        assert_eq!(spec.domain_count(), 13);
    }

    #[test]
    fn uniform_domains_assigns_domains_correctly() {
        let spec = TopologySpec::uniform_domains(3, 2, true);
        // 6 chips: [D0, D0, D1, D1, D2, D2]
        assert_eq!(spec.domain_for(ChipId(0)), DomainId(0));
        assert_eq!(spec.domain_for(ChipId(1)), DomainId(0));
        assert_eq!(spec.domain_for(ChipId(2)), DomainId(1));
        assert_eq!(spec.domain_for(ChipId(3)), DomainId(1));
        assert_eq!(spec.domain_for(ChipId(4)), DomainId(2));
        assert_eq!(spec.domain_for(ChipId(5)), DomainId(2));
    }

    #[test]
    fn individual_domains_creates_separate_domains() {
        let spec = TopologySpec::individual_domains(12, false);
        assert_eq!(spec.expected_chip_count(), 12);
        assert_eq!(spec.domain_count(), 12);

        for i in 0..12 {
            assert_eq!(spec.domain_for(ChipId(i)), DomainId(i));
        }
    }

    #[test]
    fn single_domain_puts_all_in_domain_zero() {
        let spec = TopologySpec::single_domain(5);
        assert_eq!(spec.expected_chip_count(), 5);
        assert_eq!(spec.domain_count(), 1);

        for i in 0..5 {
            assert_eq!(spec.domain_for(ChipId(i)), DomainId(0));
        }
    }

    #[test]
    fn custom_accepts_valid_domains() {
        let spec = TopologySpec::custom(vec![0, 0, 1, 1, 2, 2], true).unwrap();
        assert_eq!(spec.expected_chip_count(), 6);
        assert_eq!(spec.domain_count(), 3);
        assert!(spec.needs_domain_config());
    }

    #[test]
    fn custom_rejects_sparse_domains() {
        // Skips domain 1
        let result = TopologySpec::custom(vec![0, 0, 2, 2], true);
        assert!(result.is_err());
    }

    #[test]
    fn custom_accepts_empty() {
        let spec = TopologySpec::custom(vec![], false).unwrap();
        assert_eq!(spec.expected_chip_count(), 0);
        assert_eq!(spec.domain_count(), 0);
    }

    #[test]
    fn needs_domain_config_propagates() {
        let with = TopologySpec::uniform_domains(2, 3, true);
        assert!(with.needs_domain_config());

        let without = TopologySpec::single_domain(5);
        assert!(!without.needs_domain_config());
    }
}
