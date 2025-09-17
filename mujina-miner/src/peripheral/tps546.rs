//! TPS546D24A Power Management Controller Driver
//!
//! This module provides a driver for the Texas Instruments TPS546D24A
//! synchronous buck converter with PMBus interface.
//!
//! Datasheet: <https://www.ti.com/lit/ds/symlink/tps546d24a.pdf>

use crate::hw_trait::I2c;
use anyhow::{bail, Result};
use thiserror::Error;
use tracing::{debug, error, info, trace, warn};

use super::pmbus::{self, Linear11, Linear16, PmbusCommand, StatusDecoder};

/// Protocol dissection utilities for TPS546
pub mod protocol {
    use super::pmbus::{self, PmbusCommand};

    /// Default I2C address for TPS546
    pub const DEFAULT_ADDRESS: u8 = 0x24;

    /// Known device IDs for TPS546 variants
    pub const DEVICE_ID1: [u8; 6] = [0x54, 0x49, 0x54, 0x6B, 0x24, 0x41]; // TPS546D24A
    pub const DEVICE_ID2: [u8; 6] = [0x54, 0x49, 0x54, 0x6D, 0x24, 0x41]; // TPS546D24A
    pub const DEVICE_ID3: [u8; 6] = [0x54, 0x49, 0x54, 0x6D, 0x24, 0x62]; // TPS546D24S

    /// Decode device ID to model string
    pub fn decode_device_id(data: &[u8]) -> String {
        if data.len() >= 6 {
            let id = &data[0..6];
            let model = if id == DEVICE_ID1 || id == DEVICE_ID2 {
                "TPS546D24A"
            } else if id == DEVICE_ID3 {
                "TPS546D24S"
            } else {
                "Unknown device"
            };

            format!("{:02x?} ({})", data, model)
        } else {
            format!("{:02x?}", data)
        }
    }

    /// TPS546 context state for tracking VOUT_MODE across operations
    #[derive(Debug, Default)]
    pub struct Tps546Context {
        pub vout_mode: Option<u8>,
    }

    /// Format a TPS546 I2C transaction with comprehensive PMBus decoding
    pub fn format_transaction(cmd: u8, data: Option<&[u8]>, is_read: bool) -> String {
        format_transaction_with_context(cmd, data, is_read, &mut Tps546Context::default())
    }

    /// Format a TPS546 I2C transaction with context-aware decoding
    pub fn format_transaction_with_context(
        cmd: u8,
        data: Option<&[u8]>,
        is_read: bool,
        context: &mut Tps546Context,
    ) -> String {
        let cmd_name = pmbus::command_name(cmd);

        // Update context based on this operation
        if is_read && cmd == PmbusCommand::VoutMode.as_u8() {
            if let Some(data) = data {
                if data.len() >= 1 {
                    context.vout_mode = Some(data[0]);
                }
            }
        } else if !is_read && cmd == PmbusCommand::VoutMode.as_u8() {
            if let Some(data) = data {
                if data.len() >= 1 {
                    context.vout_mode = Some(data[0]);
                }
            }
        }

        if is_read {
            if let Some(data) = data {
                let decoded = decode_read_value_with_context(cmd, data, context);
                format!("⟶ READ {}={}", cmd_name, decoded)
            } else {
                format!("⟶ READ {}", cmd_name)
            }
        } else {
            if let Some(data) = data {
                let decoded = decode_write_value_with_context(cmd, data, context);
                format!("⟵ WRITE {}={}", cmd_name, decoded)
            } else {
                // Data-less command - only use generic description for truly data-less commands
                if let Some(command) = PmbusCommand::from_u8(cmd) {
                    match command {
                        PmbusCommand::ClearFaults => {
                            format!("⟵ WRITE {} (clears all fault status bits)", cmd_name)
                        }
                        _ => format!("⟵ WRITE {} (register select)", cmd_name),
                    }
                } else {
                    format!("⟵ WRITE CMD[0x{:02x}] (unknown command)", cmd)
                }
            }
        }
    }

    /// Decode PMBus read operation value with context
    fn decode_read_value_with_context(cmd: u8, data: &[u8], context: &Tps546Context) -> String {
        if let Some(command) = PmbusCommand::from_u8(cmd) {
            match command {
                // Linear16 format readings (use VOUT_MODE context if available)
                PmbusCommand::ReadVout => {
                    decode_linear16_voltage_with_context(data, context.vout_mode)
                }
                _ => decode_read_value(cmd, data),
            }
        } else {
            decode_read_value(cmd, data)
        }
    }

    /// Decode PMBus write operation value with context
    fn decode_write_value_with_context(cmd: u8, data: &[u8], context: &Tps546Context) -> String {
        if let Some(command) = PmbusCommand::from_u8(cmd) {
            match command {
                // Two-byte Linear16 values (VOUT commands - use VOUT_MODE context if available)
                PmbusCommand::VoutCommand
                | PmbusCommand::VoutMax
                | PmbusCommand::VoutMin
                | PmbusCommand::VoutMarginHigh
                | PmbusCommand::VoutMarginLow
                | PmbusCommand::VoutOvFaultLimit
                | PmbusCommand::VoutOvWarnLimit
                | PmbusCommand::VoutUvWarnLimit
                | PmbusCommand::VoutUvFaultLimit => {
                    decode_write_linear16_with_context(data, context.vout_mode)
                }
                _ => decode_write_value(cmd, data),
            }
        } else {
            decode_write_value(cmd, data)
        }
    }

    /// Decode PMBus read operation value
    fn decode_read_value(cmd: u8, data: &[u8]) -> String {
        if let Some(command) = PmbusCommand::from_u8(cmd) {
            match command {
                // Device identification
                PmbusCommand::IcDeviceId => decode_device_id(data),
                PmbusCommand::MfrId | PmbusCommand::MfrModel | PmbusCommand::MfrRevision => {
                    decode_string_data(data)
                }

                // Status registers (16-bit)
                PmbusCommand::StatusWord => {
                    if data.len() >= 2 {
                        let status = u16::from_le_bytes([data[0], data[1]]);
                        let flags = pmbus::StatusDecoder::decode_status_word(status);
                        if flags.is_empty() {
                            format!("0x{:04x}", status)
                        } else {
                            format!("0x{:04x} ({})", status, flags.join(" | "))
                        }
                    } else {
                        format!("{:02x?}", data)
                    }
                }

                // Status registers (8-bit)
                PmbusCommand::StatusVout => {
                    decode_status_byte(data, |b| pmbus::StatusDecoder::decode_status_vout(b))
                }
                PmbusCommand::StatusIout => {
                    decode_status_byte(data, |b| pmbus::StatusDecoder::decode_status_iout(b))
                }
                PmbusCommand::StatusInput => {
                    decode_status_byte(data, |b| pmbus::StatusDecoder::decode_status_input(b))
                }
                PmbusCommand::StatusTemperature => {
                    decode_status_byte(data, |b| pmbus::StatusDecoder::decode_status_temp(b))
                }
                PmbusCommand::StatusCml => {
                    decode_status_byte(data, |b| pmbus::StatusDecoder::decode_status_cml(b))
                }

                // Linear11 format readings (voltage, current, temperature)
                PmbusCommand::ReadVin => decode_linear11_voltage(data),
                PmbusCommand::ReadIout => decode_linear11_current(data),
                PmbusCommand::ReadTemperature1 => decode_linear11_temperature(data),

                // Linear11 voltage configuration reads
                PmbusCommand::VinOn
                | PmbusCommand::VinOff
                | PmbusCommand::VinOvFaultLimit
                | PmbusCommand::VinUvWarnLimit => decode_linear11_voltage(data),

                // Linear11 current configuration reads
                PmbusCommand::IoutOcWarnLimit | PmbusCommand::IoutOcFaultLimit => {
                    decode_linear11_current(data)
                }

                // Linear16 format readings (requires VOUT_MODE context)
                PmbusCommand::ReadVout => decode_linear16_voltage(data),

                // Single byte values
                PmbusCommand::Page => decode_page(data),
                PmbusCommand::Operation => decode_operation_mode(data),
                PmbusCommand::OnOffConfig => decode_on_off_config(data),
                PmbusCommand::VoutMode => decode_vout_mode(data),
                PmbusCommand::Phase => decode_phase(data),
                PmbusCommand::Capability => decode_capability(data),

                // Data-less write-only command
                PmbusCommand::ClearFaults => decode_clear_faults_read(data),

                // Fault response bytes
                PmbusCommand::VinOvFaultResponse
                | PmbusCommand::IoutOcFaultResponse
                | PmbusCommand::OtFaultResponse
                | PmbusCommand::TonMaxFaultResponse => decode_fault_response(data),

                // Default: show raw hex for known commands without specific decoders
                _ => format!("{:02x?}", data),
            }
        } else {
            // Unknown command code
            format!("{:02x?}", data)
        }
    }

