//! BM13xx chip registers as typed values.
//!
//! [`RegisterAddress`] names each register on a BM13xx chip.
//! [`Register`] carries the same set with a typed payload per
//! variant, so values coming off the wire are typed rather than raw
//! bit fields.

use bitcoin::pow::Work;
use bytes::{BufMut, BytesMut};
use num_enum::{FromPrimitive, IntoPrimitive, TryFromPrimitive};
use std::fmt;

use super::chip_config::CRYSTAL_MHZ;
use super::error::ProtocolError;
use crate::types::Difficulty;

/// Register addresses on the wire.
#[derive(TryFromPrimitive, Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum RegisterAddress {
    ChipId = 0x00,
    PllDivider = 0x08,
    HashCountingNumber = 0x10,
    TicketMask = 0x14,
    MiscControl = 0x18,
    UartBaud = 0x28,
    UartRelay = 0x2C,
    CoreMailbox = 0x3C,
    AnalogMux = 0x54,
    IoDriverStrength = 0x58,
    RingOscPadDisable = 0x68,
    MidstateConfig = 0xA4,
    SoftResetControl = 0xA8,
    AdcCtrl1 = 0xB9,
}

/// A register with its typed payload.
#[derive(Debug, Clone, PartialEq)]
pub enum Register {
    ChipId(ChipId),
    PllDivider(PllDivider),
    HashCountingNumber(HashCountingNumber),
    TicketMask(TicketMask),
    MiscControl(MiscControl),
    UartBaud(UartBaud),
    UartRelay(UartRelay),
    CoreMailbox(CoreCommand),
    AnalogMux(AnalogMux),
    IoDriverStrength(IoDriverStrength),
    RingOscPadDisable(RingOscPadDisable),
    MidstateConfig(MidstateConfig),
    SoftResetControl(SoftResetControl),
    AdcCtrl1(AdcCtrl1),
}

impl Register {
    pub fn decode(address: RegisterAddress, bytes: [u8; 4]) -> Result<Register, ProtocolError> {
        Ok(match address {
            RegisterAddress::ChipId => Register::ChipId(ChipId::decode(bytes)?),
            RegisterAddress::PllDivider => Register::PllDivider(PllDivider::decode(bytes)),
            RegisterAddress::HashCountingNumber => {
                Register::HashCountingNumber(HashCountingNumber::decode(bytes))
            }
            RegisterAddress::TicketMask => Register::TicketMask(TicketMask::decode(bytes)),
            RegisterAddress::MiscControl => Register::MiscControl(MiscControl::decode(bytes)),
            RegisterAddress::UartBaud => Register::UartBaud(UartBaud::decode(bytes)),
            RegisterAddress::UartRelay => Register::UartRelay(UartRelay::decode(bytes)),
            RegisterAddress::CoreMailbox => Register::CoreMailbox(CoreCommand::decode(bytes)),
            RegisterAddress::AnalogMux => Register::AnalogMux(AnalogMux::decode(bytes)),
            RegisterAddress::IoDriverStrength => {
                Register::IoDriverStrength(IoDriverStrength::decode(bytes))
            }
            RegisterAddress::RingOscPadDisable => {
                Register::RingOscPadDisable(RingOscPadDisable::decode(bytes))
            }
            RegisterAddress::MidstateConfig => {
                Register::MidstateConfig(MidstateConfig::decode(bytes))
            }
            RegisterAddress::SoftResetControl => {
                Register::SoftResetControl(SoftResetControl::decode(bytes))
            }
            RegisterAddress::AdcCtrl1 => Register::AdcCtrl1(AdcCtrl1::decode(bytes)),
        })
    }

