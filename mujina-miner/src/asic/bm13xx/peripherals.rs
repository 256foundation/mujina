//! Board-supplied hardware interfaces for a BM13xx hash thread.
//!
//! A board assembles these and hands them to the thread it creates.
//! The thread operates them; the board decides what hardware sits
//! behind each one.

use anyhow::Result;
use async_trait::async_trait;

use crate::peripheral::regulator::VoltageRegulator;

/// The chips' reset line.
///
/// Hash threads hold the chips in reset until bring-up and assert
/// reset again at shutdown. What drives the line (a GPIO, a
/// management protocol command, etc.) is the board's concern.
#[async_trait]
pub trait ResetLine: Send + Sync {
    /// Asserts reset, stopping the chips.
    async fn assert(&mut self) -> Result<()>;

    /// Releases reset, letting the chips run.
    async fn release(&mut self) -> Result<()>;
}

/// Hardware interfaces provided by the board to the hash thread.
///
/// Every board supplies every interface. A board whose hardware
/// lacks a control (a fixed rail, no host-driven reset) supplies an
/// implementation declaring that, such as a regulator whose disable
/// is a no-op, rather than omitting the interface.
pub struct BoardPeripherals {
    /// The chips' reset line
    pub reset_line: Box<dyn ResetLine>,

    /// Voltage regulator control
    pub voltage_regulator: Box<dyn VoltageRegulator>,
}
