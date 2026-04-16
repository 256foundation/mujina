//! System operations using bitaxe-raw control protocol.

use std::io;

use super::channel::ControlChannel;
use super::{Packet, Page};

/// Reboot the board.
///
/// Sends a reboot command and returns immediately without waiting
/// for a response; the firmware resets before it can reply. The
/// host can detect success by observing USB re-enumeration.
pub async fn reboot(channel: &ControlChannel) -> io::Result<()> {
    const CMD_REBOOT: u8 = 0x01;
    const REBOOT_MAGIC: [u8; 4] = [0xDE, 0xAD, 0xBE, 0xEF];

    let packet = Packet::new(Page::System, CMD_REBOOT, REBOOT_MAGIC.to_vec());
    channel.send_packet_no_reply(packet).await
}

/// Reboot the board into its USB bootloader.
///
/// Like [`reboot`], no response is sent. The host can detect success
/// by observing USB re-enumeration as a UF2 mass storage device.
pub async fn reboot_to_bootloader(channel: &ControlChannel) -> io::Result<()> {
    const CMD_REBOOT_TO_BOOTLOADER: u8 = 0x02;
    const REBOOT_TO_BOOTLOADER_MAGIC: [u8; 4] = [0xB0, 0x07, 0x10, 0xAD];

    let packet = Packet::new(
        Page::System,
        CMD_REBOOT_TO_BOOTLOADER,
        REBOOT_TO_BOOTLOADER_MAGIC.to_vec(),
    );
    channel.send_packet_no_reply(packet).await
}
