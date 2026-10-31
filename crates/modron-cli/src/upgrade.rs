//! Self-upgrade (Phase 6e): `modron upgrade` fetches the latest GitHub
//! release for this repo, downloads the platform archive, verifies it, and
//! replaces the running binary (backup kept).
//!
//! Update checks (`check_for_update`) are cached for 24h under
//! `~/.modron/update-check.json` so startup stays fast and offline-friendly;
//! the TUI and chat render the notice when a newer version exists.
//!
//! Access: unauthenticated first; on failure, `MODRON_GITHUB_TOKEN`, then
//! `GITHUB_TOKEN`, then the `gh` keyring token — needed while the repo is
//! private. A public repo needs none of it.

use anyhow::anyhow;
use serde_json::json;
use serde_json::Value;
use std::path::PathBuf;

pub const REPO: &str = "jbeck018/modron";
/// Update-check cache lifetime.
pub const CHECK_TTL_HOURS: i64 = 24;

/// The platform archive name — MUST match the release workflow
/// (`.github/workflows/release.yml`) and `install.sh`.
pub fn platform_archive() -> anyhow::Result<String> {
    let arch = match std::env::consts::ARCH {
        "aarch64" => "aarch64",
        "x86_64" => "x86_64",
        other => return Err(anyhow!("unsupported architecture: {other}")),
    };
    let os = match std::env::consts::OS {
        "macos" => "apple-darwin",
        "linux" => "unknown-linux-gnu",
        other => return Err(anyhow!("unsupported OS: {other} (build from source)")),
    };
    Ok(format!("modron-{arch}-{os}.tar.gz"))
}

/// Parse "0.1.0" / "v0.1.0" into comparable numbers.
pub fn parse_version(tag: &str) -> Option<(u64, u64, u64)> {
    let t = tag.trim_start_matches('v');
    let mut parts = t.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

/// True when `latest` is newer than `current`.
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

/// Resolve the GitHub API auth header value: explicit env wins, then the gh
/// keyring (the repo is private until published). `None` = try anonymous.
fn github_token() -> Option<String> {
    if let Ok(t) = std::env::var("MODRON_GITHUB_TOKEN") {
        if !t.is_empty() {
            return Some(t);
        }
    }
    if let Ok(t) = std::env::var("GITHUB_TOKEN") {
        if !t.is_empty() {
            return Some(t);
        }
    }
    // Fall back to the gh keyring if the CLI exists.
    let out = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .ok()?;
    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if out.status.success() && !token.is_empty() {
        Some(token)
    } else {
        None
    }
}

fn api_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent("modron-upgrade")
        .build()
}

/// The latest release: (tag, archive download URL).
pub async fn latest_release() -> anyhow::Result<(String, String)> {
    let archive = platform_archive()?;
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let client = api_client()?;
    let mut request = client.get(&url);
    if let Some(token) = github_token() {
        request = request.bearer_auth(token);
    }
    let response = request.send().await?;
    if !response.status().is_success() {
        return Err(anyhow!(
            "cannot query releases ({}): is there a published release? \
             (tag a version and let the Release workflow build the archives)",
            response.status()
        ));
    }
    let body: Value = response.json().await?;
    let tag = body
        .get("tag_name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("release response missing tag_name"))?
        .to_string();
    let asset_url = body
        .get("assets")
        .and_then(|a| a.as_array())
        .and_then(|assets| {
            assets
                .iter()
                .find(|a| a.get("name").and_then(|v| v.as_str()) == Some(archive.as_str()))
                .and_then(|a| a.get("url").and_then(|v| v.as_str()).map(String::from))
        })
        .ok_or_else(|| {
            anyhow!(
                "release {tag} has no asset named `{archive}` — the Release \
                 workflow may have failed; check the Actions tab"
            )
        })?;
    Ok((tag, asset_url))
}

/// The cached update-check verdict: Some(tag) when a newer version exists.
pub fn cached_update() -> Option<String> {
    let path = check_cache_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let checked_at = value.get("checked_at")?.as_str()?;
    let checked = chrono::DateTime::parse_from_rfc3339(checked_at).ok()?;
    let age = chrono::Utc::now().signed_duration_since(checked);
    if age.num_hours() >= CHECK_TTL_HOURS {
        return None;
    }
    let tag = value.get("newer_version")?.as_str()?;
    (!tag.is_empty()).then(|| tag.to_string())
}

fn check_cache_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".modron").join("update-check.json"))
}

