//! Output formatting for dissected frames.

use crate::bm13xx::Direction;
use crate::capture::BaudRate;
use crate::dissect::{CrcStatus, DissectedFrame, DissectedI2c, FrameContent, I2cDevice};
use colored::Colorize;

/// Gray color for hex data output
const HEX_DATA_GRAY_R: u8 = 128;
const HEX_DATA_GRAY_G: u8 = 128;
const HEX_DATA_GRAY_B: u8 = 128;

/// Apply gray color to hex data
fn gray_hex(text: &str) -> colored::ColoredString {
    text.truecolor(HEX_DATA_GRAY_R, HEX_DATA_GRAY_G, HEX_DATA_GRAY_B)
}

/// Output formatter configuration
#[derive(Debug, Clone)]
pub struct OutputConfig {
    pub show_raw_hex: bool,
    pub use_relative_time: bool,
    pub start_time: Option<f64>,
    pub use_color: bool,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            show_raw_hex: false,
            use_relative_time: false,
            start_time: None,
            use_color: true,
        }
    }
}

/// Format a dissected serial frame
pub fn format_serial_frame(frame: &DissectedFrame, config: &OutputConfig) -> String {
    let timestamp = format_timestamp(frame.timestamp, config);

    let baud_str = match frame.baud_rate {
        BaudRate::Baud115200 => "115k",
        BaudRate::Baud1M => "1M",
    };

    let direction_str = if config.use_color {
        match frame.direction {
            Direction::HostToChip => {
                let color = get_device_color(&DeviceId::AsicHostToChip);
                format!("{}", format!("CI → ASIC {}", baud_str).color(color))
            }
            Direction::ChipToHost => {
                let color = get_device_color(&DeviceId::AsicChipToHost);
                format!("{}", format!("RO ← ASIC {}", baud_str).color(color))
            }
        }
    } else {
        match frame.direction {
            Direction::HostToChip => format!("CI → ASIC {}", baud_str),
            Direction::ChipToHost => format!("RO ← ASIC {}", baud_str),
        }
    };

    let content_str = match &frame.content {
        FrameContent::Command(cmd) => cmd.clone(),
        FrameContent::Response(resp) => resp.clone(),
    };

    let mut result = format!("{} {}: {}", timestamp, direction_str, content_str);

    if frame.crc_status != CrcStatus::NotChecked {
        result.push_str(&format!(" [{}]", frame.crc_status));
    }

    if config.show_raw_hex && !frame.raw_data.is_empty() {
        let hex_lines = format_hex_multiline(&frame.raw_data);
        let formatted_hex = if config.use_color {
            hex_lines
                .lines()
                .map(|line| format!("\n        {}", gray_hex(line)))
                .collect::<String>()
        } else {
            hex_lines
                .lines()
                .map(|line| format!("\n        {}", line))
                .collect::<String>()
        };
        result.push_str(&formatted_hex);
    }

    result
}

/// Device/direction identifier for consistent coloring
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum DeviceId {
    I2cAddress(u8),
    AsicHostToChip,
    AsicChipToHost,
}

/// Get a consistent color for any device or ASIC direction
fn get_device_color(device_id: &DeviceId) -> colored::Color {
    match device_id {
        // ASIC directions get two bright, complementary colors
        DeviceId::AsicHostToChip => colored::Color::BrightCyan, // CI → ASIC
        DeviceId::AsicChipToHost => colored::Color::BrightYellow, // RO ← ASIC

        // I2C addresses get colors from remaining palette (reserve red for errors, avoid bright/regular pairs)
        DeviceId::I2cAddress(addr) => {
            // Distinct colors for I2C devices (avoiding ASIC colors: BrightCyan, BrightYellow)
            // Only use colors that are clearly distinguishable from each other
            const I2C_COLORS: &[colored::Color] = &[
                colored::Color::Green,
                colored::Color::Blue,
                colored::Color::Magenta,
                colored::Color::BrightGreen,
                colored::Color::BrightBlue,
                colored::Color::BrightMagenta,
                colored::Color::BrightRed,
            ];

            // Hash I2C addresses to the I2C color palette
            let hash = (addr.wrapping_mul(37)) ^ (addr >> 4);
            let index = (hash as usize) % I2C_COLORS.len();
            I2C_COLORS[index]
        }
    }
}

