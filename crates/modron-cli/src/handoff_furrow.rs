//! The furrow carrier (PLAN.md 10.8): the working tree travels as a furrow
//! snapshot in an S3 or local store, and the session bundle travels inside
//! that snapshot. Everything else (the HandedOff event, ownership, refusals,
//! the bundle allow-list) is shared with the git carrier in `handoff.rs`.
//!
//! Modron shells out to the `furrow` CLI, found on PATH or named by
//! `MODRON_FURROW_BIN`. Furrow 0.3.7 reads its own settings from the
//! environment; modron never reads them from config and never prints them:
//!
//! - `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION` (or
//!   `AWS_DEFAULT_REGION`): S3 credentials and region.
//! - `FURROW_S3_ENDPOINT`, `FURROW_S3_PATH_STYLE`, `FURROW_S3_ALLOW_HTTP`:
//!   the S3-compatible endpoint (furrow-core/src/s3_remote.rs).
//! - `FURROW_RECOVERY_KEY`: the 64-hex workspace key. Every host that imports
//!   or exports the same workspace must hold the same key.
//!
//! Command shapes (furrow 0.3.7, furrow-cli/src/main.rs):
//!
//! - export: `furrow --repo D remote add STORE --name modron-ID --key K`
//!   (`Command::Remote` / `RemoteCommand::Add`), `furrow --repo D snap -m MSG`
//!   (`Command::Snap`), `furrow --json --repo D sync --push --takeover`
//!   (`Command::Sync`; `--json` prints a report with a `snapshot` field).
//! - import: `furrow clone URL DEST --no-watch` with the key in
//!   `FURROW_RECOVERY_KEY` (`Command::Clone`, `#[arg(env = ...)]`).
//!
//! The recovery key reaches `remote add` on argv because furrow offers no
//! environment variable for it there. It is never echoed or logged by modron.

use crate::handoff::{self, Cleanup, ImportReport, LogDecision};
use anyhow::{bail, Context, Result};
use modron_core::SessionId;
use modron_core::SessionStore;
use modron_store::JsonlSessionStore;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Env var that holds the workspace recovery key (read by furrow `clone`).
pub const RECOVERY_KEY_ENV: &str = "FURROW_RECOVERY_KEY";
/// Env var that names the furrow binary to run instead of the one on PATH.
pub const BIN_ENV: &str = "MODRON_FURROW_BIN";
/// Env vars whose values must never appear in an error message.
const SECRET_ENVS: [&str; 3] = [
    "AWS_SECRET_ACCESS_KEY",
    "AWS_ACCESS_KEY_ID",
    RECOVERY_KEY_ENV,
];

/// `[handoff] furrow_store` and the runner. `runner` is `None` in production
/// (furrow is looked up on PATH at use time); tests set it to a test double.
#[derive(Debug, Clone)]
pub struct FurrowSettings {
    /// Store URL: `s3://bucket/prefix` or a local directory path.
    pub store: String,
    pub runner: Option<Furrow>,
}

impl FurrowSettings {
    /// The runner for this call: the test double, or the real binary and key.
    pub fn runner(&self) -> Result<Furrow> {
        match &self.runner {
            Some(runner) => Ok(runner.clone()),
            None => Furrow::from_env(),
        }
    }
}

/// How furrow is run: the binary, extra child environment (tests only; empty
/// in production) and the recovery key.
#[derive(Debug, Clone)]
pub struct Furrow {
    pub bin: PathBuf,
    pub envs: Vec<(String, String)>,
    pub recovery_key: String,
}

impl Furrow {
    /// The production runner: `MODRON_FURROW_BIN` or `furrow` on PATH, and the
    /// recovery key from `FURROW_RECOVERY_KEY`.
    pub fn from_env() -> Result<Self> {
        let bin = locate(
            std::env::var_os(BIN_ENV).as_deref(),
            std::env::var_os("PATH").as_deref(),
        )?;
        let recovery_key = std::env::var(RECOVERY_KEY_ENV)
            .ok()
            .map(|key| key.trim().to_string())
            .filter(|key| !key.is_empty())
            .with_context(|| {
                format!(
                    "set {RECOVERY_KEY_ENV} (the workspace recovery key) to use the furrow carrier"
                )
            })?;
        Ok(Self {
            bin,
            envs: Vec::new(),
            recovery_key,
        })
    }

