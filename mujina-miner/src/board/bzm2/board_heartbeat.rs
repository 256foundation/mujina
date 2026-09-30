//! The heartbeat that keeps a hashboard energised — and arms the watchdog that
//! de-energises it.
//!
//! # This is the one actuator whose SUCCESS creates an obligation
//!
//! Every other command here is complete when it returns. This one is not: the
//! board MCU runs a shed timer that cuts board power when it stops hearing a
//! fresh heartbeat, and that timer is **gated on a connection flag which only a
//! heartbeat sets**. A client that issues nothing but queries never arms it, so
//! it cannot start a countdown and cannot cause a shed — which is why
//! [`super::board_mcu`] is safe to poll indefinitely on a freshly booted unit.
//!
//! Sending one heartbeat changes that. From the first beat the board is on a
//! deadline, and the caller has taken on the duty of feeding it.
//!
//! **That is the failure direction we want.** A miner that wedges, is killed,
//! loses its host, or is stopped by a debugger stops beating, and the board
//! goes dark on its own a short time later without anything on the controller
//! having to notice. It is the same shape as a dead-man's handle, and it is
//! strictly better than the alternative we have today, where a wedged
//! controller leaves a powered board hashing with nobody watching it.
//!
//! But it is only safe if the beating is genuinely continuous. Arm this and
//! then block the thread it lives on, and the board drops mid-work.
//!
//! # Why the value has to change
//!
//! The MCU does not check that a write arrived; it checks that the value is
//! **new**. Re-sending the same number is indistinguishable from a sender that
//! has stopped, which is the whole point — a stuck task repeating its last
//! output must not read as liveness. So the rolling value advances on every
//! beat and wraps, and nothing may send a fixed one.
//!
//! # What is deliberately absent
//!
//! There is no `disarm`. Once the connection flag is set the MCU's timer is
//! the authority, and a software "never mind" would be a second mechanism
//! claiming to control a shed that the firmware already owns. To bring a board
//! down deliberately, command it down ([`super::board_power`]) rather than
//! withdrawing the heartbeat and waiting.

use crate::hw_trait::HwError;
use crate::hw_trait::i2c::{I2c, I2cError};

use super::board_mcu::{
    BoardMcuError, MCU_I2C_ADDRESS, OP_GET_PLATFORM_THERMAL, OP_GET_VDD_TOTAL, RAIL_DARK_MV,
    THERMAL_INLET, THERMAL_OUTLET, ThermalReading,
};

/// Bytes an actuate writes: opcode, argument, then a little-endian word.
const ACTUATE_LEN: usize = 4;

/// Tell the MCU the controller is still here.
const OP_SYNC_HEARTBEAT: u8 = 50;

/// The low byte of the heartbeat word: the connection flag.
///
/// Setting it is what ARMS the MCU's shed timer. It is a constant rather than a
/// parameter because there is no caller that should be able to beat without
/// arming: a heartbeat that does not arm is a write with no effect, and offering
/// it would only create a way to believe the board is protected when it is not.
const CONNECTION_ARMED: u8 = 1;

/// Keeps one board's MCU satisfied that a controller is still alive.
pub struct Bzm2BoardHeartbeat<I> {
    bus: I,
    /// Advances on every beat and wraps. Starts at zero so the first value sent
    /// is one, which is distinguishable from an MCU that has seen nothing.
    rolling: u8,
    beats: u64,
    /// Whether this board's rail has EVER been seen up while we were beating.
    ///
    /// Without it a board that was dark before we started looks identical to
    /// one we allowed to shed, and only the second is a failure of ours.
    seen_energised: bool,
}

impl<I: I2c> Bzm2BoardHeartbeat<I> {
    /// Take the bus that reaches one board's MCU.
    ///
    /// Constructing this arms nothing. The obligation begins at the first
    /// [`Self::beat`].
    pub fn new(bus: I) -> Self {
        Self {
            bus,
            rolling: 0,
            beats: 0,
            seen_energised: false,
        }
    }

    /// Beats sent since construction. The number a supervisor should watch for
    /// advancement, rather than trusting that this task is still scheduled.
    pub fn beats(&self) -> u64 {
        self.beats
    }

    /// The value the next beat will carry. Exposed for tests and diagnostics.
    #[cfg(test)]
    pub fn next_value(&self) -> u8 {
        self.rolling.wrapping_add(1)
    }

