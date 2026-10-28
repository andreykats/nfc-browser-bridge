//! macOS only: copies the system log's lines about USB, the smart-card service and sleep/wake into
//! `macos.YYYY-MM-DD.log` next to the bridge's own log (daily, 7 kept). macOS keeps its own log for
//! only a day or two and never stores the informational lines, so they are gone by the time anyone
//! looks into a reader that stopped working. Runs `log stream` and restarts it if it exits.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tracing_appender::rolling::{RollingFileAppender, Rotation};

/// The reader's USB connection (kernel), Apple's CCID driver and the driver host, smart-card
/// service errors, and sleep/wake. Without the XPC chatter, which is ~2 lines per second.
const PREDICATE: &str = r#"NOT subsystem == "com.apple.xpc" AND (
    (process == "kernel" AND senderImagePath CONTAINS "IOUSBHost")
    OR process == "usbsmartcardreaderd"
    OR process == "com.apple.ifdreader"
    OR (process == "com.apple.ctkpcscd" AND messageType >= 16)
    OR (subsystem == "com.apple.powerd" AND category == "sleepWake"))"#;

/// Runs `log stream` (the arguments) and kills it once this app's end of stdin closes, which
/// happens however the app exits. macOS has no "kill me when my parent dies" for child processes.
const WRAPPER: &str = r#"exec 3<&0
"$@" &
child=$!
{ read -r _ <&3; kill "$child" 2>/dev/null; } &
watcher=$!
wait "$child"
status=$?
kill "$watcher" 2>/dev/null
exit "$status""#;

const MIN_DELAY: Duration = Duration::from_secs(5);
const MAX_DELAY: Duration = Duration::from_secs(10 * 60);
/// A stream that ran this long was healthy; the next restart starts again from `MIN_DELAY`.
const STABLE: Duration = Duration::from_secs(60);

/// Start copying the system log into `dir` in the background.
pub fn spawn(dir: &Path) {
    let appender = match RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("macos")
        .filename_suffix("log")
        .max_log_files(7)
        .build(dir)
    {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(error = %e, "cannot open the macOS log file; not copying the system log");
            return;
        }
    };
    let spawned = std::thread::Builder::new()
        .name("os-log".into())
        .spawn(move || run(appender));
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "cannot start copying the system log");
    }
}

fn run(mut out: RollingFileAppender) {
    let mut delay = MIN_DELAY;
    let mut last_error = String::new();
    loop {
        let started = Instant::now();
        match stream_once(&mut out) {
            Ok(status) => {
                tracing::warn!(%status, "the system log stream ended; restarting it");
            }
            Err(e) => {
                // A permanent problem (e.g. the user may not read the log) is logged once.
                if e != last_error {
                    tracing::warn!(error = %e, "cannot copy the system log; retrying");
                }
                last_error = e;
            }
        }
        if started.elapsed() >= STABLE {
            delay = MIN_DELAY;
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(MAX_DELAY);
    }
}

/// Copy lines until the stream ends. `Err` describes a stream that could not run.
fn stream_once(out: &mut RollingFileAppender) -> Result<std::process::ExitStatus, String> {
    let mut child = Command::new("/bin/sh")
        .args(["-c", WRAPPER, "sh", "/usr/bin/log", "stream"])
        .args([
            "--style",
            "compact",
            "--level",
            "info",
            "--predicate",
            PREDICATE,
        ])
        // Never written: it closes when this process exits, which stops the stream.
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run log stream: {e}"))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    let mut lines = 0u64;
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {
                lines += 1;
                // The first line repeats the filter; the second is the column header.
                if lines > 2 && out.write_all(&line).is_err() {
                    tracing::debug!("cannot write the macOS log file");
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, "reading the system log stream failed");
                break;
            }
        }
    }
    let mut stderr = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut stderr);
    }
    let status = child
        .wait()
        .map_err(|e| format!("cannot wait for log stream: {e}"))?;
    if !status.success() && lines <= 2 {
        let reason = stderr.trim();
        return Err(if reason.is_empty() {
            format!("log stream exited with {status}")
        } else {
            reason.to_string()
        });
    }
    Ok(status)
}
