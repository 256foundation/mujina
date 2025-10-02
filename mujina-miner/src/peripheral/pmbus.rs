//! PMBus Protocol Support
//!
//! This module provides generic PMBus protocol definitions and utilities
//! that can be used by PMBus-compliant device drivers.
//!
//! PMBus is a variant of SMBus with extensions for power management.
//! Specification: <https://pmbus.org/specification-documents/>

use bitflags::bitflags;
use std::fmt;
use thiserror::Error;

// ============================================================================
// Constants
// ============================================================================

/// Default VOUT_MODE for devices that don't specify
/// Uses exponent -9 (2^-9 ≈ 0.00195V resolution) which provides
/// millivolt-level precision suitable for most power supplies
const DEFAULT_VOUT_MODE: u8 = 0x17; // -9 in 5-bit two's complement

// ============================================================================
// PMBus Commands
// ============================================================================

/// Macro to define PMBus commands with metadata in one place
macro_rules! define_pmbus_commands {
    (
        $(
            $variant:ident = $value:literal,
            $name:literal,
            $desc:literal
        ),* $(,)?
    ) => {
        /// PMBus standard command codes
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[repr(u8)]
        pub enum PmbusCommand {
            $(
                $variant = $value,
            )*
        }

        impl PmbusCommand {
            /// Command metadata: (value, name, description)
            const METADATA: &'static [(u8, &'static str, &'static str)] = &[
                $(
                    ($value, $name, $desc),
                )*
            ];

            /// Get the command name as a string
            pub fn name(&self) -> &'static str {
                let value = self.as_u8();
                Self::METADATA
                    .iter()
                    .find(|(v, _, _)| *v == value)
                    .map(|(_, name, _)| *name)
                    .unwrap_or("UNKNOWN")
            }

            /// Get command description
            pub fn description(&self) -> &'static str {
                let value = self.as_u8();
                Self::METADATA
                    .iter()
                    .find(|(v, _, _)| *v == value)
                    .map(|(_, _, desc)| *desc)
                    .unwrap_or("unknown command")
            }

            /// Convert to u8 command code
            pub fn as_u8(self) -> u8 {
                self as u8
            }
        }

        impl fmt::Display for PmbusCommand {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.name())
            }
        }

        impl TryFrom<u8> for PmbusCommand {
            type Error = PMBusError;

            fn try_from(value: u8) -> Result<Self, Self::Error> {
                match value {
                    $(
                        $value => Ok(Self::$variant),
                    )*
                    _ => Err(PMBusError::CommandNotSupported),
                }
            }
        }

        impl From<PmbusCommand> for u8 {
            fn from(cmd: PmbusCommand) -> Self {
                cmd.as_u8()
            }
        }
    };
}

// Define all PMBus commands in one place
define_pmbus_commands! {
    Page = 0x00, "PAGE", "select page for multi-rail devices",
    Operation = 0x01, "OPERATION", "turn on/off control",
    OnOffConfig = 0x02, "ON_OFF_CONFIG", "on/off configuration",
    ClearFaults = 0x03, "CLEAR_FAULTS", "clears all fault status bits",
    Phase = 0x04, "PHASE", "phase selection",
    Capability = 0x19, "CAPABILITY", "device capability",
    VoutMode = 0x20, "VOUT_MODE", "output voltage data format",
    VoutCommand = 0x21, "VOUT_COMMAND", "commanded output voltage",
    VoutMax = 0x24, "VOUT_MAX", "maximum output voltage",
    VoutMarginHigh = 0x25, "VOUT_MARGIN_HIGH", "margin high voltage",
    VoutMarginLow = 0x26, "VOUT_MARGIN_LOW", "margin low voltage",
    VoutScaleLoop = 0x29, "VOUT_SCALE_LOOP", "scale loop compensation",
    VoutMin = 0x2B, "VOUT_MIN", "minimum output voltage",
    FrequencySwitch = 0x33, "FREQUENCY_SWITCH", "switching frequency",
    VinOn = 0x35, "VIN_ON", "input turn-on voltage",
    VinOff = 0x36, "VIN_OFF", "input turn-off voltage",
    Interleave = 0x37, "INTERLEAVE", "interleave configuration",
    VoutOvFaultLimit = 0x40, "VOUT_OV_FAULT_LIMIT", "output overvoltage fault limit",
    VoutOvWarnLimit = 0x42, "VOUT_OV_WARN_LIMIT", "output overvoltage warning limit",
    VoutUvWarnLimit = 0x43, "VOUT_UV_WARN_LIMIT", "output undervoltage warning limit",
    VoutUvFaultLimit = 0x44, "VOUT_UV_FAULT_LIMIT", "output undervoltage fault limit",
    IoutOcFaultLimit = 0x46, "IOUT_OC_FAULT_LIMIT", "output overcurrent fault limit",
    IoutOcFaultResponse = 0x47, "IOUT_OC_FAULT_RESPONSE", "output overcurrent fault response",
    IoutOcWarnLimit = 0x4A, "IOUT_OC_WARN_LIMIT", "output overcurrent warning limit",
    OtFaultLimit = 0x4F, "OT_FAULT_LIMIT", "overtemperature fault limit",
    OtFaultResponse = 0x50, "OT_FAULT_RESPONSE", "overtemperature fault response",
    OtWarnLimit = 0x51, "OT_WARN_LIMIT", "overtemperature warning limit",
    VinOvFaultLimit = 0x55, "VIN_OV_FAULT_LIMIT", "input overvoltage fault limit",
    VinOvFaultResponse = 0x56, "VIN_OV_FAULT_RESPONSE", "input overvoltage fault response",
    VinUvWarnLimit = 0x58, "VIN_UV_WARN_LIMIT", "input undervoltage warning limit",
    TonDelay = 0x60, "TON_DELAY", "turn-on delay",
    TonRise = 0x61, "TON_RISE", "turn-on rise time",
    TonMaxFaultLimit = 0x62, "TON_MAX_FAULT_LIMIT", "maximum turn-on time limit",
    TonMaxFaultResponse = 0x63, "TON_MAX_FAULT_RESPONSE", "maximum turn-on fault response",
    ToffDelay = 0x64, "TOFF_DELAY", "turn-off delay",
    ToffFall = 0x65, "TOFF_FALL", "turn-off fall time",
    StatusWord = 0x79, "STATUS_WORD", "status summary",
    StatusVout = 0x7A, "STATUS_VOUT", "output voltage status",
    StatusIout = 0x7B, "STATUS_IOUT", "output current status",
    StatusInput = 0x7C, "STATUS_INPUT", "input status",
    StatusTemperature = 0x7D, "STATUS_TEMPERATURE", "temperature status",
    StatusCml = 0x7E, "STATUS_CML", "communication/logic/memory status",
    StatusOther = 0x7F, "STATUS_OTHER", "other status",
    StatusMfrSpecific = 0x80, "STATUS_MFR_SPECIFIC", "manufacturer specific status",
    ReadVin = 0x88, "READ_VIN", "input voltage",
    ReadVout = 0x8B, "READ_VOUT", "output voltage",
    ReadIout = 0x8C, "READ_IOUT", "output current",
    ReadTemperature1 = 0x8D, "READ_TEMPERATURE_1", "temperature 1",
    MfrId = 0x99, "MFR_ID", "manufacturer ID",
    MfrModel = 0x9A, "MFR_MODEL", "manufacturer model",
    MfrRevision = 0x9B, "MFR_REVISION", "manufacturer revision",
    IcDeviceId = 0xAD, "IC_DEVICE_ID", "IC device ID",
    CompensationConfig = 0xB1, "COMPENSATION_CONFIG", "compensation configuration",
    SyncConfig = 0xE4, "SYNC_CONFIG", "synchronization configuration",
    StackConfig = 0xEC, "STACK_CONFIG", "stacking configuration",
    // TPS546-specific commands - TODO: move to device-specific module
    MiscOptions = 0xED, "MISC_OPTIONS", "miscellaneous options",
    PinDetectOverride = 0xEE, "PIN_DETECT_OVERRIDE", "pin detect override",
    SlaveAddress = 0xEF, "SLAVE_ADDRESS", "slave address",
    NvmChecksum = 0xF0, "NVM_CHECKSUM", "NVM checksum",
    SimulateFault = 0xF1, "SIMULATE_FAULT", "simulate fault",
    FusionId0 = 0xFC, "FUSION_ID0", "fusion ID 0",
    FusionId1 = 0xFD, "FUSION_ID1", "fusion ID 1",
}

