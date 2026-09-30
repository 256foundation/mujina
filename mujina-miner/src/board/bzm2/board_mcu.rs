//! Read-only sensor layer for the BZM2 hashboard MCU.
//!
//! Each hashboard carries its own small MCU on an I2C segment of its own:
//! address [`MCU_I2C_ADDRESS`] on `/dev/i2c-2`, `-3` and `-4` for boards 0, 1
//! and 2 ([`bus_path`]). The segments run at 100 kHz -- a property of each
//! adapter's driver configuration, which a userspace client can neither set
//! nor read back. There is deliberately no constant for it here: a number this
//! layer cannot establish or check would read as a setting when it is only a
//! note, and the honest home for it is this sentence.
//!
//! # Framing
//!
//! A query is one transaction: a two-byte selector `[opcode, param]` is
//! written, a repeated start follows, and two bytes come back little-endian.
//! That is exactly one [`I2c::write_read`] call, and it is the only shape of
//! traffic this module can produce.
//!
//! # Why this module cannot actuate
//!
//! The MCU's actuator commands are a different shape: they carry their operand
//! in the same write, making the write longer than a selector. This module has
//! no constant for any of them and no code path that writes more than two
//! bytes -- [`Query`] holds read opcodes only, and [`Bzm2BoardMcu::query`] is
//! the single place a transaction is issued. On the MCU side the two shapes
//! are handled by different halves of the I2C interrupt: actuation is decided
//! when a write completes, and no read opcode has a case there. A two-byte
//! selector is consumed by the reply path and changes nothing.
//!
//! The bus handle is private and this module hands out no reference to it, so
//! a caller holding a [`Bzm2BoardMcu`] cannot reach past it to write anything
//! either. `every_transaction_is_a_two_byte_query` in the tests below is the
//! standing check that this stays true.
//!
//! # Why this is safe on a freshly booted unit
//!
//! The MCU sheds board power some tens of seconds after it stops hearing a
//! heartbeat, but that timer is gated on a connection flag that only the
//! heartbeat actuator sets. A client that issues nothing but queries never
//! arms it, so it cannot start a countdown and cannot cause a shed. An unarmed
//! MCU stays unarmed no matter how long this layer polls it. (Taking over a
//! unit whose previous controller already armed the watchdog is a different
//! situation, and not one this module creates.)
//!
//! # Dark chains
//!
//! Every read here works with the hashboard rails down. The MCU is powered
//! independently -- it is what gates the rails -- so identity, thermal and
//! power-status reads answer on a board that has never been energised.
//! [`BoardState::rail_millivolts`] near zero on such a board is a valid
//! measurement meaning "the rail is down", not a failed read, and nothing in
//! this module treats it as an error.
//!
//! # What is deliberately absent
//!
//! There is no LED read. The MCU's LED commands are write-only; it keeps no
//! readable LED state, so this module offers no accessor that would have to
//! invent one.

#[cfg(test)]
use std::path::PathBuf;

use crate::hw_trait::HwError;
use crate::hw_trait::i2c::{I2c, I2cError};

/// The address every hashboard MCU answers on.
pub const MCU_I2C_ADDRESS: u8 = 0x76;

/// Total board rail in millivolts, from the MCU's own ADC.
///
/// THE NON-CIRCULAR WITNESS, and it lives here because three separate callers
/// need it for the same reason: the power-down verify, the heartbeat's check
/// that its beating is actually working, and bring-up confirming a voltage it
/// commanded through a write-only sysfs setpoint. Each of those writes through
/// one path and must confirm through another, and this is the other one — a
/// real conversion of a real voltage rather than a flag the MCU sets when it
/// agrees with us.
pub const OP_GET_VDD_TOTAL: u8 = 23;

/// Below this the rail is down. Well above the ADC's noise floor on a dark
/// board and far below any operating rail, which sits near 17.5 V.
pub const RAIL_DARK_MV: u16 = 1000;

/// Bytes a query writes, and bytes it reads back. The MCU's buffers are this
/// size in each direction; nothing larger fits through the interface.
const QUERY_LEN: usize = 2;

/// Serial number field in the EEPROM: 24 ASCII bytes from offset 0.
const EEPROM_SERIAL: std::ops::Range<u8> = 0..24;

/// ASIC part/stepping id: two bytes immediately after the serial, low byte
/// first, matching the byte order of everything else on this interface.
/// Derived from where the serial field ends rather than written out again, so
/// a change to that field's length cannot leave these two pointing into it.
const EEPROM_PART_ID_LO: u8 = EEPROM_SERIAL.end;
const EEPROM_PART_ID_HI: u8 = EEPROM_PART_ID_LO + 1;

/// The Linux adapter node carrying board `index`'s MCU.
///
/// `None` for an index no machine has.
#[cfg(test)]
pub fn bus_path(board_index: usize) -> Option<PathBuf> {
    super::platform::DEFAULT.i2c_bus_path(board_index)
}

