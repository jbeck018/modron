//! `modron handoff export|import` (PLAN.md 10.8, C6c): move the working tree,
//! the session log and the ownership of one session to another host.
//!
//! Carrier: git only. The export commits the working tree (tracked changes
//! and untracked, non-ignored files) to `refs/heads/modron/handoff/<id>` from
//! a temporary index, so the user's index, HEAD and branch never move. The
//! session bundle (a tar of allow-listed store files) is a second commit on
//! `refs/heads/modron/handoff/<id>-session`. Both refs are pushed when a
//! remote is named. The furrow carrier is not built yet.

use anyhow::{bail, Context, Result};
use chrono::Utc;
use modron_core::handoff::handed_off_ref;
use modron_core::{SessionEvent, SessionId, SessionStore};
use modron_store::JsonlSessionStore;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Where a handoff goes. Only git exists today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Carrier {
    Git,
}

impl Carrier {
    /// `[handoff] carrier`: unset means git. `furrow` is accepted by the
    /// config but its carrier is not built in this version.
    pub fn from_config(name: Option<&str>) -> Result<Self> {
        match name.unwrap_or("git") {
            "git" => Ok(Carrier::Git),
            "furrow" => bail!(
                "the furrow handoff carrier is not built yet; set `carrier = \"git\"` \
                 in the [handoff] section of config.toml"
            ),
            other => bail!("unknown handoff carrier `{other}`: use \"git\""),
        }
    }
}

/// The ref that carries a session's working tree.
pub fn tree_ref(id: SessionId) -> String {
    format!("refs/heads/modron/handoff/{id}")
}

/// The ref that carries a session's bundle.
pub fn bundle_ref(id: SessionId) -> String {
    format!("{}-session", tree_ref(id))
}

fn bundle_name(id: SessionId) -> String {
    format!("{id}.handoff.tar")
}

/// The only store files a bundle may hold (allow-list; the store directory is
/// never globbed). Credentials and env files live elsewhere and never match.
pub fn bundle_members(id: SessionId) -> [String; 3] {
    [
        format!("{id}.jsonl"),
        format!("{id}.meta.json"),
        format!("{id}.rewind"),
    ]
}

/// Accepts a session id or a handoff ref (`refs/heads/modron/handoff/<id>`,
/// with or without the `-session` suffix) and returns the session id.
pub fn parse_reference(arg: &str) -> Result<SessionId> {
    let s = arg.trim();
    let s = s.strip_prefix("refs/heads/").unwrap_or(s);
    let s = s.strip_prefix("modron/handoff/").unwrap_or(s);
    let s = s.strip_suffix("-session").unwrap_or(s);
    SessionId::parse_str(s)
        .with_context(|| format!("`{arg}` is neither a session id nor a handoff ref"))
}

/// Removes a path when dropped (temporary index files and scratch dirs).
struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = if self.0.is_dir() {
            std::fs::remove_dir_all(&self.0)
        } else {
            std::fs::remove_file(&self.0)
        };
    }
}

/// Runs `git -C dir args` with extra environment; returns stdout, or an error
/// that carries stderr.
fn git_env(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .envs(envs.iter().copied())
        .output()
        .with_context(|| format!("could not run git {}", args.first().unwrap_or(&"")))?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    git_env(dir, args, &[]).map(|out| String::from_utf8_lossy(&out).trim().to_string())
}

/// `Some(stdout)` when the command succeeds, `None` when it fails.
fn git_try(dir: &Path, args: &[&str]) -> Option<String> {
    git(dir, args).ok()
}

fn has_remote(dir: &Path, remote: &str) -> bool {
    git_try(dir, &["remote", "get-url", remote]).is_some()
}

fn scratch_dir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "modron-handoff-{label}-{}-{nanos}",
        std::process::id()
    ))
}

/// What `export` did, for the command's report.
#[derive(Debug, Clone)]
pub struct ExportReport {
    pub session: SessionId,
    pub tree_ref: String,
    pub bundle_ref: String,
    pub files: usize,
    pub ignored_left_behind: usize,
    pub pushed_to: Option<String>,
}