// ============================================================================
// Typed PMBus Values
// ============================================================================

/// Typed PMBus voltage value that preserves the original encoding format
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PmbusVoltage {
    /// Linear11 format (5-bit exponent + 11-bit mantissa)
    Linear11(Linear11),
    /// Linear16 format (16-bit mantissa + VOUT_MODE exponent)
    Linear16(Linear16),
    /// Direct floating-point value
    Direct(f32),
}

impl PmbusVoltage {
    /// Create from direct floating-point value
    pub fn new(value: f32) -> Self {
        Self::Direct(value)
    }

    /// Create from Linear11 encoded value
    pub fn from_linear11(value: u16) -> Self {
        Self::Linear11(Linear11::new(value))
    }

    /// Create from Linear16 encoded value with VOUT_MODE
    pub fn from_linear16(value: u16, vout_mode: u8) -> Self {
        let mode = VoutMode::new(vout_mode);
        Self::Linear16(Linear16::new(value, mode))
    }

    /// Get the voltage value as f32
    pub fn value(&self) -> f32 {
        match self {
            Self::Linear11(l11) => l11.to_f32(),
            Self::Linear16(l16) => l16.to_f32(),
            Self::Direct(v) => *v,
        }
    }

    /// Try to encode as Linear11, returns None if precision loss is too high
    pub fn to_linear11(&self) -> Option<Linear11> {
        match self {
            Self::Linear11(l11) => Some(*l11),
            _ => {
                let value = self.value();
                Linear11::from_f32(value).ok()
            }
        }
    }

    /// Try to encode as Linear16 with given VOUT_MODE
    pub fn to_linear16(&self, vout_mode: VoutMode) -> Option<Linear16> {
        match self {
            Self::Linear16(l16) if l16.mode == vout_mode => Some(*l16),
            _ => {
                let value = self.value();
                Linear16::from_f32(value, vout_mode).ok()
            }
        }
    }
}

impl From<f32> for PmbusVoltage {
    fn from(value: f32) -> Self {
        Self::Direct(value)
    }
}

impl From<PmbusVoltage> for f32 {
    fn from(voltage: PmbusVoltage) -> Self {
        voltage.value()
    }
}

impl fmt::Display for PmbusVoltage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.3}V", self.value())
    }
}

impl Default for PmbusVoltage {
    fn default() -> Self {
        Self::Direct(0.0)
    }
}

impl PartialOrd for PmbusVoltage {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.value().partial_cmp(&other.value())
    }
}

/// Typed PMBus current value that preserves the original encoding format
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PmbusCurrent {
    /// Linear11 format (5-bit exponent + 11-bit mantissa)
    Linear11(Linear11),
    /// Direct floating-point value
    Direct(f32),
}

impl PmbusCurrent {
    /// Create from direct floating-point value
    pub fn new(value: f32) -> Self {
        Self::Direct(value)
    }

    /// Create from Linear11 encoded value
    pub fn from_linear11(value: u16) -> Self {
        Self::Linear11(Linear11::new(value))
    }

    /// Get the current value as f32
    pub fn value(&self) -> f32 {
        match self {
            Self::Linear11(l11) => l11.to_f32(),
            Self::Direct(v) => *v,
        }
    }

    /// Try to encode as Linear11, returns None if precision loss is too high
    pub fn to_linear11(&self) -> Option<Linear11> {
        match self {
            Self::Linear11(l11) => Some(*l11),
            Self::Direct(v) => Linear11::from_f32(*v).ok(),
        }
    }
}

impl From<f32> for PmbusCurrent {
    fn from(value: f32) -> Self {
        Self::Direct(value)
    }
}

impl From<PmbusCurrent> for f32 {
    fn from(current: PmbusCurrent) -> Self {
        current.value()
    }
}

impl Default for PmbusCurrent {
    fn default() -> Self {
        Self::Direct(0.0)
    }
}

impl PartialOrd for PmbusCurrent {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.value().partial_cmp(&other.value())
    }
}

impl fmt::Display for PmbusCurrent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.3}A", self.value())
    }
}

/// Typed PMBus temperature value that preserves the original encoding format
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PmbusTemperature {
    /// Linear11 format (5-bit exponent + 11-bit mantissa)
    Linear11(Linear11),
    /// Direct floating-point value
    Direct(f32),
}

impl PmbusTemperature {
    /// Create from direct floating-point value
    pub fn new(value: f32) -> Self {
        Self::Direct(value)
    }

    /// Create from Linear11 encoded value
    pub fn from_linear11(value: u16) -> Self {
        Self::Linear11(Linear11::new(value))
    }

    /// Get the temperature value as f32
    pub fn value(&self) -> f32 {
        match self {
            Self::Linear11(l11) => l11.to_f32(),
            Self::Direct(v) => *v,
        }
    }

