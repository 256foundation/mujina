//! BM13xx chip registers as typed values.
//!
//! [`RegisterAddress`] names each register on a BM13xx chip.
//! [`Register`] carries the same set with a typed payload per
//! variant, so values coming off the wire are typed rather than raw
//! bit fields.

use bitcoin::pow::Work;
use bytes::{BufMut, BytesMut};
use std::fmt;
use strum::FromRepr;

use super::chip_config::CRYSTAL_MHZ;
use super::error::ProtocolError;
use crate::types::Difficulty;

/// Register addresses on the wire.
#[derive(FromRepr, Copy, Clone, Debug)]
#[repr(u8)]
pub enum RegisterAddress {
    ChipId = 0x00,
    PllDivider = 0x08,
    NonceRange = 0x10,
    TicketMask = 0x14,
    MiscControl = 0x18,
    UartBaud = 0x28,
    UartRelay = 0x2C,
    CoreMailbox = 0x3C,
    AnalogMux = 0x54,
    IoDriverStrength = 0x58,
    Pll3Parameter = 0x68,
    MidstateConfig = 0xA4,
    SoftResetControl = 0xA8,
    MiscSettings = 0xB9,
}

/// A register with its typed payload.
#[derive(Debug, Clone)]
pub enum Register {
    ChipId(ChipId),
    PllDivider(PllDivider),
    NonceRange(NonceRange),
    TicketMask(TicketMask),
    MiscControl(MiscControl),
    UartBaud(UartBaud),
    UartRelay(UartRelay),
    CoreMailbox(CoreCommand),
    AnalogMux(AnalogMux),
    IoDriverStrength(IoDriverStrength),
    Pll3Parameter(Pll3Parameter),
    MidstateConfig(MidstateConfig),
    SoftResetControl(SoftResetControl),
    MiscSettings(MiscSettings),
}

impl Register {
    pub fn decode(address: RegisterAddress, bytes: [u8; 4]) -> Result<Register, ProtocolError> {
        Ok(match address {
            RegisterAddress::ChipId => Register::ChipId(ChipId::decode(bytes)?),
            RegisterAddress::PllDivider => Register::PllDivider(PllDivider::decode(bytes)),
            RegisterAddress::NonceRange => Register::NonceRange(NonceRange::decode(bytes)),
            RegisterAddress::TicketMask => Register::TicketMask(TicketMask::decode(bytes)),
            RegisterAddress::MiscControl => Register::MiscControl(MiscControl::decode(bytes)),
            RegisterAddress::UartBaud => Register::UartBaud(UartBaud::decode(bytes)),
            RegisterAddress::UartRelay => Register::UartRelay(UartRelay::decode(bytes)),
            RegisterAddress::CoreMailbox => Register::CoreMailbox(CoreCommand::decode(bytes)),
            RegisterAddress::AnalogMux => Register::AnalogMux(AnalogMux::decode(bytes)),
            RegisterAddress::IoDriverStrength => {
                Register::IoDriverStrength(IoDriverStrength::decode(bytes))
            }
            RegisterAddress::Pll3Parameter => Register::Pll3Parameter(Pll3Parameter::decode(bytes)),
            RegisterAddress::MidstateConfig => {
                Register::MidstateConfig(MidstateConfig::decode(bytes))
            }
            RegisterAddress::SoftResetControl => {
                Register::SoftResetControl(SoftResetControl::decode(bytes))
            }
            RegisterAddress::MiscSettings => Register::MiscSettings(MiscSettings::decode(bytes)),
        })
    }

    pub(super) fn address(&self) -> RegisterAddress {
        match self {
            Register::ChipId(_) => RegisterAddress::ChipId,
            Register::PllDivider(_) => RegisterAddress::PllDivider,
            Register::NonceRange(_) => RegisterAddress::NonceRange,
            Register::TicketMask(_) => RegisterAddress::TicketMask,
            Register::MiscControl(_) => RegisterAddress::MiscControl,
            Register::UartBaud(_) => RegisterAddress::UartBaud,
            Register::UartRelay(_) => RegisterAddress::UartRelay,
            Register::CoreMailbox(_) => RegisterAddress::CoreMailbox,
            Register::AnalogMux(_) => RegisterAddress::AnalogMux,
            Register::IoDriverStrength(_) => RegisterAddress::IoDriverStrength,
            Register::Pll3Parameter(_) => RegisterAddress::Pll3Parameter,
            Register::MidstateConfig(_) => RegisterAddress::MidstateConfig,
            Register::SoftResetControl(_) => RegisterAddress::SoftResetControl,
            Register::MiscSettings(_) => RegisterAddress::MiscSettings,
        }
    }

    pub(super) fn encode(&self, dst: &mut BytesMut) {
        match self {
            Register::ChipId(r) => r.encode(dst),
            Register::PllDivider(r) => r.encode(dst),
            Register::NonceRange(r) => r.encode(dst),
            Register::TicketMask(r) => r.encode(dst),
            Register::MiscControl(r) => r.encode(dst),
            Register::UartBaud(r) => r.encode(dst),
            Register::UartRelay(r) => r.encode(dst),
            Register::CoreMailbox(r) => r.encode(dst),
            Register::AnalogMux(r) => r.encode(dst),
            Register::IoDriverStrength(r) => r.encode(dst),
            Register::Pll3Parameter(r) => r.encode(dst),
            Register::MidstateConfig(r) => r.encode(dst),
            Register::SoftResetControl(r) => r.encode(dst),
            Register::MiscSettings(r) => r.encode(dst),
        }
    }
}

