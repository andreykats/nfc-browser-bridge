//! Check of the latest GitHub release: daily, and when the user asks. Installing it is up to the
//! user (see `install`).

use std::time::Duration;

use serde::Deserialize;

use crate::http;

pub const REPO: &str = "andreykats/nfc-browser-bridge";
const INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const TIMEOUT: Duration = Duration::from_secs(20);

/// This platform's installer in each release, as published by release.yml.
#[cfg(target_os = "macos")]
pub const ASSET: &str = "NFC-Browser-Bridge-macos.dmg";
#[cfg(windows)]
pub const ASSET: &str = "NFC-Browser-Bridge-windows-setup.exe";
/// The minisign signature of an asset is published next to it under this suffix.
pub const SIGNATURE_SUFFIX: &str = ".minisig";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateInfo {
    pub version: String,
    /// The release page, opened when the update cannot be installed from the app.
    pub url: String,
    /// None if the release has no signed installer for this platform.
    pub download: Option<Download>,
}

/// A signed installer in a release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Download {
    pub name: String,
    pub url: String,
    pub signature_url: String,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

/// The installer `name` and its signature among a release's assets.
fn find_download(assets: &[Asset], name: &str) -> Option<Download> {
    let url = |n: &str| {
        assets
            .iter()
            .find(|a| a.name == n)
            .map(|a| a.browser_download_url.clone())
    };
    Some(Download {
        name: name.to_string(),
        url: url(name)?,
        signature_url: url(&format!("{name}{SIGNATURE_SUFFIX}"))?,
    })
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
    let release: Release = http::agent(TIMEOUT)
        .get(format!(
            "https://api.github.com/repos/{REPO}/releases/latest"
        ))
        .header("User-Agent", format!("nfc-browser-bridge/{current}"))
        .header("Accept", "application/vnd.github+json")
        .call()?
        .body_mut()
        .read_json()?;
    Ok(is_newer(&release.tag_name, current).then(|| UpdateInfo {
        version: release.tag_name.trim_start_matches('v').to_string(),
        url: release.html_url,
        download: find_download(&release.assets, ASSET),
    }))
}

/// Check once and log the outcome. `Ok(None)`: up to date (or nothing released yet).
fn check_logged(current: &str) -> Result<Option<UpdateInfo>, ureq::Error> {
    match check(current) {
        Ok(Some(info)) => {
            tracing::info!(version = %info.version, "update available");
            Ok(Some(info))
        }
        Ok(None) => {
            tracing::debug!("up to date");
            Ok(None)
        }
        Err(ureq::Error::StatusCode(404)) => {
            tracing::debug!("no releases");
            Ok(None)
        }
        Err(e) => {
            tracing::info!(error = %e, "update check failed");
            Err(e)
        }
    }
}

/// Check now and then daily on a background thread; `found` is called when a newer release exists.
pub fn spawn(current: &'static str, found: impl Fn(UpdateInfo) + Send + 'static) {
    let _ = std::thread::Builder::new()
        .name("update-check".into())
        .spawn(move || {
            loop {
                if let Ok(Some(info)) = check_logged(current) {
                    found(info);
                }
                std::thread::sleep(INTERVAL);
            }
        });
}

/// Check once on a background thread, for the user; `done` gets the newer release (`None`: up to
/// date) or why the check failed.
pub fn check_now(
    current: &'static str,
    done: impl FnOnce(Result<Option<UpdateInfo>, String>) + Send + 'static,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("update-check-now".into())
        .spawn(move || done(check_logged(current).map_err(|e| e.to_string())))
        .map(drop)
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

    fn asset(name: &str) -> Asset {
        Asset {
            name: name.into(),
            browser_download_url: format!("https://example.com/{name}"),
        }
    }

    #[test]
    fn finds_the_installer_and_its_signature() {
        let assets = [asset("a.dmg"), asset("a.dmg.minisig"), asset("b.exe")];
        assert_eq!(
            find_download(&assets, "a.dmg"),
            Some(Download {
                name: "a.dmg".into(),
                url: "https://example.com/a.dmg".into(),
                signature_url: "https://example.com/a.dmg.minisig".into(),
            })
        );
        // Unsigned or missing: the update can only be downloaded by hand.
        assert_eq!(find_download(&assets, "b.exe"), None);
        assert_eq!(find_download(&assets, "c.dmg"), None);
    }

    #[test]
    fn release_without_assets_parses() {
        let r: Release =
            serde_json::from_str(r#"{"tag_name":"v1.0.8","html_url":"https://x"}"#).unwrap();
        assert!(r.assets.is_empty());
    }

    /// Network: `cargo test -p bridge-app -- --ignored`
    #[test]
    #[ignore]
    fn reaches_github_over_os_tls() {
        let r: Release = http::agent(TIMEOUT)
            .get("https://api.github.com/repos/tauri-apps/tray-icon/releases/latest")
            .header("User-Agent", "nfc-browser-bridge-test")
            .call()
            .unwrap()
            .body_mut()
            .read_json()
            .unwrap();
        assert!(r.tag_name.contains('.'));
    }
}
