//! The worker thread owns all card I/O: one job at a time, so a reader never sees concurrent APDUs.
//!
//! While a card is on a reader the worker also holds one idle shared handle to it. Without an open
//! handle, macOS powers an idle card down after ~8 s; the next connect then fails with
//! NO_SMARTCARD and the reader reports a remove/re-insert "bounce" (see docs/HARDWARE.md).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;
use std::time::Duration;

use tokio::sync::oneshot;

use crate::backend::{Disposition, PcscBackend, PcscCard, PcscContext, PcscError};
use crate::health::{Health, Part};
use crate::hub::Hub;
use crate::protocol::{ApduResponse, BridgeError, ErrorCode, encode_hex};

pub type Reply<T> = oneshot::Sender<Result<T, BridgeError>>;

pub enum Job {
    /// Read the UID of a newly placed card and announce it through the hub.
    ReadUid { reader: String, generation: u64 },
    Transmit {
        reader: String,
        apdus: Vec<Vec<u8>>,
        stop_on_error: bool,
        reply: Reply<Vec<ApduResponse>>,
    },
    Reset {
        reader: String,
        reply: Reply<String>,
    },
    /// The card left (or the reader went away): drop the held handle.
    Release { reader: String },
}

const QUEUE: usize = 64;
const SHARING_RETRY_DELAYS_MS: [u64; 3] = [50, 100, 200];
/// A card swiped past the reader often fails its first command; the kiosk ignores a tap without a
/// UID, so the UID read is retried before giving up.
const UID_RETRY_DELAYS_MS: [u64; 2] = [50, 150];
const GET_UID: [u8; 5] = [0xFF, 0xCA, 0x00, 0x00, 0x00];

#[derive(Clone)]
pub struct WorkerHandle {
    tx: SyncSender<Job>,
}

impl WorkerHandle {
    /// Queue a client job; `busy` if the queue is full.
    pub fn submit(&self, job: Job) -> Result<(), BridgeError> {
        self.tx.try_send(job).map_err(|e| match e {
            TrySendError::Full(_) => BridgeError::new(ErrorCode::Busy, "Bridge queue is full"),
            TrySendError::Disconnected(_) => {
                BridgeError::new(ErrorCode::PcscUnavailable, "Bridge is shutting down")
            }
        })
    }

    /// Queue a monitor job, waiting for space.
    pub fn submit_blocking(&self, job: Job) {
        let _ = self.tx.send(job);
    }
}

/// Start the worker. It exits when every `WorkerHandle` has been dropped.
pub fn spawn<B: PcscBackend>(
    backend: Arc<B>,
    hub: Arc<Hub>,
    health: Arc<Health>,
) -> (WorkerHandle, JoinHandle<()>) {
    let (tx, rx) = sync_channel(QUEUE);
    let thread = std::thread::Builder::new()
        .name("pcsc-worker".into())
        .spawn(move || {
            Worker {
                backend,
                hub,
                health,
                ctx: None,
                held: HashMap::new(),
            }
            .run(rx)
        })
        .expect("spawn worker thread");
    (WorkerHandle { tx }, thread)
}

struct Worker<B: PcscBackend> {
    backend: Arc<B>,
    hub: Arc<Hub>,
    health: Arc<Health>,
    ctx: Option<B::Context>,
    /// Idle handles keeping present cards powered, by reader.
    held: HashMap<String, <B::Context as PcscContext>::Card>,
}

impl<B: PcscBackend> Worker<B> {
    fn run(mut self, rx: Receiver<Job>) {
        for job in rx {
            let health = self.health.clone();
            health.busy(Part::Worker, || self.handle(job));
        }
    }

    fn handle(&mut self, job: Job) {
        match job {
            Job::ReadUid { reader, generation } => {
                match self.identify(&reader) {
                    Ok(uid) => {
                        self.hold(&reader);
                        self.hub.card_identified(&reader, generation, uid);
                    }
                    // Gone already, or the service died: the monitor will clean up.
                    Err(e) if is_card_gone(e) || e.is_service_lost() => {}
                    Err(e) => {
                        tracing::warn!(%reader, pcsc = %e, "reading UID failed");
                        self.hold(&reader);
                        self.hub.card_identified(&reader, generation, None);
                    }
                }
            }
            Job::Release { reader } => {
                if let Some(card) = self.held.remove(&reader) {
                    let _ = card.disconnect(Disposition::LeaveCard);
                }
            }
            Job::Transmit {
                reader,
                apdus,
                stop_on_error,
                reply,
            } => {
                let _ = reply.send(self.transmit(&reader, &apdus, stop_on_error));
            }
            Job::Reset { reader, reply } => {
                let result = self.with_card(&reader, |card| {
                    card.reconnect_reset().map(|atr| encode_hex(&atr))
                });
                if let Ok(atr) = &result {
                    tracing::info!(%reader, %atr, "card reset");
                }
                let _ = reply.send(result.map_err(BridgeError::from_pcsc));
            }
        }
    }

