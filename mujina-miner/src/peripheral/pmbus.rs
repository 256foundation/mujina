//! PMBus Protocol Support
//!
//! This module provides generic PMBus protocol definitions and utilities
//! that can be used by PMBus-compliant device drivers.
//!
//! PMBus is a variant of SMBus with extensions for power management.
//! Specification: <https://pmbus.org/specification-documents/>

use thiserror::Error;

/// PMBus standard command codes
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum PmbusCommand {
    Page = 0x00,
    Operation = 0x01,
    OnOffConfig = 0x02,
    ClearFaults = 0x03,
    Phase = 0x04,
    Capability = 0x19,
    VoutMode = 0x20,
    VoutCommand = 0x21,
    VoutMax = 0x24,
    VoutMarginHigh = 0x25,
    VoutMarginLow = 0x26,
    VoutScaleLoop = 0x29,
    VoutMin = 0x2B,
    FrequencySwitch = 0x33,
    VinOn = 0x35,
    VinOff = 0x36,
    Interleave = 0x37,
    VoutOvFaultLimit = 0x40,
    VoutOvWarnLimit = 0x42,
    VoutUvWarnLimit = 0x43,
    VoutUvFaultLimit = 0x44,
    IoutOcFaultLimit = 0x46,
    IoutOcFaultResponse = 0x47,
    IoutOcWarnLimit = 0x4A,
    OtFaultLimit = 0x4F,
    OtFaultResponse = 0x50,
    OtWarnLimit = 0x51,
    VinOvFaultLimit = 0x55,
    VinOvFaultResponse = 0x56,
    VinUvWarnLimit = 0x58,
    TonDelay = 0x60,
    TonRise = 0x61,
    TonMaxFaultLimit = 0x62,
    TonMaxFaultResponse = 0x63,
    ToffDelay = 0x64,
    ToffFall = 0x65,
    StatusWord = 0x79,
    StatusVout = 0x7A,
    StatusIout = 0x7B,
    StatusInput = 0x7C,
    StatusTemperature = 0x7D,
    StatusCml = 0x7E,
    StatusOther = 0x7F,
    StatusMfrSpecific = 0x80,
    ReadVin = 0x88,
    ReadVout = 0x8B,
    ReadIout = 0x8C,
    ReadTemperature1 = 0x8D,
    MfrId = 0x99,
    MfrModel = 0x9A,
    MfrRevision = 0x9B,
    IcDeviceId = 0xAD,
    CompensationConfig = 0xB1,
    SyncConfig = 0xE4,
    StackConfig = 0xEC,
    // TODO: Refactor manufacturer-specific commands to be device-specific
    // These TPS546-specific commands should be moved to a separate Tps546Command enum
    // to avoid polluting the generic PMBus namespace and enable proper device-specific
    // type safety. Other PMBus devices will have their own manufacturer commands.
    MiscOptions = 0xED,
    PinDetectOverride = 0xEE,
    SlaveAddress = 0xEF,
    NvmChecksum = 0xF0,
    SimulateFault = 0xF1,
    FusionId0 = 0xFC,
    FusionId1 = 0xFD,
}

impl PmbusCommand {
    /// Get the command name as a string
    pub fn name(&self) -> &'static str {
        match self {
            Self::Page => "PAGE",
            Self::Operation => "OPERATION",
            Self::OnOffConfig => "ON_OFF_CONFIG",
            Self::ClearFaults => "CLEAR_FAULTS",
            Self::Phase => "PHASE",
            Self::Capability => "CAPABILITY",
            Self::VoutMode => "VOUT_MODE",
            Self::VoutCommand => "VOUT_COMMAND",
            Self::VoutMax => "VOUT_MAX",
            Self::VoutMarginHigh => "VOUT_MARGIN_HIGH",
            Self::VoutMarginLow => "VOUT_MARGIN_LOW",
            Self::VoutScaleLoop => "VOUT_SCALE_LOOP",
            Self::VoutMin => "VOUT_MIN",
            Self::FrequencySwitch => "FREQUENCY_SWITCH",
            Self::VinOn => "VIN_ON",
            Self::VinOff => "VIN_OFF",
            Self::Interleave => "INTERLEAVE",
            Self::VoutOvFaultLimit => "VOUT_OV_FAULT_LIMIT",
            Self::VoutOvWarnLimit => "VOUT_OV_WARN_LIMIT",
            Self::VoutUvWarnLimit => "VOUT_UV_WARN_LIMIT",
            Self::VoutUvFaultLimit => "VOUT_UV_FAULT_LIMIT",
            Self::IoutOcFaultLimit => "IOUT_OC_FAULT_LIMIT",
            Self::IoutOcFaultResponse => "IOUT_OC_FAULT_RESPONSE",
            Self::IoutOcWarnLimit => "IOUT_OC_WARN_LIMIT",
            Self::OtFaultLimit => "OT_FAULT_LIMIT",
            Self::OtFaultResponse => "OT_FAULT_RESPONSE",
            Self::OtWarnLimit => "OT_WARN_LIMIT",
            Self::VinOvFaultLimit => "VIN_OV_FAULT_LIMIT",
            Self::VinOvFaultResponse => "VIN_OV_FAULT_RESPONSE",
            Self::VinUvWarnLimit => "VIN_UV_WARN_LIMIT",
            Self::TonDelay => "TON_DELAY",
            Self::TonRise => "TON_RISE",
            Self::TonMaxFaultLimit => "TON_MAX_FAULT_LIMIT",
            Self::TonMaxFaultResponse => "TON_MAX_FAULT_RESPONSE",
            Self::ToffDelay => "TOFF_DELAY",
            Self::ToffFall => "TOFF_FALL",
            Self::StatusWord => "STATUS_WORD",
            Self::StatusVout => "STATUS_VOUT",
            Self::StatusIout => "STATUS_IOUT",
            Self::StatusInput => "STATUS_INPUT",
            Self::StatusTemperature => "STATUS_TEMPERATURE",
            Self::StatusCml => "STATUS_CML",
            Self::StatusOther => "STATUS_OTHER",
            Self::StatusMfrSpecific => "STATUS_MFR_SPECIFIC",
            Self::ReadVin => "READ_VIN",
            Self::ReadVout => "READ_VOUT",
            Self::ReadIout => "READ_IOUT",
            Self::ReadTemperature1 => "READ_TEMPERATURE_1",
            Self::MfrId => "MFR_ID",
            Self::MfrModel => "MFR_MODEL",
            Self::MfrRevision => "MFR_REVISION",
            Self::IcDeviceId => "IC_DEVICE_ID",
            Self::CompensationConfig => "COMPENSATION_CONFIG",
            Self::SyncConfig => "SYNC_CONFIG",
            Self::StackConfig => "STACK_CONFIG",
            Self::MiscOptions => "MISC_OPTIONS",
            Self::PinDetectOverride => "PIN_DETECT_OVERRIDE",
            Self::SlaveAddress => "SLAVE_ADDRESS",
            Self::NvmChecksum => "NVM_CHECKSUM",
            Self::SimulateFault => "SIMULATE_FAULT",
            Self::FusionId0 => "FUSION_ID0",
            Self::FusionId1 => "FUSION_ID1",
        }
    }

    /// Convert from u8 command code to enum variant
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0x00 => Some(Self::Page),
            0x01 => Some(Self::Operation),
            0x02 => Some(Self::OnOffConfig),
            0x03 => Some(Self::ClearFaults),
            0x04 => Some(Self::Phase),
            0x19 => Some(Self::Capability),
            0x20 => Some(Self::VoutMode),
            0x21 => Some(Self::VoutCommand),
            0x24 => Some(Self::VoutMax),
            0x25 => Some(Self::VoutMarginHigh),
            0x26 => Some(Self::VoutMarginLow),
            0x29 => Some(Self::VoutScaleLoop),
            0x2B => Some(Self::VoutMin),
            0x33 => Some(Self::FrequencySwitch),
            0x35 => Some(Self::VinOn),
            0x36 => Some(Self::VinOff),
            0x37 => Some(Self::Interleave),
            0x40 => Some(Self::VoutOvFaultLimit),
            0x42 => Some(Self::VoutOvWarnLimit),
            0x43 => Some(Self::VoutUvWarnLimit),
            0x44 => Some(Self::VoutUvFaultLimit),
            0x46 => Some(Self::IoutOcFaultLimit),
            0x47 => Some(Self::IoutOcFaultResponse),
            0x4A => Some(Self::IoutOcWarnLimit),
            0x4F => Some(Self::OtFaultLimit),
            0x50 => Some(Self::OtFaultResponse),
            0x51 => Some(Self::OtWarnLimit),
            0x55 => Some(Self::VinOvFaultLimit),
            0x56 => Some(Self::VinOvFaultResponse),
            0x58 => Some(Self::VinUvWarnLimit),
            0x60 => Some(Self::TonDelay),
            0x61 => Some(Self::TonRise),
            0x62 => Some(Self::TonMaxFaultLimit),
            0x63 => Some(Self::TonMaxFaultResponse),
            0x64 => Some(Self::ToffDelay),
            0x65 => Some(Self::ToffFall),
            0x79 => Some(Self::StatusWord),
            0x7A => Some(Self::StatusVout),
            0x7B => Some(Self::StatusIout),
            0x7C => Some(Self::StatusInput),
            0x7D => Some(Self::StatusTemperature),
            0x7E => Some(Self::StatusCml),
            0x7F => Some(Self::StatusOther),
            0x80 => Some(Self::StatusMfrSpecific),
            0x88 => Some(Self::ReadVin),
            0x8B => Some(Self::ReadVout),
            0x8C => Some(Self::ReadIout),
            0x8D => Some(Self::ReadTemperature1),
            0x99 => Some(Self::MfrId),
            0x9A => Some(Self::MfrModel),
            0x9B => Some(Self::MfrRevision),
            0xAD => Some(Self::IcDeviceId),
            0xB1 => Some(Self::CompensationConfig),
            0xE4 => Some(Self::SyncConfig),
            0xEC => Some(Self::StackConfig),
            0xED => Some(Self::MiscOptions),
            0xEE => Some(Self::PinDetectOverride),
            0xEF => Some(Self::SlaveAddress),
            0xF0 => Some(Self::NvmChecksum),
            0xF1 => Some(Self::SimulateFault),
            0xFC => Some(Self::FusionId0),
            0xFD => Some(Self::FusionId1),
            _ => None,
        }
    }

    /// Convert to u8 command code
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

