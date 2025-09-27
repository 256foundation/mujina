//! Output formatting for dissected frames.

use crate::dissect::{CrcStatus, DissectedFrame, DissectedI2c, FrameContent, I2cDevice};
use crate::serial::Direction;
use colored::Colorize;

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

    let direction_str = match frame.direction {
        Direction::HostToChip => "CI → ASIC",
        Direction::ChipToHost => "RO ← ASIC",
    };

    let content_str = match &frame.content {
        FrameContent::Command(cmd) => cmd.clone(),
        FrameContent::Response(resp) => resp.clone(),
        FrameContent::Unknown(msg) => msg.clone(),
        FrameContent::Invalid(msg) => {
            if config.use_color {
                format!("{}", msg.red())
            } else {
                msg.clone()
            }
        }
    };

    let mut result = format!("[{}] {}: {}", timestamp, direction_str, content_str);

    if frame.crc_status != CrcStatus::NotChecked {
        result.push_str(&format!(" [{}]", frame.crc_status));
    }

    if config.show_raw_hex && !frame.raw_data.is_empty() {
        let hex_lines = format_hex_multiline(&frame.raw_data);
        let formatted_hex = if config.use_color {
            hex_lines
                .lines()
                .map(|line| format!("\n        {}", line.white().dimmed()))
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

/// Get a consistent color for an I2C address
fn get_address_color(address: u8) -> colored::Color {
    // Use a simple hash to map addresses to a fixed set of colors
    // This ensures the same address always gets the same color
    const COLORS: &[colored::Color] = &[
        colored::Color::Green,
        colored::Color::Yellow,
        colored::Color::Blue,
        colored::Color::Magenta,
        colored::Color::Cyan,
        colored::Color::Red,
        colored::Color::BrightGreen,
        colored::Color::BrightYellow,
        colored::Color::BrightBlue,
        colored::Color::BrightMagenta,
        colored::Color::BrightCyan,
        colored::Color::BrightRed,
    ];

    // Better hash: mix the bits to avoid clustering similar addresses
    // Multiply by a prime and XOR the upper bits for better distribution
    let hash = (address.wrapping_mul(37)) ^ (address >> 4);
    let index = (hash as usize) % COLORS.len();
    COLORS[index]
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
        let color = get_address_color(op.address);
        format!("{}", device_name.color(color))
    } else {
        match op.device {
            I2cDevice::Emc2101 => format!("EMC2101@0x{:02x}", op.address),
            I2cDevice::Tps546 => format!("TPS546@0x{:02x}", op.address),
            I2cDevice::Unknown => format!("Device@0x{:02x}", op.address),
        }
    };

    let mut result = format!("[{}] I2C: {} {}", timestamp, device_str, op.operation);

    // Add NAK indicator if the transaction was NAKed
    if op.was_naked {
        if config.use_color {
            result.push_str(&format!(" {}", "[NAK]".red().bold()));
        } else {
            result.push_str(" [NAK]");
        }
    }

    if config.show_raw_hex && !op.raw_data.is_empty() {
        result.push_str(&format!(" [{}]", format_hex(&op.raw_data)));
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

/// Format hex bytes
fn format_hex(data: &[u8]) -> String {
    data.iter()
        .map(|b| format!("{:02x}", b))
        .collect::<Vec<_>>()
        .join(" ")
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