    /// Try to encode as Linear11, returns None if precision loss is too high
    pub fn to_linear11(&self) -> Option<Linear11> {
        match self {
            Self::Linear11(l11) => Some(*l11),
            Self::Direct(v) => Linear11::from_f32(*v).ok(),
        }
    }
}

impl From<f32> for PmbusTemperature {
    fn from(value: f32) -> Self {
        Self::Direct(value)
    }
}

impl From<PmbusTemperature> for f32 {
    fn from(temp: PmbusTemperature) -> Self {
        temp.value()
    }
}

impl Default for PmbusTemperature {
    fn default() -> Self {
        Self::Direct(0.0)
    }
}

impl PartialOrd for PmbusTemperature {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.value().partial_cmp(&other.value())
    }
}

impl fmt::Display for PmbusTemperature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.1}°C", self.value())
    }
}

/// Typed PMBus frequency value with units
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Default)]
pub struct PmbusFrequency(f32);

impl PmbusFrequency {
    pub fn new(value: f32) -> Self {
        Self(value)
    }

    pub fn from_linear11(value: u16) -> Self {
        Self(linear11::to_float(value))
    }

    pub fn value(&self) -> f32 {
        self.0
    }
}

impl From<f32> for PmbusFrequency {
    fn from(value: f32) -> Self {
        Self(value)
    }
}

impl From<PmbusFrequency> for f32 {
    fn from(freq: PmbusFrequency) -> Self {
        freq.0
    }
}

impl fmt::Display for PmbusFrequency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.0}kHz", self.0)
    }
}

/// Typed PMBus time value with units
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Default)]
pub struct PmbusTime(f32);

impl PmbusTime {
    pub fn new(value: f32) -> Self {
        Self(value)
    }

    pub fn from_linear11(value: u16) -> Self {
        Self(linear11::to_float(value))
    }

    pub fn value(&self) -> f32 {
        self.0
    }
}

impl From<f32> for PmbusTime {
    fn from(value: f32) -> Self {
        Self(value)
    }
}

impl From<PmbusTime> for f32 {
    fn from(time: PmbusTime) -> Self {
        time.0
    }
}

impl fmt::Display for PmbusTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.1}ms", self.0)
    }
}

/// PMBus value enumeration for polymorphic value handling
#[derive(Debug, Clone)]
pub enum PmbusValue {
    Voltage(PmbusVoltage),
    Current(PmbusCurrent),
    Temperature(PmbusTemperature),
    ScaleFactor(f32),
    Frequency(PmbusFrequency),
    Time(PmbusTime),
    StatusWord(u16, Vec<&'static str>),
    StatusByte(u8, Vec<&'static str>),
    FaultResponse(u8, String),
    Operation(u8, String),
    OnOffConfig(u8, Vec<&'static str>),
    DeviceId(Vec<u8>, String),
    Phase(u8, String),
    Page(u8, String),
    VoutMode(u8, String),
    Capability(u8, Vec<&'static str>),
    ConfigWord(u16, String),
    ConfigBytes(Vec<u8>, String),
    String(String),
    Raw(Vec<u8>),
}

impl fmt::Display for PmbusValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Voltage(v) => v.fmt(f),
            Self::Current(c) => c.fmt(f),
            Self::Temperature(t) => t.fmt(f),
            Self::ScaleFactor(s) => write!(f, "{:.3}", s),
            Self::Frequency(freq) => freq.fmt(f),
            Self::Time(t) => t.fmt(f),
            Self::StatusWord(value, flags) => {
                if flags.is_empty() {
                    write!(f, "0x{:04x}", value)
                } else {
                    write!(f, "0x{:04x} (", value)?;
                    for (i, flag) in flags.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{}", flag)?;
                    }
                    write!(f, ")")
                }
            }
            Self::StatusByte(value, flags) => {
                if flags.is_empty() {
                    write!(f, "0x{:02x}", value)
                } else {
                    write!(f, "0x{:02x} (", value)?;
                    for (i, flag) in flags.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{}", flag)?;
                    }
                    write!(f, ")")
                }
            }
            Self::FaultResponse(value, desc) => write!(f, "0x{:02x} ({})", value, desc),
            Self::Operation(value, desc) => write!(f, "0x{:02x} ({})", value, desc),
            Self::OnOffConfig(value, flags) => {
                if flags.is_empty() {
                    write!(f, "0x{:02x}", value)
                } else {
                    write!(f, "0x{:02x} (", value)?;
                    for (i, flag) in flags.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{}", flag)?;
                    }
                    write!(f, ")")
                }
            }
            Self::DeviceId(bytes, desc) => {
                write!(f, "[")?;
                for (i, b) in bytes.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{:02x}", b)?;
                }
                write!(f, "] ({})", desc)
            }
            Self::Phase(value, desc) => write!(f, "0x{:02x} ({})", value, desc),
            Self::Page(value, desc) => write!(f, "0x{:02x} ({})", value, desc),
            Self::VoutMode(value, desc) => write!(f, "0x{:02x} ({})", value, desc),
            Self::Capability(value, flags) => {
                if flags.is_empty() {
                    write!(f, "0x{:02x}", value)
                } else {
                    write!(f, "0x{:02x} (", value)?;
                    for (i, flag) in flags.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{}", flag)?;
                    }
                    write!(f, ")")
                }
            }
            Self::ConfigWord(value, desc) => write!(f, "0x{:04x} ({})", value, desc),
            Self::ConfigBytes(bytes, desc) => {
                write!(f, "[")?;
                for (i, b) in bytes.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{:02x}", b)?;
                }
                write!(f, "] ({})", desc)
            }
            Self::String(s) => write!(f, "{}", s),
            Self::Raw(bytes) => write!(f, "{:02x?}", bytes),
        }
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Parse a little-endian u16 from data
fn parse_u16_le(data: &[u8]) -> Option<u16> {
    data.get(..2)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u16::from_le_bytes)
}

/// Parse string data from PMBus block read format
fn parse_string_data(data: &[u8]) -> String {
    // PMBus block reads have length byte first
    let text_bytes = if data.len() > 1 && data[0] as usize == data.len() - 1 {
        &data[1..]
    } else {
        data
    };

    match std::str::from_utf8(text_bytes) {
        Ok(s) => s.trim_end_matches('\0').to_string(),
        Err(_) => format!("{:02x?}", text_bytes),
    }
}

// ============================================================================
// Value Parsing
// ============================================================================