    pub(super) fn address(&self) -> RegisterAddress {
        match self {
            Register::ChipId(_) => RegisterAddress::ChipId,
            Register::PllDivider(_) => RegisterAddress::PllDivider,
            Register::HashCountingNumber(_) => RegisterAddress::HashCountingNumber,
            Register::TicketMask(_) => RegisterAddress::TicketMask,
            Register::MiscControl(_) => RegisterAddress::MiscControl,
            Register::UartBaud(_) => RegisterAddress::UartBaud,
            Register::UartRelay(_) => RegisterAddress::UartRelay,
            Register::CoreMailbox(_) => RegisterAddress::CoreMailbox,
            Register::AnalogMux(_) => RegisterAddress::AnalogMux,
            Register::IoDriverStrength(_) => RegisterAddress::IoDriverStrength,
            Register::RingOscPadDisable(_) => RegisterAddress::RingOscPadDisable,
            Register::MidstateConfig(_) => RegisterAddress::MidstateConfig,
            Register::SoftResetControl(_) => RegisterAddress::SoftResetControl,
            Register::AdcCtrl1(_) => RegisterAddress::AdcCtrl1,
        }
    }

    pub(super) fn encode(&self, dst: &mut BytesMut) {
        match self {
            Register::ChipId(r) => r.encode(dst),
            Register::PllDivider(r) => r.encode(dst),
            Register::HashCountingNumber(r) => r.encode(dst),
            Register::TicketMask(r) => r.encode(dst),
            Register::MiscControl(r) => r.encode(dst),
            Register::UartBaud(r) => r.encode(dst),
            Register::UartRelay(r) => r.encode(dst),
            Register::CoreMailbox(r) => r.encode(dst),
            Register::AnalogMux(r) => r.encode(dst),
            Register::IoDriverStrength(r) => r.encode(dst),
            Register::RingOscPadDisable(r) => r.encode(dst),
            Register::MidstateConfig(r) => r.encode(dst),
            Register::SoftResetControl(r) => r.encode(dst),
            Register::AdcCtrl1(r) => r.encode(dst),
        }
    }
}

/// Chip identification (0x00): the model, an unknown byte, and the
/// assigned chain address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChipId {
    pub model: ChipModel,
    /// Unknown byte; reads 0x00 on the BM1370 and 0x03 on the
    /// BM1362.
    pub unknown: u8,
    pub address: u8,
}

impl ChipId {
    pub fn encode(&self, dst: &mut BytesMut) {
        dst.put_slice(&self.model.id_bytes());
        dst.put_u8(self.unknown);
        dst.put_u8(self.address);
    }
    pub fn decode(bytes: [u8; 4]) -> Result<Self, ProtocolError> {
        Ok(Self {
            model: ChipModel::try_from([bytes[0], bytes[1]])?,
            unknown: bytes[2],
            address: bytes[3],
        })
    }
}

/// Chip models the BM13xx stack supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipModel {
    /// Used in the Antminer S19j Pro and the EmberOne00.
    BM1362,
    /// Used in the Antminer S19 XP and S19k Pro.
    BM1366,
    /// Used in the Bitaxe Gamma and the Antminer S21 Pro.
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
    /// PLL lock report (LOCKED, bit 31). Written 0; the chip
    /// answers 1 once the PLL locks.
    pub locked: bool,
    /// PLL enable (PLLEN, bit 30). 1 in every captured write.
    pub pll_en: bool,
    /// PLL bypass (BYPASS, bit 29). 0 in every captured write.
    pub bypass: bool,
    /// VCO range select (VCOSEL, bit 28), derived from the
    /// dividers.
    pub vco_sel: VcoSel,
    /// Feedback divider.
    pub fb_div: u8,
    /// Reference divider (typically 1 or 2).
    pub ref_div: u8,
    /// Post divider, encoded as `((post_div1-1) << 4) | (post_div2-1)`.
    pub post_div: u8,
}

impl PllDivider {
    /// Builds an enabled [`PllDivider`] with `vco_sel` derived from
    /// the dividers.
    pub fn new(fb_div: u8, ref_div: u8, post_div: u8) -> Self {
        const VCO_SEL_THRESHOLD_MHZ: f32 = 2400.0;

        let vco_mhz = fb_div as f32 * CRYSTAL_MHZ / ref_div as f32;
        let vco_sel = if vco_mhz >= VCO_SEL_THRESHOLD_MHZ {
            VcoSel::High
        } else {
            VcoSel::Low
        };
        Self {
            locked: false,
            pll_en: true,
            bypass: false,
            vco_sel,
            fb_div,
            ref_div,
            post_div,
        }
    }

