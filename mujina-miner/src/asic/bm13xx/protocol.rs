//! Vestigial command-builder for BM13xx chips.
//!
//! [`BM13xxProtocol`] predates the typed `Register` payloads and
//! returns `Vec<RegisterCommand>` for callers to execute. Only
//! `discover_chips` retains a live caller (Bitaxe). The multi-chip
//! EmberOne work replaces this type with a typed driver.

use super::chip_config::ChipConfig;
use super::command::{
    ChainInactive, Destination, ReadRegister, RegisterCommand, SetChipAddress, WriteRegister,
};
use super::error::ProtocolError;
use super::register::{
    AnalogMux, Core, InitControl, IoDriverStrength, Log2Difficulty, MiscControl, MiscSettings,
    NonceRange, Pll3Parameter, PllDivider, Register, RegisterAddress, TicketMask, UartBaud,
    UartRelay, VersionMask,
};
use crate::types::{Difficulty, Frequency};

/// Protocol handler for BM13xx family chips.
///
/// Encodes high-level operations into chip-specific commands and
/// decodes chip responses into meaningful results.
pub struct BM13xxProtocol {}

impl Default for BM13xxProtocol {
    fn default() -> Self {
        Self::new()
    }
}

impl BM13xxProtocol {
    /// Create a new protocol instance.
    pub fn new() -> Self {
        Self {}
    }

