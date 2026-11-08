//! Staging probe for the on-demand long-running test (TESTING.md). Runs next to the bridge on the
//! staging Mac mini while a run is on: stays connected the way the kiosk does, reads the tag left
//! on the reader every minute, and records each result and every bridge event in `run.log`.
//!
//! `staging-probe finish` ends the run: it summarises `run.log`, uploads the summary with the run's
//! log and the bridge's logs to the logs bucket (next to the bridge's own "Upload logs" folders),
//! and keeps the log as `run.<start>.log`. `scripts/staging.sh` starts and stops runs.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{ErrorKind, Write as _};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bridge_app::upload;
use serde_json::{Value, json};
use time::{Date, OffsetDateTime};
use tungstenite::client::IntoClientRequest;
use tungstenite::http::HeaderValue;
use tungstenite::{Message, WebSocket};

const URL: &str = "ws://127.0.0.1:38241/v1";
const ORIGIN: &str = "http://staging-probe.localhost";
/// How often the tag is read, and how long to wait before reconnecting to the bridge.
const INTERVAL: Duration = Duration::from_secs(60);
/// Longest wait for the bridge's answer to one read.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// How long to keep trying to reach the bridge before a read counts as failed: an update restarts
/// it.
const RECONNECT: Duration = Duration::from_secs(30);
/// How long one socket read blocks, so reads, uploads and pings are handled in between.
const POLL: Duration = Duration::from_secs(1);
/// The tag's UID, then pages 4 to 7 (the start of the NDEF area).
const APDUS: [&str; 2] = ["FFCA000000", "FFB0000410"];
/// The current run's log, in the probe folder.
const RUN_LOG: &str = "run.log";

fn main() {
    let Some(dir) = probe_dir() else {
        eprintln!("cannot find the home folder");
        std::process::exit(1);
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("cannot create {}: {e}", dir.display());
        std::process::exit(1);
    }
    match std::env::args().nth(1).as_deref() {
        None => run(dir),
        Some("finish") => match finish(&dir) {
            Ok((folder, summary)) => println!("{summary}\nuploaded to {folder}"),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        },
        Some(_) => {
            eprintln!("usage: staging-probe [finish]");
            std::process::exit(2);
        }
    }
}

/// Probe until stopped, appending to the run's log.
fn run(dir: PathBuf) -> ! {
    let mut probe = Probe::new(dir);
    probe.log("event", "started");
    loop {
        match connect_patiently() {
            Ok(ws) => probe.watch(ws),
            Err(e) => {
                probe.log("fail", &format!("bridge_unreachable {e}"));
                std::thread::sleep(INTERVAL);
            }
        }
    }
}

/// ~/Library/Logs/NFCBrowserBridgeStaging on macOS, %LOCALAPPDATA%\NFCBrowserBridgeStaging
/// elsewhere.
fn probe_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    return dirs::home_dir().map(|h| h.join("Library/Logs/NFCBrowserBridgeStaging"));
    #[cfg(not(target_os = "macos"))]
    return dirs::data_local_dir().map(|d| d.join("NFCBrowserBridgeStaging"));
}

type Ws = WebSocket<TcpStream>;

/// Keep trying for [`RECONNECT`].
fn connect_patiently() -> Result<Ws, String> {
    let deadline = Instant::now() + RECONNECT;
    loop {
        match connect() {
            Ok(ws) => return Ok(ws),
            Err(e) if Instant::now() >= deadline => return Err(e),
            Err(_) => std::thread::sleep(Duration::from_secs(2)),
        }
    }
}

fn connect() -> Result<Ws, String> {
    let stream = TcpStream::connect("127.0.0.1:38241").map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(POLL))
        .map_err(|e| e.to_string())?;
    let mut req = URL.into_client_request().map_err(|e| e.to_string())?;
    req.headers_mut()
        .insert("origin", HeaderValue::from_static(ORIGIN));
    tungstenite::client(req, stream)
        .map(|(ws, _)| ws)
        .map_err(|e| e.to_string())
}

