//! macOS USB discovery implementation using IOKit.
//!
//! This module discovers USB devices and monitors for hotplug events using
//! IOKit on macOS systems.
//!
//! ## Architecture
//!
//! USB monitoring runs in a dedicated OS thread with a CFRunLoop for event
//! handling. This design mirrors the Linux udev approach:
//!
//! - Enumerate existing USB devices at startup
//! - Extract VID/PID/serial number from IORegistry properties
//! - Find associated serial port (cu.*) devices
//! - Monitor for add/remove events via IOKit notifications
//!
//! ## Serial Port Naming
//!
//! macOS uses `/dev/cu.*` for callout devices (what we want for serial comms):
//! - `/dev/cu.usbmodemXXXX` - CDC ACM devices
//! - `/dev/cu.usbserial-XXXX` - FTDI and similar USB-serial adapters

use super::{TransportEvent as UsbEvent, UsbDeviceInfo};
use crate::{
    error::{Error, Result},
    tracing::prelude::*,
    transport::TransportEvent,
};
use core_foundation::{
    base::{kCFAllocatorDefault, CFType, TCFType},
    number::CFNumber,
    runloop::{kCFRunLoopDefaultMode, CFRunLoop, CFRunLoopRunResult},
    string::CFString,
};
use io_kit_sys::{
    kIOMasterPortDefault, kIORegistryIterateParents, kIORegistryIterateRecursively,
    keys::{kIOFirstMatchNotification, kIOTerminatedNotification},
    ret::kIOReturnSuccess,
    types::io_iterator_t,
    IOIteratorNext, IONotificationPortCreate, IONotificationPortDestroy,
    IONotificationPortGetRunLoopSource, IOObjectRelease, IORegistryEntryCreateCFProperty,
    IORegistryEntrySearchCFProperty, IOServiceAddMatchingNotification,
    IOServiceGetMatchingServices, IOServiceMatching,
};
use std::{ffi::c_void, sync::OnceLock};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// IOKit USB device class name for matching.
const IOUSB_DEVICE_CLASS_NAME: *const std::ffi::c_char = c"IOUSBHostDevice".as_ptr();

/// macOS IOKit-based USB discovery.
pub struct MacOsIoKitDiscovery {
    /// Master port for IOKit communication (default port).
    /// We use kIOMasterPortDefault which doesn't need explicit cleanup.
    _master_port: mach2::port::mach_port_t,
}

impl MacOsIoKitDiscovery {
    /// Create a new macOS USB discovery instance.
    pub fn new() -> Result<Self> {
        // Verify IOKit is accessible by attempting to create a matching dictionary
        let matching = unsafe { IOServiceMatching(IOUSB_DEVICE_CLASS_NAME) };
        if matching.is_null() {
            return Err(Error::Other(
                "Failed to create IOKit USB matching dictionary".to_string(),
            ));
        }
        // Dictionary is consumed by IOServiceGetMatchingServices, but we're just
        // testing here. Release it since we won't use it.
        // Note: IOServiceMatching returns a CFMutableDictionaryRef that follows
        // the Create Rule, but IOServiceGetMatchingServices consumes it.
        // Since we're not calling that, we need to release.
        unsafe {
            core_foundation::base::CFRelease(matching as *const c_void);
        }

        Ok(Self {
            _master_port: unsafe { kIOMasterPortDefault },
        })
    }

    /// Extract device properties from an IORegistry USB device entry.
    fn extract_device_properties(
        &self,
        device: io_kit_sys::types::io_object_t,
    ) -> Option<DeviceProperties> {
        let vid = get_device_property_number(device, "idVendor")? as u16;
        let pid = get_device_property_number(device, "idProduct")? as u16;
        let serial_number = get_device_property_string(device, "USB Serial Number");
        let manufacturer = get_device_property_string(device, "USB Vendor Name");
        let product = get_device_property_string(device, "USB Product Name");
        let location_id = get_device_property_number(device, "locationID")? as u32;

        Some(DeviceProperties {
            vid,
            pid,
            serial_number,
            manufacturer,
            product,
            location_id,
        })
    }