    /// Values to strip from furrow's output before it reaches an error.
    fn secrets(&self) -> Vec<String> {
        let mut out = vec![self.recovery_key.clone()];
        for name in SECRET_ENVS {
            let value = self
                .envs
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
                .or_else(|| std::env::var(name).ok());
            if let Some(value) = value {
                if value.len() >= 8 {
                    out.push(value);
                }
            }
        }
        out
    }

    /// Runs furrow with `args` and returns its stdout. A non-zero exit becomes
    /// an error carrying furrow's stderr with every secret value removed.
    /// `key_env` passes the recovery key in `FURROW_RECOVERY_KEY` (clone).
    fn run(&self, label: &str, args: &[&str], key_env: bool) -> Result<String> {
        let mut cmd = Command::new(&self.bin);
        cmd.args(args)
            .stdin(Stdio::null())
            .envs(self.envs.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        if key_env {
            cmd.env(RECOVERY_KEY_ENV, &self.recovery_key);
        }
        let output = cmd
            .output()
            .with_context(|| format!("could not run furrow at {}", self.bin.display()))?;
        let secrets = self.secrets();
        let text = |bytes: &[u8]| redact(&String::from_utf8_lossy(bytes), &secrets);
        if !output.status.success() {
            bail!(
                "furrow {label} failed ({}): {}",
                output.status,
                text(&output.stderr).trim()
            );
        }
        Ok(text(&output.stdout))
    }
}

/// Removes every secret value from `text`.
fn redact(text: &str, secrets: &[String]) -> String {
    secrets
        .iter()
        .filter(|secret| !secret.is_empty())
        .fold(text.to_string(), |acc, secret| {
            acc.replace(secret.as_str(), "<redacted>")
        })
}

/// Finds the furrow binary: the `MODRON_FURROW_BIN` path when set, else the
/// first `furrow` on `path`. Pure, so tests pass the variables directly.
pub fn locate(bin_env: Option<&OsStr>, path: Option<&OsStr>) -> Result<PathBuf> {
    if let Some(bin) = bin_env {
        let bin = PathBuf::from(bin);
        anyhow::ensure!(
            bin.is_file(),
            "{BIN_ENV} points at {}, which is not a file",
            bin.display()
        );
        return Ok(bin);
    }
    if let Some(path) = path {
        for dir in std::env::split_paths(path) {
            let candidate = dir.join("furrow");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    bail!(
        "the furrow CLI was not found on PATH; install furrow or set {BIN_ENV} \
         to its path"
    )
}

/// The furrow namespace of a session: one workspace per session.
pub fn namespace(id: SessionId) -> String {
    format!("modron-{id}")
}

/// The clone URL of a session's furrow workspace in `store`.
pub fn workspace_url(store: &str, id: SessionId) -> String {
    format!("{}/{}", store.trim_end_matches('/'), namespace(id))
}

/// The workspace URL a handoff reference names: `furrow:<url>[@snapshot]`
/// names its own URL; a bare session id uses the configured store.
pub fn url_for(reference: &str, store: &str, id: SessionId) -> String {
    match reference.trim().strip_prefix("furrow:") {
        Some(rest) => rest.split('@').next().unwrap_or(rest).to_string(),
        None => workspace_url(store, id),
    }
}

/// Where the session bundle sits inside the working tree while furrow
/// snapshots it. It is removed on every path out of `export`.
struct BundleFile {
    tar: PathBuf,
    dir: PathBuf,
    modron_dir: PathBuf,
}

impl BundleFile {
    fn write(store_root: &Path, id: SessionId, repo: &Path) -> Result<Self> {
        let modron_dir = repo.join(".modron");
        let dir = modron_dir.join("handoff");
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("could not create {}", dir.display()))?;
        let guard = Self {
            tar: dir.join(format!("{id}.handoff.tar")),
            dir,
            modron_dir,
        };
        handoff::bundle_tar(store_root, id, &guard.dir)?;
        Ok(guard)
    }
}

impl Drop for BundleFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.tar);
        // Only empty directories go: `.modron` is the user's marker directory.
        let _ = std::fs::remove_dir(&self.dir);
        let _ = std::fs::remove_dir(&self.modron_dir);
    }
}

