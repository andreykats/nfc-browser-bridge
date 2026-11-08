//! Shared by the app binary, the staging probe and the icon generator example.

use std::path::PathBuf;

pub mod http;
pub mod icons;
pub mod upload;

/// The bridge's log folder: ~/Library/Logs/NFCBrowserBridge on macOS,
/// %LOCALAPPDATA%\NFCBrowserBridge\logs on Windows.
pub fn log_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    return dirs::home_dir().map(|h| h.join("Library/Logs/NFCBrowserBridge"));
    #[cfg(not(target_os = "macos"))]
    return dirs::data_local_dir().map(|d| d.join("NFCBrowserBridge").join("logs"));
}
