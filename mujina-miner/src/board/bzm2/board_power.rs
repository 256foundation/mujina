//! De-energising a hashboard through its own MCU.
//!
//! # Why this is a separate file from the sensor layer
//!
//! [`super::board_mcu::Bzm2BoardMcu`] states, and enforces, that every method
//! on it is a sensor read: "there is no constructor argument, field or method
//! that can make this emit anything but a query." That guarantee is worth more
//! than the convenience of putting the one write beside the reads, because it
//! is the reason a telemetry path can be handed to anything without wondering
//! what it might do to a 3 kW machine.
//!
//! So the actuator lives here, alone, and a caller has to reach for it by name.
//!
//! # Why it exists at all
//!
//! Until now **nothing in the shippable tree could de-energise a hashboard by
//! any route.** The bring-up rails have an enable node, but that path only
//! exists when this process performed the bring-up and only describes the
//! supply; the board's own ordered shutdown is an MCU operation and no code
//! reached it. A miner that cannot turn a board off cannot stop one it did not
//! start, cannot respond to its own thermal trip with anything but ceasing to
//! send work, and leaves the rails up when it exits.
//!
//! # The discipline
//!
//! One write, in one place, reachable only through a method whose name says
//! what it does. It verifies afterwards by reading the power status back, and
//! reports NOT the fact that it wrote but whether the rails actually fell --
//! those are different claims and the vendor daemon conflates them.

use crate::hw_trait::HwError;
use crate::hw_trait::i2c::{I2c, I2cError};

use super::board_mcu::{
    BoardMcuError, MCU_I2C_ADDRESS, OP_GET_VDD_TOTAL, PowerStatus, RAIL_DARK_MV,
};

/// Bytes an actuate writes. Longer than a query's two, and that difference is
/// the whole of the distinction between reading this MCU and commanding it.
const ACTUATE_LEN: usize = 4;

/// Command the MCU's ordered power-down of the board it sits on.
const OP_SET_POWER_DISABLE: u8 = 14;

/// The MCU's power-status word: which rails it BELIEVES it has enabled.
const OP_GET_POWER_STATUS: u8 = 17;

/// What a de-energise attempt actually achieved.
///
/// Three outcomes, not two, because "the write succeeded" and "the board is
/// off" are different claims and only the second one is the point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerDownOutcome {
    /// The write landed and the board reads dark afterwards.
    Down,
    /// The write landed and the board still reports power. The MCU accepted the
    /// command and the rails did not fall -- which is the case a caller must
    /// escalate from, not retry quietly.
    StillUp(PowerStatus),
    /// The write landed and the verify could not be read. Not proof of either
    /// state: a caller that treats this as success is guessing.
    Unverified,
    /// The MCU's flags say the board is off and its rail says it is not.
    ///
    /// The most dangerous outcome and the one a shadow read alone can never
    /// produce: the MCU believes it complied, so nothing downstream will
    /// question it, while roughly a hundred series devices remain energised.
    StillEnergised { status: PowerStatus, rail_mv: u16 },
}

/// The one thing in this driver that can turn a hashboard off.
pub struct Bzm2BoardPower<I> {
    bus: I,
}

impl<I: I2c> Bzm2BoardPower<I> {
    /// Take the bus that reaches one board's MCU.
    ///
    /// Consuming it is deliberate: a caller holding this cannot also be holding
    /// something that believes it is read-only.
    pub fn new(bus: I) -> Self {
        Self { bus }
    }

