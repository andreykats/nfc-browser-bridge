//! Installs a newer release when the user picks it in the menu. Downloads this platform's installer
//! and its minisign signature, and installs nothing unless the signature verifies against the key
//! built into the app and names this version and file. Then macOS swaps the app bundle in place;
//! Windows runs the setup silently once this process has exited.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use minisign_verify::{PublicKey, Signature, StreamVerifier};

use crate::http;
use crate::update::UpdateInfo;

/// The release signing key. release.yml signs with its secret half (secret `MINISIGN_SECRET_KEY`).
const RELEASE_KEY: &str = include_str!("../release-key.pub");
/// Installers are a few MB.
const MAX_INSTALLER: u64 = 200 * 1024 * 1024;
const MAX_SIGNATURE: u64 = 4096;
const TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// What the app does once the update is in place.
#[derive(Debug, PartialEq, Eq)]
pub enum Next {
    /// macOS: the new version has replaced this one on disk; start it.
    #[cfg(target_os = "macos")]
    Restart,
    /// Windows: the installer stops this process, then a helper starts the new version.
    #[cfg(windows)]
    Exit,
}

#[derive(Debug)]
pub enum InstallError {
    /// The release has no signed installer for this platform.
    NoDownload,
    Http(ureq::Error),
    Io(&'static str, io::Error),
    Signature(minisign_verify::Error),
    /// The signature is valid but for another version or file (e.g. an older release re-uploaded).
    WrongFile {
        expected: String,
        signed: String,
    },
    /// The downloaded disk image does not contain the app.
    #[cfg(target_os = "macos")]
    BadImage,
    /// macOS: running from a disk image or a translocated copy, which cannot be replaced.
    #[cfg(target_os = "macos")]
    NotInstalled,
    /// Windows: a path the installer command line cannot carry.
    #[cfg(any(windows, test))]
    UnsupportedPath(PathBuf),
    #[cfg(target_os = "macos")]
    Command {
        program: &'static str,
        output: String,
    },
}

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDownload => write!(f, "the release has no signed installer for this computer"),
            Self::Http(e) => write!(f, "download failed: {e}"),
            Self::Io(what, e) => write!(f, "cannot {what}: {e}"),
            Self::Signature(e) => write!(f, "the download's signature does not verify ({e})"),
            Self::WrongFile { expected, signed } => {
                write!(f, "the signature is for \"{signed}\", not \"{expected}\"")
            }
            #[cfg(target_os = "macos")]
            Self::BadImage => write!(f, "the downloaded disk image does not contain the app"),
            #[cfg(target_os = "macos")]
            Self::NotInstalled => write!(
                f,
                "the app runs from a disk image or a temporary copy. Move NFC Browser Bridge to the Applications folder, open it from there and try again"
            ),
            #[cfg(any(windows, test))]
            Self::UnsupportedPath(p) => write!(f, "cannot run the installer from {}", p.display()),
            #[cfg(target_os = "macos")]
            Self::Command { program, output } => write!(f, "{program} failed: {output}"),
        }
    }
}

fn io_err(what: &'static str) -> impl FnOnce(io::Error) -> InstallError {
    move |e| InstallError::Io(what, e)
}

/// Download, verify and install `info` on a background thread; `done` gets the outcome.
pub fn spawn(
    current: &'static str,
    info: UpdateInfo,
    done: impl FnOnce(Result<Next, InstallError>) + Send + 'static,
) -> io::Result<()> {
    std::thread::Builder::new()
        .name("update-install".into())
        .spawn(move || done(install(current, &info)))
        .map(|_| ())
}

/// Where downloads go. Emptied at every start; on Windows the setup runs from here after exit.
fn work_dir() -> PathBuf {
    std::env::temp_dir().join("nfc-browser-bridge-update")
}