/// Every file and symlink under `root` as a `/`-separated relative path, with
/// the control state left out: `.git`, `.furrow` and `.modron/handoff`.
fn tree_paths(root: &Path) -> Result<BTreeSet<String>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeSet<String>) -> Result<()> {
        for entry in
            std::fs::read_dir(dir).with_context(|| format!("could not read {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let rel = path
                .strip_prefix(root)?
                .to_string_lossy()
                .replace('\\', "/");
            if rel == ".git" || rel == ".furrow" || rel == ".modron/handoff" {
                continue;
            }
            // `file_type` does not follow symlinks, so a link is a file here.
            if entry.file_type()?.is_dir() {
                walk(root, &path, out)?;
            } else {
                out.insert(rel);
            }
        }
        Ok(())
    }
    let mut out = BTreeSet::new();
    walk(root, root, &mut out)?;
    Ok(out)
}

/// Exports `id` from `repo` into the furrow store. Returns the carrier ref
/// (`furrow:<workspace url>@<snapshot>`) and the number of files captured.
/// The bundle is inside the snapshot and gone from the tree afterwards.
pub fn export(
    settings: &FurrowSettings,
    store_root: &Path,
    repo: &Path,
    id: SessionId,
) -> Result<(String, usize)> {
    let furrow = settings.runner()?;
    let repo_arg = repo.to_string_lossy().to_string();
    let ns = namespace(id);
    let bundle = BundleFile::write(store_root, id, repo)?;
    furrow.run(
        "remote add",
        &[
            "--repo",
            &repo_arg,
            "remote",
            "add",
            &settings.store,
            "--name",
            &ns,
            "--key",
            &furrow.recovery_key,
        ],
        false,
    )?;
    furrow.run(
        "snap",
        &[
            "--repo",
            &repo_arg,
            "snap",
            "-m",
            &format!("modron handoff of session {id}"),
        ],
        false,
    )?;
    let out = furrow.run(
        "sync --push",
        &[
            "--json",
            "--repo",
            &repo_arg,
            "sync",
            "--push",
            "--takeover",
        ],
        false,
    )?;
    let report: serde_json::Value =
        serde_json::from_str(out.trim()).context("furrow sync printed no JSON report")?;
    let snapshot = report
        .get("snapshot")
        .and_then(|value| value.as_str())
        .context("furrow sync report names no snapshot")?;
    let short = &snapshot[..snapshot.len().min(12)];
    let carrier_ref = format!("furrow:{}@{short}", workspace_url(&settings.store, id));
    drop(bundle);
    Ok((carrier_ref, tree_paths(repo)?.len()))
}