/// Parse PMBus value from raw data based on command type
pub fn parse_pmbus_value(cmd: PmbusCommand, data: &[u8], vout_mode: Option<u8>) -> PmbusValue {
    if data.is_empty() {
        return PmbusValue::Raw(vec![]);
    }

    // Try specialized parsers first
    if let Some(value) = parse_voltage_value(cmd, data, vout_mode) {
        return value;
    }
    if let Some(value) = parse_current_value(cmd, data) {
        return value;
    }
    if let Some(value) = parse_temperature_value(cmd, data) {
        return value;
    }
    if let Some(value) = parse_time_value(cmd, data) {
        return value;
    }
    if let Some(value) = parse_status_value(cmd, data) {
        return value;
    }
    if let Some(value) = parse_string_value(cmd, data) {
        return value;
    }

    // Default: raw bytes
    PmbusValue::Raw(data.to_vec())
}

fn parse_voltage_value(
    cmd: PmbusCommand,
    data: &[u8],
    vout_mode: Option<u8>,
) -> Option<PmbusValue> {
    use PmbusCommand::*;

    match cmd {
        ReadVin | VinOn | VinOff | VinOvFaultLimit | VinUvWarnLimit => {
            parse_u16_le(data).map(|v| PmbusValue::Voltage(PmbusVoltage::from_linear11(v)))
        }
        VoutScaleLoop => parse_u16_le(data).map(|v| {
            let linear11 = Linear11::new(v);
            PmbusValue::ScaleFactor(linear11.to_f32())
        }),
        ReadVout | VoutCommand | VoutMax | VoutMarginHigh | VoutMarginLow | VoutMin
        | VoutOvFaultLimit | VoutOvWarnLimit | VoutUvWarnLimit | VoutUvFaultLimit => {
            parse_u16_le(data).map(|v| {
                let mode = vout_mode.unwrap_or(DEFAULT_VOUT_MODE);
                PmbusValue::Voltage(PmbusVoltage::from_linear16(v, mode))
            })
        }
        _ => None,
    }
}

fn parse_current_value(cmd: PmbusCommand, data: &[u8]) -> Option<PmbusValue> {
    use PmbusCommand::*;

    match cmd {
        ReadIout | IoutOcFaultLimit | IoutOcWarnLimit => {
            parse_u16_le(data).map(|v| PmbusValue::Current(PmbusCurrent::from_linear11(v)))
        }
        _ => None,
    }
}

fn parse_temperature_value(cmd: PmbusCommand, data: &[u8]) -> Option<PmbusValue> {
    use PmbusCommand::*;

    match cmd {
        ReadTemperature1 | OtFaultLimit | OtWarnLimit => {
            parse_u16_le(data).map(|v| PmbusValue::Temperature(PmbusTemperature::from_linear11(v)))
        }
        _ => None,
    }
}

fn parse_time_value(cmd: PmbusCommand, data: &[u8]) -> Option<PmbusValue> {
    use PmbusCommand::*;

    match cmd {
        TonDelay | TonRise | TonMaxFaultLimit | ToffDelay | ToffFall => {
            parse_u16_le(data).map(|v| PmbusValue::Time(PmbusTime::from_linear11(v)))
        }
        _ => None,
    }
}

fn parse_status_value(cmd: PmbusCommand, data: &[u8]) -> Option<PmbusValue> {
    use PmbusCommand::*;

    match cmd {
        StatusWord => parse_u16_le(data).map(|v| {
            let flags = StatusDecoder::decode_status_word(v);
            PmbusValue::StatusWord(v, flags)
        }),
        StatusVout if !data.is_empty() => {
            let flags = StatusDecoder::decode_status_vout(data[0]);
            Some(PmbusValue::StatusByte(data[0], flags))
        }
        StatusIout if !data.is_empty() => {
            let flags = StatusDecoder::decode_status_iout(data[0]);
            Some(PmbusValue::StatusByte(data[0], flags))
        }
        StatusInput if !data.is_empty() => {
            let flags = StatusDecoder::decode_status_input(data[0]);
            Some(PmbusValue::StatusByte(data[0], flags))
        }
        StatusTemperature if !data.is_empty() => {
            let flags = StatusDecoder::decode_status_temp(data[0]);
            Some(PmbusValue::StatusByte(data[0], flags))
        }
        StatusCml if !data.is_empty() => {
            let flags = StatusDecoder::decode_status_cml(data[0]);
            Some(PmbusValue::StatusByte(data[0], flags))
        }
        IoutOcFaultResponse | OtFaultResponse | VinOvFaultResponse | TonMaxFaultResponse
            if !data.is_empty() =>
        {
            let desc = StatusDecoder::decode_fault_response(data[0]);
            Some(PmbusValue::FaultResponse(data[0], desc))
        }
        Operation if !data.is_empty() => {
            let desc = StatusDecoder::decode_operation(data[0]);
            Some(PmbusValue::Operation(data[0], desc))
        }
        OnOffConfig if !data.is_empty() => {
            let flags = StatusDecoder::decode_on_off_config(data[0]);
            Some(PmbusValue::OnOffConfig(data[0], flags))
        }
        IcDeviceId => {
            let desc = StatusDecoder::decode_device_id(data);
            Some(PmbusValue::DeviceId(data.to_vec(), desc))
        }
        Phase if !data.is_empty() => {
            let desc = StatusDecoder::decode_phase(data[0]);
            Some(PmbusValue::Phase(data[0], desc))
        }
        Page if !data.is_empty() => {
            let desc = StatusDecoder::decode_page(data[0]);
            Some(PmbusValue::Page(data[0], desc))
        }
        VoutMode if !data.is_empty() => {
            let desc = StatusDecoder::decode_vout_mode(data[0]);
            Some(PmbusValue::VoutMode(data[0], desc))
        }
        Capability if !data.is_empty() => {
            let flags = StatusDecoder::decode_capability(data[0]);
            Some(PmbusValue::Capability(data[0], flags))
        }
        StackConfig => parse_u16_le(data).map(|v| {
            let desc = StatusDecoder::decode_stack_config(v);
            PmbusValue::ConfigWord(v, desc)
        }),
        Interleave => parse_u16_le(data).map(|v| {
            let desc = StatusDecoder::decode_interleave(v);
            PmbusValue::ConfigWord(v, desc)
        }),
        SyncConfig if !data.is_empty() => {
            let desc = StatusDecoder::decode_sync_config(data[0]);
            Some(PmbusValue::ConfigBytes(vec![data[0]], desc))
        }
        CompensationConfig => {
            let desc = StatusDecoder::decode_compensation_config(data);
            Some(PmbusValue::ConfigBytes(data.to_vec(), desc))
        }
        PinDetectOverride => parse_u16_le(data).map(|v| {
            let desc = StatusDecoder::decode_pin_detect_override(v);
            PmbusValue::ConfigWord(v, desc)
        }),
        FrequencySwitch => {
            parse_u16_le(data).map(|v| PmbusValue::Frequency(PmbusFrequency::from_linear11(v)))
        }
        _ => None,
    }
}