impl std::fmt::Display for PmbusCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

impl From<PmbusCommand> for u8 {
    fn from(cmd: PmbusCommand) -> Self {
        cmd.as_u8()
    }
}

impl std::str::FromStr for PmbusCommand {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_uppercase().as_str() {
            "PAGE" => Ok(Self::Page),
            "OPERATION" => Ok(Self::Operation),
            "ON_OFF_CONFIG" => Ok(Self::OnOffConfig),
            "CLEAR_FAULTS" => Ok(Self::ClearFaults),
            "PHASE" => Ok(Self::Phase),
            "CAPABILITY" => Ok(Self::Capability),
            "VOUT_MODE" => Ok(Self::VoutMode),
            "VOUT_COMMAND" => Ok(Self::VoutCommand),
            "VOUT_MAX" => Ok(Self::VoutMax),
            "VOUT_MARGIN_HIGH" => Ok(Self::VoutMarginHigh),
            "VOUT_MARGIN_LOW" => Ok(Self::VoutMarginLow),
            "VOUT_SCALE_LOOP" => Ok(Self::VoutScaleLoop),
            "VOUT_MIN" => Ok(Self::VoutMin),
            "FREQUENCY_SWITCH" => Ok(Self::FrequencySwitch),
            "VIN_ON" => Ok(Self::VinOn),
            "VIN_OFF" => Ok(Self::VinOff),
            "INTERLEAVE" => Ok(Self::Interleave),
            "VOUT_OV_FAULT_LIMIT" => Ok(Self::VoutOvFaultLimit),
            "VOUT_OV_WARN_LIMIT" => Ok(Self::VoutOvWarnLimit),
            "VOUT_UV_WARN_LIMIT" => Ok(Self::VoutUvWarnLimit),
            "VOUT_UV_FAULT_LIMIT" => Ok(Self::VoutUvFaultLimit),
            "IOUT_OC_FAULT_LIMIT" => Ok(Self::IoutOcFaultLimit),
            "IOUT_OC_FAULT_RESPONSE" => Ok(Self::IoutOcFaultResponse),
            "IOUT_OC_WARN_LIMIT" => Ok(Self::IoutOcWarnLimit),
            "OT_FAULT_LIMIT" => Ok(Self::OtFaultLimit),
            "OT_FAULT_RESPONSE" => Ok(Self::OtFaultResponse),
            "OT_WARN_LIMIT" => Ok(Self::OtWarnLimit),
            "VIN_OV_FAULT_LIMIT" => Ok(Self::VinOvFaultLimit),
            "VIN_OV_FAULT_RESPONSE" => Ok(Self::VinOvFaultResponse),
            "VIN_UV_WARN_LIMIT" => Ok(Self::VinUvWarnLimit),
            "TON_DELAY" => Ok(Self::TonDelay),
            "TON_RISE" => Ok(Self::TonRise),
            "TON_MAX_FAULT_LIMIT" => Ok(Self::TonMaxFaultLimit),
            "TON_MAX_FAULT_RESPONSE" => Ok(Self::TonMaxFaultResponse),
            "TOFF_DELAY" => Ok(Self::ToffDelay),
            "TOFF_FALL" => Ok(Self::ToffFall),
            "STATUS_WORD" => Ok(Self::StatusWord),
            "STATUS_VOUT" => Ok(Self::StatusVout),
            "STATUS_IOUT" => Ok(Self::StatusIout),
            "STATUS_INPUT" => Ok(Self::StatusInput),
            "STATUS_TEMPERATURE" => Ok(Self::StatusTemperature),
            "STATUS_CML" => Ok(Self::StatusCml),
            "STATUS_OTHER" => Ok(Self::StatusOther),
            "STATUS_MFR_SPECIFIC" => Ok(Self::StatusMfrSpecific),
            "READ_VIN" => Ok(Self::ReadVin),
            "READ_VOUT" => Ok(Self::ReadVout),
            "READ_IOUT" => Ok(Self::ReadIout),
            "READ_TEMPERATURE_1" => Ok(Self::ReadTemperature1),
            "MFR_ID" => Ok(Self::MfrId),
            "MFR_MODEL" => Ok(Self::MfrModel),
            "MFR_REVISION" => Ok(Self::MfrRevision),
            "IC_DEVICE_ID" => Ok(Self::IcDeviceId),
            "COMPENSATION_CONFIG" => Ok(Self::CompensationConfig),
            "SYNC_CONFIG" => Ok(Self::SyncConfig),
            "STACK_CONFIG" => Ok(Self::StackConfig),
            "MISC_OPTIONS" => Ok(Self::MiscOptions),
            "PIN_DETECT_OVERRIDE" => Ok(Self::PinDetectOverride),
            "SLAVE_ADDRESS" => Ok(Self::SlaveAddress),
            "NVM_CHECKSUM" => Ok(Self::NvmChecksum),
            "SIMULATE_FAULT" => Ok(Self::SimulateFault),
            "FUSION_ID0" => Ok(Self::FusionId0),
            "FUSION_ID1" => Ok(Self::FusionId1),
            _ => Err("Unknown PMBus command name"),
        }
    }
}