/// The next text message, `None` when nothing arrived within [`POLL`].
fn next(ws: &mut Ws) -> Result<Option<Value>, String> {
    match ws.read() {
        Ok(Message::Text(t)) => serde_json::from_str(&t)
            .map(Some)
            .map_err(|e| format!("bad message from the bridge: {e}")),
        Ok(Message::Close(_)) => Err("the bridge closed the connection".into()),
        Ok(_) => Ok(None),
        Err(tungstenite::Error::Io(e))
            if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
        {
            // Sends any pong queued by the read.
            ws.flush().map(|_| None).map_err(|e| e.to_string())
        }
        Err(e) => Err(e.to_string()),
    }
}

struct Probe {
    dir: PathBuf,
    /// The bridge's version from the last `hello`.
    version: Option<String>,
    /// The first contactless reader, and the UID of the card on it.
    reader: Option<(String, Option<String>)>,
    next_id: u64,
}

impl Probe {
    fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            version: None,
            reader: None,
            next_id: 1,
        }
    }

    /// Append `<UTC time> <kind> <text>` to the run's log.
    fn log(&self, kind: &str, text: &str) {
        let now = OffsetDateTime::now_utc();
        let line = format!("{} {kind} {}\n", timestamp(now), text.replace('\n', " "));
        let path = self.dir.join(RUN_LOG);
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| f.write_all(line.as_bytes()));
        if let Err(e) = written {
            eprintln!("cannot write {}: {e}", path.display());
        }
    }

    /// Stay connected, read the tag every [`INTERVAL`], and log the bridge's events.
    fn watch(&mut self, mut ws: Ws) {
        // `hello` comes first and lists the readers; the first read waits for it.
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            match next(&mut ws) {
                Ok(Some(v)) if v["event"] == "hello" => {
                    self.event(&v);
                    break;
                }
                Ok(_) if Instant::now() < deadline => {}
                Ok(_) => {
                    self.log("fail", "disconnected no hello from the bridge");
                    return;
                }
                Err(e) => {
                    self.log("fail", &format!("disconnected {e}"));
                    return;
                }
            }
        }
        let mut due = Instant::now();
        loop {
            if Instant::now() >= due {
                due += INTERVAL;
                if let Err(e) = self.read_tag(&mut ws) {
                    self.log("fail", &format!("disconnected {e}"));
                    return;
                }
            }
            match next(&mut ws) {
                Ok(Some(v)) => self.event(&v),
                Ok(None) => {}
                Err(e) => {
                    self.log("fail", &format!("disconnected {e}"));
                    return;
                }
            }
        }
    }

    /// Track the reader list and log what happened.
    fn event(&mut self, v: &Value) {
        let Some(event) = v["event"].as_str() else {
            return;
        };
        match event {
            "hello" => {
                self.version = v["version"].as_str().map(str::to_string);
                self.track(v);
                let version = self.version.as_deref().unwrap_or("unknown");
                self.log("event", &format!("connected v{version}"));
            }
            "readers" => self.track(v),
            "card_present" => {
                self.log(
                    "event",
                    &format!(
                        "card_present {} {}",
                        v["reader"].as_str().unwrap_or("?"),
                        v["uid"].as_str().unwrap_or("-")
                    ),
                );
            }
            "card_removed" => {
                self.log(
                    "event",
                    &format!("card_removed {}", v["reader"].as_str().unwrap_or("?")),
                );
            }
            "recovery" => {
                self.log(
                    "event",
                    &format!(
                        "recovery_{} {}",
                        v["action"].as_str().unwrap_or("?"),
                        v["reason"].as_str().unwrap_or("")
                    ),
                );
            }
            _ => {}
        }
    }

    fn track(&mut self, v: &Value) {
        self.reader = v["readers"].as_array().and_then(|rs| {
            rs.iter().find(|r| r["kind"] == "picc").map(|r| {
                (
                    r["name"].as_str().unwrap_or_default().to_string(),
                    r["card"]["uid"].as_str().map(str::to_string),
                )
            })
        });
    }

    /// One probe: read the tag through the bridge and log `ok` or `fail`. Errs only when the
    /// connection is gone.
    fn read_tag(&mut self, ws: &mut Ws) -> Result<(), String> {
        let Some((reader, uid)) = self.reader.clone() else {
            self.log("fail", "no_reader the bridge lists no contactless reader");
            return Ok(());
        };
        if uid.is_none() {
            self.log("fail", &format!("no_card no tag on {reader}"));
            return Ok(());
        }
        let id = self.next_id;
        self.next_id += 1;
        let req = json!({"id": id, "type": "transmit", "reader": reader, "apdus": APDUS});
        let started = Instant::now();
        ws.send(Message::text(req.to_string()))
            .map_err(|e| e.to_string())?;
        let deadline = started + REQUEST_TIMEOUT;
        while Instant::now() < deadline {
            let Some(v) = next(ws)? else { continue };
            if v["id"] != id {
                self.event(&v);
                continue;
            }
            let (kind, text) = outcome(&v, started.elapsed().as_millis());
            self.log(kind, &text);
            return Ok(());
        }
        self.log(
            "fail",
            &format!("timeout no answer within {}s", REQUEST_TIMEOUT.as_secs()),
        );
        Ok(())
    }
}