    /// Decode PMBus write operation value
    fn decode_write_value(cmd: u8, data: &[u8]) -> String {
        if let Some(command) = PmbusCommand::from_u8(cmd) {
            match command {
                // Single byte commands
                PmbusCommand::Page => {
                    if data.len() >= 1 {
                        let page = data[0];
                        let desc = match page {
                            0x00 => "page 0",
                            0xFF => "all pages (multicast)",
                            p => return format!("{:02x?} (page {})", data, p),
                        };
                        format!("{:02x?} ({})", data, desc)
                    } else {
                        format!("{:02x?}", data)
                    }
                }

                PmbusCommand::Operation => {
                    if data.len() >= 1 {
                        let op = data[0];
                        let desc = match op {
                            pmbus::operation::OFF_IMMEDIATE => "OFF",
                            pmbus::operation::SOFT_OFF => "SOFT_OFF",
                            pmbus::operation::ON => "ON",
                            pmbus::operation::ON_MARGIN_LOW => "ON_MARGIN_LOW",
                            pmbus::operation::ON_MARGIN_HIGH => "ON_MARGIN_HIGH",
                            _ => "unknown",
                        };
                        format!("{:02x?} ({})", data, desc)
                    } else {
                        format!("{:02x?}", data)
                    }
                }

                PmbusCommand::OnOffConfig => {
                    if data.len() >= 1 {
                        let config = data[0];
                        let mut flags = Vec::new();
                        if config & pmbus::on_off_config::PU != 0 {
                            flags.push("PU");
                        }
                        if config & pmbus::on_off_config::CMD != 0 {
                            flags.push("CMD");
                        }
                        if config & pmbus::on_off_config::CP != 0 {
                            flags.push("CP");
                        }
                        if config & pmbus::on_off_config::POLARITY != 0 {
                            flags.push("POL");
                        }
                        if config & pmbus::on_off_config::DELAY != 0 {
                            flags.push("DELAY");
                        }

                        if flags.is_empty() {
                            format!("{:02x?}", data)
                        } else {
                            format!("{:02x?} ({})", data, flags.join(" | "))
                        }
                    } else {
                        format!("{:02x?}", data)
                    }
                }

                PmbusCommand::VoutMode => {
                    if data.len() >= 1 {
                        let mode = data[0];
                        let exponent = (mode & 0x1F) as i8;
                        let exp_signed = if exponent & 0x10 != 0 {
                            ((exponent as u8) | 0xE0u8) as i8
                        } else {
                            exponent
                        };
                        format!("{:02x?} (exp={})", data, exp_signed)
                    } else {
                        format!("{:02x?}", data)
                    }
                }

                PmbusCommand::Phase => decode_phase(data),

                // Voltage values (Linear11 format)
                PmbusCommand::VinOn
                | PmbusCommand::VinOff
                | PmbusCommand::VinOvFaultLimit
                | PmbusCommand::VinUvWarnLimit => decode_write_linear11_voltage(data),

                // Current values (Linear11 format)
                PmbusCommand::IoutOcWarnLimit | PmbusCommand::IoutOcFaultLimit => {
                    decode_write_linear11_current(data)
                }

                // Temperature values (Linear11 format)
                PmbusCommand::OtWarnLimit | PmbusCommand::OtFaultLimit => {
                    decode_write_linear11(data)
                } // TODO: Add temperature units

                // Frequency in kHz (Linear11 format)
                PmbusCommand::FrequencySwitch => decode_write_frequency(data),

                // Two-byte Linear16 values (VOUT commands - need VOUT_MODE context)
                PmbusCommand::VoutCommand
                | PmbusCommand::VoutMax
                | PmbusCommand::VoutMin
                | PmbusCommand::VoutMarginHigh
                | PmbusCommand::VoutMarginLow
                | PmbusCommand::VoutOvFaultLimit
                | PmbusCommand::VoutOvWarnLimit
                | PmbusCommand::VoutUvWarnLimit
                | PmbusCommand::VoutUvFaultLimit => {
                    format!("{:02x?} (Linear16 - needs VOUT_MODE)", data)
                }

                // Timing values (Linear11 format, in milliseconds)
                PmbusCommand::TonDelay
                | PmbusCommand::TonRise
                | PmbusCommand::TonMaxFaultLimit
                | PmbusCommand::ToffDelay
                | PmbusCommand::ToffFall => decode_write_linear11_time(data),

                // Data-less write-only command
                PmbusCommand::ClearFaults => decode_clear_faults_write(data),

                // Fault response bytes
                PmbusCommand::VinOvFaultResponse
                | PmbusCommand::IoutOcFaultResponse
                | PmbusCommand::OtFaultResponse
                | PmbusCommand::TonMaxFaultResponse => {
                    if data.len() >= 1 {
                        let response = data[0];
                        let desc = pmbus::StatusDecoder::decode_fault_response(response);
                        format!("{:02x?} ({})", data, desc)
                    } else {
                        format!("{:02x?}", data)
                    }
                }

                // Default: show raw hex
                _ => format!("{:02x?}", data),
            }
        } else {
            // Unknown command code
            format!("{:02x?}", data)
        }
    }

    /// Decode string data (manufacturer ID, model, revision)
    fn decode_string_data(data: &[u8]) -> String {
        if data.is_empty() {
            return "empty".to_string();
        }

        // PMBus block reads have length byte first, then data
        let actual_data = if data.len() > 1 && data[0] as usize == data.len() - 1 {
            &data[1..] // Skip length byte
        } else {
            data
        };

        // Check if data contains printable ASCII
        let has_printable = actual_data.iter().any(|&b| b.is_ascii_graphic());

        if has_printable {
            let text: String = actual_data
                .iter()
                .map(|&b| {
                    if b.is_ascii_graphic() || b == b' ' {
                        b as char
                    } else {
                        '.'
                    }
                })
                .collect();
            format!("{:02x?} (\"{}\")", data, text.trim())
        } else {
            // All non-printable, just show hex
            format!("{:02x?} (binary data)", data)
        }
    }

    /// Decode status byte with custom decoder
    fn decode_status_byte<F>(data: &[u8], decoder: F) -> String
    where
        F: Fn(u8) -> Vec<&'static str>,
    {
        if data.len() >= 1 {
            let status = data[0];
            let flags = decoder(status);
            if flags.is_empty() {
                format!("0x{:02x}", status)
            } else {
                format!("0x{:02x} ({})", status, flags.join(" | "))
            }
        } else {
            format!("{:02x?}", data)
        }
    }

    /// Decode Linear11 voltage reading
    fn decode_linear11_voltage(data: &[u8]) -> String {
        if data.len() >= 2 {
            let value = u16::from_le_bytes([data[0], data[1]]);
            let voltage = pmbus::Linear11::to_float(value);
            format!("{:02x?} ({:.3}V)", data, voltage)
        } else {
            format!("{:02x?}", data)
        }
    }

    /// Decode Linear11 current reading
    fn decode_linear11_current(data: &[u8]) -> String {
        if data.len() >= 2 {
            let value = u16::from_le_bytes([data[0], data[1]]);
            let current = pmbus::Linear11::to_float(value);
            format!("{:02x?} ({:.3}A)", data, current)
        } else {
            format!("{:02x?}", data)
        }
    }

    /// Decode Linear11 temperature reading
    fn decode_linear11_temperature(data: &[u8]) -> String {
        if data.len() >= 2 {
            let value = u16::from_le_bytes([data[0], data[1]]);
            let temp = pmbus::Linear11::to_int(value);
            format!("{:02x?} ({}°C)", data, temp)
        } else {
            format!("{:02x?}", data)
        }
    }

    /// Decode Linear16 voltage reading (needs VOUT_MODE - show raw for now)
    fn decode_linear16_voltage(data: &[u8]) -> String {
        if data.len() >= 2 {
            let value = u16::from_le_bytes([data[0], data[1]]);
            // For now, show raw value since we need VOUT_MODE context
            // This will be enhanced in the context-aware phase
            format!("{:02x?} (0x{:04x} Linear16)", data, value)
        } else {
            format!("{:02x?}", data)
        }
    }

    /// Decode Linear16 voltage reading with VOUT_MODE context
    fn decode_linear16_voltage_with_context(data: &[u8], vout_mode: Option<u8>) -> String {
        if data.len() >= 2 {
            let value = u16::from_le_bytes([data[0], data[1]]);

            if let Some(mode) = vout_mode {
                let voltage = pmbus::Linear16::to_float(value, mode);
                format!("{:02x?} ({:.3}V)", data, voltage)
            } else {
                format!("{:02x?} (0x{:04x} Linear16 - no VOUT_MODE)", data, value)
            }
        } else {
            format!("{:02x?}", data)
        }
    }