/// Imports the furrow handoff `reference` into `repo`. Refuses before
/// touching a file when the session log decision refuses, or when the tree
/// is dirty (or would overwrite ignored or untracked files) without `force`.
pub async fn import(
    settings: &FurrowSettings,
    store_root: &Path,
    repo: &Path,
    id: SessionId,
    reference: &str,
    force: bool,
) -> Result<ImportReport> {
    let furrow = settings.runner()?;
    let url = url_for(reference, &settings.store, id);
    // The working tree must be a git checkout: the dirty and tracked checks use git.
    handoff::git(repo, &["rev-parse", "--show-toplevel"])?;

    let scratch = Cleanup(handoff::scratch_dir(&format!("furrow-import-{id}")));
    std::fs::create_dir_all(&scratch.0)?;
    let tree = scratch.0.join("tree");
    let tree_arg = tree.to_string_lossy().to_string();
    furrow
        .run("clone", &["clone", &url, &tree_arg, "--no-watch"], true)
        .with_context(|| format!("could not fetch the handoff from {url}"))?;

    let tar = tree
        .join(".modron/handoff")
        .join(format!("{id}.handoff.tar"));
    anyhow::ensure!(
        tar.is_file(),
        "the furrow snapshot at {url} holds no session bundle for {id}; it was not exported by modron"
    );
    let extract = scratch.0.join("bundle");
    let bundle = handoff::unpack_bundle(&tar, id, &extract)?;

    // The log decision runs before any file is touched.
    let store = JsonlSessionStore::new(store_root);
    let local = std::fs::read_to_string(store.transcript_path(id)).ok();
    let log = handoff::decide_log(local.as_deref(), &bundle.log);
    if let LogDecision::Refuse(reason) = &log {
        bail!("{reason}");
    }

    let dirty = handoff::git(repo, &["status", "--porcelain"])?;
    if !dirty.is_empty() && !force {
        bail!("the working tree has uncommitted changes; commit or stash them, or pass --force");
    }

    // The snapshot's own control state stays behind: its git dir, furrow
    // state and the bundle. Everything else is the handed-off tree.
    for path in [".git", ".furrow", ".modron/handoff"] {
        let full = tree.join(path);
        if full.is_dir() {
            let _ = std::fs::remove_dir_all(&full);
        } else {
            let _ = std::fs::remove_file(&full);
        }
    }
    let target = tree_paths(&tree)?;
    let tracked: BTreeSet<String> = handoff::git(repo, &["ls-files", "-z"])?
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect();
    if !force {
        for path in &target {
            if !tracked.contains(path) && repo.join(path).symlink_metadata().is_ok() {
                bail!("{path} exists here (ignored or untracked) and would be overwritten; pass --force");
            }
        }
    }
    let status = Command::new("cp")
        .arg("-a")
        .arg(format!("{}/.", tree.display()))
        .arg(repo)
        .status()
        .context("could not run cp")?;
    anyhow::ensure!(
        status.success(),
        "cp failed while restoring the furrow snapshot"
    );
    let mut files_removed = 0;
    for path in tracked.difference(&target) {
        let full = repo.join(path);
        if full.symlink_metadata().is_ok() {
            std::fs::remove_file(&full)
                .with_context(|| format!("could not remove {}", full.display()))?;
            files_removed += 1;
        }
    }

    if log == LogDecision::Write {
        store
            .replace_transcript(id, &bundle.log, bundle.title.as_deref())
            .map_err(anyhow::Error::from)?;
        if let Some(rewind) = bundle.rewind.as_deref() {
            std::fs::write(store_root.join(format!("{id}.rewind")), rewind)?;
        }
    }
    let events = store.load(id).await?;
    Ok(ImportReport {
        session: id,
        files_written: target.len(),
        files_removed,
        events: events.len(),
        still_handed_off: modron_core::handoff::handed_off_ref(&events).is_some(),
    })
}