/// Format an I2C operation
pub fn format_i2c_operation(op: &DissectedI2c, config: &OutputConfig) -> String {
    let timestamp = format_timestamp(op.timestamp, config);

    // Format device string with consistent color based on address
    let device_str = if config.use_color {
        let device_name = match op.device {
            I2cDevice::Emc2101 => format!("EMC2101@0x{:02x}", op.address),
            I2cDevice::Tps546 => format!("TPS546@0x{:02x}", op.address),
            I2cDevice::Unknown => format!("Device@0x{:02x}", op.address),
        };

        // Apply color based on address for consistency
        let color = get_device_color(&DeviceId::I2cAddress(op.address));
        format!("{}", device_name.color(color))
    } else {
        match op.device {
            I2cDevice::Emc2101 => format!("EMC2101@0x{:02x}", op.address),
            I2cDevice::Tps546 => format!("TPS546@0x{:02x}", op.address),
            I2cDevice::Unknown => format!("Device@0x{:02x}", op.address),
        }
    };

    let i2c_label = if config.use_color {
        let color = get_device_color(&DeviceId::I2cAddress(op.address));
        format!("{}", "I2C:".color(color))
    } else {
        "I2C:".to_string()
    };

    let mut result = format!(
        "{} {} {} {}",
        timestamp, i2c_label, device_str, op.operation
    );

    // Add NAK indicator if the transaction was NAKed
    if op.was_naked {
        if config.use_color {
            result.push_str(&format!(" {}", "[NAK]".red().bold()));
        } else {
            result.push_str(" [NAK]");
        }
    }

    if config.show_raw_hex && !op.raw_data.is_empty() {
        let hex_lines = format_i2c_transaction(&op.raw_data, op.address);
        let formatted_hex = if config.use_color {
            hex_lines
                .lines()
                .map(|line| format!("\n        {}", gray_hex(line)))
                .collect::<String>()
        } else {
            hex_lines
                .lines()
                .map(|line| format!("\n        {}", line))
                .collect::<String>()
        };
        result.push_str(&formatted_hex);
    }

    result
}

/// Format timestamp
fn format_timestamp(timestamp: f64, config: &OutputConfig) -> String {
    if config.use_relative_time {
        let relative = if let Some(start) = config.start_time {
            timestamp - start
        } else {
            timestamp
        };
        format!("{:10.6}", relative)
    } else {
        format!("{:10.6}", timestamp)
    }
}

/// Format hex data with line wrapping at 16 bytes per line
fn format_hex_multiline(data: &[u8]) -> String {
    if data.is_empty() {
        return String::new();
    }

    let mut lines = Vec::new();
    for chunk in data.chunks(16) {
        let hex_line = chunk
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<Vec<_>>()
            .join(" ");
        lines.push(hex_line);
    }
    lines.join("\n")
}

/// Format I2C transaction with readable address interpretation
fn format_i2c_transaction(data: &[u8], expected_address: u8) -> String {
    if data.is_empty() {
        return String::new();
    }

    let mut result = Vec::new();
    let mut i = 0;

    while i < data.len() {
        let byte = data[i];
        let address = byte >> 1;
        let _is_read = (byte & 1) != 0;

        // Check if this looks like an I2C address byte
        if address == expected_address {
            result.push(format!("{:02x}", address));
            i += 1;

            // Collect following data bytes until next potential address byte
            let mut data_bytes = Vec::new();
            while i < data.len() {
                let next_byte = data[i];
                let next_address = next_byte >> 1;

                // If this could be another address byte for the same device, break
                if next_address == expected_address && i < data.len() - 1 {
                    break;
                }

                data_bytes.push(format!("{:02x}", next_byte));
                i += 1;
            }

            if !data_bytes.is_empty() {
                result.push(data_bytes.join(" "));
            }
        } else {
            // Not an address byte we recognize, just show as hex
            result.push(format!("{:02x}", byte));
            i += 1;
        }
    }

    result.join(" ")
}

/// Event type for unified output
#[derive(Debug)]
pub enum OutputEvent {
    Serial(DissectedFrame),
    I2c(DissectedI2c),
}

impl OutputEvent {
    pub fn timestamp(&self) -> f64 {
        match self {
            OutputEvent::Serial(frame) => frame.timestamp,
            OutputEvent::I2c(op) => op.timestamp,
        }
    }

    pub fn format(&self, config: &OutputConfig) -> String {
        match self {
            OutputEvent::Serial(frame) => format_serial_frame(frame, config),
            OutputEvent::I2c(op) => format_i2c_operation(op, config),
        }
    }
}