/// Get command name from u8 code (for protocol dissection compatibility)
pub fn command_name(cmd: u8) -> &'static str {
    PmbusCommand::from_u8(cmd)
        .map(|c| c.name())
        .unwrap_or("UNKNOWN")
}

/// Backward compatibility module - provides constants for migration
pub mod commands {
    use super::PmbusCommand;

    pub const PAGE: u8 = PmbusCommand::Page as u8;
    pub const OPERATION: u8 = PmbusCommand::Operation as u8;
    pub const ON_OFF_CONFIG: u8 = PmbusCommand::OnOffConfig as u8;
    pub const CLEAR_FAULTS: u8 = PmbusCommand::ClearFaults as u8;
    pub const PHASE: u8 = PmbusCommand::Phase as u8;
    pub const CAPABILITY: u8 = PmbusCommand::Capability as u8;
    pub const VOUT_MODE: u8 = PmbusCommand::VoutMode as u8;
    pub const VOUT_COMMAND: u8 = PmbusCommand::VoutCommand as u8;
    pub const VOUT_MAX: u8 = PmbusCommand::VoutMax as u8;
    pub const VOUT_MARGIN_HIGH: u8 = PmbusCommand::VoutMarginHigh as u8;
    pub const VOUT_MARGIN_LOW: u8 = PmbusCommand::VoutMarginLow as u8;
    pub const VOUT_SCALE_LOOP: u8 = PmbusCommand::VoutScaleLoop as u8;
    pub const VOUT_MIN: u8 = PmbusCommand::VoutMin as u8;
    pub const FREQUENCY_SWITCH: u8 = PmbusCommand::FrequencySwitch as u8;
    pub const VIN_ON: u8 = PmbusCommand::VinOn as u8;
    pub const VIN_OFF: u8 = PmbusCommand::VinOff as u8;
    pub const INTERLEAVE: u8 = PmbusCommand::Interleave as u8;
    pub const VOUT_OV_FAULT_LIMIT: u8 = PmbusCommand::VoutOvFaultLimit as u8;
    pub const VOUT_OV_WARN_LIMIT: u8 = PmbusCommand::VoutOvWarnLimit as u8;
    pub const VOUT_UV_WARN_LIMIT: u8 = PmbusCommand::VoutUvWarnLimit as u8;
    pub const VOUT_UV_FAULT_LIMIT: u8 = PmbusCommand::VoutUvFaultLimit as u8;
    pub const IOUT_OC_FAULT_LIMIT: u8 = PmbusCommand::IoutOcFaultLimit as u8;
    pub const IOUT_OC_FAULT_RESPONSE: u8 = PmbusCommand::IoutOcFaultResponse as u8;
    pub const IOUT_OC_WARN_LIMIT: u8 = PmbusCommand::IoutOcWarnLimit as u8;
    pub const OT_FAULT_LIMIT: u8 = PmbusCommand::OtFaultLimit as u8;
    pub const OT_FAULT_RESPONSE: u8 = PmbusCommand::OtFaultResponse as u8;
    pub const OT_WARN_LIMIT: u8 = PmbusCommand::OtWarnLimit as u8;
    pub const VIN_OV_FAULT_LIMIT: u8 = PmbusCommand::VinOvFaultLimit as u8;
    pub const VIN_OV_FAULT_RESPONSE: u8 = PmbusCommand::VinOvFaultResponse as u8;
    pub const VIN_UV_WARN_LIMIT: u8 = PmbusCommand::VinUvWarnLimit as u8;
    pub const TON_DELAY: u8 = PmbusCommand::TonDelay as u8;
    pub const TON_RISE: u8 = PmbusCommand::TonRise as u8;
    pub const TON_MAX_FAULT_LIMIT: u8 = PmbusCommand::TonMaxFaultLimit as u8;
    pub const TON_MAX_FAULT_RESPONSE: u8 = PmbusCommand::TonMaxFaultResponse as u8;
    pub const TOFF_DELAY: u8 = PmbusCommand::ToffDelay as u8;
    pub const TOFF_FALL: u8 = PmbusCommand::ToffFall as u8;
    pub const STATUS_WORD: u8 = PmbusCommand::StatusWord as u8;
    pub const STATUS_VOUT: u8 = PmbusCommand::StatusVout as u8;
    pub const STATUS_IOUT: u8 = PmbusCommand::StatusIout as u8;
    pub const STATUS_INPUT: u8 = PmbusCommand::StatusInput as u8;
    pub const STATUS_TEMPERATURE: u8 = PmbusCommand::StatusTemperature as u8;
    pub const STATUS_CML: u8 = PmbusCommand::StatusCml as u8;
    pub const STATUS_OTHER: u8 = PmbusCommand::StatusOther as u8;
    pub const STATUS_MFR_SPECIFIC: u8 = PmbusCommand::StatusMfrSpecific as u8;
    pub const READ_VIN: u8 = PmbusCommand::ReadVin as u8;
    pub const READ_VOUT: u8 = PmbusCommand::ReadVout as u8;
    pub const READ_IOUT: u8 = PmbusCommand::ReadIout as u8;
    pub const READ_TEMPERATURE_1: u8 = PmbusCommand::ReadTemperature1 as u8;
    pub const MFR_ID: u8 = PmbusCommand::MfrId as u8;
    pub const MFR_MODEL: u8 = PmbusCommand::MfrModel as u8;
    pub const MFR_REVISION: u8 = PmbusCommand::MfrRevision as u8;
    pub const IC_DEVICE_ID: u8 = PmbusCommand::IcDeviceId as u8;
    pub const COMPENSATION_CONFIG: u8 = PmbusCommand::CompensationConfig as u8;
    pub const SYNC_CONFIG: u8 = PmbusCommand::SyncConfig as u8;
    pub const STACK_CONFIG: u8 = PmbusCommand::StackConfig as u8;
    pub const MISC_OPTIONS: u8 = PmbusCommand::MiscOptions as u8;
    pub const PIN_DETECT_OVERRIDE: u8 = PmbusCommand::PinDetectOverride as u8;
    pub const SLAVE_ADDRESS: u8 = PmbusCommand::SlaveAddress as u8;
    pub const NVM_CHECKSUM: u8 = PmbusCommand::NvmChecksum as u8;
    pub const SIMULATE_FAULT: u8 = PmbusCommand::SimulateFault as u8;
    pub const FUSION_ID0: u8 = PmbusCommand::FusionId0 as u8;
    pub const FUSION_ID1: u8 = PmbusCommand::FusionId1 as u8;
}