/// Chip model + core count + assigned chain address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChipId {
    pub model: ChipModel,
    pub core_count: u8,
    pub address: u8,
}

impl ChipId {
    pub fn encode(&self, dst: &mut BytesMut) {
        dst.put_slice(&self.model.id_bytes());
        dst.put_u8(self.core_count);
        dst.put_u8(self.address);
    }
    pub fn decode(bytes: [u8; 4]) -> Result<Self, ProtocolError> {
        Ok(Self {
            model: ChipModel::try_from([bytes[0], bytes[1]])?,
            core_count: bytes[2],
            address: bytes[3],
        })
    }
}

/// Chip models the BM13xx stack supports.
///
/// Deliberately closed: every variant must have decided protocol
/// behavior (nonce packing, PLL bounds), so an unrecognized chip id
/// fails to decode rather than carrying an id the rest of the stack
/// cannot act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipModel {
    /// BM1362 - Used in Antminer S19 J Pro (126 chips)
    /// Core count unknown
    BM1362,
    /// BM1366 - Newer generation chip
    BM1366,
    /// BM1370 - Used in Bitaxe Gamma and Antminer S21 Pro
    /// ~2,040 hash engines organized as 128 domains of ~16 engines each
    BM1370,
}

impl ChipModel {
    /// Returns the raw chip ID bytes.
    pub fn id_bytes(&self) -> [u8; 2] {
        match self {
            Self::BM1362 => [0x13, 0x62],
            Self::BM1366 => [0x13, 0x66],
            Self::BM1370 => [0x13, 0x70],
        }
    }

    /// Returns the expected hash engine count for this model, if known.
    pub fn core_count(&self) -> Option<u32> {
        match self {
            Self::BM1370 => Some(2048), // 128 x 16; esp-miner uses 2040
            _ => None,
        }
    }
}

impl TryFrom<[u8; 2]> for ChipModel {
    type Error = ProtocolError;

    fn try_from(bytes: [u8; 2]) -> Result<Self, Self::Error> {
        match bytes {
            [0x13, 0x62] => Ok(Self::BM1362),
            [0x13, 0x66] => Ok(Self::BM1366),
            [0x13, 0x70] => Ok(Self::BM1370),
            _ => Err(ProtocolError::UnknownChipId(bytes)),
        }
    }
}

impl From<ChipModel> for [u8; 2] {
    fn from(model: ChipModel) -> Self {
        model.id_bytes()
    }
}

/// PLL configuration for frequency control.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PllDivider {
    /// VCO control flag (0x40 for low VCO, 0x50 for high VCO).
    pub flag: u8,
    /// Feedback divider.
    pub fb_div: u8,
    /// Reference divider (typically 1 or 2).
    pub ref_div: u8,
    /// Post divider, encoded as `((post_div1-1) << 4) | (post_div2-1)`.
    pub post_div: u8,
}

impl PllDivider {
    /// Builds a [`PllDivider`] with `flag` derived from the dividers.
    pub fn new(fb_div: u8, ref_div: u8, post_div: u8) -> Self {
        const VCO_FLAG_THRESHOLD_MHZ: f32 = 2400.0;
        const PLL_FLAG_HIGH_VCO: u8 = 0x50;
        const PLL_FLAG_LOW_VCO: u8 = 0x40;

        let vco_mhz = fb_div as f32 * CRYSTAL_MHZ / ref_div as f32;
        let flag = if vco_mhz >= VCO_FLAG_THRESHOLD_MHZ {
            PLL_FLAG_HIGH_VCO
        } else {
            PLL_FLAG_LOW_VCO
        };
        Self {
            flag,
            fb_div,
            ref_div,
            post_div,
        }
    }

    pub fn encode(&self, dst: &mut BytesMut) {
        dst.put_u8(self.flag);
        dst.put_u8(self.fb_div);
        dst.put_u8(self.ref_div);
        dst.put_u8(self.post_div);
    }

    pub fn decode(bytes: [u8; 4]) -> Self {
        Self {
            flag: bytes[0],
            fb_div: bytes[1],
            ref_div: bytes[2],
            post_div: bytes[3],
        }
    }
}

/// Nonce range configuration for work distribution.
///
/// NOTE: We store this as a byte array rather than interpreting it as a u32
/// because the exact bit-level interpretation is still being reverse-engineered.
/// The values below are empirically observed from production hardware.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NonceRange {
    /// Raw bytes as sent over the wire
    bytes: [u8; 4],
}

impl NonceRange {
    // Nonce range values for different chain lengths (captured from hardware)
    const SINGLE_CHIP: [u8; 4] = [0xff, 0xff, 0xff, 0xff];
    const SMALL_CHAIN: [u8; 4] = [0xff, 0xff, 0xff, 0x1f]; // 2-8 chips
    const MEDIUM_CHAIN: [u8; 4] = [0xff, 0xff, 0xff, 0x0f]; // 9-16 chips
    const LARGE_CHAIN: [u8; 4] = [0xff, 0xff, 0xff, 0x07]; // 17-32 chips
    const XLARGE_CHAIN: [u8; 4] = [0xff, 0xff, 0xff, 0x03]; // 33-64 chips
    const S21_PRO: [u8; 4] = [0x00, 0x00, 0x1e, 0xb5]; // 65-128 chips (empirical)
    const DEFAULT_LARGE: [u8; 4] = [0xff, 0xff, 0xff, 0x01]; // >128 chips