    /// Build a UsbDeviceInfo from IORegistry device properties.
    fn build_device_info(
        &self,
        device: io_kit_sys::types::io_object_t,
    ) -> Option<UsbDeviceInfo> {
        let props = self.extract_device_properties(device)?;

        // Use location ID as the device path identifier (hex string)
        let device_path = format!("IOKit:0x{:08x}", props.location_id);

        Some(UsbDeviceInfo {
            vid: props.vid,
            pid: props.pid,
            serial_number: props.serial_number,
            manufacturer: props.manufacturer,
            product: props.product,
            device_path,
            serial_ports: OnceLock::new(),
        })
    }

    /// Enumerate currently connected USB devices.
    fn enumerate_devices(&self) -> Result<Vec<UsbDeviceInfo>> {
        let mut devices = Vec::new();

        // Create matching dictionary for USB devices
        let matching = unsafe { IOServiceMatching(IOUSB_DEVICE_CLASS_NAME) };
        if matching.is_null() {
            return Err(Error::Other(
                "Failed to create USB matching dictionary".to_string(),
            ));
        }

        let mut iterator: io_iterator_t = 0;
        let result = unsafe {
            IOServiceGetMatchingServices(kIOMasterPortDefault, matching, &mut iterator)
        };

        if result != kIOReturnSuccess {
            return Err(Error::Other(format!(
                "IOServiceGetMatchingServices failed: 0x{:08x}",
                result
            )));
        }

        // Iterate through all USB devices
        loop {
            let device = unsafe { IOIteratorNext(iterator) };
            if device == 0 {
                break;
            }

            if let Some(info) = self.build_device_info(device) {
                devices.push(info);
            }

            unsafe { IOObjectRelease(device) };
        }

        unsafe { IOObjectRelease(iterator) };

        Ok(devices)
    }
}

/// Helper struct for extracted device properties.
struct DeviceProperties {
    vid: u16,
    pid: u16,
    serial_number: Option<String>,
    manufacturer: Option<String>,
    product: Option<String>,
    location_id: u32,
}

/// Get a numeric property from an IORegistry entry.
fn get_device_property_number(
    device: io_kit_sys::types::io_object_t,
    key: &str,
) -> Option<i64> {
    let cf_key = CFString::new(key);
    let cf_value = unsafe {
        IORegistryEntryCreateCFProperty(
            device,
            cf_key.as_concrete_TypeRef() as *const _,
            kCFAllocatorDefault,
            0,
        )
    };

    if cf_value.is_null() {
        return None;
    }

    // Safety: We own this reference and must release it
    let cf_type: CFType = unsafe { TCFType::wrap_under_create_rule(cf_value as *const _) };

    // Try to downcast to CFNumber
    if let Some(number) = cf_type.downcast::<CFNumber>() {
        number.to_i64()
    } else {
        None
    }
}

/// Get a string property from an IORegistry entry.
fn get_device_property_string(
    device: io_kit_sys::types::io_object_t,
    key: &str,
) -> Option<String> {
    let cf_key = CFString::new(key);
    let cf_value = unsafe {
        IORegistryEntryCreateCFProperty(
            device,
            cf_key.as_concrete_TypeRef() as *const _,
            kCFAllocatorDefault,
            0,
        )
    };

    if cf_value.is_null() {
        return None;
    }

    // Safety: We own this reference and must release it
    let cf_type: CFType = unsafe { TCFType::wrap_under_create_rule(cf_value as *const _) };

    // Try to downcast to CFString
    if let Some(string) = cf_type.downcast::<CFString>() {
        Some(string.to_string())
    } else {
        None
    }
}

/// Find serial ports associated with a USB device by location ID.
///
/// Searches for IOSerialBSDClient entries that are children of USB devices
/// with the matching location ID.
pub(super) fn find_serial_ports_for_device(device_path: &str) -> Result<Vec<String>> {
    // Parse location ID from device path (format: "IOKit:0xXXXXXXXX")
    let location_id = device_path
        .strip_prefix("IOKit:0x")
        .and_then(|s| u32::from_str_radix(s, 16).ok())
        .ok_or_else(|| Error::Other(format!("Invalid device path: {}", device_path)))?;

    let mut ports = Vec::new();

    // Create matching dictionary for serial devices
    let matching = unsafe {
        IOServiceMatching(c"IOSerialBSDClient".as_ptr())
    };
    if matching.is_null() {
        return Err(Error::Other(
            "Failed to create serial matching dictionary".to_string(),
        ));
    }

    let mut iterator: io_iterator_t = 0;
    let result = unsafe {
        IOServiceGetMatchingServices(kIOMasterPortDefault, matching, &mut iterator)
    };

    if result != kIOReturnSuccess {
        return Err(Error::Other(format!(
            "IOServiceGetMatchingServices failed for serial: 0x{:08x}",
            result
        )));
    }

    // Iterate through serial devices and check if they belong to our USB device
    loop {
        let serial_device = unsafe { IOIteratorNext(iterator) };
        if serial_device == 0 {
            break;
        }

        // Get the callout device path (e.g., /dev/cu.usbmodem1234)
        if let Some(callout_device) = get_device_property_string(serial_device, "IOCalloutDevice") {
            // Walk up the parent chain to find the USB device and check location ID
            if device_has_location_id(serial_device, location_id) {
                ports.push(callout_device);
            }
        }

        unsafe { IOObjectRelease(serial_device) };
    }

    unsafe { IOObjectRelease(iterator) };

    // Sort for consistent ordering
    ports.sort();

    Ok(ports)
}

