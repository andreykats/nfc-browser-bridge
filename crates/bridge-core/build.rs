//! Sets `BRIDGE_VERSION`, the version the app shows and reports:
//! - `BRIDGE_VERSION` from the environment when set (release builds);
//! - otherwise the highest `vX.Y.Z` release tag in HEAD's history, with
//!   `+<commits>.g<hash>` appended when HEAD is past it (local builds);
//! - otherwise the Cargo package version.

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=BRIDGE_VERSION");
    let version = match std::env::var("BRIDGE_VERSION") {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => git_version().unwrap_or_else(|why| {
            println!("cargo:warning=using the Cargo.toml version: {why}");
            std::env::var("CARGO_PKG_VERSION").unwrap()
        }),
    };
    println!("cargo:rustc-env=BRIDGE_VERSION={version}");
}

fn git_version() -> Result<String, String> {
    let git_dir = git(&["rev-parse", "--absolute-git-dir"])?;
    watch_refs(Path::new(&git_dir));

    let tags = git(&[
        "tag",
        "--merged",
        "HEAD",
        "--list",
        "v*",
        "--sort=-v:refname",
    ])?;
    let tag = tags
        .lines()
        .find(|t| is_release(t))
        .ok_or("no vX.Y.Z release tag in HEAD's history (try `git fetch --tags`)")?;
    let ahead = git(&["rev-list", "--count", &format!("{tag}..HEAD")])?;
    let version = tag.trim_start_matches('v');
    if ahead == "0" {
        return Ok(version.to_string());
    }
    let hash = git(&["rev-parse", "--short", "HEAD"])?;
    Ok(format!("{version}+{ahead}.g{hash}"))
}

/// Rerun when HEAD moves (checkout, commit) or tags change (fetch).
fn watch_refs(git_dir: &Path) {
    let mut paths = vec![
        git_dir.join("HEAD"),
        git_dir.join("refs/tags"),
        git_dir.join("packed-refs"),
    ];
    if let Ok(branch) = git(&["symbolic-ref", "-q", "HEAD"]) {
        paths.push(git_dir.join(branch));
    }
    // Cargo reruns on every build for a path that doesn't exist.
    for p in paths.iter().filter(|p| p.exists()) {
        println!("cargo:rerun-if-changed={}", p.display());
    }
}

fn is_release(tag: &str) -> bool {
    let Some(v) = tag.strip_prefix('v') else {
        return false;
    };
    let parts: Vec<&str> = v.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

fn git(args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
