use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::fs;
use tokio::time::sleep;

use crate::{
    asic::hash_thread::{AsicEnable, VoltageRegulator},
    hw_trait::{
        gpio::{GpioPin, PinValue},
        i2c::I2c,
    },
    peripheral::tps546::Tps546,
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PowerRailTelemetry {
    pub vin_volts: f32,
    pub vout_volts: f32,
    pub current_amps: f32,
    pub temperature_c: f32,
    pub power_watts: f32,
}

#[async_trait]
pub trait PowerRail: Send + Sync {
    async fn initialize(&mut self) -> Result<()>;
    async fn set_voltage(&mut self, volts: f32) -> Result<()>;
    async fn telemetry(&mut self) -> Result<PowerRailTelemetry>;
}

pub struct Tps546PowerRail<I2C> {
    inner: Tps546<I2C>,
}

impl<I2C> Tps546PowerRail<I2C> {
    pub fn new(inner: Tps546<I2C>) -> Self {
        Self { inner }
    }

    pub fn into_inner(self) -> Tps546<I2C> {
        self.inner
    }
}

#[async_trait]
impl<I2C: I2c> PowerRail for Tps546PowerRail<I2C> {
    async fn initialize(&mut self) -> Result<()> {
        self.inner.init().await
    }

    async fn set_voltage(&mut self, volts: f32) -> Result<()> {
        self.inner.set_vout(volts).await
    }

    async fn telemetry(&mut self) -> Result<PowerRailTelemetry> {
        Ok(PowerRailTelemetry {
            vin_volts: self.inner.get_vin().await? as f32 / 1000.0,
            vout_volts: self.inner.get_vout().await? as f32 / 1000.0,
            current_amps: self.inner.get_iout().await? as f32 / 1000.0,
            temperature_c: self.inner.get_temperature().await? as f32,
            power_watts: self.inner.get_power().await? as f32 / 1000.0,
        })
    }
}

#[async_trait]
impl<I2C: I2c> VoltageRegulator for Tps546PowerRail<I2C> {
    async fn set_voltage(&mut self, volts: f32) -> Result<()> {
        PowerRail::set_voltage(self, volts).await
    }
}

pub struct GpioResetLine<PIN> {
    pin: PIN,
    active_low: bool,
}

impl<PIN> GpioResetLine<PIN> {
    pub fn new(pin: PIN, active_low: bool) -> Self {
        Self { pin, active_low }
    }

    pub fn into_inner(self) -> PIN {
        self.pin
    }

    async fn drive(&mut self, asserted: bool) -> Result<()>
    where
        PIN: GpioPin,
    {
        let value = if asserted == self.active_low {
            PinValue::Low
        } else {
            PinValue::High
        };
        self.pin.write(value).await?;
        Ok(())
    }

    pub async fn pulse(&mut self, assert_for: Duration, settle_for: Duration) -> Result<()>
    where
        PIN: GpioPin,
    {
        self.drive(true).await?;
        sleep(assert_for).await;
        self.drive(false).await?;
        sleep(settle_for).await;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct FileGpioPin {
    path: String,
    high_value: String,
    low_value: String,
}

impl FileGpioPin {
    pub fn new(
        path: impl Into<String>,
        high_value: impl Into<String>,
        low_value: impl Into<String>,
    ) -> Self {
        Self {
            path: path.into(),
            high_value: high_value.into(),
            low_value: low_value.into(),
        }
    }
}

#[async_trait]
impl GpioPin for FileGpioPin {
    async fn set_mode(
        &mut self,
        _mode: crate::hw_trait::gpio::PinMode,
    ) -> crate::hw_trait::Result<()> {
        Ok(())
    }

    async fn write(&mut self, value: PinValue) -> crate::hw_trait::Result<()> {
        let raw = match value {
            PinValue::Low => &self.low_value,
            PinValue::High => &self.high_value,
        };
        fs::write(&self.path, raw).await?;
        Ok(())
    }

    async fn read(&mut self) -> crate::hw_trait::Result<PinValue> {
        let raw = fs::read_to_string(&self.path).await?;
        if raw.trim() == self.high_value.trim() {
            Ok(PinValue::High)
        } else {
            Ok(PinValue::Low)
        }
    }
}

#[async_trait]
impl<PIN: GpioPin> AsicEnable for GpioResetLine<PIN> {
    async fn enable(&mut self) -> Result<()> {
        self.drive(false).await
    }

    async fn disable(&mut self) -> Result<()> {
        self.drive(true).await
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VoltageStackStep {
    pub rail_index: usize,
    pub voltage: f32,
    pub settle_for: Duration,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VoltageStackBringupPlan {
    pub assert_reset_before_power: bool,
    pub pre_power_delay: Duration,
    pub post_power_delay: Duration,
    pub release_reset_delay: Duration,
    pub steps: Vec<VoltageStackStep>,
}

#[derive(Debug, Clone)]
pub struct FilePowerRail {
    set_path: String,
    write_scale: f32,
    enable_path: Option<String>,
    enable_value: Option<String>,
}

impl FilePowerRail {
    pub fn new(path: impl Into<String>, write_scale: f32) -> Self {
        Self {
            set_path: path.into(),
            write_scale,
            enable_path: None,
            enable_value: None,
        }
    }

    pub fn with_enable(
        mut self,
        enable_path: impl Into<String>,
        enable_value: impl Into<String>,
    ) -> Self {
        self.enable_path = Some(enable_path.into());
        self.enable_value = Some(enable_value.into());
        self
    }

    fn encode_voltage(&self, volts: f32) -> String {
        if (self.write_scale - 1.0).abs() < f32::EPSILON {
            format!("{volts:.6}")
        } else {
            format!("{}", (volts * self.write_scale).round() as i64)
        }
    }
}

#[async_trait]
impl PowerRail for FilePowerRail {
    async fn initialize(&mut self) -> Result<()> {
        if let (Some(path), Some(value)) = (&self.enable_path, &self.enable_value) {
            fs::write(path, value).await?;
        }
        Ok(())
    }

    async fn set_voltage(&mut self, volts: f32) -> Result<()> {
        fs::write(&self.set_path, self.encode_voltage(volts)).await?;
        Ok(())
    }

    async fn telemetry(&mut self) -> Result<PowerRailTelemetry> {
        Ok(PowerRailTelemetry {
            vin_volts: 0.0,
            vout_volts: 0.0,
            current_amps: 0.0,
            temperature_c: 0.0,
            power_watts: 0.0,
        })
    }
}

impl Default for VoltageStackBringupPlan {
    fn default() -> Self {
        Self {
            assert_reset_before_power: true,
            pre_power_delay: Duration::from_millis(10),
            post_power_delay: Duration::from_millis(25),
            release_reset_delay: Duration::from_millis(25),
            steps: Vec::new(),
        }
    }
}

impl VoltageStackBringupPlan {
    pub async fn apply<R, PIN>(
        &self,
        rails: &mut [R],
        mut reset_line: Option<&mut GpioResetLine<PIN>>,
    ) -> Result<()>
    where
        R: PowerRail,
        PIN: GpioPin,
    {
        if self.assert_reset_before_power {
            if let Some(reset_line) = reset_line.as_deref_mut() {
                reset_line.disable().await?;
            }
            sleep(self.pre_power_delay).await;
        }

        for rail in rails.iter_mut() {
            rail.initialize().await?;
        }

        for step in &self.steps {
            let rail = rails
                .get_mut(step.rail_index)
                .ok_or_else(|| anyhow::anyhow!("rail index {} out of range", step.rail_index))?;
            rail.set_voltage(step.voltage).await?;
            sleep(step.settle_for).await;
        }

        sleep(self.post_power_delay).await;

        if let Some(reset_line) = reset_line.as_deref_mut() {
            reset_line.enable().await?;
            sleep(self.release_reset_delay).await;
        }

        Ok(())
    }

    pub async fn shutdown<R, PIN>(
        &self,
        rails: &mut [R],
        mut reset_line: Option<&mut GpioResetLine<PIN>>,
    ) -> Result<()>
    where
        R: PowerRail,
        PIN: GpioPin,
    {
        if let Some(reset_line) = reset_line.as_deref_mut() {
            reset_line.disable().await?;
        }

        for rail in rails.iter_mut().rev() {
            rail.set_voltage(0.0).await?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hw_trait::{Result as HwResult, gpio::PinMode};

    #[derive(Default)]
    struct MockPin {
        writes: Vec<PinValue>,
    }

    #[async_trait]
    impl GpioPin for MockPin {
        async fn set_mode(&mut self, _mode: PinMode) -> HwResult<()> {
            Ok(())
        }

        async fn write(&mut self, value: PinValue) -> HwResult<()> {
            self.writes.push(value);
            Ok(())
        }

        async fn read(&mut self) -> HwResult<PinValue> {
            Ok(self.writes.last().copied().unwrap_or(PinValue::Low))
        }
    }

    #[derive(Default)]
    struct MockRail {
        initialized: bool,
        voltages: Vec<f32>,
    }

    #[async_trait]
    impl PowerRail for MockRail {
        async fn initialize(&mut self) -> Result<()> {
            self.initialized = true;
            Ok(())
        }

        async fn set_voltage(&mut self, volts: f32) -> Result<()> {
            self.voltages.push(volts);
            Ok(())
        }

        async fn telemetry(&mut self) -> Result<PowerRailTelemetry> {
            Ok(PowerRailTelemetry {
                vin_volts: 12.0,
                vout_volts: self.voltages.last().copied().unwrap_or_default(),
                current_amps: 1.0,
                temperature_c: 42.0,
                power_watts: 12.0,
            })
        }
    }

    #[tokio::test]
    async fn bringup_plan_sequences_rails_then_releases_reset() {
        let mut rails = vec![MockRail::default(), MockRail::default()];
        let mut reset = GpioResetLine::new(MockPin::default(), true);
        let plan = VoltageStackBringupPlan {
            pre_power_delay: Duration::from_millis(0),
            post_power_delay: Duration::from_millis(0),
            release_reset_delay: Duration::from_millis(0),
            steps: vec![
                VoltageStackStep {
                    rail_index: 0,
                    voltage: 0.82,
                    settle_for: Duration::from_millis(0),
                },
                VoltageStackStep {
                    rail_index: 1,
                    voltage: 0.79,
                    settle_for: Duration::from_millis(0),
                },
            ],
            ..Default::default()
        };

        plan.apply(&mut rails, Some(&mut reset)).await.unwrap();

        assert!(rails[0].initialized);
        assert!(rails[1].initialized);
        assert_eq!(rails[0].voltages, vec![0.82]);
        assert_eq!(rails[1].voltages, vec![0.79]);
        let pin = reset.into_inner();
        assert_eq!(pin.writes, vec![PinValue::Low, PinValue::High]);
    }

    #[tokio::test]
    async fn shutdown_plan_asserts_reset_then_powers_off_rails() {
        let mut rails = vec![MockRail::default(), MockRail::default()];
        let mut reset = GpioResetLine::new(MockPin::default(), true);
        let plan = VoltageStackBringupPlan::default();

        plan.shutdown(&mut rails, Some(&mut reset)).await.unwrap();

        assert_eq!(rails[0].voltages, vec![0.0]);
        assert_eq!(rails[1].voltages, vec![0.0]);
        let pin = reset.into_inner();
        assert_eq!(pin.writes, vec![PinValue::Low]);
    }
}