/// End the run: summarise `run.log`, upload the summary with the run's log and the bridge's logs
/// for the days it covered, and keep the log as `run.<start>.log`. Returns the upload folder and
/// the summary.
fn finish(dir: &Path) -> Result<(String, String), String> {
    let run = dir.join(RUN_LOG);
    let log = std::fs::read_to_string(&run)
        .map_err(|e| format!("no run to finish ({}): {e}", run.display()))?;
    let (start, end) = span(&log).ok_or("the run's log has no entries")?;
    let host = upload::host_name();
    let summary = summarize(&host, &log);
    let version = last_version(&log).unwrap_or_else(|| "unknown".into());
    let folder = upload::folder(&host, OffsetDateTime::now_utc(), &version);
    let first_day = day(start.date());
    let mut files = vec![
        (
            format!("summary.{first_day}.txt"),
            summary.clone().into_bytes(),
        ),
        (format!("probe.{first_day}.log"), log.into_bytes()),
    ];
    if let Some(logs) = bridge_app::log_dir() {
        let mut date = start.date();
        while date <= end.date() {
            for prefix in ["bridge", "macos"] {
                let file = format!("{prefix}.{}.log", day(date));
                if let Ok(body) = std::fs::read(logs.join(&file)) {
                    files.push((file, body));
                }
            }
            let Some(next) = date.next_day() else { break };
            date = next;
        }
    }
    let agent = upload::agent();
    for (file, body) in files {
        upload::send(&agent, &folder, &file, body).map_err(|e| format!("upload {file}: {e}"))?;
    }
    let kept = dir.join(format!("run.{}.log", compact(start)));
    std::fs::rename(&run, &kept).map_err(|e| format!("cannot rename {}: {e}", run.display()))?;
    Ok((folder, summary))
}

/// The log entry for the bridge's answer to a read: `ok`, `<ms>ms <uid>` or `fail`,
/// `<code> <message>`.
fn outcome(v: &Value, ms: u128) -> (&'static str, String) {
    if v["ok"] == true {
        let responses = v["result"]["responses"].as_array();
        let all_ok = responses
            .is_some_and(|rs| rs.len() == APDUS.len() && rs.iter().all(|r| r["sw"] == "9000"));
        if all_ok {
            let uid = responses
                .and_then(|rs| rs[0]["data"].as_str())
                .unwrap_or("-");
            return ("ok", format!("{ms}ms {uid}"));
        }
        let sws: Vec<&str> = responses
            .map(|rs| rs.iter().filter_map(|r| r["sw"].as_str()).collect())
            .unwrap_or_default();
        return ("fail", format!("status_word {}", sws.join(",")));
    }
    let text = format!(
        "{} {}",
        v["error"]["code"].as_str().unwrap_or("error"),
        v["error"]["message"].as_str().unwrap_or("")
    );
    ("fail", text)
}

