//! Daily check of the latest GitHub release. Never downloads or installs anything.

use std::time::Duration;

use serde::Deserialize;

pub const REPO: &str = "andreykats/topps-nfc-bridge";
const INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateInfo {
    pub version: String,
    pub url: String,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
}

/// `v1.2.3` / `1.2.3` -> (1, 2, 3). Pre-release suffixes are ignored.
fn parse(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim().trim_start_matches('v');
    let core = v.split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
    Some((
        it.next()??,
        it.next().flatten().unwrap_or(0),
        it.next().flatten().unwrap_or(0),
    ))
}

pub fn is_newer(latest: &str, current: &str) -> bool {
    matches!((parse(latest), parse(current)), (Some(l), Some(c)) if l > c)
}

fn check(current: &str) -> Result<Option<UpdateInfo>, ureq::Error> {
    let release: Release = ureq::get(format!(
        "https://api.github.com/repos/{REPO}/releases/latest"
    ))
    .header("User-Agent", format!("topps-nfc-bridge/{current}"))
    .header("Accept", "application/vnd.github+json")
    .config()
    .timeout_global(Some(Duration::from_secs(20)))
    .build()
    .call()?
    .body_mut()
    .read_json()?;
    Ok(is_newer(&release.tag_name, current).then(|| UpdateInfo {
        version: release.tag_name.trim_start_matches('v').to_string(),
        url: release.html_url,
    }))
}

/// Check now and then daily on a background thread; `found` is called when a newer release exists.
pub fn spawn(current: &'static str, found: impl Fn(UpdateInfo) + Send + 'static) {
    let _ = std::thread::Builder::new()
        .name("update-check".into())
        .spawn(move || {
            loop {
                match check(current) {
                    Ok(Some(info)) => {
                        tracing::info!(version = %info.version, "update available");
                        found(info);
                    }
                    Ok(None) => tracing::debug!("up to date"),
                    Err(e) => tracing::info!(error = %e, "update check failed"),
                }
                std::thread::sleep(INTERVAL);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("v0.1.10", "0.1.9"));
        assert!(is_newer("1.0", "0.9.9"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("v0.0.9", "0.1.0"));
        assert!(!is_newer("garbage", "0.1.0"));
        assert!(!is_newer("v0.1.0-rc1", "0.1.0"));
    }
}
