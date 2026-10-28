//! A scriptable in-memory PC/SC for tests: readers come and go, cards answer FF CA / FF B0 / FF D6
//! from an NTAG memory image, and failures can be injected. `FakeUsb` adds the readers' USB side:
//! a reader can stay on USB while PC/SC loses it, and a USB reset brings it back.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::backend::{
    Disposition, PcscBackend, PcscCard, PcscContext, PcscError, ReaderWatch, Transmit, state,
};
use crate::usb::{UsbControl, UsbReader};

/// How long a fake reader is gone from PC/SC after a USB reset.
const USB_RESET_GAP: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NtagType {
    Ntag213,
    Ntag215,
    Ntag216,
}

impl NtagType {
    pub fn total_pages(self) -> usize {
        match self {
            Self::Ntag213 => 45,
            Self::Ntag215 => 135,
            Self::Ntag216 => 231,
        }
    }

    fn cc_size(self) -> u8 {
        match self {
            Self::Ntag213 => 0x12,
            Self::Ntag215 => 0x3E,
            Self::Ntag216 => 0x6D,
        }
    }
}

/// ATR of an ISO 14443-3 Type 2 card (PC/SC part 3 card name 00 03), as seen on the ACR1255U-J1.
pub const ATR_ULTRALIGHT: &str = "3B8F8001804F0CA0000003060300030000000068";
/// ATR of a MIFARE Classic 1K (card name 00 01).
pub const ATR_CLASSIC_1K: &str = "3B8F8001804F0CA000000306030001000000006A";

#[derive(Debug, Clone)]
pub struct FakeCard {
    pub atr: Vec<u8>,
    pub uid: Vec<u8>,
    pub memory: Vec<u8>,
    /// Set by a failed command; every APDU answers 6F00 until the card is reset.
    pub wedged: bool,
}

impl FakeCard {
    /// A blank NTAG with a valid CC and an empty NDEF TLV.
    pub fn ntag(kind: NtagType, uid: [u8; 7]) -> Self {
        let mut memory = vec![0u8; kind.total_pages() * 4];
        memory[0..3].copy_from_slice(&uid[0..3]);
        memory[3] = 0x88 ^ uid[0] ^ uid[1] ^ uid[2];
        memory[4..8].copy_from_slice(&uid[3..7]);
        memory[8] = uid[3] ^ uid[4] ^ uid[5] ^ uid[6];
        memory[9] = 0x48;
        memory[12..16].copy_from_slice(&[0xE1, 0x10, kind.cc_size(), 0x00]);
        memory[16..19].copy_from_slice(&[0x03, 0x00, 0xFE]);
        Self {
            atr: crate::protocol::decode_hex(ATR_ULTRALIGHT).unwrap(),
            uid: uid.to_vec(),
            memory,
            wedged: false,
        }
    }