    pub fn encode(&self, dst: &mut BytesMut) {
        let flags = (self.locked as u8) << 7
            | (self.pll_en as u8) << 6
            | (self.bypass as u8) << 5
            | ((self.vco_sel == VcoSel::High) as u8) << 4;
        dst.put_u8(flags);
        dst.put_u8(self.fb_div);
        dst.put_u8(self.ref_div);
        dst.put_u8(self.post_div);
    }

    pub fn decode(bytes: [u8; 4]) -> Self {
        Self {
            locked: bytes[0] & 0x80 != 0,
            pll_en: bytes[0] & 0x40 != 0,
            bypass: bytes[0] & 0x20 != 0,
            vco_sel: if bytes[0] & 0x10 != 0 {
                VcoSel::High
            } else {
                VcoSel::Low
            },
            fb_div: bytes[1],
            ref_div: bytes[2],
            post_div: bytes[3],
        }
    }
}

/// VCO range select (VCOSEL), bit 28 of PLL_DIVIDER (0x08).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VcoSel {
    /// VCO below 2400 MHz.
    Low,
    /// VCO at or above 2400 MHz.
    High,
}

/// Hash counting number (0x10), the nonce sweep deadline.
///
/// Sets how long each core sweeps nonces for the current batch of
/// rolled versions: the register counts ticks of the 25 MHz
/// crystal, a deadline rather than a nonce quota. When the count
/// runs out, the chip rolls the next batch of versions and the
/// core re-sweeps the same nonce window. Zero halts hashing. A
/// value is correct only for the hash frequency it was computed
/// at; REFERENCE.md gives the formula and the factory values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HashCountingNumber {
    /// Ticks of the 25 MHz crystal.
    ticks: u32,
}

impl HashCountingNumber {
    pub fn encode(&self, dst: &mut BytesMut) {
        dst.put_u32(self.ticks);
    }

    pub fn decode(bytes: [u8; 4]) -> Self {
        Self {
            ticks: u32::from_be_bytes(bytes),
        }
    }
}

impl From<u32> for HashCountingNumber {
    fn from(ticks: u32) -> Self {
        Self { ticks }
    }
}

impl From<HashCountingNumber> for u32 {
    fn from(hcn: HashCountingNumber) -> Self {
        hcn.ticks
    }
}

/// Ticket mask controlling ASIC nonce reporting
#[derive(derive_more::Debug, Clone, Copy, PartialEq)]
pub struct TicketMask {
    // Mask of additional zero bits required in the bit-reversed
    // hash, beyond the base 32 bits the chip always requires. Held
    // as the mask word rather than a count so a readback of a
    // non-contiguous mask round-trips instead of normalizing.
    #[debug("{mask:#010x}")]
    mask: u32,
}

impl TicketMask {
    /// Create ticket mask from an ASIC difficulty.
    ///
    /// The [`Log2Difficulty`] exponent maps directly to the number
    /// of extra zero bits the chip requires beyond its hardwired
    /// difficulty-1 gate.
    pub const fn new(difficulty: Log2Difficulty) -> Self {
        let zero_bits = difficulty.exponent();
        let mask = if zero_bits >= 32 {
            u32::MAX
        } else {
            (1u32 << zero_bits) - 1
        };
        Self { mask }
    }

    /// Encode ticket mask to wire format bytes
    pub fn to_wire_bytes(&self) -> [u8; 4] {
        // Encode to wire format with bit-reversal and byte-reversal
        let mut bytes = [0u8; 4];
        for i in 0..4 {
            let byte = ((self.mask >> (8 * i)) & 0xFF) as u8;
            bytes[3 - i] = reverse_bits(byte);
        }

        bytes
    }

    pub fn encode(&self, dst: &mut BytesMut) {
        dst.put_slice(&self.to_wire_bytes());
    }