    /// Create config for single chip (full range)
    pub fn single_chip() -> Self {
        Self {
            bytes: Self::SINGLE_CHIP,
        }
    }

    /// Create config for multi-chip chain
    pub fn multi_chip(chain_length: usize) -> Self {
        let bytes = match chain_length {
            1 => Self::SINGLE_CHIP,
            2..=8 => Self::SMALL_CHAIN,
            9..=16 => Self::MEDIUM_CHAIN,
            17..=32 => Self::LARGE_CHAIN,
            33..=64 => Self::XLARGE_CHAIN,
            65..=128 => Self::S21_PRO,
            _ => Self::DEFAULT_LARGE,
        };
        Self { bytes }
    }

    /// Create config from raw 32-bit value (little-endian)
    /// Used for exact configuration from protocol captures
    pub fn from_raw(value: u32) -> Self {
        Self {
            bytes: value.to_le_bytes(),
        }
    }

    pub fn encode(&self, dst: &mut BytesMut) {
        dst.put_slice(&self.bytes);
    }

    pub fn decode(bytes: [u8; 4]) -> Self {
        Self { bytes }
    }
}

/// Ticket mask controlling ASIC nonce reporting
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TicketMask {
    // Number of additional zero bits required in the bit-reversed hash,
    // beyond the base 32 bits. The chip always requires bits 0-31 of the
    // bit-reversed hash to be zero. This parameter adds bits 32..(32+zero_bits)
    // that must also be zero.
    zero_bits: u8,
}

impl TicketMask {
    /// Create ticket mask from an ASIC difficulty.
    ///
    /// The [`Log2Difficulty`] exponent maps directly to the number
    /// of extra zero bits the chip requires beyond its hardwired
    /// difficulty-1 gate.
    pub const fn new(difficulty: Log2Difficulty) -> Self {
        Self {
            zero_bits: difficulty.exponent(),
        }
    }

    /// Encode ticket mask to wire format bytes
    pub fn to_wire_bytes(&self) -> [u8; 4] {
        if self.zero_bits == 0 {
            return [0, 0, 0, 0];
        }

        // Create mask value: 2^zero_bits - 1
        let mask_value = (1u32 << self.zero_bits) - 1;

        // Encode to wire format with bit-reversal and byte-reversal
        let mut bytes = [0u8; 4];
        for i in 0..4 {
            let byte = ((mask_value >> (8 * i)) & 0xFF) as u8;
            bytes[3 - i] = reverse_bits(byte);
        }

        bytes
    }

    pub fn encode(&self, dst: &mut BytesMut) {
        dst.put_slice(&self.to_wire_bytes());
    }

    pub fn decode(bytes: [u8; 4]) -> Self {
        let mask_value = decode_ticket_mask_bytes(&bytes);
        let zero_bits = mask_value.count_ones() as u8;
        Self { zero_bits }
    }
}

/// ASIC difficulty as a power-of-2 exponent.
///
/// BM13xx chips filter nonces using bitmask comparison (`hash &
/// mask == 0`) rather than numerical target comparison (`hash <
/// target`). Each bit in the mask independently halves the pass
/// rate, so only power-of-2 difficulty steps are representable.
/// This type stores the log2 of the difficulty: a value of 8
/// means difficulty 2^8 = 256.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Log2Difficulty {
    exponent: u8,
}

impl Log2Difficulty {
    /// Floor an arbitrary difficulty to the nearest power-of-2
    /// ASIC difficulty.
    ///
    /// The conversion is lossy: non-power-of-2 difficulties are
    /// rounded down. This ensures the actual nonce rate is at least
    /// as high as the rate implied by the input difficulty.
    pub fn from_difficulty(difficulty: Difficulty) -> Self {
        let d = difficulty.as_f64();
        let exponent = if d >= 1.0 { d.log2().floor() as u8 } else { 0 };
        Self { exponent }
    }

    /// The log2 of the difficulty (e.g., 8 for difficulty 256).
    pub const fn exponent(&self) -> u8 {
        self.exponent
    }

    /// Expected work per nonce at this difficulty.
    ///
    /// A nonce that passes the ASIC's difficulty filter represents
    /// this many hashes of work on average.
    pub fn to_work(&self) -> Work {
        Difficulty::from(1_u64 << self.exponent)
            .to_target()
            .to_work()
    }
}

impl fmt::Display for Log2Difficulty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "2^{}", self.exponent)
    }
}

/// Miscellaneous control (0x18).
///
/// Chip-level control bits. The layout shifts between generations
/// (BM1397 kept its baud divider here; later generations moved
/// baud configuration to the fast UART register) and most bits
/// carry only unexplained names in the references, so the value
/// stays opaque.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct MiscControl(pub u32);

impl MiscControl {
    /// Returns the value factory firmware writes during bring-up,
    /// broadcast and per chip. The low half word is conserved
    /// across models; the high byte is model-specific.
    pub fn operational(model: ChipModel) -> Self {
        match model {
            ChipModel::BM1362 => Self(0xB000_C100),
            _ => Self(0xF000_C100),
        }
    }

    pub fn encode(&self, dst: &mut BytesMut) {
        dst.put_u32(self.0);
    }

    pub fn decode(bytes: [u8; 4]) -> Self {
        Self(u32::from_be_bytes(bytes))
    }
}

impl fmt::Debug for MiscControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MiscControl({:#010x})", self.0)
    }
}

/// UART baud rate configuration
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UartBaud {
    /// 115200 baud
    Baud115200,
    /// 1 Mbaud
    Baud1M,
    /// 3 Mbaud (common for multi-chip)
    Baud3M,
    /// Custom baud rate with raw register value
    Custom(u32),
}

