//! "Start at login": a LaunchAgent on macOS, the HKCU Run key on Windows.

use std::path::{Path, PathBuf};

#[cfg(target_os = "macos")]
const LABEL: &str = "dev.drey.topps-nfc-bridge";
#[cfg(windows)]
const RUN_VALUE: &str = "ToppsNFCBridge";

#[derive(Debug)]
pub enum AutostartError {
    /// macOS: running from a disk image or a translocated copy; the path will not survive.
    NotInstalled,
    Io(std::io::Error),
}

impl std::fmt::Display for AutostartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotInstalled => {
                write!(
                    f,
                    "Move Topps NFC Bridge to the Applications folder and open it from there, then try again."
                )
            }
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl From<std::io::Error> for AutostartError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

fn current_exe() -> Result<PathBuf, AutostartError> {
    Ok(std::env::current_exe()?)
}

/// macOS Gatekeeper runs quarantined apps from a randomized read-only path, and a mounted DMG
/// disappears after ejecting. Neither path can be used for login items.
pub fn is_transient_location(exe: &Path) -> bool {
    let s = exe.to_string_lossy();
    s.contains("/AppTranslocation/") || s.starts_with("/Volumes/")
}

#[cfg(target_os = "macos")]
fn plist_path() -> Result<PathBuf, AutostartError> {
    let home = dirs::home_dir().ok_or_else(|| std::io::Error::other("no home directory"))?;
    Ok(home
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The LaunchAgent: start at login in the GUI session; restart after a crash but not after Quit.
#[cfg(target_os = "macos")]
pub fn launch_agent_plist(exe: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>LimitLoadToSessionType</key>
    <string>Aqua</string>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>
"#,
        xml_escape(&exe.to_string_lossy())
    )
}

#[cfg(target_os = "macos")]
pub fn is_enabled() -> bool {
    plist_path().map(|p| p.exists()).unwrap_or(false)
}

#[cfg(target_os = "macos")]
pub fn set_enabled(enabled: bool) -> Result<(), AutostartError> {
    let path = plist_path()?;
    if !enabled {
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => return Ok(()),
        }
    }
    let exe = current_exe()?;
    if is_transient_location(&exe) {
        return Err(AutostartError::NotInstalled);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Written only; launchd loads it at the next login. Loading it now would start a second copy.
    std::fs::write(&path, launch_agent_plist(&exe))?;
    Ok(())
}

/// If enabled, point the login item at this copy of the app (e.g. after it was moved).
#[cfg(target_os = "macos")]
pub fn refresh() {
    if is_enabled()
        && let Ok(exe) = std::env::current_exe()
        && !is_transient_location(&exe)
        && let Ok(path) = plist_path()
    {
        let want = launch_agent_plist(&exe);
        if std::fs::read_to_string(&path).ok().as_deref() != Some(want.as_str())
            && let Err(e) = std::fs::write(&path, want)
        {
            tracing::warn!(error = %e, "cannot update login item");
        }
    }
}

#[cfg(windows)]
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

#[cfg(windows)]
fn hkcu() -> winreg::RegKey {
    winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
}

#[cfg(windows)]
pub fn is_enabled() -> bool {
    hkcu()
        .open_subkey(RUN_KEY)
        .and_then(|k| k.get_value::<String, _>(RUN_VALUE))
        .is_ok()
}

#[cfg(windows)]
pub fn set_enabled(enabled: bool) -> Result<(), AutostartError> {
    let (key, _) = hkcu().create_subkey(RUN_KEY)?;
    if enabled {
        let exe = current_exe()?;
        key.set_value(RUN_VALUE, &format!("\"{}\"", exe.display()))?;
    } else {
        match key.delete_value(RUN_VALUE) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
    }
    Ok(())
}

/// If enabled, point the Run value at this copy of the exe.
#[cfg(windows)]
pub fn refresh() {
    if is_enabled() {
        let _ = set_enabled(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_locations() {
        assert!(is_transient_location(Path::new(
            "/private/var/folders/x/T/AppTranslocation/ABC/d/Topps NFC Bridge.app/Contents/MacOS/topps-nfc-bridge"
        )));
        assert!(is_transient_location(Path::new(
            "/Volumes/Topps NFC Bridge/Topps NFC Bridge.app/Contents/MacOS/x"
        )));
        assert!(!is_transient_location(Path::new(
            "/Applications/Topps NFC Bridge.app/Contents/MacOS/topps-nfc-bridge"
        )));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn plist_escapes_and_has_keepalive() {
        let p = launch_agent_plist(Path::new("/Applications/A&B.app/Contents/MacOS/x"));
        assert!(p.contains("<string>/Applications/A&amp;B.app/Contents/MacOS/x</string>"));
        assert!(p.contains("<key>SuccessfulExit</key>\n        <false/>"));
        assert!(p.contains("<string>Aqua</string>"));
    }
}
