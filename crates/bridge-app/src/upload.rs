//! "Upload logs": copies the log files to Cloud Storage in the "Logs" Firebase project
//! (logs-wvmye), into a new folder per upload:
//! `nfc-browser-bridge/<host>/<UTC time>_v<version>/<file>`.
//! The bucket's rules (`firebase/storage.rules`) let anyone add files there and nobody read,
//! overwrite or delete them, so the app carries no credentials.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use time::OffsetDateTime;

use crate::http;

const ENDPOINT: &str =
    "https://firebasestorage.googleapis.com/v0/b/logs-wvmye.firebasestorage.app/o";
/// For the whole of one file's request; logs are normally a few hundred KB at most.
const TIMEOUT: Duration = Duration::from_secs(120);
/// The rules refuse files of 10 MB or more. A bigger log (e.g. `RUST_LOG=debug`) is uploaded from
/// this far before its end, with a note at the top.
const MAX_BYTES: usize = 9 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Uploaded {
    /// Where the files went, e.g. `nfc-browser-bridge/Kiosk-Mac-mini/20261028T180000Z_v1.0.7`.
    pub folder: String,
    pub files: usize,
}

#[derive(Debug)]
pub enum UploadError {
    NoLogDir,
    NoLogs(PathBuf),
    Read(PathBuf, io::Error),
    Send {
        file: String,
        done: usize,
        total: usize,
        error: ureq::Error,
    },
}

impl fmt::Display for UploadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoLogDir => write!(f, "cannot find the log folder"),
            Self::NoLogs(dir) => write!(f, "no log files in {}", dir.display()),
            Self::Read(path, e) => write!(f, "cannot read {}: {e}", path.display()),
            Self::Send {
                file,
                done,
                total,
                error,
            } => write!(
                f,
                "cannot upload {file} ({done} of {total} files uploaded): {error}"
            ),
        }
    }
}

/// Upload on a background thread; `done` is called with the result.
pub fn spawn(
    version: &'static str,
    done: impl FnOnce(Result<Uploaded, UploadError>) + Send + 'static,
) -> io::Result<()> {
    std::thread::Builder::new()
        .name("upload-logs".into())
        .spawn(move || done(upload(version)))
        .map(|_| ())
}

fn upload(version: &str) -> Result<Uploaded, UploadError> {
    let dir = crate::logging::log_dir().ok_or(UploadError::NoLogDir)?;
    let files = log_files(&dir)?;
    let folder = folder(&host_name(), OffsetDateTime::now_utc(), version);
    // Logged first, so the uploaded log shows which upload it belongs to.
    tracing::info!(%folder, files = files.len(), "uploading logs");
    let agent = http::agent(TIMEOUT);
    for (done, (name, path)) in files.iter().enumerate() {
        let body = std::fs::read(path).map_err(|e| UploadError::Read(path.clone(), e))?;
        agent
            .post(ENDPOINT)
            .query("name", format!("{folder}/{name}"))
            .header("Content-Type", "text/plain")
            .send(&capped(body, MAX_BYTES)[..])
            .map_err(|error| UploadError::Send {
                file: name.clone(),
                done,
                total: files.len(),
                error,
            })?;
    }
    Ok(Uploaded {
        folder,
        files: files.len(),
    })
}

/// The bridge's log files in `dir` (and on macOS the system log copies), by name.
fn log_files(dir: &Path) -> Result<Vec<(String, PathBuf)>, UploadError> {
    let entries = std::fs::read_dir(dir).map_err(|e| UploadError::Read(dir.to_path_buf(), e))?;
    let mut files: Vec<_> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            is_log_file(&name).then(|| (name, e.path()))
        })
        .collect();
    if files.is_empty() {
        return Err(UploadError::NoLogs(dir.to_path_buf()));
    }
    files.sort();
    Ok(files)
}

/// `bridge.YYYY-MM-DD.log` or `macos.YYYY-MM-DD.log`, the names the bucket's rules accept.
fn is_log_file(name: &str) -> bool {
    let Some((prefix, date)) = name
        .strip_suffix(".log")
        .and_then(|stem| stem.split_once('.'))
    else {
        return false;
    };
    let b = date.as_bytes();
    matches!(prefix, "bridge" | "macos")
        && b.len() == 10
        && b.iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            _ => c.is_ascii_digit(),
        })
}

