//! Daily-rotated log files (7 kept): ~/Library/Logs/ToppsNFCBridge on macOS,
//! %LOCALAPPDATA%\ToppsNFCBridge\logs on Windows.

use std::path::PathBuf;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::MakeWriterExt;

pub fn log_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    return dirs::home_dir().map(|h| h.join("Library/Logs/ToppsNFCBridge"));
    #[cfg(not(target_os = "macos"))]
    return dirs::data_local_dir().map(|d| d.join("ToppsNFCBridge").join("logs"));
}

/// Start logging. Keep the guard alive for the life of the process. `RUST_LOG` overrides the level
/// (e.g. `RUST_LOG=debug` logs APDUs).
pub fn init() -> Option<WorkerGuard> {
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
    let guard = match file {
        Some(appender) => {
            let (writer, guard) = tracing_appender::non_blocking(appender);
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(writer.and(std::io::stderr))
                .init();
            Some(guard)
        }
        None => {
            tracing_subscriber::fmt().with_env_filter(filter).init();
            None
        }
    };
    std::panic::set_hook(Box::new(|info| tracing::error!("panic: {info}")));
    guard
}