/// Builds the carrier from `[handoff]` settings (used by `Carrier::from_config`).
pub fn settings_from_config(store: Option<&str>) -> Result<FurrowSettings> {
    let store = store.map(str::trim).filter(|s| !s.is_empty()).context(
        "`[handoff] furrow_store` is not set in config.toml (an s3:// URL, or a local directory)",
    )?;
    Ok(FurrowSettings {
        store: store.to_string(),
        runner: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handoff::Carrier;
    use modron_core::SessionEvent;

    const SENTINEL: &str = "SENTINEL-SECRET-DO-NOT-PRINT";

    fn key() -> String {
        "ab".repeat(32)
    }

    /// A test double for the furrow CLI. It records each call, maps the
    /// workspace named by `remote add` onto a directory store, and copies the
    /// whole tree (ignored files and `.git` included, as furrow does).
    const FAKE: &str = r#"#!/usr/bin/env bash
set -u
{ printf 'CALL'; printf ' [%s]' "$@"; printf '\n'; } >> "${FAKE_FURROW_LOG:?}"
if [ "${FAKE_LEAK:-}" = 1 ]; then echo "leak: ${AWS_SECRET_ACCESS_KEY:-}" >&2; exit 3; fi
json=0; repo=""
while [ $# -gt 0 ]; do
  case "$1" in
    --json) json=1; shift;;
    --repo) repo=$2; shift 2;;
    *) break;;
  esac
done
cmd=$1; shift
pairfile="${FAKE_FURROW_STATE:?}/pair-$(printf '%s' "$repo" | tr '/' '_')"
if [ "${FAKE_FAIL:-}" = "$cmd" ]; then echo "boom: simulated failure" >&2; exit 4; fi
case "$cmd" in
  remote)
    [ "$1" = add ] || exit 2; shift
    store=$1; shift; ns=""; key=""
    while [ $# -gt 0 ]; do
      case "$1" in --name) ns=$2; shift 2;; --key) key=$2; shift 2;; *) shift;; esac
    done
    mkdir -p "$store"
    printf '%s\n%s\n%s\n' "$store" "$ns" "$key" > "$pairfile"
    echo "Paired $ns";;
  snap) echo "Snapshot 000";;
  sync)
    [ -f "$pairfile" ] || { echo "not paired" >&2; exit 2; }
    store=$(sed -n 1p "$pairfile"); ns=$(sed -n 2p "$pairfile"); key=$(sed -n 3p "$pairfile")
    rm -rf "$store/$ns"; mkdir -p "$store/$ns"
    cp -a "$repo/." "$store/$ns/"
    printf '%s' "$key" > "$store/$ns.key"
    snap=$(printf '%s-%s-%s' "$ns" "$RANDOM" "$RANDOM" | sha1sum | cut -c1-40)
    if [ $json = 1 ]; then printf '{"snapshot":"%s","uploaded_objects":1}\n' "$snap"
    else echo "Published snapshot ${snap:0:12}"; fi;;
  clone)
    url=$1; dest=$2
    store=$(dirname "$url"); ns=$(basename "$url")
    [ -d "$store/$ns" ] || { echo "no workspace $ns" >&2; exit 2; }
    [ "$(cat "$store/$ns.key")" = "${FURROW_RECOVERY_KEY:-}" ] || { echo "recovery key does not match" >&2; exit 5; }
    mkdir -p "$dest"; cp -a "$store/$ns/." "$dest/"
    echo "Cloned $ns";;
  *) echo "unknown command $cmd" >&2; exit 2;;