/// Check if a device or any of its parents has the specified location ID.
fn device_has_location_id(device: io_kit_sys::types::io_object_t, target_location_id: u32) -> bool {
    // Search up the parent chain for the locationID property
    let cf_key = CFString::new("locationID");
    let cf_value = unsafe {
        IORegistryEntrySearchCFProperty(
            device,
            c"IOService".as_ptr(),
            cf_key.as_concrete_TypeRef() as *const _,
            kCFAllocatorDefault,
            kIORegistryIterateParents | kIORegistryIterateRecursively,
        )
    };

    if cf_value.is_null() {
        return false;
    }

    let cf_type: CFType = unsafe { TCFType::wrap_under_create_rule(cf_value as *const _) };

    if let Some(number) = cf_type.downcast::<CFNumber>() {
        if let Some(location_id) = number.to_i64() {
            return location_id as u32 == target_location_id;
        }
    }

    false
}

impl super::UsbDiscoveryImpl for MacOsIoKitDiscovery {
    fn monitor_blocking(
        self: Box<Self>,
        event_tx: mpsc::Sender<TransportEvent>,
        shutdown: CancellationToken,
    ) -> Result<()> {
        // Initial enumeration - send Connected events for existing devices
        for device_info in self.enumerate_devices()? {
            debug!(
                vid = %format!("{:04x}", device_info.vid),
                pid = %format!("{:04x}", device_info.pid),
                manufacturer = ?device_info.manufacturer,
                product = ?device_info.product,
                "USB device enumerated"
            );

            let usb_event = UsbEvent::UsbDeviceConnected(device_info);
            let transport_event = TransportEvent::Usb(usb_event);

            if event_tx.blocking_send(transport_event).is_err() {
                info!("Event receiver dropped during enumeration");
                return Ok(());
            }
        }

        // Set up IOKit notification port for hotplug events
        let notify_port = unsafe { IONotificationPortCreate(kIOMasterPortDefault) };
        if notify_port.is_null() {
            return Err(Error::Other(
                "Failed to create IOKit notification port".to_string(),
            ));
        }

        // Get the run loop source from the notification port
        let run_loop_source = unsafe { IONotificationPortGetRunLoopSource(notify_port) };
        if run_loop_source.is_null() {
            unsafe { IONotificationPortDestroy(notify_port) };
            return Err(Error::Other(
                "Failed to get run loop source from notification port".to_string(),
            ));
        }

        // Add the notification source to the current run loop
        let run_loop = CFRunLoop::get_current();
        unsafe {
            core_foundation::runloop::CFRunLoopAddSource(
                run_loop.as_concrete_TypeRef(),
                run_loop_source,
                kCFRunLoopDefaultMode,
            );
        }

        // Create callback context with our sender
        let context = Box::new(NotificationContext {
            event_tx: event_tx.clone(),
            discovery: *self,
        });
        let context_ptr = Box::into_raw(context);

        // Register for device arrival notifications
        let mut add_iterator: io_iterator_t = 0;
        let matching_add = unsafe { IOServiceMatching(IOUSB_DEVICE_CLASS_NAME) };
        if !matching_add.is_null() {
            let result = unsafe {
                IOServiceAddMatchingNotification(
                    notify_port,
                    kIOFirstMatchNotification as *mut _,
                    matching_add,
                    device_added_callback,
                    context_ptr as *mut c_void,
                    &mut add_iterator,
                )
            };

            if result == kIOReturnSuccess {
                // Drain the iterator (required to arm the notification)
                drain_iterator(add_iterator);
            } else {
                warn!("Failed to register for USB add notifications: 0x{:08x}", result);
            }
        }

        // Register for device removal notifications
        let mut remove_iterator: io_iterator_t = 0;
        let matching_remove = unsafe { IOServiceMatching(IOUSB_DEVICE_CLASS_NAME) };
        if !matching_remove.is_null() {
            let result = unsafe {
                IOServiceAddMatchingNotification(
                    notify_port,
                    kIOTerminatedNotification as *mut _,
                    matching_remove,
                    device_removed_callback,
                    context_ptr as *mut c_void,
                    &mut remove_iterator,
                )
            };

            if result == kIOReturnSuccess {
                // Drain the iterator (required to arm the notification)
                drain_iterator(remove_iterator);
            } else {
                warn!("Failed to register for USB remove notifications: 0x{:08x}", result);
            }
        }

        // Run the event loop, checking for shutdown periodically
        info!("macOS USB monitor started");
        loop {
            if shutdown.is_cancelled() {
                break;
            }

            // Run the loop for a short interval, then check shutdown
            let result = unsafe {
                core_foundation::runloop::CFRunLoopRunInMode(
                    kCFRunLoopDefaultMode,
                    0.5, // 500ms timeout
                    1,   // returnAfterSourceHandled = true
                )
            };

            // Check if the run loop was stopped or had issues
            if result == CFRunLoopRunResult::Stopped as i32 {
                break;
            }
        }

        // Cleanup
        info!("macOS USB monitor shutting down");
        unsafe {
            core_foundation::runloop::CFRunLoopRemoveSource(
                run_loop.as_concrete_TypeRef(),
                run_loop_source,
                kCFRunLoopDefaultMode,
            );
            IONotificationPortDestroy(notify_port);

            // Clean up iterators
            if add_iterator != 0 {
                IOObjectRelease(add_iterator);
            }
            if remove_iterator != 0 {
                IOObjectRelease(remove_iterator);
            }

            // Reclaim and drop the context
            drop(Box::from_raw(context_ptr));
        }

        Ok(())
    }
}

