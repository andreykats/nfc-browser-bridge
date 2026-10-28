//! The monitor thread: watches readers and cards with blocking `SCardGetStatusChange`, survives the
//! smart-card service stopping, and feeds the hub.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::backend::{PNP_READER, PcscBackend, PcscContext, PcscError, ReaderWatch, state};
use crate::health::{Health, Part};
use crate::hub::Hub;
use crate::protocol::{ReaderKind, ServiceStatus, encode_hex};
use crate::worker::{Job, WorkerHandle};

const STATUS_TIMEOUT: Duration = Duration::from_secs(1);
/// Consecutive unexpected PC/SC failures before the context is treated as broken and re-established.
const MAX_FAILURES: u32 = 5;
const BACKOFF: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
];

/// Stop flag with an interruptible sleep.
#[derive(Default)]
pub struct Stopper {
    stopped: Mutex<bool>,
    cv: Condvar,
}

impl Stopper {
    pub fn stop(&self) {
        *self.stopped.lock().unwrap() = true;
        self.cv.notify_all();
    }

    pub fn is_stopped(&self) -> bool {
        *self.stopped.lock().unwrap()
    }

    /// Sleep up to `d`; returns true if stopped.
    pub fn sleep(&self, d: Duration) -> bool {
        let guard = self.stopped.lock().unwrap();
        *self.cv.wait_timeout_while(guard, d, |s| !*s).unwrap().0
    }
}

pub struct MonitorHandle<C> {
    stopper: Arc<Stopper>,
    ctx: Arc<Mutex<Option<C>>>,
    thread: Option<JoinHandle<()>>,
}

