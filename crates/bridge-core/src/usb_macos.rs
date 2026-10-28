//! `UsbControl` over IOKit. Readers are the USB devices with a CCID-class interface; a reset goes
//! through IOUSBHost. Opening the device without the root-only capture option works as a normal
//! user while Apple's CCID driver holds the reader's interface (checked on macOS 27 with an
//! ACR1255U-J1): `resetWithError` re-enumerates it and the smart-card service picks it up again.

use std::ffi::{CStr, c_char};

use objc2::AllocAnyThread;
use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFString};
use objc2_io_kit::{
    IOIteratorNext, IOObjectRelease, IORegistryEntryCreateCFProperty,
    IORegistryEntryGetParentEntry, IORegistryEntryGetRegistryEntryID, IORegistryEntryIDMatching,
    IOServiceGetMatchingService, IOServiceGetMatchingServices, IOServiceMatching, io_iterator_t,
    io_object_t, kIOMainPortDefault, kIOReturnSuccess, kIOServicePlane,
};
use objc2_io_usb_host::{IOUSBHostDevice, IOUSBHostObjectInitOptions};

use crate::usb::{UsbControl, UsbReader};

/// USB interface class of smart-card readers (CCID).
const CCID_CLASS: i64 = 0x0B;

pub struct MacUsb;

impl UsbControl for MacUsb {
    fn readers(&self) -> Option<Vec<UsbReader>> {
        let interfaces = matching_services(c"IOUSBHostInterface")?;
        let mut readers: Vec<UsbReader> = Vec::new();
        for interface in interfaces {
            if number_property(&interface, "bInterfaceClass") != Some(CCID_CLASS) {
                continue;
            }
            let Some(device) = interface.parent() else {
                continue;
            };
            let Some(id) = device.registry_id() else {
                continue;
            };
            if readers.iter().any(|r| r.id == id) {
                continue; // a second CCID interface of the same device
            }
            let name = [
                string_property(&device, "USB Vendor Name"),
                string_property(&device, "USB Product Name"),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
            readers.push(UsbReader { name, id });
        }
        Some(readers)
    }

    fn reset(&self, reader: &UsbReader) -> Result<(), String> {
        // SAFETY: IORegistryEntryIDMatching returns a fresh dictionary (or None).
        let matching = unsafe { IORegistryEntryIDMatching(reader.id) }
            .ok_or("cannot build an IOKit matching dictionary")?;
        // SAFETY: the dictionary is consumed; the returned service is ours to release.
        let service = Object(unsafe {
            IOServiceGetMatchingService(kIOMainPortDefault, Some(upcast(matching)))
        });
        if service.0 == 0 {
            return Err("the reader is no longer on USB".into());
        }
        let mut error = None;
        // SAFETY: a valid service, no queue (IOUSBHost creates one), no interest handler. The
        // object keeps its own reference to the service.
        let device = unsafe {
            IOUSBHostDevice::initWithIOService_options_queue_error_interestHandler(
                IOUSBHostDevice::alloc(),
                service.0,
                IOUSBHostObjectInitOptions::None,
                None,
                Some(&mut error),
                std::ptr::null_mut(),
            )
        };
        let Some(device) = device else {
            return Err(match error {
                Some(e) => format!("cannot open the reader: {}", e.localizedDescription()),
                None => "cannot open the reader".into(),
            });
        };
        // SAFETY: an open device. On success the device is gone and the object is spent; dropping
        // it releases what is left.
        unsafe { device.resetWithError() }
            .map_err(|e| format!("reset failed: {}", e.localizedDescription()))
    }
}

/// An IOKit object handle, released on drop.
struct Object(io_object_t);

impl Drop for Object {
    fn drop(&mut self) {
        if self.0 != 0 {
            IOObjectRelease(self.0);
        }
    }
}

impl Object {
    fn parent(&self) -> Option<Object> {
        let mut parent: io_object_t = 0;
        let mut plane: [c_char; 128] = [0; 128];
        for (dst, src) in plane.iter_mut().zip(kIOServicePlane.to_bytes()) {
            *dst = *src as c_char;
        }
        // SAFETY: valid entry and out pointer; `plane` is NUL-terminated (it is longer than the name).
        let kr = unsafe { IORegistryEntryGetParentEntry(self.0, &mut plane, &mut parent) };
        (kr == kIOReturnSuccess && parent != 0).then_some(Object(parent))
    }

    fn registry_id(&self) -> Option<u64> {
        let mut id = 0;
        // SAFETY: valid entry and out pointer.
        let kr = unsafe { IORegistryEntryGetRegistryEntryID(self.0, &mut id) };
        (kr == kIOReturnSuccess).then_some(id)
    }
}

fn upcast(d: CFRetained<objc2_core_foundation::CFMutableDictionary>) -> CFRetained<CFDictionary> {
    // SAFETY: a CFMutableDictionary is a CFDictionary.
    unsafe { CFRetained::cast_unchecked(d) }
}

/// Every registered service of IOKit class `class`; `None` if IOKit fails.
fn matching_services(class: &CStr) -> Option<Vec<Object>> {
    // SAFETY: a NUL-terminated class name.
    let matching = unsafe { IOServiceMatching(class.as_ptr()) }?;
    let mut iterator: io_iterator_t = 0;
    // SAFETY: the dictionary is consumed; the iterator is ours to release.
    let kr = unsafe {
        IOServiceGetMatchingServices(kIOMainPortDefault, Some(upcast(matching)), &mut iterator)
    };
    if kr != kIOReturnSuccess {
        tracing::debug!(kr, "IOServiceGetMatchingServices failed");
        return None;
    }
    let iterator = Object(iterator);
    Some(
        std::iter::from_fn(|| match IOIteratorNext(iterator.0) {
            0 => None,
            o => Some(Object(o)),
        })
        .collect(),
    )
}

fn property(entry: &Object, key: &str) -> Option<CFRetained<objc2_core_foundation::CFType>> {
    let key = CFString::from_str(key);
    // SAFETY: valid entry and key; default allocator.
    unsafe { IORegistryEntryCreateCFProperty(entry.0, Some(&key), None, 0) }
}

fn number_property(entry: &Object, key: &str) -> Option<i64> {
    property(entry, key)?.downcast::<CFNumber>().ok()?.as_i64()
}

fn string_property(entry: &Object, key: &str) -> Option<String> {
    let s = property(entry, key)?
        .downcast::<CFString>()
        .ok()?
        .to_string();
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}