    /// Read the board rail, and judge it against what we have seen before.
    ///
    /// Returns `Some(false)` only for the case that matters: a board that was
    /// energised while we were beating and is not any more. A board that has
    /// never been up yields `Some(true)` -- we are beating at something that
    /// was already dark, which is not a protection failure -- and an unreadable
    /// rail yields `None`, because unmeasured is not the same as down.
    pub async fn verify_still_energised(&mut self) -> Option<bool> {
        let selector = [OP_GET_VDD_TOTAL, 0];
        let mut reply = [0u8; 2];
        let mv = self
            .bus
            .write_read(MCU_I2C_ADDRESS, &selector, &mut reply)
            .await
            .ok()
            .map(|()| u16::from_le_bytes(reply))?;
        if mv >= RAIL_DARK_MV {
            self.seen_energised = true;
            return Some(true);
        }
        Some(!self.seen_energised)
    }

    /// Read this board's inlet and outlet platform thermals, in degrees C.
    ///
    /// Lives on the heartbeat because the heartbeat already owns this board's
    /// MCU and already talks to it on a measured-safe cadence. A second task
    /// polling the same segment is the contention that shed a board on
    /// hardware; one owner per board's MCU is the
    /// design, not an accident of where the code sits.
    ///
    /// `None` per channel is a read that did not answer -- UNMEASURED, never
    /// "cool". The caller must not publish a value it did not get.
    pub async fn read_thermals(&mut self) -> (Option<f32>, Option<f32>) {
        let inlet = self.thermal_channel(THERMAL_INLET).await;
        let outlet = self.thermal_channel(THERMAL_OUTLET).await;
        (inlet, outlet)
    }

    async fn thermal_channel(&mut self, param: u8) -> Option<f32> {
        let selector = [OP_GET_PLATFORM_THERMAL, param];
        let mut reply = [0u8; 2];
        self.bus
            .write_read(MCU_I2C_ADDRESS, &selector, &mut reply)
            .await
            .ok()
            .map(|()| ThermalReading::from_raw(u16::from_le_bytes(reply)).degrees_c())
    }