/// Exports `session` from the repository at `repo`. `remote` is pushed to
/// when `Some`. The user's index, HEAD and branch are not changed.
pub async fn export(
    carrier: Carrier,
    store_root: &Path,
    repo: &Path,
    session: Option<SessionId>,
    remote: Option<&str>,
) -> Result<ExportReport> {
    let Carrier::Git = carrier;
    let store = JsonlSessionStore::new(store_root);
    let id = match session {
        Some(id) => id,
        None => store
            .list()
            .await?
            .first()
            .map(|meta| meta.id)
            .context("no sessions to export")?,
    };
    let events = store
        .load(id)
        .await
        .with_context(|| format!("cannot load session {id}"))?;
    if events.is_empty() {
        bail!("session {id} has no events — nothing to hand off");
    }
    if let Some(carrier_ref) = handed_off_ref(&events) {
        bail!("session {id} is already handed off (ref {carrier_ref}); import it back first");
    }

    let top = PathBuf::from(git(repo, &["rev-parse", "--show-toplevel"])?);
    let gitdir = PathBuf::from(git(repo, &["rev-parse", "--absolute-git-dir"])?);
    let head = git_try(&top, &["rev-parse", "-q", "--verify", "HEAD^{commit}"]);

    // Tree commit from a temporary index: the user's index stays as it is.
    let index = Cleanup(gitdir.join(format!("modron-handoff-{id}.index")));
    let index_str = index.0.to_string_lossy().to_string();
    let envs = [("GIT_INDEX_FILE", index_str.as_str())];
    match &head {
        Some(head) => git_env(&top, &["read-tree", head], &envs)?,
        None => git_env(&top, &["read-tree", "--empty"], &envs)?,
    };
    git_env(&top, &["add", "-A"], &envs)?;
    let tree = String::from_utf8_lossy(&git_env(&top, &["write-tree"], &envs)?)
        .trim()
        .to_string();
    let files = git(&top, &["ls-tree", "-r", "--name-only", &tree])?
        .lines()
        .count();
    let message = format!("modron handoff: working tree of session {id}");
    let mut commit_args = vec!["commit-tree", tree.as_str(), "-m", message.as_str()];
    if let Some(head) = head.as_deref() {
        commit_args.extend(["-p", head]);
    }
    let author = [
        ("GIT_AUTHOR_NAME", "modron"),
        ("GIT_AUTHOR_EMAIL", "handoff@modron.invalid"),
        ("GIT_COMMITTER_NAME", "modron"),
        ("GIT_COMMITTER_EMAIL", "handoff@modron.invalid"),
    ];
    let tree_commit = String::from_utf8_lossy(&git_env(&top, &commit_args, &author)?)
        .trim()
        .to_string();
    drop(index);

    // Bundle: allow-listed store files as a tar, stored as one blob.
    let scratch = Cleanup(scratch_dir(&id.to_string()));
    std::fs::create_dir_all(&scratch.0)?;
    let tar_path = scratch.0.join(bundle_name(id));
    let mut tar = Command::new("tar");
    tar.arg("-cf").arg(&tar_path).arg("-C").arg(store_root);
    for name in bundle_members(id) {
        if store_root.join(&name).is_file() {
            tar.arg(name);
        }
    }
    let status = tar.status().context("could not run tar")?;
    if !status.success() {
        bail!("tar failed while bundling session {id}");
    }
    let blob = git(&top, &["hash-object", "-w", &tar_path.to_string_lossy()])?;
    let bundle_index = Cleanup(gitdir.join(format!("modron-handoff-{id}-bundle.index")));
    let bundle_index_str = bundle_index.0.to_string_lossy().to_string();
    let bundle_envs = [("GIT_INDEX_FILE", bundle_index_str.as_str())];
    git_env(&top, &["read-tree", "--empty"], &bundle_envs)?;
    let cacheinfo = format!("100644,{blob},{}", bundle_name(id));
    git_env(
        &top,
        &["update-index", "--add", "--cacheinfo", &cacheinfo],
        &bundle_envs,
    )?;
    let bundle_tree = String::from_utf8_lossy(&git_env(&top, &["write-tree"], &bundle_envs)?)
        .trim()
        .to_string();
    drop(bundle_index);
    let bundle_message = format!("modron handoff: session bundle {id}");
    let bundle_commit = String::from_utf8_lossy(&git_env(
        &top,
        &[
            "commit-tree",
            bundle_tree.as_str(),
            "-m",
            bundle_message.as_str(),
        ],
        &author,
    )?)
    .trim()
    .to_string();

    let tree_ref = tree_ref(id);
    let bundle_ref = bundle_ref(id);
    git(&top, &["update-ref", &tree_ref, &tree_commit])?;
    git(&top, &["update-ref", &bundle_ref, &bundle_commit])?;

    let ignored_left_behind = git(&top, &["status", "--porcelain", "--ignored"])?
        .lines()
        .filter(|line| line.starts_with("!! "))
        .count();

    // Push before the log is marked: a failed push leaves the session usable.
    let pushed_to = match remote {
        Some(remote) => {
            if !has_remote(&top, remote) {
                bail!(
                    "no git remote `{remote}` in this repository; pass --no-push \
                     to keep the handoff local"
                );
            }
            // Forced: each handoff replaces the previous one on these refs.
            // The log prefix rule, not the ref history, decides ownership.
            git(
                &top,
                &[
                    "push",
                    "--quiet",
                    remote,
                    &format!("+{tree_ref}"),
                    &format!("+{bundle_ref}"),
                ],
            )?;
            Some(remote.to_string())
        }
        None => None,
    };

    store
        .append(
            id,
            &SessionEvent::HandedOff {
                to: None,
                at: Utc::now(),
                carrier_ref: tree_ref.clone(),
            },
        )
        .await?;

    Ok(ExportReport {
        session: id,
        tree_ref,
        bundle_ref,
        files,
        ignored_left_behind,
        pushed_to,
    })
}