/// Context passed to IOKit notification callbacks.
struct NotificationContext {
    event_tx: mpsc::Sender<TransportEvent>,
    discovery: MacOsIoKitDiscovery,
}

/// Drain an IOKit iterator without processing (required to arm notifications).
fn drain_iterator(iterator: io_iterator_t) {
    loop {
        let device = unsafe { IOIteratorNext(iterator) };
        if device == 0 {
            break;
        }
        unsafe { IOObjectRelease(device) };
    }
}

/// Callback invoked when a USB device is added.
unsafe extern "C" fn device_added_callback(refcon: *mut c_void, iterator: io_iterator_t) {
    let context = unsafe { &*(refcon as *const NotificationContext) };

    loop {
        let device = unsafe { IOIteratorNext(iterator) };
        if device == 0 {
            break;
        }

        if let Some(device_info) = context.discovery.build_device_info(device) {
            debug!(
                vid = %format!("{:04x}", device_info.vid),
                pid = %format!("{:04x}", device_info.pid),
                manufacturer = ?device_info.manufacturer,
                product = ?device_info.product,
                "USB device added"
            );

            let usb_event = UsbEvent::UsbDeviceConnected(device_info);
            let transport_event = TransportEvent::Usb(usb_event);

            // Use blocking_send since we're in a callback (not async context)
            let _ = context.event_tx.blocking_send(transport_event);
        }

        unsafe { IOObjectRelease(device) };
    }
}

/// Callback invoked when a USB device is removed.
unsafe extern "C" fn device_removed_callback(refcon: *mut c_void, iterator: io_iterator_t) {
    let context = unsafe { &*(refcon as *const NotificationContext) };

    loop {
        let device = unsafe { IOIteratorNext(iterator) };
        if device == 0 {
            break;
        }

        // Try to get the location ID for the device path
        if let Some(location_id) = get_device_property_number(device, "locationID") {
            let device_path = format!("IOKit:0x{:08x}", location_id as u32);

            debug!(device_path = %device_path, "USB device removed");

            let usb_event = UsbEvent::UsbDeviceDisconnected { device_path };
            let transport_event = TransportEvent::Usb(usb_event);

            let _ = context.event_tx.blocking_send(transport_event);
        }

        unsafe { IOObjectRelease(device) };
    }
}