/// STATUS_WORD bits (PMBus specification section 17.2)
pub mod status_word {
    pub const VOUT: u16 = 0x8000; // Bit 15: Output voltage fault/warning
    pub const IOUT: u16 = 0x4000; // Bit 14: Output current fault/warning
    pub const INPUT: u16 = 0x2000; // Bit 13: Input voltage fault/warning
    pub const MFR: u16 = 0x1000; // Bit 12: Manufacturer specific fault/warning
    pub const PGOOD: u16 = 0x0800; // Bit 11: Power good (not a fault)
    pub const FANS: u16 = 0x0400; // Bit 10: One or more fans fault/warning
    pub const OTHER: u16 = 0x0200; // Bit 9: Other fault/warning
    pub const UNKNOWN: u16 = 0x0100; // Bit 8: Unknown fault/warning
    pub const BUSY: u16 = 0x0080; // Bit 7: Busy - unable to respond
    pub const OFF: u16 = 0x0040; // Bit 6: Unit is off
    pub const VOUT_OV: u16 = 0x0020; // Bit 5: Output overvoltage fault
    pub const IOUT_OC: u16 = 0x0010; // Bit 4: Output overcurrent fault
    pub const VIN_UV: u16 = 0x0008; // Bit 3: Input undervoltage fault
    pub const TEMP: u16 = 0x0004; // Bit 2: Temperature fault/warning
    pub const CML: u16 = 0x0002; // Bit 1: Communication/Logic/Memory fault
    pub const NONE: u16 = 0x0001; // Bit 0: No faults (NONE_OF_THE_ABOVE)
}

/// STATUS_VOUT bits (PMBus specification section 17.7)
pub mod status_vout {
    pub const VOUT_OV_FAULT: u8 = 0x80; // Bit 7: Output overvoltage fault
    pub const VOUT_OV_WARN: u8 = 0x40; // Bit 6: Output overvoltage warning
    pub const VOUT_UV_WARN: u8 = 0x20; // Bit 5: Output undervoltage warning
    pub const VOUT_UV_FAULT: u8 = 0x10; // Bit 4: Output undervoltage fault
    pub const VOUT_MAX: u8 = 0x08; // Bit 3: VOUT at max (tracking or margin)
    pub const TON_MAX_FAULT: u8 = 0x02; // Bit 1: Unit did not power up
    pub const VOUT_MIN: u8 = 0x01; // Bit 0: VOUT at min (tracking)
}

/// STATUS_IOUT bits (PMBus specification section 17.8)
pub mod status_iout {
    pub const IOUT_OC_FAULT: u8 = 0x80; // Bit 7: Output overcurrent fault
    pub const IOUT_OC_LV_FAULT: u8 = 0x40; // Bit 6: Output OC and low voltage fault
    pub const IOUT_OC_WARN: u8 = 0x20; // Bit 5: Output overcurrent warning
    pub const IOUT_UC_FAULT: u8 = 0x10; // Bit 4: Output undercurrent fault
    pub const CURR_SHARE_FAULT: u8 = 0x08; // Bit 3: Current share fault
    pub const IN_PWR_LIM: u8 = 0x04; // Bit 2: Unit in power limiting mode
    pub const POUT_OP_FAULT: u8 = 0x02; // Bit 1: Output overpower fault
    pub const POUT_OP_WARN: u8 = 0x01; // Bit 0: Output overpower warning
}

/// STATUS_INPUT bits (PMBus specification section 17.9)
pub mod status_input {
    pub const VIN_OV_FAULT: u8 = 0x80; // Bit 7: Input overvoltage fault
    pub const VIN_OV_WARN: u8 = 0x40; // Bit 6: Input overvoltage warning
    pub const VIN_UV_WARN: u8 = 0x20; // Bit 5: Input undervoltage warning
    pub const VIN_UV_FAULT: u8 = 0x10; // Bit 4: Input undervoltage fault
    pub const UNIT_OFF_VIN_LOW: u8 = 0x08; // Bit 3: Unit off for insufficient input
    pub const IIN_OC_FAULT: u8 = 0x04; // Bit 2: Input overcurrent fault
    pub const IIN_OC_WARN: u8 = 0x02; // Bit 1: Input overcurrent warning
    pub const PIN_OP_WARN: u8 = 0x01; // Bit 0: Input overpower warning
}

/// STATUS_TEMPERATURE bits (PMBus specification section 17.10)
pub mod status_temperature {
    pub const OT_FAULT: u8 = 0x80; // Bit 7: Overtemperature fault
    pub const OT_WARN: u8 = 0x40; // Bit 6: Overtemperature warning
    pub const UT_WARN: u8 = 0x20; // Bit 5: Undertemperature warning
    pub const UT_FAULT: u8 = 0x10; // Bit 4: Undertemperature fault
}

/// STATUS_CML bits (PMBus specification section 17.11)
pub mod status_cml {
    pub const INVALID_CMD: u8 = 0x80; // Bit 7: Invalid/unsupported command
    pub const INVALID_DATA: u8 = 0x40; // Bit 6: Invalid/unsupported data
    pub const PEC_FAULT: u8 = 0x20; // Bit 5: Packet Error Check failed
    pub const MEMORY_FAULT: u8 = 0x10; // Bit 4: Memory fault detected
    pub const PROCESSOR_FAULT: u8 = 0x08; // Bit 3: Processor fault detected
    pub const OTHER_COMM_FAULT: u8 = 0x02; // Bit 1: Other communication fault
    pub const OTHER_MEM_LOGIC: u8 = 0x01; // Bit 0: Other memory or logic fault
}

/// OPERATION command values (PMBus specification section 12.1)
pub mod operation {
    pub const OFF_IMMEDIATE: u8 = 0x00; // Turn off immediately
    pub const SOFT_OFF: u8 = 0x40; // Soft off (using programmed delays)
    pub const ON_MARGIN_LOW: u8 = 0x98; // On with margin low
    pub const ON_MARGIN_HIGH: u8 = 0xA8; // On with margin high
    pub const ON: u8 = 0x80; // Turn on
}

/// ON_OFF_CONFIG bits (PMBus specification section 12.2)
pub mod on_off_config {
    pub const PU: u8 = 0x10; // Bit 4: Power-up from CONTROL pin
    pub const CMD: u8 = 0x08; // Bit 3: Respond to OPERATION command
    pub const CP: u8 = 0x04; // Bit 2: Control pin present
    pub const POLARITY: u8 = 0x02; // Bit 1: Control pin polarity (1=active high)
    pub const DELAY: u8 = 0x01; // Bit 0: Turn off delay (0=disabled)
}

/// PMBus error types
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

/// SLINEAR11 data format conversion utilities
///
/// Format: [5-bit two's complement exponent][11-bit two's complement mantissa]
/// Value = mantissa × 2^exponent
pub struct Linear11;

impl Linear11 {
    /// Convert SLINEAR11 format to floating point
    pub fn to_float(value: u16) -> f32 {
        // Extract 5-bit exponent (bits 15-11) as two's complement
        let exp_raw = ((value >> 11) & 0x1F) as i8;
        let exponent = if exp_raw & 0x10 != 0 {
            // Sign extend for negative exponent
            (exp_raw as u8 | 0xE0) as i8 as i32
        } else {
            exp_raw as i32
        };

        // Extract 11-bit mantissa (bits 10-0) as two's complement
        let mant_raw = (value & 0x7FF) as i16;
        let mantissa = if mant_raw & 0x400 != 0 {
            // Sign extend for negative mantissa
            ((mant_raw as u16 | 0xF800) as i16) as i32
        } else {
            mant_raw as i32
        };

        mantissa as f32 * 2.0_f32.powi(exponent)
    }