fn folder(host: &str, now: OffsetDateTime, version: &str) -> String {
    format!(
        "nfc-browser-bridge/{host}/{:04}{:02}{:02}T{:02}{:02}{:02}Z_v{version}",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
    )
}

/// The computer's name as set in System Settings / Windows.
fn host_name() -> String {
    #[cfg(target_os = "macos")]
    let raw = std::process::Command::new("scutil")
        .args(["--get", "LocalHostName"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    #[cfg(not(target_os = "macos"))]
    let raw = std::env::var("COMPUTERNAME").ok();
    sanitize_host(raw.as_deref().unwrap_or_default())
}

/// Letters, digits and `-` (what the rules accept), at most 63 characters.
fn sanitize_host(raw: &str) -> String {
    let host: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(63)
        .collect();
    if host.chars().all(|c| c == '-') {
        "unknown".to_string()
    } else {
        host
    }
}

/// At most `max` bytes: the end of the file from the first whole line, after a note.
fn capped(body: Vec<u8>, max: usize) -> Vec<u8> {
    if body.len() <= max {
        return body;
    }
    let tail = &body[body.len() - max..];
    let start = tail.iter().position(|&c| c == b'\n').map_or(0, |i| i + 1);
    let mut out = format!(
        "[upload: the file is {} bytes; only its last lines are included]\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(&tail[start..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn log_file_names() {
        assert!(is_log_file("bridge.2026-10-28.log"));
        assert!(is_log_file("macos.2026-10-28.log"));
        assert!(!is_log_file("bridge.2026-10-28.log.tmp"));
        assert!(!is_log_file("bridge.log"));
        assert!(!is_log_file("other.2026-10-28.log"));
        assert!(!is_log_file("bridge.2026-1-28.log"));
        assert!(!is_log_file("bridge.2026/10/28.log"));
        assert!(!is_log_file(".DS_Store"));
    }

    #[test]
    fn folder_name() {
        assert_eq!(
            folder(
                "Kiosk-Mac-mini",
                datetime!(2026-10-28 09:05:03 UTC),
                "1.0.7"
            ),
            "nfc-browser-bridge/Kiosk-Mac-mini/20261028T090503Z_v1.0.7"
        );
    }

    #[test]
    fn host_names() {
        assert_eq!(sanitize_host("Kiosk-Mac-mini"), "Kiosk-Mac-mini");
        assert_eq!(sanitize_host("Andrey’s Mac.local"), "Andrey-s-Mac-local");
        assert_eq!(sanitize_host(""), "unknown");
        assert_eq!(sanitize_host("..."), "unknown");
        assert_eq!(sanitize_host(&"a".repeat(80)).len(), 63);
    }

    #[test]
    fn large_files_keep_their_last_whole_lines() {
        assert_eq!(capped(b"one\ntwo\n".to_vec(), 8), b"one\ntwo\n");
        let out = String::from_utf8(capped(b"one\ntwo\nthree\n".to_vec(), 9)).unwrap();
        assert_eq!(
            out,
            "[upload: the file is 14 bytes; only its last lines are included]\nthree\n"
        );
    }

    #[test]
    fn lists_only_log_files_sorted() {
        let dir = std::env::temp_dir().join(format!("bridge-upload-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in [
            "macos.2026-10-28.log",
            "bridge.2026-10-28.log",
            "bridge.2026-10-27.log",
            "notes.txt",
        ] {
            std::fs::write(dir.join(name), "x").unwrap();
        }
        let names: Vec<_> = log_files(&dir)
            .unwrap()
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            names,
            [
                "bridge.2026-10-27.log",
                "bridge.2026-10-28.log",
                "macos.2026-10-28.log"
            ]
        );
    }

    /// Network: `cargo test -p bridge-app -- --ignored`. Uploads this machine's logs to the real
    /// bucket (view them in the Firebase console; they are deleted after 90 days).
    #[test]
    #[ignore]
    fn uploads_to_the_logs_bucket() {
        let u = upload("0.0.0-test").unwrap();
        assert!(u.folder.ends_with("_v0.0.0-test"), "{}", u.folder);
        assert!(u.files > 0);
        println!("uploaded {} files to {}", u.files, u.folder);
    }
}