/// A read-only handle on one hashboard's MCU.
///
/// Every method is a sensor read. There is no constructor argument, field or
/// method that can make this emit anything but a query.
pub struct Bzm2BoardMcu<I> {
    bus: I,
    fault_phase: FaultPhase,
}

impl<I: I2c> Bzm2BoardMcu<I> {
    /// Wrap a bus that reaches one board's MCU.
    ///
    /// The bus is consumed: a caller who wants to share a segment clones the
    /// backend before handing one over, and cannot get this one back.
    pub fn new(bus: I) -> Self {
        Self {
            bus,
            fault_phase: FaultPhase::Aligned,
        }
    }

    /// Everything the board reports about what it is.
    ///
    /// Costs 29 transactions, 26 of them the byte-at-a-time EEPROM walk, so
    /// this is a bring-up read rather than a polling read.
    pub async fn read_identity(&mut self) -> Result<BoardPresence<BoardIdentity>, BoardMcuError> {
        into_presence(self.identity().await)
    }

    /// One sample of what the MCU can see about board state.
    ///
    /// Costs four transactions and is safe to poll. Valid on a dark board.
    #[cfg(test)]
    pub async fn read_state(&mut self) -> Result<BoardPresence<BoardState>, BoardMcuError> {
        into_presence(self.state().await)
    }

    /// Pop one entry from the MCU's fault queue.
    ///
    /// **This is destructive and costs two transactions.** The MCU answers the
    /// fault query only on every second call: a counter advances on each one
    /// and dequeues when it reaches two. The odd call is not an error and not
    /// an empty answer -- it returns whatever is still sitting in the reply
    /// buffer, which is usually the answer to some *earlier, unrelated* query.
    /// A caller who read that as a fault would invent faults out of voltages.
    ///
    /// So both calls are made here, the first answer is discarded, and there
    /// is deliberately no single-transaction fault read to reach for. `Ok(None)`
    /// means the queue answered empty; anything returned has been removed from
    /// the MCU and exists nowhere else, so log it before dropping it.
    ///
    /// The pairing also keeps the MCU's counter on even parity. Nothing else
    /// may issue the fault query against the same MCU concurrently: a third
    /// party's call shifts the parity and this handle will then discard a real
    /// fault and return a stale word as if it were the queue's answer.
    #[cfg(test)]
    pub async fn take_fault(&mut self) -> Result<BoardPresence<Option<BoardFault>>, BoardMcuError> {
        into_presence(self.fault().await)
    }

    /// Empty the fault queue, up to `limit` entries.
    ///
    /// Two transactions per entry, plus two for the empty answer that ends it.
    /// `limit` bounds the cost against an MCU that queues faults faster than
    /// this drains them; hitting it means entries remain.
    ///
    /// Returns a [`FaultDrain`] rather than a `Result` on purpose. Each entry
    /// this has already popped is gone from the MCU and exists nowhere else,
    /// so a failure part-way through must hand back what it took as well as
    /// why it stopped. A `Result` would let a caller `?` the evidence into
    /// the bin, and the entries it discarded would be exactly the ones from
    /// the board that was misbehaving.
    pub async fn drain_faults(&mut self, limit: usize) -> FaultDrain {
        let mut faults = Vec::new();
        for _ in 0..limit {
            match self.fault().await {
                Ok(Some(fault)) => faults.push(fault),
                Ok(None) => {
                    return FaultDrain {
                        faults,
                        stopped: DrainStop::Empty,
                    };
                }
                Err(BoardMcuError::NoBoard(_)) => {
                    return FaultDrain {
                        faults,
                        stopped: DrainStop::Absent,
                    };
                }
                Err(err) => {
                    return FaultDrain {
                        faults,
                        stopped: DrainStop::Failed(err),
                    };
                }
            }
        }
        FaultDrain {
            faults,
            stopped: DrainStop::Limit,
        }
    }

    /// Declare the fault-queue counter aligned again after a failed read.
    ///
    /// A transfer that fails part-way through a fault read leaves the MCU's
    /// counter in a state this side cannot observe, so [`take_fault`] refuses
    /// to guess and returns [`BoardMcuError::FaultQueuePhaseUnknown`] instead.
    /// Calling this accepts the consequence: the next pair may consume and
    /// discard one queued fault. There is no way to recover it -- the read is
    /// the only copy -- so prefer logging the desync over clearing it quietly.
    ///
    /// [`take_fault`]: Self::take_fault
    #[cfg(test)]
    pub fn assume_fault_queue_aligned(&mut self) {
        self.fault_phase = FaultPhase::Aligned;
    }

