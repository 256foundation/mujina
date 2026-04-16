use std::time::Duration;

use tokio_serial::SerialPortBuilderExt;

use crate::hw_trait::rgb_led::{RgbColor, RgbLed};
use crate::mgmt_protocol::ControlChannel;
use crate::mgmt_protocol::bitaxe_raw::ResponseFormat;
use crate::mgmt_protocol::bitaxe_raw::led::BitaxeRawLed;
use crate::peripheral::led::{CalibratedLed, ColorProfile, Status, StatusLed};

/// Find a connected emberOne/00 board via udev and return its
/// control serial port path and bcdDevice, or None if no board
/// is connected.
fn find_emberone00() -> Option<(String, Option<u16>)> {
    let mut enumerator = udev::Enumerator::new().ok()?;
    enumerator.match_subsystem("usb").ok()?;

    for device in enumerator.scan_devices().ok()? {
        let manufacturer = device
            .attribute_value("manufacturer")
            .and_then(|v| v.to_str());
        let product = device.attribute_value("product").and_then(|v| v.to_str());

        if manufacturer != Some("256F") || product != Some("EmberOne00") {
            continue;
        }

        let bcd_device = device
            .attribute_value("bcdDevice")
            .and_then(|v| v.to_str())
            .and_then(|s| u16::from_str_radix(s, 16).ok());

        let device_path = device.syspath().to_str()?;

        // Find tty children of this USB device
        let mut tty_enum = udev::Enumerator::new().ok()?;
        tty_enum.match_subsystem("tty").ok()?;
        let mut ports: Vec<String> = tty_enum
            .scan_devices()
            .ok()?
            .filter(|tty| {
                let mut cur = tty.parent();
                while let Some(p) = cur {
                    if p.syspath().to_str() == Some(device_path) {
                        return true;
                    }
                    cur = p.parent();
                }
                false
            })
            .filter_map(|tty| tty.devnode().and_then(|n| n.to_str()).map(String::from))
            .collect();
        ports.sort();
        let port = ports.into_iter().next()?;
        return Some((port, bcd_device));
    }
    None
}

/// Open a raw (uncalibrated) LED on the connected emberOne/00.
fn open_raw_led() -> BitaxeRawLed {
    let (port_path, bcd_device) =
        find_emberone00().expect("no emberOne/00 found; is the board connected and powered?");

    let format = match bcd_device {
        Some(bcd) if (bcd & 0x00F0) >= 0x0010 => ResponseFormat::V1,
        _ => ResponseFormat::V0,
    };
    eprintln!(
        "Using control port: {port_path} (bcdDevice: {bcd_device:#06x?}, format: {format:?})"
    );

    let serial = tokio_serial::new(&port_path, 115200)
        .open_native_async()
        .expect("failed to open control port");
    let channel = ControlChannel::new(serial, format);
    BitaxeRawLed::new(channel)
}

/// Open a calibrated LED on the connected emberOne/00.
fn open_led() -> CalibratedLed {
    let raw_led = open_raw_led();
    let profile = ColorProfile::SK6812;
    eprintln!("Profile: {profile:?}");
    CalibratedLed::new(Box::new(raw_led), profile)
}

