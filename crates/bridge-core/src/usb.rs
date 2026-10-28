//! Smart-card readers as seen on USB, independent of PC/SC. Tells a reader that is still plugged in
//! but no longer listed by the smart-card service from one that was unplugged, and resets a reader
//! (re-enumerates it) as the software equivalent of unplugging it. Only macOS implements this;
//! elsewhere the bridge cannot see USB.

use std::sync::Arc;

/// A USB device with a smart-card (CCID class) interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbReader {
    /// "Vendor Product" from the USB strings, e.g. "ACS ACR1255U-J1". macOS names the PC/SC reader
    /// the same way, plus a slot suffix on multi-slot readers.
    pub name: String,
    /// The OS's handle for this attachment. It changes when the device re-enumerates (replug or
    /// reset), so it identifies one attachment, not the reader.
    pub id: u64,
}

impl UsbReader {
    /// True if the PC/SC reader `pcsc_name` is (a slot of) this device.
    pub fn matches(&self, pcsc_name: &str) -> bool {
        !self.name.is_empty()
            && pcsc_name
                .to_ascii_lowercase()
                .starts_with(&self.name.to_ascii_lowercase())
    }
}

pub trait UsbControl: Send + Sync + 'static {
    /// The smart-card readers on USB, or `None` if this platform cannot see USB.
    fn readers(&self) -> Option<Vec<UsbReader>>;
    /// Reset `reader` and let the OS enumerate it again, like unplugging it and plugging it back
    /// in, except that the reader keeps its power.
    fn reset(&self, reader: &UsbReader) -> Result<(), String>;
}

/// For platforms where the bridge cannot see USB.
pub struct NoUsb;

impl UsbControl for NoUsb {
    fn readers(&self) -> Option<Vec<UsbReader>> {
        None
    }

    fn reset(&self, _reader: &UsbReader) -> Result<(), String> {
        Err("USB reset is not supported on this platform".into())
    }
}

/// The USB access of the running OS.
pub fn system() -> Arc<dyn UsbControl> {
    #[cfg(target_os = "macos")]
    return Arc::new(crate::usb_macos::MacUsb);
    #[cfg(not(target_os = "macos"))]
    return Arc::new(NoUsb);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_pcsc_names() {
        let r = UsbReader {
            name: "ACS ACR1255U-J1".into(),
            id: 1,
        };
        assert!(r.matches("ACS ACR1255U-J1"));
        assert!(r.matches("ACS ACR1255U-J1 00 00"));
        assert!(r.matches("acs acr1255u-j1"));
        assert!(!r.matches("ACS ACR1552 1S CL Reader PICC 0"));
        let unnamed = UsbReader {
            name: String::new(),
            id: 2,
        };
        assert!(!unnamed.matches("ACS ACR1255U-J1"));
    }
}