    async fn identity(&mut self) -> Result<BoardIdentity, BoardMcuError> {
        let information = self.query(Query::Information, 0).await?;
        let stack_id = self.query(Query::StackId, 0).await?;
        let board_id = self.query(Query::BoardId, 0).await?;

        let mut serial = [0u8; EEPROM_SERIAL.end as usize - EEPROM_SERIAL.start as usize];
        for (offset, byte) in EEPROM_SERIAL.zip(serial.iter_mut()) {
            *byte = self.read_eeprom_byte(offset).await?;
        }
        let part_lo = self.read_eeprom_byte(EEPROM_PART_ID_LO).await?;
        let part_hi = self.read_eeprom_byte(EEPROM_PART_ID_HI).await?;

        // The information word packs three fields into two bytes: the low byte
        // is the firmware version outright, the high byte splits into a board
        // revision and the revision of the I2C protocol the firmware speaks.
        let revisions = (information >> 8) as u8;

        Ok(BoardIdentity {
            serial: decode_serial(&serial)?,
            asic_part_id: u16::from(part_lo) | (u16::from(part_hi) << 8),
            firmware_version: (information & 0xff) as u8,
            board_revision: revisions >> 2,
            protocol_revision: revisions & 0x03,
            board_id_straps: (board_id & 0xff) as u8,
            stack_id,
        })
    }

    #[cfg(test)]
    async fn state(&mut self) -> Result<BoardState, BoardMcuError> {
        Ok(BoardState {
            rail_millivolts: self.query(Query::VddTotal, 0).await?,
            inlet: ThermalReading(self.query(Query::PlatformThermal, THERMAL_INLET).await?),
            outlet: ThermalReading(self.query(Query::PlatformThermal, THERMAL_OUTLET).await?),
        })
    }

    async fn fault(&mut self) -> Result<Option<BoardFault>, BoardMcuError> {
        if self.fault_phase == FaultPhase::Unknown {
            return Err(BoardMcuError::FaultQueuePhaseUnknown);
        }

        // Halfway through the pair the MCU's counter is odd, and a failure now
        // leaves it somewhere this side cannot see. Mark it before the pair
        // and clear it only once both legs have landed.
        self.fault_phase = FaultPhase::Unknown;

        // Discarded on purpose: this is the reply buffer's previous contents,
        // not a fault.
        //
        // An unanswered first leg is the one failure that leaves the counter
        // provably untouched: nothing acknowledged, so nothing counted. It,
        // and only it, restores alignment. Once this leg has landed the
        // counter is odd, and a second leg that goes unanswered leaves it
        // there -- "no board" says nothing about the counter by then, so the
        // phase stays unknown however the second failure is spelled.
        let _stale = self.query(Query::Error, 0).await.inspect_err(|err| {
            if matches!(err, BoardMcuError::NoBoard(_)) {
                self.fault_phase = FaultPhase::Aligned;
            }
        })?;

        let popped = self.query(Query::Error, 0).await?;
        self.fault_phase = FaultPhase::Aligned;

        Ok(BoardFault::from_word(popped))
    }

    async fn read_eeprom_byte(&mut self, offset: u8) -> Result<u8, BoardMcuError> {
        // The EEPROM query is addressed a byte at a time and answers in the
        // low half of the two-byte reply; the high half carries nothing.
        Ok((self.query(Query::Eeprom, offset).await? & 0xff) as u8)
    }

    /// Issue one query and decode its reply.
    ///
    /// The only place this module touches the bus. The selector is two bytes
    /// and the reply is two bytes, always: there is no branch here that can
    /// lengthen the write.
    async fn query(&mut self, opcode: Query, param: u8) -> Result<u16, BoardMcuError> {
        let selector: [u8; QUERY_LEN] = [opcode as u8, param];
        let mut reply = [0u8; QUERY_LEN];

        match self
            .bus
            .write_read(MCU_I2C_ADDRESS, &selector, &mut reply)
            .await
        {
            Ok(()) => Ok(u16::from_le_bytes(reply)),
            // Nothing acknowledged: the slot is empty. That is a fact about
            // the machine, not a transport failure, and the caller decides.
            Err(HwError::I2c(I2cError::NoAck(addr))) => Err(BoardMcuError::NoBoard(addr)),
            Err(err) => Err(BoardMcuError::Transport(err)),
        }
    }
}

/// What a read of a board slot found.
///
/// Distinguishing "empty slot" from "error" in the type keeps a two-board
/// machine from reading as a three-board machine with a fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardPresence<T> {
    /// The MCU answered.
    Present(T),
    /// Nothing acknowledged at the MCU address: no board is fitted here.
    Absent,
}

impl<T> BoardPresence<T> {
    /// The value, if a board answered.
    pub fn present(self) -> Option<T> {
        match self {
            Self::Present(value) => Some(value),
            Self::Absent => None,
        }
    }

    /// Whether the slot is empty.
    #[cfg(test)]
    pub fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }
}

