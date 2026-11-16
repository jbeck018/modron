//! Self-upgrade (Phase 6e): `modron upgrade` fetches the latest GitHub
//! release for this repo, downloads the platform archive and the release's
//! `SHA256SUMS`, verifies the archive digest (see `crate::checksums`), and only
//! then extracts it and replaces the running binary (backup kept).
//!
//! Verification detects a corrupted or truncated download and an archive
//! swapped without `SHA256SUMS` changing. It does NOT detect an attacker who
//! can replace both files in the release; that needs signed releases.
//!
//! Downgrades are refused: `run_upgrade` only installs a tag that is newer
//! than the running version, and there is no flag to override that.
//! Redirects are followed only to https URLs.
//!
//! Update checks (`check_for_update`) are cached for 24h under
//! `~/.modron/update-check.json` so startup stays fast and offline-friendly;
//! the TUI and chat render the notice when a newer version exists.
//!
//! Access: unauthenticated first; on failure, `MODRON_GITHUB_TOKEN`, then
//! `GITHUB_TOKEN`, then the `gh` keyring token — needed while the repo is
//! private. A public repo needs none of it.

use crate::checksums::{push_sums_chunk, verify_archive};
use anyhow::{anyhow, bail};
use serde_json::json;
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The asset name of the checksum manifest the release workflow uploads.
pub const SUMS_ASSET: &str = "SHA256SUMS";
/// Redirect hops allowed for a download (reqwest's own default).
const MAX_REDIRECTS: usize = 10;

pub const REPO: &str = "jbeck018/modron";
/// The terminal app shipped in the same archive as `modron` (release.yml).
pub const APP_BIN: &str = "modron-app-tui";
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
        // A stalled GitHub connection must time out — `modron upgrade` runs
        // interactively and must not hang forever on a dead network.
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(60))
        .redirect(redirect_policy())
        .build()
}

/// Redirects are followed only when the target is https and the hop limit
/// has not been reached.
pub fn redirect_allowed(url: &reqwest::Url, hops: usize) -> bool {
    url.scheme() == "https" && hops < MAX_REDIRECTS
}

fn redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if redirect_allowed(attempt.url(), attempt.previous().len()) {
            attempt.follow()
        } else {
            attempt.error("refusing redirect to a non-https URL or too many redirects")
        }
    })
}

/// Refuse to start a download from anything but https.
pub fn require_https(url: &str) -> anyhow::Result<()> {
    if url.starts_with("https://") {
        Ok(())
    } else {
        Err(anyhow!("refusing to download from a non-https URL"))
    }
}

/// A resolved GitHub release and the two assets an upgrade needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub tag: String,
    /// The platform archive's name (`platform_archive`).
    pub archive_name: String,
    /// API URL of the platform archive asset.
    pub archive_url: String,
    /// API URL of the `SHA256SUMS` asset from the same release.
    pub sums_url: String,
}