    /// Read the UID, retrying failures other than the card or the service going away.
    fn identify(&mut self, reader: &str) -> Result<Option<String>, PcscError> {
        let mut result = self.with_card(reader, read_uid);
        for delay in UID_RETRY_DELAYS_MS {
            match result {
                Err(e) if !is_card_gone(e) && !e.is_service_lost() => {
                    tracing::debug!(%reader, pcsc = %e, "reading UID failed; retrying");
                    std::thread::sleep(Duration::from_millis(delay));
                    result = self.with_card(reader, read_uid);
                }
                _ => break,
            }
        }
        result
    }

    /// Keep an idle shared handle to the card on `reader` until `Job::Release`.
    fn hold(&mut self, reader: &str) {
        if self.held.contains_key(reader) {
            return;
        }
        let Some(ctx) = self.ctx.clone() else { return };
        match connect(&ctx, reader) {
            Ok(card) => {
                self.held.insert(reader.to_string(), card);
            }
            Err(e) => tracing::debug!(%reader, pcsc = %e, "cannot hold card handle"),
        }
    }

    /// Forget a dead context and every handle opened on it.
    fn drop_context(&mut self) {
        self.ctx = None;
        self.held.clear();
    }

    /// The cached context, or a fresh one. The flag says whether it was cached.
    fn context(&mut self) -> Result<(B::Context, bool), PcscError> {
        if let Some(ctx) = &self.ctx {
            return Ok((ctx.clone(), true));
        }
        let ctx = self.backend.establish()?;
        self.ctx = Some(ctx.clone());
        Ok((ctx, false))
    }

    /// Connect (retrying sharing violations), run `f`, disconnect. A PC/SC error from `f` resets the card.
    /// If a cached context turns out to be dead (the service restarted), retry once on a fresh one.
    /// Any other error that is not about the card drops the context, so the next job starts afresh.
    fn with_card<T>(
        &mut self,
        reader: &str,
        mut f: impl FnMut(&mut <B::Context as PcscContext>::Card) -> Result<T, PcscError>,
    ) -> Result<T, PcscError> {
        loop {
            let (ctx, cached) = self.context()?;
            let mut card = match connect(&ctx, reader) {
                Ok(card) => card,
                Err(e) if e.is_service_lost() => {
                    self.drop_context();
                    if cached {
                        continue;
                    }
                    return Err(e);
                }
                Err(e) => {
                    if !e.concerns_card() {
                        tracing::warn!(%reader, pcsc = %e, "connect failed; dropping the context");
                        self.drop_context();
                    }
                    return Err(e);
                }
            };
            let result = f(&mut card);
            let disposition = if result.is_ok() {
                Disposition::LeaveCard
            } else {
                Disposition::ResetCard
            };
            if let Err(e) = card.disconnect(disposition) {
                tracing::debug!(%reader, pcsc = %e, "disconnect failed");
            }
            if let Err(e) = result
                && !e.concerns_card()
            {
                if !e.is_service_lost() {
                    tracing::warn!(%reader, pcsc = %e, "card command failed; dropping the context");
                }
                self.drop_context();
            }
            return result;
        }
    }

    fn transmit(
        &mut self,
        reader: &str,
        apdus: &[Vec<u8>],
        stop_on_error: bool,
    ) -> Result<Vec<ApduResponse>, BridgeError> {
        let mut responses = Vec::with_capacity(apdus.len());
        let outcome = self.with_card(reader, |card| {
            responses.clear();
            card.transaction(|tx| {
                for apdu in apdus {
                    tracing::debug!(%reader, apdu = %encode_hex(apdu), "->");
                    let raw = tx.transmit(apdu)?;
                    tracing::debug!(%reader, resp = %encode_hex(&raw), "<-");
                    // A response without a status word is a reader fault.
                    let resp = ApduResponse::from_raw(&raw).ok_or(PcscError::COMM_ERROR)?;
                    let stop = stop_on_error && !resp.is_success();
                    responses.push(resp);
                    if stop {
                        break;
                    }
                }
                Ok(())
            })?
        });
        match outcome {
            Ok(()) => Ok(responses),
            Err(e) => {
                tracing::info!(%reader, pcsc = %e, completed = responses.len(), "transmit failed");
                let mut err = BridgeError::from_pcsc(e);
                err.completed = Some(responses.len());
                Err(err)
            }
        }
    }
}

fn connect<C: PcscContext>(ctx: &C, reader: &str) -> Result<C::Card, PcscError> {
    for delay in SHARING_RETRY_DELAYS_MS {
        match ctx.connect(reader) {
            Err(PcscError::SHARING_VIOLATION) => std::thread::sleep(Duration::from_millis(delay)),
            other => return other,
        }
    }
    ctx.connect(reader)
}

fn read_uid<C: PcscCard>(card: &mut C) -> Result<Option<String>, PcscError> {
    let raw = card.transaction(|tx| tx.transmit(&GET_UID))??;
    Ok(ApduResponse::from_raw(&raw)
        .filter(|r| r.is_success() && !r.data.is_empty())
        .map(|r| r.data))
}

fn is_card_gone(e: PcscError) -> bool {
    matches!(
        e,
        PcscError::NO_SMARTCARD
            | PcscError::REMOVED_CARD
            | PcscError::UNKNOWN_READER
            | PcscError::READER_UNAVAILABLE
    )
}
