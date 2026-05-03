//! Live runtime model of the chip serial chain, built from a TopologySpec.
//!
//! TopologySpec is the static blueprint; Chain is what the miner actively
//! tracks during a session. It owns the Chip and Domain instances, holds
//! the serial addresses assigned during enumeration, and carries per-chip
//! runtime stats that mutate as work flows.
//!
//! Chain order and domain grouping are orthogonal. Chain order is how
//! chips are wired for serial communication; domains group chips that
//! share voltage rails. A domain's chips may not be contiguous in chain
//! order.

use std::time::Instant;

use crate::types::HashRate;

use super::topology::TopologySpec;

/// The complete chain model.
///
/// Owns all chips and domains. Structure reflects physical wiring; state
/// reflects current operation.
#[derive(Debug, Clone)]
pub struct Chain {
    chips: Vec<Chip>,
    domains: Vec<Domain>,
}

impl Chain {
    /// Builds the chain structure from a topology specification.
    ///
    /// Creates chips and domains based on the topology. Addresses are not
    /// yet assigned; call `assign_addresses()` after construction.
    pub fn from_topology(spec: &TopologySpec) -> Self {
        let chip_count = spec.expected_chip_count();

        let chips: Vec<Chip> = (0..chip_count)
            .map(|i| Chip {
                address: 0,
                domain: spec.domain_for(ChipId(i)),
                stats: ChipStats::default(),
            })
            .collect();

        let domain_count = spec.domain_count();
        let mut domains: Vec<Domain> = (0..domain_count)
            .map(|i| Domain {
                id: DomainId(i),
                chips: Vec::new(),
            })
            .collect();

        for (i, chip) in chips.iter().enumerate() {
            domains[chip.domain.index()].chips.push(ChipId(i));
        }

        Self { chips, domains }
    }

    /// Assigns chip addresses with the standard interval of 2.
    ///
    /// Interval 2 is the convention observed in firmware captures:
    /// - S21 Pro (65 chips): addresses 0x00, 0x02, ... 0x80
    /// - S19 J Pro (126 chips): addresses 0x00, 0x02, ... 0xFA
    ///
    /// Returns error if chip count exceeds address space (max 128 chips).
    pub fn assign_addresses(&mut self) -> Result<(), AddressOverflowError> {
        self.assign_addresses_with_interval(2)
    }

    /// Assigns addresses with an explicit interval for unusual boards.
    pub fn assign_addresses_with_interval(
        &mut self,
        interval: u8,
    ) -> Result<(), AddressOverflowError> {
        if self.chips.is_empty() {
            return Ok(());
        }

        let last_address = (self.chips.len() - 1) * interval as usize;
        if last_address > 0xFF {
            return Err(AddressOverflowError {
                chip_count: self.chips.len(),
                interval,
            });
        }

        for (i, chip) in self.chips.iter_mut().enumerate() {
            chip.address = (i * interval as usize) as u8;
        }

        Ok(())
    }

    pub fn chip(&self, id: ChipId) -> &Chip {
        &self.chips[id.index()]
    }

    pub fn chip_mut(&mut self, id: ChipId) -> &mut Chip {
        &mut self.chips[id.index()]
    }

    pub fn domain(&self, id: DomainId) -> &Domain {
        &self.domains[id.index()]
    }

    /// Iterates over all chips in chain order.
    pub fn chips(&self) -> impl Iterator<Item = (ChipId, &Chip)> {
        self.chips.iter().enumerate().map(|(i, c)| (ChipId(i), c))
    }

    /// Iterates over all domains.
    pub fn domains(&self) -> impl Iterator<Item = &Domain> {
        self.domains.iter()
    }

    /// Iterates over domains from far end toward host (for domain
    /// configuration).
    pub fn domains_far_to_near(&self) -> impl Iterator<Item = &Domain> {
        self.domains.iter().rev()
    }

    /// Returns the first chip of the domain in chain order.
    pub fn domain_first(&self, id: DomainId) -> ChipId {
        *self.domain(id).chips.first().expect("domain has no chips")
    }

    /// Returns the last chip of the domain in chain order.
    pub fn domain_last(&self, id: DomainId) -> ChipId {
        *self.domain(id).chips.last().expect("domain has no chips")
    }

    pub fn chip_count(&self) -> usize {
        self.chips.len()
    }

    pub fn domain_count(&self) -> usize {
        self.domains.len()
    }
}

/// Individual BM13xx ASIC chip.
#[derive(Debug, Clone)]
pub struct Chip {
    /// Serial address (assigned during enumeration)
    pub address: u8,
    /// Which voltage domain this chip belongs to
    pub domain: DomainId,
    /// Runtime statistics
    pub stats: ChipStats,
}

/// A voltage domain: a group of chips sharing power rails.
#[derive(Debug, Clone)]
pub struct Domain {
    pub id: DomainId,
    /// Chips in this domain, ordered by chain position
    pub chips: Vec<ChipId>,
}

/// Runtime statistics for a single chip.
#[derive(Debug, Clone, Default)]
pub struct ChipStats {
    pub shares_found: u64,
    pub last_share_time: Option<Instant>,
    pub estimated_hashrate: HashRate,
    pub healthy: bool,
}

/// Type-safe index for chips in the chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChipId(pub usize);

