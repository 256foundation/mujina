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
/// Ports are sorted by device node name for consistent ordering
/// across reconnections.
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
                    ports.push(path_str.to_string());
                }
            }

            if ports.is_empty() {
                return Ok(vec![]);
            }

            // Verify that udev rules have set permissions.
            // Use access(2) rather than opening the device, since
            // opening a serial port can toggle DTR/RTS on some
            // drivers.
            let all_accessible = ports
                .iter()
                .all(|port| access(port.as_str(), AccessFlags::R_OK | AccessFlags::W_OK).is_ok());

            if !all_accessible {
                debug!(
                    count = ports.len(),
                    "serial ports found but not yet accessible"
                );
                return Ok(vec![]);
            }

            ports.sort();
            Ok(ports)
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
