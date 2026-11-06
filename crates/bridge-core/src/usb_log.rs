//! Reader faults that only the macOS system log shows. The USB host controller can fail a transfer
//! from a reader (kernel: `AppleUSBIORequest::complete: device 1 (ACR1255U-J1@00100000) endpoint
//! 0x83: status 0xe00002ed (transaction error)`), and Apple's CCID driver then stops reading from
//! it (`usbsmartcardreaderd … Unable to send IO`). After that the reader still beeps at a tag, but
//! the smart-card service never hears of it: the reader stays listed and empty, so nothing in PC/SC
//! shows the trouble. Seen on an ACR1255U-J1 in production; unplugging it fixed it.
//!
//! The same two lines, in either order, are also logged when a reader goes away (a USB reset or
//! unplugging it), but then the device is torn down too: the kernel's `terminateDevice` comes just
//! before and the driver's `Device terminate` just after. So the pair counts as a fault only once
//! [`TEARDOWN_WINDOW`] has passed with no teardown on either side of it.
//!
//! The app feeds the lines of `log stream --style compact` into a [`UsbFaultLog`]; reader recovery
//! takes the faults from it and resets the reader over USB.

use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::usb::UsbReader;

/// The kernel's and the driver's errors must come within this time of each other (seen: 0–5 ms).
const PAIR_WINDOW: Duration = Duration::from_secs(1);
/// A teardown this close to the pair, before or after, means the reader went away (seen: 1–2 ms).
pub const TEARDOWN_WINDOW: Duration = Duration::from_secs(1);
/// Faults kept until recovery takes them; older ones are dropped (no bridge is running to take them).
const MAX_PENDING: usize = 8;

/// A reader's USB transfer failed and the CCID driver gave up on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbFault {
    /// The device's USB product name as the kernel logs it, e.g. "ACR1255U-J1".
    pub device: String,
    /// The port it is plugged into (macOS `locationID`), if the line gives one.
    pub location: Option<u32>,
    /// What failed, e.g. "endpoint 0x83, status 0xe00002ed (transaction error)".
    pub detail: String,
    /// When the second of the two errors was read.
    pub at: Instant,
}

impl UsbFault {
    /// True if this fault is on `reader`: the same port, or failing that the same product name.
    pub fn is_on(&self, reader: &UsbReader) -> bool {
        match (self.location, reader.identity.location) {
            (Some(a), Some(b)) => a == b,
            _ => {
                !self.device.is_empty()
                    && reader
                        .name
                        .to_ascii_lowercase()
                        .ends_with(&self.device.to_ascii_lowercase())
            }
        }
    }
}

/// A failed transfer from a kernel line, before the driver's error confirms it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TransferError {
    device: String,
    location: Option<u32>,
    detail: String,
}

/// Parse `… kernel[…] … AppleUSBIORequest::complete: device N (NAME@LOCATION) endpoint EP: status
/// CODE (TEXT): …`.
fn transfer_error(line: &str) -> Option<TransferError> {
    if !line.contains(" kernel[") {
        return None;
    }
    let rest = line.split_once("AppleUSBIORequest::complete: device ")?.1;
    let rest = rest.split_once(" (")?.1;
    let (device, rest) = rest.split_once(") endpoint ")?;
    let (endpoint, rest) = rest.split_once(": status ")?;
    // "0xe00002ed (transaction error): 0 bytes transferred"
    let status = rest.find("): ").map_or(rest, |i| &rest[..=i]);
    let (name, location) = match device.rsplit_once('@') {
        Some((n, l)) => (n, u32::from_str_radix(l, 16).ok()),
        None => (device, None),
    };
    Some(TransferError {
        device: name.to_string(),
        location,
        detail: format!("endpoint {endpoint}, status {}", status.trim()),
    })
}

fn driver_gave_up(line: &str) -> bool {
    line.contains(" usbsmartcardreaderd[") && line.contains("Unable to send IO")
}

/// A USB device being torn down: the kernel destroying it (`terminateDevice: destroying … (NAME):
/// reset API call` / `hardware connection lost`) or the CCID driver letting go of it.
fn teardown(line: &str) -> bool {
    (line.contains(" kernel[") && line.contains("::terminateDevice: destroying "))
        || (line.contains(" usbsmartcardreaderd[") && line.contains("] Device terminate"))
}