    pub fn decode(bytes: [u8; 4]) -> Self {
        Self {
            mask: decode_ticket_mask_bytes(&bytes),
        }
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
/// Gates the chip's nonce reporting. A fresh chip powers on with
/// all report enables clear and cannot report a nonce.
///
/// - bits 31-28: report enables, one section of the core array
///   per bit
/// - bits 27-16: unexplained
/// - bits 15-0: 0xC100 out of reset; every observed write sets
///   them to 0xC100
#[derive(derive_more::Debug, Clone, Copy, PartialEq, Eq)]
pub struct MiscControl {
    /// Report enables, one section of the core array per element;
    /// element 0 holds register bit 28, element 3 register bit 31.
    report_enables: [bool; 4],
    /// Undecoded bits 27-16, held in place.
    #[debug("{unexplained:#010x}")]
    unexplained: u32,
    /// Low half word, 0xC100 out of reset.
    #[debug("{power_on:#06x}")]
    power_on: u16,
}

impl MiscControl {
    /// Returns the value factory firmware writes during bring-up
    /// to switch nonce reporting on ("open core" in the
    /// references).
    pub fn reporting_enabled(model: ChipModel) -> Self {
        let report_enables = match model {
            ChipModel::BM1362 => [true, true, false, true],
            _ => [true; 4],
        };
        Self {
            report_enables,
            unexplained: 0,
            power_on: 0xC100,
        }
    }

    pub fn encode(&self, dst: &mut BytesMut) {
        let mut value = self.unexplained | self.power_on as u32;
        for (i, enabled) in self.report_enables.iter().enumerate() {
            value |= (*enabled as u32) << (28 + i);
        }
        dst.put_u32(value);
    }

    pub fn decode(bytes: [u8; 4]) -> Self {
        let value = u32::from_be_bytes(bytes);
        let mut report_enables = [false; 4];
        for (i, enabled) in report_enables.iter_mut().enumerate() {
            *enabled = value >> (28 + i) & 1 == 1;
        }
        Self {
            report_enables,
            unexplained: value & 0x0fff_0000,
            power_on: (value & 0xffff) as u16,
        }
    }
}

/// UART baud rate (0x28).
///
/// Sets the serial link's baud rate with a clock divider:
/// baud = 25 MHz / (8 x (divider + 1)). The register resets to
/// divider 26, 115,740 baud; the host raises the rate once during
/// bring-up by writing a smaller divider.
///
/// - bit 28: set by BM1362-generation firmware and cleared by
///   BM1370-generation firmware, for the same resulting rate; its
///   function is unknown
/// - bits 27-16: 0x130 in the reset value and in every observed
///   write; unexplained
/// - bits 15-8: the divider
///
/// Bits 31-29 and 7-0 are zero in every observation.
#[derive(derive_more::Debug, Clone, Copy, PartialEq, Eq)]
pub struct UartBaud {
    /// Baud rate divider: baud = 25 MHz / (8 x (divider + 1)).
    pub divider: u8,
    /// Unknown-function bit 28; firmware generations disagree on it.
    pub bit28: bool,
    /// Undecoded bits, held in place: bits 27-16 hold 0x130 in
    /// every observation, bits 31-29 and 7-0 zero.
    #[debug("{unexplained:#010x}")]
    pub unexplained: u32,
}

impl UartBaud {
    /// Returns the register selecting the representable baud rate
    /// nearest `target`, following observed firmware, which picks
    /// the nearest rate even when it is above the target. Bit 28
    /// is clear and the unexplained bits hold their observed value.
    pub fn for_baud(target: u32) -> Self {
        let steps = (CRYSTAL_MHZ * 1_000_000.0 / (8.0 * target as f32)).round() as u8;
        Self {
            divider: steps.saturating_sub(1),
            bit28: false,
            unexplained: 0x0130_0000,
        }
    }

    /// Returns the baud rate the divider selects.
    pub fn baud(&self) -> u32 {
        (CRYSTAL_MHZ * 1_000_000.0 / (8.0 * (self.divider as f32 + 1.0))) as u32
    }

    pub fn encode(&self, dst: &mut BytesMut) {
        let value = (self.bit28 as u32) << 28 | self.unexplained | (self.divider as u32) << 8;
        dst.put_u32(value);
    }

    pub fn decode(bytes: [u8; 4]) -> Self {
        let value = u32::from_be_bytes(bytes);
        Self {
            divider: (value >> 8) as u8,
            bit28: value >> 28 & 1 == 1,
            unexplained: value & 0xefff_00ff,
        }
    }
}

/// UART relay control (0x2C).
///
/// Holds a relay enable for each serial direction and a gap
/// count that sets a wait. Relay and gap count are names from the
/// references. The wait most likely applies to the response line,
/// which every chip shares to reach the host; REFERENCE.md argues
/// the interpretation under The Serial Chain.
///
/// - bits 31-16: gap count
/// - bits 15-2: zero in every capture
/// - bit 1: relay the response line, toward the host
/// - bit 0: relay the command line, toward the next chip
#[derive(derive_more::Debug, Clone, Copy, PartialEq, Eq)]
pub struct UartRelay {
    /// Relay timing parameter for the domain; units unknown.
    pub gap_count: u16,
    /// Undecoded bits 15-2, zero in every capture, held in place.
    #[debug("{unexplained:#010x}")]
    pub unexplained: u32,
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
            unexplained: 0,
            response_relay: true,
            command_relay: true,
        }
    }