/// The prefix rule (C6c). Pure, so it is unit-tested directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogDecision {
    /// Replace the local log with the incoming one.
    Write,
    /// Leave the local log as it is (identical, or the same handed-off state).
    Keep,
    /// Refuse, with the reason to show.
    Refuse(String),
}

fn log_lines(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .collect()
}

fn is_handoff_line(line: &str) -> bool {
    matches!(
        serde_json::from_str::<SessionEvent>(line),
        Ok(SessionEvent::HandedOff { .. })
    )
}

/// Number of trailing HandedOff lines.
fn trailing_handoffs(lines: &[&str]) -> usize {
    lines
        .iter()
        .rev()
        .take_while(|line| is_handoff_line(line))
        .count()
}

/// Decides what an import does to the session log. The local log, minus any
/// trailing HandedOff line, must be identical to or a strict prefix of the
/// incoming log; a handed-off local log is cleared by an incoming log that
/// continues past its HandedOff line. Anything else is refused.
pub fn decide_log(local: Option<&str>, incoming: &str) -> LogDecision {
    let Some(local) = local else {
        return LogDecision::Write;
    };
    let local_lines = log_lines(local);
    let incoming_lines = log_lines(incoming);
    let local_ho = trailing_handoffs(&local_lines);
    let incoming_ho = trailing_handoffs(&incoming_lines);
    let base = &local_lines[..local_lines.len() - local_ho];
    let incoming_base = &incoming_lines[..incoming_lines.len() - incoming_ho];
    let diverged = || {
        LogDecision::Refuse(format!(
            "session log diverged: local has {} events, incoming has {}; neither is a \
             prefix of the other, so nothing is merged (C6c prefix rule)",
            local_lines.len(),
            incoming_lines.len()
        ))
    };
    if local_ho == 0 {
        if local_lines == incoming_lines {
            LogDecision::Keep
        } else if incoming_lines.len() > local_lines.len()
            && incoming_lines.starts_with(&local_lines)
        {
            LogDecision::Write
        } else {
            diverged()
        }
    } else if incoming_base == base {
        LogDecision::Keep
    } else if incoming_base.len() > base.len() && incoming_base.starts_with(base) {
        LogDecision::Write
    } else {
        diverged()
    }
}