#[derive(Default)]
struct Inner {
    transfer: Option<(TransferError, Instant)>,
    gave_up: Option<Instant>,
    last_teardown: Option<Instant>,
    /// Pairs waiting out `TEARDOWN_WINDOW`, oldest first.
    pending: Vec<UsbFault>,
}

impl Inner {
    fn pair(&mut self, e: TransferError, now: Instant) {
        self.transfer = None;
        self.gave_up = None;
        if self
            .last_teardown
            .is_some_and(|t| now.saturating_duration_since(t) <= TEARDOWN_WINDOW)
        {
            return; // the reader went away
        }
        if self.pending.len() >= MAX_PENDING {
            self.pending.remove(0);
        }
        self.pending.push(UsbFault {
            device: e.device,
            location: e.location,
            detail: e.detail,
            at: now,
        });
    }
}

/// Turns system log lines into reader faults. Shared by the app's log reader and reader recovery;
/// outlives a bridge that is restarted.
#[derive(Default)]
pub struct UsbFaultLog {
    inner: Mutex<Inner>,
}

impl UsbFaultLog {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// One line of `log stream --style compact`, read just now.
    pub fn feed(&self, line: &str) {
        self.feed_at(line, Instant::now());
    }

    /// One line, read at `now`.
    pub fn feed_at(&self, line: &str, now: Instant) {
        let mut inner = self.lock();
        let recent = |t: Instant| now.saturating_duration_since(t) <= PAIR_WINDOW;
        if let Some(e) = transfer_error(line) {
            if inner.gave_up.is_some_and(recent) {
                inner.pair(e, now);
            } else {
                inner.transfer = Some((e, now));
            }
        } else if driver_gave_up(line) {
            match inner.transfer.take() {
                Some((e, seen)) if recent(seen) => inner.pair(e, now),
                _ => inner.gave_up = Some(now),
            }
        } else if teardown(line) {
            inner.last_teardown = Some(now);
            inner
                .pending
                .retain(|f| now.saturating_duration_since(f.at) > TEARDOWN_WINDOW);
        }
    }

    /// The faults confirmed since the last call, oldest first.
    pub fn take(&self) -> Vec<UsbFault> {
        self.take_at(Instant::now())
    }

