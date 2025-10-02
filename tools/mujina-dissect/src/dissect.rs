//! Protocol dissection engine.
//!
//! TODO: Build comprehensive unit tests based on known serial captures
//! - Use captured frames from ~/mujina/captures/bitaxe-gamma-logic/esp-miner-boot.csv
//! - Test CRC validation for both work frames (CRC16) and response frames (CRC5)
//! - Test frame parsing for JobFull work frames and register responses
//! - Add regression tests to prevent future parsing failures

use crate::bm13xx::{DecodedFrame, Direction};
use crate::capture::BaudRate;
use crate::i2c::I2cOperation;
use colored::Colorize;
use mujina_miner::peripheral::{emc2101, pmbus};
use std::collections::HashMap;
use std::fmt;

/// Dissected frame with decoded content
#[derive(Debug)]
pub struct DissectedFrame {
    pub timestamp: f64,
    pub direction: Direction,
    pub baud_rate: BaudRate,
    pub raw_data: Vec<u8>,
    pub content: FrameContent,
    pub crc_status: CrcStatus,
}

/// Decoded frame content
#[derive(Debug)]
pub enum FrameContent {
    Command(String),
    Response(String),
}

/// CRC validation status
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrcStatus {
    Valid,
    NotChecked,
}

impl fmt::Display for CrcStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CrcStatus::Valid => write!(f, "{}", "CRC OK".green()),
            CrcStatus::NotChecked => write!(f, ""),
        }
    }
}

/// Convert a decoded frame from the codec to a dissected frame
pub fn dissect_decoded_frame(frame: &DecodedFrame) -> DissectedFrame {
    let (content, crc_status) = match frame {
        DecodedFrame::Command { command, .. } => {
            // For now, assume CRC is valid since the codec decoded it successfully
            // TODO: Extract actual CRC validation from codec
            (
                FrameContent::Command(format!("{:?}", command)),
                CrcStatus::Valid,
            )
        }
        DecodedFrame::Response { response, .. } => {
            // For now, assume CRC is valid since the codec decoded it successfully
            // TODO: Extract actual CRC validation from codec
            (
                FrameContent::Response(format!("{:?}", response)),
                CrcStatus::Valid,
            )
        }
    };

    DissectedFrame {
        timestamp: frame.timestamp(),
        direction: frame.direction(),
        baud_rate: frame.baud_rate(),
        raw_data: match frame {
            DecodedFrame::Command { raw_bytes, .. } => raw_bytes.clone(),
            DecodedFrame::Response { raw_bytes, .. } => raw_bytes.clone(),
        },
        content,
        crc_status,
    }
}

/// Dissected I2C operation
#[derive(Debug)]
pub struct DissectedI2c {
    pub timestamp: f64,
    pub address: u8,
    pub device: I2cDevice,
    pub operation: String,
    pub raw_data: Vec<u8>,
    pub was_naked: bool,
}

/// I2C device contexts for state tracking
#[derive(Debug, Default)]
pub struct I2cContexts {
    /// VOUT_MODE cache for each TPS546 device address
    pub tps546_vout_modes: HashMap<u8, u8>,
}

/// Known I2C devices
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum I2cDevice {
    Emc2101,
    Tps546,
    Unknown,
}

/// Dissect an I2C operation with context tracking
pub fn dissect_i2c_operation_with_context(
    op: &I2cOperation,
    contexts: &mut I2cContexts,
) -> DissectedI2c {
    let device = match op.address {
        0x4C => I2cDevice::Emc2101,
        0x24 => I2cDevice::Tps546,
        _ => I2cDevice::Unknown,
    };

    let operation = if let Some(reg) = op.register {
        let is_read = op.read_data.is_some();

        // Get data directly - PMBus parser already separated command from data
        let data = if is_read {
            op.read_data.as_deref()
        } else {
            op.write_data.as_deref()
        };

        match device {
            I2cDevice::Emc2101 => {
                // For now, keep using EMC2101 formatting until we refactor it too
                emc2101::protocol::format_transaction(reg, data, is_read)
            }
            I2cDevice::Tps546 => {
                // Update VOUT_MODE cache if this is a VOUT_MODE operation
                if reg == pmbus::PmbusCommand::VoutMode.as_u8() {
                    if let Some(data) = data {
                        if !data.is_empty() {
                            contexts.tps546_vout_modes.insert(op.address, data[0]);
                        }
                    }
                }

                // Format using PMBus value parser
                if let Ok(pmbus_cmd) = pmbus::PmbusCommand::try_from(reg) {
                    let direction = if is_read { "⟶" } else { "⟵" };
                    let op_type = if is_read { "READ" } else { "WRITE" };

                    if let Some(data) = data {
                        let vout_mode = contexts.tps546_vout_modes.get(&op.address).copied();
                        let value = pmbus::parse_pmbus_value(pmbus_cmd, data, vout_mode);
                        format!("{} {} {}={}", direction, op_type, pmbus_cmd, value)
                    } else {
                        // Data-less command
                        if pmbus_cmd == pmbus::PmbusCommand::ClearFaults {
                            format!(
                                "{} {} {} ({})",
                                direction,
                                op_type,
                                pmbus_cmd,
                                pmbus_cmd.description()
                            )
                        } else {
                            format!("{} {} {} (register select)", direction, op_type, pmbus_cmd)
                        }
                    }
                } else {
                    // Unknown command
                    if let Some(data) = data {
                        let direction = if is_read { "⟶" } else { "⟵" };
                        let op_type = if is_read { "READ" } else { "WRITE" };
                        format!("{} {} CMD[0x{:02x}]={:02x?}", direction, op_type, reg, data)
                    } else {
                        format!("⟵ WRITE CMD[0x{:02x}] (unknown command)", reg)
                    }
                }
            }
            I2cDevice::Unknown => {
                if let Some(data) = &op.read_data {
                    format!("⟶ READ [0x{:02x}]={:02x?}", reg, data)
                } else if let Some(data) = &op.write_data {
                    format!("⟵ WRITE [0x{:02x}]={:02x?}", reg, data)
                } else {
                    // Command-only write (no data after register/command byte)
                    format!("⟵ WRITE [0x{:02x}]", reg)
                }
            }
        }
    } else {
        // No register specified, but we can still describe the operation
        if let Some(data) = &op.read_data {
            format!("⟶ READ {:02x?} (no register)", data)
        } else if let Some(data) = &op.write_data {
            format!("⟵ WRITE {:02x?} (no register)", data)
        } else {
            format!("I2C op @ 0x{:02x}", op.address)
        }
    };

    // Build complete raw transaction bytes including I2C address
    let mut raw_data = Vec::new();

    // Handle write operations (including register select operations)
    if op.write_data.is_some() || (op.register.is_some() && op.read_data.is_none()) {
        raw_data.push(op.address << 1); // Address + Write bit
        if let Some(reg) = op.register {
            raw_data.push(reg); // Register byte
        }
        if let Some(write_data) = &op.write_data {
            raw_data.extend_from_slice(write_data);
        }
    }

    // Add address byte for read operation (if this is a read)
    if let Some(read_data) = &op.read_data {
        // If we had a write phase, this is a restart read
        if op.write_data.is_some() || op.register.is_some() {
            raw_data.push((op.address << 1) | 1); // Address + Read bit (restart)
        } else {
            // Direct read without register - just address + read bit
            raw_data.push((op.address << 1) | 1); // Address + Read bit
        }
        raw_data.extend_from_slice(read_data);
    }

    DissectedI2c {
        timestamp: op.start_time,
        address: op.address,
        device,
        operation,
        raw_data,
        was_naked: op.was_naked,
    }
}