impl ChipId {
    pub fn index(self) -> usize {
        self.0
    }
}

/// Type-safe index for voltage domains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DomainId(pub usize);

impl DomainId {
    pub fn index(self) -> usize {
        self.0
    }
}

/// Error when chip addresses would overflow the 8-bit address space.
#[derive(Debug, Clone)]
pub struct AddressOverflowError {
    pub chip_count: usize,
    pub interval: u8,
}

impl std::fmt::Display for AddressOverflowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "address overflow: {} chips with interval {} exceeds 0xFF",
            self.chip_count, self.interval
        )
    }
}

impl std::error::Error for AddressOverflowError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hardware constants from protocol captures.
    mod fixtures {
        /// Antminer S21 Pro: 65 BM1370 chips in 13 voltage domains.
        pub mod s21_pro {
            pub const CHIP_COUNT: usize = 65;
            pub const DOMAIN_COUNT: usize = 13;
            pub const CHIPS_PER_DOMAIN: usize = 5;
            pub const FIRST_CHIP_ADDRESS: u8 = 0x00;
            pub const LAST_CHIP_ADDRESS: u8 = 0x80; // (65-1) * 2
            /// Last chip address in each domain (chips 4, 9, 14, ... 64).
            pub const DOMAIN_END_ADDRESSES: [u8; 13] = [
                0x08, 0x12, 0x1C, 0x26, 0x30, 0x3A, 0x44, 0x4E, 0x58, 0x62, 0x6C, 0x76, 0x80,
            ];
        }

        /// Antminer S19 J Pro: 126 BM1362 chips, single domain.
        pub mod s19_jpro {
            pub const CHIP_COUNT: usize = 126;
            pub const FIRST_CHIP_ADDRESS: u8 = 0x00;
            pub const LAST_CHIP_ADDRESS: u8 = 0xFA; // (126-1) * 2
        }

        /// Bitaxe Gamma: single BM1370 chip.
        pub mod bitaxe {
            pub const CHIP_COUNT: usize = 1;
            pub const CHIP_ADDRESS: u8 = 0x00;
        }
    }

    #[test]
    fn s21_pro_addresses() {
        use fixtures::s21_pro::*;

        let topology = TopologySpec::uniform_domains(DOMAIN_COUNT, CHIPS_PER_DOMAIN, true);
        let mut chain = Chain::from_topology(&topology);
        chain.assign_addresses().unwrap();

        assert_eq!(chain.chip_count(), CHIP_COUNT);
        assert_eq!(chain.chip(ChipId(0)).address, FIRST_CHIP_ADDRESS);
        assert_eq!(
            chain.chip(ChipId(CHIP_COUNT - 1)).address,
            LAST_CHIP_ADDRESS
        );
    }

    #[test]
    fn s21_pro_domain_ends() {
        use fixtures::s21_pro::*;

        let topology = TopologySpec::uniform_domains(DOMAIN_COUNT, CHIPS_PER_DOMAIN, true);
        let mut chain = Chain::from_topology(&topology);
        chain.assign_addresses().unwrap();

        let actual: Vec<u8> = (0..DOMAIN_COUNT)
            .map(|d| chain.chip(chain.domain_last(DomainId(d))).address)
            .collect();

        assert_eq!(actual.as_slice(), DOMAIN_END_ADDRESSES);
    }

    #[test]
    fn s19_jpro_addresses() {
        use fixtures::s19_jpro::*;

        let topology = TopologySpec::single_domain(CHIP_COUNT);
        let mut chain = Chain::from_topology(&topology);
        chain.assign_addresses().unwrap();

        assert_eq!(chain.chip(ChipId(0)).address, FIRST_CHIP_ADDRESS);
        assert_eq!(
            chain.chip(ChipId(CHIP_COUNT - 1)).address,
            LAST_CHIP_ADDRESS
        );
    }

    #[test]
    fn bitaxe_single_chip() {
        use fixtures::bitaxe::*;

        let topology = TopologySpec::single_domain(CHIP_COUNT);
        let mut chain = Chain::from_topology(&topology);
        chain.assign_addresses().unwrap();

        assert_eq!(chain.chip_count(), 1);
        assert_eq!(chain.chip(ChipId(0)).address, CHIP_ADDRESS);
    }

    #[test]
    fn address_overflow_detected() {
        // 129 chips * interval 2 = last address 256, which overflows u8
        let topology = TopologySpec::single_domain(129);
        let mut chain = Chain::from_topology(&topology);

        let result = chain.assign_addresses();
        assert!(result.is_err());
    }

    #[test]
    fn domain_chips_in_chain_order() {
        // With contiguous domains, chips 0-4 are domain 0, 5-9 are domain 1, etc.
        let topology = TopologySpec::uniform_domains(3, 5, true);
        let chain = Chain::from_topology(&topology);

        let domain0_chips: Vec<usize> = chain
            .domain(DomainId(0))
            .chips
            .iter()
            .map(|c| c.index())
            .collect();
        assert_eq!(domain0_chips, vec![0, 1, 2, 3, 4]);

        let domain1_chips: Vec<usize> = chain
            .domain(DomainId(1))
            .chips
            .iter()
            .map(|c| c.index())
            .collect();
        assert_eq!(domain1_chips, vec![5, 6, 7, 8, 9]);
    }
}
