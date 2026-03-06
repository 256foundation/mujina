use std::time::Duration;

use tokio::time::{Instant, sleep};

use crate::transport::{SerialReader, SerialWriter};

use super::uart::{Bzm2UartController, Bzm2UartError};

const REF_CLK_MHZ: f32 = 50.0;
const REF_DIVIDER: u8 = 1;
const POST2_DIVIDER: u8 = 0;

const LOCAL_REG_PLL_POSTDIV: u8 = 0x10;
const LOCAL_REG_PLL_FBDIV: u8 = 0x11;
const LOCAL_REG_PLL_ENABLE: u8 = 0x12;
const LOCAL_REG_PLL_MISC: u8 = 0x13;
const LOCAL_REG_PLL1_POSTDIV: u8 = 0x1a;
const LOCAL_REG_PLL1_FBDIV: u8 = 0x1b;
const LOCAL_REG_PLL1_ENABLE: u8 = 0x1c;
const LOCAL_REG_PLL1_MISC: u8 = 0x1d;

const LOCAL_REG_CKDCCR_2_0: u8 = 0x56;
const LOCAL_REG_CKDCCR_3_0: u8 = 0x57;
const LOCAL_REG_CKDCCR_4_0: u8 = 0x58;
const LOCAL_REG_CKDCCR_5_0: u8 = 0x59;
const LOCAL_REG_CKDLLR_0_0: u8 = 0x5a;
const LOCAL_REG_CKDLLR_1_0: u8 = 0x5b;
const LOCAL_REG_CKDCCR_2_1: u8 = 0x5e;
const LOCAL_REG_CKDCCR_3_1: u8 = 0x5f;
const LOCAL_REG_CKDCCR_4_1: u8 = 0x60;
const LOCAL_REG_CKDCCR_5_1: u8 = 0x61;
const LOCAL_REG_CKDLLR_0_1: u8 = 0x62;
const LOCAL_REG_CKDLLR_1_1: u8 = 0x63;