/// Store the check result (empty `newer_version` = up to date).
pub fn cache_check(newer_version: Option<&str>) -> anyhow::Result<()> {
    let Some(path) = check_cache_path() else {
        return Err(anyhow!("no home directory"));
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = json!({
        "checked_at": chrono::Utc::now().to_rfc3339(),
        "newer_version": newer_version.unwrap_or(""),
    });
    std::fs::write(&path, body.to_string())?;
    Ok(())
}

/// The startup check: cached verdict first; on cache miss, one live query
/// (short timeout) and cache the outcome. Never fails the caller.
pub fn startup_update_notice() -> Option<String> {
    if let Some(tag) = cached_update() {
        return Some(tag);
    }
    // curl subprocess, not reqwest::blocking: a blocking client panics on
    // drop inside the tokio runtime, and curl matches install.sh.
    let output = std::process::Command::new("curl")
        .args([
            "-fsSL",
            "-m",
            "2",
            &format!("https://api.github.com/repos/{REPO}/releases/latest"),
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None; // private repo without auth, offline, or no release
    }
    let value: Value = serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).ok()?;
    let tag = value.get("tag_name")?.as_str()?.to_string();
    let newer = is_newer(&tag, env!("CARGO_PKG_VERSION"));
    let _ = cache_check(newer.then_some(tag.as_str()));
    newer.then_some(tag)
}

/// Download `url` (the asset API URL) to a temp file next to the current
/// binary, extract the `modron` binary, and replace the current executable
/// (previous binary kept as `.bak`).
pub async fn perform_upgrade(asset_url: &str) -> anyhow::Result<PathBuf> {
    let current = std::env::current_exe()?;
    let Some(current) = current.parent().map(|p| p.to_path_buf()) else {
        return Err(anyhow!("cannot resolve the current binary directory"));
    };
    let staging = current.join(format!(".modron-upgrade-{}", std::process::id()));
    std::fs::create_dir_all(&staging)?;

    let archive_path = staging.join("modron.tar.gz");
    let client = api_client()?;
    let mut request = client.get(asset_url);
    request = request.header("Accept", "application/octet-stream");
    if let Some(token) = github_token() {
        request = request.bearer_auth(token);
    }
    let response = request.send().await?;
    if !response.status().is_success() {
        return Err(anyhow!(
            "download failed ({}): asset access needs repo visibility or a token",
            response.status()
        ));
    }
    let bytes = response.bytes().await?;
    if bytes.len() < 1024 {
        return Err(anyhow!("downloaded archive is suspiciously small"));
    }
    std::fs::write(&archive_path, &bytes)?;

    // Extract with the system tar (the archive contains a single `modron`).
    let extract = tokio::process::Command::new("tar")
        .args(["-xzf"])
        .arg(&archive_path)
        .current_dir(&staging)
        .output()
        .await?;
    if !extract.status.success() {
        return Err(anyhow!(
            "tar extraction failed: {}",
            String::from_utf8_lossy(&extract.stderr)
        ));
    }

    let new_binary = staging.join("modron");
    // Sanity: the new binary must be able to report a version.
    let version_check = tokio::process::Command::new(&new_binary)
        .arg("--version")
        .output()
        .await?;
    if !version_check.status.success() {
        return Err(anyhow!("downloaded binary failed --version — aborting"));
    }

    // Replace: current → .bak, new → current.
    let backup = current.join("modron.bak");
    let _ = std::fs::remove_file(&backup);
    std::fs::rename(current.join("modron"), &backup).map_err(|e| {
        anyhow!(
            "cannot back up the current binary (are you upgrading a cargo-built \
             target/release binary? upgrade the installed one): {e}"
        )
    })?;
    std::fs::rename(new_binary, current.join("modron")).map_err(|e| {
        // Roll the backup back on failure.
        let _ = std::fs::rename(&backup, current.join("modron"));
        anyhow!("cannot install the new binary: {e}")
    })?;
    let _ = std::fs::remove_dir_all(&staging);
    Ok(current.join("modron"))
}

/// `modron upgrade` (Phase 6e).
pub(crate) async fn run_upgrade() -> anyhow::Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    println!("modron {current} — checking {REPO} for the latest release…");
    let (tag, asset_url) = latest_release().await?;
    if !is_newer(&tag, current) {
        println!("already up to date ({current} ≥ {tag})");
        return Ok(());
    }
    println!("upgrading {current} → {tag}…");
    let installed = perform_upgrade(&asset_url).await?;
    let _ = cache_check(None); // stale notice cleared by the next startup check
    println!("upgraded → {} ({})", tag, installed.display());
    println!("the running process is still the old binary — restart to use {tag}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_name_matches_the_release_workflow() {
        // On this macOS host the expected archive mirrors release.yml's
        // aarch64-apple-darwin target.
        if cfg!(target_os = "macos") && cfg!(target_arch = "aarch64") {
            assert_eq!(
                platform_archive().unwrap(),
                "modron-aarch64-apple-darwin.tar.gz"
            );
        }
    }

    #[test]
    fn version_comparison() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        assert!(is_newer("0.1.1", "0.1.0"));
    }

    #[test]
    fn cache_roundtrip() {
        // Real path (home-dependent) — exercise parse only; the file path
        // test lives in the store-style integration suites.
        cache_check(Some("v9.9.9")).unwrap();
        assert_eq!(cached_update(), Some("v9.9.9".to_string()));
        cache_check(None).unwrap();
        assert_eq!(cached_update(), None);
    }
}