fn parse_string_value(cmd: PmbusCommand, data: &[u8]) -> Option<PmbusValue> {
    use PmbusCommand::*;

    match cmd {
        MfrId | MfrModel | MfrRevision => Some(PmbusValue::String(parse_string_data(data))),
        _ => None,
    }
}

// ============================================================================
// Status Register Bits
// ============================================================================

bitflags! {
    /// PMBus STATUS_WORD (0x79) register flags
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct StatusWord: u16 {
        const VOUT = 0x8000;
        const IOUT = 0x4000;
        const INPUT = 0x2000;
        const MFR = 0x1000;
        const PGOOD = 0x0800;
        const FANS = 0x0400;
        const OTHER = 0x0200;
        const UNKNOWN = 0x0100;
        const BUSY = 0x0080;
        const OFF = 0x0040;
        const VOUT_OV = 0x0020;
        const IOUT_OC = 0x0010;
        const VIN_UV = 0x0008;
        const TEMP = 0x0004;
        const CML = 0x0002;
        const NONE = 0x0001;
    }
}

bitflags! {
    /// PMBus STATUS_VOUT (0x7A) register flags
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct StatusVout: u8 {
        const VOUT_OV_FAULT = 0x80;
        const VOUT_OV_WARN = 0x40;
        const VOUT_UV_WARN = 0x20;
        const VOUT_UV_FAULT = 0x10;
        const VOUT_MAX = 0x08;
        const TON_MAX_FAULT = 0x02;
        const VOUT_MIN = 0x01;
    }
}

bitflags! {
    /// PMBus STATUS_IOUT (0x7B) register flags
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct StatusIout: u8 {
        const IOUT_OC_FAULT = 0x80;
        const IOUT_OC_LV_FAULT = 0x40;
        const IOUT_OC_WARN = 0x20;
        const IOUT_UC_FAULT = 0x10;
        const CURR_SHARE_FAULT = 0x08;
        const IN_PWR_LIM = 0x04;
        const POUT_OP_FAULT = 0x02;
        const POUT_OP_WARN = 0x01;
    }
}

bitflags! {
    /// PMBus STATUS_INPUT (0x7C) register flags
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct StatusInput: u8 {
        const VIN_OV_FAULT = 0x80;
        const VIN_OV_WARN = 0x40;
        const VIN_UV_WARN = 0x20;
        const VIN_UV_FAULT = 0x10;
        const UNIT_OFF_VIN_LOW = 0x08;
        const IIN_OC_FAULT = 0x04;
        const IIN_OC_WARN = 0x02;
        const PIN_OP_WARN = 0x01;
    }
}

bitflags! {
    /// PMBus STATUS_TEMPERATURE (0x7D) register flags
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct StatusTemperature: u8 {
        const OT_FAULT = 0x80;
        const OT_WARN = 0x40;
        const UT_WARN = 0x20;
        const UT_FAULT = 0x10;
    }
}

bitflags! {
    /// PMBus STATUS_CML (0x7E) register flags
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct StatusCml: u8 {
        const INVALID_CMD = 0x80;
        const INVALID_DATA = 0x40;
        const PEC_FAULT = 0x20;
        const MEMORY_FAULT = 0x10;
        const PROCESSOR_FAULT = 0x08;
        const OTHER_COMM_FAULT = 0x02;
        const OTHER_MEM_LOGIC = 0x01;
    }
}

/// PMBus OPERATION (0x01) command values
/// Note: These are not bitflags, but discrete command values
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Operation {
    OffImmediate = 0x00,
    MarginLow = 0x18,
    MarginHigh = 0x28,
    SoftOff = 0x40,
    On = 0x80,
    OnMarginLow = 0x98,
    OnMarginHigh = 0xA8,
}

impl Operation {
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

impl From<Operation> for u8 {
    fn from(op: Operation) -> Self {
        op as u8
    }
}

impl TryFrom<u8> for Operation {
    type Error = PMBusError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x00 => Ok(Self::OffImmediate),
            0x18 => Ok(Self::MarginLow),
            0x28 => Ok(Self::MarginHigh),
            0x40 => Ok(Self::SoftOff),
            0x80 => Ok(Self::On),
            0x98 => Ok(Self::OnMarginLow),
            0xA8 => Ok(Self::OnMarginHigh),
            _ => Err(PMBusError::InvalidDataFormat),
        }
    }
}

bitflags! {
    /// PMBus ON_OFF_CONFIG (0x02) register flags
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct OnOffConfig: u8 {
        const PU = 0x10;
        const CMD = 0x08;
        const CP = 0x04;
        const POLARITY = 0x02;
        const DELAY = 0x01;
    }
}

// ============================================================================
// Status Decoder
// ============================================================================

/// Macro to generate status decoder methods
macro_rules! decode_status_flags {
    ($flags:expr => {
        $($flag:expr => $desc:literal),* $(,)?
    }) => {{
        let mut desc = Vec::new();
        $(if $flags.contains($flag) { desc.push($desc); })*
        desc
    }};
}

pub struct StatusDecoder;

impl StatusDecoder {
    pub fn decode_status_word(status: u16) -> Vec<&'static str> {
        let flags = StatusWord::from_bits_truncate(status);
        let mut desc = decode_status_flags!(flags => {
            StatusWord::VOUT => "VOUT fault/warning",
            StatusWord::IOUT => "IOUT fault/warning",
            StatusWord::INPUT => "INPUT fault/warning",
            StatusWord::MFR => "MFR specific",
            StatusWord::PGOOD => "PGOOD",
            StatusWord::FANS => "FAN fault/warning",
            StatusWord::OTHER => "OTHER",
            StatusWord::UNKNOWN => "UNKNOWN",
            StatusWord::BUSY => "BUSY",
            StatusWord::OFF => "OFF",
            StatusWord::VOUT_OV => "VOUT_OV fault",
            StatusWord::IOUT_OC => "IOUT_OC fault",
            StatusWord::VIN_UV => "VIN_UV fault",
            StatusWord::TEMP => "TEMP fault/warning",
            StatusWord::CML => "CML fault",
        });