    /// Ask the MCU to bring this board down, then read back whether it did.
    ///
    /// The MCU runs its own ordered sequence -- engine power before board power
    /// -- which is why this is one command rather than a sequence assembled
    /// here. Reimplementing that ordering would be a second copy of a fact the
    /// firmware already holds, and getting it wrong drops a rail under load.
    pub async fn power_down(&mut self) -> Result<PowerDownOutcome, BoardMcuError> {
        let command: [u8; ACTUATE_LEN] = [OP_SET_POWER_DISABLE, 0, 0, 0];
        match self.bus.write(MCU_I2C_ADDRESS, &command).await {
            Ok(()) => {}
            Err(HwError::I2c(I2cError::NoAck(addr))) => {
                return Err(BoardMcuError::NoBoard(addr));
            }
            Err(err) => return Err(BoardMcuError::Transport(err)),
        }

        // VERIFY, and report what was found rather than what was asked for.
        //
        // TWO READS, AND THE SECOND ONE IS THE ONE THAT COUNTS.
        //
        // The power word is a SHADOW of the command the MCU last obeyed. An MCU
        // that accepts opcode 14, clears its own flags, and fails to bring the
        // DC-DC down reports exactly what a successful power-down reports. This
        // code used to stop there, which verified our write against our write.
        //
        // Both sibling tools on this machine already refuse that: rds-mcu and
        // the killswitch each read the rail as well, in their own words because
        // "a circular check is not a check". The rail is a real ADC conversion
        // of a real voltage and cannot be produced by the MCU merely agreeing
        // with us.
        let status = match self.read_word(OP_GET_POWER_STATUS).await {
            Some(raw) => PowerStatus::from_raw(raw),
            // The command went out; the confirmation did not come back. Saying
            // "down" here would be the exact failure this type exists to avoid.
            None => return Ok(PowerDownOutcome::Unverified),
        };

        let rail_mv = match self.read_word(OP_GET_VDD_TOTAL).await {
            Some(mv) => mv,
            // Without the witness we cannot claim dark, even if the flags agree.
            // An unread rail is unmeasured, not zero.
            None => return Ok(PowerDownOutcome::Unverified),
        };

        if status.board_power_on() || status.engine_power_on() {
            return Ok(PowerDownOutcome::StillUp(status));
        }
        if rail_mv >= RAIL_DARK_MV {
            // The flags say off and the rail says otherwise. This is the case
            // the shadow read alone could never produce, and it is the one
            // worth having the type for: the MCU believes it complied.
            return Ok(PowerDownOutcome::StillEnergised { status, rail_mv });
        }
        Ok(PowerDownOutcome::Down)
    }