impl<C: PcscContext> MonitorHandle<C> {
    pub fn stop(mut self) {
        self.stopper.stop();
        if let Some(ctx) = self.ctx.lock().unwrap().as_ref() {
            let _ = ctx.cancel();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

pub fn spawn<B: PcscBackend>(
    backend: Arc<B>,
    hub: Arc<Hub>,
    worker: WorkerHandle,
    health: Arc<Health>,
    reopen: Arc<AtomicBool>,
) -> MonitorHandle<B::Context> {
    let stopper = Arc::new(Stopper::default());
    let ctx = Arc::new(Mutex::new(None));
    let m = Monitor {
        backend,
        hub,
        worker,
        health,
        reopen,
        stopper: stopper.clone(),
        ctx_slot: ctx.clone(),
    };
    let thread = std::thread::Builder::new()
        .name("pcsc-monitor".into())
        .spawn(move || m.run())
        .expect("spawn monitor thread");
    MonitorHandle {
        stopper,
        ctx,
        thread: Some(thread),
    }
}

/// How `Monitor::watch` ended.
enum WatchEnd {
    /// Stopped, or the context died. `worked`: it listed readers at least once.
    Ended { worked: bool },
    /// Reader recovery asked for a new context.
    Reopen,
}

struct Monitor<B: PcscBackend> {
    backend: Arc<B>,
    hub: Arc<Hub>,
    worker: WorkerHandle,
    health: Arc<Health>,
    /// Set by reader recovery: drop the context and establish a new one.
    reopen: Arc<AtomicBool>,
    stopper: Arc<Stopper>,
    /// The live context, so `stop` can cancel a blocking call.
    ctx_slot: Arc<Mutex<Option<B::Context>>>,
}

impl<B: PcscBackend> Monitor<B> {
    fn run(self) {
        let mut attempt = 0;
        // The last establish failure logged, so a long outage logs once, not every few seconds.
        let mut failing: Option<PcscError> = None;
        while !self.stopper.is_stopped() {
            match self.health.busy(Part::Monitor, || self.backend.establish()) {
                Ok(ctx) => {
                    if let Some(e) = failing.take() {
                        tracing::info!(last_error = %e, "connected to the smart card service again");
                    }
                    *self.ctx_slot.lock().unwrap() = Some(ctx.clone());
                    // Re-check: stop() may have run before the context was visible to it.
                    if self.stopper.is_stopped() {
                        break;
                    }
                    let end = self.watch(&ctx);
                    *self.ctx_slot.lock().unwrap() = None;
                    match end {
                        // The service is fine: reconnect at once, without a service outage.
                        WatchEnd::Reopen => {
                            attempt = 0;
                            continue;
                        }
                        WatchEnd::Ended { worked: true } => attempt = 0,
                        WatchEnd::Ended { worked: false } => {}
                    }
                }
                Err(e) => {
                    if failing != Some(e) {
                        tracing::warn!(pcsc = %e, "cannot connect to the smart card service; retrying");
                    }
                    failing = Some(e);
                }
            }
            self.hub.set_service(ServiceStatus::Unavailable);
            let delay = BACKOFF[attempt.min(BACKOFF.len() - 1)];
            attempt += 1;
            if self.stopper.sleep(delay) {
                break;
            }
        }
        tracing::debug!("monitor stopped");
    }

    /// Watch until the context dies, recovery asks for a new one, or we are stopped.
    fn watch(&self, ctx: &B::Context) -> WatchEnd {
        let mut watches = vec![ReaderWatch::new(PNP_READER)];
        let mut worked = false;
        let mut failures = 0;
        loop {
            if self.stopper.is_stopped() {
                return WatchEnd::Ended { worked };
            }
            if self.reopen.swap(false, Ordering::SeqCst) {
                tracing::info!("dropping the PC/SC context to reconnect");
                return WatchEnd::Reopen;
            }
            // The unexpected error of this round, if any.
            let mut fault = None;
            match self.health.busy(Part::Monitor, || ctx.list_readers()) {
                Ok(names) => {
                    if !worked {
                        worked = true;
                        self.hub.set_service(ServiceStatus::Ready);
                    }
                    watches.retain(|w| w.is_pnp() || names.contains(&w.name));
                    for n in &names {
                        if !watches.iter().any(|w| &w.name == n) {
                            watches.push(ReaderWatch::new(n.clone()));
                        }
                    }
                    for reader in self.hub.set_readers(&names) {
                        self.worker.submit_blocking(Job::Release { reader });
                    }
                }
                Err(e) if e.is_service_lost() => {
                    tracing::info!(pcsc = %e, "smart card service lost");
                    return WatchEnd::Ended { worked };
                }
                Err(e) => {
                    tracing::warn!(pcsc = %e, "list readers failed");
                    fault = Some(e);
                }
            }

            // Wait before the next round only after a failed wait, so errors cannot spin.
            let mut pause = false;
            if watches.is_empty() {
                if self.stopper.sleep(STATUS_TIMEOUT) {
                    return WatchEnd::Ended { worked };
                }
            } else {
                match self.health.busy(Part::Monitor, || {
                    ctx.get_status_change(STATUS_TIMEOUT, &mut watches)
                }) {
                    Ok(()) => self.process(&mut watches),
                    Err(PcscError::TIMEOUT | PcscError::CANCELLED) => {}
                    Err(e) if e.is_service_lost() => {
                        tracing::info!(pcsc = %e, "smart card service lost");
                        return WatchEnd::Ended { worked };
                    }
                    // A reader vanished between listing and waiting: the next listing fixes it, so
                    // re-list at once the first time.
                    Err(e @ (PcscError::UNKNOWN_READER | PcscError::READER_UNAVAILABLE)) => {
                        pause = failures > 0;
                        fault = Some(e);
                    }
                    Err(e) => {
                        tracing::warn!(pcsc = %e, "get status change failed");
                        pause = true;
                        fault = Some(e);
                    }
                }
            }

            let Some(e) = fault else {
                failures = 0;
                continue;
            };
            failures += 1;
            // Errors that persist mean the context itself is broken: start over with a new one.
            if failures >= MAX_FAILURES {
                tracing::warn!(pcsc = %e, "PC/SC keeps failing; re-establishing the context");
                return WatchEnd::Ended { worked };
            }
            if pause && self.stopper.sleep(STATUS_TIMEOUT) {
                return WatchEnd::Ended { worked };
            }
        }
    }

    fn process(&self, watches: &mut Vec<ReaderWatch>) {
        let mut drop_pnp = false;
        for w in watches.iter_mut() {
            if !w.has(state::CHANGED) {
                continue;
            }
            if !w.is_pnp() {
                log_abnormal_state(w);
            }
            w.sync();
            if w.is_pnp() {
                // macOS reports the PnP pseudo-reader as UNKNOWN: fall back to the 1 s re-list.
                if w.has(state::UNKNOWN) {
                    tracing::info!("PnP notifications unsupported; polling the reader list");
                    drop_pnp = true;
                }
                continue;
            }
            let present =
                w.has(state::PRESENT) && !w.has(state::MUTE | state::UNKNOWN | state::UNAVAILABLE);
            if !present {
                if self.hub.card_gone(&w.name) {
                    self.worker.submit_blocking(Job::Release {
                        reader: w.name.clone(),
                    });
                }
                continue;
            }
            if let Some((generation, kind)) = self.hub.card_seen(&w.name, encode_hex(&w.atr)) {
                if kind == ReaderKind::Sam {
                    // SAMs are contact cards: no UID to read.
                    self.hub.card_identified(&w.name, generation, None);
                } else {
                    self.worker.submit_blocking(Job::ReadUid {
                        reader: w.name.clone(),
                        generation,
                    });
                }
            }
        }
        if drop_pnp {
            watches.retain(|w| !w.is_pnp());
        }
    }
}

/// States that mean the reader or card is not working normally.
const ABNORMAL: [(u32, &str); 3] = [
    (state::MUTE, "mute"),
    (state::UNAVAILABLE, "unavailable"),
    (state::UNKNOWN, "unknown"),
];

/// Log when a reader enters or leaves an abnormal state (before `sync`, which forgets the old one).
fn log_abnormal_state(w: &ReaderWatch) {
    let describe = |bits: u32| {
        let names: Vec<_> = ABNORMAL
            .iter()
            .filter(|(b, _)| bits & b != 0)
            .map(|(_, n)| *n)
            .collect();
        if names.is_empty() {
            "normal".to_string()
        } else {
            names.join("+")
        }
    };
    let mask = ABNORMAL.iter().fold(0, |m, (b, _)| m | b);
    // A reader seen for the first time has no old state worth comparing with.
    let before = if w.current_state == state::UNAWARE {
        0
    } else {
        w.current_state & mask
    };
    let after = w.event_state & mask;
    if before != after {
        tracing::info!(reader = %w.name, state = %describe(after), was = %describe(before), "reader state");
    }
}