/// Pick the archive and `SHA256SUMS` assets out of a release JSON body.
/// A release without either asset is an error: there is nothing to verify
/// the archive against, so nothing is installed.
pub fn release_from_json(body: &Value, archive_name: &str) -> anyhow::Result<Release> {
    let tag = body
        .get("tag_name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("release response missing tag_name"))?
        .to_string();
    let asset_url = |name: &str| -> Option<String> {
        body.get("assets")
            .and_then(|a| a.as_array())?
            .iter()
            .find(|a| a.get("name").and_then(|v| v.as_str()) == Some(name))
            .and_then(|a| a.get("url").and_then(|v| v.as_str()).map(String::from))
    };
    let archive_url = asset_url(archive_name).ok_or_else(|| {
        anyhow!(
            "release {tag} has no asset named `{archive_name}` — the Release \
             workflow may have failed; check the Actions tab"
        )
    })?;
    let sums_url = asset_url(SUMS_ASSET).ok_or_else(|| {
        anyhow!(
            "release {tag} has no {SUMS_ASSET} asset - refusing to install an \
             unverified archive (the Release workflow's checksums job uploads it)"
        )
    })?;
    Ok(Release {
        tag,
        archive_name: archive_name.to_string(),
        archive_url,
        sums_url,
    })
}

/// The latest release for this repo, with its archive and checksum assets.
pub async fn latest_release() -> anyhow::Result<Release> {
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
    release_from_json(&body, &archive)
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

/// Fetch `url` (a GitHub asset API URL) as a stream. The URL must be https,
/// and redirects are followed only to https (see `redirect_policy`).
async fn fetch_asset(url: &str) -> anyhow::Result<reqwest::Response> {
    require_https(url)?;
    let client = api_client()?;
    let mut request = client.get(url).header("Accept", "application/octet-stream");
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
    Ok(response)
}

/// Download the release's `SHA256SUMS`, capped at `MAX_SUMS_BYTES`.
async fn download_sums(url: &str) -> anyhow::Result<String> {
    let mut response = fetch_asset(url).await?;
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        push_sums_chunk(&mut body, &chunk)?;
    }
    String::from_utf8(body).map_err(|_| anyhow!("{SUMS_ASSET} is not valid UTF-8"))
}

/// Stream the archive to `path`. Returns the byte count.
async fn download_archive(url: &str, path: &Path) -> anyhow::Result<u64> {
    let mut response = fetch_asset(url).await?;
    let mut file = std::fs::File::create(path)?;
    let mut total: u64 = 0;
    while let Some(chunk) = response.chunk().await? {
        total += chunk.len() as u64;
        file.write_all(&chunk)?;
    }
    file.flush()?;
    if total < 1024 {
        bail!("downloaded archive is suspiciously small");
    }
    Ok(total)
}

/// Download the release, verify it against its `SHA256SUMS`, and only then
/// replace the running binary (previous binary kept as `.bak`). Nothing
/// installed is touched unless verification has passed.
pub async fn perform_upgrade(release: &Release) -> anyhow::Result<PathBuf> {
    let current = std::env::current_exe()?;
    let Some(current) = current.parent().map(|p| p.to_path_buf()) else {
        return Err(anyhow!("cannot resolve the current binary directory"));
    };
    let staging = current.join(format!(".modron-upgrade-{}", std::process::id()));
    std::fs::create_dir_all(&staging)?;
    let outcome = download_and_install(&current, &staging, release).await;
    let _ = std::fs::remove_dir_all(&staging);
    outcome
}

async fn download_and_install(
    current: &Path,
    staging: &Path,
    release: &Release,
) -> anyhow::Result<PathBuf> {
    let sums = download_sums(&release.sums_url).await?;
    let archive_path = staging.join("modron.tar.gz");
    download_archive(&release.archive_url, &archive_path).await?;
    install_verified(
        current,
        staging,
        &archive_path,
        &release.archive_name,
        &sums,
    )
}

/// Verify `archive_path` against `sums` for the asset `archive_name`, then
/// extract it into `staging`, check that the new `modron` runs, and swap it
/// into `current`. Verification runs first: on any failure, no file in
/// `current` is renamed and no `.bak` is created.
fn install_verified(
    current: &Path,
    staging: &Path,
    archive_path: &Path,
    archive_name: &str,
    sums: &str,
) -> anyhow::Result<PathBuf> {
    verify_archive(archive_path, archive_name, sums)?;

    // Extract with the system tar (the archive contains `modron` and,
    // in newer releases, `modron-app-tui`).
    let extract = std::process::Command::new("tar")
        .args(["-xzf"])
        .arg(archive_path)
        .current_dir(staging)
        .output()?;
    if !extract.status.success() {
        return Err(anyhow!(
            "tar extraction failed: {}",
            String::from_utf8_lossy(&extract.stderr)
        ));
    }

    let new_binary = staging.join("modron");
    // Sanity: the new binary must be able to report a version.
    let version_check = std::process::Command::new(&new_binary)
        .arg("--version")
        .output()?;
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
    // The release archive carries the terminal app (release.yml) and `modron
    // app` needs it next to modron, so it is installed whether or not an older
    // one is there. An archive without it (older releases) leaves an installed
    // app untouched.
    let new_app = staging.join(APP_BIN);
    if new_app.is_file() {
        let existed = current.join(APP_BIN).exists();
        swap_in(current, APP_BIN, &new_app)
            .map_err(|e| anyhow!("modron was upgraded, but {APP_BIN} was not installed: {e}"))?;
        let verb = if existed { "replaced" } else { "installed" };
        println!("{verb} {APP_BIN} ({})", current.join(APP_BIN).display());
    }
    Ok(current.join("modron"))
}

/// Replace `dir/name` with `new_file` (same filesystem: the staging folder
/// lives under `dir`). A previous file is kept as `name.bak`; a failed
/// install restores it. With no previous file, the new one is just installed.
fn swap_in(dir: &std::path::Path, name: &str, new_file: &std::path::Path) -> anyhow::Result<()> {
    let installed = dir.join(name);
    let backup = dir.join(format!("{name}.bak"));
    let _ = std::fs::remove_file(&backup);
    let had_previous = installed.exists();
    if had_previous {
        std::fs::rename(&installed, &backup)?;
    }
    if let Err(e) = std::fs::rename(new_file, &installed) {
        if had_previous {
            let _ = std::fs::rename(&backup, &installed);
        }
        return Err(anyhow!("cannot install the new {name}: {e}"));
    }
    Ok(())
}

/// `modron upgrade` (Phase 6e).
pub(crate) async fn run_upgrade() -> anyhow::Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    println!("modron {current} — checking {REPO} for the latest release…");
    let release = latest_release().await?;
    // Downgrades and re-installs of the same version are refused here; there
    // is deliberately no flag to override this.
    if !is_newer(&release.tag, current) {
        println!("already up to date ({current} ≥ {})", release.tag);
        return Ok(());
    }
    println!("upgrading {current} → {}…", release.tag);
    let installed = perform_upgrade(&release).await?;
    let _ = cache_check(None); // stale notice cleared by the next startup check
    println!(
        "upgraded → {} ({}), sha256 verified against {SUMS_ASSET}",
        release.tag,
        installed.display()
    );
    let tag = &release.tag;
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
    fn swap_in_keeps_a_backup_and_installs_the_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("staging");
        std::fs::create_dir_all(&staged).unwrap();
        let new_file = staged.join(APP_BIN);
        std::fs::write(&new_file, b"new app").unwrap();
        std::fs::write(dir.path().join(APP_BIN), b"old app").unwrap();

        swap_in(dir.path(), APP_BIN, &new_file).unwrap();

        assert_eq!(std::fs::read(dir.path().join(APP_BIN)).unwrap(), b"new app");
        assert_eq!(
            std::fs::read(dir.path().join(format!("{APP_BIN}.bak"))).unwrap(),
            b"old app"
        );
        assert!(!new_file.exists());
    }

    #[test]
    fn swap_in_installs_a_new_file_when_none_is_installed_yet() {
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("staging");
        std::fs::create_dir_all(&staged).unwrap();
        let new_file = staged.join(APP_BIN);
        std::fs::write(&new_file, b"new app").unwrap();

        swap_in(dir.path(), APP_BIN, &new_file).unwrap();

        assert_eq!(std::fs::read(dir.path().join(APP_BIN)).unwrap(), b"new app");
        assert!(!dir.path().join(format!("{APP_BIN}.bak")).exists());
    }

    #[test]
    fn swap_in_restores_the_backup_when_the_install_fails() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(APP_BIN), b"old app").unwrap();
        // The staged file does not exist, so the install step fails.
        let missing = dir.path().join("staging").join(APP_BIN);

        assert!(swap_in(dir.path(), APP_BIN, &missing).is_err());
        assert_eq!(std::fs::read(dir.path().join(APP_BIN)).unwrap(), b"old app");
    }

    const ARCHIVE: &str = "modron-x86_64-unknown-linux-gnu.tar.gz";

    fn release_body(assets: &[&str]) -> Value {
        let assets: Vec<Value> = assets
            .iter()
            .map(|name| json!({ "name": name, "url": format!("https://api.example/{name}") }))
            .collect();
        json!({ "tag_name": "v9.9.9", "assets": assets })
    }

    #[test]
    fn release_resolves_both_archive_and_sums_assets() {
        let release = release_from_json(&release_body(&[ARCHIVE, "SHA256SUMS"]), ARCHIVE).unwrap();
        assert_eq!(release.tag, "v9.9.9");
        assert_eq!(
            release.archive_url,
            format!("https://api.example/{ARCHIVE}")
        );
        assert_eq!(release.sums_url, "https://api.example/SHA256SUMS");
    }

    #[test]
    fn release_without_sums_is_refused() {
        let err = release_from_json(&release_body(&[ARCHIVE]), ARCHIVE)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no SHA256SUMS asset"), "{err}");
        assert!(err.contains("unverified"), "{err}");
    }

    #[test]
    fn release_without_archive_is_refused() {
        let err = release_from_json(&release_body(&["SHA256SUMS"]), ARCHIVE)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no asset named"), "{err}");
    }

    #[test]
    fn release_without_tag_is_refused() {
        let body = json!({ "assets": [] });
        assert!(release_from_json(&body, ARCHIVE).is_err());
    }

    #[test]
    fn redirects_only_to_https_within_the_hop_limit() {
        let https = reqwest::Url::parse("https://objects.example/asset").unwrap();
        let http = reqwest::Url::parse("http://objects.example/asset").unwrap();
        let ftp = reqwest::Url::parse("ftp://objects.example/asset").unwrap();
        let cases: &[(&reqwest::Url, usize, bool)] = &[
            (&https, 0, true),
            (&https, MAX_REDIRECTS - 1, true),
            (&https, MAX_REDIRECTS, false),
            (&http, 0, false),
            (&ftp, 0, false),
        ];
        for (url, hops, expected) in cases {
            assert_eq!(
                redirect_allowed(url, *hops),
                *expected,
                "{url} after {hops}"
            );
        }
    }

    #[test]
    fn download_urls_must_be_https() {
        assert!(require_https("https://api.example/asset").is_ok());
        assert!(require_https("http://api.example/asset").is_err());
        assert!(require_https("file:///etc/passwd").is_err());
    }

    /// Build `root/<name>` as a gzipped tar holding an executable `modron`
    /// that prints `version_text`.
    #[cfg(unix)]
    fn fake_archive(root: &Path, name: &str, version_text: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let src = root.join(format!("src-{name}"));
        std::fs::create_dir_all(&src).unwrap();
        let bin = src.join("modron");
        std::fs::write(&bin, format!("#!/bin/sh\necho \"{version_text}\"\n")).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let archive = root.join(name);
        let status = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&src)
            .arg("modron")
            .status()
            .unwrap();
        assert!(status.success());
        archive
    }

    /// An install dir holding `modron` = "old modron", plus a staging dir.
    #[cfg(unix)]
    fn installed_dir(root: &Path) -> (PathBuf, PathBuf) {
        let current = root.join("install");
        let staging = current.join(".modron-upgrade-test");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(current.join("modron"), b"old modron").unwrap();
        (current, staging)
    }

    #[cfg(unix)]
    fn sums_line(archive: &Path, name: &str) -> String {
        let digest = crate::checksums::to_hex(&crate::checksums::sha256_file(archive).unwrap());
        format!("{digest}  {name}\n")
    }

    /// Build `root/<name>` as a gzipped tar holding `modron` and the app
    /// binary, both as small shell scripts.
    #[cfg(unix)]
    fn archive_with_app(root: &Path, name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let src = root.join(format!("src-{name}"));
        std::fs::create_dir_all(&src).unwrap();
        for file in ["modron", APP_BIN] {
            let path = src.join(file);
            std::fs::write(&path, format!("#!/bin/sh\necho \"{file} 9.9.9\"\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let archive = root.join(name);
        let status = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&src)
            .args(["modron", APP_BIN])
            .status()
            .unwrap();
        assert!(status.success());
        archive
    }

    #[cfg(unix)]
    #[test]
    fn archive_with_the_app_installs_the_app_even_when_none_was_installed() {
        let root = tempfile::tempdir().unwrap();
        let (current, staging) = installed_dir(root.path());
        let archive = archive_with_app(root.path(), "download.tar.gz");
        let sums = sums_line(&archive, ARCHIVE);

        install_verified(&current, &staging, &archive, ARCHIVE, &sums).unwrap();

        let app = std::fs::read_to_string(current.join(APP_BIN)).unwrap();
        assert!(app.contains("modron-app-tui 9.9.9"), "{app}");
        assert!(!current.join(format!("{APP_BIN}.bak")).exists());
        assert!(std::fs::read_to_string(current.join("modron"))
            .unwrap()
            .contains("modron 9.9.9"));
    }

    #[cfg(unix)]
    #[test]
    fn archive_with_the_app_replaces_an_installed_app_and_keeps_it_as_bak() {
        let root = tempfile::tempdir().unwrap();
        let (current, staging) = installed_dir(root.path());
        std::fs::write(current.join(APP_BIN), b"old app").unwrap();
        let archive = archive_with_app(root.path(), "download.tar.gz");
        let sums = sums_line(&archive, ARCHIVE);

        install_verified(&current, &staging, &archive, ARCHIVE, &sums).unwrap();

        assert!(std::fs::read_to_string(current.join(APP_BIN))
            .unwrap()
            .contains("modron-app-tui 9.9.9"));
        assert_eq!(
            std::fs::read(current.join(format!("{APP_BIN}.bak"))).unwrap(),
            b"old app"
        );
    }

    #[cfg(unix)]
    #[test]
    fn good_archive_with_matching_sums_is_swapped_in() {
        let root = tempfile::tempdir().unwrap();
        let (current, staging) = installed_dir(root.path());
        let archive = fake_archive(root.path(), "download.tar.gz", "modron 9.9.9");
        let sums = sums_line(&archive, ARCHIVE);

        install_verified(&current, &staging, &archive, ARCHIVE, &sums).unwrap();

        let installed = std::fs::read_to_string(current.join("modron")).unwrap();
        assert!(installed.contains("modron 9.9.9"), "{installed}");
        assert_eq!(
            std::fs::read(current.join("modron.bak")).unwrap(),
            b"old modron"
        );
    }

    #[cfg(unix)]
    #[test]
    fn checksum_mismatch_leaves_the_installed_binary_untouched() {
        let root = tempfile::tempdir().unwrap();
        let (current, staging) = installed_dir(root.path());
        let archive = fake_archive(root.path(), "download.tar.gz", "modron 9.9.9");
        let wrong = format!("{}  {ARCHIVE}\n", "0".repeat(64));

        let err = install_verified(&current, &staging, &archive, ARCHIVE, &wrong)
            .unwrap_err()
            .to_string();

        assert!(err.contains("checksum mismatch"), "{err}");
        assert!(err.contains(ARCHIVE), "{err}");
        assert!(err.contains(&"0".repeat(64)), "{err}");
        assert_eq!(
            std::fs::read(current.join("modron")).unwrap(),
            b"old modron"
        );
        assert!(!current.join("modron.bak").exists(), "no half-done swap");
        assert!(
            !staging.join("modron").exists(),
            "archive must not be extracted"
        );
    }

    #[cfg(unix)]
    #[test]
    fn archive_not_listed_in_sums_leaves_the_installed_binary_untouched() {
        let root = tempfile::tempdir().unwrap();
        let (current, staging) = installed_dir(root.path());
        let archive = fake_archive(root.path(), "download.tar.gz", "modron 9.9.9");
        let other = format!("{}  modron-aarch64-apple-darwin.tar.gz\n", "1".repeat(64));

        let err = install_verified(&current, &staging, &archive, ARCHIVE, &other)
            .unwrap_err()
            .to_string();

        assert!(err.contains("does not list"), "{err}");
        assert_eq!(
            std::fs::read(current.join("modron")).unwrap(),
            b"old modron"
        );
        assert!(!current.join("modron.bak").exists());
    }

    #[cfg(unix)]
    #[test]
    fn malformed_sums_leave_the_installed_binary_untouched() {
        let root = tempfile::tempdir().unwrap();
        let (current, staging) = installed_dir(root.path());
        let archive = fake_archive(root.path(), "download.tar.gz", "modron 9.9.9");
        let good = sums_line(&archive, ARCHIVE);
        let malformed = format!("{}\n{good}", good.to_uppercase());

        let err = install_verified(&current, &staging, &archive, ARCHIVE, &malformed)
            .unwrap_err()
            .to_string();

        assert!(err.contains("malformed SHA256SUMS line"), "{err}");
        assert_eq!(
            std::fs::read(current.join("modron")).unwrap(),
            b"old modron"
        );
        assert!(!current.join("modron.bak").exists());
    }

    #[cfg(unix)]
    #[test]
    fn extra_entries_and_binary_marker_still_verify() {
        let root = tempfile::tempdir().unwrap();
        let (current, staging) = installed_dir(root.path());
        let archive = fake_archive(root.path(), "download.tar.gz", "modron 9.9.9");
        let digest = crate::checksums::to_hex(&crate::checksums::sha256_file(&archive).unwrap());
        let sums = format!(
            "{}  modron-aarch64-apple-darwin.tar.gz\n{digest} *{ARCHIVE}\n",
            "2".repeat(64)
        );

        install_verified(&current, &staging, &archive, ARCHIVE, &sums).unwrap();
        assert!(std::fs::read_to_string(current.join("modron"))
            .unwrap()
            .contains("modron 9.9.9"));
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