    /// Decode Linear16 write value with VOUT_MODE context
    fn decode_write_linear16_with_context(data: &[u8], vout_mode: Option<u8>) -> String {
        if data.len() >= 2 {
            let value = u16::from_le_bytes([data[0], data[1]]);

            if let Some(mode) = vout_mode {
                let voltage = pmbus::Linear16::to_float(value, mode);
                format!("{:02x?} ({:.3}V)", data, voltage)
            } else {
                format!("{:02x?} (Linear16 - no VOUT_MODE)", data)
            }
        } else {
            format!("{:02x?}", data)
        }
    }

    /// Decode operation mode
    fn decode_operation_mode(data: &[u8]) -> String {
        if data.len() >= 1 {
            let op = data[0];
            let desc = match op {
                pmbus::operation::OFF_IMMEDIATE => "OFF",
                pmbus::operation::SOFT_OFF => "SOFT_OFF",
                pmbus::operation::ON => "ON",
                pmbus::operation::ON_MARGIN_LOW => "ON_MARGIN_LOW",
                pmbus::operation::ON_MARGIN_HIGH => "ON_MARGIN_HIGH",
                _ => "unknown",
            };
            format!("0x{:02x} ({})", op, desc)
        } else {
            format!("{:02x?}", data)
        }
    }

    /// Decode ON_OFF_CONFIG register
    fn decode_on_off_config(data: &[u8]) -> String {
        if data.len() >= 1 {
            let config = data[0];
            let mut flags = Vec::new();
            if config & pmbus::on_off_config::PU != 0 {
                flags.push("PowerUp");
            }
            if config & pmbus::on_off_config::CMD != 0 {
                flags.push("CMD");
            }
            if config & pmbus::on_off_config::CP != 0 {
                flags.push("CONTROL_pin");
            }
            if config & pmbus::on_off_config::POLARITY != 0 {
                flags.push("Active_high");
            }
            if config & pmbus::on_off_config::DELAY != 0 {
                flags.push("Turn-off_delay");
            }

            if flags.is_empty() {
                format!("0x{:02x}", config)
            } else {
                format!("0x{:02x} ({})", config, flags.join(" | "))
            }
        } else {
            format!("{:02x?}", data)
        }
    }

    /// Decode VOUT_MODE register
    fn decode_vout_mode(data: &[u8]) -> String {
        if data.len() >= 1 {
            let mode = data[0];
            let exponent = (mode & 0x1F) as i8;
            let exp_signed = if exponent & 0x10 != 0 {
                ((exponent as u8) | 0xE0u8) as i8
            } else {
                exponent
            };
            format!("0x{:02x} (Linear16, exp={})", mode, exp_signed)
        } else {
            format!("{:02x?}", data)
        }
    }

    /// Decode PAGE register (PMBus page selection)
    fn decode_page(data: &[u8]) -> String {
        if data.len() >= 1 {
            let page = data[0];
            match page {
                0x00 => format!("0x{:02x} (page 0)", page),
                0xFF => format!("0x{:02x} (all pages - multicast)", page),
                p => format!("0x{:02x} (page {})", page, p),
            }
        } else {
            format!("{:02x?}", data)
        }
    }

    /// Decode PHASE register
    fn decode_phase(data: &[u8]) -> String {
        if data.len() >= 1 {
            let phase = data[0];
            match phase {
                0xFF => format!("[{:02x}] (all phases)", phase),
                0x00 => format!("[{:02x}] (master/phase 0)", phase),
                p => format!("[{:02x}] (phase {})", phase, p),
            }
        } else {
            format!("{:02x?}", data)
        }
    }

    /// Decode CAPABILITY register
    fn decode_capability(data: &[u8]) -> String {
        if data.len() >= 1 {
            let cap = data[0];
            let mut flags = Vec::new();
            if cap & 0x80 != 0 {
                flags.push("PEC");
            }
            if cap & 0x40 != 0 {
                flags.push("400kHz");
            }
            if cap & 0x20 != 0 {
                flags.push("Alert");
            }

            if flags.is_empty() {
                format!("0x{:02x}", cap)
            } else {
                format!("0x{:02x} ({})", cap, flags.join(" | "))
            }
        } else {
            format!("{:02x?}", data)
        }
    }

    /// Decode fault response byte
    fn decode_fault_response(data: &[u8]) -> String {
        if data.len() >= 1 {
            let response = data[0];
            let desc = pmbus::StatusDecoder::decode_fault_response(response);
            format!("0x{:02x} ({})", response, desc)
        } else {
            format!("{:02x?}", data)
        }
    }

    /// Decode Linear11 write value (voltage/current)
    fn decode_write_linear11(data: &[u8]) -> String {
        if data.len() >= 2 {
            let value = u16::from_le_bytes([data[0], data[1]]);
            let decoded = pmbus::Linear11::to_float(value);
            format!("{:02x?} ({:.3})", data, decoded)
        } else {
            format!("{:02x?}", data)
        }
    }

    /// Decode Linear11 time value (milliseconds)
    fn decode_write_linear11_time(data: &[u8]) -> String {
        if data.len() >= 2 {
            let value = u16::from_le_bytes([data[0], data[1]]);
            let time_ms = pmbus::Linear11::to_int(value);
            format!("{:02x?} ({}ms)", data, time_ms)
        } else {
            format!("{:02x?}", data)
        }
    }

    /// Decode Linear11 voltage value
    fn decode_write_linear11_voltage(data: &[u8]) -> String {
        if data.len() >= 2 {
            let value = u16::from_le_bytes([data[0], data[1]]);
            let decoded = pmbus::Linear11::to_float(value);
            format!("{:02x?} ({:.3}V)", data, decoded)
        } else {
            format!("{:02x?}", data)
        }
    }

    /// Decode Linear11 current value
    fn decode_write_linear11_current(data: &[u8]) -> String {
        if data.len() >= 2 {
            let value = u16::from_le_bytes([data[0], data[1]]);
            let decoded = pmbus::Linear11::to_float(value);
            format!("{:02x?} ({:.3}A)", data, decoded)
        } else {
            format!("{:02x?}", data)
        }
    }

    /// Decode CLEAR_FAULTS read operation (should not exist)
    fn decode_clear_faults_read(data: &[u8]) -> String {
        format!(
            "{:02x?} (ERROR: CLEAR_FAULTS is write-only per PMBus spec)",
            data
        )
    }

    /// Decode CLEAR_FAULTS write operation (should be data-less)
    fn decode_clear_faults_write(data: &[u8]) -> String {
        if data.is_empty() {
            "data-less command (clears all fault bits)".to_string()
        } else {
            format!(
                "{:02x?} (ERROR: CLEAR_FAULTS should be data-less per PMBus spec)",
                data
            )
        }
    }

    /// Decode FREQUENCY_SWITCH (Linear11 format, units in kHz)
    fn decode_write_frequency(data: &[u8]) -> String {
        if data.len() >= 2 {
            let value = u16::from_le_bytes([data[0], data[1]]);
            let freq_khz = pmbus::Linear11::to_float(value);
            format!("{:02x?} ({:.0}kHz)", data, freq_khz)
        } else {
            format!("{:02x?}", data)
        }
    }
}

// Use protocol constants for driver
use protocol::{DEFAULT_ADDRESS as TPS546_I2C_ADDR, DEVICE_ID1, DEVICE_ID2, DEVICE_ID3};

/// TPS546 configuration parameters
#[derive(Debug, Clone)]
pub struct Tps546Config {
    /// Input voltage turn-on threshold (V)
    pub vin_on: f32,
    /// Input voltage turn-off threshold (V)
    pub vin_off: f32,
    /// Input undervoltage warning limit (V)
    pub vin_uv_warn_limit: f32,
    /// Input overvoltage fault limit (V)
    pub vin_ov_fault_limit: f32,
    /// Output voltage scale factor
    pub vout_scale_loop: f32,
    /// Minimum output voltage (V)
    pub vout_min: f32,
    /// Maximum output voltage (V)
    pub vout_max: f32,
    /// Initial output voltage command (V)
    pub vout_command: f32,
    /// Output current overcurrent warning limit (A)
    pub iout_oc_warn_limit: f32,
    /// Output current overcurrent fault limit (A)
    pub iout_oc_fault_limit: f32,
}

