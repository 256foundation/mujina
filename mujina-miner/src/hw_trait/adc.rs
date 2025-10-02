//! ADC hardware abstraction trait.

use super::Result;
use async_trait::async_trait;

/// ADC channel identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdcChannel(pub u8);

/// ADC abstraction for reading analog values
#[async_trait]
pub trait Adc: Send + Sync {
    /// Read raw ADC value from a channel.
    async fn read_raw(&mut self, channel: AdcChannel) -> Result<u16>;

    /// Read voltage from a channel in millivolts.
    async fn read_millivolts(&mut self, channel: AdcChannel) -> Result<u32>;
}