/// Remove what an earlier update left behind.
pub fn cleanup() {
    if let Err(e) = remove_if_exists(&work_dir()) {
        tracing::debug!(error = %e, "cannot remove old update files");
    }
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

fn install(current: &str, info: &UpdateInfo) -> Result<Next, InstallError> {
    let download = info.download.as_ref().ok_or(InstallError::NoDownload)?;
    let key = PublicKey::decode(RELEASE_KEY).map_err(InstallError::Signature)?;
    let work = work_dir();
    remove_if_exists(&work).map_err(io_err("clear the download folder"))?;
    fs::create_dir_all(&work).map_err(io_err("create the download folder"))?;

    let agent = http::agent(TIMEOUT);
    let user_agent = format!("nfc-browser-bridge/{current}");
    let signature = agent
        .get(&download.signature_url)
        .header("User-Agent", &user_agent)
        .call()
        .and_then(|mut r| {
            r.body_mut()
                .with_config()
                .limit(MAX_SIGNATURE)
                .read_to_string()
        })
        .map_err(InstallError::Http)?;
    let signature = Signature::decode(&signature).map_err(InstallError::Signature)?;
    check_signed_file(&signature, &info.version, &download.name)?;

    tracing::info!(version = %info.version, file = %download.name, "downloading update");
    let path = work.join(&download.name);
    let mut response = agent
        .get(&download.url)
        .header("User-Agent", &user_agent)
        .call()
        .map_err(InstallError::Http)?;
    let mut body = response
        .body_mut()
        .with_config()
        .limit(MAX_INSTALLER)
        .reader();
    let mut file = File::create(&path).map_err(io_err("save the download"))?;
    let verified = key
        .verify_stream(&signature)
        .map_err(InstallError::Signature)
        .and_then(|mut v| {
            copy_hashing(&mut body, &mut file, &mut v).map_err(io_err("download the update"))?;
            file.sync_all().map_err(io_err("save the download"))?;
            v.finalize().map_err(InstallError::Signature)
        });
    drop(file);
    if let Err(e) = verified {
        let _ = fs::remove_file(&path);
        return Err(e);
    }
    tracing::info!(version = %info.version, "update verified; installing");
    platform::install(&path, &work)
}

/// The trusted comment release.yml signs into each `.minisig`, binding it to one version and file.
pub fn signed_comment(version: &str, file: &str) -> String {
    format!("nfc-browser-bridge {version} {file}")
}

fn check_signed_file(signature: &Signature, version: &str, file: &str) -> Result<(), InstallError> {
    let expected = signed_comment(version, file);
    let signed = signature.trusted_comment();
    if signed == expected {
        Ok(())
    } else {
        Err(InstallError::WrongFile {
            expected,
            signed: signed.to_string(),
        })
    }
}

/// Copy `from` to `to`, feeding every byte to the signature check.
fn copy_hashing(
    from: &mut impl Read,
    to: &mut impl Write,
    v: &mut StreamVerifier,
) -> io::Result<u64> {
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0;
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) => return Ok(total),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        v.update(&buf[..n]);
        to.write_all(&buf[..n])?;
        total += n as u64;
    }
}