impl Tps546Config {
    /// Configuration for Bitaxe Gamma (single ASIC)
    pub fn bitaxe_gamma() -> Self {
        Self {
            vin_on: 4.8,
            vin_off: 4.5,
            vin_uv_warn_limit: 0.0, // Disabled due to TI bug
            vin_ov_fault_limit: 6.5,
            vout_scale_loop: 0.25,
            vout_min: 1.0,
            vout_max: 2.0,
            vout_command: 1.15, // BM1370 default voltage
            iout_oc_warn_limit: 25.0,
            iout_oc_fault_limit: 30.0,
        }
    }
}

/// TPS546 error types
#[derive(Error, Debug)]
pub enum Tps546Error {
    #[error("Device ID mismatch")]
    DeviceIdMismatch,
    #[error("Voltage out of range: {0:.2}V (min: {1:.2}V, max: {2:.2}V)")]
    VoltageOutOfRange(f32, f32, f32),
    #[error("PMBus fault detected: {0}")]
    FaultDetected(String),
}

/// TPS546D24A driver
pub struct Tps546<I2C> {
    i2c: I2C,
    config: Tps546Config,
}

impl<I2C: I2c> Tps546<I2C> {
    /// Create a new TPS546 instance
    pub fn new(i2c: I2C, config: Tps546Config) -> Self {
        Self { i2c, config }
    }

    /// Initialize the TPS546
    pub async fn init(&mut self) -> Result<()> {
        debug!("Initializing TPS546D24A power regulator");

        // First verify device ID to ensure I2C communication is working
        self.verify_device_id().await?;

        // Turn off output during configuration
        self.write_byte(PmbusCommand::Operation, pmbus::operation::OFF_IMMEDIATE)
            .await?;
        debug!("Power output turned off");

        // Configure ON_OFF_CONFIG immediately after turning off (esp-miner sequence)
        // Using same configuration as esp-miner - both CONTROL pin and OPERATION command
        let on_off_val = pmbus::on_off_config::DELAY
            | pmbus::on_off_config::POLARITY
            | pmbus::on_off_config::CP
            | pmbus::on_off_config::CMD
            | pmbus::on_off_config::PU;
        self.write_byte(PmbusCommand::OnOffConfig, on_off_val)
            .await?;
        let mut config_desc = Vec::new();
        if on_off_val & pmbus::on_off_config::PU != 0 {
            config_desc.push("PowerUp from CONTROL");
        }
        if on_off_val & pmbus::on_off_config::CMD != 0 {
            config_desc.push("OPERATION cmd enabled");
        }
        if on_off_val & pmbus::on_off_config::CP != 0 {
            config_desc.push("CONTROL pin present");
        }
        if on_off_val & pmbus::on_off_config::POLARITY != 0 {
            config_desc.push("Active high");
        }
        if on_off_val & pmbus::on_off_config::DELAY != 0 {
            config_desc.push("Turn-off delay enabled");
        }
        debug!(
            "ON_OFF_CONFIG set to 0x{:02X} ({})",
            on_off_val,
            config_desc.join(", ")
        );

        // Read VOUT_MODE to verify data format (esp-miner does this)
        let vout_mode = self.read_byte(PmbusCommand::VoutMode).await?;
        debug!("VOUT_MODE: 0x{:02X}", vout_mode);

        // Write entire configuration like esp-miner does
        self.write_config().await?;

        // Read back STATUS_WORD for verification
        let status = self.read_word(PmbusCommand::StatusWord).await?;
        let status_desc = self.decode_status_word(status);
        if status_desc.is_empty() {
            debug!("STATUS_WORD after config: 0x{:04X}", status);
        } else {
            debug!(
                "STATUS_WORD after config: 0x{:04X} ({})",
                status,
                status_desc.join(", ")
            );
        }

        Ok(())
    }

