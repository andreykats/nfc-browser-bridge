//! Keeps the bridge running unattended. The process started at login (or by the user) runs the app
//! as a child process and starts it again whenever it exits abnormally: a panic, the watchdog, or a
//! crash. Quit exits with success and ends both. On macOS this also covers the time before the
//! LaunchAgent takes over at the next login; Windows has no other restart mechanism.

use std::io;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

/// Exit status after a panic.
pub const EXIT_PANIC: i32 = 70;
/// Exit status when the watchdog finds a thread stuck in a PC/SC call.
pub const EXIT_STALLED: i32 = 71;
/// Exit status asking for a fresh start of both processes, e.g. after an update replaced the app.
pub const EXIT_RESTART: i32 = 72;

/// Set in the child's environment.
const CHILD_ENV: &str = "NFC_BROWSER_BRIDGE_CHILD";
const MIN_DELAY: Duration = Duration::from_secs(1);
const MAX_DELAY: Duration = Duration::from_secs(60);
/// A child that ran this long was healthy; the next restart starts again from `MIN_DELAY`.
const STABLE: Duration = Duration::from_secs(60);

/// True in the supervised child, which runs the app itself.
pub fn is_child() -> bool {
    std::env::var_os(CHILD_ENV).is_some()
}

/// Run the app as a child until it exits successfully, then exit the same way. Returns only if
/// the child cannot be started at all; the caller then runs the app unsupervised.
pub fn run() {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            tracing::error!(error = %e, "cannot find own executable; running unsupervised");
            return;
        }
    };
    let mut delay = MIN_DELAY;
    loop {
        let started = Instant::now();
        let status = match Command::new(&exe)
            .args(std::env::args_os().skip(1))
            .env(CHILD_ENV, "1")
            .status()
        {
            Ok(status) => status,
            Err(e) => {
                tracing::error!(error = %e, "cannot start the bridge process; running unsupervised");
                return;
            }
        };
        if status.success() {
            std::process::exit(0);
        }
        if status.code() == Some(EXIT_RESTART) {
            tracing::info!("restarting");
            let e = relaunch(&exe);
            tracing::error!(error = %e, "cannot restart; starting the bridge process again");
            delay = MIN_DELAY;
            continue;
        }
        if started.elapsed() >= STABLE {
            delay = MIN_DELAY;
        }
        tracing::warn!(%status, restart_in = ?delay, "bridge exited abnormally; restarting");
        std::thread::sleep(delay);
        delay = (delay * 2).min(MAX_DELAY);
    }
}

/// Start the app afresh from `exe`, as a supervisor rather than a child. On Unix this process
/// becomes the new one (same PID, so launchd keeps tracking it). Returns only on failure.
#[cfg(unix)]
pub fn relaunch(exe: &Path) -> io::Error {
    use std::os::unix::process::CommandExt;
    Command::new(exe)
        .args(std::env::args_os().skip(1))
        .env_remove(CHILD_ENV)
        .exec()
}

/// Start the app afresh from `exe`, as a supervisor rather than a child, and exit. Returns only on
/// failure.
#[cfg(windows)]
pub fn relaunch(exe: &Path) -> io::Error {
    match Command::new(exe)
        .args(std::env::args_os().skip(1))
        .env_remove(CHILD_ENV)
        .spawn()
    {
        Ok(_) => std::process::exit(0),
        Err(e) => e,
    }
}