        if flags.contains(StatusWord::NONE) && desc.is_empty() {
            desc.push("NONE_OF_THE_ABOVE");
        }
        desc
    }

    pub fn decode_status_vout(status: u8) -> Vec<&'static str> {
        let flags = StatusVout::from_bits_truncate(status);
        decode_status_flags!(flags => {
            StatusVout::VOUT_OV_FAULT => "OV fault",
            StatusVout::VOUT_OV_WARN => "OV warning",
            StatusVout::VOUT_UV_WARN => "UV warning",
            StatusVout::VOUT_UV_FAULT => "UV fault",
            StatusVout::VOUT_MAX => "at MAX",
            StatusVout::TON_MAX_FAULT => "failed to start",
            StatusVout::VOUT_MIN => "at MIN",
        })
    }

    pub fn decode_status_iout(status: u8) -> Vec<&'static str> {
        let flags = StatusIout::from_bits_truncate(status);
        decode_status_flags!(flags => {
            StatusIout::IOUT_OC_FAULT => "OC fault",
            StatusIout::IOUT_OC_LV_FAULT => "OC+LV fault",
            StatusIout::IOUT_OC_WARN => "OC warning",
            StatusIout::IOUT_UC_FAULT => "UC fault",
            StatusIout::CURR_SHARE_FAULT => "current share fault",
            StatusIout::IN_PWR_LIM => "power limiting",
            StatusIout::POUT_OP_FAULT => "overpower fault",
            StatusIout::POUT_OP_WARN => "overpower warning",
        })
    }

    pub fn decode_status_input(status: u8) -> Vec<&'static str> {
        let flags = StatusInput::from_bits_truncate(status);
        decode_status_flags!(flags => {
            StatusInput::VIN_OV_FAULT => "VIN OV fault",
            StatusInput::VIN_OV_WARN => "VIN OV warning",
            StatusInput::VIN_UV_WARN => "VIN UV warning",
            StatusInput::VIN_UV_FAULT => "VIN UV fault",
            StatusInput::UNIT_OFF_VIN_LOW => "off due to low VIN",
            StatusInput::IIN_OC_FAULT => "IIN OC fault",
            StatusInput::IIN_OC_WARN => "IIN OC warning",
            StatusInput::PIN_OP_WARN => "input overpower warning",
        })
    }

    pub fn decode_status_temp(status: u8) -> Vec<&'static str> {
        let flags = StatusTemperature::from_bits_truncate(status);
        decode_status_flags!(flags => {
            StatusTemperature::OT_FAULT => "overtemp fault",
            StatusTemperature::OT_WARN => "overtemp warning",
            StatusTemperature::UT_WARN => "undertemp warning",
            StatusTemperature::UT_FAULT => "undertemp fault",
        })
    }

    pub fn decode_status_cml(status: u8) -> Vec<&'static str> {
        let flags = StatusCml::from_bits_truncate(status);
        decode_status_flags!(flags => {
            StatusCml::INVALID_CMD => "invalid command",
            StatusCml::INVALID_DATA => "invalid data",
            StatusCml::PEC_FAULT => "PEC error",
            StatusCml::MEMORY_FAULT => "memory fault",
            StatusCml::PROCESSOR_FAULT => "processor fault",
            StatusCml::OTHER_COMM_FAULT => "other comm fault",
            StatusCml::OTHER_MEM_LOGIC => "other mem/logic fault",
        })
    }

    pub fn decode_fault_response(response: u8) -> String {
        let response_type = (response >> 6) & 0x03; // Bits 7:6 (correct format)
        let retry_count = (response >> 3) & 0x07; // Bits 5:3 (correct format)
        let delay_time = response & 0x07; // Bits 2:0

        let response_desc = match response_type {
            0b00 => "ignore",
            0b01 => "delayed shutdown",
            0b10 => "immediate shutdown",
            0b11 => "special shutdown", // varies by command
            _ => "unknown",
        };

        let retries_desc = match retry_count {
            0 => "no retries (latch off)".to_string(),
            1..=6 => format!("{} retries", retry_count),
            7 => "infinite retries".to_string(),
            _ => "unknown".to_string(),
        };

        let delay_desc = match delay_time {
            0 => "TON_RISE delay".to_string(),
            1 => "TON_RISE delay".to_string(),
            2..=4 => format!("{} × TON_RISE delay", delay_time),
            5..=7 => format!("{} × TON_RISE delay", delay_time),
            _ => "unknown delay".to_string(),
        };

        if response_type == 0b00 {
            response_desc.to_string()
        } else {
            format!("{}, {}, {}", response_desc, retries_desc, delay_desc)
        }
    }

    pub fn decode_operation(value: u8) -> String {
        match Operation::try_from(value) {
            Ok(Operation::OffImmediate) => "OFF immediate".to_string(),
            Ok(Operation::MarginLow) => "margin low test".to_string(),
            Ok(Operation::MarginHigh) => "margin high test".to_string(),
            Ok(Operation::SoftOff) => "soft off (with delay)".to_string(),
            Ok(Operation::On) => "ON".to_string(),
            Ok(Operation::OnMarginLow) => "ON with margin low".to_string(),
            Ok(Operation::OnMarginHigh) => "ON with margin high".to_string(),
            Err(_) => format!("unknown (0x{:02x})", value),
        }
    }

    pub fn decode_on_off_config(value: u8) -> Vec<&'static str> {
        let flags = OnOffConfig::from_bits_truncate(value);
        decode_status_flags!(flags => {
            OnOffConfig::PU => "power up from CONTROL pin",
            OnOffConfig::CMD => "OPERATION command enabled",
            OnOffConfig::CP => "CONTROL pin present",
            OnOffConfig::POLARITY => "CONTROL active high",
            OnOffConfig::DELAY => "turn-off delay enabled",
        })
    }

    pub fn decode_device_id(data: &[u8]) -> String {
        // Known TPS546 device IDs
        const TPS546D24A_ID1: &[u8; 6] = &[0x54, 0x49, 0x54, 0x6B, 0x24, 0x41];
        const TPS546D24A_ID2: &[u8; 6] = &[0x54, 0x49, 0x54, 0x6D, 0x24, 0x41];
        const TPS546D24S_ID: &[u8; 6] = &[0x54, 0x49, 0x54, 0x6D, 0x24, 0x62];

        // Skip length byte if present (PMBus block read format)
        let id_bytes = if data.len() > 6 && data[0] as usize == data.len() - 1 {
            &data[1..]
        } else {
            data
        };

        if id_bytes.len() >= 6 {
            let id_slice = &id_bytes[0..6];
            if id_slice == TPS546D24A_ID1 || id_slice == TPS546D24A_ID2 {
                "TPS546D24A".to_string()
            } else if id_slice == TPS546D24S_ID {
                "TPS546D24S".to_string()
            } else {
                // Try to decode as ASCII if printable
                if id_slice.iter().all(|&b| b.is_ascii_graphic() || b == b' ') {
                    match std::str::from_utf8(id_slice) {
                        Ok(s) => format!("unknown device: {}", s.trim()),
                        Err(_) => "unknown device".to_string(),
                    }
                } else {
                    "unknown device".to_string()
                }
            }
        } else {
            "invalid ID length".to_string()
        }
    }

    pub fn decode_phase(value: u8) -> String {
        match value {
            0xFF => "all phases".to_string(),
            0x00..=0x07 => format!("phase {}", value),
            _ => format!("invalid phase (0x{:02x})", value),
        }
    }

    pub fn decode_page(value: u8) -> String {
        match value {
            0x00 => "page 0 (main rail)".to_string(),
            0x01..=0x1F => format!("page {} (aux rail)", value),
            _ => format!("invalid page (0x{:02x})", value),
        }
    }

    pub fn decode_vout_mode(value: u8) -> String {
        // Check if this looks like a TPS546D24A format
        // TPS546D24A uses: bit 7=REL, bits 6:5=MODE, bits 4:0=exponent
        let rel_field = (value >> 7) & 0x01;
        let mode_field_tps546 = (value >> 5) & 0x03;
        let exponent = value & 0x1F;

        // Convert 5-bit two's complement to signed
        let exp_signed = extract_5bit_exponent(exponent);

        // TPS546D24A pattern: MODE field is 2 bits (6:5), only supports 00b=linear
        // and has REL field in bit 7, with exponent range -4 to -12
        if mode_field_tps546 == 0b00 && (-12..=-4).contains(&exp_signed) {
            let format_type = if rel_field == 1 {
                "relative"
            } else {
                "absolute"
            };
            format!("{}, ULINEAR16, ^{}", format_type, exp_signed)
        } else {
            // Fall back to standard PMBus format interpretation
            let mode = (value >> 5) & 0x07;
            match mode {
                0b000 => format!("Linear format, exponent {}", exp_signed),
                0b001 => format!("VID format, exponent {}", exp_signed),
                0b010 => format!("Direct format, exponent {}", exp_signed),
                0b011 => "IEEE754 half precision".to_string(),
                _ => format!("reserved mode {} (0x{:02x})", mode, value),
            }
        }
    }

    pub fn decode_capability(value: u8) -> Vec<&'static str> {
        let mut flags = Vec::new();

        if value & 0x80 != 0 {
            flags.push("PEC supported");
        }
        if value & 0x40 != 0 {
            flags.push("400kHz max");
        } else {
            flags.push("100kHz max");
        }
        if value & 0x20 != 0 {
            flags.push("SMBALERT supported");
        }
        if value & 0x10 != 0 {
            flags.push("Bus info command");
        }

        let version = value & 0x0F;
        match version {
            0b0001 => flags.push("PMBus v1.0"),
            0b0010 => flags.push("PMBus v1.1"),
            0b0011 => flags.push("PMBus v1.2"),
            0b0100 => flags.push("PMBus v1.3"),
            _ => flags.push("unknown version"),
        }

        flags
    }

    pub fn decode_stack_config(value: u16) -> String {
        let position = (value >> 8) & 0xFF;
        let total = value & 0xFF;

        if position == 0 && total == 0 {
            "not stacked".to_string()
        } else if position == 0 || total == 0 {
            format!("invalid stacking (pos={}, total={})", position, total)
        } else {
            format!("position {} of {} in stack", position, total)
        }
    }

    pub fn decode_interleave(value: u16) -> String {
        let phase_count = (value >> 8) & 0xFF;
        let phase_offset = value & 0xFF;

        if phase_count <= 1 {
            "no interleaving".to_string()
        } else {
            format!("{} phases, offset {}", phase_count, phase_offset)
        }
    }

    pub fn decode_sync_config(value: u8) -> String {
        let sync_out = (value >> 4) & 0x0F;
        let sync_in = value & 0x0F;

        let sync_out_desc = match sync_out {
            0 => "disabled",
            1 => "master",
            _ => "reserved",
        };

        let sync_in_desc = match sync_in {
            0 => "disabled",
            1 => "slave",
            _ => "reserved",
        };

        format!("out: {}, in: {}", sync_out_desc, sync_in_desc)
    }

    pub fn decode_compensation_config(data: &[u8]) -> String {
        if data.is_empty() {
            return "no data".to_string();
        }

        // Skip length byte if present
        let config_data = if data.len() > 1 && data[0] as usize == data.len() - 1 {
            &data[1..]
        } else {
            data
        };

        if config_data.len() >= 2 {
            format!("coefficients: {:02x?}", config_data)
        } else {
            "insufficient data".to_string()
        }
    }

    pub fn decode_pin_detect_override(value: u16) -> String {
        if value == 0xFFFF {
            "auto-detect all pins".to_string()
        } else if value == 0x0000 {
            "override all pins".to_string()
        } else {
            format!("mixed config (0x{:04x})", value)
        }
    }
}