    /// One two-byte query, or `None` if the bus would not answer.
    async fn read_word(&mut self, opcode: u8) -> Option<u16> {
        let selector = [opcode, 0];
        let mut reply = [0u8; 2];
        self.bus
            .write_read(MCU_I2C_ADDRESS, &selector, &mut reply)
            .await
            .ok()
            .map(|()| u16::from_le_bytes(reply))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hw_trait::Result as HwResult;
    use async_trait::async_trait;

    /// A board MCU that records what was written to it.
    ///
    /// Deliberately its own, not shared with the sensor layer's fake: that one
    /// is built to answer queries, and the property under test here is what
    /// gets WRITTEN. A harness that could not express a four-byte write would
    /// not be able to fail the test that matters.
    struct FakeBoard {
        wrote: Vec<Vec<u8>>,
        verify: Option<[u8; 2]>,
        write_nacks: bool,
        verify_fails: bool,
        power_word: Option<u16>,
        rail_mv: Option<u16>,
    }

    impl FakeBoard {
        fn answering_verify(reply: [u8; 2]) -> Self {
            Self {
                wrote: Vec::new(),
                verify: Some(reply),
                write_nacks: false,
                verify_fails: false,
                power_word: None,
                rail_mv: None,
            }
        }
        fn with_unreadable_verify() -> Self {
            Self {
                wrote: Vec::new(),
                verify: None,
                write_nacks: false,
                verify_fails: true,
                power_word: None,
                rail_mv: None,
            }
        }
        /// Answer each query on its own terms: the power-status word and the
        /// rail separately.
        ///
        /// The single-reply fake could not express the case that matters --
        /// flags saying off while the rail is still up -- because it answered
        /// every read with the same bytes. A harness that cannot state the
        /// dangerous case cannot fail on it, which is why the original test
        /// passed over code that only read the shadow.
        fn answering(power_word: u16, rail_mv: u16) -> Self {
            Self {
                wrote: Vec::new(),
                verify: None,
                write_nacks: false,
                verify_fails: false,
                power_word: Some(power_word),
                rail_mv: Some(rail_mv),
            }
        }
        fn absent() -> Self {
            Self {
                wrote: Vec::new(),
                verify: None,
                write_nacks: true,
                verify_fails: false,
                power_word: None,
                rail_mv: None,
            }
        }
    }

    #[async_trait]
    impl I2c for FakeBoard {
        async fn write(&mut self, addr: u8, data: &[u8]) -> HwResult<()> {
            self.wrote.push(data.to_vec());
            if self.write_nacks {
                return Err(HwError::I2c(I2cError::NoAck(addr)));
            }
            Ok(())
        }
        async fn read(&mut self, _addr: u8, _buffer: &mut [u8]) -> HwResult<()> {
            Ok(())
        }
        async fn write_read(&mut self, addr: u8, write: &[u8], read: &mut [u8]) -> HwResult<()> {
            self.wrote.push(write.to_vec());
            if self.verify_fails {
                return Err(HwError::I2c(I2cError::BusError));
            }
            // Per-opcode answers first, so a test can say "flags off, rail up".
            match (write.first(), self.power_word, self.rail_mv) {
                (Some(&OP_GET_POWER_STATUS), Some(w), _) => {
                    read.copy_from_slice(&w.to_le_bytes());
                    return Ok(());
                }
                (Some(&OP_GET_VDD_TOTAL), _, Some(mv)) => {
                    read.copy_from_slice(&mv.to_le_bytes());
                    return Ok(());
                }
                _ => {}
            }
            if let Some(reply) = self.verify {
                read.copy_from_slice(&reply);
                return Ok(());
            }
            Err(HwError::I2c(I2cError::NoAck(addr)))
        }
        async fn set_frequency(&mut self, _hz: u32) -> HwResult<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn flags_saying_off_while_the_rail_is_up_is_not_down() {
        // THE CASE THE SHADOW READ ALONE CANNOT PRODUCE.
        //
        // The MCU accepts opcode 14, clears its own flags, and the rail does
        // not fall. The power word is a shadow of the command it last obeyed,
        // so it reports exactly what a successful power-down reports. Before
        // the rail was read, this returned `Down` and roughly a hundred series
        // devices stayed energised while the miner believed the board was dark
        // -- it would stop dispatching work and stop reading that board's
        // temperature, which is the worst possible combination.
        let mut power = Bzm2BoardPower::new(FakeBoard::answering(0x0000, 17_500));
        match power.power_down().await.unwrap() {
            PowerDownOutcome::StillEnergised { status, rail_mv } => {
                assert!(!status.board_power_on(), "the flags did say off");
                assert_eq!(rail_mv, 17_500, "and the rail said otherwise");
            }
            other => panic!("flags-off-rail-up must not report {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_unreadable_rail_is_unverified_not_down() {
        // The flags agree, and the witness cannot be read. That is not proof,
        // and an unread rail is unmeasured rather than zero.
        let mut board = FakeBoard::answering(0x0000, 0);
        board.rail_mv = None;
        board.verify = None;
        let mut power = Bzm2BoardPower::new(board);
        assert_eq!(
            power.power_down().await.unwrap(),
            PowerDownOutcome::Unverified
        );
    }

    #[tokio::test]
    async fn power_down_writes_the_command_and_verifies_dark() {
        let mut power = Bzm2BoardPower::new(FakeBoard::answering_verify([0, 0]));
        assert_eq!(power.power_down().await.unwrap(), PowerDownOutcome::Down);
        // Four bytes, not two: the length IS the difference between reading
        // this MCU and commanding it.
        assert_eq!(power.bus.wrote[0], vec![OP_SET_POWER_DISABLE, 0, 0, 0]);
        assert_eq!(power.bus.wrote[0].len(), ACTUATE_LEN);
        // And it read the power status back rather than trusting the write.
        assert_eq!(power.bus.wrote[1], vec![17, 0]);
    }

    /// The command landing is not the rails falling, and only the second is
    /// what a caller asked for.
    #[tokio::test]
    async fn a_board_that_stays_up_is_not_reported_down() {
        // 0x0101: board flag in the low byte, engine flag in the high byte.
        let mut power = Bzm2BoardPower::new(FakeBoard::answering_verify([1, 1]));
        match power.power_down().await.unwrap() {
            PowerDownOutcome::StillUp(_) => {}
            other => panic!("a board still reporting power must not read as down: {other:?}"),
        }
    }

    /// An unreadable verify is not a success. Reporting one would be the exact
    /// failure this type exists to avoid.
    #[tokio::test]
    async fn an_unverifiable_result_says_so() {
        let mut power = Bzm2BoardPower::new(FakeBoard::with_unreadable_verify());
        assert_eq!(
            power.power_down().await.unwrap(),
            PowerDownOutcome::Unverified
        );
    }

    /// An empty slot is a fact about the machine, not a transport failure.
    #[tokio::test]
    async fn an_absent_board_is_reported_as_absent() {
        let mut power = Bzm2BoardPower::new(FakeBoard::absent());
        assert!(matches!(
            power.power_down().await,
            Err(BoardMcuError::NoBoard(_))
        ));
    }
}