    /// Write all configuration parameters
    async fn write_config(&mut self) -> Result<()> {
        trace!("---Writing new config values to TPS546---");

        // Phase configuration
        trace!("Setting PHASE: 00");
        self.write_byte(PmbusCommand::Phase, 0x00).await?;

        // Switching frequency (650 kHz)
        trace!("Setting FREQUENCY: 650kHz");
        self.write_word(PmbusCommand::FrequencySwitch, self.int_to_slinear11(650))
            .await?;

        // Input voltage thresholds (handle UV_WARN_LIMIT bug like esp-miner)
        if self.config.vin_uv_warn_limit > 0.0 {
            trace!(
                "Setting VIN_UV_WARN_LIMIT: {:.2}V",
                self.config.vin_uv_warn_limit
            );
            self.write_word(
                PmbusCommand::VinUvWarnLimit,
                self.float_to_slinear11(self.config.vin_uv_warn_limit),
            )
            .await?;
        }

        trace!("Setting VIN_ON: {:.2}V", self.config.vin_on);
        self.write_word(
            PmbusCommand::VinOn,
            self.float_to_slinear11(self.config.vin_on),
        )
        .await?;

        trace!("Setting VIN_OFF: {:.2}V", self.config.vin_off);
        self.write_word(
            PmbusCommand::VinOff,
            self.float_to_slinear11(self.config.vin_off),
        )
        .await?;

        trace!(
            "Setting VIN_OV_FAULT_LIMIT: {:.2}V",
            self.config.vin_ov_fault_limit
        );
        self.write_word(
            PmbusCommand::VinOvFaultLimit,
            self.float_to_slinear11(self.config.vin_ov_fault_limit),
        )
        .await?;

        // VIN_OV_FAULT_RESPONSE (0xB7 = shutdown with 4 retries, 182ms delay)
        const VIN_OV_FAULT_RESPONSE: u8 = 0xB7;
        trace!(
            "Setting VIN_OV_FAULT_RESPONSE: 0x{:02X} (shutdown, 4 retries, 182ms delay)",
            VIN_OV_FAULT_RESPONSE
        );
        self.write_byte(PmbusCommand::VinOvFaultResponse, VIN_OV_FAULT_RESPONSE)
            .await?;

        // Output voltage configuration
        trace!("Setting VOUT SCALE: {:.2}", self.config.vout_scale_loop);
        self.write_word(
            PmbusCommand::VoutScaleLoop,
            self.float_to_slinear11(self.config.vout_scale_loop),
        )
        .await?;

        trace!("Setting VOUT_COMMAND: {:.2}V", self.config.vout_command);
        let vout_command = self.float_to_ulinear16(self.config.vout_command).await?;
        self.write_word(PmbusCommand::VoutCommand, vout_command)
            .await?;

        trace!("Setting VOUT_MAX: {:.2}V", self.config.vout_max);
        let vout_max = self.float_to_ulinear16(self.config.vout_max).await?;
        self.write_word(PmbusCommand::VoutMax, vout_max).await?;

        trace!("Setting VOUT_MIN: {:.2}V", self.config.vout_min);
        let vout_min = self.float_to_ulinear16(self.config.vout_min).await?;
        self.write_word(PmbusCommand::VoutMin, vout_min).await?;

        // Output voltage protection
        const VOUT_OV_FAULT_LIMIT: f32 = 1.25; // 125% of VOUT_COMMAND
        const VOUT_OV_WARN_LIMIT: f32 = 1.16; // 116% of VOUT_COMMAND
        const VOUT_MARGIN_HIGH: f32 = 1.10; // 110% of VOUT_COMMAND
        const VOUT_MARGIN_LOW: f32 = 0.90; // 90% of VOUT_COMMAND
        const VOUT_UV_WARN_LIMIT: f32 = 0.90; // 90% of VOUT_COMMAND
        const VOUT_UV_FAULT_LIMIT: f32 = 0.75; // 75% of VOUT_COMMAND

        trace!("Setting VOUT_OV_FAULT_LIMIT: {:.2}", VOUT_OV_FAULT_LIMIT);
        let vout_ov_fault = self.float_to_ulinear16(VOUT_OV_FAULT_LIMIT).await?;
        self.write_word(PmbusCommand::VoutOvFaultLimit, vout_ov_fault)
            .await?;

        trace!("Setting VOUT_OV_WARN_LIMIT: {:.2}", VOUT_OV_WARN_LIMIT);
        let vout_ov_warn = self.float_to_ulinear16(VOUT_OV_WARN_LIMIT).await?;
        self.write_word(PmbusCommand::VoutOvWarnLimit, vout_ov_warn)
            .await?;

        trace!("Setting VOUT_MARGIN_HIGH: {:.2}", VOUT_MARGIN_HIGH);
        let vout_margin_high = self.float_to_ulinear16(VOUT_MARGIN_HIGH).await?;
        self.write_word(PmbusCommand::VoutMarginHigh, vout_margin_high)
            .await?;

        trace!("Setting VOUT_MARGIN_LOW: {:.2}", VOUT_MARGIN_LOW);
        let vout_margin_low = self.float_to_ulinear16(VOUT_MARGIN_LOW).await?;
        self.write_word(PmbusCommand::VoutMarginLow, vout_margin_low)
            .await?;

        trace!("Setting VOUT_UV_WARN_LIMIT: {:.2}", VOUT_UV_WARN_LIMIT);
        let vout_uv_warn = self.float_to_ulinear16(VOUT_UV_WARN_LIMIT).await?;
        self.write_word(PmbusCommand::VoutUvWarnLimit, vout_uv_warn)
            .await?;

        trace!("Setting VOUT_UV_FAULT_LIMIT: {:.2}", VOUT_UV_FAULT_LIMIT);
        let vout_uv_fault = self.float_to_ulinear16(VOUT_UV_FAULT_LIMIT).await?;
        self.write_word(PmbusCommand::VoutUvFaultLimit, vout_uv_fault)
            .await?;

        // Output current protection
        trace!("----- IOUT");
        trace!(
            "Setting IOUT_OC_WARN_LIMIT: {:.2}A",
            self.config.iout_oc_warn_limit
        );
        self.write_word(
            PmbusCommand::IoutOcWarnLimit,
            self.float_to_slinear11(self.config.iout_oc_warn_limit),
        )
        .await?;

        trace!(
            "Setting IOUT_OC_FAULT_LIMIT: {:.2}A",
            self.config.iout_oc_fault_limit
        );
        self.write_word(
            PmbusCommand::IoutOcFaultLimit,
            self.float_to_slinear11(self.config.iout_oc_fault_limit),
        )
        .await?;

        // IOUT_OC_FAULT_RESPONSE (0xC0 = shutdown immediately, no retries)
        const IOUT_OC_FAULT_RESPONSE: u8 = 0xC0;
        trace!(
            "Setting IOUT_OC_FAULT_RESPONSE: 0x{:02X} (shutdown immediately, no retries)",
            IOUT_OC_FAULT_RESPONSE
        );
        self.write_byte(PmbusCommand::IoutOcFaultResponse, IOUT_OC_FAULT_RESPONSE)
            .await?;

        // Temperature protection
        trace!("----- TEMPERATURE");
        const OT_WARN_LIMIT: i32 = 105; // °C
        const OT_FAULT_LIMIT: i32 = 145; // °C
        const OT_FAULT_RESPONSE: u8 = 0xFF; // Infinite retries

        trace!("Setting OT_WARN_LIMIT: {}°C", OT_WARN_LIMIT);
        self.write_word(
            PmbusCommand::OtWarnLimit,
            self.int_to_slinear11(OT_WARN_LIMIT),
        )
        .await?;
        trace!("Setting OT_FAULT_LIMIT: {}°C", OT_FAULT_LIMIT);
        self.write_word(
            PmbusCommand::OtFaultLimit,
            self.int_to_slinear11(OT_FAULT_LIMIT),
        )
        .await?;
        trace!(
            "Setting OT_FAULT_RESPONSE: 0x{:02X} (infinite retries, wait for cooling)",
            OT_FAULT_RESPONSE
        );
        self.write_byte(PmbusCommand::OtFaultResponse, OT_FAULT_RESPONSE)
            .await?;

        // Timing configuration
        trace!("----- TIMING");
        const TON_DELAY: i32 = 0;
        const TON_RISE: i32 = 3;
        const TON_MAX_FAULT_LIMIT: i32 = 0;
        const TON_MAX_FAULT_RESPONSE: u8 = 0x3B; // 3 retries, 91ms delay
        const TOFF_DELAY: i32 = 0;
        const TOFF_FALL: i32 = 0;

        trace!("Setting TON_DELAY: {}ms", TON_DELAY);
        self.write_word(PmbusCommand::TonDelay, self.int_to_slinear11(TON_DELAY))
            .await?;
        trace!("Setting TON_RISE: {}ms", TON_RISE);
        self.write_word(PmbusCommand::TonRise, self.int_to_slinear11(TON_RISE))
            .await?;
        trace!("Setting TON_MAX_FAULT_LIMIT: {}ms", TON_MAX_FAULT_LIMIT);
        self.write_word(
            PmbusCommand::TonMaxFaultLimit,
            self.int_to_slinear11(TON_MAX_FAULT_LIMIT),
        )
        .await?;
        trace!(
            "Setting TON_MAX_FAULT_RESPONSE: 0x{:02X} (3 retries, 91ms delay)",
            TON_MAX_FAULT_RESPONSE
        );
        self.write_byte(PmbusCommand::TonMaxFaultResponse, TON_MAX_FAULT_RESPONSE)
            .await?;
        trace!("Setting TOFF_DELAY: {}ms", TOFF_DELAY);
        self.write_word(PmbusCommand::ToffDelay, self.int_to_slinear11(TOFF_DELAY))
            .await?;
        trace!("Setting TOFF_FALL: {}ms", TOFF_FALL);
        self.write_word(PmbusCommand::ToffFall, self.int_to_slinear11(TOFF_FALL))
            .await?;

        // Pin detect override
        trace!("Setting PIN_DETECT_OVERRIDE");
        const PIN_DETECT_OVERRIDE: u16 = 0xFFFF;
        self.write_word(PmbusCommand::PinDetectOverride, PIN_DETECT_OVERRIDE)
            .await?;

        debug!("TPS546 configuration written successfully");
        Ok(())
    }

    /// Verify the device ID
    async fn verify_device_id(&mut self) -> Result<()> {
        let mut id_data = vec![0u8; 7]; // Length byte + 6 ID bytes
        self.i2c
            .write_read(
                TPS546_I2C_ADDR,
                &[PmbusCommand::IcDeviceId.as_u8()],
                &mut id_data,
            )
            .await?;

        // First byte is length, actual ID starts at byte 1
        let device_id = &id_data[1..7];
        debug!(
            "Device ID: {:02X} {:02X} {:02X} {:02X} {:02X} {:02X}",
            device_id[0], device_id[1], device_id[2], device_id[3], device_id[4], device_id[5]
        );

        if device_id != DEVICE_ID1 && device_id != DEVICE_ID2 && device_id != DEVICE_ID3 {
            error!("Device ID mismatch");
            bail!(Tps546Error::DeviceIdMismatch);
        }

        Ok(())
    }

    /// Clear all faults
    pub async fn clear_faults(&mut self) -> Result<()> {
        self.i2c
            .write(TPS546_I2C_ADDR, &[PmbusCommand::ClearFaults.as_u8()])
            .await?;
        Ok(())
    }

    /// Set output voltage
    pub async fn set_vout(&mut self, volts: f32) -> Result<()> {
        if volts == 0.0 {
            // Turn off output
            self.write_byte(PmbusCommand::Operation, pmbus::operation::OFF_IMMEDIATE)
                .await?;
            info!("Output voltage turned off");
        } else {
            // Check voltage range
            if volts < self.config.vout_min || volts > self.config.vout_max {
                bail!(Tps546Error::VoltageOutOfRange(
                    volts,
                    self.config.vout_min,
                    self.config.vout_max
                ));
            }

            // Set voltage
            let value = self.float_to_ulinear16(volts).await?;
            self.write_word(PmbusCommand::VoutCommand, value).await?;
            debug!("Output voltage set to {:.2}V", volts);

            // Clear any faults before turning on
            self.clear_faults().await?;
            debug!("Cleared faults before turn-on");

            // Turn on output
            self.write_byte(PmbusCommand::Operation, pmbus::operation::ON)
                .await?;

            // Verify operation
            let op_val = self.read_byte(PmbusCommand::Operation).await?;
            if op_val != pmbus::operation::ON {
                error!("Failed to turn on output, OPERATION = 0x{:02X}", op_val);
            } else {
                debug!("Power turned ON successfully, OPERATION = 0x{:02X}", op_val);
            }

            // Check immediate status after turn-on
            let status = self.read_word(PmbusCommand::StatusWord).await?;
            if status & pmbus::status_word::OFF != 0 {
                error!(
                    "WARNING: Power is still OFF after turn-on command! STATUS_WORD = 0x{:04X}",
                    status
                );
                // Read all status registers to understand why
                if let Ok(vout_status) = self.read_byte(PmbusCommand::StatusVout).await {
                    error!("  STATUS_VOUT: 0x{:02X}", vout_status);
                }
                if let Ok(iout_status) = self.read_byte(PmbusCommand::StatusIout).await {
                    error!("  STATUS_IOUT: 0x{:02X}", iout_status);
                }
            } else {
                debug!("Power is ON, STATUS_WORD = 0x{:04X}", status);
            }
        }
        Ok(())
    }

