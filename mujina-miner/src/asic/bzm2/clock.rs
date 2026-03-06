use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{Instant, sleep};

use crate::transport::{SerialReader, SerialWriter};

use super::protocol::{encode_read_register, encode_write_register};

const NOTCH_REG: u16 = 0x0fff;
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

#[derive(Debug, thiserror::Error)]
pub enum Bzm2ClockError {
    #[error("serial write failed: {0}")]
    Write(#[from] std::io::Error),

    #[error("invalid PLL index")]
    InvalidPll,

    #[error("invalid desired PLL frequency {0} MHz")]
    InvalidFrequency(f32),

    #[error("invalid PLL post divider {0}")]
    InvalidPostDivider(u8),

    #[error("short read-register response: expected {expected} bytes, got {actual}")]
    ShortRead { expected: usize, actual: usize },

    #[error(
        "unexpected read-register response header: expected asic {expected_asic:#x} opcode 0x3, got asic {actual_asic:#x} opcode {actual_opcode:#x}"
    )]
    UnexpectedResponseHeader {
        expected_asic: u8,
        actual_asic: u8,
        actual_opcode: u8,
    },

    #[error(
        "PLL {pll:?} on ASIC {asic} did not lock before timeout; last enable value {last_enable:#x}"
    )]
    PllLockTimeout {
        asic: u8,
        pll: Bzm2Pll,
        last_enable: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bzm2Pll {
    Pll0,
    Pll1,
}

impl Bzm2Pll {
    fn register_block(self) -> (u8, u8, u8) {
        match self {
            Self::Pll0 => (
                LOCAL_REG_PLL_POSTDIV,
                LOCAL_REG_PLL_FBDIV,
                LOCAL_REG_PLL_ENABLE,
            ),
            Self::Pll1 => (
                LOCAL_REG_PLL1_POSTDIV,
                LOCAL_REG_PLL1_FBDIV,
                LOCAL_REG_PLL1_ENABLE,
            ),
        }
    }

    fn misc_register(self) -> u8 {
        match self {
            Self::Pll0 => LOCAL_REG_PLL_MISC,
            Self::Pll1 => LOCAL_REG_PLL1_MISC,
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
pub struct Bzm2PllStatus {
    pub pll: Bzm2Pll,
    pub enable_register: u32,
    pub misc_register: u32,
    pub enabled: bool,
    pub locked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bzm2ClockDebugReport {
    pub asic: u8,
    pub pll0: Bzm2PllStatus,
    pub pll1: Bzm2PllStatus,
}

pub struct Bzm2ClockController {
    reader: SerialReader,
    writer: SerialWriter,
}

impl Bzm2ClockController {
    pub fn new(reader: SerialReader, writer: SerialWriter) -> Self {
        Self { reader, writer }
    }

    pub async fn write_local_reg_u8(
        &mut self,
        asic: u8,
        offset: u8,
        value: u8,
    ) -> Result<(), Bzm2ClockError> {
        self.writer
            .write_all(&encode_write_register(asic, NOTCH_REG, offset, &[value]))
            .await?;
        self.writer.flush().await?;
        Ok(())
    }

    pub async fn write_local_reg_u32(
        &mut self,
        asic: u8,
        offset: u8,
        value: u32,
    ) -> Result<(), Bzm2ClockError> {
        self.writer
            .write_all(&encode_write_register(
                asic,
                NOTCH_REG,
                offset,
                &value.to_le_bytes(),
            ))
            .await?;
        self.writer.flush().await?;
        Ok(())
    }

    pub async fn read_local_reg_u8(&mut self, asic: u8, offset: u8) -> Result<u8, Bzm2ClockError> {
        let data = self.read_local_reg(asic, offset, 1).await?;
        Ok(data[0])
    }

    pub async fn read_local_reg_u32(
        &mut self,
        asic: u8,
        offset: u8,
    ) -> Result<u32, Bzm2ClockError> {
        let data = self.read_local_reg(asic, offset, 4).await?;
        Ok(u32::from_le_bytes(data.try_into().unwrap()))
    }

    pub async fn program_pll(
        &mut self,
        asic: u8,
        pll: Bzm2Pll,
        config: Bzm2PllConfig,
    ) -> Result<(), Bzm2ClockError> {
        let (postdiv_reg, fbdiv_reg, _) = pll.register_block();
        self.write_local_reg_u32(asic, fbdiv_reg, config.feedback_divider as u32)
            .await?;
        self.write_local_reg_u32(asic, postdiv_reg, config.packed_post_divider)
            .await?;
        sleep(Duration::from_millis(1)).await;
        Ok(())
    }

    pub async fn enable_pll(&mut self, asic: u8, pll: Bzm2Pll) -> Result<(), Bzm2ClockError> {
        let (_, _, enable_reg) = pll.register_block();
        self.write_local_reg_u32(asic, enable_reg, 1).await
    }

    pub async fn disable_pll(&mut self, asic: u8, pll: Bzm2Pll) -> Result<(), Bzm2ClockError> {
        let (_, _, enable_reg) = pll.register_block();
        self.write_local_reg_u32(asic, enable_reg, 0).await
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

    pub async fn wait_for_pll_lock(
        &mut self,
        asic: u8,
        pll: Bzm2Pll,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<Bzm2PllStatus, Bzm2ClockError> {
        let (_, _, enable_reg) = pll.register_block();
        let start = Instant::now();
        let mut last_enable = 0u32;

        loop {
            last_enable = self.read_local_reg_u32(asic, enable_reg).await?;
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
        let (_, _, enable_reg) = pll.register_block();
        let enable = self.read_local_reg_u32(asic, enable_reg).await?;
        let misc = self.read_local_reg_u32(asic, pll.misc_register()).await?;
        Ok(Bzm2PllStatus {
            pll,
            enable_register: enable,
            misc_register: misc,
            enabled: (enable & 0x1) != 0,
            locked: (enable & 0x4) != 0,
        })
    }

    pub async fn debug_report(&mut self, asic: u8) -> Result<Bzm2ClockDebugReport, Bzm2ClockError> {
        Ok(Bzm2ClockDebugReport {
            asic,
            pll0: self.read_pll_status(asic, Bzm2Pll::Pll0).await?,
            pll1: self.read_pll_status(asic, Bzm2Pll::Pll1).await?,
        })
    }

    async fn read_local_reg(
        &mut self,
        asic: u8,
        offset: u8,
        count: u8,
    ) -> Result<Vec<u8>, Bzm2ClockError> {
        let request = encode_read_register(asic, NOTCH_REG, offset, count);
        self.writer.write_all(&request).await?;
        self.writer.flush().await?;

        let expected = count as usize + 2;
        let mut response = vec![0u8; expected];
        let actual = self.reader.read_exact(&mut response).await;
        if let Err(err) = actual {
            if let std::io::ErrorKind::UnexpectedEof = err.kind() {
                return Err(Bzm2ClockError::ShortRead {
                    expected,
                    actual: 0,
                });
            }
            return Err(Bzm2ClockError::Write(err));
        }

        validate_read_response_header(asic, &response)?;
        Ok(response[2..].to_vec())
    }
}

fn validate_read_response_header(expected_asic: u8, response: &[u8]) -> Result<(), Bzm2ClockError> {
    if response.len() < 2 {
        return Err(Bzm2ClockError::ShortRead {
            expected: 2,
            actual: response.len(),
        });
    }
    let actual_asic = response[0];
    let actual_opcode = response[1];
    if actual_asic != expected_asic || actual_opcode != 0x03 {
        return Err(Bzm2ClockError::UnexpectedResponseHeader {
            expected_asic,
            actual_asic,
            actual_opcode,
        });
    }
    Ok(())
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
        assert_eq!(config.feedback_divider, 13);
        assert_eq!(config.packed_post_divider, 0x1001);
    }

    #[test]
    fn pll_divider_rounding_uses_half_down_legacy_behavior() {
        assert_eq!(round_legacy(12.49), 12);
        assert_eq!(round_legacy(12.50), 12);
        assert_eq!(round_legacy(12.51), 13);
    }

    #[test]
    fn read_response_header_validation_matches_legacy_contract() {
        validate_read_response_header(0x12, &[0x12, 0x03, 0xaa]).unwrap();
        let err = validate_read_response_header(0x12, &[0x13, 0x0f, 0xaa]).unwrap_err();
        assert!(matches!(
            err,
            Bzm2ClockError::UnexpectedResponseHeader {
                expected_asic: 0x12,
                actual_asic: 0x13,
                actual_opcode: 0x0f,
            }
        ));
    }
}