    pub fn encode(&self, dst: &mut BytesMut) {
        let word = (self.gap_count as u32) << 16
            | self.unexplained
            | (self.response_relay as u32) << 1
            | self.command_relay as u32;
        dst.put_u32(word);
    }

    pub fn decode(bytes: [u8; 4]) -> Self {
        let word = u32::from_be_bytes(bytes);
        Self {
            gap_count: (word >> 16) as u16,
            unexplained: word & 0xfffc,
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
/// - bit 31: address all cores
/// - bits 30-24: num, zero in every observation
/// - bits 23-16: core id, ignored when addressing all cores
/// - bit 15: write enable, clear on reads
/// - bit 14: read done
/// - bit 13: zero in every observation
/// - bits 12-8: core register id
/// - bits 7-0: value written to or read from the core register
#[derive(derive_more::Debug, Clone, Copy, PartialEq, Eq)]
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
    /// Undecoded bit 13, zero in every observation, held in place.
    #[debug("{unexplained:#010x}")]
    pub unexplained: u32,
    /// Core register addressed.
    pub reg: CoreRegister,
    /// Value written to or read from the core register.
    #[debug("{value:#04x}")]
    pub value: u8,
}

impl CoreCommand {
    /// Returns a write switching the nonce bin overflow control on
    /// or off, using the values observed in captures (0xEE on,
    /// 0xEF off).
    pub fn nonce_bin_overflow(enable: bool) -> Self {
        Self::write_all(
            CoreRegister::NonceBinOverflow,
            if enable { 0xEE } else { 0xEF },
        )
    }

    /// Returns a write of one core register, broadcast to every
    /// core of the addressed chip. The only command shape observed
    /// in captured traffic.
    pub fn write_all(reg: CoreRegister, value: u8) -> Self {
        Self {
            all: true,
            num: 0,
            core_id: 0,
            write: true,
            rd_done: false,
            unexplained: 0,
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
            | self.unexplained
            | (u8::from(self.reg) as u32 & 0x1f) << 8
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
            unexplained: word & 0x2000,
            reg: CoreRegister::from((word >> 8 & 0x1f) as u8),
            value: (word & 0xff) as u8,
        }
    }
}

/// A core register addressed through the mailbox's 5-bit id field.
///
/// The named registers are the ones REFERENCE.md decodes from
/// bring-up traffic; any other id decodes as [`CoreRegister::Unknown`].
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoPrimitive, FromPrimitive)]
pub enum CoreRegister {
    /// Clock delay control.
    ClockDelay = 0x00,
    /// Core enable.
    CoreEnable = 0x02,
    /// Clock select on the BM1362 and BM1366.
    ClockSelectBM1362 = 0x05,
    /// Clock select on the BM1368 and BM1370.
    ClockSelectBM1368 = 0x0B,
    /// Nonce bin overflow control.
    NonceBinOverflow = 0x0D,
    /// A core register id without a decoded meaning.
    #[num_enum(catch_all)]
    Unknown(u8),
}

