//! Voltage regulator consumer interface.
//!
//! The trait carries the contract between a board and the code that
//! consumes a rail. Regulator chip drivers in this layer implement
//! it; a board picks the implementation and hands it to the
//! consumer.

use anyhow::Result;
use async_trait::async_trait;

use crate::types::Voltage;

/// Voltage regulator control for a consumer of one rail.
///
/// A consumer handle: the board is the provider and sets the
/// limits; the consumer operates the rail through this handle.
/// Voltages here are set points, what the regulator is programmed
/// to deliver, never measurements. The measured rail voltage is
/// telemetry and lives outside this trait.
#[async_trait]
pub trait VoltageRegulator: Send + Sync {
    /// Enable the regulator output.
    async fn enable(&mut self) -> Result<()>;

    /// Disable the regulator output.
    async fn disable(&mut self) -> Result<()>;

    /// Whether the regulator output is enabled.
    ///
    /// Reports the switch state, not the rail: a disabled output
    /// reads false while the rail decays, and an enabled output
    /// reads true even if the rail has collapsed.
    async fn is_enabled(&mut self) -> Result<bool>;

    /// Set the output voltage set point.
    async fn set_voltage(&mut self, voltage: Voltage) -> Result<()>;

    /// The output voltage set point.
    ///
    /// The set point keeps its value while the output is disabled.
    async fn get_voltage(&mut self) -> Result<Voltage>;
}