// ============================================================================
// Linear Format Conversion Modules
// ============================================================================

// ============================================================================
// Shared PMBus Format Utilities
// ============================================================================

/// Extract 5-bit two's complement exponent from PMBus format
fn extract_5bit_exponent(raw_exp: u8) -> i8 {
    if raw_exp & 0x10 != 0 {
        (raw_exp | 0xE0) as i8 // Sign extend
    } else {
        raw_exp as i8
    }
}

/// SLINEAR11 data format conversion
pub mod linear11 {
    use super::extract_5bit_exponent;

    const EXPONENT_SHIFT: u8 = 11;
    const MANTISSA_MASK: u16 = 0x07FF;
    const MANTISSA_SIGN_BIT: u16 = 0x0400;

    /// Convert SLINEAR11 format to floating point
    pub fn to_float(value: u16) -> f32 {
        let exp_raw = ((value >> EXPONENT_SHIFT) & 0x1F) as u8;
        let exponent = extract_5bit_exponent(exp_raw) as i32;

        let mant_raw = (value & MANTISSA_MASK) as i16;
        let mantissa = if mant_raw & MANTISSA_SIGN_BIT as i16 != 0 {
            ((mant_raw as u16 | 0xF800) as i16) as i32
        } else {
            mant_raw as i32
        };

        mantissa as f32 * 2.0_f32.powi(exponent)
    }

    /// Convert ULINEAR11 format to floating point (unsigned mantissa)
    pub fn to_float_unsigned(value: u16) -> f32 {
        let exp_raw = ((value >> EXPONENT_SHIFT) & 0x1F) as u8;
        let exponent = extract_5bit_exponent(exp_raw) as i32;

        let mantissa = (value & MANTISSA_MASK) as u32;
        mantissa as f32 * 2.0_f32.powi(exponent)
    }