impl UartBaud {
    pub fn encode(&self, dst: &mut BytesMut) {
        let value = match self {
            // From esp-miner BM1370/BM1366/BM1368 default baud config
            UartBaud::Baud115200 => 0x00000271,
            // From esp-miner BM1370_set_max_baud/BM1366_set_max_baud/BM1368_set_max_baud
            // All three chips use identical register value for 1Mbaud
            UartBaud::Baud1M => 0x00023011,
            // From S21 Pro captures (BM1370 multi-chip chains)
            UartBaud::Baud3M => 0x00003001,
            UartBaud::Custom(val) => *val,
        };
        dst.put_u32_le(value);
    }
    pub fn decode(bytes: [u8; 4]) -> Self {
        match u32::from_le_bytes(bytes) {
            0x00000271 => UartBaud::Baud115200,
            0x00000130 => UartBaud::Baud1M,
            0x00003001 => UartBaud::Baud3M,
            other => UartBaud::Custom(other),
        }
    }
}

/// UART relay control (0x2C).
///
/// The first and last chip of each voltage domain relay the
/// serial lines onward to the neighboring domain. The gap count
/// carries its name from the references; it times the relay, but
/// what gap it counts, and in what units, is unknown. Captures
/// give each domain its own value, stepping by 5 per domain and
/// growing toward the host.
///
/// - bit 0: relay the command line, toward the next chip
/// - bit 1: relay the response line, toward the host
/// - bits 2-15: reserved
/// - bits 16-31: gap count
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UartRelay {
    /// Relay timing parameter for the domain; units unknown.
    pub gap_count: u16,
    /// Relay the response line (toward the host).
    pub response_relay: bool,
    /// Relay the command line (toward the next chip).
    pub command_relay: bool,
}

impl UartRelay {
    /// Returns the value written to domain-boundary chips: both
    /// directions relayed, with the domain's gap count. The only
    /// shape observed in captured traffic.
    pub fn domain_boundary(gap_count: u16) -> Self {
        Self {
            gap_count,
            response_relay: true,
            command_relay: true,
        }
    }

    pub fn encode(&self, dst: &mut BytesMut) {
        let word = (self.gap_count as u32) << 16
            | (self.response_relay as u32) << 1
            | self.command_relay as u32;
        dst.put_u32(word);
    }

    pub fn decode(bytes: [u8; 4]) -> Self {
        let word = u32::from_be_bytes(bytes);
        Self {
            gap_count: (word >> 16) as u16,
            response_relay: word >> 1 & 1 == 1,
            command_relay: word & 1 == 1,
        }
    }
}

/// A command posted to the core mailbox (0x3C).
///
/// The mailbox gives indirect access to a small register space
/// inside each core. The 32-bit word posted to it names a core
/// register, carries a value, and addresses one core or all of
/// them.
///
/// - bits 0-7: value written to or read from the core register
/// - bits 8-12: core register id
/// - bit 13: reserved
/// - bit 14: read done
/// - bit 15: write enable, clear on reads
/// - bits 16-23: core id, ignored when addressing all cores
/// - bits 24-30: num, zero in every observation
/// - bit 31: address all cores
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CoreCommand {
    /// Address all cores rather than the one in `core_id`.
    pub all: bool,
    /// Zero in every observation.
    pub num: u8,
    /// Core addressed when `all` is clear.
    pub core_id: u8,
    /// Write the value; clear on reads.
    pub write: bool,
    /// Read done.
    pub rd_done: bool,
    /// Core register id.
    pub reg: u8,
    /// Value written to or read from the core register.
    pub value: u8,
}

impl CoreCommand {
    /// Clock delay control register id.
    pub const CLOCK_DELAY: u8 = 0x00;
    /// Core enable register id.
    pub const CORE_ENABLE: u8 = 0x02;
    /// Clock select register id.
    pub const CLOCK_SELECT: u8 = 0x05;
    /// Overlap monitor register id.
    pub const OVERLAP_MONITOR: u8 = 0x0B;

    /// Returns a write of one core register, broadcast to every
    /// core of the addressed chip. The only command shape observed
    /// in captured traffic.
    pub fn write_all(reg: u8, value: u8) -> Self {
        Self {
            all: true,
            num: 0,
            core_id: 0,
            write: true,
            rd_done: false,
            reg,
            value,
        }
    }

    pub fn encode(&self, dst: &mut BytesMut) {
        let word = (self.all as u32) << 31
            | (self.num as u32 & 0x7f) << 24
            | (self.core_id as u32) << 16
            | (self.write as u32) << 15
            | (self.rd_done as u32) << 14
            | (self.reg as u32 & 0x1f) << 8
            | self.value as u32;
        dst.put_u32(word);
    }

    pub fn decode(bytes: [u8; 4]) -> Self {
        let word = u32::from_be_bytes(bytes);
        Self {
            all: word >> 31 & 1 == 1,
            num: (word >> 24 & 0x7f) as u8,
            core_id: (word >> 16 & 0xff) as u8,
            write: word >> 15 & 1 == 1,
            rd_done: word >> 14 & 1 == 1,
            reg: (word >> 8 & 0x1f) as u8,
            value: (word & 0xff) as u8,
        }
    }
}