/// What `import` did, for the command's report.
#[derive(Debug, Clone)]
pub struct ImportReport {
    pub session: SessionId,
    pub files_written: usize,
    pub files_removed: usize,
    pub events: usize,
    pub still_handed_off: bool,
}

/// Imports the handoff `reference` (a handoff ref or a session id) into the
/// repository at `repo` and the store at `store_root`. Refuses before changing
/// anything when the log decision refuses, or when the working tree is dirty
/// and `force` is not set.
pub async fn import(
    store_root: &Path,
    repo: &Path,
    reference: &str,
    remote: Option<&str>,
    force: bool,
) -> Result<ImportReport> {
    let id = parse_reference(reference)?;
    let tree_ref = tree_ref(id);
    let bundle_ref = bundle_ref(id);
    let top = PathBuf::from(git(repo, &["rev-parse", "--show-toplevel"])?);
    let gitdir = PathBuf::from(git(repo, &["rev-parse", "--absolute-git-dir"])?);

    if let Some(remote) = remote.filter(|remote| has_remote(&top, remote)) {
        git(
            &top,
            &[
                "fetch",
                "--quiet",
                remote,
                &format!("+{tree_ref}:{tree_ref}"),
                &format!("+{bundle_ref}:{bundle_ref}"),
            ],
        )
        .with_context(|| format!("could not fetch the handoff from `{remote}`"))?;
    }
    let tree_commit = git(
        &top,
        &[
            "rev-parse",
            "--verify",
            "-q",
            &format!("{tree_ref}^{{commit}}"),
        ],
    )
    .with_context(|| format!("no handoff ref {tree_ref} here (fetch it or pass --remote)"))?;
    let bundle_commit = git(&top, &["rev-parse", "--verify", "-q", &bundle_ref])
        .with_context(|| format!("no bundle ref {bundle_ref} here"))?;

    // Unpack the bundle; only the allow-listed names are accepted.
    let scratch = Cleanup(scratch_dir(&format!("import-{id}")));
    std::fs::create_dir_all(&scratch.0)?;
    let tar_path = scratch.0.join(bundle_name(id));
    let tar_bytes = git_env(
        &top,
        &[
            "cat-file",
            "blob",
            &format!("{bundle_commit}:{}", bundle_name(id)),
        ],
        &[],
    )?;
    std::fs::write(&tar_path, tar_bytes)?;
    let listed = Command::new("tar")
        .arg("-tf")
        .arg(&tar_path)
        .output()
        .context("could not run tar")?;
    if !listed.status.success() {
        bail!("the session bundle in {bundle_ref} is not a readable tar");
    }
    let names: BTreeSet<String> = String::from_utf8_lossy(&listed.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    let allowed: BTreeSet<String> = bundle_members(id).into_iter().collect();
    if !names.is_subset(&allowed) || !names.contains(&format!("{id}.jsonl")) {
        bail!("the session bundle holds unexpected entries: {names:?}");
    }
    let extract = scratch.0.join("bundle");
    std::fs::create_dir_all(&extract)?;
    let status = Command::new("tar")
        .arg("-xf")
        .arg(&tar_path)
        .arg("-C")
        .arg(&extract)
        .status()
        .context("could not run tar")?;
    if !status.success() {
        bail!("tar failed while unpacking the session bundle");
    }
    let incoming = std::fs::read_to_string(extract.join(format!("{id}.jsonl")))
        .context("bundle has no session log")?;
    let incoming_rewind = std::fs::read_to_string(extract.join(format!("{id}.rewind"))).ok();

    // Decide the log before touching any file.
    let store = JsonlSessionStore::new(store_root);
    let local_path = store.transcript_path(id);
    let local = std::fs::read_to_string(&local_path).ok();
    let log = decide_log(local.as_deref(), &incoming);
    if let LogDecision::Refuse(reason) = &log {
        bail!("{reason}");
    }

    let dirty = git(&top, &["status", "--porcelain"])?;
    if !dirty.is_empty() && !force {
        bail!("the working tree has uncommitted changes; commit or stash them, or pass --force");
    }

    // Files: what the handoff tree has, and what this checkout tracks now.
    let target: BTreeSet<String> =
        git(&top, &["ls-tree", "-r", "-z", "--name-only", &tree_commit])?
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect();
    let tracked: BTreeSet<String> = git(&top, &["ls-files", "-z"])?
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect();
    if !force {
        for path in &target {
            if !tracked.contains(path) && top.join(path).symlink_metadata().is_ok() {
                bail!("{path} exists here (ignored or untracked) and would be overwritten; pass --force");
            }
        }
    }
    let index = Cleanup(gitdir.join(format!("modron-import-{id}.index")));
    let index_str = index.0.to_string_lossy().to_string();
    let envs = [("GIT_INDEX_FILE", index_str.as_str())];
    git_env(&top, &["read-tree", &tree_commit], &envs)?;
    git_env(&top, &["checkout-index", "-a", "-f"], &envs)?;
    drop(index);
    let mut files_removed = 0;
    for path in tracked.difference(&target) {
        let full = top.join(path);
        if std::fs::symlink_metadata(&full).is_ok() {
            std::fs::remove_file(&full)
                .with_context(|| format!("could not remove {}", full.display()))?;
            files_removed += 1;
        }
    }

    if log == LogDecision::Write {
        store
            .replace_transcript(id, &incoming, incoming_title(&extract, id).as_deref())
            .map_err(anyhow::Error::from)?;
        if let Some(rewind) = incoming_rewind {
            std::fs::write(store_root.join(format!("{id}.rewind")), rewind)?;
        }
    }
    let events = store.load(id).await?;
    Ok(ImportReport {
        session: id,
        files_written: target.len(),
        files_removed,
        events: events.len(),
        still_handed_off: handed_off_ref(&events).is_some(),
    })
}

fn incoming_title(extract: &Path, id: SessionId) -> Option<String> {
    let text = std::fs::read_to_string(extract.join(format!("{id}.meta.json"))).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value.get("title")?.as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn git_ok(dir: &Path, args: &[&str]) -> String {
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
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn event_line(event: &SessionEvent) -> String {
        serde_json::to_string(event).unwrap()
    }

    fn handed_off_line() -> String {
        event_line(&SessionEvent::HandedOff {
            to: None,
            at: Utc::now(),
            carrier_ref: "refs/heads/modron/handoff/x".into(),
        })
    }

    fn task_line(text: &str) -> String {
        event_line(&SessionEvent::Task { text: text.into() })
    }

    /// Walks the non-ignored files of `dir` into a comparable map: content and
    /// executable bit for files, the target for symlinks.
    fn snapshot(dir: &Path) -> BTreeMap<String, String> {
        let listed = git_ok(
            dir,
            &["ls-files", "--cached", "--others", "--exclude-standard"],
        );
        let mut map = BTreeMap::new();
        for path in listed.lines().filter(|l| !l.is_empty()) {
            let full = dir.join(path);
            // A tracked file deleted in the working tree is still listed.
            let Ok(meta) = full.symlink_metadata() else {
                continue;
            };
            let entry = if meta.file_type().is_symlink() {
                format!("link {}", std::fs::read_link(&full).unwrap().display())
            } else {
                use std::os::unix::fs::PermissionsExt;
                let exec = meta.permissions().mode() & 0o111 != 0;
                format!(
                    "file exec={exec} {}",
                    std::fs::read_to_string(&full).unwrap()
                )
            };
            map.insert(path.to_string(), entry);
        }
        map
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        origin: PathBuf,
        repo_a: PathBuf,
        repo_b: PathBuf,
        store_a: PathBuf,
        store_b: PathBuf,
    }

    /// A bare origin, clone A with a tracked README, a tracked file that is
    /// later deleted, and a .gitignore; clone B at the same commit.
    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let origin = root.join("origin.git");
        let repo_a = root.join("a");
        let repo_b = root.join("b");
        let store_a = root.join("sessions-a");
        let store_b = root.join("sessions-b");
        git_ok(
            &root,
            &[
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                origin.to_str().unwrap(),
            ],
        );
        git_ok(
            &root,
            &["init", "-q", "-b", "main", repo_a.to_str().unwrap()],
        );
        std::fs::write(repo_a.join("README.md"), "hello\n").unwrap();
        std::fs::write(repo_a.join("obsolete.txt"), "old\n").unwrap();
        std::fs::write(repo_a.join(".gitignore"), "target/\n*.log\n").unwrap();
        git_ok(&repo_a, &["add", "-A"]);
        git_ok(&repo_a, &["commit", "-q", "-m", "base"]);
        git_ok(
            &repo_a,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        git_ok(&repo_a, &["push", "-q", "origin", "main"]);
        git_ok(
            &root,
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                repo_b.to_str().unwrap(),
            ],
        );
        Fixture {
            _tmp: tmp,
            origin,
            repo_a,
            repo_b,
            store_a,
            store_b,
        }
    }

    async fn seed_session(store_root: &Path, texts: &[&str]) -> SessionId {
        let store = JsonlSessionStore::new(store_root);
        let meta = store.create_session("round trip").await.unwrap();
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

    #[tokio::test]
    async fn round_trip_restores_tree_and_session_and_leaves_ignored_behind() {
        let fx = fixture();
        let a = &fx.repo_a;
        std::fs::write(a.join("README.md"), "hello, changed\n").unwrap();
        std::fs::create_dir_all(a.join("notes")).unwrap();
        std::fs::write(a.join("notes/new.txt"), "untracked\n").unwrap();
        std::fs::create_dir_all(a.join("scripts")).unwrap();
        std::fs::write(a.join("scripts/run.sh"), "#!/bin/sh\necho hi\n").unwrap();
        std::fs::set_permissions(
            a.join("scripts/run.sh"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        std::os::unix::fs::symlink("README.md", a.join("link.md")).unwrap();
        std::fs::remove_file(a.join("obsolete.txt")).unwrap();
        std::fs::create_dir_all(a.join("target")).unwrap();
        std::fs::write(a.join("target/build.bin"), "ignored\n").unwrap();
        std::fs::write(a.join("debug.log"), "ignored\n").unwrap();

        let id = seed_session(&fx.store_a, &["fix the readme"]).await;
        let before = (
            git_ok(a, &["status", "--porcelain"]),
            git_ok(a, &["rev-parse", "HEAD"]),
            git_ok(a, &["symbolic-ref", "HEAD"]),
            git_ok(a, &["ls-files", "-s"]),
        );

        let report = export(Carrier::Git, &fx.store_a, a, Some(id), Some("origin"))
            .await
            .unwrap();
        assert_eq!(report.ignored_left_behind, 2, "target/ and debug.log");
        assert_eq!(report.pushed_to.as_deref(), Some("origin"));

        let after = (
            git_ok(a, &["status", "--porcelain"]),
            git_ok(a, &["rev-parse", "HEAD"]),
            git_ok(a, &["symbolic-ref", "HEAD"]),
            git_ok(a, &["ls-files", "-s"]),
        );
        assert_eq!(
            before, after,
            "export must not move the index, HEAD or branch"
        );

        let origin_refs = git_ok(&fx.origin, &["for-each-ref", "--format=%(refname)"]);
        assert!(origin_refs.contains(&tree_ref(id)));
        assert!(origin_refs.contains(&bundle_ref(id)));

        // Host B imports the handoff; its branch stays at the base commit.
        let b = &fx.repo_b;
        let import_report = import(&fx.store_b, b, &tree_ref(id), Some("origin"), false)
            .await
            .unwrap();
        assert!(!import_report.still_handed_off);
        assert_eq!(git_ok(b, &["symbolic-ref", "HEAD"]), "refs/heads/main");

        assert_eq!(snapshot(a), snapshot(b), "working trees must match");
        assert!(
            !b.join("target/build.bin").exists(),
            "ignored files do not travel"
        );
        assert!(!b.join("debug.log").exists());
        assert!(!b.join("obsolete.txt").exists(), "deleted file is deleted");
        let symlink = std::fs::read_link(b.join("link.md")).unwrap();
        assert_eq!(symlink, PathBuf::from("README.md"));
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(b.join("scripts/run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert!(mode & 0o111 != 0, "executable bit survives");

        let store_b = JsonlSessionStore::new(&fx.store_b);
        let events = store_b.load(id).await.unwrap();
        assert_eq!(
            events.len(),
            1,
            "the log travels without the HandedOff line"
        );
        assert!(handed_off_ref(&events).is_none());
        // The exporting host now holds the handoff as its last event.
        let local = store_b_load(&fx.store_a, id).await;
        assert!(handed_off_ref(&local).is_some());
    }

    async fn store_b_load(root: &Path, id: SessionId) -> Vec<SessionEvent> {
        JsonlSessionStore::new(root)
            .load(id)
            .await
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn a_longer_log_brought_back_clears_the_handoff() {
        let fx = fixture();
        let id = seed_session(&fx.store_a, &["first"]).await;
        export(
            Carrier::Git,
            &fx.store_a,
            &fx.repo_a,
            Some(id),
            Some("origin"),
        )
        .await
        .unwrap();
        // B takes it over, continues the session, and hands it back.
        import(
            &fx.store_b,
            &fx.repo_b,
            &id.to_string(),
            Some("origin"),
            false,
        )
        .await
        .unwrap();
        let store_b = JsonlSessionStore::new(&fx.store_b);
        store_b
            .append(
                id,
                &SessionEvent::Task {
                    text: "second".into(),
                },
            )
            .await
            .unwrap();
        export(
            Carrier::Git,
            &fx.store_b,
            &fx.repo_b,
            Some(id),
            Some("origin"),
        )
        .await
        .unwrap();
        assert!(handed_off_ref(&store_b_load(&fx.store_a, id).await).is_some());

        // A imports the longer log: its HandedOff line is replaced, so the
        // session runs here again.
        let report = import(
            &fx.store_a,
            &fx.repo_a,
            &id.to_string(),
            Some("origin"),
            false,
        )
        .await
        .unwrap();
        assert!(!report.still_handed_off);
        let events = store_b_load(&fx.store_a, id).await;
        assert_eq!(events.len(), 2);
        assert!(handed_off_ref(&events).is_none());
    }

    #[tokio::test]
    async fn export_refuses_a_session_already_handed_off() {
        let fx = fixture();
        let id = seed_session(&fx.store_a, &["task"]).await;
        export(Carrier::Git, &fx.store_a, &fx.repo_a, Some(id), None)
            .await
            .unwrap();
        let err = export(Carrier::Git, &fx.store_a, &fx.repo_a, Some(id), None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already handed off"), "{err}");
    }

    #[tokio::test]
    async fn export_without_the_named_remote_refuses_before_marking() {
        let fx = fixture();
        let id = seed_session(&fx.store_a, &["task"]).await;
        let err = export(
            Carrier::Git,
            &fx.store_a,
            &fx.repo_a,
            Some(id),
            Some("nowhere"),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("--no-push"), "{err}");
        let events = store_b_load(&fx.store_a, id).await;
        assert!(handed_off_ref(&events).is_none());
    }

    #[tokio::test]
    async fn dirty_tree_is_refused_until_forced_and_then_the_log_is_restored() {
        let fx = fixture();
        let id = seed_session(&fx.store_a, &["task"]).await;
        std::fs::write(fx.repo_a.join("README.md"), "from A\n").unwrap();
        export(
            Carrier::Git,
            &fx.store_a,
            &fx.repo_a,
            Some(id),
            Some("origin"),
        )
        .await
        .unwrap();

        std::fs::write(fx.repo_b.join("README.md"), "local B edit\n").unwrap();
        let err = import(
            &fx.store_b,
            &fx.repo_b,
            &id.to_string(),
            Some("origin"),
            false,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("uncommitted changes"), "{err}");
        assert_eq!(
            std::fs::read_to_string(fx.repo_b.join("README.md")).unwrap(),
            "local B edit\n"
        );
        assert!(
            store_b_load(&fx.store_b, id).await.is_empty(),
            "nothing written on refusal"
        );

        import(
            &fx.store_b,
            &fx.repo_b,
            &id.to_string(),
            Some("origin"),
            true,
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(fx.repo_b.join("README.md")).unwrap(),
            "from A\n"
        );
        assert_eq!(store_b_load(&fx.store_b, id).await.len(), 1);
    }

    #[tokio::test]
    async fn bundle_holds_only_allow_listed_store_files() {
        let fx = fixture();
        let id = seed_session(&fx.store_a, &["task"]).await;
        std::fs::write(fx.store_a.join("stray.txt"), "not ours\n").unwrap();
        std::fs::write(fx.store_a.join(".env"), "SECRET=1\n").unwrap();
        std::fs::write(fx.store_a.join(format!("{id}.rewind")), "0 1").unwrap();
        export(Carrier::Git, &fx.store_a, &fx.repo_a, Some(id), None)
            .await
            .unwrap();

        let scratch = tempfile::tempdir().unwrap();
        let tar = scratch.path().join("bundle.tar");
        let blob = Command::new("git")
            .arg("-C")
            .arg(&fx.repo_a)
            .args(["show", &format!("{}:{}.handoff.tar", bundle_ref(id), id)])
            .output()
            .unwrap();
        std::fs::write(&tar, blob.stdout).unwrap();
        let listed = Command::new("tar").arg("-tf").arg(&tar).output().unwrap();
        let names: BTreeSet<String> = String::from_utf8_lossy(&listed.stdout)
            .lines()
            .map(str::to_string)
            .collect();
        let expected: BTreeSet<String> = [
            format!("{id}.jsonl"),
            format!("{id}.meta.json"),
            format!("{id}.rewind"),
        ]
        .into_iter()
        .collect();
        assert_eq!(names, expected);
    }

    #[test]
    fn furrow_carrier_says_not_built_yet() {
        let err = Carrier::from_config(Some("furrow")).unwrap_err();
        assert!(err.to_string().contains("not built yet"), "{err}");
        assert_eq!(Carrier::from_config(None).unwrap(), Carrier::Git);
        assert_eq!(Carrier::from_config(Some("git")).unwrap(), Carrier::Git);
        assert!(Carrier::from_config(Some("ftp")).is_err());
    }

    #[test]
    fn prefix_rule_decisions() {
        let (a, b) = (task_line("a"), task_line("b"));
        let c = task_line("c");
        let h = handed_off_line();
        let local = format!("{a}\n{b}\n");
        assert_eq!(decide_log(None, &local), LogDecision::Write);
        assert_eq!(decide_log(Some(&local), &local), LogDecision::Keep);
        assert_eq!(
            decide_log(Some(&format!("{a}\n")), &local),
            LogDecision::Write
        );
        match decide_log(Some(&format!("{a}\n{c}\n")), &format!("{a}\n{b}\n{c}\n")) {
            LogDecision::Refuse(msg) => {
                assert!(msg.contains("local has 2 events, incoming has 3"), "{msg}")
            }
            other => panic!("diverged logs must refuse, got {other:?}"),
        }
        assert!(matches!(
            decide_log(Some(&local), &format!("{a}\n")),
            LogDecision::Refuse(_)
        ));
        // A handed-off local log: the same base keeps the handoff, a longer
        // continuation clears it, a diverged one is refused.
        let handed = format!("{a}\n{h}\n");
        assert_eq!(
            decide_log(Some(&handed), &format!("{a}\n")),
            LogDecision::Keep
        );
        assert_eq!(
            decide_log(Some(&handed), &format!("{a}\n{b}\n")),
            LogDecision::Write
        );
        assert!(matches!(
            decide_log(Some(&handed), &format!("{c}\n{b}\n")),
            LogDecision::Refuse(_)
        ));
    }
}
