//! Shared bridge state. The monitor and worker report into it; it broadcasts protocol events to
//! WebSocket connections and status snapshots to the UI. All mutations happen under one lock and
//! events are sent while holding it, so a `snapshot_and_subscribe` never misses or duplicates one.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::broadcast;

use crate::protocol::{CardInfo, Event, Reader, ReaderKind, ServiceStatus};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerState {
    Starting,
    Listening { port: u16 },
    Stopped,
}

/// Everything the tray menu shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeStatus {
    pub server: ServerState,
    pub service: ServiceStatus,
    pub readers: Vec<Reader>,
    /// Distinct origins of connected clients ("topps.drey.dev", "localhost:5173", "local app").
    pub clients: Vec<String>,
}

pub type StatusListener = Arc<dyn Fn(&BridgeStatus) + Send + Sync>;

#[derive(Debug, Clone)]
enum Slot {
    Empty,
    /// Seen by the monitor; UID not read yet. Not visible to clients.
    Pending {
        generation: u64,
        atr: String,
    },
    Announced {
        uid: Option<String>,
        atr: String,
    },
}

struct ReaderEntry {
    name: String,
    kind: ReaderKind,
    slot: Slot,
}

impl ReaderEntry {
    fn public(&self) -> Reader {
        let card = match &self.slot {
            Slot::Announced { uid, atr } => Some(CardInfo {
                uid: uid.clone(),
                atr: atr.clone(),
            }),
            _ => None,
        };
        Reader {
            name: self.name.clone(),
            kind: self.kind,
            card,
        }
    }
}

struct State {
    server: ServerState,
    service: ServiceStatus,
    readers: Vec<ReaderEntry>,
    clients: BTreeMap<u64, String>,
    next_client: u64,
    next_generation: u64,
}

pub struct Hub {
    state: Mutex<State>,
    events: broadcast::Sender<Event>,
    listener: Option<StatusListener>,
}

/// Per-connection event buffer. A client that falls this far behind is disconnected and
/// resynchronises from `hello` on reconnect.
const EVENT_BUFFER: usize = 256;

