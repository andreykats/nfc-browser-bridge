//! Smart-card readers as seen on USB, independent of PC/SC. Tells a reader that is still plugged in
//! but no longer listed by the smart-card service from one that was unplugged, and resets a reader
//! (re-enumerates it) as the software equivalent of unplugging it. Only macOS implements this;
//! elsewhere the bridge cannot see USB.

use std::fmt;
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
    pub identity: UsbIdentity,
}

/// What USB says about the device, for the log: which unit and firmware, and where it is plugged in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsbIdentity {
    pub vendor_id: Option<u16>,
    pub product_id: Option<u16>,
    /// Device release (`bcdDevice`); ACS uses it for the firmware version.
    pub release: Option<u16>,
    pub serial: Option<String>,
    /// The port it is plugged into (macOS `locationID`): the same port gives the same value.
    pub location: Option<u32>,
    /// Bus power the host allocated to it, in mA.
    pub power_ma: Option<u32>,
}

impl fmt::Display for UsbIdentity {
    /// E.g. "072F:2303 release 1.06 serial RR673-000423 port 0x02100000 500 mA".
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        if let (Some(v), Some(p)) = (self.vendor_id, self.product_id) {
            parts.push(format!("{v:04X}:{p:04X}"));
        }
        if let Some(r) = self.release {
            parts.push(format!("release {:X}.{:02X}", r >> 8, r & 0xFF));
        }
        if let Some(s) = &self.serial {
            parts.push(format!("serial {s}"));
        }
        if let Some(l) = self.location {
            parts.push(format!("port {l:#010x}"));
        }
        if let Some(p) = self.power_ma {
            parts.push(format!("{p} mA"));
        }
        if parts.is_empty() {
            f.write_str("unknown")
        } else {
            f.write_str(&parts.join(" "))
        }
    }
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
            identity: UsbIdentity::default(),
        };
        assert!(r.matches("ACS ACR1255U-J1"));
        assert!(r.matches("ACS ACR1255U-J1 00 00"));
        assert!(r.matches("acs acr1255u-j1"));
        assert!(!r.matches("ACS ACR1552 1S CL Reader PICC 0"));
        let unnamed = UsbReader {
            name: String::new(),
            id: 2,
            identity: UsbIdentity::default(),
        };
        assert!(!unnamed.matches("ACS ACR1255U-J1"));
    }

    #[test]
    fn describes_identity() {
        let id = UsbIdentity {
            vendor_id: Some(0x072F),
            product_id: Some(0x2303),
            release: Some(0x0106),
            serial: Some("RR673-000423".into()),
            location: Some(0x0210_0000),
            power_ma: Some(500),
        };
        assert_eq!(
            id.to_string(),
            "072F:2303 release 1.06 serial RR673-000423 port 0x02100000 500 mA"
        );
        assert_eq!(UsbIdentity::default().to_string(), "unknown");
    }
}