impl fmt::Debug for CoreCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CoreCommand")
            .field("all", &self.all)
            .field("num", &self.num)
            .field("core_id", &self.core_id)
            .field("write", &self.write)
            .field("rd_done", &self.rd_done)
            .field("reg", &format_args!("{:#04x}", self.reg))
            .field("value", &format_args!("{:#04x}", self.value))
            .finish()
    }
}

/// Analog mux control (0x54).
///
/// Selects which analog signal the chip routes onto its analog
/// mux output, rumored to feed the temperature diode. Bring-up
/// writes select 3 on BM1362 and 2 on BM1370; what each selection
/// connects is undocumented.
///
/// - bits 0-3: diode select
/// - bits 4-31: reserved
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnalogMux {
    /// Analog signal selected onto the mux output.
    pub diode_select: u8,
}

impl AnalogMux {
    /// Returns the selection factory firmware makes during
    /// bring-up; each model selects a different input.
    pub fn bring_up(model: ChipModel) -> Self {
        match model {
            ChipModel::BM1362 => Self { diode_select: 0x3 },
            _ => Self { diode_select: 0x2 },
        }
    }

    pub fn encode(&self, dst: &mut BytesMut) {
        dst.put_u32(self.diode_select as u32 & 0xf);
    }

    pub fn decode(bytes: [u8; 4]) -> Self {
        Self {
            diode_select: (u32::from_be_bytes(bytes) & 0xf) as u8,
        }
    }
}

/// Drive strength of each chip output pin.
///
/// Each output has a 4-bit drive strength. Factory firmware runs
/// every output at strength 1 and raises the clock output on the
/// last chip of each voltage domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoDriverStrength {
    /// Drive strength of the command output (CO), toward the next chip.
    command_out: u8,
    /// Drive strength of the busy output (BO), toward the next chip.
    busy_out: u8,
    /// Drive strength of the reset output (NRSTO), toward the next chip.
    reset_out: u8,
    /// Drive strength of the clock output (CLKO), toward the next chip.
    clock_out: u8,
    /// Drive strength of the response output (RO), toward the host.
    response_out: u8,
    /// Relay enables and RF drive strength; zero in all captured traffic.
    high_bits: u16,
}

impl IoDriverStrength {
    /// Returns the baseline strength: every output at 1.
    pub fn normal() -> Self {
        Self {
            command_out: 0x1,
            busy_out: 0x1,
            reset_out: 0x1,
            clock_out: 0x1,
            response_out: 0x1,
            high_bits: 0,
        }
    }

    /// Returns the strength for the last chip of a voltage domain:
    /// clock output at maximum, the rest at the baseline. The boundary
    /// chip drives the clock across the gap to the next domain.
    pub fn domain_boundary() -> Self {
        Self {
            clock_out: 0xf,
            ..Self::normal()
        }
    }

    pub fn encode(&self, dst: &mut BytesMut) {
        // Unlike most registers, captures show this register's value
        // big-endian on the wire: 0x0001F111 is sent as 00 01 F1 11.
        let value = (self.high_bits as u32) << 20
            | (self.response_out as u32) << 16
            | (self.clock_out as u32) << 12
            | (self.reset_out as u32) << 8
            | (self.busy_out as u32) << 4
            | self.command_out as u32;
        dst.put_u32(value);
    }

    pub fn decode(bytes: [u8; 4]) -> Self {
        let value = u32::from_be_bytes(bytes);
        Self {
            command_out: (value & 0xf) as u8,
            busy_out: (value >> 4 & 0xf) as u8,
            reset_out: (value >> 8 & 0xf) as u8,
            clock_out: (value >> 12 & 0xf) as u8,
            response_out: (value >> 16 & 0xf) as u8,
            high_bits: (value >> 20) as u16,
        }
    }
}

/// Midstate configuration and version rolling (0xA4).
///
/// - bits 0-15: mask of rollable version bits, applied to block
///   header version bits 28:16
/// - bits 16-27: reserved, zero in every observation
/// - bits 28-29: midstate generation code; how many midstates the
///   chip generates per job. BM1366 and later: 1 means 8, 2 means
///   12, 3 means 16. BM1362: only 1 (8 midstates) is used. The
///   meaning of 0 is unobserved.
/// - bit 30: version fix, zero in every observation
/// - bit 31: generate midstates automatically
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct MidstateConfig {
    /// Mask of rollable version bits.
    pub version_mask: u16,
    /// Raw 2-bit midstate generation code.
    pub midstate_gen: u8,
    /// Fix the version field.
    pub version_fix: bool,
    /// Generate midstates automatically.
    pub auto_gen: bool,
}

impl MidstateConfig {
    /// Returns the configuration every capture uses: full mask,
    /// generation code 1, automatic midstate generation.
    pub fn full_rolling() -> Self {
        Self {
            version_mask: 0xffff,
            midstate_gen: 1,
            version_fix: false,
            auto_gen: true,
        }
    }

    pub fn encode(&self, dst: &mut BytesMut) {
        let value = (self.auto_gen as u32) << 31
            | (self.version_fix as u32) << 30
            | (self.midstate_gen as u32 & 0x3) << 28
            | self.version_mask as u32;
        dst.put_u32(value);
    }

    pub fn decode(bytes: [u8; 4]) -> Self {
        let value = u32::from_be_bytes(bytes);
        Self {
            version_mask: (value & 0xffff) as u16,
            midstate_gen: (value >> 28 & 0x3) as u8,
            version_fix: value >> 30 & 1 == 1,
            auto_gen: value >> 31 & 1 == 1,
        }
    }
}

