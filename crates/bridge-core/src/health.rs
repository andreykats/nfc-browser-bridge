//! Liveness of the monitor and worker threads, for a watchdog in the app. A PC/SC call that never
//! returns cannot be interrupted from another thread, so the only recovery is restarting the
//! process; the bridge only reports what is stuck.

use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    /// The thread that watches readers and cards.
    Monitor,
    /// The thread that runs card I/O jobs.
    Worker,
}

impl fmt::Display for Part {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Monitor => "monitor",
            Self::Worker => "worker",
        })
    }
}

/// A part that has been busy in one call for too long.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stall {
    pub part: Part,
    pub busy_for: Duration,
}

impl fmt::Display for Stall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} stuck for {} s", self.part, self.busy_for.as_secs())
    }
}

/// When each part entered the call it is in, if any. Waiting for work does not count.
#[derive(Default)]
pub struct Health {
    monitor: Mutex<Option<Instant>>,
    worker: Mutex<Option<Instant>>,
}

impl Health {
    fn slot(&self, part: Part) -> &Mutex<Option<Instant>> {
        match part {
            Part::Monitor => &self.monitor,
            Part::Worker => &self.worker,
        }
    }

    fn set(&self, part: Part, since: Option<Instant>) {
        *self.slot(part).lock().unwrap_or_else(|e| e.into_inner()) = since;
    }

    /// Run `f`, counting the time it takes as busy.
    pub(crate) fn busy<T>(&self, part: Part, f: impl FnOnce() -> T) -> T {
        self.set(part, Some(Instant::now()));
        let out = f();
        self.set(part, None);
        out
    }

    /// The part that has been busy for longer than `limit`, if any.
    pub fn stalled(&self, limit: Duration) -> Option<Stall> {
        self.stalled_at(Instant::now(), limit)
    }

    fn stalled_at(&self, now: Instant, limit: Duration) -> Option<Stall> {
        [Part::Monitor, Part::Worker].into_iter().find_map(|part| {
            let since = (*self.slot(part).lock().unwrap_or_else(|e| e.into_inner()))?;
            let busy_for = now.saturating_duration_since(since);
            (busy_for > limit).then_some(Stall { part, busy_for })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_only_long_calls() {
        let h = Health::default();
        let limit = Duration::from_secs(30);
        let now = Instant::now();
        assert_eq!(h.stalled_at(now, limit), None);

        h.set(Part::Worker, Some(now));
        assert_eq!(h.stalled_at(now + Duration::from_secs(10), limit), None);
        assert_eq!(
            h.stalled_at(now + Duration::from_secs(31), limit),
            Some(Stall {
                part: Part::Worker,
                busy_for: Duration::from_secs(31)
            })
        );

        // A finished call is not busy, however long ago it started.
        h.busy(Part::Worker, || ());
        assert_eq!(h.stalled_at(now + Duration::from_secs(60), limit), None);
    }
}
