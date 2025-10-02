//! I2C implementation using bitaxe-raw control protocol.

use async_trait::async_trait;

use super::channel::ControlChannel;
use super::{I2CCommand, Packet, Page};
use crate::hw_trait::i2c::{I2c, I2cError};
use crate::hw_trait::{HwError, Result};

/// I2C bus implementation using bitaxe-raw control protocol.
#[derive(Clone)]
pub struct BitaxeRawI2c {
    channel: ControlChannel,
}

impl BitaxeRawI2c {
    /// Create a new I2C bus using the given control channel.
    pub fn new(channel: ControlChannel) -> Self {
        Self { channel }
    }
}

#[async_trait]
impl I2c for BitaxeRawI2c {
    async fn write(&mut self, addr: u8, data: &[u8]) -> Result<()> {
        let packet = Packet::new(
            0, // ID will be assigned by channel
            Page::I2C,
            I2CCommand::Write as u8,
            [vec![addr], data.to_vec()].concat(),
        );

        self.channel
            .send_packet(packet)
            .await
            .map_err(|e| HwError::I2c(I2cError::Other(format!("Write failed: {}", e))))?;

        Ok(())
    }

    async fn read(&mut self, addr: u8, buffer: &mut [u8]) -> Result<()> {
        let packet = Packet::new(
            0, // ID will be assigned by channel
            Page::I2C,
            I2CCommand::Read as u8,
            vec![addr, buffer.len() as u8],
        );

        let response = self
            .channel
            .send_packet(packet)
            .await
            .map_err(|e| HwError::I2c(I2cError::Other(format!("Read failed: {}", e))))?;

        if response.data.len() != buffer.len() {
            return Err(HwError::I2c(I2cError::Other(format!(
                "Expected {} bytes, got {}",
                buffer.len(),
                response.data.len()
            ))));
        }

        buffer.copy_from_slice(&response.data);
        Ok(())
    }

    async fn write_read(&mut self, addr: u8, write: &[u8], read: &mut [u8]) -> Result<()> {
        let mut data = vec![addr];
        data.extend_from_slice(write);
        data.push(read.len() as u8);

        let packet = Packet::new(
            0, // ID will be assigned by channel
            Page::I2C,
            I2CCommand::WriteRead as u8,
            data,
        );

        let response = self
            .channel
            .send_packet(packet)
            .await
            .map_err(|e| HwError::I2c(I2cError::Other(format!("WriteRead failed: {}", e))))?;

        if response.data.len() != read.len() {
            return Err(HwError::I2c(I2cError::Other(format!(
                "Expected {} bytes, got {}",
                read.len(),
                response.data.len()
            ))));
        }

        read.copy_from_slice(&response.data);
        Ok(())
    }

    async fn set_frequency(&mut self, hz: u32) -> Result<()> {
        let packet = Packet::new(
            0, // ID will be assigned by channel
            Page::I2C,
            I2CCommand::SetFrequency as u8,
            hz.to_le_bytes().to_vec(),
        );

        self.channel
            .send_packet(packet)
            .await
            .map_err(|e| HwError::I2c(I2cError::Other(format!("SetFrequency failed: {}", e))))?;

        Ok(())
    }
}