/// What a board reports about what it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardIdentity {
    /// Serial number from the EEPROM, padding trimmed. `None` when the field
    /// is blank, which is what an unprogrammed board looks like.
    pub serial: Option<String>,
    /// ASIC part and stepping id from the EEPROM.
    pub asic_part_id: u16,
    /// The MCU's own firmware version.
    pub firmware_version: u8,
    /// Board revision as the MCU firmware reports it.
    pub board_revision: u8,
    /// Revision of the I2C command protocol the firmware speaks. A value this
    /// module has not been validated against is a reason to distrust the
    /// framing, not to carry on.
    pub protocol_revision: u8,
    /// Board revision straps, read from the pins rather than from firmware.
    ///
    /// Deliberately kept separate from [`board_revision`]: they are two
    /// independent measurements of the same thing, and a disagreement between
    /// them is evidence (a reflashed MCU, a wrong image, a misread strap)
    /// rather than something to average away. The mapping between the two has
    /// not been established on our own hardware, so nothing here derives one
    /// from the other.
    ///
    /// [`board_revision`]: Self::board_revision
    pub board_id_straps: u8,
    /// The board's position identifier in the stack.
    pub stack_id: u16,
}

/// One sample of board state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(test)]
pub struct BoardState {
    /// Board rail voltage in millivolts.
    ///
    /// Near zero is a measurement, not a failure: it is what a board with its
    /// rails down reports, and the MCU answers this query in that state
    /// because it runs from its own supply.
    pub rail_millivolts: u16,
    /// Inlet platform thermal channel.
    pub inlet: ThermalReading,
    /// Outlet platform thermal channel.
    pub outlet: ThermalReading,
}

/// One platform thermal channel, as the MCU reports it.
///
/// Kept as the raw count. The count-to-degrees scale has not been measured
/// against a reference on our own hardware, so this type offers no conversion:
/// a guessed scale would print as a temperature and read as a measurement.
/// Give it an `as_temperature` only once a calibration exists to cite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThermalReading(u16);

impl ThermalReading {
    /// Build one from the word the MCU returned.
    pub fn from_raw(word: u16) -> Self {
        Self(word)
    }

    /// The raw count.
    #[cfg(test)]
    pub fn raw(self) -> u16 {
        self.0
    }

    /// Degrees Celsius. THE SCALE IS MEASURED, NOT ASSUMED.
    ///
    /// The MCU reports centidegrees: one hundred counts per degree, no offset
    /// and no gain error. Established by putting this instrument and a second
    /// one side by side across a full power-on and calibration,
    /// measured on hardware:
    ///
    ///   inlet    27 matched reads, median AND worst residual 0.0000 degC
    ///   outlet   26 matched reads, worst 0.4342 degC, over 21.5..58.1 degC
    ///   fit      degC = 0.010000 * raw, R^2 1.000000 on the inlet
    ///
    /// The 50-count granularity of the readings is the sensor's own 0.5 degC
    /// step, not a quantisation of this conversion.
    ///
    /// **Re-run the agreement tool per unit.** The scale is a property of this
    /// MCU and a second machine is not assumed to share it -- which is what
    /// that tool exists for.
    pub fn degrees_c(self) -> f32 {
        f32::from(self.0) * CENTIDEGREES_PER_COUNT
    }
}

/// Measured on hardware; see [`ThermalReading::degrees_c`] for the evidence.
const CENTIDEGREES_PER_COUNT: f32 = 0.01;

/// One entry popped from the MCU's fault queue.
///
/// Popping removes it from the MCU, so this value is the only copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoardFault {
    /// What kind of fault the MCU recorded.
    pub error_type: u8,
    /// Which thing on the board it concerns; the meaning depends on
    /// [`error_type`](Self::error_type).
    pub id: u8,
}

impl BoardFault {
    /// Decode a popped fault word, or `None` if the queue answered empty.
    fn from_word(word: u16) -> Option<Self> {
        let error_type = (word & 0xff) as u8;
        (error_type != NO_FAULT).then_some(Self {
            error_type,
            id: (word >> 8) as u8,
        })
    }
}

/// Error type the MCU returns when its fault queue has nothing to give.
const NO_FAULT: u8 = 0;

/// What a drain took off the MCU, and why it stopped.
///
/// `#[must_use]` for the same reason [`Bzm2BoardMcu::drain_faults`] does not
/// return a `Result`: the entries inside have already been removed from the
/// MCU and exist nowhere else, so dropping this value on the floor destroys
/// the only copy. The compiler is the cheapest place to catch that.
#[derive(Debug)]
#[must_use = "the popped faults are the only copy; log them before dropping this"]
pub struct FaultDrain {
    /// Entries removed from the MCU, oldest first. Popping is destructive:
    /// these exist nowhere else, whatever `stopped` says.
    pub faults: Vec<BoardFault>,
    /// Why the drain ended.
    pub stopped: DrainStop,
}

impl FaultDrain {
    /// Whether the queue was emptied rather than cut short.
    ///
    /// False means entries may remain on the MCU, and for
    /// [`DrainStop::Failed`] it also means the counter parity is in doubt.
    #[cfg(test)]
    pub fn is_complete(&self) -> bool {
        matches!(self.stopped, DrainStop::Empty)
    }
}