    fn respond(&mut self, apdu: &[u8]) -> Vec<u8> {
        const OK: [u8; 2] = [0x90, 0x00];
        if self.wedged {
            return vec![0x6F, 0x00];
        }
        match apdu {
            [0xFF, 0xCA, 0x00, 0x00, _] => [self.uid.as_slice(), &OK].concat(),
            [0xFF, 0xB0, p1, p2, le] => {
                let start = (usize::from(*p1) << 8 | usize::from(*p2)) * 4;
                if start >= self.memory.len() {
                    self.wedged = true;
                    return vec![0x63, 0x00];
                }
                // NTAG READ rolls over to page 0 past the end of memory.
                let mut out: Vec<u8> = (0..usize::from(*le))
                    .map(|i| self.memory[(start + i) % self.memory.len()])
                    .collect();
                out.extend_from_slice(&OK);
                out
            }
            [0xFF, 0xD6, p1, p2, 4, data @ ..] if data.len() == 4 => {
                let start = (usize::from(*p1) << 8 | usize::from(*p2)) * 4;
                if start + 4 > self.memory.len() {
                    self.wedged = true;
                    return vec![0x63, 0x00];
                }
                self.memory[start..start + 4].copy_from_slice(data);
                OK.to_vec()
            }
            _ => vec![0x6A, 0x81],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultOp {
    Establish,
    Connect,
    Transmit,
    ListReaders,
    GetStatusChange,
}

struct FakeReader {
    name: String,
    card: Option<FakeCard>,
    /// Bumped on every card insertion/removal; reported in the upper 16 bits of the state.
    events: u32,
    /// Connects left that fail with SHARING_VIOLATION.
    held: u32,
}

impl FakeReader {
    fn raw_state(&self) -> u32 {
        let presence = if self.card.is_some() {
            state::PRESENT
        } else {
            state::EMPTY
        };
        (self.events << 16) | presence
    }
}

#[derive(Default)]
struct World {
    readers: Vec<FakeReader>,
    /// Bumped on reader arrival/removal (PnP).
    reader_events: u32,
    service_up: bool,
    /// Bumped whenever the service restarts; older contexts become invalid.
    service_epoch: u64,
    pnp_supported: bool,
    /// (operation, successful calls to let through first, error)
    faults: VecDeque<(FaultOp, u32, PcscError)>,
    sw_overrides: Vec<(Vec<u8>, [u8; 2], u32)>,
    /// The next APDU takes this long (a reader that hangs).
    transmit_delay: Option<Duration>,
    apdu_log: Vec<(String, Vec<u8>)>,
    disconnects: Vec<Disposition>,
    resets: u32,
    connects: u32,
    open_handles: std::collections::HashMap<String, u32>,
    /// USB devices (named like their PC/SC reader) and their attachment ids.
    usb: Vec<(String, u64)>,
    next_usb_id: u64,
    usb_resets: u32,
    /// Readers on USB that PC/SC does not list, and when a USB reset brings them back (`None`:
    /// only a reset does).
    lost: Vec<(FakeReader, Option<Instant>)>,
    /// Readers whose cards answer every command with COMM_ERROR until a USB reset.
    unresponsive: HashSet<String>,
    /// USB resets change the id but bring nothing back.
    usb_reset_broken: bool,
}

impl World {
    fn take_fault(&mut self, op: FaultOp) -> Result<(), PcscError> {
        let Some(i) = self.faults.iter().position(|(o, _, _)| *o == op) else {
            return Ok(());
        };
        if self.faults[i].1 > 0 {
            self.faults[i].1 -= 1;
            return Ok(());
        }
        Err(self.faults.remove(i).unwrap().2)
    }

    fn check_epoch(&self, epoch: u64) -> Result<(), PcscError> {
        if !self.service_up {
            Err(PcscError::SERVICE_STOPPED)
        } else if epoch != self.service_epoch {
            Err(PcscError::INVALID_HANDLE)
        } else {
            Ok(())
        }
    }

    fn reader(&mut self, name: &str) -> Option<&mut FakeReader> {
        self.readers.iter_mut().find(|r| r.name == name)
    }

    /// Bring back readers whose USB reset has finished.
    fn restore_reset_readers(&mut self) {
        let now = Instant::now();
        let (back, still): (Vec<_>, Vec<_>) = std::mem::take(&mut self.lost)
            .into_iter()
            .partition(|(_, at)| at.is_some_and(|t| t <= now));
        self.lost = still;
        for (mut r, _) in back {
            // Re-enumerated: a card on it is a fresh placement as far as PC/SC knows.
            r.events += 1;
            self.readers.push(r);
            self.reader_events += 1;
        }
    }
}

type Shared = Arc<(Mutex<World>, Condvar)>;

/// Scriptable fake PC/SC. Clones share the same world.
#[derive(Clone)]
pub struct FakePcsc {
    world: Shared,
}

impl Default for FakePcsc {
    fn default() -> Self {
        Self::new()
    }
}

impl FakePcsc {
    pub fn new() -> Self {
        let world = World {
            service_up: true,
            pnp_supported: true,
            ..World::default()
        };
        Self {
            world: Arc::new((Mutex::new(world), Condvar::new())),
        }
    }

    fn lock(&self) -> MutexGuard<'_, World> {
        self.world.0.lock().unwrap()
    }

    fn changed(&self, f: impl FnOnce(&mut World)) {
        f(&mut self.lock());
        self.world.1.notify_all();
    }

    pub fn add_reader(&self, name: &str) {
        self.changed(|w| {
            w.readers.push(FakeReader {
                name: name.into(),
                card: None,
                events: 0,
                held: 0,
            });
            w.reader_events += 1;
        });
    }

    pub fn remove_reader(&self, name: &str) {
        self.changed(|w| {
            w.readers.retain(|r| r.name != name);
            w.reader_events += 1;
        });
    }

    pub fn insert_card(&self, reader: &str, card: FakeCard) {
        self.changed(|w| {
            let r = w.reader(reader).expect("no such fake reader");
            r.card = Some(card);
            r.events += 1;
        });
    }

    pub fn remove_card(&self, reader: &str) -> Option<FakeCard> {
        let mut out = None;
        self.changed(|w| {
            let r = w.reader(reader).expect("no such fake reader");
            out = r.card.take();
            r.events += 1;
        });
        out
    }

    pub fn card(&self, reader: &str) -> Option<FakeCard> {
        self.lock().reader(reader).and_then(|r| r.card.clone())
    }

    /// Stop the service (all contexts fail) or start it again (new contexts work, old ones stay invalid).
    pub fn set_service(&self, up: bool) {
        self.changed(|w| {
            w.service_up = up;
            if up {
                w.service_epoch += 1;
            }
        });
    }

    pub fn set_pnp_supported(&self, supported: bool) {
        self.changed(|w| w.pnp_supported = supported);
    }

    /// The next `op` fails with `err` (once).
    pub fn fail_next(&self, op: FaultOp, err: PcscError) {
        self.fail_after(op, 0, err);
    }

    /// Let `skip` calls of `op` succeed, then fail the next one with `err` (once).
    pub fn fail_after(&self, op: FaultOp, skip: u32, err: PcscError) {
        self.changed(|w| w.faults.push_back((op, skip, err)));
    }

    /// The next `n` connects to `reader` fail with SHARING_VIOLATION.
    pub fn hold_exclusive(&self, reader: &str, n: u32) {
        self.changed(|w| w.reader(reader).expect("no such fake reader").held = n);
    }

    /// The next `times` APDUs starting with `prefix` answer `sw` without executing.
    pub fn override_sw(&self, prefix: &[u8], sw: [u8; 2], times: u32) {
        self.changed(|w| w.sw_overrides.push((prefix.to_vec(), sw, times)));
    }

    /// The next APDU takes `d` to answer.
    pub fn delay_next_transmit(&self, d: Duration) {
        self.changed(|w| w.transmit_delay = Some(d));
    }

    pub fn apdu_log(&self) -> Vec<(String, Vec<u8>)> {
        self.lock().apdu_log.clone()
    }

    pub fn disconnects(&self) -> Vec<Disposition> {
        self.lock().disconnects.clone()
    }

    pub fn resets(&self) -> u32 {
        self.lock().resets
    }

    pub fn connects(&self) -> u32 {
        self.lock().connects
    }

    /// Handles currently open on `reader` (connected and not yet disconnected/dropped).
    pub fn open_handles(&self, reader: &str) -> u32 {
        self.lock().open_handles.get(reader).copied().unwrap_or(0)
    }

    /// Put `reader` on USB as well (see `usb`).
    pub fn plug_usb(&self, reader: &str) {
        self.changed(|w| {
            w.next_usb_id += 1;
            let id = w.next_usb_id;
            w.usb.push((reader.into(), id));
        });
    }

    /// PC/SC stops listing `reader` although it stays on USB, until a USB reset.
    pub fn lose_reader(&self, reader: &str) {
        self.changed(|w| {
            let i = w
                .readers
                .iter()
                .position(|r| r.name == reader)
                .expect("no such fake reader");
            let r = w.readers.remove(i);
            w.lost.push((r, None));
            w.reader_events += 1;
        });
    }

    /// Cards on `reader` stop answering (COMM_ERROR) until a USB reset.
    pub fn make_unresponsive(&self, reader: &str) {
        self.changed(|w| {
            w.unresponsive.insert(reader.into());
        });
    }

    /// USB resets stop (or start again) bringing readers back.
    pub fn set_usb_reset_broken(&self, broken: bool) {
        self.changed(|w| w.usb_reset_broken = broken);
    }

    pub fn usb_resets(&self) -> u32 {
        self.lock().usb_resets
    }

    /// The USB side of this fake.
    pub fn usb(&self) -> FakeUsb {
        FakeUsb {
            world: self.world.clone(),
        }
    }
}

/// `UsbControl` over a `FakePcsc` world. A reset gives the device a new id, hides its reader from
/// PC/SC for a moment, then lists it again (with any card on it) and clears `make_unresponsive`.
#[derive(Clone)]
pub struct FakeUsb {
    world: Shared,
}

impl UsbControl for FakeUsb {
    fn readers(&self) -> Option<Vec<UsbReader>> {
        let w = self.world.0.lock().unwrap();
        Some(
            w.usb
                .iter()
                .map(|(name, id)| UsbReader {
                    name: name.clone(),
                    id: *id,
                })
                .collect(),
        )
    }

    fn reset(&self, reader: &UsbReader) -> Result<(), String> {
        {
            let mut guard = self.world.0.lock().unwrap();
            let w = &mut *guard;
            let Some(dev) = w.usb.iter_mut().find(|(_, id)| *id == reader.id) else {
                return Err("the reader is no longer on USB".into());
            };
            w.next_usb_id += 1;
            dev.1 = w.next_usb_id;
            let name = dev.0.clone();
            w.usb_resets += 1;
            if w.usb_reset_broken {
                return Ok(());
            }
            w.unresponsive.remove(&name);
            let back = Some(Instant::now() + USB_RESET_GAP);
            if let Some(lost) = w.lost.iter_mut().find(|(r, _)| r.name == name) {
                lost.1 = back;
            } else if let Some(i) = w.readers.iter().position(|r| r.name == name) {
                let r = w.readers.remove(i);
                w.lost.push((r, back));
                w.reader_events += 1;
            }
        }
        self.world.1.notify_all();
        Ok(())
    }
}

impl PcscBackend for FakePcsc {
    type Context = FakeContext;

    fn establish(&self) -> Result<FakeContext, PcscError> {
        let mut w = self.lock();
        w.take_fault(FaultOp::Establish)?;
        if !w.service_up {
            return Err(PcscError::NO_SERVICE);
        }
        Ok(FakeContext {
            world: self.world.clone(),
            epoch: w.service_epoch,
            cancelled: Arc::new(AtomicBool::new(false)),
        })
    }
}

#[derive(Clone)]
pub struct FakeContext {
    world: Shared,
    epoch: u64,
    cancelled: Arc<AtomicBool>,
}

impl FakeContext {
    fn lock(&self) -> MutexGuard<'_, World> {
        self.world.0.lock().unwrap()
    }
}

impl PcscContext for FakeContext {
    type Card = FakeHandle;