    /// Convert ULINEAR11 format to floating point (unsigned mantissa)
    pub fn to_float_unsigned(value: u16) -> f32 {
        // Extract 5-bit exponent (bits 15-11) as two's complement
        let exp_raw = ((value >> 11) & 0x1F) as i8;
        let exponent = if exp_raw & 0x10 != 0 {
            // Sign extend for negative exponent
            (exp_raw as u8 | 0xE0) as i8 as i32
        } else {
            exp_raw as i32
        };

        // Extract 11-bit mantissa (bits 10-0) as unsigned
        let mantissa = (value & 0x7FF) as u32;

        mantissa as f32 * 2.0_f32.powi(exponent)
    }

    /// Convert SLINEAR11 format to integer
    pub fn to_int(value: u16) -> i32 {
        Self::to_float(value) as i32
    }

    /// Convert floating point to SLINEAR11 format
    pub fn from_float(value: f32) -> u16 {
        if value == 0.0 {
            return 0;
        }

        // Find best exponent to keep mantissa in 11-bit range
        let mut best_exp = 0i8;
        let mut best_error = f32::MAX;

        // Try exponents from -16 to +15 (5-bit two's complement range)
        for exp in -16i8..=15 {
            let mantissa_f = value / 2.0_f32.powi(exp as i32);

            // Check if mantissa fits in 11-bit two's complement (-1024 to 1023)
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

        // Pack into SLINEAR11 format
        let exp_bits = (best_exp as u16) & 0x1F;
        let mant_bits = (mantissa as u16) & 0x7FF;

        (exp_bits << 11) | mant_bits
    }

    /// Convert integer to SLINEAR11 format
    pub fn from_int(value: i32) -> u16 {
        Self::from_float(value as f32)
    }
}

/// ULINEAR16 data format conversion utilities
///
/// Format: 16-bit unsigned mantissa, exponent from VOUT_MODE
/// Value = mantissa × 2^exponent
pub struct Linear16;

impl Linear16 {
    /// Convert ULINEAR16 format to floating point
    ///
    /// The exponent must be extracted from VOUT_MODE register
    pub fn to_float(value: u16, vout_mode: u8) -> f32 {
        // Extract 5-bit two's complement exponent from VOUT_MODE
        let exp_raw = (vout_mode & 0x1F) as i8;
        let exponent = if exp_raw & 0x10 != 0 {
            // Sign extend for negative exponent
            (exp_raw as u8 | 0xE0) as i8 as i32
        } else {
            exp_raw as i32
        };

        value as f32 * 2.0_f32.powi(exponent)
    }

    /// Convert floating point to ULINEAR16 format
    ///
    /// The exponent must be extracted from VOUT_MODE register
    pub fn from_float(value: f32, vout_mode: u8) -> Result<u16, PMBusError> {
        // Extract 5-bit two's complement exponent from VOUT_MODE
        let exp_raw = (vout_mode & 0x1F) as i8;
        let exponent = if exp_raw & 0x10 != 0 {
            // Sign extend for negative exponent
            (exp_raw as u8 | 0xE0) as i8 as i32
        } else {
            exp_raw as i32
        };

        let mantissa = (value / 2.0_f32.powi(exponent)).round() as u32;
        if mantissa > 0xFFFF {
            return Err(PMBusError::ValueOutOfRange);
        }

        Ok(mantissa as u16)
    }
}

/// Helper functions for decoding status registers
pub struct StatusDecoder;

impl StatusDecoder {
    /// Decode STATUS_WORD bits into human-readable descriptions
    pub fn decode_status_word(status: u16) -> Vec<&'static str> {
        let mut desc = Vec::new();
        if status & status_word::VOUT != 0 {
            desc.push("VOUT fault/warning");
        }
        if status & status_word::IOUT != 0 {
            desc.push("IOUT fault/warning");
        }
        if status & status_word::INPUT != 0 {
            desc.push("INPUT fault/warning");
        }
        if status & status_word::MFR != 0 {
            desc.push("MFR specific");
        }
        if status & status_word::PGOOD != 0 {
            desc.push("PGOOD");
        }
        if status & status_word::FANS != 0 {
            desc.push("FAN fault/warning");
        }
        if status & status_word::OTHER != 0 {
            desc.push("OTHER");
        }
        if status & status_word::UNKNOWN != 0 {
            desc.push("UNKNOWN");
        }
        if status & status_word::BUSY != 0 {
            desc.push("BUSY");
        }
        if status & status_word::OFF != 0 {
            desc.push("OFF");
        }
        if status & status_word::VOUT_OV != 0 {
            desc.push("VOUT_OV fault");
        }
        if status & status_word::IOUT_OC != 0 {
            desc.push("IOUT_OC fault");
        }
        if status & status_word::VIN_UV != 0 {
            desc.push("VIN_UV fault");
        }
        if status & status_word::TEMP != 0 {
            desc.push("TEMP fault/warning");
        }
        if status & status_word::CML != 0 {
            desc.push("CML fault");
        }
        if status & status_word::NONE != 0 && desc.is_empty() {
            desc.push("NONE_OF_THE_ABOVE");
        }
        desc
    }