impl Hub {
    pub fn new(listener: Option<StatusListener>) -> Self {
        Self {
            state: Mutex::new(State {
                server: ServerState::Starting,
                service: ServiceStatus::Unavailable,
                readers: Vec::new(),
                clients: BTreeMap::new(),
                next_client: 0,
                next_generation: 0,
            }),
            events: broadcast::channel(EVENT_BUFFER).0,
            listener,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn emit(&self, event: Event) {
        // No receivers is fine.
        let _ = self.events.send(event);
    }

    fn emit_readers(&self, s: &State) {
        self.emit(Event::Readers {
            service: s.service,
            readers: public_readers(s),
        });
    }

    fn notify(&self, s: &State) {
        if let Some(l) = &self.listener {
            let mut clients: Vec<String> = s.clients.values().cloned().collect();
            clients.sort();
            clients.dedup();
            l(&BridgeStatus {
                server: s.server.clone(),
                service: s.service,
                readers: public_readers(s),
                clients,
            });
        }
    }

    pub fn status_snapshot(&self) -> (ServiceStatus, Vec<Reader>) {
        let s = self.lock();
        (s.service, public_readers(&s))
    }

    /// Current state plus a receiver for every event after it.
    pub fn snapshot_and_subscribe(
        &self,
    ) -> (ServiceStatus, Vec<Reader>, broadcast::Receiver<Event>) {
        let s = self.lock();
        (s.service, public_readers(&s), self.events.subscribe())
    }

    pub fn has_reader(&self, name: &str) -> bool {
        self.lock().readers.iter().any(|r| r.name == name)
    }

    pub fn set_server(&self, server: ServerState) {
        let mut s = self.lock();
        s.server = server;
        self.notify(&s);
    }

    pub fn set_service(&self, service: ServiceStatus) {
        let mut s = self.lock();
        if s.service == service {
            return;
        }
        if service == ServiceStatus::Unavailable {
            for r in std::mem::take(&mut s.readers) {
                if matches!(r.slot, Slot::Announced { .. }) {
                    self.emit(Event::CardRemoved { reader: r.name });
                }
            }
        }
        s.service = service;
        tracing::info!(?service, "smart card service");
        self.emit_readers(&s);
        self.notify(&s);
    }

    /// Replace the reader list (from `SCardListReaders`), keeping state for readers that remain.
    /// Returns the names of readers that went away.
    pub fn set_readers(&self, names: &[String]) -> Vec<String> {
        let mut s = self.lock();
        let mut changed = false;
        let mut gone_names = Vec::new();
        let removed: Vec<ReaderEntry> = {
            let (keep, gone) = std::mem::take(&mut s.readers)
                .into_iter()
                .partition(|r| names.contains(&r.name));
            s.readers = keep;
            gone
        };
        for r in removed {
            tracing::info!(reader = %r.name, "reader removed");
            if matches!(r.slot, Slot::Announced { .. }) {
                self.emit(Event::CardRemoved {
                    reader: r.name.clone(),
                });
            }
            gone_names.push(r.name);
            changed = true;
        }
        for name in names {
            if !s.readers.iter().any(|r| &r.name == name) {
                let kind = ReaderKind::from_name(name);
                tracing::info!(reader = %name, ?kind, "reader added");
                s.readers.push(ReaderEntry {
                    name: name.clone(),
                    kind,
                    slot: Slot::Empty,
                });
                changed = true;
            }
        }
        if changed {
            self.emit_readers(&s);
            self.notify(&s);
        }
        gone_names
    }

    /// A card is in the field. Returns a generation to pass to `card_identified` if this is a new
    /// placement, or `None` if the card was already known.
    pub fn card_seen(&self, reader: &str, atr: String) -> Option<(u64, ReaderKind)> {
        let mut s = self.lock();
        s.next_generation += 1;
        let generation = s.next_generation;
        let r = s.readers.iter_mut().find(|r| r.name == reader)?;
        if !matches!(r.slot, Slot::Empty) {
            return None;
        }
        r.slot = Slot::Pending { generation, atr };
        Some((generation, r.kind))
    }

    /// The worker read the UID of the card placed as `generation`. Announces it if it is still there.
    pub fn card_identified(&self, reader: &str, generation: u64, uid: Option<String>) {
        let mut s = self.lock();
        let Some(r) = s.readers.iter_mut().find(|r| r.name == reader) else {
            return;
        };
        let Slot::Pending { generation: g, atr } = &r.slot else {
            return;
        };
        if *g != generation {
            return;
        }
        let atr = atr.clone();
        tracing::info!(reader, uid = uid.as_deref().unwrap_or("-"), %atr, "card present");
        r.slot = Slot::Announced {
            uid: uid.clone(),
            atr: atr.clone(),
        };
        self.emit(Event::CardPresent {
            reader: reader.into(),
            uid,
            atr,
        });
        self.emit_readers(&s);
        self.notify(&s);
    }

    /// The card left. Returns true if a card (pending or announced) was known on the reader.
    pub fn card_gone(&self, reader: &str) -> bool {
        let mut s = self.lock();
        let Some(r) = s.readers.iter_mut().find(|r| r.name == reader) else {
            return false;
        };
        match std::mem::replace(&mut r.slot, Slot::Empty) {
            Slot::Empty => false,
            Slot::Pending { .. } => true,
            Slot::Announced { .. } => {
                tracing::info!(reader, "card removed");
                self.emit(Event::CardRemoved {
                    reader: reader.into(),
                });
                self.emit_readers(&s);
                self.notify(&s);
                true
            }
        }
    }

    pub fn add_client(&self, origin: String) -> u64 {
        let mut s = self.lock();
        s.next_client += 1;
        let id = s.next_client;
        tracing::info!(%origin, "client connected");
        s.clients.insert(id, origin);
        self.notify(&s);
        id
    }

    pub fn remove_client(&self, id: u64) {
        let mut s = self.lock();
        if let Some(origin) = s.clients.remove(&id) {
            tracing::info!(%origin, "client disconnected");
        }
        self.notify(&s);
    }
}

fn public_readers(s: &State) -> Vec<Reader> {
    s.readers.iter().map(ReaderEntry::public).collect()
}
