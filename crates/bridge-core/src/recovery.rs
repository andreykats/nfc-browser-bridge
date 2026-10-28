//! Reader recovery: notices a reader that stopped working while still plugged in and brings it
//! back without anyone unplugging it. Also logs USB changes and a periodic status line.
//!
//! Two kinds of trouble:
//! - **Missing:** the reader is on USB but the smart-card service does not list it (the kiosk
//!   shows "plug in the reader" although it is plugged in).
//! - **Unresponsive:** a card held on the reader stopped answering, even after the worker reset it.
//!
//! The ladder: wait a moment (a replug or reset takes about a second to show up), reconnect to the
//! smart-card service (missing only), reset the reader over USB, reset it once more, then give up
//! and ask for a replug. Only platforms that can see USB (macOS) recover; elsewhere this thread
//! still checks held cards and logs status.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::health::{Health, Part};
use crate::hub::Hub;
use crate::monitor::Stopper;
use crate::protocol::{
    BridgeError, ErrorCode, ReaderKind, RecoveryAction, RecoveryState, ServiceStatus, UsbReaderInfo,
};
use crate::usb::{UsbControl, UsbReader};
use crate::worker::{Job, WorkerHandle};

/// After the bridge resets readers, changes to their USB attachments within this time are its own
/// doing, and a card that was on a reset reader is not announced again when it reappears.
const RESET_SETTLE: Duration = Duration::from_secs(15);
/// When the first status line is logged after start.
const STATUS_DELAY: Duration = Duration::from_secs(5);

pub struct RecoveryConfig {
    pub usb: Arc<dyn UsbControl>,
    /// How often USB and the reader list are compared.
    pub tick: Duration,
    /// How long a reader may be missing before anything is done (a replug takes about 1 s).
    pub missing_grace: Duration,
    /// How long to wait after reconnecting to the smart-card service before resetting the reader.
    pub reopen_wait: Duration,
    /// How long to wait after a USB reset before judging it.
    pub reset_wait: Duration,
    /// USB resets before giving up.
    pub max_resets: u32,
    /// How often a card held on a reader is checked.
    pub probe_interval: Duration,
    /// How often the status line is logged.
    pub heartbeat: Duration,
}