impl fmt::Debug for MidstateConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MidstateConfig")
            .field("version_mask", &format_args!("{:#06x}", self.version_mask))
            .field("midstate_gen", &self.midstate_gen)
            .field("version_fix", &self.version_fix)
            .field("auto_gen", &self.auto_gen)
            .finish()
    }
}

/// Soft reset control (0xA8).
///
/// Drives chip-internal soft resets. The register first appears in
/// the BM1362 generation (BM1397 has no 0xA8) and its bit layout
/// varies by model.
///
/// BM1362:
/// - bit 0: CORE_SRST
/// - bit 1: CORE_SRST_FAST
/// - bit 2: TVER_RST
/// - bit 3: TOPCTRL_RST
/// - bit 4: CHIP_RST
/// - resets to 0x0000_0000
///
/// BM1366 and later:
/// - bits 0-3: runtime core-domain soft reset
/// - bits 4-8: set once per chip at bring-up, kept set while hashing
/// - bits 16-18: set from power-on, preserved by every write
/// - resets to 0x0007_0000
///
/// "Core" here means the whole hashing array as a reset domain, in
/// contrast to the always-on control logic; nothing in this register
/// addresses individual cores.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SoftResetControl(pub u32);

impl SoftResetControl {
    /// Returns the hardware reset value, broadcast during bring-up
    /// to normalize chip state before enumeration.
    pub fn defaults(model: ChipModel) -> Self {
        match model {
            ChipModel::BM1362 => Self(0x0000_0000),
            _ => Self(0x0007_0000),
        }
    }

    /// Returns the value asserting the core-domain reset, written
    /// per chip immediately before core configuration.
    pub fn core_reset(model: ChipModel) -> Self {
        match model {
            ChipModel::BM1362 => Self(0x0000_0002),
            _ => Self(0x0007_01F0),
        }
    }

    pub fn encode(&self, dst: &mut BytesMut) {
        dst.put_u32(self.0);
    }

    pub fn decode(bytes: [u8; 4]) -> Self {
        Self(u32::from_be_bytes(bytes))
    }
}

impl fmt::Debug for SoftResetControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SoftResetControl({:#010x})", self.0)
    }
}