/// Analog mux control (0x54).
///
/// Selects which analog signal the chip routes onto its analog
/// mux output. Select 2 routes the temperature diode to the
/// output, where an external sensor reads it (seen on the
/// BM1370). Firmware writes select 3 when it is not reading the
/// diode (seen on the BM1362 and BM1370). No source names what
/// the other selects connect, and the mapping may differ by
/// model.
///
/// - bits 31-4: zero in every capture
/// - bits 3-0: diode select
#[derive(derive_more::Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnalogMux {
    /// Analog signal selected onto the mux output.
    pub diode_select: u8,
    /// Undecoded bits 31-4, zero in every capture, held in place.
    #[debug("{unexplained:#010x}")]
    pub unexplained: u32,
}

impl AnalogMux {
    /// Returns the selection factory firmware makes during
    /// bring-up; each model selects a different input.
    pub fn bring_up(model: ChipModel) -> Self {
        match model {
            ChipModel::BM1362 => Self {
                diode_select: 0x3,
                unexplained: 0,
            },
            _ => Self {
                diode_select: 0x2,
                unexplained: 0,
            },
        }
    }

    pub fn encode(&self, dst: &mut BytesMut) {
        dst.put_u32(self.unexplained | self.diode_select as u32 & 0xf);
    }

    pub fn decode(bytes: [u8; 4]) -> Self {
        let value = u32::from_be_bytes(bytes);
        Self {
            diode_select: (value & 0xf) as u8,
            unexplained: value & !0xf,
        }
    }
}

/// Drive strength of each chip output pin.
///
/// Each output has a 4-bit drive strength. Factory firmware runs
/// every output at strength 1 and raises the clock output on the
/// last chip of each voltage domain.
#[derive(derive_more::Debug, Clone, Copy, PartialEq, Eq)]
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
    /// Undecoded bits 31-20, zero in every capture, held in place.
    #[debug("{unexplained:#010x}")]
    unexplained: u32,
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
            unexplained: 0,
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
        let value = self.unexplained
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
            unexplained: value & 0xfff0_0000,
        }
    }
}

/// Midstate configuration and version rolling (0xA4).
///
/// - bit 31: generate midstates automatically
/// - bit 30: version fix, zero in every observation
/// - bits 29-28: midstate generation code; how many midstates the
///   chip generates per job. BM1366 and later: 1 means 8, 2 means
///   12, 3 means 16. BM1362: only 1 (8 midstates) is used. The
///   meaning of 0 is unobserved.
/// - bits 27-16: zero in every observation
/// - bits 15-0: mask of rollable version bits, applied to block
///   header version bits 28-13
#[derive(derive_more::Debug, Clone, Copy, PartialEq, Eq)]
pub struct MidstateConfig {
    /// Mask of rollable version bits.
    #[debug("{version_mask:#06x}")]
    pub version_mask: u16,
    /// Raw 2-bit midstate generation code.
    pub midstate_gen: u8,
    /// Undecoded bits 27-16, zero in every observation, held in
    /// place.
    #[debug("{unexplained:#010x}")]
    pub unexplained: u32,
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
            unexplained: 0,
            version_fix: false,
            auto_gen: true,
        }
    }

    pub fn encode(&self, dst: &mut BytesMut) {
        let value = (self.auto_gen as u32) << 31
            | (self.version_fix as u32) << 30
            | (self.midstate_gen as u32 & 0x3) << 28
            | self.unexplained
            | self.version_mask as u32;
        dst.put_u32(value);
    }

    pub fn decode(bytes: [u8; 4]) -> Self {
        let value = u32::from_be_bytes(bytes);
        Self {
            version_mask: (value & 0xffff) as u16,
            midstate_gen: (value >> 28 & 0x3) as u8,
            unexplained: value & 0x0fff_0000,
            version_fix: value >> 30 & 1 == 1,
            auto_gen: value >> 31 & 1 == 1,
        }
    }
}