    fn list_readers(&self) -> Result<Vec<String>, PcscError> {
        let mut w = self.lock();
        w.take_fault(FaultOp::ListReaders)?;
        w.check_epoch(self.epoch)?;
        w.restore_reset_readers();
        Ok(w.readers.iter().map(|r| r.name.clone()).collect())
    }

    fn get_status_change(
        &self,
        timeout: Duration,
        readers: &mut [ReaderWatch],
    ) -> Result<(), PcscError> {
        let deadline = Instant::now() + timeout;
        let mut w = self.lock();
        loop {
            if self.cancelled.swap(false, Ordering::SeqCst) {
                return Err(PcscError::CANCELLED);
            }
            w.take_fault(FaultOp::GetStatusChange)?;
            w.check_epoch(self.epoch)?;
            w.restore_reset_readers();

            let mut any = false;
            for watch in readers.iter_mut() {
                let (raw, atr) = if watch.is_pnp() {
                    if w.pnp_supported {
                        (w.reader_events << 16, Vec::new())
                    } else {
                        (state::UNKNOWN, Vec::new())
                    }
                } else {
                    match w.readers.iter().find(|r| r.name == watch.name) {
                        Some(r) => (
                            r.raw_state(),
                            r.card.as_ref().map(|c| c.atr.clone()).unwrap_or_default(),
                        ),
                        None => (state::UNKNOWN, Vec::new()),
                    }
                };
                let changed = raw != watch.current_state & !state::CHANGED;
                watch.event_state = if changed { raw | state::CHANGED } else { raw };
                watch.atr = atr;
                any |= changed;
            }
            if any {
                return Ok(());
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(PcscError::TIMEOUT);
            }
            // Wake up when a reset reader comes back, as PnP would.
            let wake = w
                .lost
                .iter()
                .filter_map(|(_, at)| *at)
                .fold(deadline, Instant::min);
            w = self
                .world
                .1
                .wait_timeout(w, wake.saturating_duration_since(now))
                .unwrap()
                .0;
        }
    }