    fn broadcast_write(&self, register: Register) -> RegisterCommand {
        RegisterCommand::WriteRegister(WriteRegister {
            destination: Destination::Broadcast,
            register,
        })
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn write_to(&self, chip_address: u8, register: Register) -> RegisterCommand {
        RegisterCommand::WriteRegister(WriteRegister {
            destination: Destination::Chip(chip_address),
            register,
        })
    }

    /// Returns the initialization sequence for a single chip (e.g., Bitaxe).
    ///
    /// The commands configure the chip for mining:
    /// 1. Enable version rolling
    /// 2. Set PLL parameters for desired frequency using the chip's
    ///    family-specific PLL parameters
    ///
    /// Returns `None` when `frequency` is unreachable for this chip
    /// model.
    pub fn single_chip_init(
        &self,
        chip_config: &ChipConfig,
        frequency: Frequency,
    ) -> Option<Vec<RegisterCommand>> {
        let pll_config = chip_config.calculate_pll(frequency)?;
        Some(vec![
            self.broadcast_write(Register::VersionMask(VersionMask::full_rolling())),
            self.broadcast_write(Register::PllDivider(pll_config)),
        ])
    }

    /// Initialize a multi-chip chain (e.g., S21 Pro, S19 J Pro).
    ///
    /// This follows the initialization sequence from production miners:
    /// 1. Enable version rolling on all chips
    /// 2. Configure initial settings
    /// 3. Set chain inactive and assign addresses
    /// 4. Configure domain boundaries
    /// 5. Ramp up frequency gradually
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn multi_chip_init(&self, chain_length: usize) -> Vec<RegisterCommand> {
        // Multi-chip initialization register values
        const INIT_CONTROL_VALUE: u32 = 0x00000700;
        const MISC_CONTROL_MULTI_CHIP: u32 = 0x0000c1f0;
        const CORE_REG_INIT_1: u32 = 0x00008b80;
        const CORE_REG_INIT_2: u32 = 0x0c800080;
        const ADDRESS_INCREMENT: u8 = 2;

        // Pre-allocate for efficiency (rough estimate of commands)
        let mut commands = Vec::with_capacity(10 + chain_length);

        // Step 1: Enable version rolling on all chips (broadcast)
        commands.push(self.broadcast_write(Register::VersionMask(VersionMask::full_rolling())));

        // Step 2: Configure init control register
        commands.push(self.broadcast_write(Register::InitControl(InitControl(INIT_CONTROL_VALUE))));

        // Step 3: Configure misc control
        commands.push(
            self.broadcast_write(Register::MiscControl(MiscControl(MISC_CONTROL_MULTI_CHIP))),
        );

        // Step 4: Set chain inactive for address assignment
        commands.push(RegisterCommand::ChainInactive(ChainInactive));

        // Step 5: Assign addresses (increment by 2)
        for i in 0..chain_length {
            let address = (i as u8) * ADDRESS_INCREMENT;
            commands.push(RegisterCommand::SetChipAddress(SetChipAddress {
                chip_address: address,
            }));
        }

        // Step 6: Configure core registers on all chips
        commands.push(self.broadcast_write(Register::Core(Core(CORE_REG_INIT_1))));
        commands.push(self.broadcast_write(Register::Core(Core(CORE_REG_INIT_2))));

        // Step 7: Set ticket mask (difficulty 256 = ~1 nonce/sec at 1 TH/s)
        let log2_diff = Log2Difficulty::from_difficulty(Difficulty::from(256_u64));
        commands.push(self.broadcast_write(Register::TicketMask(TicketMask::new(log2_diff))));

        // Step 8: Configure IO driver strength on all chips
        commands.push(self.broadcast_write(Register::IoDriverStrength(IoDriverStrength::normal())));

        // Step 9: Configure nonce range partitioning
        commands.extend(self.configure_nonce_ranges(chain_length));

        commands
    }

    /// Configure domain boundaries for a multi-chip chain.
    ///
    /// Domains are groups of chips that share signal integrity settings.
    /// This configures IO driver strength and UART relay for domain boundaries.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn configure_domains(
        &self,
        chain_length: usize,
        chips_per_domain: usize,
    ) -> Vec<RegisterCommand> {
        const UART_RELAY_BASE: u32 = 0x03000000;
        const ADDRESS_INCREMENT: u8 = 2;

        let mut commands = Vec::new();
        let num_domains = chain_length.div_ceil(chips_per_domain);

        // Configure IO driver strength at domain boundaries
        for domain in 0..num_domains {
            let last_chip_in_domain = ((domain + 1) * chips_per_domain - 1).min(chain_length - 1);
            let chip_address = (last_chip_in_domain as u8) * ADDRESS_INCREMENT;

            commands.push(self.write_to(
                chip_address,
                Register::IoDriverStrength(IoDriverStrength::domain_boundary()),
            ));
        }

        // Configure UART relay for each domain
        for domain in 0..num_domains {
            let first_chip = domain * chips_per_domain;
            let last_chip = ((domain + 1) * chips_per_domain - 1).min(chain_length - 1);

            // Configure first chip in domain
            let first_address = (first_chip as u8) * ADDRESS_INCREMENT;
            let relay_offset = (domain * chips_per_domain) as u32;
            commands.push(self.write_to(
                first_address,
                Register::UartRelay(UartRelay(UART_RELAY_BASE | (relay_offset << 8))),
            ));

            // Configure last chip in domain
            if first_chip != last_chip {
                let last_address = (last_chip as u8) * ADDRESS_INCREMENT;
                commands.push(self.write_to(
                    last_address,
                    Register::UartRelay(UartRelay(UART_RELAY_BASE | (relay_offset << 8))),
                ));
            }
        }

        commands
    }

    /// Configure nonce range partitioning for multi-chip operation.
    ///
    /// This distributes the 32-bit nonce space across all chips in the chain
    /// to avoid duplicate work. Each chip searches a unique portion of the nonce space.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn configure_nonce_ranges(&self, chain_length: usize) -> Vec<RegisterCommand> {
        let mut commands = Vec::new();

        // Calculate nonce range based on chain length
        let nonce_config = NonceRange::multi_chip(chain_length);

        // Write nonce range to all chips
        commands.push(self.broadcast_write(Register::NonceRange(nonce_config)));