    /// Read input voltage in millivolts
    pub async fn get_vin(&mut self) -> Result<u32> {
        let value = self.read_word(PmbusCommand::ReadVin).await?;
        let volts = self.slinear11_to_float(value);
        Ok((volts * 1000.0) as u32)
    }

    /// Read output voltage in millivolts
    pub async fn get_vout(&mut self) -> Result<u32> {
        let value = self.read_word(PmbusCommand::ReadVout).await?;
        let volts = self.ulinear16_to_float(value).await?;
        Ok((volts * 1000.0) as u32)
    }

    /// Read output current in milliamps
    pub async fn get_iout(&mut self) -> Result<u32> {
        // Set phase to 0xFF to read all phases
        self.write_byte(PmbusCommand::Phase, 0xFF).await?;

        let value = self.read_word(PmbusCommand::ReadIout).await?;
        let amps = self.slinear11_to_float(value);
        Ok((amps * 1000.0) as u32)
    }

    /// Read temperature in degrees Celsius
    pub async fn get_temperature(&mut self) -> Result<i32> {
        let value = self.read_word(PmbusCommand::ReadTemperature1).await?;
        Ok(self.slinear11_to_int(value))
    }

    /// Calculate power in milliwatts
    pub async fn get_power(&mut self) -> Result<u32> {
        let vout_mv = self.get_vout().await?;
        let iout_ma = self.get_iout().await?;
        let power_mw = (vout_mv as u64 * iout_ma as u64) / 1000;
        Ok(power_mw as u32)
    }

    /// Check and report status
    pub async fn check_status(&mut self) -> Result<()> {
        let status = self.read_word(PmbusCommand::StatusWord).await?;

        if status == 0 {
            return Ok(());
        }

        // Track if we have critical faults that should fail the check
        let mut critical_faults = Vec::new();
        let mut warnings = Vec::new();

        // Check for output voltage faults (critical)
        if status & pmbus::status_word::VOUT != 0 {
            let vout_status = self.read_byte(PmbusCommand::StatusVout).await?;
            let desc = self.decode_status_vout(vout_status);

            // OV and UV faults are critical - the output is not within safe operating range
            if vout_status & (pmbus::status_vout::VOUT_OV_FAULT | pmbus::status_vout::VOUT_UV_FAULT)
                != 0
            {
                error!(
                    "CRITICAL: VOUT fault detected: 0x{:02X} ({})",
                    vout_status,
                    desc.join(", ")
                );
                critical_faults.push(format!("VOUT fault: {}", desc.join(", ")));
            } else {
                warn!("VOUT warning: 0x{:02X} ({})", vout_status, desc.join(", "));
                warnings.push(format!("VOUT warning: {}", desc.join(", ")));
            }
        }

        // Check for output current faults (critical)
        if status & pmbus::status_word::IOUT != 0 {
            let iout_status = self.read_byte(PmbusCommand::StatusIout).await?;
            let desc = self.decode_status_iout(iout_status);

            // Overcurrent fault is critical - can damage hardware
            if iout_status & pmbus::status_iout::IOUT_OC_FAULT != 0 {
                error!(
                    "CRITICAL: IOUT overcurrent fault detected: 0x{:02X} ({})",
                    iout_status,
                    desc.join(", ")
                );
                critical_faults.push(format!("IOUT overcurrent: {}", desc.join(", ")));
            } else {
                warn!("IOUT warning: 0x{:02X} ({})", iout_status, desc.join(", "));
                warnings.push(format!("IOUT warning: {}", desc.join(", ")));
            }
        }

        // Check for input voltage faults (critical if unit is off)
        if status & pmbus::status_word::INPUT != 0 {
            let input_status = self.read_byte(PmbusCommand::StatusInput).await?;
            let desc = self.decode_status_input(input_status);

            // Unit off due to low input or UV/OV faults are critical
            if input_status
                & (pmbus::status_input::UNIT_OFF_VIN_LOW
                    | pmbus::status_input::VIN_UV_FAULT
                    | pmbus::status_input::VIN_OV_FAULT)
                != 0
            {
                error!(
                    "CRITICAL: INPUT fault detected: 0x{:02X} ({})",
                    input_status,
                    desc.join(", ")
                );
                critical_faults.push(format!("INPUT fault: {}", desc.join(", ")));
            } else {
                warn!(
                    "INPUT warning: 0x{:02X} ({})",
                    input_status,
                    desc.join(", ")
                );
                warnings.push(format!("INPUT warning: {}", desc.join(", ")));
            }
        }

        // Check for temperature faults (critical)
        if status & pmbus::status_word::TEMP != 0 {
            let temp_status = self.read_byte(PmbusCommand::StatusTemperature).await?;
            let desc = self.decode_status_temp(temp_status);

            // Overtemperature fault is critical
            if temp_status & pmbus::status_temperature::OT_FAULT != 0 {
                error!(
                    "CRITICAL: Overtemperature fault detected: 0x{:02X} ({})",
                    temp_status,
                    desc.join(", ")
                );
                critical_faults.push(format!("Overtemperature: {}", desc.join(", ")));
            } else {
                warn!(
                    "TEMPERATURE warning: 0x{:02X} ({})",
                    temp_status,
                    desc.join(", ")
                );
                warnings.push(format!("TEMP warning: {}", desc.join(", ")));
            }
        }

        // Check for communication/memory/logic faults (treat as critical)
        if status & pmbus::status_word::CML != 0 {
            let cml_status = self.read_byte(PmbusCommand::StatusCml).await?;
            let desc = self.decode_status_cml(cml_status);
            error!(
                "CRITICAL: CML fault detected: 0x{:02X} ({})",
                cml_status,
                desc.join(", ")
            );
            critical_faults.push(format!("CML fault: {}", desc.join(", ")));
        }

        // Check if unit is OFF (critical - means power has shut down)
        if status & pmbus::status_word::OFF != 0 {
            error!(
                "CRITICAL: Power controller is OFF - Reading all status registers for diagnostics"
            );

            // Read ALL status registers to understand why it shut off
            error!("STATUS_WORD: 0x{:04X}", status);

            // Always read these status bytes when OFF to understand the fault
            if let Ok(vout_status) = self.read_byte(PmbusCommand::StatusVout).await {
                let desc = self.decode_status_vout(vout_status);
                error!("  STATUS_VOUT: 0x{:02X} ({})", vout_status, desc.join(", "));
            }

            if let Ok(iout_status) = self.read_byte(PmbusCommand::StatusIout).await {
                let desc = self.decode_status_iout(iout_status);
                error!("  STATUS_IOUT: 0x{:02X} ({})", iout_status, desc.join(", "));
            }

            if let Ok(input_status) = self.read_byte(PmbusCommand::StatusInput).await {
                let desc = self.decode_status_input(input_status);
                error!(
                    "  STATUS_INPUT: 0x{:02X} ({})",
                    input_status,
                    desc.join(", ")
                );
            }

            if let Ok(temp_status) = self.read_byte(PmbusCommand::StatusTemperature).await {
                let desc = self.decode_status_temp(temp_status);
                error!(
                    "  STATUS_TEMPERATURE: 0x{:02X} ({})",
                    temp_status,
                    desc.join(", ")
                );
            }

            if let Ok(cml_status) = self.read_byte(PmbusCommand::StatusCml).await {
                let desc = self.decode_status_cml(cml_status);
                error!("  STATUS_CML: 0x{:02X} ({})", cml_status, desc.join(", "));
            }

            // Also read current telemetry
            if let Ok(vout_mv) = self.get_vout().await {
                error!("  Current VOUT: {:.3}V", vout_mv as f32 / 1000.0);
            }
            if let Ok(iout_ma) = self.get_iout().await {
                error!("  Current IOUT: {:.2}A", iout_ma as f32 / 1000.0);
            }
            if let Ok(vin_mv) = self.get_vin().await {
                error!("  Current VIN: {:.2}V", vin_mv as f32 / 1000.0);
            }
            if let Ok(temp) = self.get_temperature().await {
                error!("  Current Temperature: {}°C", temp);
            }

            critical_faults.push("Power controller is OFF".to_string());
        }

        // Return error if any critical faults detected
        if !critical_faults.is_empty() {
            bail!(Tps546Error::FaultDetected(critical_faults.join("; ")));
        }

        Ok(())
    }