#[derive(Debug, thiserror::Error)]
pub enum Bzm2ClockError {
    #[error(transparent)]
    Uart(#[from] Bzm2UartError),

    #[error("invalid desired PLL frequency {0} MHz")]
    InvalidFrequency(f32),

    #[error("invalid PLL post divider {0}")]
    InvalidPostDivider(u8),

    #[error("unsupported DLL duty cycle {0}; supported values are 25, 50, 55, 60, 75")]
    InvalidDllDutyCycle(u8),

    #[error(
        "PLL {pll:?} on ASIC {asic} did not lock before timeout; last enable value {last_enable:#x}"
    )]
    PllLockTimeout {
        asic: u8,
        pll: Bzm2Pll,
        last_enable: u32,
    },

    #[error(
        "DLL {dll:?} on ASIC {asic} did not lock before timeout; last control value {last_control:#x}"
    )]
    DllLockTimeout {
        asic: u8,
        dll: Bzm2Dll,
        last_control: u8,
    },

    #[error("DLL {dll:?} on ASIC {asic} reported invalid fincon {fincon:#x}")]
    InvalidDllFincon { asic: u8, dll: Bzm2Dll, fincon: u8 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bzm2Pll {
    Pll0,
    Pll1,
}

impl Bzm2Pll {
    fn register_block(self) -> (u8, u8, u8, u8) {
        match self {
            Self::Pll0 => (
                LOCAL_REG_PLL_POSTDIV,
                LOCAL_REG_PLL_FBDIV,
                LOCAL_REG_PLL_ENABLE,
                LOCAL_REG_PLL_MISC,
            ),
            Self::Pll1 => (
                LOCAL_REG_PLL1_POSTDIV,
                LOCAL_REG_PLL1_FBDIV,
                LOCAL_REG_PLL1_ENABLE,
                LOCAL_REG_PLL1_MISC,
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bzm2Dll {
    Dll0,
    Dll1,
}

impl Bzm2Dll {
    fn registers(self) -> (u8, u8, u8, u8, u8) {
        match self {
            Self::Dll0 => (
                LOCAL_REG_CKDCCR_2_0,
                LOCAL_REG_CKDCCR_3_0,
                LOCAL_REG_CKDCCR_4_0,
                LOCAL_REG_CKDCCR_5_0,
                LOCAL_REG_CKDLLR_0_0,
            ),
            Self::Dll1 => (
                LOCAL_REG_CKDCCR_2_1,
                LOCAL_REG_CKDCCR_3_1,
                LOCAL_REG_CKDCCR_4_1,
                LOCAL_REG_CKDCCR_5_1,
                LOCAL_REG_CKDLLR_0_1,
            ),
        }
    }

    fn fincon_register(self) -> u8 {
        match self {
            Self::Dll0 => LOCAL_REG_CKDLLR_1_0,
            Self::Dll1 => LOCAL_REG_CKDLLR_1_1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bzm2PllConfig {
    pub frequency_mhz: f32,
    pub post1_divider: u8,
    pub ref_divider: u8,
    pub post2_divider: u8,
    pub feedback_divider: u16,
    pub packed_post_divider: u32,
}

impl Bzm2PllConfig {
    pub fn from_target_frequency(
        frequency_mhz: f32,
        post1_divider: u8,
    ) -> Result<Self, Bzm2ClockError> {
        if !frequency_mhz.is_finite() || frequency_mhz <= 0.0 {
            return Err(Bzm2ClockError::InvalidFrequency(frequency_mhz));
        }
        if post1_divider > 7 {
            return Err(Bzm2ClockError::InvalidPostDivider(post1_divider));
        }

        let feedback = REF_DIVIDER as f32
            * (post1_divider as f32 + 1.0)
            * (POST2_DIVIDER as f32 + 1.0)
            * frequency_mhz
            / REF_CLK_MHZ;
        let feedback_divider = round_legacy(feedback);
        let packed_post_divider = (1u32 << 12)
            | ((POST2_DIVIDER as u32) << 9)
            | ((post1_divider as u32) << 6)
            | REF_DIVIDER as u32;

        Ok(Self {
            frequency_mhz,
            post1_divider,
            ref_divider: REF_DIVIDER,
            post2_divider: POST2_DIVIDER,
            feedback_divider,
            packed_post_divider,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bzm2DllConfig {
    pub duty_cycle: u8,
    pub nde_dll: u8,
    pub nde_clk: u8,
    pub npi_clk: u8,
    pub pibypb: u8,
    pub dllfreeze: u8,
}

impl Bzm2DllConfig {
    pub fn from_duty_cycle(duty_cycle: u8) -> Result<Self, Bzm2ClockError> {
        let mut config = Self {
            duty_cycle,
            nde_dll: 0x1f,
            nde_clk: 0x0f,
            npi_clk: 0x0,
            pibypb: 1,
            dllfreeze: 0,
        };

        match duty_cycle {
            50 => {}
            75 => config.nde_clk = 0x17,
            60 => {
                config.nde_dll = 0x1d;
                config.nde_clk = 0x11;
            }
            55 => {
                config.nde_dll = 0x1d;
                config.nde_clk = 0x0f;
                config.npi_clk = 0x4;
            }
            25 => config.nde_clk = 0x07,
            _ => return Err(Bzm2ClockError::InvalidDllDutyCycle(duty_cycle)),
        }

        Ok(config)
    }

    fn control2(self) -> u8 {
        ((self.npi_clk & 0x7) << 3) | ((self.pibypb & 0x1) << 2) | ((self.dllfreeze & 0x1) << 1)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bzm2PllStatus {
    pub pll: Bzm2Pll,
    pub enable_register: u32,
    pub misc_register: u32,
    pub enabled: bool,
    pub locked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bzm2DllStatus {
    pub dll: Bzm2Dll,
    pub control2: u8,
    pub control5: u8,
    pub coarsecon: u8,
    pub fincon: u8,
    pub freeze_valid: bool,
    pub locked: bool,
    pub fincon_valid: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bzm2ClockDebugReport {
    pub asic: u8,
    pub pll0: Bzm2PllStatus,
    pub pll1: Bzm2PllStatus,
    pub dll0: Bzm2DllStatus,
    pub dll1: Bzm2DllStatus,
}

pub struct Bzm2ClockController {
    uart: Bzm2UartController,
}

impl Bzm2ClockController {
    pub fn new(reader: SerialReader, writer: SerialWriter) -> Self {
        Self {
            uart: Bzm2UartController::new(reader, writer),
        }
    }

    pub fn from_uart(uart: Bzm2UartController) -> Self {
        Self { uart }
    }

    pub fn into_uart(self) -> Bzm2UartController {
        self.uart
    }

    pub async fn program_pll(
        &mut self,
        asic: u8,
        pll: Bzm2Pll,
        config: Bzm2PllConfig,
    ) -> Result<(), Bzm2ClockError> {
        let (postdiv_reg, fbdiv_reg, _, _) = pll.register_block();
        self.uart
            .write_local_reg_u32(asic, fbdiv_reg, config.feedback_divider as u32)
            .await?;
        self.uart
            .write_local_reg_u32(asic, postdiv_reg, config.packed_post_divider)
            .await?;
        sleep(Duration::from_millis(1)).await;
        Ok(())
    }

    pub async fn enable_pll(&mut self, asic: u8, pll: Bzm2Pll) -> Result<(), Bzm2ClockError> {
        let (_, _, enable_reg, _) = pll.register_block();
        self.uart.write_local_reg_u32(asic, enable_reg, 1).await?;
        Ok(())
    }

    pub async fn disable_pll(&mut self, asic: u8, pll: Bzm2Pll) -> Result<(), Bzm2ClockError> {
        let (_, _, enable_reg, _) = pll.register_block();
        self.uart.write_local_reg_u32(asic, enable_reg, 0).await?;
        Ok(())
    }

    pub async fn set_pll_frequency(
        &mut self,
        asic: u8,
        pll: Bzm2Pll,
        frequency_mhz: f32,
        post1_divider: u8,
    ) -> Result<Bzm2PllConfig, Bzm2ClockError> {
        let config = Bzm2PllConfig::from_target_frequency(frequency_mhz, post1_divider)?;
        self.program_pll(asic, pll, config).await?;
        Ok(config)
    }

    pub async fn broadcast_pll_frequency(
        &mut self,
        pll: Bzm2Pll,
        frequency_mhz: f32,
        post1_divider: u8,
    ) -> Result<Bzm2PllConfig, Bzm2ClockError> {
        let config = Bzm2PllConfig::from_target_frequency(frequency_mhz, post1_divider)?;
        let (postdiv_reg, fbdiv_reg, _, _) = pll.register_block();
        self.uart
            .broadcast_local_reg_u32(fbdiv_reg, config.feedback_divider as u32)
            .await?;
        self.uart
            .broadcast_local_reg_u32(postdiv_reg, config.packed_post_divider)
            .await?;
        sleep(Duration::from_millis(1)).await;
        Ok(config)
    }

    pub async fn broadcast_enable_pll(&mut self, pll: Bzm2Pll) -> Result<(), Bzm2ClockError> {
        let (_, _, enable_reg, _) = pll.register_block();
        self.uart.broadcast_local_reg_u32(enable_reg, 1).await?;
        Ok(())
    }

    pub async fn broadcast_disable_pll(&mut self, pll: Bzm2Pll) -> Result<(), Bzm2ClockError> {
        let (_, _, enable_reg, _) = pll.register_block();
        self.uart.broadcast_local_reg_u32(enable_reg, 0).await?;
        Ok(())
    }

    pub async fn wait_for_pll_lock(
        &mut self,
        asic: u8,
        pll: Bzm2Pll,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<Bzm2PllStatus, Bzm2ClockError> {
        let (_, _, enable_reg, _) = pll.register_block();
        let start = Instant::now();

        loop {
            let last_enable = self.uart.read_local_reg_u32(asic, enable_reg).await?;
            let status = self.read_pll_status(asic, pll).await?;
            if status.locked {
                return Ok(status);
            }
            if start.elapsed() >= timeout {
                return Err(Bzm2ClockError::PllLockTimeout {
                    asic,
                    pll,
                    last_enable,
                });
            }
            sleep(poll_interval).await;
        }
    }

    pub async fn configure_and_lock_pll(
        &mut self,
        asic: u8,
        pll: Bzm2Pll,
        frequency_mhz: f32,
        post1_divider: u8,
        timeout: Duration,
    ) -> Result<(Bzm2PllConfig, Bzm2PllStatus), Bzm2ClockError> {
        let config = self
            .set_pll_frequency(asic, pll, frequency_mhz, post1_divider)
            .await?;
        self.enable_pll(asic, pll).await?;
        let status = self
            .wait_for_pll_lock(asic, pll, timeout, Duration::from_millis(100))
            .await?;
        Ok((config, status))
    }

    pub async fn read_pll_status(
        &mut self,
        asic: u8,
        pll: Bzm2Pll,
    ) -> Result<Bzm2PllStatus, Bzm2ClockError> {
        let (_, _, enable_reg, misc_reg) = pll.register_block();
        let enable = self.uart.read_local_reg_u32(asic, enable_reg).await?;
        let misc = self.uart.read_local_reg_u32(asic, misc_reg).await?;
        Ok(Bzm2PllStatus {
            pll,
            enable_register: enable,
            misc_register: misc,
            enabled: (enable & 0x1) != 0,
            locked: (enable & 0x4) != 0,
        })
    }

    pub async fn program_dll(
        &mut self,
        asic: u8,
        dll: Bzm2Dll,
        config: Bzm2DllConfig,
    ) -> Result<(), Bzm2ClockError> {
        let (control2_reg, control3_reg, control4_reg, _, _) = dll.registers();
        self.uart
            .write_local_reg_u8(asic, control3_reg, config.nde_dll & 0x1f)
            .await?;
        self.uart
            .write_local_reg_u8(asic, control4_reg, config.nde_clk & 0x1f)
            .await?;
        self.uart
            .write_local_reg_u8(asic, control2_reg, config.control2())
            .await?;
        sleep(Duration::from_millis(1)).await;
        Ok(())
    }

    pub async fn set_dll_duty_cycle(
        &mut self,
        asic: u8,
        dll: Bzm2Dll,
        duty_cycle: u8,
    ) -> Result<Bzm2DllConfig, Bzm2ClockError> {
        let config = Bzm2DllConfig::from_duty_cycle(duty_cycle)?;
        self.program_dll(asic, dll, config).await?;
        Ok(config)
    }

    pub async fn enable_dll(&mut self, asic: u8, dll: Bzm2Dll) -> Result<(), Bzm2ClockError> {
        let (_, _, _, control5_reg, _) = dll.registers();
        let value = self.uart.read_local_reg_u8(asic, control5_reg).await?;
        self.uart
            .write_local_reg_u8(asic, control5_reg, value | 0x1)
            .await?;
        let value = self.uart.read_local_reg_u8(asic, control5_reg).await?;
        self.uart
            .write_local_reg_u8(asic, control5_reg, value | (0x1 << 2))
            .await?;
        Ok(())
    }

    pub async fn disable_dll(&mut self, asic: u8, dll: Bzm2Dll) -> Result<(), Bzm2ClockError> {
        let (_, _, _, control5_reg, _) = dll.registers();
        self.uart.write_local_reg_u8(asic, control5_reg, 0).await?;
        Ok(())
    }

    pub async fn wait_for_dll_lock(
        &mut self,
        asic: u8,
        dll: Bzm2Dll,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<Bzm2DllStatus, Bzm2ClockError> {
        let (control2_reg, _, _, control5_reg, _) = dll.registers();
        let control2 = self.uart.read_local_reg_u8(asic, control2_reg).await?;
        if (control2 & 0x2) != 0 {
            sleep(Duration::from_millis(10)).await;
            return self.read_dll_status(asic, dll).await;
        }

        let start = Instant::now();

        loop {
            let last_control = self.uart.read_local_reg_u8(asic, control5_reg).await?;
            let status = self.read_dll_status(asic, dll).await?;
            if status.locked {
                return Ok(status);
            }
            if start.elapsed() >= timeout {
                return Err(Bzm2ClockError::DllLockTimeout {
                    asic,
                    dll,
                    last_control,
                });
            }
            sleep(poll_interval).await;
        }
    }

    pub async fn ensure_dll_fincon_valid(
        &mut self,
        asic: u8,
        dll: Bzm2Dll,
    ) -> Result<Bzm2DllStatus, Bzm2ClockError> {
        let status = self.read_dll_status(asic, dll).await?;
        if !status.fincon_valid {
            return Err(Bzm2ClockError::InvalidDllFincon {
                asic,
                dll,
                fincon: status.fincon,
            });
        }
        Ok(status)
    }

    pub async fn configure_and_lock_dll(
        &mut self,
        asic: u8,
        dll: Bzm2Dll,
        duty_cycle: u8,
        timeout: Duration,
    ) -> Result<(Bzm2DllConfig, Bzm2DllStatus), Bzm2ClockError> {
        let config = self.set_dll_duty_cycle(asic, dll, duty_cycle).await?;
        self.enable_dll(asic, dll).await?;
        self.wait_for_dll_lock(asic, dll, timeout, Duration::from_millis(10))
            .await?;
        let status = self.ensure_dll_fincon_valid(asic, dll).await?;
        Ok((config, status))
    }

    pub async fn read_dll_status(
        &mut self,
        asic: u8,
        dll: Bzm2Dll,
    ) -> Result<Bzm2DllStatus, Bzm2ClockError> {
        let (control2_reg, _, _, control5_reg, coarse_reg) = dll.registers();
        let control2 = self.uart.read_local_reg_u8(asic, control2_reg).await?;
        let control5 = self.uart.read_local_reg_u8(asic, control5_reg).await?;
        let coarse_raw = self.uart.read_local_reg_u8(asic, coarse_reg).await?;
        let fincon = self
            .uart
            .read_local_reg_u8(asic, dll.fincon_register())
            .await?;

        Ok(Bzm2DllStatus {
            dll,
            control2,
            control5,
            coarsecon: (coarse_raw >> 5) & 0x7,
            fincon,
            freeze_valid: (control2 & 0x2) != 0,
            locked: (control5 & 0x2) != 0,
            fincon_valid: fincon_is_valid(fincon),
        })
    }

    pub async fn debug_report(&mut self, asic: u8) -> Result<Bzm2ClockDebugReport, Bzm2ClockError> {
        Ok(Bzm2ClockDebugReport {
            asic,
            pll0: self.read_pll_status(asic, Bzm2Pll::Pll0).await?,
            pll1: self.read_pll_status(asic, Bzm2Pll::Pll1).await?,
            dll0: self.read_dll_status(asic, Bzm2Dll::Dll0).await?,
            dll1: self.read_dll_status(asic, Bzm2Dll::Dll1).await?,
        })
    }
}

fn fincon_is_valid(fincon: u8) -> bool {
    !matches!(fincon & 0xf0, 0xf0 | 0x00) && !matches!(fincon & 0xe0, 0xe0 | 0x00)
}

fn round_legacy(value: f32) -> u16 {
    let truncated = value as u16;
    if value - truncated as f32 > 0.5 {
        truncated.saturating_add(1)
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pll_divider_rounding_matches_legacy_formula() {
        let config = Bzm2PllConfig::from_target_frequency(625.0, 0).unwrap();
        assert_eq!(config.ref_divider, 1);
        assert_eq!(config.post1_divider, 0);
        assert_eq!(config.post2_divider, 0);
        assert_eq!(config.feedback_divider, 12);
        assert_eq!(config.packed_post_divider, 0x1001);
    }

    #[test]
    fn pll_divider_rounding_uses_half_down_legacy_behavior() {
        assert_eq!(round_legacy(12.49), 12);
        assert_eq!(round_legacy(12.50), 12);
        assert_eq!(round_legacy(12.51), 13);
    }

    #[test]
    fn dll_duty_cycle_matches_legacy_presets() {
        let duty_55 = Bzm2DllConfig::from_duty_cycle(55).unwrap();
        assert_eq!(duty_55.nde_dll, 0x1d);
        assert_eq!(duty_55.nde_clk, 0x0f);
        assert_eq!(duty_55.npi_clk, 0x4);

        let duty_75 = Bzm2DllConfig::from_duty_cycle(75).unwrap();
        assert_eq!(duty_75.nde_dll, 0x1f);
        assert_eq!(duty_75.nde_clk, 0x17);
        assert_eq!(duty_75.npi_clk, 0x0);
    }

    #[test]
    fn fincon_validation_matches_legacy_rules() {
        assert!(fincon_is_valid(0x9c));
        assert!(!fincon_is_valid(0xf4));
        assert!(!fincon_is_valid(0x0f));
        assert!(!fincon_is_valid(0xe1));
    }
}