    /// Send one heartbeat, advancing the rolling value.
    ///
    /// The counter advances only on a write the transport accepted, so a run of
    /// failures does not silently consume the value space and leave the MCU
    /// seeing a jump when the bus recovers.
    pub async fn beat(&mut self) -> Result<u8, BoardMcuError> {
        let value = self.rolling.wrapping_add(1);
        // Word is little-endian on the wire: the connection flag occupies the
        // low byte and the rolling value the high one.
        let command: [u8; ACTUATE_LEN] = [OP_SYNC_HEARTBEAT, 0, CONNECTION_ARMED, value];
        match self.bus.write(MCU_I2C_ADDRESS, &command).await {
            Ok(()) => {
                self.rolling = value;
                self.beats = self.beats.saturating_add(1);
                Ok(value)
            }
            Err(HwError::I2c(I2cError::NoAck(addr))) => Err(BoardMcuError::NoBoard(addr)),
            Err(err) => Err(BoardMcuError::Transport(err)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hw_trait::Result as HwResult;
    use async_trait::async_trait;

    /// Records what was written. The property under test is the bytes.
    struct FakeBoard {
        wrote: Vec<Vec<u8>>,
        nack_after: Option<usize>,
        /// Millivolts the rail answers with, or None to make it unreadable.
        rail_mv: Option<u16>,
    }

    impl FakeBoard {
        fn new() -> Self {
            Self {
                wrote: Vec::new(),
                nack_after: None,
                rail_mv: Some(0),
            }
        }
        fn with_rail(mv: Option<u16>) -> Self {
            Self {
                wrote: Vec::new(),
                nack_after: None,
                rail_mv: mv,
            }
        }
        fn nacking_after(n: usize) -> Self {
            Self {
                wrote: Vec::new(),
                nack_after: Some(n),
                rail_mv: Some(0),
            }
        }
    }

    #[async_trait]
    impl I2c for FakeBoard {
        async fn write(&mut self, _addr: u8, bytes: &[u8]) -> HwResult<()> {
            if let Some(n) = self.nack_after {
                if self.wrote.len() >= n {
                    return Err(HwError::I2c(I2cError::NoAck(MCU_I2C_ADDRESS)));
                }
            }
            self.wrote.push(bytes.to_vec());
            Ok(())
        }

        async fn read(&mut self, _addr: u8, _buf: &mut [u8]) -> HwResult<()> {
            unreachable!("a heartbeat never reads")
        }

        async fn write_read(&mut self, _a: u8, _w: &[u8], r: &mut [u8]) -> HwResult<()> {
            match self.rail_mv {
                Some(mv) => {
                    r.copy_from_slice(&mv.to_le_bytes());
                    Ok(())
                }
                None => Err(HwError::I2c(I2cError::BusError)),
            }
        }

        async fn set_frequency(&mut self, _hz: u32) -> HwResult<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_beat_arms_the_shed_timer_and_carries_a_fresh_value() {
        let mut hb = Bzm2BoardHeartbeat::new(FakeBoard::new());
        assert_eq!(hb.beat().await.unwrap(), 1);
        assert_eq!(hb.beat().await.unwrap(), 2);

        let wrote = &hb.bus.wrote;
        assert_eq!(wrote.len(), 2);
        // Opcode, argument, connection flag, rolling value.
        assert_eq!(wrote[0], vec![OP_SYNC_HEARTBEAT, 0, CONNECTION_ARMED, 1]);
        assert_eq!(wrote[1], vec![OP_SYNC_HEARTBEAT, 0, CONNECTION_ARMED, 2]);
        // The connection flag is set on EVERY beat, not only the first. An MCU
        // that were reset underneath us must be re-armed by the next beat
        // rather than left counting down against a flag it no longer holds.
        for w in wrote {
            assert_eq!(w[2], CONNECTION_ARMED, "every beat must arm");
        }
    }

    #[tokio::test]
    async fn the_value_never_repeats_across_a_wrap() {
        // The MCU checks for a NEW value, so the one thing this must never do
        // is emit the same number twice in a row -- including at the wrap,
        // where a naive `+= 1` on a u8 would panic in debug builds and a
        // saturating one would stick at 255 forever and read as a dead sender.
        let mut hb = Bzm2BoardHeartbeat::new(FakeBoard::new());
        let mut last = None;
        for _ in 0..600 {
            let v = hb.beat().await.unwrap();
            assert_ne!(Some(v), last, "a repeated value reads as a stopped sender");
            last = Some(v);
        }
        assert_eq!(hb.beats(), 600);
        // 600 beats through a 256-value space means it wrapped at least twice.
        assert!(hb.bus.wrote.len() == 600);
    }

    #[tokio::test]
    async fn a_refused_write_does_not_consume_the_value() {
        // A run of bus failures must not advance the counter, or the MCU sees a
        // jump when the bus recovers. It only checks for a new value, so a jump
        // is survivable -- but a counter that advanced while nothing was
        // delivered also makes `beats()` lie to a supervisor watching it.
        let mut hb = Bzm2BoardHeartbeat::new(FakeBoard::nacking_after(2));
        assert_eq!(hb.beat().await.unwrap(), 1);
        assert_eq!(hb.beat().await.unwrap(), 2);
        for _ in 0..5 {
            assert!(matches!(
                hb.beat().await,
                Err(BoardMcuError::NoBoard(MCU_I2C_ADDRESS))
            ));
        }
        assert_eq!(hb.beats(), 2, "a refused write is not a beat");
        assert_eq!(hb.next_value(), 3, "the value space was not consumed");
    }

    #[tokio::test]
    async fn a_board_that_goes_dark_while_we_beat_it_is_a_failure() {
        // THE MEASURED CASE. A board shed while this was beating with zero
        // delivery failures logged, because a write returning Ok means the I2C
        // layer took it, not that the MCU was satisfied by it. The rail is the
        // only thing that can tell those apart.
        let mut hb = Bzm2BoardHeartbeat::new(FakeBoard::with_rail(Some(17_500)));
        hb.beat().await.unwrap();
        assert_eq!(hb.verify_still_energised().await, Some(true), "it was up");

        hb.bus.rail_mv = Some(0); // it shed, under us
        assert_eq!(
            hb.verify_still_energised().await,
            Some(false),
            "a board we saw energised going dark while we beat it is OUR failure"
        );
    }

    #[tokio::test]
    async fn a_board_that_was_never_up_is_not_our_failure() {
        // Beating at a board that was already dark is not a protection
        // failure, and reporting it as one would train an operator to ignore
        // the message that matters.
        let mut hb = Bzm2BoardHeartbeat::new(FakeBoard::with_rail(Some(0)));
        hb.beat().await.unwrap();
        assert_eq!(hb.verify_still_energised().await, Some(true));
    }

    #[tokio::test]
    async fn an_unreadable_rail_is_unmeasured_not_dark() {
        // The distinction the whole safety story rests on: we do not know is
        // not the same as it is off.
        let mut hb = Bzm2BoardHeartbeat::new(FakeBoard::with_rail(None));
        hb.beat().await.unwrap();
        assert_eq!(hb.verify_still_energised().await, None);
    }

    #[tokio::test]
    async fn constructing_one_arms_nothing() {
        // The obligation starts at the first beat, not at construction, so a
        // caller can build this and decide not to use it without having put a
        // board on a deadline.
        let hb = Bzm2BoardHeartbeat::new(FakeBoard::new());
        assert!(hb.bus.wrote.is_empty());
        assert_eq!(hb.beats(), 0);
    }
}