    /// Dump the complete TPS546 configuration for debugging
    pub async fn dump_configuration(&mut self) -> Result<()> {
        debug!("=== TPS546D24A Configuration Dump ===");

        // Voltage Configuration
        debug!("--- Voltage Configuration ---");

        // VIN settings
        let vin_on = self.read_word(PmbusCommand::VinOn).await?;
        debug!(
            "VIN_ON: {:.2}V (raw: 0x{:04X})",
            self.slinear11_to_float(vin_on),
            vin_on
        );

        let vin_off = self.read_word(PmbusCommand::VinOff).await?;
        debug!(
            "VIN_OFF: {:.2}V (raw: 0x{:04X})",
            self.slinear11_to_float(vin_off),
            vin_off
        );

        let vin_ov_fault = self.read_word(PmbusCommand::VinOvFaultLimit).await?;
        debug!(
            "VIN_OV_FAULT_LIMIT: {:.2}V (raw: 0x{:04X})",
            self.slinear11_to_float(vin_ov_fault),
            vin_ov_fault
        );

        let vin_uv_warn = self.read_word(PmbusCommand::VinUvWarnLimit).await?;
        debug!(
            "VIN_UV_WARN_LIMIT: {:.2}V (raw: 0x{:04X})",
            self.slinear11_to_float(vin_uv_warn),
            vin_uv_warn
        );

        let vin_ov_response = self.read_byte(PmbusCommand::VinOvFaultResponse).await?;
        let vin_ov_desc = self.decode_fault_response(vin_ov_response);
        debug!(
            "VIN_OV_FAULT_RESPONSE: 0x{:02X} ({})",
            vin_ov_response, vin_ov_desc
        );

        // VOUT settings
        let vout_max = self.read_word(PmbusCommand::VoutMax).await?;
        debug!(
            "VOUT_MAX: {:.2}V (raw: 0x{:04X})",
            self.ulinear16_to_float(vout_max).await?,
            vout_max
        );

        let vout_ov_fault = self.read_word(PmbusCommand::VoutOvFaultLimit).await?;
        let vout_ov_fault_v = self.ulinear16_to_float(vout_ov_fault).await?;
        debug!(
            "VOUT_OV_FAULT_LIMIT: {:.2}V (raw: 0x{:04X})",
            vout_ov_fault_v * self.config.vout_command,
            vout_ov_fault
        );

        let vout_ov_warn = self.read_word(PmbusCommand::VoutOvWarnLimit).await?;
        let vout_ov_warn_v = self.ulinear16_to_float(vout_ov_warn).await?;
        debug!(
            "VOUT_OV_WARN_LIMIT: {:.2}V (raw: 0x{:04X})",
            vout_ov_warn_v * self.config.vout_command,
            vout_ov_warn
        );

        let vout_margin_high = self.read_word(PmbusCommand::VoutMarginHigh).await?;
        let vout_margin_high_v = self.ulinear16_to_float(vout_margin_high).await?;
        debug!(
            "VOUT_MARGIN_HIGH: {:.2}V (raw: 0x{:04X})",
            vout_margin_high_v * self.config.vout_command,
            vout_margin_high
        );

        let vout_command = self.read_word(PmbusCommand::VoutCommand).await?;
        debug!(
            "VOUT_COMMAND: {:.2}V (raw: 0x{:04X})",
            self.ulinear16_to_float(vout_command).await?,
            vout_command
        );

        let vout_margin_low = self.read_word(PmbusCommand::VoutMarginLow).await?;
        let vout_margin_low_v = self.ulinear16_to_float(vout_margin_low).await?;
        debug!(
            "VOUT_MARGIN_LOW: {:.2}V (raw: 0x{:04X})",
            vout_margin_low_v * self.config.vout_command,
            vout_margin_low
        );

        let vout_uv_warn = self.read_word(PmbusCommand::VoutUvWarnLimit).await?;
        let vout_uv_warn_v = self.ulinear16_to_float(vout_uv_warn).await?;
        debug!(
            "VOUT_UV_WARN_LIMIT: {:.2}V (raw: 0x{:04X})",
            vout_uv_warn_v * self.config.vout_command,
            vout_uv_warn
        );

        let vout_uv_fault = self.read_word(PmbusCommand::VoutUvFaultLimit).await?;
        let vout_uv_fault_v = self.ulinear16_to_float(vout_uv_fault).await?;
        debug!(
            "VOUT_UV_FAULT_LIMIT: {:.2}V (raw: 0x{:04X})",
            vout_uv_fault_v * self.config.vout_command,
            vout_uv_fault
        );

        let vout_min = self.read_word(PmbusCommand::VoutMin).await?;
        debug!(
            "VOUT_MIN: {:.2}V (raw: 0x{:04X})",
            self.ulinear16_to_float(vout_min).await?,
            vout_min
        );

        // Current Configuration and Limits
        debug!("--- Current Configuration ---");

        let iout_oc_warn = self.read_word(PmbusCommand::IoutOcWarnLimit).await?;
        debug!(
            "IOUT_OC_WARN_LIMIT: {:.2}A (raw: 0x{:04X})",
            self.slinear11_to_float(iout_oc_warn),
            iout_oc_warn
        );

        let iout_oc_fault = self.read_word(PmbusCommand::IoutOcFaultLimit).await?;
        debug!(
            "IOUT_OC_FAULT_LIMIT: {:.2}A (raw: 0x{:04X})",
            self.slinear11_to_float(iout_oc_fault),
            iout_oc_fault
        );

        let iout_oc_response = self.read_byte(PmbusCommand::IoutOcFaultResponse).await?;
        let iout_oc_desc = self.decode_fault_response(iout_oc_response);
        debug!(
            "IOUT_OC_FAULT_RESPONSE: 0x{:02X} ({})",
            iout_oc_response, iout_oc_desc
        );

        // Temperature Configuration
        debug!("--- Temperature Configuration ---");

        let ot_warn = self.read_word(PmbusCommand::OtWarnLimit).await?;
        debug!(
            "OT_WARN_LIMIT: {}°C (raw: 0x{:04X})",
            self.slinear11_to_int(ot_warn),
            ot_warn
        );

        let ot_fault = self.read_word(PmbusCommand::OtFaultLimit).await?;
        debug!(
            "OT_FAULT_LIMIT: {}°C (raw: 0x{:04X})",
            self.slinear11_to_int(ot_fault),
            ot_fault
        );

        let ot_response = self.read_byte(PmbusCommand::OtFaultResponse).await?;
        let ot_desc = self.decode_fault_response(ot_response);
        debug!("OT_FAULT_RESPONSE: 0x{:02X} ({})", ot_response, ot_desc);

        // Current Readings
        debug!("--- Current Readings ---");

        let read_vin = self.read_word(PmbusCommand::ReadVin).await?;
        debug!("READ_VIN: {:.2}V", self.slinear11_to_float(read_vin));

        let read_vout = self.read_word(PmbusCommand::ReadVout).await?;
        debug!(
            "READ_VOUT: {:.2}V",
            self.ulinear16_to_float(read_vout).await?
        );

        let read_iout = self.read_word(PmbusCommand::ReadIout).await?;
        debug!("READ_IOUT: {:.2}A", self.slinear11_to_float(read_iout));

        let read_temp = self.read_word(PmbusCommand::ReadTemperature1).await?;
        debug!("READ_TEMPERATURE_1: {}°C", self.slinear11_to_int(read_temp));

        // Timing Configuration
        debug!("--- Timing Configuration ---");

        let ton_delay = self.read_word(PmbusCommand::TonDelay).await?;
        debug!("TON_DELAY: {}ms", self.slinear11_to_int(ton_delay));

        let ton_rise = self.read_word(PmbusCommand::TonRise).await?;
        debug!("TON_RISE: {}ms", self.slinear11_to_int(ton_rise));

        let ton_max_fault = self.read_word(PmbusCommand::TonMaxFaultLimit).await?;
        debug!(
            "TON_MAX_FAULT_LIMIT: {}ms",
            self.slinear11_to_int(ton_max_fault)
        );

        let ton_max_response = self.read_byte(PmbusCommand::TonMaxFaultResponse).await?;
        let ton_max_desc = self.decode_fault_response(ton_max_response);
        debug!(
            "TON_MAX_FAULT_RESPONSE: 0x{:02X} ({})",
            ton_max_response, ton_max_desc
        );

        let toff_delay = self.read_word(PmbusCommand::ToffDelay).await?;
        debug!("TOFF_DELAY: {}ms", self.slinear11_to_int(toff_delay));

        let toff_fall = self.read_word(PmbusCommand::ToffFall).await?;
        debug!("TOFF_FALL: {}ms", self.slinear11_to_int(toff_fall));

        // Operational Configuration
        debug!("--- Operational Configuration ---");

        let phase = self.read_byte(PmbusCommand::Phase).await?;
        let phase_desc = if phase == 0xFF {
            "all phases".to_string()
        } else {
            format!("phase {}", phase)
        };
        debug!("PHASE: 0x{:02X} ({})", phase, phase_desc);

        let stack_config = self.read_word(PmbusCommand::StackConfig).await?;
        debug!("STACK_CONFIG: 0x{:04X}", stack_config);

        let sync_config = self.read_byte(PmbusCommand::SyncConfig).await?;
        debug!("SYNC_CONFIG: 0x{:02X}", sync_config);

        let interleave = self.read_word(PmbusCommand::Interleave).await?;
        debug!("INTERLEAVE: 0x{:04X}", interleave);

        let capability = self.read_byte(PmbusCommand::Capability).await?;
        let mut cap_desc = Vec::new();
        if capability & 0x80 != 0 {
            cap_desc.push("PEC supported");
        }
        if capability & 0x40 != 0 {
            cap_desc.push("400kHz max");
        }
        if capability & 0x20 != 0 {
            cap_desc.push("Alert supported");
        }
        debug!(
            "CAPABILITY: 0x{:02X} ({})",
            capability,
            if cap_desc.is_empty() {
                "none".to_string()
            } else {
                cap_desc.join(", ")
            }
        );

        let op_val = self.read_byte(PmbusCommand::Operation).await?;
        let op_desc = match op_val {
            pmbus::operation::OFF_IMMEDIATE => "OFF (immediate)",
            pmbus::operation::SOFT_OFF => "SOFT OFF",
            pmbus::operation::ON => "ON",
            pmbus::operation::ON_MARGIN_LOW => "ON (margin low)",
            pmbus::operation::ON_MARGIN_HIGH => "ON (margin high)",
            _ => "unknown",
        };
        debug!("OPERATION: 0x{:02X} ({})", op_val, op_desc);

        let on_off_val = self.read_byte(PmbusCommand::OnOffConfig).await?;
        let mut on_off_desc = Vec::new();
        if on_off_val & pmbus::on_off_config::PU != 0 {
            on_off_desc.push("PowerUp from CONTROL");
        }
        if on_off_val & pmbus::on_off_config::CMD != 0 {
            on_off_desc.push("CMD enabled");
        }
        if on_off_val & pmbus::on_off_config::CP != 0 {
            on_off_desc.push("CONTROL present");
        }
        if on_off_val & pmbus::on_off_config::POLARITY != 0 {
            on_off_desc.push("Active high");
        }
        if on_off_val & pmbus::on_off_config::DELAY != 0 {
            on_off_desc.push("Turn-off delay");
        }
        debug!(
            "ON_OFF_CONFIG: 0x{:02X} ({})",
            on_off_val,
            on_off_desc.join(", ")
        );

        // Compensation Configuration
        match self.read_block(PmbusCommand::CompensationConfig, 5).await {
            Ok(comp_config) => {
                debug!("COMPENSATION_CONFIG: {:02X?}", comp_config);
            }
            Err(e) => {
                debug!("Failed to read COMPENSATION_CONFIG: {}", e);
            }
        }

        // Status Information
        debug!("--- Status Information ---");

        let status_word = self.read_word(PmbusCommand::StatusWord).await?;
        let status_desc = self.decode_status_word(status_word);

        if status_desc.is_empty() {
            debug!("STATUS_WORD: 0x{:04X} (no flags set)", status_word);
        } else {
            debug!(
                "STATUS_WORD: 0x{:04X} ({})",
                status_word,
                status_desc.join(", ")
            );
        }

        // Read detailed status registers if main status indicates issues
        if status_word & pmbus::status_word::VOUT != 0 {
            let vout_status = self.read_byte(PmbusCommand::StatusVout).await?;
            let desc = self.decode_status_vout(vout_status);
            debug!("STATUS_VOUT: 0x{:02X} ({})", vout_status, desc.join(", "));
        }

        if status_word & pmbus::status_word::IOUT != 0 {
            let iout_status = self.read_byte(PmbusCommand::StatusIout).await?;
            let desc = self.decode_status_iout(iout_status);
            debug!("STATUS_IOUT: 0x{:02X} ({})", iout_status, desc.join(", "));
        }

        if status_word & pmbus::status_word::INPUT != 0 {
            let input_status = self.read_byte(PmbusCommand::StatusInput).await?;
            let desc = self.decode_status_input(input_status);
            debug!("STATUS_INPUT: 0x{:02X} ({})", input_status, desc.join(", "));
        }

        if status_word & pmbus::status_word::TEMP != 0 {
            let temp_status = self.read_byte(PmbusCommand::StatusTemperature).await?;
            let desc = self.decode_status_temp(temp_status);
            debug!(
                "STATUS_TEMPERATURE: 0x{:02X} ({})",
                temp_status,
                desc.join(", ")
            );
        }

        if status_word & pmbus::status_word::CML != 0 {
            let cml_status = self.read_byte(PmbusCommand::StatusCml).await?;
            let desc = self.decode_status_cml(cml_status);
            debug!("STATUS_CML: 0x{:02X} ({})", cml_status, desc.join(", "));
        }

        debug!("=== End Configuration Dump ===");
        Ok(())
    }