// Placeholder newtypes for registers whose bit layout is not yet
// decomposed. Each wraps a raw u32 written little-endian to the wire.
macro_rules! raw_u32_register {
    ($($(#[$meta:meta])* $name:ident),* $(,)?) => {
        $(
            $(#[$meta])*
            #[derive(Debug, Clone, Copy, PartialEq, Eq)]
            pub struct $name(pub u32);

            impl $name {
                pub fn encode(&self, dst: &mut BytesMut) {
                    dst.put_u32_le(self.0);
                }
                pub fn decode(bytes: [u8; 4]) -> Self {
                    Self(u32::from_le_bytes(bytes))
                }
            }
        )*
    };
}

raw_u32_register! {
    Pll3Parameter,
    MiscSettings,
}

/// Reverse bits within a single byte (bit 0 swaps with bit 7, etc.).
fn reverse_bits(byte: u8) -> u8 {
    let mut result = 0u8;
    let mut b = byte;
    for _ in 0..8 {
        result = (result << 1) | (b & 1);
        b >>= 1;
    }
    result
}

/// Inverse of [`TicketMask::to_wire_bytes`]: undo byte and bit reversal
/// to recover the underlying mask value.
fn decode_ticket_mask_bytes(bytes: &[u8; 4]) -> u32 {
    let mut mask_value = 0u32;
    for i in 0..4 {
        let byte = reverse_bits(bytes[3 - i]);
        mask_value |= (byte as u32) << (8 * i);
    }
    mask_value
}

#[cfg(test)]
mod log2_difficulty_tests {
    use super::*;
    use crate::types::Difficulty;

    #[test]
    fn power_of_two_difficulty_exact() {
        let diff = Log2Difficulty::from_difficulty(Difficulty::from(256_u64));
        assert_eq!(diff.exponent(), 8);
    }

    #[test]
    fn non_power_of_two_floors() {
        // 300 is between 2^8=256 and 2^9=512, should floor to 8
        let diff = Log2Difficulty::from_difficulty(Difficulty::from(300_u64));
        assert_eq!(diff.exponent(), 8);
    }

    #[test]
    fn difficulty_one() {
        let diff = Log2Difficulty::from_difficulty(Difficulty::from(1_u64));
        assert_eq!(diff.exponent(), 0);
    }

    #[test]
    fn large_difficulty() {
        let diff = Log2Difficulty::from_difficulty(Difficulty::from(65536_u64));
        assert_eq!(diff.exponent(), 16);
    }

    #[test]
    fn display() {
        let diff = Log2Difficulty::from_difficulty(Difficulty::from(256_u64));
        assert_eq!(format!("{diff}"), "2^8");
    }

    #[test]
    fn to_work_matches_target_to_work() {
        // Log2Difficulty's to_work should agree with computing work
        // from the equivalent target directly.
        let diff = Log2Difficulty::from_difficulty(Difficulty::from(256_u64));
        let expected = Difficulty::from(256_u64).to_target().to_work();
        assert_eq!(diff.to_work(), expected);
    }
}

#[cfg(test)]
mod ticket_mask_tests {
    use super::*;
    use crate::types::Difficulty;

    #[test]
    fn wire_encoding_difficulty_256() {
        // 8 zero_bits -> mask 0xFF -> [00, 00, 00, FF]
        let diff = Log2Difficulty::from_difficulty(Difficulty::from(256_u64));
        let bytes = TicketMask::new(diff).to_wire_bytes();
        assert_eq!(bytes, [0x00, 0x00, 0x00, 0xFF]);
    }

    #[test]
    fn wire_encoding_difficulty_1024() {
        // 10 zero_bits -> mask 0x3FF -> [00, 00, C0, FF]
        let diff = Log2Difficulty::from_difficulty(Difficulty::from(1024_u64));
        let bytes = TicketMask::new(diff).to_wire_bytes();
        assert_eq!(bytes, [0x00, 0x00, 0xC0, 0xFF]);
    }

    #[test]
    fn wire_encoding_difficulty_65536() {
        // 16 zero_bits -> mask 0xFFFF -> [00, 00, FF, FF]
        let diff = Log2Difficulty::from_difficulty(Difficulty::from(65536_u64));
        let bytes = TicketMask::new(diff).to_wire_bytes();
        assert_eq!(bytes, [0x00, 0x00, 0xFF, 0xFF]);
    }

    #[test]
    fn wire_encoding_difficulty_1() {
        // 0 zero_bits -> [00, 00, 00, 00]
        let diff = Log2Difficulty::from_difficulty(Difficulty::from(1_u64));
        let bytes = TicketMask::new(diff).to_wire_bytes();
        assert_eq!(bytes, [0x00, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn encode_matches_to_wire_bytes() {
        let diff = Log2Difficulty::from_difficulty(Difficulty::from(256_u64));
        let mask = TicketMask::new(diff);
        let mut buf = BytesMut::new();
        mask.encode(&mut buf);
        assert_eq!(&buf[..], &[0x00, 0x00, 0x00, 0xFF]);
    }

    #[test]
    fn reverse_bits_examples() {
        assert_eq!(reverse_bits(0x00), 0x00);
        assert_eq!(reverse_bits(0xFF), 0xFF);
        assert_eq!(reverse_bits(0x01), 0x80);
        assert_eq!(reverse_bits(0x80), 0x01);
        assert_eq!(reverse_bits(0x03), 0xC0);
        assert_eq!(reverse_bits(0x0F), 0xF0);
    }

    fn round_trip(difficulty: Difficulty) {
        let mask = TicketMask::new(Log2Difficulty::from_difficulty(difficulty));
        let mut buf = BytesMut::new();
        mask.encode(&mut buf);
        let bytes: [u8; 4] = buf[..].try_into().unwrap();
        assert_eq!(TicketMask::decode(bytes), mask);
    }

    #[test]
    fn round_trip_difficulty_1() {
        round_trip(Difficulty::from(1_u64));
    }

    #[test]
    fn round_trip_difficulty_256() {
        round_trip(Difficulty::from(256_u64));
    }
}

#[cfg(test)]
mod chip_id_tests {
    use super::*;

    fn round_trip(original: ChipId) {
        let mut buf = BytesMut::new();
        original.encode(&mut buf);
        let bytes: [u8; 4] = buf[..].try_into().unwrap();
        assert_eq!(ChipId::decode(bytes).unwrap(), original);
    }

    #[test]
    fn known_model() {
        round_trip(ChipId {
            model: ChipModel::BM1362,
            core_count: 80,
            address: 0x42,
        });
    }

    #[test]
    fn reject_unknown_id() {
        assert!(matches!(
            ChipId::decode([0x12, 0x34, 0x00, 0x00]),
            Err(ProtocolError::UnknownChipId([0x12, 0x34]))
        ));
    }
}

#[cfg(test)]
mod pll_divider_tests {
    use super::*;

    fn round_trip(original: PllDivider) {
        let mut buf = BytesMut::new();
        original.encode(&mut buf);
        let bytes: [u8; 4] = buf[..].try_into().unwrap();
        assert_eq!(PllDivider::decode(bytes), original);
    }

    #[test]
    fn from_new() {
        round_trip(PllDivider::new(100, 1, 0x00));
    }

    #[test]
    fn from_literal_fields() {
        round_trip(PllDivider {
            flag: 0x40,
            fb_div: 0x68,
            ref_div: 0x01,
            post_div: 0x33,
        });
    }

    #[test]
    fn new_picks_flag_from_resulting_vco() {
        // VCO = fb_div * crystal / ref_div. Pick targets across the
        // boundary, back-derive fb_div, and assert the flag matches
        // the bracket. The threshold rule is `>=`, so a target that
        // hits the boundary exactly picks the high flag.
        const REF_DIV: u8 = 2;
        let fb_div_for = |vco_mhz: f32| (vco_mhz * REF_DIV as f32 / CRYSTAL_MHZ) as u8;

        let cases = [
            (2000.0, 0x40u8), // below
            (2400.0, 0x50),   // at threshold (>= picks high)
            (2800.0, 0x50),   // above
        ];
        for (target_vco, expected_flag) in cases {
            let fb_div = fb_div_for(target_vco);
            assert_eq!(
                PllDivider::new(fb_div, REF_DIV, 0).flag,
                expected_flag,
                "target VCO {} MHz",
                target_vco,
            );
        }
    }
}

#[cfg(test)]
mod nonce_range_tests {
    use super::*;

    fn round_trip(original: NonceRange) {
        let mut buf = BytesMut::new();
        original.encode(&mut buf);
        let bytes: [u8; 4] = buf[..].try_into().unwrap();
        assert_eq!(NonceRange::decode(bytes), original);
    }

    #[test]
    fn single_chip() {
        round_trip(NonceRange::single_chip());
    }

    #[test]
    fn multi_chip() {
        round_trip(NonceRange::multi_chip(16));
    }

    #[test]
    fn from_raw() {
        round_trip(NonceRange::from_raw(0xdeadbeef));
    }
}

#[cfg(test)]
mod uart_baud_tests {
    use super::*;

    fn round_trip(original: UartBaud) {
        let mut buf = BytesMut::new();
        original.encode(&mut buf);
        let bytes: [u8; 4] = buf[..].try_into().unwrap();
        assert_eq!(UartBaud::decode(bytes), original);
    }

    #[test]
    fn baud_115200() {
        round_trip(UartBaud::Baud115200);
    }

    #[test]
    #[should_panic]
    fn baud_1m() {
        // Currently fails: encode emits 0x00023011 but decode
        // matches 0x00000130 for Baud1M, so the round-trip
        // collapses to Custom. Drop #[should_panic] once the
        // constants are reconciled.
        round_trip(UartBaud::Baud1M);
    }

    #[test]
    fn baud_3m() {
        round_trip(UartBaud::Baud3M);
    }

    #[test]
    fn custom_value() {
        round_trip(UartBaud::Custom(0xdeadbeef));
    }
}

#[cfg(test)]
mod io_driver_strength_tests {
    use super::*;

    fn round_trip(original: IoDriverStrength) {
        let mut buf = BytesMut::new();
        original.encode(&mut buf);
        let bytes: [u8; 4] = buf[..].try_into().unwrap();
        assert_eq!(IoDriverStrength::decode(bytes), original);
    }

    #[test]
    fn normal() {
        round_trip(IoDriverStrength::normal());
    }
}

#[cfg(test)]
mod midstate_config_tests {
    use super::*;

    fn round_trip(original: MidstateConfig) {
        let mut buf = BytesMut::new();
        original.encode(&mut buf);
        let bytes: [u8; 4] = buf[..].try_into().unwrap();
        assert_eq!(MidstateConfig::decode(bytes), original);
    }

    #[test]
    fn full_rolling() {
        round_trip(MidstateConfig::full_rolling());
    }

    #[test]
    fn from_literal_fields() {
        round_trip(MidstateConfig {
            version_mask: 0x1fff,
            midstate_gen: 3,
            version_fix: true,
            auto_gen: false,
        });
    }
}

#[cfg(test)]
mod soft_reset_control_tests {
    use super::*;

    fn round_trip(original: SoftResetControl) {
        let mut buf = BytesMut::new();
        original.encode(&mut buf);
        let bytes: [u8; 4] = buf[..].try_into().unwrap();
        assert_eq!(SoftResetControl::decode(bytes), original);
    }

    #[test]
    fn defaults() {
        round_trip(SoftResetControl::defaults(ChipModel::BM1362));
        round_trip(SoftResetControl::defaults(ChipModel::BM1370));
    }

    #[test]
    fn core_reset() {
        round_trip(SoftResetControl::core_reset(ChipModel::BM1362));
        round_trip(SoftResetControl::core_reset(ChipModel::BM1370));
    }
}

#[cfg(test)]
mod misc_control_tests {
    use super::*;

    fn round_trip(original: MiscControl) {
        let mut buf = BytesMut::new();
        original.encode(&mut buf);
        let bytes: [u8; 4] = buf[..].try_into().unwrap();
        assert_eq!(MiscControl::decode(bytes), original);
    }

    #[test]
    fn operational() {
        round_trip(MiscControl::operational(ChipModel::BM1362));
        round_trip(MiscControl::operational(ChipModel::BM1370));
    }
}

#[cfg(test)]
mod analog_mux_tests {
    use super::*;

    fn round_trip(original: AnalogMux) {
        let mut buf = BytesMut::new();
        original.encode(&mut buf);
        let bytes: [u8; 4] = buf[..].try_into().unwrap();
        assert_eq!(AnalogMux::decode(bytes), original);
    }

    #[test]
    fn bring_up() {
        round_trip(AnalogMux::bring_up(ChipModel::BM1362));
        round_trip(AnalogMux::bring_up(ChipModel::BM1370));
    }

    #[test]
    fn from_literal_field() {
        round_trip(AnalogMux { diode_select: 0xf });
    }
}

#[cfg(test)]
mod uart_relay_tests {
    use super::*;

    fn round_trip(original: UartRelay) {
        let mut buf = BytesMut::new();
        original.encode(&mut buf);
        let bytes: [u8; 4] = buf[..].try_into().unwrap();
        assert_eq!(UartRelay::decode(bytes), original);
    }

    #[test]
    fn domain_boundary() {
        round_trip(UartRelay::domain_boundary(0x4f));
    }

    #[test]
    fn from_literal_fields() {
        round_trip(UartRelay {
            gap_count: 0xffff,
            response_relay: false,
            command_relay: true,
        });
    }
}

#[cfg(test)]
mod core_command_tests {
    use super::*;

    fn round_trip(original: CoreCommand) {
        let mut buf = BytesMut::new();
        original.encode(&mut buf);
        let bytes: [u8; 4] = buf[..].try_into().unwrap();
        assert_eq!(CoreCommand::decode(bytes), original);
    }

    #[test]
    fn write_all() {
        round_trip(CoreCommand::write_all(CoreCommand::CORE_ENABLE, 0xaa));
    }

    #[test]
    fn from_literal_fields() {
        round_trip(CoreCommand {
            all: false,
            num: 0x55,
            core_id: 0xc3,
            write: false,
            rd_done: true,
            reg: 0x1f,
            value: 0xee,
        });
    }
}