/// Why a drain ended.
#[derive(Debug)]
pub enum DrainStop {
    /// The queue answered empty; nothing remains.
    Empty,
    /// The caller's limit was reached; entries may remain.
    Limit,
    /// Nothing acknowledged at the MCU address.
    Absent,
    /// A read failed. Entries taken before the failure come back alongside it.
    Failed(BoardMcuError),
}

/// Failures of this layer.
#[derive(Debug, thiserror::Error)]
pub enum BoardMcuError {
    /// Nothing acknowledged at the MCU address.
    ///
    /// Reached the caller only through [`BoardPresence::Absent`]; the variant
    /// exists so the internal path can tell an empty slot from a bus fault.
    #[error("no board MCU acknowledged at I2C address 0x{0:02x}")]
    NoBoard(u8),

    /// The bus itself failed.
    #[error("board MCU transport failure: {0}")]
    Transport(#[source] HwError),

    /// The EEPROM serial field holds something that is not text.
    #[error("EEPROM serial byte {offset} is 0x{byte:02x}, which is not printable ASCII")]
    MalformedSerial { offset: usize, byte: u8 },

    /// A fault read failed part-way through its pair.
    #[error(
        "fault queue counter parity is unknown after a failed read; \
         call assume_fault_queue_aligned() to continue, accepting that \
         one queued fault may be consumed and discarded"
    )]
    FaultQueuePhaseUnknown,
}

/// The read opcodes, and only the read opcodes.
///
/// Every actuator opcode the MCU implements is absent by construction. Adding
/// one would not merely widen this enum, it would give the module a capability
/// the rest of it is built to refuse -- see the module documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Query {
    /// MCU firmware version, board revision and protocol revision.
    Information = 1,
    /// One platform thermal channel, selected by param.
    PlatformThermal = 6,
    /// Pop one entry from the fault queue. See `Bzm2BoardMcu::take_fault`.
    Error = 7,
    /// The board's position identifier in the stack.
    StackId = 18,
    /// Board rail voltage, millivolts.
    #[cfg(test)]
    VddTotal = 23,
    /// One EEPROM byte, addressed by param.
    Eeprom = 41,
    /// Board revision straps.
    BoardId = 44,
}

/// `PlatformThermal` param selecting the inlet channel.
/// `Query::PlatformThermal` as a bare opcode, for the heartbeat's own
/// read path -- it owns the bus and does not go through `Bzm2BoardMcu`.
pub(super) const OP_GET_PLATFORM_THERMAL: u8 = Query::PlatformThermal as u8;

pub(super) const THERMAL_INLET: u8 = 0;
/// `PlatformThermal` param selecting the outlet channel.
pub(super) const THERMAL_OUTLET: u8 = 1;

/// Whether the MCU's fault-queue counter is where this handle thinks it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FaultPhase {
    /// An even number of fault queries have been issued: the next one primes.
    Aligned,
    /// A fault read failed mid-pair; the counter's parity is unobservable.
    Unknown,
}

/// Turn "nothing answered" into a presence outcome and leave real failures
/// alone, so an empty slot never ends a run.
fn into_presence<T>(result: Result<T, BoardMcuError>) -> Result<BoardPresence<T>, BoardMcuError> {
    match result {
        Ok(value) => Ok(BoardPresence::Present(value)),
        Err(BoardMcuError::NoBoard(_)) => Ok(BoardPresence::Absent),
        Err(err) => Err(err),
    }
}