    /// Convert floating point to SLINEAR11 format
    pub fn from_float(value: f32) -> u16 {
        if value == 0.0 {
            return 0;
        }

        let mut best_exp = 0i8;
        let mut best_error = f32::MAX;

        for exp in -16i8..=15 {
            let mantissa_f = value / 2.0_f32.powi(exp as i32);

            if (-1024.0..1024.0).contains(&mantissa_f) {
                let mantissa = mantissa_f.round() as i32;
                let reconstructed = mantissa as f32 * 2.0_f32.powi(exp as i32);
                let error = (reconstructed - value).abs();

                if error < best_error {
                    best_error = error;
                    best_exp = exp;
                }
            }
        }

        let mantissa = (value / 2.0_f32.powi(best_exp as i32)).round() as i32;
        let exp_bits = (best_exp as u16) & 0x1F;
        let mant_bits = (mantissa as u16) & MANTISSA_MASK;

        (exp_bits << EXPONENT_SHIFT) | mant_bits
    }
}

/// ULINEAR16 data format conversion
pub mod linear16 {
    use super::{extract_5bit_exponent, PMBusError};

    /// Convert ULINEAR16 format to floating point
    pub fn to_float(value: u16, vout_mode: u8) -> f32 {
        let exp_raw = vout_mode & 0x1F;
        let exponent = extract_5bit_exponent(exp_raw) as i32;
        value as f32 * 2.0_f32.powi(exponent)
    }

    /// Convert floating point to ULINEAR16 format
    pub fn from_float(value: f32, vout_mode: u8) -> Result<u16, PMBusError> {
        let exp_raw = vout_mode & 0x1F;
        let exponent = extract_5bit_exponent(exp_raw) as i32;

        let mantissa = (value / 2.0_f32.powi(exponent)).round();
        if mantissa < 0.0 || mantissa > u16::MAX as f32 {
            return Err(PMBusError::ValueOutOfRange);
        }

        Ok(mantissa as u16)
    }
}

// ============================================================================
// Error Types
// ============================================================================

#[derive(Error, Debug)]
pub enum PMBusError {
    #[error("Invalid data format")]
    InvalidDataFormat,
    #[error("Value out of range")]
    ValueOutOfRange,
    #[error("Command not supported")]
    CommandNotSupported,
    #[error("Communication error")]
    CommunicationError,
}

// Import type-safe Linear format types
mod pmbus_types;
pub use pmbus_types::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pmbus_voltage_direct() {
        let voltage = PmbusVoltage::new(3.3);
        assert_eq!(voltage.value(), 3.3);

        let voltage2 = PmbusVoltage::from(1.8f32);
        assert_eq!(voltage2.value(), 1.8);

        assert_eq!(format!("{}", voltage), "3.300V");
    }

    #[test]
    fn test_pmbus_voltage_linear11() {
        // Test creating from Linear11 raw value
        let voltage = PmbusVoltage::from_linear11(0x0064); // exp=0, mant=100
        assert_eq!(voltage.value(), 100.0);

        // Check that it preserves the Linear11 encoding
        if let PmbusVoltage::Linear11(l11) = voltage {
            assert_eq!(l11.0, 0x0064);
        } else {
            panic!("Expected Linear11 variant");
        }

        // Test conversion back to Linear11
        let l11 = voltage.to_linear11().unwrap();
        assert_eq!(l11.0, 0x0064);
    }

    #[test]
    fn test_pmbus_voltage_linear16() {
        let mode = VoutMode::new(0x97); // TPS546 default
        let voltage = PmbusVoltage::from_linear16(0x0266, 0x97); // 1.199V

        assert!((voltage.value() - 1.199).abs() < 0.001);

        // Check that it preserves the Linear16 encoding and mode
        if let PmbusVoltage::Linear16(l16) = voltage {
            assert_eq!(l16.value, 0x0266);
            assert_eq!(l16.mode.0, 0x97);
        } else {
            panic!("Expected Linear16 variant");
        }

        // Test conversion back to Linear16 with same mode
        let l16 = voltage.to_linear16(mode).unwrap();
        assert_eq!(l16.value, 0x0266);
        assert_eq!(l16.mode, mode);
    }

    #[test]
    fn test_pmbus_voltage_ordering() {
        let v1 = PmbusVoltage::new(1.0);
        let v2 = PmbusVoltage::from_linear11(0x0200); // 2^0 * 512 = 512.0
        let v3 = PmbusVoltage::from_linear16(0x0100, 0x97); // Should be around 0.5V

        assert!(v1 < v2);
        assert!(v3 < v1);
    }

    #[test]
    fn test_pmbus_voltage_conversions() {
        // Test converting between formats
        let original = 1.2f32;
        let direct = PmbusVoltage::new(original);

        // Convert to Linear11
        let l11 = direct.to_linear11().unwrap();
        let l11_voltage = PmbusVoltage::Linear11(l11);
        assert!((l11_voltage.value() - original).abs() < 0.01); // Allow some rounding error

        // Convert to Linear16
        let mode = VoutMode::new(0x97);
        let l16 = direct.to_linear16(mode).unwrap();
        let l16_voltage = PmbusVoltage::Linear16(l16);
        assert!((l16_voltage.value() - original).abs() < 0.001); // Linear16 is more precise
    }

    #[test]
    fn test_pmbus_voltage_trait_implementations() {
        let v1 = PmbusVoltage::new(2.5);
        let v2 = v1; // Test Copy
        assert_eq!(v1, v2); // Test PartialEq

        let f: f32 = v1.into(); // Test Into<f32>
        assert_eq!(f, 2.5);

        let v3 = PmbusVoltage::default(); // Test Default
        assert_eq!(v3.value(), 0.0);

        println!("{:?}", v1); // Test Debug
        println!("{}", v1); // Test Display
    }

    #[test]
    fn test_pmbus_voltage_precision_preservation() {
        // Verify precision is maintained by keeping the original encoding
        let l11_raw = 0xD2E6; // Known Linear11 value: 11.59375
        let voltage = PmbusVoltage::from_linear11(l11_raw);

        // Converting back to Linear11 should give exact same value
        let l11_back = voltage.to_linear11().unwrap();
        assert_eq!(l11_back.0, l11_raw);

        // But converting to Linear16 and back might have rounding
        let mode = VoutMode::new(0x17);
        let l16 = voltage.to_linear16(mode).unwrap();
        let l16_voltage = PmbusVoltage::Linear16(l16);

        // Should be close but not necessarily exact due to different precision
        assert!((l16_voltage.value() - voltage.value()).abs() < 0.01);
    }
}
