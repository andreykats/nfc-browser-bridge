//! Daily-rotated log files (7 kept): ~/Library/Logs/NFCBrowserBridge on macOS,
//! %LOCALAPPDATA%\NFCBrowserBridge\logs on Windows.

use std::path::PathBuf;

use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::MakeWriterExt;

pub fn log_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    return dirs::home_dir().map(|h| h.join("Library/Logs/NFCBrowserBridge"));
    #[cfg(not(target_os = "macos"))]
    return dirs::data_local_dir().map(|d| d.join("NFCBrowserBridge").join("logs"));
}

/// Start logging. `RUST_LOG` overrides the level (e.g. `RUST_LOG=debug` logs APDUs).
///
/// Lines are written synchronously (the bridge logs a few lines per tap), so the panic hook's line
/// is on disk before the process exits.
pub fn init() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let file = log_dir().and_then(|dir| {
        std::fs::create_dir_all(&dir).ok()?;
        RollingFileAppender::builder()
            .rotation(Rotation::DAILY)
            .filename_prefix("bridge")
            .filename_suffix("log")
            .max_log_files(7)
            .build(&dir)
            .ok()
    });
    match file {
        Some(appender) => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(appender.and(std::io::stderr))
            .init(),
        None => tracing_subscriber::fmt().with_env_filter(filter).init(),
    }
    // A panic kills only its own thread; the rest of the bridge would carry on half-dead (e.g. no
    // card events but still accepting connections and holding the port). Exit instead, so the
    // supervisor starts a whole new bridge.
    std::panic::set_hook(Box::new(|info| {
        tracing::error!("panic: {info}");
        std::process::exit(crate::supervisor::EXIT_PANIC);
    }));
}