/// Trim the EEPROM serial field and check what is left is text.
///
/// Unprogrammed EEPROM reads as all ones or all zeros; either is blank rather
/// than malformed. A field that is part text and part garbage is a bad read,
/// and says so.
fn decode_serial(field: &[u8]) -> Result<Option<String>, BoardMcuError> {
    let trimmed = field
        .iter()
        .rposition(|byte| !matches!(byte, 0x00 | 0xff | b' '))
        .map(|last| &field[..=last])
        .unwrap_or(&[]);

    if trimmed.is_empty() {
        return Ok(None);
    }
    if let Some(offset) = trimmed.iter().position(|byte| !byte.is_ascii_graphic()) {
        return Err(BoardMcuError::MalformedSerial {
            offset,
            byte: trimmed[offset],
        });
    }

    Ok(Some(String::from_utf8_lossy(trimmed).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hw_trait::Result as HwResult;
    use async_trait::async_trait;
    use std::collections::HashMap;

    /// Opcodes this layer is allowed to emit, spelled out here rather than
    /// taken from [`Query`] so that a change to the enum fails this test
    /// instead of agreeing with it.
    const ALLOWED_OPCODES: [u8; 8] = [1, 6, 7, 17, 18, 23, 41, 44];

    #[tokio::test]
    async fn information_splits_into_firmware_board_and_protocol_revisions() {
        // 0xad == 0b1010_1101 -> revision 0b101011 (43), protocol 0b01 (1).
        let mut mcu = Bzm2BoardMcu::new(FakeMcu::new().answering(1, 0, [0x2a, 0xad]).answering(
            44,
            0,
            [0x07, 0x00],
        ));

        let identity = mcu.read_identity().await.unwrap().present().unwrap();

        assert_eq!(identity.firmware_version, 42);
        assert_eq!(identity.board_revision, 43);
        assert_eq!(identity.protocol_revision, 1);
        assert_eq!(identity.board_id_straps, 7);
    }

    #[tokio::test]
    async fn platform_thermal_selects_inlet_and_outlet_by_param() {
        let mut mcu = Bzm2BoardMcu::new(FakeMcu::new().answering(6, 0, [0x10, 0x00]).answering(
            6,
            1,
            [0x20, 0x00],
        ));

        let state = mcu.read_state().await.unwrap().present().unwrap();

        assert_eq!(state.inlet.raw(), 16);
        assert_eq!(state.outlet.raw(), 32);
        assert_eq!(mcu.bus.params_for(6), vec![0, 1]);
    }

    #[tokio::test]
    async fn a_fault_read_discards_the_stale_reply_and_returns_the_popped_entry() {
        let mut mcu = Bzm2BoardMcu::new(
            FakeMcu::new()
                .answering(23, 0, [0xef, 0xbe])
                .with_queued_fault([0x05, 0x03]),
        );

        // Leaves 0xbeef in the MCU's reply buffer, which the first fault
        // transaction will hand back.
        mcu.read_state().await.unwrap();
        let before = mcu.bus.log.len();

        let fault = mcu.take_fault().await.unwrap().present().unwrap();

        assert_eq!(
            fault,
            Some(BoardFault {
                error_type: 5,
                id: 3
            })
        );
        assert_eq!(
            mcu.bus.log.len() - before,
            2,
            "one fault costs two transactions"
        );
        assert_eq!(mcu.bus.params_for(7).len(), 2);
    }

    #[tokio::test]
    async fn an_empty_fault_queue_still_costs_two_transactions() {
        let mut mcu = Bzm2BoardMcu::new(FakeMcu::new());

        let fault = mcu.take_fault().await.unwrap().present().unwrap();

        assert_eq!(fault, None);
        assert_eq!(mcu.bus.log.len(), 2);
    }

    #[tokio::test]
    async fn draining_costs_two_transactions_per_entry_plus_two_to_end() {
        let mut mcu = Bzm2BoardMcu::new(
            FakeMcu::new()
                .with_queued_fault([0x11, 0x01])
                .with_queued_fault([0x22, 0x02]),
        );

        let drain = mcu.drain_faults(8).await;

        assert!(drain.is_complete());
        assert_eq!(
            drain.faults,
            vec![
                BoardFault {
                    error_type: 0x11,
                    id: 1
                },
                BoardFault {
                    error_type: 0x22,
                    id: 2
                },
            ]
        );
        assert_eq!(mcu.bus.log.len(), 6);
    }

    #[tokio::test]
    async fn draining_stops_at_its_limit() {
        let mut mcu = Bzm2BoardMcu::new(
            FakeMcu::new()
                .with_queued_fault([0x11, 0x01])
                .with_queued_fault([0x22, 0x02]),
        );

        let drain = mcu.drain_faults(1).await;

        assert_eq!(drain.faults.len(), 1);
        assert!(matches!(drain.stopped, DrainStop::Limit));
        assert!(!drain.is_complete(), "one entry is still queued");
        assert_eq!(mcu.bus.log.len(), 2);
    }

    #[tokio::test]
    async fn a_failure_mid_pair_blocks_further_fault_reads_until_acknowledged() {
        let mut mcu = Bzm2BoardMcu::new(FakeMcu::new().failing_transaction(2));

        let first = mcu.take_fault().await;
        assert!(
            matches!(first, Err(BoardMcuError::Transport(_))),
            "got {first:?}"
        );

        let second = mcu.take_fault().await;
        assert!(
            matches!(second, Err(BoardMcuError::FaultQueuePhaseUnknown)),
            "a desynchronised counter must not silently eat the next fault: got {second:?}"
        );

        mcu.assume_fault_queue_aligned();
        assert!(mcu.take_fault().await.is_ok());
    }

    #[tokio::test]
    async fn a_board_that_goes_quiet_mid_pair_leaves_the_counter_in_doubt() {
        // The first leg lands, so the MCU counted it and its counter is now
        // odd; the second goes unanswered. "Nothing acknowledged" only proves
        // the counter is untouched when it is the FIRST leg that went
        // unanswered -- here it does not, and claiming alignment would make
        // the next pair discard a real fault as if it were a stale reply.
        let mut mcu = Bzm2BoardMcu::new(
            FakeMcu::new()
                .with_queued_fault([0x11, 0x01])
                .nacking_from(2),
        );

        assert!(mcu.take_fault().await.unwrap().is_absent());

        let next = mcu.take_fault().await;
        assert!(
            matches!(next, Err(BoardMcuError::FaultQueuePhaseUnknown)),
            "a pair cut short after its first leg must not claim alignment: got {next:?}"
        );
    }

    #[tokio::test]
    async fn the_serial_is_assembled_from_twenty_four_single_byte_reads() {
        let mut fake = FakeMcu::new();
        for (offset, byte) in b"BZM2-0007".iter().enumerate() {
            fake = fake.answering(41, offset as u8, [*byte, 0x00]);
        }
        fake = fake
            .answering(41, 24, [0x12, 0x00])
            .answering(41, 25, [0x34, 0x00]);
        let mut mcu = Bzm2BoardMcu::new(fake);

        let identity = mcu.read_identity().await.unwrap().present().unwrap();

        assert_eq!(identity.serial.as_deref(), Some("BZM2-0007"));
        assert_eq!(identity.asic_part_id, 0x3412);
        assert_eq!(
            mcu.bus.params_for(41),
            (0u8..=25).collect::<Vec<_>>(),
            "every EEPROM byte is addressed individually, in order"
        );
        assert_eq!(
            mcu.bus.log.len(),
            29,
            "an identity read costs the transactions read_identity documents"
        );
    }

    #[tokio::test]
    async fn a_blank_eeprom_reports_no_serial_rather_than_a_blank_one() {
        let mut fake = FakeMcu::new();
        for offset in 0u8..24 {
            fake = fake.answering(41, offset, [0xff, 0x00]);
        }
        let mut mcu = Bzm2BoardMcu::new(fake);

        let identity = mcu.read_identity().await.unwrap().present().unwrap();

        assert_eq!(identity.serial, None);
    }

    #[tokio::test]
    async fn a_serial_with_a_non_text_byte_is_an_error() {
        let mut fake = FakeMcu::new();
        for (offset, byte) in b"BZM2\x01007".iter().enumerate() {
            fake = fake.answering(41, offset as u8, [*byte, 0x00]);
        }
        let mut mcu = Bzm2BoardMcu::new(fake);

        let err = mcu.read_identity().await.unwrap_err();

        assert!(
            matches!(err, BoardMcuError::MalformedSerial { offset: 4, byte: 1 }),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn a_drain_that_fails_still_returns_what_it_already_popped() {
        // Fails on the third transaction, the first half of the second
        // entry's pair -- after the first entry has already been popped and
        // removed from the MCU for good.
        let mut mcu = Bzm2BoardMcu::new(
            FakeMcu::new()
                .with_queued_fault([0x11, 0x01])
                .with_queued_fault([0x22, 0x02])
                .failing_transaction(3),
        );

        let drain = mcu.drain_faults(8).await;

        assert_eq!(
            drain.faults,
            vec![BoardFault {
                error_type: 0x11,
                id: 1
            }],
            "the popped entry is the only copy and must survive the failure"
        );
        match drain.stopped {
            DrainStop::Failed(err) => {
                assert!(matches!(err, BoardMcuError::Transport(_)), "got {err:?}");
            }
            other => panic!("expected the drain to report its failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_absent_board_is_absent_and_not_an_error() {
        let mut mcu = Bzm2BoardMcu::new(FakeMcu::new().absent());

        assert!(mcu.read_identity().await.unwrap().is_absent());
        assert!(mcu.read_state().await.unwrap().is_absent());
        assert!(mcu.take_fault().await.unwrap().is_absent());

        let drain = mcu.drain_faults(4).await;
        assert!(matches!(drain.stopped, DrainStop::Absent));
        assert!(drain.faults.is_empty());
    }

    #[tokio::test]
    async fn an_absent_board_leaves_the_fault_counter_alone() {
        let mut mcu = Bzm2BoardMcu::new(FakeMcu::new().absent());

        assert!(mcu.take_fault().await.unwrap().is_absent());
        // An MCU that never answered has no counter to desynchronise, so the
        // next attempt must not be blocked on a phase question.
        assert!(mcu.take_fault().await.unwrap().is_absent());
    }

    #[tokio::test]
    async fn every_transaction_is_a_two_byte_query() {
        let mut mcu = Bzm2BoardMcu::new(FakeMcu::new().with_queued_fault([0x09, 0x01]));

        mcu.read_identity().await.unwrap();
        mcu.read_state().await.unwrap();
        mcu.take_fault().await.unwrap();
        let _drain = mcu.drain_faults(4).await;

        assert!(!mcu.bus.log.is_empty());
        for transaction in &mcu.bus.log {
            let Transaction::WriteRead {
                addr,
                wrote,
                read_len,
            } = transaction
            else {
                panic!("this layer must never issue a bare write or read: {transaction:?}");
            };
            assert_eq!(*addr, 0x76);
            assert_eq!(
                wrote.len(),
                2,
                "an actuator write is longer than a selector: {wrote:?}"
            );
            assert_eq!(*read_len, 2);
            assert!(
                ALLOWED_OPCODES.contains(&wrote[0]),
                "opcode {} is not a read opcode",
                wrote[0]
            );
        }
    }

    #[test]
    fn boards_map_onto_consecutive_adapters() {
        assert_eq!(bus_path(0).unwrap().to_str(), Some("/dev/i2c-2"));
        assert_eq!(bus_path(1).unwrap().to_str(), Some("/dev/i2c-3"));
        assert_eq!(bus_path(2).unwrap().to_str(), Some("/dev/i2c-4"));
        assert_eq!(bus_path(3), None);
    }

    /// What the fake saw on the wire.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Transaction {
        Write {
            addr: u8,
            wrote: Vec<u8>,
        },
        Read {
            addr: u8,
            read_len: usize,
        },
        WriteRead {
            addr: u8,
            wrote: Vec<u8>,
            read_len: usize,
        },
    }

    /// A stand-in for one hashboard MCU.
    ///
    /// Answers are raw wire bytes in the order they arrive, so a test that
    /// asserts a decoded value is asserting the byte order too.
    struct FakeMcu {
        log: Vec<Transaction>,
        answers: HashMap<(u8, u8), [u8; 2]>,
        faults: Vec<[u8; 2]>,
        /// Fault queries seen since the last pop.
        error_calls: u8,
        /// The reply buffer's contents, which the odd fault query returns.
        tx_buffer: [u8; 2],
        absent: bool,
        fail_on: Option<usize>,
        nack_on: Option<usize>,
    }

    impl FakeMcu {
        fn new() -> Self {
            Self {
                log: Vec::new(),
                answers: HashMap::new(),
                faults: Vec::new(),
                error_calls: 0,
                tx_buffer: [0, 0],
                absent: false,
                fail_on: None,
                nack_on: None,
            }
        }

        fn answering(mut self, opcode: u8, param: u8, reply: [u8; 2]) -> Self {
            self.answers.insert((opcode, param), reply);
            self
        }

        fn with_queued_fault(mut self, entry: [u8; 2]) -> Self {
            self.faults.push(entry);
            self
        }

        fn absent(mut self) -> Self {
            self.absent = true;
            self
        }

        /// Fail the nth transaction, counting from one.
        fn failing_transaction(mut self, nth: usize) -> Self {
            self.fail_on = Some(nth);
            self
        }

        /// Stop acknowledging from the nth transaction on, counting from one.
        /// A board that answers and then goes quiet, which is what a marginal
        /// connector or an MCU reset looks like from this side.
        fn nacking_from(mut self, nth: usize) -> Self {
            self.nack_on = Some(nth);
            self
        }

        /// Params seen for one opcode, in order.
        fn params_for(&self, opcode: u8) -> Vec<u8> {
            self.log
                .iter()
                .filter_map(|transaction| match transaction {
                    Transaction::WriteRead { wrote, .. } if wrote[0] == opcode => Some(wrote[1]),
                    _ => None,
                })
                .collect()
        }

        fn answer(&mut self, opcode: u8, param: u8) -> [u8; 2] {
            let reply = if opcode == 7 {
                // The counter advances on every fault query and only dequeues
                // on the second; the odd one hands back the reply buffer.
                self.error_calls += 1;
                if self.error_calls == 2 {
                    self.error_calls = 0;
                    if self.faults.is_empty() {
                        [0, 0]
                    } else {
                        self.faults.remove(0)
                    }
                } else {
                    self.tx_buffer
                }
            } else {
                self.answers
                    .get(&(opcode, param))
                    .copied()
                    .unwrap_or([0, 0])
            };
            self.tx_buffer = reply;
            reply
        }
    }

    #[async_trait]
    impl I2c for FakeMcu {
        async fn write(&mut self, addr: u8, data: &[u8]) -> HwResult<()> {
            self.log.push(Transaction::Write {
                addr,
                wrote: data.to_vec(),
            });
            Ok(())
        }

        async fn read(&mut self, addr: u8, buffer: &mut [u8]) -> HwResult<()> {
            self.log.push(Transaction::Read {
                addr,
                read_len: buffer.len(),
            });
            Ok(())
        }

        async fn write_read(&mut self, addr: u8, write: &[u8], read: &mut [u8]) -> HwResult<()> {
            self.log.push(Transaction::WriteRead {
                addr,
                wrote: write.to_vec(),
                read_len: read.len(),
            });

            if self.absent || self.nack_on.is_some_and(|nth| self.log.len() >= nth) {
                return Err(HwError::I2c(I2cError::NoAck(addr)));
            }
            if self.fail_on == Some(self.log.len()) {
                return Err(HwError::I2c(I2cError::BusError));
            }

            let reply = self.answer(write[0], write[1]);
            read.copy_from_slice(&reply);
            Ok(())
        }

        async fn set_frequency(&mut self, _hz: u32) -> HwResult<()> {
            Ok(())
        }
    }
}