        commands
    }

    /// Create a command to read a register.
    pub fn read_register(&self, chip_address: u8, register: RegisterAddress) -> RegisterCommand {
        RegisterCommand::ReadRegister(ReadRegister {
            destination: Destination::Chip(chip_address),
            register_address: register,
        })
    }

    /// Set UART baud rate on all chips
    pub fn set_baudrate(&self, baudrate: UartBaud) -> RegisterCommand {
        RegisterCommand::WriteRegister(WriteRegister {
            destination: Destination::Broadcast,
            register: Register::UartBaud(baudrate),
        })
    }

    /// Create a command to write a register.
    ///
    /// Note: This is a placeholder - actual register encoding depends on the register type
    pub fn write_register(
        &self,
        chip_address: u8,
        register: RegisterAddress,
        value: u32,
    ) -> Result<RegisterCommand, ProtocolError> {
        let register_value = match register {
            RegisterAddress::ChipId => {
                // Can't write chip ID register directly
                return Err(ProtocolError::ReadOnlyRegister(register));
            }
            RegisterAddress::PllDivider => {
                Register::PllDivider(PllDivider::decode(value.to_le_bytes()))
            }
            RegisterAddress::NonceRange => {
                Register::NonceRange(NonceRange::decode(value.to_le_bytes()))
            }
            RegisterAddress::TicketMask => {
                Register::TicketMask(TicketMask::decode(value.to_le_bytes()))
            }
            RegisterAddress::MiscControl => Register::MiscControl(MiscControl(value)),
            RegisterAddress::UartBaud => Register::UartBaud(UartBaud::Custom(value)),
            RegisterAddress::UartRelay => Register::UartRelay(UartRelay(value)),
            RegisterAddress::Core => Register::Core(Core(value)),
            RegisterAddress::AnalogMux => Register::AnalogMux(AnalogMux(value)),
            RegisterAddress::IoDriverStrength => {
                Register::IoDriverStrength(IoDriverStrength::decode(value.to_le_bytes()))
            }
            RegisterAddress::Pll3Parameter => Register::Pll3Parameter(Pll3Parameter(value)),
            RegisterAddress::VersionMask => {
                Register::VersionMask(VersionMask::decode(value.to_le_bytes()))
            }
            RegisterAddress::InitControl => Register::InitControl(InitControl(value)),
            RegisterAddress::MiscSettings => Register::MiscSettings(MiscSettings(value)),
        };

        Ok(RegisterCommand::WriteRegister(WriteRegister {
            destination: Destination::Chip(chip_address),
            register: register_value,
        }))
    }

    /// Create a broadcast command to discover all chips.
    pub fn discover_chips() -> RegisterCommand {
        RegisterCommand::ReadRegister(ReadRegister {
            destination: Destination::Broadcast,
            register_address: RegisterAddress::ChipId,
        })
    }
}

#[cfg(test)]
mod init_tests {
    use bytes::BytesMut;

    use super::*;

    #[test]
    fn multi_chip_init_sequence() {
        let protocol = BM13xxProtocol::new();
        let commands = protocol.multi_chip_init(65); // S21 Pro has 65 chips

        // Verify the sequence starts with version rolling enable
        assert!(matches!(
            &commands[0],
            RegisterCommand::WriteRegister(WriteRegister {
                destination: Destination::Broadcast,
                register: Register::VersionMask(_),
            })
        ));

        // Verify chain inactive command
        let chain_inactive_pos = commands
            .iter()
            .position(|c| matches!(c, RegisterCommand::ChainInactive(ChainInactive)))
            .expect("ChainInactive command not found in initialization sequence");
        assert!(chain_inactive_pos > 0);

        // Verify chip addressing starts after chain inactive
        let first_address_pos = chain_inactive_pos + 1;
        assert!(matches!(
            &commands[first_address_pos],
            RegisterCommand::SetChipAddress(SetChipAddress { chip_address: 0x00 })
        ));

        // Verify we have 65 address assignments
        let address_commands: Vec<_> = commands[first_address_pos..first_address_pos + 65]
            .iter()
            .collect();
        assert_eq!(address_commands.len(), 65);

        // Verify addresses increment by 2
        for (i, cmd) in address_commands.iter().enumerate() {
            match cmd {
                RegisterCommand::SetChipAddress(SetChipAddress { chip_address }) => {
                    assert_eq!(*chip_address, (i * 2) as u8);
                }
                _ => panic!("Expected SetChipAddress command, got {:?}", cmd),
            }
        }
    }