/// Show primary and secondary colors for calibration.
///
/// Run with:
/// ```sh
/// cargo test --package mujina-miner emberone00::integration_tests::led_calibration \
///     -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore = "requires connected emberOne/00 hardware"]
async fn led_calibration() {
    let mut led = open_led();

    let colors = [
        // Primaries
        ("Red", RgbColor::RED),
        ("Green", RgbColor::GREEN),
        ("Blue", RgbColor::BLUE),
        // Secondaries (channel balance test)
        (
            "Yellow (R+G)",
            RgbColor {
                r: 255,
                g: 255,
                b: 0,
            },
        ),
        (
            "Cyan (G+B)",
            RgbColor {
                r: 0,
                g: 255,
                b: 255,
            },
        ),
        (
            "Magenta (R+B)",
            RgbColor {
                r: 255,
                g: 0,
                b: 255,
            },
        ),
        // Final check
        ("White", RgbColor::WHITE),
    ];

    for (name, color) in colors {
        eprintln!("{name}");
        led.set(color, 1.0).await.unwrap();
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    // Breathing white to check for tint at varying brightness
    let mut status_led = StatusLed::new(Box::new(led), Status::Hashing);
    eprintln!("Breathing white for 6s");
    tokio::time::sleep(Duration::from_secs(6)).await;

    eprintln!("Off");
    status_led.off().await;

    eprintln!("Done.");
}

/// Breathe each primary channel to compare fade curves.
///
/// Watch for uneven fading -- if one channel rushes through dim
/// values while another fades smoothly, per-channel gamma needs
/// adjustment.
///
/// Run with:
/// ```sh
/// cargo test --package mujina-miner emberone00::integration_tests::led_channel_breathe \
///     -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore = "requires connected emberOne/00 hardware"]
async fn led_channel_breathe() {
    use crate::peripheral::led::animation;

    let led: Box<dyn RgbLed> = Box::new(open_led());

    let colors = [
        ("Red", RgbColor::RED),
        ("Green", RgbColor::GREEN),
        ("Blue", RgbColor::BLUE),
        ("White", RgbColor::WHITE),
    ];

    let mut led = led;
    for (name, color) in colors {
        eprintln!("{name}");
        let handle = animation::breathe(led, color);
        tokio::time::sleep(Duration::from_secs(9)).await;
        led = handle.cancel().await;
    }

    led.off().await.unwrap();
    eprintln!("Done.");
}

/// Show each channel at a single low value to find visibility
/// thresholds.
///
/// Run with:
/// ```sh
/// cargo test --package mujina-miner emberone00::integration_tests::led_threshold \
///     -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore = "requires connected emberOne/00 hardware"]
async fn led_threshold() {
    let mut led = open_raw_led();

    let level = 1;
    let colors = [
        (
            "Red",
            RgbColor {
                r: level,
                g: 0,
                b: 0,
            },
        ),
        (
            "Green",
            RgbColor {
                r: 0,
                g: level,
                b: 0,
            },
        ),
        (
            "Blue",
            RgbColor {
                r: 0,
                g: 0,
                b: level,
            },
        ),
        (
            "White",
            RgbColor {
                r: level,
                g: level,
                b: level,
            },
        ),
    ];

    for (name, color) in colors {
        eprintln!("{name} at {level}/255");
        led.set(color, 1.0).await.unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    eprintln!("Off");
    led.off().await.unwrap();

    eprintln!("Done.");
}

/// Cycle through each board status state briefly.
///
/// Run with:
/// ```sh
/// cargo test --package mujina-miner emberone00::integration_tests::led_status \
///     -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore = "requires connected emberOne/00 hardware"]
async fn led_status() {
    let led = open_led();
    let mut status_led = StatusLed::new(Box::new(led), Status::Initializing);

    for status in [
        Status::Initializing,
        Status::Hashing,
        Status::Idle,
        Status::Fault,
        Status::Identify,
    ] {
        eprintln!("{status:?}");
        status_led.set(status).await;
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    eprintln!("Off");
    status_led.off().await;

    eprintln!("Done.");
}

/// Show breathing animations for extended evaluation.
///
/// Run with:
/// ```sh
/// cargo test --package mujina-miner emberone00::integration_tests::led_breathe \
///     -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore = "requires connected emberOne/00 hardware"]
async fn led_breathe() {
    let led = open_led();
    let mut status_led = StatusLed::new(Box::new(led), Status::Initializing);

    eprintln!("Initializing (orange) for 15s");
    tokio::time::sleep(Duration::from_secs(15)).await;

    eprintln!("Hashing (white) for 15s");
    status_led.set(Status::Hashing).await;
    tokio::time::sleep(Duration::from_secs(15)).await;

    eprintln!("Off");
    status_led.off().await;

    eprintln!("Done.");
}