/// Soft reset control (0xA8).
///
/// Drives chip-internal soft resets. The register first appears in
/// the BM1362 generation (BM1397 has no 0xA8) and its bit layout
/// varies by model.
///
/// BM1362:
/// - bit 4: CHIP_RST
/// - bit 3: TOPCTRL_RST
/// - bit 2: TVER_RST
/// - bit 1: CORE_SRST_FAST
/// - bit 0: CORE_SRST
/// - resets to 0x0000_0000
///
/// BM1366 and later:
/// - bits 18-16: set from power-on, preserved by every write
/// - bits 8-4: set once per chip at bring-up, kept set while hashing
/// - bits 3-0: runtime core-domain soft reset
/// - resets to 0x0007_0000
///
/// "Core" here means the whole hashing array as a reset domain, in
/// contrast to the always-on control logic; nothing in this register
/// addresses individual cores.
#[derive(derive_more::Debug, Clone, Copy, PartialEq, Eq)]
#[debug("SoftResetControl({_0:#010x})")]
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

// Newtypes for registers no source decomposes into bit fields. Each
// holds its four wire bytes verbatim, so a value in code reads the
// same as it does in a capture.
macro_rules! opaque_register {
    ($($(#[$meta:meta])* $name:ident),* $(,)?) => {
        $(
            $(#[$meta])*
            #[derive(derive_more::Debug, Clone, Copy, PartialEq, Eq)]
            pub struct $name(#[debug("{}", _0.map(|b| format!("{b:02X}")).join(" "))] pub [u8; 4]);

            impl $name {
                pub fn encode(&self, dst: &mut BytesMut) {
                    dst.put_slice(&self.0);
                }
                pub fn decode(bytes: [u8; 4]) -> Self {
                    Self(bytes)
                }
            }
        )*
    };
}

opaque_register! {
    /// Ring-oscillator pad disable (0x68).
    RingOscPadDisable,
    /// On-die ADC control (0xB9).
    AdcCtrl1,
}

impl RingOscPadDisable {
    /// Returns the fixed guard pattern, the register's only
    /// observed value.
    pub fn guard_pattern() -> Self {
        Self([0x5A, 0xA5, 0x5A, 0xA5])
    }
}

impl AdcCtrl1 {
    /// Returns the bring-up value from the Bitaxe capture, written
    /// once before and once after the analog mux select.
    pub fn bring_up() -> Self {
        Self([0x00, 0x00, 0x44, 0x80])
    }
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
    fn non_contiguous_mask_round_trips() {
        // No capture shows such a readback, but decode must keep
        // the mask as received, not normalize it to a bit count.
        let bytes = [0x12, 0x34, 0x56, 0x78];
        let mut buf = BytesMut::new();
        TicketMask::decode(bytes).encode(&mut buf);
        assert_eq!(&buf[..], &bytes);
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
            unknown: 0x03,
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
            locked: false,
            pll_en: true,
            bypass: false,
            vco_sel: VcoSel::Low,
            fb_div: 0x68,
            ref_div: 0x01,
            post_div: 0x33,
        });
    }

    #[test]
    fn new_picks_vco_sel_from_resulting_vco() {
        // VCO = fb_div * crystal / ref_div. Pick targets across the
        // boundary, back-derive fb_div, and assert the select bit
        // matches the bracket. The threshold rule is `>=`, so a
        // target that hits the boundary exactly picks the high range.
        const REF_DIV: u8 = 2;
        let fb_div_for = |vco_mhz: f32| (vco_mhz * REF_DIV as f32 / CRYSTAL_MHZ) as u8;

        let cases = [
            (2000.0, VcoSel::Low),  // below
            (2400.0, VcoSel::High), // at threshold (>= picks high)
            (2800.0, VcoSel::High), // above
        ];
        for (target_vco, expected_vco_sel) in cases {
            let fb_div = fb_div_for(target_vco);
            assert_eq!(
                PllDivider::new(fb_div, REF_DIV, 0).vco_sel,
                expected_vco_sel,
                "target VCO {} MHz",
                target_vco,
            );
        }
    }
}

#[cfg(test)]
mod hash_counting_number_tests {
    use super::*;

    #[test]
    fn round_trip() {
        let original = HashCountingNumber::from(0x1EB5);
        let mut buf = BytesMut::new();
        original.encode(&mut buf);
        let bytes: [u8; 4] = buf[..].try_into().unwrap();
        assert_eq!(HashCountingNumber::decode(bytes), original);
    }

    #[test]
    fn encodes_big_endian() {
        let mut buf = BytesMut::new();
        HashCountingNumber::from(0x00001EB5).encode(&mut buf);
        assert_eq!(&buf[..], &[0x00, 0x00, 0x1E, 0xB5]);
    }
}

#[cfg(test)]
mod uart_baud_tests {
    use super::*;

    // Observed values from REFERENCE.md's UART_BAUD table.
    const RESET: [u8; 4] = [0x01, 0x30, 0x1A, 0x00];
    const BITAXE_1M: [u8; 4] = [0x11, 0x30, 0x02, 0x00];
    const S19J_PRO_3M: [u8; 4] = [0x11, 0x30, 0x00, 0x00];
    const S21_PRO_3M: [u8; 4] = [0x01, 0x30, 0x00, 0x00];

    #[test]
    fn observed_values_round_trip() {
        for bytes in [RESET, BITAXE_1M, S19J_PRO_3M, S21_PRO_3M] {
            let mut buf = BytesMut::new();
            UartBaud::decode(bytes).encode(&mut buf);
            assert_eq!(&buf[..], &bytes);
        }
    }

    #[test]
    fn decodes_reset_value() {
        let reg = UartBaud::decode(RESET);
        assert_eq!(reg.divider, 26);
        assert!(!reg.bit28);
        assert_eq!(reg.unexplained, 0x0130_0000);
    }

    #[test]
    fn derives_baud_from_divider() {
        assert_eq!(UartBaud::decode(RESET).baud(), 115_740);
        assert_eq!(UartBaud::decode(BITAXE_1M).baud(), 1_041_666);
        assert_eq!(UartBaud::decode(S21_PRO_3M).baud(), 3_125_000);
    }

    #[test]
    fn solves_divider_for_target_rate() {
        // The targets pick the dividers the captures show, above
        // the target when the nearest representable rate is above.
        assert_eq!(UartBaud::for_baud(115_200).divider, 26);
        assert_eq!(UartBaud::for_baud(1_000_000).divider, 2);
        assert_eq!(UartBaud::for_baud(3_000_000).divider, 0);
    }

    #[test]
    fn target_rate_encodes_reset_value() {
        let mut buf = BytesMut::new();
        UartBaud::for_baud(115_200).encode(&mut buf);
        assert_eq!(&buf[..], &RESET);
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
            unexplained: 0x0aa0_0000,
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
    fn reporting_enabled() {
        round_trip(MiscControl::reporting_enabled(ChipModel::BM1362));
        round_trip(MiscControl::reporting_enabled(ChipModel::BM1370));
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
        round_trip(AnalogMux {
            diode_select: 0xf,
            unexplained: 0x1234_5670,
        });
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
            unexplained: 0x5554,
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
        round_trip(CoreCommand::write_all(CoreRegister::CoreEnable, 0xaa));
    }

    #[test]
    fn from_literal_fields() {
        round_trip(CoreCommand {
            all: false,
            num: 0x55,
            core_id: 0xc3,
            write: false,
            rd_done: true,
            unexplained: 0x2000,
            reg: CoreRegister::Unknown(0x1f),
            value: 0xee,
        });
    }
}