    #[test]
    fn domain_configuration() {
        let protocol = BM13xxProtocol::new();
        let commands = protocol.configure_domains(65, 5); // 65 chips, 5 per domain

        // Should have 13 domains
        let io_strength_commands: Vec<_> = commands
            .iter()
            .filter(|c| {
                matches!(
                    c,
                    RegisterCommand::WriteRegister(WriteRegister {
                        register: Register::IoDriverStrength { .. },
                        ..
                    })
                )
            })
            .collect();
        assert_eq!(io_strength_commands.len(), 13);

        // Check first domain boundary (chip 8 = address 0x08)
        let first_boundary = io_strength_commands[0];
        if let RegisterCommand::WriteRegister(WriteRegister {
            destination: Destination::Chip(chip_address),
            register: Register::IoDriverStrength(strength),
        }) = first_boundary
        {
            assert_eq!(*chip_address, 0x08); // 5th chip (index 4) * 2
            let mut buf = BytesMut::new();
            strength.encode(&mut buf);
            // Expected bytes from hardware capture
            assert_eq!(&buf[..], &[0x00, 0xf1, 0x11, 0x11]);
        }
    }

    #[test]
    fn nonce_range_configuration() {
        let protocol = BM13xxProtocol::new();

        // Test single chip - full range
        let commands = protocol.configure_nonce_ranges(1);
        assert_eq!(commands.len(), 1);
        if let RegisterCommand::WriteRegister(WriteRegister {
            register: Register::NonceRange(config),
            destination: Destination::Broadcast,
        }) = &commands[0]
        {
            let mut buf = BytesMut::new();
            config.encode(&mut buf);
            assert_eq!(&buf[..], &[0xff, 0xff, 0xff, 0xff]);
        }

        // Test S21 Pro configuration (65 chips)
        let commands = protocol.configure_nonce_ranges(65);
        assert_eq!(commands.len(), 1);
        if let RegisterCommand::WriteRegister(WriteRegister {
            register: Register::NonceRange(config),
            ..
        }) = &commands[0]
        {
            let mut buf = BytesMut::new();
            config.encode(&mut buf);
            assert_eq!(&buf[..], &[0x00, 0x00, 0x1e, 0xb5]);
        }

        // Test small chain
        let commands = protocol.configure_nonce_ranges(8);
        if let RegisterCommand::WriteRegister(WriteRegister {
            register: Register::NonceRange(config),
            ..
        }) = &commands[0]
        {
            let mut buf = BytesMut::new();
            config.encode(&mut buf);
            assert_eq!(&buf[..], &[0xff, 0xff, 0xff, 0x1f]);
        }
    }

    #[test]
    fn multi_chip_init_includes_nonce_range() {
        let protocol = BM13xxProtocol::new();
        let commands = protocol.multi_chip_init(65);

        // Find the nonce range configuration
        let nonce_range_cmd = commands.iter().find(|c| {
            matches!(
                c,
                RegisterCommand::WriteRegister(WriteRegister {
                    register: Register::NonceRange { .. },
                    ..
                })
            )
        });

        assert!(nonce_range_cmd.is_some());

        if let Some(RegisterCommand::WriteRegister(WriteRegister {
            register: Register::NonceRange(config),
            ..
        })) = nonce_range_cmd
        {
            let mut buf = BytesMut::new();
            config.encode(&mut buf);
            assert_eq!(&buf[..], &[0x00, 0x00, 0x1e, 0xb5]); // S21 Pro value
        }
    }
}
