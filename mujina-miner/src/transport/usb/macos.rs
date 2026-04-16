//! macOS-specific USB platform support.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use core_foundation::base::{CFType, TCFType, kCFAllocatorDefault};
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use io_kit_sys::{
    IOIteratorNext, IOObjectRelease, IORegistryEntryCreateCFProperty,
    IORegistryEntrySearchCFProperty, IOServiceGetMatchingServices, IOServiceMatching,
    kIOMasterPortDefault, kIORegistryIterateParents, kIORegistryIterateRecursively,
    ret::kIOReturnSuccess,
    types::{io_iterator_t, io_object_t},
};
use nusb::DeviceInfo;
use tokio::time::sleep;

use crate::tracing::prelude::*;

/// Encode the IOKit location ID as a device path string.
pub fn device_path(device: &DeviceInfo) -> String {
    format!("IOKit:0x{:08x}", device.location_id())
}

/// Search for serial ports associated with a USB device by IOKit
/// location ID.
///
/// Searches for IOSerialBSDClient entries whose parent USB device
/// has a matching location ID. Returns /dev/cu.* callout device
/// paths, sorted for consistent ordering.
///
/// The IOKit scan runs on a blocking thread to avoid stalling the
/// Tokio runtime.
///
/// Retries until at least `expected` ports appear, since macOS
/// has the same race as Linux where the USB device is visible
/// before the CDC ACM driver has published the IOSerialBSDClient
/// nub and the /dev/cu.* node exists. Unlike Linux, permissions
/// are not a concern since macOS creates device nodes with final
/// permissions.
pub async fn get_serial_ports(device_path: &str, expected: usize) -> Result<Vec<String>> {
    const RETRIES: u32 = 20;
    const DELAY: Duration = Duration::from_millis(100);

    let path = device_path.to_string();
    let mut found = 0;

    for attempt in 0..RETRIES {
        let p = path.clone();
        let ports = tokio::task::spawn_blocking(move || find_serial_ports_blocking(&p)).await??;

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

fn find_serial_ports_blocking(device_path: &str) -> Result<Vec<String>> {
    let location_id = device_path
        .strip_prefix("IOKit:0x")
        .and_then(|s| u32::from_str_radix(s, 16).ok())
        .with_context(|| format!("invalid IOKit device path: {}", device_path))?;

    let mut ports = Vec::new();

    // SAFETY: IOServiceMatching returns a CFMutableDictionaryRef
    // following the Create Rule.  IOServiceGetMatchingServices
    // consumes it (releases on our behalf), so we must not release
    // the dictionary ourselves.
    let matching = unsafe { IOServiceMatching(c"IOSerialBSDClient".as_ptr()) };
    if matching.is_null() {
        bail!("failed to create IOSerialBSDClient matching dictionary");
    }

    let mut iterator: io_iterator_t = 0;
    // SAFETY: matching is non-null and will be consumed by this call.
    // iterator is written to on success.
    let result =
        unsafe { IOServiceGetMatchingServices(kIOMasterPortDefault, matching, &mut iterator) };
    if result != kIOReturnSuccess {
        bail!(
            "IOServiceGetMatchingServices failed for serial: 0x{:08x}",
            result
        );
    }

    loop {
        // SAFETY: iterator is a valid io_iterator_t from a successful
        // IOServiceGetMatchingServices call above.
        let entry = unsafe { IOIteratorNext(iterator) };
        if entry == 0 {
            break;
        }

        if let Some(callout) = get_string_property(entry, "IOCalloutDevice")
            && parent_has_location_id(entry, location_id)
        {
            ports.push(callout);
        }

        // SAFETY: entry is a valid io_object_t returned by
        // IOIteratorNext; we are done with it.
        unsafe { IOObjectRelease(entry) };
    }

    // SAFETY: iterator is a valid io_object_t from
    // IOServiceGetMatchingServices; we are done iterating.
    unsafe { IOObjectRelease(iterator) };

    ports.sort();
    Ok(ports)
}

/// Check whether any parent of `entry` has the given IOKit
/// location ID.
///
/// Uses IORegistryEntrySearchCFProperty to find the nearest
/// ancestor with a locationID property. IOKit location IDs
/// encode the full hub port path, so each composite USB device
/// has a unique value and the nearest match is the right one.
fn parent_has_location_id(entry: io_object_t, target: u32) -> bool {
    let key = CFString::new("locationID");
    // SAFETY: entry is a valid io_object_t.  The "IOService" plane
    // name is a well-known constant.  The returned CFTypeRef follows
    // the Create Rule (caller must release).
    let value = unsafe {
        IORegistryEntrySearchCFProperty(
            entry,
            c"IOService".as_ptr(),
            key.as_concrete_TypeRef() as *const _,
            kCFAllocatorDefault,
            kIORegistryIterateParents | kIORegistryIterateRecursively,
        )
    };

    if value.is_null() {
        return false;
    }

    // SAFETY: value is a non-null CFTypeRef that we own per the
    // Create Rule; wrap_under_create_rule takes ownership.
    let cf: CFType = unsafe { TCFType::wrap_under_create_rule(value as *const _) };
    cf.downcast::<CFNumber>()
        .and_then(|n| n.to_i64())
        .is_some_and(|id| u32::try_from(id).ok() == Some(target))
}

/// Read a string property from an IORegistry entry.
fn get_string_property(entry: io_object_t, key: &str) -> Option<String> {
    let cf_key = CFString::new(key);
    // SAFETY: entry is a valid io_object_t.  The returned CFTypeRef
    // follows the Create Rule (caller must release).
    let value = unsafe {
        IORegistryEntryCreateCFProperty(
            entry,
            cf_key.as_concrete_TypeRef() as *const _,
            kCFAllocatorDefault,
            0,
        )
    };

    if value.is_null() {
        return None;
    }

    // SAFETY: value is a non-null CFTypeRef that we own per the
    // Create Rule; wrap_under_create_rule takes ownership.
    let cf: CFType = unsafe { TCFType::wrap_under_create_rule(value) };
    cf.downcast::<CFString>().map(|s| s.to_string())
}