    fn cancel(&self) -> Result<(), PcscError> {
        let _w = self.lock();
        self.cancelled.store(true, Ordering::SeqCst);
        self.world.1.notify_all();
        Ok(())
    }

    fn connect(&self, reader: &str) -> Result<FakeHandle, PcscError> {
        let mut w = self.lock();
        w.take_fault(FaultOp::Connect)?;
        w.check_epoch(self.epoch)?;
        w.connects += 1;
        let r = w.reader(reader).ok_or(PcscError::UNKNOWN_READER)?;
        if r.card.is_none() {
            return Err(PcscError::NO_SMARTCARD);
        }
        if r.held > 0 {
            r.held -= 1;
            return Err(PcscError::SHARING_VIOLATION);
        }
        let insertion = r.events;
        *w.open_handles.entry(reader.into()).or_default() += 1;
        Ok(FakeHandle {
            ctx: self.clone(),
            reader: reader.into(),
            insertion,
        })
    }
}

pub struct FakeHandle {
    ctx: FakeContext,
    reader: String,
    insertion: u32,
}

impl Drop for FakeHandle {
    fn drop(&mut self) {
        if let Some(n) = self.ctx.lock().open_handles.get_mut(&self.reader) {
            *n = n.saturating_sub(1);
        }
    }
}

impl FakeHandle {
    fn with_card<R>(
        &self,
        f: impl FnOnce(&mut World, usize) -> Result<R, PcscError>,
    ) -> Result<R, PcscError> {
        let mut w = self.ctx.lock();
        w.check_epoch(self.ctx.epoch)?;
        let idx = w
            .readers
            .iter()
            .position(|r| r.name == self.reader)
            .ok_or(PcscError::READER_UNAVAILABLE)?;
        let r = &w.readers[idx];
        if r.card.is_none() || r.events != self.insertion {
            return Err(PcscError::REMOVED_CARD);
        }
        f(&mut w, idx)
    }
}

impl Transmit for FakeHandle {
    fn transmit(&self, apdu: &[u8]) -> Result<Vec<u8>, PcscError> {
        // Sleep without the world lock, so only this caller hangs.
        let delay = self.ctx.lock().transmit_delay.take();
        if let Some(d) = delay {
            std::thread::sleep(d);
        }
        self.with_card(|w, idx| {
            w.take_fault(FaultOp::Transmit)?;
            if w.unresponsive.contains(&self.reader) {
                return Err(PcscError::COMM_ERROR);
            }
            w.apdu_log.push((self.reader.clone(), apdu.to_vec()));
            if let Some(o) = w
                .sw_overrides
                .iter_mut()
                .find(|(p, _, n)| *n > 0 && apdu.starts_with(p))
            {
                o.2 -= 1;
                return Ok(o.1.to_vec());
            }
            Ok(w.readers[idx].card.as_mut().unwrap().respond(apdu))
        })
    }
}

impl PcscCard for FakeHandle {
    fn transaction<R>(&mut self, f: impl FnOnce(&dyn Transmit) -> R) -> Result<R, PcscError> {
        self.with_card(|_, _| Ok(()))?;
        Ok(f(self))
    }

    fn reconnect_reset(&mut self) -> Result<Vec<u8>, PcscError> {
        self.with_card(|w, idx| {
            w.resets += 1;
            let card = w.readers[idx].card.as_mut().unwrap();
            card.wedged = false;
            Ok(card.atr.clone())
        })
    }

    fn disconnect(self, disposition: Disposition) -> Result<(), PcscError> {
        let mut w = self.ctx.lock();
        w.disconnects.push(disposition);
        if disposition == Disposition::ResetCard {
            let insertion = self.insertion;
            if let Some(c) = w
                .reader(&self.reader)
                .filter(|r| r.events == insertion)
                .and_then(|r| r.card.as_mut())
            {
                c.wedged = false;
            }
        }
        Ok(())
    }
}