    /// Decode STATUS_VOUT bits into human-readable descriptions
    pub fn decode_status_vout(status: u8) -> Vec<&'static str> {
        let mut desc = Vec::new();
        if status & status_vout::VOUT_OV_FAULT != 0 {
            desc.push("OV fault");
        }
        if status & status_vout::VOUT_OV_WARN != 0 {
            desc.push("OV warning");
        }
        if status & status_vout::VOUT_UV_WARN != 0 {
            desc.push("UV warning");
        }
        if status & status_vout::VOUT_UV_FAULT != 0 {
            desc.push("UV fault");
        }
        if status & status_vout::VOUT_MAX != 0 {
            desc.push("at MAX");
        }
        if status & status_vout::TON_MAX_FAULT != 0 {
            desc.push("failed to start");
        }
        if status & status_vout::VOUT_MIN != 0 {
            desc.push("at MIN");
        }
        desc
    }

    /// Decode STATUS_IOUT bits into human-readable descriptions
    pub fn decode_status_iout(status: u8) -> Vec<&'static str> {
        let mut desc = Vec::new();
        if status & status_iout::IOUT_OC_FAULT != 0 {
            desc.push("OC fault");
        }
        if status & status_iout::IOUT_OC_LV_FAULT != 0 {
            desc.push("OC+LV fault");
        }
        if status & status_iout::IOUT_OC_WARN != 0 {
            desc.push("OC warning");
        }
        if status & status_iout::IOUT_UC_FAULT != 0 {
            desc.push("UC fault");
        }
        if status & status_iout::CURR_SHARE_FAULT != 0 {
            desc.push("current share fault");
        }
        if status & status_iout::IN_PWR_LIM != 0 {
            desc.push("power limiting");
        }
        if status & status_iout::POUT_OP_FAULT != 0 {
            desc.push("overpower fault");
        }
        if status & status_iout::POUT_OP_WARN != 0 {
            desc.push("overpower warning");
        }
        desc
    }

    /// Decode STATUS_INPUT bits into human-readable descriptions
    pub fn decode_status_input(status: u8) -> Vec<&'static str> {
        let mut desc = Vec::new();
        if status & status_input::VIN_OV_FAULT != 0 {
            desc.push("VIN OV fault");
        }
        if status & status_input::VIN_OV_WARN != 0 {
            desc.push("VIN OV warning");
        }
        if status & status_input::VIN_UV_WARN != 0 {
            desc.push("VIN UV warning");
        }
        if status & status_input::VIN_UV_FAULT != 0 {
            desc.push("VIN UV fault");
        }
        if status & status_input::UNIT_OFF_VIN_LOW != 0 {
            desc.push("off due to low VIN");
        }
        if status & status_input::IIN_OC_FAULT != 0 {
            desc.push("IIN OC fault");
        }
        if status & status_input::IIN_OC_WARN != 0 {
            desc.push("IIN OC warning");
        }
        if status & status_input::PIN_OP_WARN != 0 {
            desc.push("input overpower warning");
        }
        desc
    }

    /// Decode STATUS_TEMPERATURE bits into human-readable descriptions
    pub fn decode_status_temp(status: u8) -> Vec<&'static str> {
        let mut desc = Vec::new();
        if status & status_temperature::OT_FAULT != 0 {
            desc.push("overtemp fault");
        }
        if status & status_temperature::OT_WARN != 0 {
            desc.push("overtemp warning");
        }
        if status & status_temperature::UT_WARN != 0 {
            desc.push("undertemp warning");
        }
        if status & status_temperature::UT_FAULT != 0 {
            desc.push("undertemp fault");
        }
        desc
    }

    /// Decode STATUS_CML bits into human-readable descriptions
    pub fn decode_status_cml(status: u8) -> Vec<&'static str> {
        let mut desc = Vec::new();
        if status & status_cml::INVALID_CMD != 0 {
            desc.push("invalid command");
        }
        if status & status_cml::INVALID_DATA != 0 {
            desc.push("invalid data");
        }
        if status & status_cml::PEC_FAULT != 0 {
            desc.push("PEC error");
        }
        if status & status_cml::MEMORY_FAULT != 0 {
            desc.push("memory fault");
        }
        if status & status_cml::PROCESSOR_FAULT != 0 {
            desc.push("processor fault");
        }
        if status & status_cml::OTHER_COMM_FAULT != 0 {
            desc.push("other comm fault");
        }
        if status & status_cml::OTHER_MEM_LOGIC != 0 {
            desc.push("other mem/logic fault");
        }
        desc
    }

    /// Decode fault response byte into human-readable description
    ///
    /// Fault response format:
    /// - Bits 7-5: Response type
    /// - Bits 4-3: Number of retries
    /// - Bits 2-0: Retry delay time
    pub fn decode_fault_response(response: u8) -> String {
        let response_type = (response >> 5) & 0x07;
        let retry_count = (response >> 3) & 0x03;
        let delay_time = response & 0x07;

        let response_desc = match response_type {
            0b000 => "ignore fault",
            0b001 => "shutdown, retry indefinitely",
            0b010 => "shutdown, no retry",
            0b011 => "shutdown with retries",
            0b100 => "continue, retry indefinitely",
            0b101 => "continue, no retry",
            0b110 => "continue with retries",
            0b111 => "shutdown with delay and retries",
            _ => "unknown",
        };

        let retries_desc = match retry_count {
            0b00 => "no retries",
            0b01 => "1 retry",
            0b10 => "2 retries",
            0b11 => match response_type {
                0b001 | 0b100 => "infinite retries",
                _ => "3 retries",
            },
            _ => "unknown",
        };

        let delay_desc = match delay_time {
            0b000 => "0ms",
            0b001 => "22.7ms",
            0b010 => "45.4ms",
            0b011 => "91ms",
            0b100 => "182ms",
            0b101 => "364ms",
            0b110 => "728ms",
            0b111 => "1456ms",
            _ => "unknown",
        };

        // Special cases for common values
        match response {
            0x00 => "ignore fault".to_string(),
            0xC0 => "shutdown immediately, no retries".to_string(),
            0xFF => "infinite retries, wait for recovery".to_string(),
            _ => {
                if retry_count == 0 || response_type == 0b010 || response_type == 0b101 {
                    response_desc.to_string()
                } else {
                    format!("{}, {}, {} delay", response_desc, retries_desc, delay_desc)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_linear11_conversion() {
        // Test zero
        assert_eq!(Linear11::to_float(0), 0.0);
        assert_eq!(Linear11::from_float(0.0), 0);

        // Test positive values
        let value = 650.0; // Typical frequency value
        let encoded = Linear11::from_float(value);
        let decoded = Linear11::to_float(encoded);
        assert!((decoded - value).abs() < 1.0); // Allow small rounding error

        // Test negative values
        let value = -10.5;
        let encoded = Linear11::from_float(value);
        let decoded = Linear11::to_float(encoded);
        assert!((decoded - value).abs() < 0.1);
    }

    #[test]
    fn test_linear11_common_voltage_values() {
        // Common voltage values in power management
        let test_cases = [
            (1.2, "BM1370 core voltage"),
            (3.3, "IO voltage"),
            (5.0, "USB voltage"),
            (12.0, "Input voltage"),
            (48.0, "Telecom voltage"),
        ];

        for (voltage, description) in test_cases {
            let encoded = Linear11::from_float(voltage);
            let decoded = Linear11::to_float(encoded);
            assert!(
                (decoded - voltage).abs() < 0.01,
                "{} conversion failed: expected {}, got {}",
                description,
                voltage,
                decoded
            );
        }
    }

    #[test]
    fn test_linear11_temperature_values() {
        // Common temperature values
        let temperatures = [
            -40.0, // Minimum operating temperature
            0.0,   // Freezing point
            25.0,  // Room temperature
            85.0,  // Maximum commercial temperature
            105.0, // Warning threshold
            125.0, // Maximum junction temperature
        ];

        for temp in temperatures {
            let encoded = Linear11::from_float(temp);
            let decoded = Linear11::to_float(encoded);
            assert!(
                (decoded - temp).abs() < 0.1,
                "Temperature {} conversion failed: got {}",
                temp,
                decoded
            );
        }
    }

    #[test]
    fn test_linear11_current_values() {
        // Common current values for mining hardware
        let currents = [
            0.5,   // Low current
            10.0,  // Moderate current
            25.0,  // Bitaxe warning threshold
            30.0,  // Bitaxe fault threshold
            100.0, // High-power ASIC current
        ];

        for current in currents {
            let encoded = Linear11::from_float(current);
            let decoded = Linear11::to_float(encoded);
            // Current measurements can tolerate slightly more error
            assert!(
                (decoded - current).abs() < 0.1,
                "Current {} conversion failed: got {}",
                current,
                decoded
            );
        }
    }

    #[test]
    fn test_linear11_edge_cases() {
        // Maximum positive mantissa with maximum exponent
        // 1023 * 2^15 = 33521664
        let max_positive = 1023.0 * (1 << 15) as f32;
        let encoded = Linear11::from_float(max_positive);
        let decoded = Linear11::to_float(encoded);
        // Large values will have more rounding error
        assert!((decoded - max_positive).abs() / max_positive < 0.001);

        // Maximum negative mantissa with minimum exponent
        // -1024 * 2^-16 = -0.015625
        let min_negative = -1024.0 / (1 << 16) as f32;
        let encoded = Linear11::from_float(min_negative);
        let decoded = Linear11::to_float(encoded);
        assert!((decoded - min_negative).abs() < 0.0001);

        // Test very small positive value
        let tiny = 0.001;
        let encoded = Linear11::from_float(tiny);
        let decoded = Linear11::to_float(encoded);
        assert!((decoded - tiny).abs() < 0.0001);
    }

    #[test]
    fn test_linear11_round_trip() {
        // Test that multiple encode/decode cycles are stable
        let values = [1.234, -5.678, 100.0, 0.01];

        for original in values {
            let mut value = original;
            for _ in 0..3 {
                let encoded = Linear11::from_float(value);
                value = Linear11::to_float(encoded);
            }
            // After multiple round trips, should still be close to original
            assert!(
                (value - original).abs() < 0.01,
                "Round trip failed for {}: got {}",
                original,
                value
            );
        }
    }

    #[test]
    fn test_linear16_conversion() {
        let vout_mode = 0x17; // Example exponent (-9)

        // Test typical voltage value
        let value = 1.15; // Typical VOUT value
        let encoded = Linear16::from_float(value, vout_mode).unwrap();
        let decoded = Linear16::to_float(encoded, vout_mode);
        assert!((decoded - value).abs() < 0.01);
    }

    #[test]
    fn test_linear16_different_exponents() {
        // Test different VOUT_MODE exponents commonly used
        let test_cases = [
            (0x17, 1.15, "millivolt precision (-9)"), // 2^-9 = ~0.00195V resolution
            (0x14, 1.15, "higher precision (-12)"),   // 2^-12 = ~0.000244V resolution
            (0x00, 12.0, "volt precision (0)"),       // 2^0 = 1V resolution
            (0x1B, 0.9, "microvolt precision (-5)"),  // 2^-5 = ~0.03125V resolution
        ];

        for (vout_mode, voltage, description) in test_cases {
            match Linear16::from_float(voltage, vout_mode) {
                Ok(encoded) => {
                    let decoded = Linear16::to_float(encoded, vout_mode);
                    // Tolerance is one LSB of the format
                    let exp_raw = (vout_mode & 0x1F) as i8;
                    let exponent = if exp_raw & 0x10 != 0 {
                        (exp_raw as u8 | 0xE0) as i8 as i32
                    } else {
                        exp_raw as i32
                    };
                    let tolerance = 2.0_f32.powi(exponent);
                    assert!(
                        (decoded - voltage).abs() <= tolerance,
                        "{} failed: expected {}, got {}, tolerance {}",
                        description,
                        voltage,
                        decoded,
                        tolerance
                    );
                }
                Err(_) => panic!("{} encoding failed for value {}", description, voltage),
            }
        }
    }

    #[test]
    fn test_linear16_boundary_values() {
        let vout_mode = 0x17; // -9 exponent

        // Test maximum mantissa value
        let max_decoded = Linear16::to_float(0xFFFF, vout_mode);
        assert_eq!(max_decoded, 65535.0 * 2.0_f32.powi(-9));

        // Test minimum non-zero value
        let min_decoded = Linear16::to_float(0x0001, vout_mode);
        assert_eq!(min_decoded, 1.0 * 2.0_f32.powi(-9));

        // Test zero
        let zero_decoded = Linear16::to_float(0x0000, vout_mode);
        assert_eq!(zero_decoded, 0.0);
    }

    #[test]
    fn test_linear16_overflow_detection() {
        // With exponent 0, maximum representable value is 65535
        let vout_mode = 0x00;

        // This should succeed
        assert!(Linear16::from_float(65535.0, vout_mode).is_ok());

        // This should fail - value too large
        assert!(Linear16::from_float(65536.0, vout_mode).is_err());

        // With positive exponent, smaller values can be represented with less precision
        let vout_mode_pos = 0x05; // +5 exponent, larger resolution steps
                                  // Value 1000 with exponent +5: mantissa = 1000 / 32 = 31.25
        assert!(Linear16::from_float(1000.0, vout_mode_pos).is_ok());
    }

    #[test]
    fn test_status_decoder() {
        // Test STATUS_WORD decoding
        let status = status_word::VOUT | status_word::TEMP;
        let desc = StatusDecoder::decode_status_word(status);
        assert!(desc.contains(&"VOUT fault/warning"));
        assert!(desc.contains(&"TEMP fault/warning"));

        // Test fault response decoding
        assert_eq!(
            StatusDecoder::decode_fault_response(0xC0),
            "shutdown immediately, no retries"
        );
        assert_eq!(
            StatusDecoder::decode_fault_response(0xFF),
            "infinite retries, wait for recovery"
        );
    }

    #[test]
    fn test_fault_response_combinations() {
        // Test various fault response byte combinations
        // Format: [response_type:3][retry_count:2][delay:3]

        // Test all delay times with shutdown and 1 retry
        // 0x58 = 0101 1000 = response_type=010 (shutdown no retry), retry=11, delay=000
        // For shutdown with retries and 1 retry, we need 0x68-0x6F
        let delays = [
            (0x68, "shutdown with retries, 1 retry, 0ms delay"),
            (0x69, "shutdown with retries, 1 retry, 22.7ms delay"),
            (0x6A, "shutdown with retries, 1 retry, 45.4ms delay"),
            (0x6B, "shutdown with retries, 1 retry, 91ms delay"),
            (0x6C, "shutdown with retries, 1 retry, 182ms delay"),
            (0x6D, "shutdown with retries, 1 retry, 364ms delay"),
            (0x6E, "shutdown with retries, 1 retry, 728ms delay"),
            (0x6F, "shutdown with retries, 1 retry, 1456ms delay"),
        ];

        for (response, expected) in delays {
            assert_eq!(
                StatusDecoder::decode_fault_response(response),
                expected,
                "Failed for response byte 0x{:02X}",
                response
            );
        }

        // Test different retry counts
        // 0x73 = 0111 0011 = type=011 (shutdown with retries), retry=10 (2), delay=011 (91ms)
        assert_eq!(
            StatusDecoder::decode_fault_response(0x73),
            "shutdown with retries, 2 retries, 91ms delay"
        );
        // 0x7B = 0111 1011 = type=011 (shutdown with retries), retry=11 (3), delay=011 (91ms)
        assert_eq!(
            StatusDecoder::decode_fault_response(0x7B),
            "shutdown with retries, 3 retries, 91ms delay"
        );

        // Test continue mode
        assert_eq!(
            StatusDecoder::decode_fault_response(0xA0),
            "continue, no retry"
        );
        assert_eq!(
            StatusDecoder::decode_fault_response(0xCB),
            "continue with retries, 1 retry, 91ms delay"
        );
    }

    #[test]
    fn test_status_word_combinations() {
        // Test no faults - should include NONE
        let no_faults = status_word::NONE;
        let desc = StatusDecoder::decode_status_word(no_faults);
        assert_eq!(desc.len(), 1);
        assert!(desc.contains(&"NONE_OF_THE_ABOVE"));

        // Test multiple simultaneous faults
        let multi_fault = status_word::VOUT_OV | status_word::IOUT_OC | status_word::TEMP;
        let desc = StatusDecoder::decode_status_word(multi_fault);
        assert!(desc.contains(&"VOUT_OV fault"));
        assert!(desc.contains(&"IOUT_OC fault"));
        assert!(desc.contains(&"TEMP fault/warning"));
        assert_eq!(desc.len(), 3);

        // Test all voltage-related faults
        let voltage_faults = status_word::VOUT | status_word::INPUT | status_word::VIN_UV;
        let desc = StatusDecoder::decode_status_word(voltage_faults);
        assert!(desc.contains(&"VOUT fault/warning"));
        assert!(desc.contains(&"INPUT fault/warning"));
        assert!(desc.contains(&"VIN_UV fault"));

        // Test status bits (not faults)
        let status_bits = status_word::PGOOD | status_word::OFF | status_word::BUSY;
        let desc = StatusDecoder::decode_status_word(status_bits);
        assert!(desc.contains(&"PGOOD"));
        assert!(desc.contains(&"OFF"));
        assert!(desc.contains(&"BUSY"));
    }

    #[test]
    fn test_status_vout_all_conditions() {
        // Test each STATUS_VOUT bit individually
        let conditions = [
            (status_vout::VOUT_OV_FAULT, "OV fault"),
            (status_vout::VOUT_OV_WARN, "OV warning"),
            (status_vout::VOUT_UV_WARN, "UV warning"),
            (status_vout::VOUT_UV_FAULT, "UV fault"),
            (status_vout::VOUT_MAX, "at MAX"),
            (status_vout::TON_MAX_FAULT, "failed to start"),
            (status_vout::VOUT_MIN, "at MIN"),
        ];

        for (bit, expected) in conditions {
            let desc = StatusDecoder::decode_status_vout(bit);
            assert_eq!(desc.len(), 1);
            assert_eq!(desc[0], expected);
        }

        // Test combination of overvoltage and undervoltage
        let ov_uv = status_vout::VOUT_OV_WARN | status_vout::VOUT_UV_WARN;
        let desc = StatusDecoder::decode_status_vout(ov_uv);
        assert_eq!(desc.len(), 2);
        assert!(desc.contains(&"OV warning"));
        assert!(desc.contains(&"UV warning"));
    }

    #[test]
    fn test_status_input_conditions() {
        // Test undervoltage + overvoltage combination (shouldn't happen but test anyway)
        let uv_ov = status_input::VIN_UV_FAULT | status_input::VIN_OV_FAULT;
        let desc = StatusDecoder::decode_status_input(uv_ov);
        assert!(desc.contains(&"VIN OV fault"));
        assert!(desc.contains(&"VIN UV fault"));

        // Test unit off due to low input
        let off_low = status_input::UNIT_OFF_VIN_LOW | status_input::VIN_UV_FAULT;
        let desc = StatusDecoder::decode_status_input(off_low);
        assert!(desc.contains(&"off due to low VIN"));
        assert!(desc.contains(&"VIN UV fault"));
    }

    #[test]
    fn test_status_temperature_all_bits() {
        // Test all temperature warning/fault combinations
        let all_temp = status_temperature::OT_FAULT
            | status_temperature::OT_WARN
            | status_temperature::UT_WARN
            | status_temperature::UT_FAULT;
        let desc = StatusDecoder::decode_status_temp(all_temp);
        assert_eq!(desc.len(), 4);
        assert!(desc.contains(&"overtemp fault"));
        assert!(desc.contains(&"overtemp warning"));
        assert!(desc.contains(&"undertemp warning"));
        assert!(desc.contains(&"undertemp fault"));
    }

    #[test]
    fn test_linear11_voltage_bug_case() {
        // Test the specific case that was incorrectly decoded
        // Raw value [85, c5] = 0xc585 should decode to ~5.520V, not negative
        let raw_value = 0xc585;

        // Test signed decoder (old buggy behavior would give negative)
        let signed_result = Linear11::to_float(raw_value);

        // Test unsigned decoder (correct for voltages)
        let unsigned_result = Linear11::to_float_unsigned(raw_value);

        // The unsigned decoder should give the correct positive voltage
        assert!(
            unsigned_result > 5.0 && unsigned_result < 6.0,
            "Expected voltage ~5.5V, got {}V",
            unsigned_result
        );

        // More precise check
        assert!(
            (unsigned_result - 5.520).abs() < 0.001,
            "Expected 5.520V, got {:.3}V",
            unsigned_result
        );

        // Document that signed decoder gives wrong result for this case
        // (This is expected since voltage readings should use unsigned mantissa)
        println!(
            "Signed decoder result: {:.3}V (incorrect for voltage)",
            signed_result
        );
        println!(
            "Unsigned decoder result: {:.3}V (correct for voltage)",
            unsigned_result
        );
    }

    #[test]
    fn test_linear11_mantissa_sign_boundary() {
        // Test mantissa values around the sign bit boundary (bit 10 = 0x400)
        let test_cases = [
            (0x03FF, "Just below sign bit"), // Mantissa 1023, should be positive
            (0x0400, "At sign bit"),         // Mantissa 1024, should be positive
            (0x0401, "Just above sign bit"), // Mantissa 1025, should be positive
            (0x07FF, "Maximum mantissa"),    // Mantissa 2047, should be positive
        ];

        for (mantissa, description) in test_cases {
            // Create Linear11 value with exponent 0 and test mantissa
            let linear11_value = mantissa; // Exponent bits 15-11 = 0

            let _signed_result = Linear11::to_float(linear11_value);
            let unsigned_result = Linear11::to_float_unsigned(linear11_value);

            // For voltage readings, unsigned should always be positive
            assert!(
                unsigned_result >= 0.0,
                "{}: unsigned decoder should give positive result, got {}",
                description,
                unsigned_result
            );

            // The unsigned result should equal the mantissa value (since exp=0)
            assert!(
                (unsigned_result - mantissa as f32).abs() < 0.001,
                "{}: expected {}, got {}",
                description,
                mantissa,
                unsigned_result
            );
        }
    }

    #[test]
    fn test_linear11_voltage_edge_cases() {
        // Test edge cases that could cause voltage decoding issues
        let test_cases = [
            (0x0000, 0.0, "Zero voltage"),
            (0x0001, 1.0, "Minimum positive voltage"),
            (0xF800, 0.0, "Zero with negative exponent"),
            (0xC001, 1.0 / 256.0, "Small voltage with negative exponent"),
            (0x07FF, 2047.0, "Maximum mantissa with exp=0"),
            (
                0x8001,
                1.0 / 32768.0,
                "Small value with max negative exponent",
            ),
        ];

        for (raw_value, expected_unsigned, description) in test_cases {
            let unsigned_result = Linear11::to_float_unsigned(raw_value);

            // Check that unsigned decoder gives expected positive results
            assert!(
                (unsigned_result - expected_unsigned).abs() < 0.001,
                "{}: expected {:.3}V, got {:.3}V",
                description,
                expected_unsigned,
                unsigned_result
            );
        }
    }

    #[test]
    fn test_linear11_current_can_be_negative() {
        // Current readings can legitimately be negative (reverse current flow)
        // So current decoders should use the signed version

        // Test a value that would be positive unsigned but negative signed
        let raw_value = 0x0500; // Mantissa with bit 10 set, exponent 0

        let signed_result = Linear11::to_float(raw_value);
        let unsigned_result = Linear11::to_float_unsigned(raw_value);

        // Signed should handle negative current correctly
        assert!(
            signed_result < 0.0,
            "Signed decoder should handle negative current"
        );

        // Unsigned should always be positive
        assert!(
            unsigned_result > 0.0,
            "Unsigned decoder should always be positive"
        );

        // They should have different signs for this test case
        assert!(
            signed_result * unsigned_result < 0.0,
            "Signed and unsigned should have opposite signs for this test case"
        );
    }
}