    // Helper methods for decoding status registers

    fn decode_status_word(&self, status: u16) -> Vec<&'static str> {
        StatusDecoder::decode_status_word(status)
    }

    fn decode_status_vout(&self, status: u8) -> Vec<&'static str> {
        StatusDecoder::decode_status_vout(status)
    }

    fn decode_status_iout(&self, status: u8) -> Vec<&'static str> {
        StatusDecoder::decode_status_iout(status)
    }

    fn decode_status_input(&self, status: u8) -> Vec<&'static str> {
        StatusDecoder::decode_status_input(status)
    }

    fn decode_status_temp(&self, status: u8) -> Vec<&'static str> {
        StatusDecoder::decode_status_temp(status)
    }

    fn decode_status_cml(&self, status: u8) -> Vec<&'static str> {
        StatusDecoder::decode_status_cml(status)
    }

    fn decode_fault_response(&self, response: u8) -> String {
        StatusDecoder::decode_fault_response(response)
    }

    // Helper methods for I2C operations

    async fn read_byte(&mut self, command: PmbusCommand) -> Result<u8> {
        let mut data = [0u8; 1];
        self.i2c
            .write_read(TPS546_I2C_ADDR, &[command.as_u8()], &mut data)
            .await?;
        Ok(data[0])
    }

    async fn write_byte(&mut self, command: PmbusCommand, data: u8) -> Result<()> {
        self.i2c
            .write(TPS546_I2C_ADDR, &[command.as_u8(), data])
            .await?;
        Ok(())
    }

    async fn read_word(&mut self, command: PmbusCommand) -> Result<u16> {
        let mut data = [0u8; 2];
        self.i2c
            .write_read(TPS546_I2C_ADDR, &[command.as_u8()], &mut data)
            .await?;
        Ok(u16::from_le_bytes(data))
    }

    async fn write_word(&mut self, command: PmbusCommand, data: u16) -> Result<()> {
        let bytes = data.to_le_bytes();
        self.i2c
            .write(TPS546_I2C_ADDR, &[command.as_u8(), bytes[0], bytes[1]])
            .await?;
        Ok(())
    }

    async fn read_block(&mut self, command: PmbusCommand, length: usize) -> Result<Vec<u8>> {
        // PMBus block read: first byte is length, then data
        let mut buffer = vec![0u8; length + 1];
        self.i2c
            .write_read(TPS546_I2C_ADDR, &[command.as_u8()], &mut buffer)
            .await?;

        // First byte is the length, verify it matches what we expect
        let reported_length = buffer[0] as usize;
        if reported_length != length {
            warn!(
                "Block read length mismatch: expected {}, got {}",
                length, reported_length
            );
        }

        // Return just the data portion (skip length byte)
        Ok(buffer[1..=length].to_vec())
    }

    // Helper functions to use PMBus format converters

    fn slinear11_to_float(&self, value: u16) -> f32 {
        Linear11::to_float(value)
    }

    fn slinear11_to_int(&self, value: u16) -> i32 {
        Linear11::to_int(value)
    }

    fn float_to_slinear11(&self, value: f32) -> u16 {
        Linear11::from_float(value)
    }

    fn int_to_slinear11(&self, value: i32) -> u16 {
        Linear11::from_int(value)
    }

    async fn ulinear16_to_float(&mut self, value: u16) -> Result<f32> {
        let vout_mode = self.read_byte(PmbusCommand::VoutMode).await?;
        Ok(Linear16::to_float(value, vout_mode))
    }

    async fn float_to_ulinear16(&mut self, value: f32) -> Result<u16> {
        let vout_mode = self.read_byte(PmbusCommand::VoutMode).await?;
        Linear16::from_float(value, vout_mode)
            .map_err(|e| anyhow::anyhow!("ULINEAR16 conversion error: {}", e))
    }
}