impl Default for RecoveryConfig {
    fn default() -> Self {
        Self {
            usb: crate::usb::system(),
            tick: Duration::from_secs(1),
            missing_grace: Duration::from_secs(5),
            reopen_wait: Duration::from_secs(5),
            reset_wait: Duration::from_secs(10),
            max_resets: 2,
            probe_interval: Duration::from_secs(60),
            heartbeat: Duration::from_secs(10 * 60),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trouble {
    Missing,
    /// The card on this reader stopped answering.
    Unresponsive(String),
}

impl fmt::Display for Trouble {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => {
                f.write_str("the reader is on USB but the smart card service does not list it")
            }
            Self::Unresponsive(r) => write!(f, "the card on {r} stopped answering"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Idle,
    /// Trouble seen; waiting out the grace period.
    Noticed {
        since: Instant,
    },
    Reopened {
        at: Instant,
    },
    Reset {
        at: Instant,
        attempts: u32,
    },
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Reopen,
    Reset { attempt: u32 },
    GiveUp,
    Recovered,
}

/// What to do next, given the trouble seen now. Pure, so it can be tested with made-up times.
#[derive(Debug)]
pub struct Ladder {
    stage: Stage,
}

impl Default for Ladder {
    fn default() -> Self {
        Self { stage: Stage::Idle }
    }
}

impl Ladder {
    pub fn state(&self) -> RecoveryState {
        match self.stage {
            Stage::Idle | Stage::Noticed { .. } => RecoveryState::Idle,
            Stage::Reopened { .. } | Stage::Reset { .. } => RecoveryState::Recovering,
            Stage::Failed => RecoveryState::Failed,
        }
    }

    pub fn step(
        &mut self,
        now: Instant,
        trouble: Option<&Trouble>,
        c: &RecoveryConfig,
    ) -> Option<Action> {
        let Some(trouble) = trouble else {
            let acted = !matches!(self.stage, Stage::Idle | Stage::Noticed { .. });
            self.stage = Stage::Idle;
            return acted.then_some(Action::Recovered);
        };
        let reset = |attempts| {
            (
                Stage::Reset { at: now, attempts },
                Some(Action::Reset { attempt: attempts }),
            )
        };
        let (stage, action) = match self.stage {
            Stage::Idle => match trouble {
                // The worker already reset an unresponsive card itself: go straight to USB.
                Trouble::Unresponsive(_) => reset(1),
                Trouble::Missing => (Stage::Noticed { since: now }, None),
            },
            Stage::Noticed { since } if now - since >= c.missing_grace => {
                (Stage::Reopened { at: now }, Some(Action::Reopen))
            }
            Stage::Reopened { at } if now - at >= c.reopen_wait => reset(1),
            Stage::Reset { at, attempts } if now - at >= c.reset_wait => {
                if attempts < c.max_resets {
                    reset(attempts + 1)
                } else {
                    (Stage::Failed, Some(Action::GiveUp))
                }
            }
            stage => (stage, None),
        };
        self.stage = stage;
        action
    }
}

struct Inner {
    ladder: Ladder,
    /// A reader whose card stopped answering (from the worker), until a check succeeds or the
    /// card leaves.
    unresponsive: Option<String>,
    /// USB changes before this time come from the bridge's own reset.
    resetting_until: Option<Instant>,
    /// USB readers at the last check (`None`: cannot see USB).
    usb: Option<Vec<UsbReader>>,
}

/// Shared by the recovery thread, the worker (card checks) and the server (manual resets).
pub struct Recovery {
    config: RecoveryConfig,
    hub: Arc<Hub>,
    health: Arc<Health>,
    /// Asks the monitor to drop its PC/SC context and establish a new one.
    reopen: Arc<AtomicBool>,
    /// One reset at a time, whoever asks.
    reset_lock: Mutex<()>,
    inner: Mutex<Inner>,
}

impl Recovery {
    pub fn new(config: RecoveryConfig, hub: Arc<Hub>, health: Arc<Health>) -> Self {
        Self {
            config,
            hub,
            health,
            reopen: Arc::new(AtomicBool::new(false)),
            reset_lock: Mutex::new(()),
            inner: Mutex::new(Inner {
                ladder: Ladder::default(),
                unresponsive: None,
                resetting_until: None,
                usb: None,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The flag the monitor checks to reconnect to the smart-card service.
    pub fn reopen_flag(&self) -> Arc<AtomicBool> {
        self.reopen.clone()
    }

    /// The card on `reader` answered (a check or a fresh UID read).
    pub fn card_ok(&self, reader: &str) {
        let mut inner = self.lock();
        if inner.unresponsive.as_deref() == Some(reader) {
            inner.unresponsive = None;
        }
    }

    /// The card on `reader` stopped answering, even after a card reset.
    pub fn card_unresponsive(&self, reader: &str) {
        self.lock().unresponsive = Some(reader.to_string());
    }

    /// Reset every smart-card reader on USB now (a staff request). Returns the readers reset.
    pub fn reset_all(&self, reason: &str) -> Result<Vec<String>, BridgeError> {
        let Some(readers) = self
            .health
            .busy(Part::Recovery, || self.config.usb.readers())
        else {
            return Err(BridgeError::new(
                ErrorCode::Unsupported,
                "The bridge cannot reset readers on this platform",
            ));
        };
        let done = self.reset(&readers, reason);
        // Start over: this was the ladder's next step anyway.
        self.lock().ladder = Ladder::default();
        Ok(done)
    }

    fn reset(&self, targets: &[UsbReader], reason: &str) -> Vec<String> {
        let _one_at_a_time = self.reset_lock.lock().unwrap_or_else(|e| e.into_inner());
        let until = Instant::now() + RESET_SETTLE;
        self.lock().resetting_until = Some(until);
        self.hub.expect_return(until);
        let mut done = Vec::new();
        for t in targets {
            tracing::warn!(reader = %t.name, %reason, "resetting the reader over USB");
            match self
                .health
                .busy(Part::Recovery, || self.config.usb.reset(t))
            {
                Ok(()) => done.push(t.name.clone()),
                Err(e) => tracing::warn!(reader = %t.name, error = %e, "USB reset failed"),
            }
        }
        self.hub
            .recovery_event(RecoveryAction::UsbReset, reason, done.clone());
        done
    }

    /// One round: compare USB with the reader list and take the ladder's next step.
    fn tick(&self, now: Instant, worker: &WorkerHandle) {
        let usb = self
            .health
            .busy(Part::Recovery, || self.config.usb.readers());
        let status = self.hub.status_snapshot();
        let mut inner = self.lock();
        if usb != inner.usb {
            let ours = inner.resetting_until.is_some_and(|t| now < t);
            log_usb_change(inner.usb.as_deref(), usb.as_deref(), ours);
            // A replug while recovery had given up: try again from the start.
            if !ours && inner.ladder.state() == RecoveryState::Failed {
                tracing::info!("a reader was plugged in again; recovery starts over");
                inner.ladder = Ladder::default();
            }
            inner.usb = usb.clone();
        }
        self.hub.set_usb(usb.as_ref().map(|l| {
            l.iter()
                .map(|r| UsbReaderInfo {
                    name: r.name.clone(),
                })
                .collect()
        }));
        let Some(usb) = usb else {
            return; // cannot see USB: nothing to recover
        };
        if let Some(r) = &inner.unresponsive
            && !self.hub.has_card(r)
        {
            inner.unresponsive = None; // the card left
        }
        let usable = status
            .readers
            .iter()
            .filter(|r| r.kind != ReaderKind::Sam)
            .count();
        // With the service down the monitor is reconnecting already, and a USB reset would not help.
        let missing = status.service == ServiceStatus::Ready && !usb.is_empty() && usable == 0;
        let trouble = if missing {
            Some(Trouble::Missing)
        } else {
            inner.unresponsive.clone().map(Trouble::Unresponsive)
        };
        let action = inner.ladder.step(now, trouble.as_ref(), &self.config);
        let state = inner.ladder.state();
        drop(inner);

        let names = || usb.iter().map(|r| r.name.clone()).collect::<Vec<_>>();
        match (action, &trouble) {
            (Some(Action::Reopen), Some(t)) => {
                tracing::warn!(reason = %t, "reconnecting to the smart card service");
                self.reopen.store(true, Ordering::SeqCst);
                worker.submit_blocking(Job::Reopen);
                self.hub
                    .recovery_event(RecoveryAction::Reopen, &t.to_string(), names());
            }
            (Some(Action::Reset { attempt }), Some(t)) => {
                let targets: Vec<UsbReader> = match t {
                    Trouble::Unresponsive(reader) => {
                        let matching: Vec<_> =
                            usb.iter().filter(|u| u.matches(reader)).cloned().collect();
                        if matching.is_empty() {
                            usb.clone()
                        } else {
                            matching
                        }
                    }
                    Trouble::Missing => usb.clone(),
                };
                let reason = format!("{t} (attempt {attempt} of {})", self.config.max_resets);
                self.reset(&targets, &reason);
            }
            (Some(Action::GiveUp), Some(t)) => {
                tracing::error!(
                    reason = %t,
                    resets = self.config.max_resets,
                    "the reader still does not work after USB resets; it needs unplugging and plugging back in"
                );
                self.hub
                    .recovery_event(RecoveryAction::Failed, &t.to_string(), names());
            }
            (Some(Action::Recovered), _) => {
                tracing::info!("the reader works again");
                self.hub.recovery_event(
                    RecoveryAction::Recovered,
                    "the reader works again",
                    names(),
                );
            }
            _ => {}
        }
        self.hub.set_recovery(state);
    }

    fn log_status(&self, started: Instant) {
        let status = self.hub.status_snapshot();
        let readers = status
            .readers
            .iter()
            .map(|r| match &r.card {
                Some(c) => format!("{} (card {})", r.name, c.uid.as_deref().unwrap_or("-")),
                None => r.name.clone(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        let usb = match &status.usb_readers {
            Some(l) => l
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            None => "unknown".into(),
        };
        tracing::info!(
            service = ?status.service,
            readers = %if readers.is_empty() { "none".into() } else { readers },
            usb = %if usb.is_empty() { "none".into() } else { usb },
            recovery = ?status.recovery,
            clients = self.hub.client_count(),
            uptime_min = started.elapsed().as_secs() / 60,
            "status"
        );
    }
}

fn log_usb_change(before: Option<&[UsbReader]>, after: Option<&[UsbReader]>, ours: bool) {
    let (before, after) = (before.unwrap_or_default(), after.unwrap_or_default());
    let why = if ours { " (bridge reset)" } else { "" };
    for r in before.iter().filter(|r| !after.contains(r)) {
        tracing::info!(reader = %r.name, id = %format_args!("{:#x}", r.id), "reader left USB{why}");
    }
    for r in after.iter().filter(|r| !before.contains(r)) {
        tracing::info!(reader = %r.name, id = %format_args!("{:#x}", r.id), "reader on USB{why}");
    }
}

pub struct RecoveryHandle {
    stopper: Arc<Stopper>,
    thread: Option<JoinHandle<()>>,
}

impl RecoveryHandle {
    pub fn stop(mut self) {
        self.stopper.stop();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Start the recovery thread. It holds a worker handle until stopped.
pub fn spawn(recovery: Arc<Recovery>, worker: WorkerHandle) -> RecoveryHandle {
    let stopper = Arc::new(Stopper::default());
    let stop = stopper.clone();
    let thread = std::thread::Builder::new()
        .name("reader-recovery".into())
        .spawn(move || {
            let started = Instant::now();
            let mut last_probe = started;
            // The first status line once the monitor has had time to list the readers.
            let mut next_status = started + STATUS_DELAY.min(recovery.config.heartbeat);
            loop {
                let now = Instant::now();
                recovery.tick(now, &worker);
                if now - last_probe >= recovery.config.probe_interval {
                    last_probe = now;
                    for reader in recovery.hub.readers_with_cards() {
                        // A full queue means the worker is busy with cards anyway.
                        let _ = worker.submit(Job::Probe { reader });
                    }
                }
                if now >= next_status {
                    next_status = now + recovery.config.heartbeat;
                    recovery.log_status(started);
                }
                if stop.sleep(recovery.config.tick) {
                    break;
                }
            }
        })
        .expect("spawn recovery thread");
    RecoveryHandle {
        stopper,
        thread: Some(thread),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usb::NoUsb;

    fn config() -> RecoveryConfig {
        RecoveryConfig {
            usb: Arc::new(NoUsb),
            ..RecoveryConfig::default()
        }
    }

    #[test]
    fn missing_reader_climbs_the_ladder_then_gives_up() {
        let c = config();
        let mut l = Ladder::default();
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let missing = Some(&Trouble::Missing);
        assert_eq!(l.step(at(0), missing, &c), None);
        assert_eq!(l.state(), RecoveryState::Idle);
        assert_eq!(l.step(at(4), missing, &c), None);
        assert_eq!(l.step(at(5), missing, &c), Some(Action::Reopen));
        assert_eq!(l.state(), RecoveryState::Recovering);
        assert_eq!(l.step(at(9), missing, &c), None);
        assert_eq!(
            l.step(at(10), missing, &c),
            Some(Action::Reset { attempt: 1 })
        );
        assert_eq!(l.step(at(19), missing, &c), None);
        assert_eq!(
            l.step(at(20), missing, &c),
            Some(Action::Reset { attempt: 2 })
        );
        assert_eq!(l.step(at(30), missing, &c), Some(Action::GiveUp));
        assert_eq!(l.state(), RecoveryState::Failed);
        assert_eq!(l.step(at(500), missing, &c), None);
        // Plugged back in by hand.
        assert_eq!(l.step(at(501), None, &c), Some(Action::Recovered));
        assert_eq!(l.state(), RecoveryState::Idle);
    }

    #[test]
    fn a_short_gap_is_not_trouble() {
        let c = config();
        let mut l = Ladder::default();
        let t0 = Instant::now();
        assert_eq!(l.step(t0, Some(&Trouble::Missing), &c), None);
        // Back within the grace period: nothing was done, so nothing recovered either.
        assert_eq!(l.step(t0 + Duration::from_secs(2), None, &c), None);
        assert_eq!(l.state(), RecoveryState::Idle);
    }

    #[test]
    fn unresponsive_card_goes_straight_to_a_usb_reset() {
        let c = config();
        let mut l = Ladder::default();
        let t0 = Instant::now();
        let stuck = Trouble::Unresponsive("R".into());
        assert_eq!(
            l.step(t0, Some(&stuck), &c),
            Some(Action::Reset { attempt: 1 })
        );
        // The reset fixed it.
        assert_eq!(
            l.step(t0 + Duration::from_secs(3), None, &c),
            Some(Action::Recovered)
        );
    }
}