esac
"#;

    struct Env {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        fake: PathBuf,
        log: PathBuf,
        state: PathBuf,
    }

    fn env() -> Env {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let fake = root.join("furrow");
        std::fs::write(&fake, FAKE).unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let state = root.join("state");
        std::fs::create_dir_all(&state).unwrap();
        Env {
            log: root.join("calls.log"),
            _tmp: tmp,
            root,
            fake,
            state,
        }
    }

    fn runner(env: &Env, extra: &[(&str, &str)]) -> Furrow {
        let mut envs = vec![
            ("FAKE_FURROW_LOG".to_string(), env.log.display().to_string()),
            (
                "FAKE_FURROW_STATE".to_string(),
                env.state.display().to_string(),
            ),
            ("AWS_SECRET_ACCESS_KEY".to_string(), SENTINEL.to_string()),
        ];
        envs.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        Furrow {
            bin: env.fake.clone(),
            envs,
            recovery_key: key(),
        }
    }

    fn settings(env: &Env, runner: Furrow) -> FurrowSettings {
        FurrowSettings {
            store: env.root.join("store").display().to_string(),
            runner: Some(runner),
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A checkout with one tracked file and a `.gitignore` that hides `*.log`.
    fn checkout(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("README.md"), "hello\n").unwrap();
        std::fs::write(dir.join(".gitignore"), "*.log\n").unwrap();
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", "base"]);
    }

    async fn seed(sessions: &Path, texts: &[&str]) -> SessionId {
        let store = JsonlSessionStore::new(sessions);
        let meta = store.create_session("furrow round trip").await.unwrap();
        for text in texts {
            store
                .append(
                    meta.id,
                    &SessionEvent::Task {
                        text: (*text).into(),
                    },
                )
                .await
                .unwrap();
        }
        meta.id
    }

    #[test]
    fn locate_prefers_the_named_binary_and_reports_a_missing_one() {
        let env = env();
        let found = locate(None, Some(env.root.as_os_str())).unwrap();
        assert_eq!(found, env.fake);
        let err = locate(None, Some(std::ffi::OsStr::new(""))).unwrap_err();
        assert!(err.to_string().contains("not found on PATH"), "{err}");
        let err = locate(Some(std::ffi::OsStr::new("/nonexistent/furrow")), None).unwrap_err();
        assert!(err.to_string().contains("MODRON_FURROW_BIN"), "{err}");
    }

    #[test]
    fn carrier_needs_a_store() {
        let err = settings_from_config(None).unwrap_err();
        assert!(err.to_string().contains("furrow_store"), "{err}");
        assert!(settings_from_config(Some("  ")).is_err());
        assert!(settings_from_config(Some("s3://bucket/modron")).is_ok());
    }

    #[test]
    fn references_name_the_workspace_and_the_session() {
        let id = SessionId::nil();
        let url = format!("s3://bucket/prefix/modron-{id}");
        let reference = format!("furrow:{url}@abc123");
        assert_eq!(url_for(&reference, "unused", id), url);
        assert_eq!(url_for(&id.to_string(), "s3://bucket/prefix", id), url);
        assert_eq!(handoff::parse_reference(&reference).unwrap(), id);
    }

    #[tokio::test]
    async fn round_trip_carries_ignored_files_and_leaves_no_bundle_behind() {
        let env = env();
        let a = env.root.join("a");
        let b = env.root.join("b");
        checkout(&a);
        checkout(&b);
        std::fs::write(a.join("build.log"), "ignored but travels\n").unwrap();
        std::fs::write(a.join("notes.txt"), "untracked but travels\n").unwrap();
        let sessions_a = env.root.join("sessions-a");
        let sessions_b = env.root.join("sessions-b");
        let id = seed(&sessions_a, &["first", "second"]).await;
        let carrier = Carrier::Furrow(settings(&env, runner(&env, &[])));

        let report = handoff::export(carrier.clone(), &sessions_a, &a, Some(id), None)
            .await
            .unwrap();
        assert!(
            report.tree_ref.starts_with("furrow:"),
            "{}",
            report.tree_ref
        );
        assert!(report.tree_ref.contains(&format!("modron-{id}@")));
        // The bundle travelled inside the snapshot ...
        let stored = Path::new(&settings(&env, runner(&env, &[])).store)
            .join(format!("modron-{id}"))
            .join(".modron/handoff")
            .join(format!("{id}.handoff.tar"));
        assert!(stored.is_file(), "bundle missing from the snapshot");
        // ... and never stayed in the working tree.
        assert!(!a.join(".modron/handoff").exists());
        // The exporter now refuses to run the session.
        let events = JsonlSessionStore::new(&sessions_a).load(id).await.unwrap();
        assert!(modron_core::handoff::handed_off_ref(&events).is_some());
        let again = handoff::export(carrier.clone(), &sessions_a, &a, Some(id), None).await;
        assert!(again
            .unwrap_err()
            .to_string()
            .contains("already handed off"));

        // Import into a fresh checkout with its own (empty) session store.
        let imported =
            handoff::import(carrier, &sessions_b, &b, &report.tree_ref, None, false).await;
        let imported = imported.expect("import succeeds");
        assert_eq!(imported.session, id);
        assert!(!imported.still_handed_off);
        assert_eq!(imported.events, 2);
        assert_eq!(
            std::fs::read_to_string(b.join("build.log")).unwrap(),
            "ignored but travels\n"
        );
        assert_eq!(
            std::fs::read_to_string(b.join("notes.txt")).unwrap(),
            "untracked but travels\n"
        );
        assert!(!b.join(".modron/handoff").exists());
        let restored = JsonlSessionStore::new(&sessions_b).load(id).await.unwrap();
        assert_eq!(restored.len(), 2);
        // The import never moved the checkout's own git state.
        let log = Command::new("git")
            .arg("-C")
            .arg(&b)
            .args(["log", "--oneline"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&log.stdout).lines().count(), 1);
        // The sync call asked for JSON, and the clone step ran.
        let calls = std::fs::read_to_string(&env.log).unwrap();
        assert!(calls.contains("CALL [--json] [--repo]"), "{calls}");
        assert!(calls.contains("[clone]"), "{calls}");
    }

    #[tokio::test]
    async fn import_refuses_a_dirty_tree_and_a_diverged_log_before_touching_files() {
        let env = env();
        let a = env.root.join("a");
        let b = env.root.join("b");
        checkout(&a);
        checkout(&b);
        let sessions_a = env.root.join("sessions-a");
        let sessions_b = env.root.join("sessions-b");
        let id = seed(&sessions_a, &["first"]).await;
        let carrier = Carrier::Furrow(settings(&env, runner(&env, &[])));
        let report = handoff::export(carrier.clone(), &sessions_a, &a, Some(id), None)
            .await
            .unwrap();

        // Dirty tree: refused unless forced.
        std::fs::write(b.join("README.md"), "local edit\n").unwrap();
        let err = handoff::import(
            carrier.clone(),
            &sessions_b,
            &b,
            &report.tree_ref,
            None,
            false,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("uncommitted changes"), "{err}");
        assert_eq!(
            std::fs::read_to_string(b.join("README.md")).unwrap(),
            "local edit\n"
        );
        assert!(!sessions_b.join(format!("{id}.jsonl")).exists());

        // Diverged log on this host: refused, nothing written.
        git(&b, &["checkout", "--", "README.md"]);
        JsonlSessionStore::new(&sessions_b)
            .replace_transcript(
                id,
                &format!(
                    "{}\n",
                    serde_json::to_string(&SessionEvent::Task {
                        text: "other".into()
                    })
                    .unwrap()
                ),
                None,
            )
            .unwrap();
        let err = handoff::import(
            carrier.clone(),
            &sessions_b,
            &b,
            &report.tree_ref,
            None,
            false,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("diverged"), "{err}");
        assert!(!b.join("build.log").exists());
    }

    #[tokio::test]
    async fn missing_binary_and_non_zero_exit_are_clear_errors_without_secrets() {
        let env = env();
        let a = env.root.join("a");
        checkout(&a);
        let sessions_a = env.root.join("sessions-a");
        let id = seed(&sessions_a, &["first"]).await;

        // Missing binary.
        let missing = Furrow {
            bin: env.root.join("no-such-furrow"),
            envs: Vec::new(),
            recovery_key: key(),
        };
        let carrier = Carrier::Furrow(settings(&env, missing));
        let err = handoff::export(carrier, &sessions_a, &a, Some(id), None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("could not run furrow"), "{err}");
        assert!(!a.join(".modron/handoff").exists());

        // Non-zero exit from the sync step: stderr passes through, no event is written.
        let carrier = Carrier::Furrow(settings(&env, runner(&env, &[("FAKE_FAIL", "sync")])));
        let err = handoff::export(carrier, &sessions_a, &a, Some(id), None)
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("boom: simulated failure"),
            "{err:#}"
        );
        assert!(!a.join(".modron/handoff").exists());
        let events = JsonlSessionStore::new(&sessions_a).load(id).await.unwrap();
        assert!(modron_core::handoff::handed_off_ref(&events).is_none());

        // A furrow that leaks its secret env var: the value is redacted.
        let carrier = Carrier::Furrow(settings(&env, runner(&env, &[("FAKE_LEAK", "1")])));
        let err = handoff::export(carrier, &sessions_a, &a, Some(id), None)
            .await
            .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("<redacted>"), "{text}");
        assert!(!text.contains(SENTINEL), "secret leaked: {text}");
        assert!(!text.contains(&key()), "recovery key leaked: {text}");
    }

    #[tokio::test]
    async fn clone_with_the_wrong_recovery_key_is_refused() {
        let env = env();
        let a = env.root.join("a");
        let b = env.root.join("b");
        checkout(&a);
        checkout(&b);
        let sessions_a = env.root.join("sessions-a");
        let id = seed(&sessions_a, &["first"]).await;
        let carrier = Carrier::Furrow(settings(&env, runner(&env, &[])));
        let report = handoff::export(carrier, &sessions_a, &a, Some(id), None)
            .await
            .unwrap();

        let mut wrong = runner(&env, &[]);
        wrong.recovery_key = "cd".repeat(32);
        let carrier = Carrier::Furrow(settings(&env, wrong));
        let err = handoff::import(
            carrier,
            &env.root.join("sessions-b"),
            &b,
            &report.tree_ref,
            None,
            false,
        )
        .await
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("recovery key does not match"),
            "{err:#}"
        );
        assert!(!b.join("build.log").exists());
    }
}