    /// The faults confirmed by `now`: no teardown within `TEARDOWN_WINDOW` after them.
    pub fn take_at(&self, now: Instant) -> Vec<UsbFault> {
        let mut inner = self.lock();
        let (ready, waiting) = std::mem::take(&mut inner.pending)
            .into_iter()
            .partition(|f| now.saturating_duration_since(f.at) > TEARDOWN_WINDOW);
        inner.pending = waiting;
        ready
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usb::UsbIdentity;

    // From the kiosk that stopped reporting tags on 2026-11-06.
    const KERNEL: &str = "2026-11-05 19:47:46.571 E  kernel[0:a2af] [com.apple.usb:all] AppleUSBXHCIRequest: AppleUSBIORequest::complete: device 1 (ACR1255U-J1@00100000) endpoint 0x83: status 0xe00002ed (transaction error): 0 bytes transferred";
    const DRIVER: &str = "2026-11-05 19:47:46.576 E  usbsmartcardreaderd[232:bc6] (IOUSBHost) Error: Unable to send IO. with reason: (null)";
    // Logged by the same kiosk's USB Ethernet adapter now and then; not a reader.
    const LAN: &str = "2026-11-05 11:38:18.545 E  kernel[0:a2af] [com.apple.usb:all] AppleUSBXHCIRequest: AppleUSBIORequest::complete: device 3 (USB 10/100/1000 LAN@01230000) endpoint 0x00: status 0xe0005000 (pipe stalled): 6 bytes transferred";
    // Logged on every tag; harmless.
    const DATA_RATE: &str = "2026-11-06 13:25:50.609 E  usbsmartcardreaderd[232:dab34] [com.apple.CryptoTokenKit:ccid] Failed to set frequency and data rate: { messageType: PC_to_RDR_SetDataRateAndClockFrequency dwLength: 8 bSlot: 0 bSeq: 255 bChainParameter: 0  }";

    fn reader(name: &str, location: Option<u32>) -> UsbReader {
        UsbReader {
            name: name.into(),
            id: 1,
            identity: UsbIdentity {
                location,
                ..UsbIdentity::default()
            },
        }
    }

    #[test]
    fn parses_the_kernel_transfer_error() {
        assert_eq!(
            transfer_error(KERNEL),
            Some(TransferError {
                device: "ACR1255U-J1".into(),
                location: Some(0x0010_0000),
                detail: "endpoint 0x83, status 0xe00002ed (transaction error)".into(),
            })
        );
        assert_eq!(
            transfer_error(LAN).map(|e| (e.device, e.location)),
            Some(("USB 10/100/1000 LAN".into(), Some(0x0123_0000)))
        );
        assert_eq!(transfer_error(DRIVER), None);
        assert_eq!(transfer_error(DATA_RATE), None);
        assert_eq!(transfer_error(""), None);
        assert_eq!(
            transfer_error("kernel[0:1] AppleUSBIORequest::complete: device 1 (X"),
            None
        );
        // Not from the kernel.
        assert_eq!(transfer_error(&KERNEL.replace(" kernel[", " other[")), None);
    }

    /// Once a fault would be confirmed.
    fn later(t: Instant) -> Instant {
        t + TEARDOWN_WINDOW + Duration::from_millis(1)
    }

    #[test]
    fn a_transfer_error_and_the_driver_giving_up_is_a_fault_once_no_teardown_follows() {
        let log = UsbFaultLog::default();
        let t0 = Instant::now();
        log.feed_at(KERNEL, t0);
        let t1 = t0 + Duration::from_millis(5);
        log.feed_at(DRIVER, t1);
        assert!(log.take_at(t1).is_empty(), "not confirmed yet");
        assert!(log.take_at(t1 + TEARDOWN_WINDOW).is_empty());
        let faults = log.take_at(later(t1));
        assert_eq!(faults.len(), 1);
        assert_eq!(faults[0].device, "ACR1255U-J1");
        assert_eq!(faults[0].location, Some(0x0010_0000));
        assert_eq!(faults[0].at, t1);
        assert!(log.take_at(later(t1)).is_empty(), "taken once");
        // The pair is used up: another driver error alone is nothing.
        log.feed_at(DRIVER, t1);
        assert!(log.take_at(later(t1)).is_empty());
    }

    #[test]
    fn the_two_errors_count_in_either_order() {
        let log = UsbFaultLog::default();
        let t0 = Instant::now();
        log.feed_at(DRIVER, t0);
        log.feed_at(KERNEL, t0);
        assert_eq!(log.take_at(later(t0)).len(), 1);
    }

    #[test]
    fn either_line_alone_is_not_a_fault() {
        let log = UsbFaultLog::default();
        let t0 = Instant::now();
        log.feed_at(DRIVER, t0);
        log.feed_at(DATA_RATE, t0);
        assert!(log.take_at(later(t0)).is_empty());
        // Too long after the driver's error, then too long after the kernel's.
        let t1 = t0 + PAIR_WINDOW + Duration::from_millis(1);
        log.feed_at(KERNEL, t1);
        let t2 = t1 + PAIR_WINDOW + Duration::from_millis(1);
        log.feed_at(DRIVER, t2);
        assert!(log.take_at(later(t2)).is_empty());
    }

    // An ACR1255U-J1 reset by the bridge on the dev Mac, 2026-11-06 (the same lines as the fault,
    // the other way round, between the teardown lines).
    const RESET: [&str; 6] = [
        "2026-11-06 14:50:42.829 Df kernel[0:159cd86] (IOUSBHostFamily) usb-drd2-port-hs@02100000: AppleUSBHostPort::terminateDevice: destroying 0x072f/223f/0204 (ACR1255U-J1): reset API call",
        "2026-11-06 14:50:42.830 E  usbsmartcardreaderd[759:159be5e] (IOUSBHost) Error: Unable to send IO. with reason: (null)",
        "2026-11-06 14:50:42.830 E  kernel[0:156d163] (IOUSBHostFamily) AppleUSBXHCIRequest: AppleUSBIORequest::complete: device 2 (ACR1255U-J1@02100000) endpoint 0x83: status 0xe00002ed (transaction error): 0 bytes transferred",
        "2026-11-06 14:50:42.832 I  usbsmartcardreaderd[759:159be5e] (IOUSBHost) Notification message 0xe0000110 argument 0x0000000000000000",
        "2026-11-06 14:50:42.832 I  usbsmartcardreaderd[759:159be5e] [com.apple.CryptoTokenKit:ccid] Device terminate",
        "2026-11-06 14:50:42.836 Df kernel[0:159be94] (IOUSBHostFamily) usb-drd2-port-hs@02100000: AppleUSBHostPort::enumerateDeviceComplete_block_invoke: enumerated 0x072f/223f/0204 (ACR1255U-J1 / 1) at 12 Mbps",
    ];
    // The production kiosk when the reader was unplugged by hand.
    const UNPLUGGED: &str = "2026-11-06 13:25:39.772 Df kernel[0:6f7] [com.apple.usb:all] usb-drd0-port-hs@00100000: AppleUSBHostPort::terminateDevice: destroying 0x072f/223f/0204 (ACR1255U-J1): hardware connection lost";
    const DRIVER_LET_GO: &str = "2026-11-06 13:25:39.775 I  usbsmartcardreaderd[232:bc6] [com.apple.CryptoTokenKit:ccid] Device terminate";

    #[test]
    fn a_reader_going_away_is_not_a_fault() {
        assert!(teardown(RESET[0]) && teardown(RESET[4]) && teardown(UNPLUGGED));
        assert!(!teardown(RESET[5]) && !teardown(KERNEL) && !teardown(DRIVER));
        let log = UsbFaultLog::default();
        let t0 = Instant::now();
        // As logged during a reset: the teardown comes first.
        for (i, line) in RESET.iter().enumerate() {
            log.feed_at(line, t0 + Duration::from_millis(i as u64));
        }
        assert!(
            log.take_at(later(t0 + Duration::from_millis(10)))
                .is_empty()
        );
        // The teardown only after the pair (unplugged right after a glitch).
        let t1 = later(t0 + Duration::from_secs(5));
        log.feed_at(KERNEL, t1);
        log.feed_at(DRIVER, t1);
        log.feed_at(UNPLUGGED, t1 + Duration::from_millis(500));
        log.feed_at(DRIVER_LET_GO, t1 + Duration::from_millis(503));
        assert!(
            log.take_at(later(t1 + Duration::from_millis(503)))
                .is_empty()
        );
        // A teardown long before does not hide a fault.
        let t2 = t1 + Duration::from_secs(5);
        log.feed_at(KERNEL, t2);
        log.feed_at(DRIVER, t2);
        assert_eq!(log.take_at(later(t2)).len(), 1);
    }

    #[test]
    fn keeps_only_the_latest_faults() {
        let log = UsbFaultLog::default();
        let t0 = Instant::now();
        for i in 0..20 {
            let at = t0 + Duration::from_secs(i);
            log.feed_at(KERNEL, at);
            log.feed_at(DRIVER, at);
        }
        let last = t0 + Duration::from_secs(19);
        let faults = log.take_at(later(last));
        assert_eq!(faults.len(), MAX_PENDING);
        assert_eq!(faults.last().unwrap().at, last);
    }

    #[test]
    fn matches_the_reader_by_port_or_else_by_name() {
        let log = UsbFaultLog::default();
        let t0 = Instant::now();
        log.feed_at(KERNEL, t0);
        log.feed_at(DRIVER, t0);
        let f = log.take_at(later(t0)).pop().unwrap();
        assert!(f.is_on(&reader("ACS ACR1255U-J1", Some(0x0010_0000))));
        // Another reader of the same model on another port.
        assert!(!f.is_on(&reader("ACS ACR1255U-J1", Some(0x0210_0000))));
        assert!(!f.is_on(&reader("ACS ACR1552", Some(0x0210_0000))));
        // No port known: by name.
        assert!(f.is_on(&reader("ACS ACR1255U-J1", None)));
        assert!(!f.is_on(&reader("ACS ACR1552", None)));
    }
}