/// The cmd.exe arguments that run the setup silently, wait for it, then start the app at `exe`
/// whether or not it succeeded (a failed setup leaves the old version in place). `/S` makes cmd
/// strip exactly the outer quotes. Quotes and `%` (variable expansion) would break the line.
#[cfg(any(windows, test))]
fn setup_command_line(setup: &Path, exe: &Path, log: &Path) -> Result<String, InstallError> {
    let arg = |p: &Path| match p.to_str() {
        Some(s) if !s.contains(['"', '%']) => Ok(s.to_string()),
        _ => Err(InstallError::UnsupportedPath(p.to_path_buf())),
    };
    let (setup, exe, log) = (arg(setup)?, arg(exe)?, arg(log)?);
    let flags = "/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /SP-";
    let script = format!(r#"start "" /wait "{setup}" {flags} /LOG="{log}" & start "" "{exe}""#);
    Ok(format!(r#"/D /V:OFF /S /C "{script}""#))
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    /// Run a command to completion; its output becomes the error if it fails.
    pub(super) fn run(program: &'static str, cmd: &mut Command) -> Result<(), InstallError> {
        let out = cmd.output().map_err(|e| InstallError::Command {
            program,
            output: e.to_string(),
        })?;
        if out.status.success() {
            return Ok(());
        }
        let mut output = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if output.is_empty() {
            output = out.status.to_string();
        }
        Err(InstallError::Command { program, output })
    }

    /// `name` beside `path` as a hidden file: `/Applications/.X.app.update`.
    fn hidden_sibling(path: &Path, tag: &str) -> PathBuf {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        path.with_file_name(format!(".{name}.{tag}"))
    }

    /// Put `staged` in place of `target`. If that fails, `target` is left as it was.
    pub(super) fn swap(target: &Path, staged: &Path) -> io::Result<()> {
        let old = hidden_sibling(target, "old");
        remove_if_exists(&old)?;
        fs::rename(target, &old)?;
        if let Err(e) = fs::rename(staged, target) {
            if let Err(back) = fs::rename(&old, target) {
                tracing::error!(error = %back, "cannot put the old app back");
            }
            return Err(e);
        }
        if let Err(e) = remove_if_exists(&old) {
            tracing::warn!(error = %e, "cannot remove the old app");
        }
        Ok(())
    }

    /// The app's name inside the release disk image (packaging/macos/bundle.sh).
    const APP_IN_IMAGE: &str = "NFC Browser Bridge.app";

    pub fn install(dmg: &Path, work: &Path) -> Result<Next, InstallError> {
        let exe = std::env::current_exe().map_err(io_err("find the app"))?;
        if crate::autostart::is_transient_location(&exe) {
            return Err(InstallError::NotInstalled);
        }
        let bundle = bundle_of(&exe).ok_or(InstallError::NotInstalled)?;
        let exe_name = exe.file_name().ok_or(InstallError::NotInstalled)?;
        replace_bundle(dmg, &bundle, Path::new(exe_name), work)?;
        let _ = remove_if_exists(work);
        tracing::info!(app = %bundle.display(), "update installed");
        Ok(Next::Restart)
    }

    /// `…/X.app/Contents/MacOS/x` -> `…/X.app`
    pub fn bundle_of(exe: &Path) -> Option<PathBuf> {
        let macos = exe.parent()?;
        let contents = macos.parent()?;
        let bundle = contents.parent()?;
        (macos.file_name()? == "MacOS"
            && contents.file_name()? == "Contents"
            && bundle.extension()? == "app")
            .then(|| bundle.to_path_buf())
    }

    /// Mount `dmg`, copy its app next to `bundle`, and swap the two.
    pub fn replace_bundle(
        dmg: &Path,
        bundle: &Path,
        exe_name: &Path,
        work: &Path,
    ) -> Result<(), InstallError> {
        let mount = work.join("mount");
        fs::create_dir_all(&mount).map_err(io_err("create the mount point"))?;
        run(
            "hdiutil attach",
            Command::new("/usr/bin/hdiutil")
                .args(["attach", "-nobrowse", "-readonly", "-noautoopen", "-quiet"])
                .arg("-mountpoint")
                .arg(&mount)
                .arg(dmg),
        )?;
        let result = stage_and_swap(&mount.join(APP_IN_IMAGE), bundle, exe_name);
        let detach = |force: bool| {
            let mut cmd = Command::new("/usr/bin/hdiutil");
            cmd.args(["detach", "-quiet"]);
            if force {
                cmd.arg("-force");
            }
            run("hdiutil detach", cmd.arg(&mount))
        };
        if let Err(e) = detach(false).or_else(|_| detach(true)) {
            tracing::warn!(error = %e, "cannot eject the update's disk image");
        }
        result
    }

    fn stage_and_swap(src: &Path, bundle: &Path, exe_name: &Path) -> Result<(), InstallError> {
        if !src.join("Contents/MacOS").join(exe_name).is_file() {
            return Err(InstallError::BadImage);
        }
        let staged = hidden_sibling(bundle, "update");
        remove_if_exists(&staged).map_err(io_err("clear an earlier update"))?;
        // ditto keeps the bundle's signature, permissions and attributes intact.
        if let Err(e) = run(
            "ditto",
            Command::new("/usr/bin/ditto").arg(src).arg(&staged),
        ) {
            let _ = remove_if_exists(&staged);
            return Err(e);
        }
        swap(bundle, &staged).map_err(|e| {
            let _ = remove_if_exists(&staged);
            InstallError::Io("replace the app", e)
        })
    }
}

#[cfg(windows)]
mod platform {
    use super::*;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    pub fn install(setup: &Path, work: &Path) -> Result<Next, InstallError> {
        use std::os::windows::process::CommandExt;
        let exe = std::env::current_exe().map_err(io_err("find the app"))?;
        let log = crate::logging::log_dir()
            .unwrap_or_else(|| work.to_path_buf())
            .join("update-install.log");
        let line = setup_command_line(setup, &exe, &log)?;
        let cmd = std::env::var_os("ComSpec").unwrap_or_else(|| "cmd.exe".into());
        // The setup stops this app (installer.iss), so it runs from a helper that outlives it.
        Command::new(cmd)
            .raw_arg(line)
            .current_dir(work)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(io_err("start the installer"))?;
        tracing::info!(log = %log.display(), "installer started");
        Ok(Next::Exit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_KEY: &str = include_str!("../testdata/test-key.pub");
    const SIGNED: &[u8] = include_bytes!("../testdata/installer.bin");
    const SIGNATURE: &str = include_str!("../testdata/installer.bin.minisig");

    fn verify(data: &[u8]) -> Result<(), minisign_verify::Error> {
        let key = PublicKey::decode(TEST_KEY).unwrap();
        let sig = Signature::decode(SIGNATURE).unwrap();
        let mut v = key.verify_stream(&sig)?;
        let mut out = Vec::new();
        copy_hashing(&mut &data[..], &mut out, &mut v).unwrap();
        assert_eq!(out, data);
        v.finalize()
    }

    #[test]
    fn release_key_parses() {
        PublicKey::decode(RELEASE_KEY).unwrap();
    }

    #[test]
    fn verifies_while_copying() {
        verify(SIGNED).unwrap();
        let mut tampered = SIGNED.to_vec();
        tampered[0] ^= 1;
        assert!(verify(&tampered).is_err());
        assert!(verify(&SIGNED[..SIGNED.len() - 1]).is_err());
    }

    #[test]
    fn signature_from_another_key_is_rejected() {
        let release = PublicKey::decode(RELEASE_KEY).unwrap();
        let sig = Signature::decode(SIGNATURE).unwrap();
        assert!(release.verify_stream(&sig).is_err());
    }

    #[test]
    fn signature_must_name_the_version_and_file() {
        let sig = Signature::decode(SIGNATURE).unwrap();
        check_signed_file(&sig, "1.0.9", "installer.bin").unwrap();
        assert!(matches!(
            check_signed_file(&sig, "1.0.10", "installer.bin"),
            Err(InstallError::WrongFile { .. })
        ));
        assert!(check_signed_file(&sig, "1.0.9", "other.bin").is_err());
    }

    #[test]
    fn setup_command() {
        let line = setup_command_line(
            Path::new(r"C:\Users\A B\AppData\Local\Temp\nfc-browser-bridge-update\setup.exe"),
            Path::new(
                r"C:\Users\A B\AppData\Local\Programs\NFC Browser Bridge\nfc-browser-bridge.exe",
            ),
            Path::new(r"C:\Users\A B\AppData\Local\NFCBrowserBridge\logs\update-install.log"),
        )
        .unwrap();
        assert_eq!(
            line,
            r#"/D /V:OFF /S /C "start "" /wait "C:\Users\A B\AppData\Local\Temp\nfc-browser-bridge-update\setup.exe" /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /SP- /LOG="C:\Users\A B\AppData\Local\NFCBrowserBridge\logs\update-install.log" & start "" "C:\Users\A B\AppData\Local\Programs\NFC Browser Bridge\nfc-browser-bridge.exe"""#
        );
        assert!(matches!(
            setup_command_line(
                Path::new(r"C:\%TEMP%\s.exe"),
                Path::new("a"),
                Path::new("b")
            ),
            Err(InstallError::UnsupportedPath(_))
        ));
    }

    #[cfg(target_os = "macos")]
    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nfc-bridge-test-{name}-{}", std::process::id()));
        remove_if_exists(&d).unwrap();
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn swaps_directories() {
        use platform::swap;
        let d = temp("swap");
        let (target, staged) = (d.join("X.app"), d.join(".X.app.update"));
        fs::create_dir_all(target.join("Contents")).unwrap();
        fs::write(target.join("Contents/v"), "old").unwrap();
        fs::create_dir_all(staged.join("Contents")).unwrap();
        fs::write(staged.join("Contents/v"), "new").unwrap();
        swap(&target, &staged).unwrap();
        assert_eq!(
            fs::read_to_string(target.join("Contents/v")).unwrap(),
            "new"
        );
        assert!(!staged.exists() && !d.join(".X.app.old").exists());
        // A failed swap keeps the target.
        assert!(swap(&target, &d.join("missing")).is_err());
        assert_eq!(
            fs::read_to_string(target.join("Contents/v")).unwrap(),
            "new"
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn finds_the_bundle() {
        use platform::bundle_of;
        assert_eq!(
            bundle_of(Path::new(
                "/Applications/NFC Browser Bridge.app/Contents/MacOS/nfc-browser-bridge"
            )),
            Some(PathBuf::from("/Applications/NFC Browser Bridge.app"))
        );
        assert_eq!(
            bundle_of(Path::new("/repo/target/debug/nfc-browser-bridge")),
            None
        );
    }

    /// Builds a disk image like release.yml does and installs it over a copy of the app.
    /// `cargo test -p bridge-app -- --ignored replaces_a_bundle`
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn replaces_a_bundle_from_a_disk_image() {
        let d = temp("dmg");
        let make_app = |dir: &Path, version: &str| {
            let app = dir.join("NFC Browser Bridge.app");
            fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
            fs::write(app.join("Contents/MacOS/nfc-browser-bridge"), version).unwrap();
            app
        };
        let src = d.join("src");
        make_app(&src, "new");
        let dmg = d.join("update.dmg");
        platform::run(
            "hdiutil create",
            Command::new("/usr/bin/hdiutil")
                .args(["create", "-quiet", "-format", "UDZO", "-srcfolder"])
                .arg(&src)
                .arg(&dmg),
        )
        .unwrap();
        let installed = make_app(&d.join("Applications"), "old");
        let work = d.join("work");
        platform::replace_bundle(&dmg, &installed, Path::new("nfc-browser-bridge"), &work).unwrap();
        assert_eq!(
            fs::read_to_string(installed.join("Contents/MacOS/nfc-browser-bridge")).unwrap(),
            "new"
        );
        assert!(
            !work.join("mount").join("NFC Browser Bridge.app").exists(),
            "image ejected"
        );
        fs::remove_dir_all(&d).unwrap();
    }
}