/// The bridge version of the day's last `connected` entry.
fn last_version(log: &str) -> Option<String> {
    log.lines().rev().find_map(|l| {
        l.split_once(" event connected v")
            .map(|(_, v)| v.trim().to_string())
    })
}

/// The first and last times in a run's log.
fn span(log: &str) -> Option<(OffsetDateTime, OffsetDateTime)> {
    let mut times = log
        .lines()
        .filter_map(|l| parse_timestamp(l.split(' ').next()?));
    let first = times.next()?;
    Some((first, times.next_back().unwrap_or(first)))
}

fn day(d: Date) -> String {
    format!("{:04}-{:02}-{:02}", d.year(), u8::from(d.month()), d.day())
}

fn parse_day(s: &str) -> Option<Date> {
    let mut parts = s.split('-');
    let (y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || y.len() != 4 || m.len() != 2 || d.len() != 2 {
        return None;
    }
    let month = time::Month::try_from(m.parse::<u8>().ok()?).ok()?;
    Date::from_calendar_date(y.parse().ok()?, month, d.parse().ok()?).ok()
}

/// `YYYY-MM-DDTHH:MM:SSZ`, as [`timestamp`] writes it.
fn parse_timestamp(s: &str) -> Option<OffsetDateTime> {
    let (date, rest) = s.split_once('T')?;
    let hms = rest.strip_suffix('Z')?;
    let mut parts = hms.split(':').map(|p| p.parse::<u8>().ok());
    let (h, m, sec) = (parts.next()??, parts.next()??, parts.next()??);
    if parts.next().is_some() {
        return None;
    }
    let t = time::Time::from_hms(h, m, sec).ok()?;
    Some(parse_day(date)?.with_time(t).assume_utc())
}

/// `YYYYMMDDTHHMMSSZ`, for file names.
fn compact(t: OffsetDateTime) -> String {
    timestamp(t).replace(['-', ':'], "")
}

fn timestamp(t: OffsetDateTime) -> String {
    format!(
        "{}T{:02}:{:02}:{:02}Z",
        day(t.date()),
        t.hour(),
        t.minute(),
        t.second()
    )
}

/// The run's verdict and numbers, from its log.
///
/// - **FAIL:** a read failed, or recovery gave up on a reader.
/// - **WARN:** every read worked, but the bridge had to recover a reader, the connection dropped,
///   or the tag came and went.
/// - **PASS:** none of that.
fn summarize(host: &str, log: &str) -> String {
    let (mut ok, mut failed) = (0u32, 0u32);
    let mut latencies: Vec<u32> = Vec::new();
    let mut reasons: BTreeMap<String, u32> = BTreeMap::new();
    let mut events: BTreeMap<String, u32> = BTreeMap::new();
    let mut versions: Vec<String> = Vec::new();
    let (mut streak, mut longest) = (0u32, 0u32);
    let mut longest_end = "";
    let mut failure_times: Vec<&str> = Vec::new();
    for line in log.lines() {
        let mut parts = line.splitn(3, ' ');
        let (Some(time), Some(kind), rest) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let rest = rest.unwrap_or("");
        let first = rest.split(' ').next().unwrap_or("");
        match kind {
            "ok" => {
                ok += 1;
                streak = 0;
                if let Some(ms) = first.strip_suffix("ms").and_then(|m| m.parse().ok()) {
                    latencies.push(ms);
                }
            }
            "fail" if first == "disconnected" => {
                *events.entry("disconnected".into()).or_default() += 1
            }
            "fail" => {
                failed += 1;
                *reasons.entry(first.to_string()).or_default() += 1;
                failure_times.push(time);
                streak += 1;
                if streak > longest {
                    longest = streak;
                    longest_end = time;
                }
            }
            "event" => {
                if first == "connected"
                    && let Some(v) = rest.split(' ').nth(1).and_then(|v| v.strip_prefix('v'))
                    && !versions.iter().any(|x| x == v)
                {
                    versions.push(v.to_string());
                }
                if first.starts_with("recovery_") || first.starts_with("card_") {
                    *events.entry(first.to_string()).or_default() += 1;
                }
            }
            _ => {}
        }
    }
    let gave_up = events.contains_key("recovery_failed");
    let unsettled = events
        .keys()
        .any(|k| k.starts_with("recovery_") || k.starts_with("card_"))
        || events.contains_key("disconnected");
    let verdict = if failed > 0 || gave_up {
        "FAIL"
    } else if unsettled {
        "WARN"
    } else if ok == 0 {
        "FAIL"
    } else {
        "PASS"
    };

    let mut s = String::new();
    let (start, end) =
        span(log).unwrap_or((OffsetDateTime::UNIX_EPOCH, OffsetDateTime::UNIX_EPOCH));
    let secs = (end - start).whole_seconds().max(0);
    let _ = writeln!(
        s,
        "NFC Browser Bridge staging run, {} to {} ({}h {:02}m)",
        timestamp(start),
        timestamp(end),
        secs / 3600,
        secs % 3600 / 60
    );
    let _ = writeln!(s, "result: {verdict}");
    let _ = writeln!(s, "host: {host}");
    let versions = if versions.is_empty() {
        "unknown".to_string()
    } else {
        versions.join(", ")
    };
    let _ = writeln!(s, "bridge versions: {versions}");
    let total = ok + failed;
    // One read at the start, then one a minute.
    let expected = secs / 60 + 1;
    let _ = writeln!(
        s,
        "reads: {total} (about {expected} expected), ok {ok}, failed {failed}"
    );
    if failed > 0 {
        let list: Vec<String> = reasons.iter().map(|(r, n)| format!("{r} {n}")).collect();
        let _ = writeln!(s, "failures by reason: {}", list.join(", "));
        let _ = writeln!(
            s,
            "longest run of failures: {longest}, ending {longest_end}"
        );
        let first = failure_times.first().copied().unwrap_or("-");
        let last = failure_times.last().copied().unwrap_or("-");
        let _ = writeln!(s, "first failure: {first}, last failure: {last}");
    }
    if !latencies.is_empty() {
        latencies.sort_unstable();
        let pick = |p: usize| latencies[(latencies.len() - 1) * p / 100];
        let _ = writeln!(
            s,
            "read time ms: median {}, p95 {}, max {}",
            pick(50),
            pick(95),
            latencies[latencies.len() - 1]
        );
    }
    let list: Vec<String> = events.iter().map(|(e, n)| format!("{e} {n}")).collect();
    let _ = writeln!(
        s,
        "events: {}",
        if list.is_empty() {
            "none".to_string()
        } else {
            list.join(", ")
        }
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_round_trip() {
        let d = parse_day("2026-11-08").unwrap();
        assert_eq!(day(d), "2026-11-08");
        assert_eq!(parse_day("2026-11-8"), None);
        assert_eq!(parse_day("2026-13-08"), None);
        assert_eq!(parse_day("2026-11-08-1"), None);
    }

    #[test]
    fn outcomes() {
        let ok = json!({"id": 1, "ok": true, "result": {"responses": [
            {"data": "04699E22C01C90", "sw": "9000"}, {"data": "00", "sw": "9000"}]}});
        assert_eq!(outcome(&ok, 12), ("ok", "12ms 04699E22C01C90".into()));
        let sw = json!({"id": 1, "ok": true, "result": {"responses": [
            {"data": "", "sw": "6300"}]}});
        assert_eq!(outcome(&sw, 5), ("fail", "status_word 6300".into()));
        let err = json!({"id": 1, "ok": false, "error": {"code": "no_card", "message": "No card on reader"}});
        assert_eq!(
            outcome(&err, 5),
            ("fail", "no_card No card on reader".into())
        );
    }

    #[test]
    fn a_clean_run_passes() {
        let log = "\
2026-11-08T00:00:00Z event connected v1.0.11
2026-11-08T00:00:01Z ok 12ms 04699E22C01C90
2026-11-08T00:01:01Z ok 30ms 04699E22C01C90
";
        let s = summarize("Mac-mini-F6E28B59", log);
        assert!(s.contains("result: PASS"), "{s}");
        assert!(s.contains("bridge versions: 1.0.11"), "{s}");
        assert!(
            s.contains("staging run, 2026-11-08T00:00:00Z to 2026-11-08T00:01:01Z (0h 01m)"),
            "{s}"
        );
        assert!(
            s.contains("reads: 2 (about 2 expected), ok 2, failed 0"),
            "{s}"
        );
        assert!(s.contains("events: none"), "{s}");
    }

    #[test]
    fn recoveries_warn_and_failures_fail() {
        let log = "\
2026-11-08T00:00:00Z event connected v1.0.11
2026-11-08T00:00:01Z ok 12ms 04699E22C01C90
2026-11-08T03:00:00Z event recovery_usb_reset ACS ACR1255U-J1 stopped reporting cards
2026-11-08T03:00:01Z event recovery_recovered the reader works again
";
        let s = summarize("h", log);
        assert!(s.contains("result: WARN"), "{s}");
        assert!(
            s.contains("recovery_recovered 1, recovery_usb_reset 1"),
            "{s}"
        );

        let log = format!(
            "{log}\
2026-11-08T04:00:01Z fail no_card no tag on ACS ACR1255U-J1
2026-11-08T04:01:01Z fail no_card no tag on ACS ACR1255U-J1
2026-11-08T04:02:01Z ok 12ms 04699E22C01C90
2026-11-08T05:00:01Z fail timeout no answer within 15s
"
        );
        let s = summarize("h", &log);
        assert!(s.contains("result: FAIL"), "{s}");
        assert!(
            s.contains("failures by reason: no_card 2, timeout 1"),
            "{s}"
        );
        assert!(
            s.contains("longest run of failures: 2, ending 2026-11-08T04:01:01Z"),
            "{s}"
        );
        assert!(
            s.contains("first failure: 2026-11-08T04:00:01Z, last failure: 2026-11-08T05:00:01Z"),
            "{s}"
        );
    }

    #[test]
    fn the_days_version_names_the_upload() {
        let log = "2026-11-08T00:00:00Z event connected v1.0.11\n\
                   2026-11-08T09:00:00Z event connected v1.0.12\n\
                   2026-11-08T09:01:00Z ok 12ms 04699E22C01C90\n";
        assert_eq!(last_version(log).as_deref(), Some("1.0.12"));
        assert_eq!(last_version("2026-11-08T00:00:00Z event started\n"), None);
    }

    #[test]
    fn timestamps_round_trip() {
        let t = parse_timestamp("2026-11-08T18:23:38Z").unwrap();
        assert_eq!(timestamp(t), "2026-11-08T18:23:38Z");
        assert_eq!(compact(t), "20261108T182338Z");
        assert_eq!(parse_timestamp("2026-11-08T18:23Z"), None);
        assert_eq!(parse_timestamp("2026-11-08 18:23:38"), None);
        let log = "2026-11-08T23:59:00Z event started\nnot a line\n2026-11-09T00:01:00Z ok 9ms X\n";
        let (a, b) = span(log).unwrap();
        assert_eq!(
            (timestamp(a), timestamp(b)),
            ("2026-11-08T23:59:00Z".into(), "2026-11-09T00:01:00Z".into())
        );
    }

    #[test]
    fn a_run_without_reads_fails() {
        let s = summarize("h", "2026-11-08T00:00:00Z event started\n");
        assert!(s.contains("result: FAIL"), "{s}");
    }
}
