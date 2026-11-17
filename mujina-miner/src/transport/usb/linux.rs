//! Linux-specific USB platform support.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use nix::unistd::{AccessFlags, access};
use nusb::DeviceInfo;
use tokio::time::sleep;
use udev::{Device, Enumerator};

use crate::tracing::prelude::*;

/// Produce the sysfs path for a USB device.
pub fn device_path(device: &DeviceInfo) -> String {
    device.sysfs_path().to_string_lossy().into_owned()
}

/// Search for serial port devices (tty) associated with a USB
/// device.
///
/// Retries until at least `expected` ports appear in sysfs and
/// become accessible, since USB hotplug events arrive before the
/// kernel has created tty children and before udev rules have set
/// permissions. Returns an error if fewer than `expected` ports
/// are found after retries are exhausted.
///
/// Ports are ordered by USB interface number, which the device's
/// firmware sets. The kernel numbers device nodes from whatever
/// is free, so their names give no order. One device's ports can
/// be ttyACM3 and ttyACM11.
pub async fn get_serial_ports(device_path: &str, expected: usize) -> Result<Vec<String>> {
    const RETRIES: u32 = 20;
    const DELAY: Duration = Duration::from_millis(100);

    let path = device_path.to_string();
    let mut found = 0;

    for attempt in 0..RETRIES {
        let p = path.clone();
        let ports = tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
            let parent = Device::from_syspath(Path::new(&p))
                .with_context(|| format!("failed to open USB device at {}", p))?;

            let mut enumerator = Enumerator::new()?;
            enumerator.match_subsystem("tty")?;
            enumerator.match_parent(&parent)?;

            let mut ports = Vec::new();
            for tty_device in enumerator.scan_devices()? {
                if let Some(devnode) = tty_device.devnode()
                    && let Some(path_str) = devnode.to_str()
                {
                    let interface = interface_number(&tty_device).with_context(|| {
                        format!("failed to read the USB interface number of {}", path_str)
                    })?;
                    ports.push((interface, path_str.to_string()));
                }
            }

            if ports.is_empty() {
                return Ok(vec![]);
            }

            // Verify that udev rules have set permissions.
            // Use access(2) rather than opening the device, since
            // opening a serial port can toggle DTR/RTS on some
            // drivers.
            let all_accessible = ports.iter().all(|(_, port)| {
                access(port.as_str(), AccessFlags::R_OK | AccessFlags::W_OK).is_ok()
            });

            if !all_accessible {
                debug!(
                    count = ports.len(),
                    "serial ports found but not yet accessible"
                );
                return Ok(vec![]);
            }

            Ok(order_by_interface(ports))
        })
        .await??;

        if ports.len() >= expected {
            return Ok(ports);
        }

        found = found.max(ports.len());

        if attempt + 1 < RETRIES {
            debug!(
                attempt = attempt + 1,
                device_path, "waiting for serial ports to become available"
            );
            sleep(DELAY).await;
        }
    }

    bail!(
        "expected {} serial ports at {}, found {} after {} retries",
        expected,
        device_path,
        found,
        RETRIES,
    );
}

/// Reads the number of the USB interface a tty belongs to.
fn interface_number(tty: &Device) -> Result<u8> {
    let interface = tty
        .parent_with_subsystem_devtype("usb", "usb_interface")?
        .context("the tty has no USB interface above it")?;
    let text = interface
        .attribute_value("bInterfaceNumber")
        .and_then(|value| value.to_str())
        .context("the USB interface has no bInterfaceNumber")?;

    parse_interface_number(text)
}

/// Parses bInterfaceNumber as sysfs gives it, two hex digits.
fn parse_interface_number(text: &str) -> Result<u8> {
    u8::from_str_radix(text.trim(), 16)
        .with_context(|| format!("invalid bInterfaceNumber {:?}", text))
}

/// Returns the device nodes in order of their interface numbers.
fn order_by_interface(mut ports: Vec<(u8, String)>) -> Vec<String> {
    ports.sort_by_key(|(interface, _)| *interface);
    ports.into_iter().map(|(_, port)| port).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn port(interface: u8, node: &str) -> (u8, String) {
        (interface, node.to_string())
    }

    #[test]
    fn test_ports_follow_interface_order_not_node_names() {
        // As a string, ttyACM11 sorts ahead of ttyACM3
        let ports = vec![port(2, "/dev/ttyACM11"), port(0, "/dev/ttyACM3")];
        assert_eq!(order_by_interface(ports), ["/dev/ttyACM3", "/dev/ttyACM11"]);

        // The lower interface may have the higher node number
        let ports = vec![port(2, "/dev/ttyACM3"), port(0, "/dev/ttyACM11")];
        assert_eq!(order_by_interface(ports), ["/dev/ttyACM11", "/dev/ttyACM3"]);
    }

    #[test]
    fn test_interface_number_parses_as_hex() {
        assert_eq!(parse_interface_number("02").unwrap(), 2);
        assert_eq!(parse_interface_number("10").unwrap(), 16);
        assert!(parse_interface_number("").is_err());
    }
}
