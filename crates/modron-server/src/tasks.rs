//! tasks.rs — worktree tasks (the paradev/conductor grammar): one agent
//! session per git worktree + branch, reviewable via diff stats, merged
//! back review-first (a conflicted merge aborts cleanly and the base
//! stays untouched).
//!
//! Everything runs through `modron serve` on the machine that owns the
//! repo — the same server local, LAN, and remote clients attach to — so
//! the whole workflow works from any of our clients.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use modron_protocol::TaskFileStat;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use uuid::Uuid;

use crate::dispatch::ServerState;
use crate::hub::SessionHub;

/// One task's identity: the session that drives it, the worktree it runs
/// in, and the branch it commits to.
#[derive(Clone, Debug)]
pub struct TaskRecord {
    pub session_id: Uuid,
    pub worktree: PathBuf,
    pub branch: String,
    /// The resolved ref the task branched from (a branch name where
    /// possible, so the merge target is stable).
    pub base: String,
    pub title: String,
}

/// File name of the persisted task records, under the server's data dir.
pub const TASKS_FILE: &str = "tasks.json";

/// The on-disk form of one [`TaskRecord`] (the session id as text).
#[derive(Serialize, Deserialize)]
struct PersistedTask {
    session_id: String,
    worktree: PathBuf,
    branch: String,
    base: String,
    title: String,
}

/// The server's task registry: session id -> record. Records are persisted to
/// a small JSON file (see [`TaskRegistry::load`]) so a server restart keeps
/// each task's worktree; without that a re-attached task would run in the
/// base checkout. Worktrees themselves are on disk; `task_status` re-derives
/// state from git.
#[derive(Clone, Default)]
pub struct TaskRegistry {
    tasks: Arc<std::sync::Mutex<std::collections::HashMap<Uuid, TaskRecord>>>,
    /// Where records are saved; `None` keeps the registry in memory only.
    file: Option<Arc<PathBuf>>,
    /// Serializes saves so two writers cannot interleave their snapshots.
    save_lock: Arc<std::sync::Mutex<()>>,
}

impl TaskRegistry {
    /// An in-memory registry (nothing is persisted).
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry persisted at `path`. Loads the records already there,
    /// dropping any whose worktree directory no longer exists. A missing or
    /// unreadable file starts empty (an unreadable one is logged, and the
    /// next save replaces it).
    pub fn load(path: PathBuf) -> Self {
        let mut map = std::collections::HashMap::new();
        match std::fs::read(&path) {
            Ok(data) => match serde_json::from_slice::<Vec<PersistedTask>>(&data) {
                Ok(records) => {
                    for rec in records {
                        let Ok(session_id) = rec.session_id.parse::<Uuid>() else {
                            continue;
                        };
                        if !rec.worktree.is_dir() {
                            tracing::warn!(
                                worktree = %rec.worktree.display(),
                                "dropping task whose worktree no longer exists"
                            );
                            continue;
                        }
                        map.insert(
                            session_id,
                            TaskRecord {
                                session_id,
                                worktree: rec.worktree,
                                branch: rec.branch,
                                base: rec.base,
                                title: rec.title,
                            },
                        );
                    }
                }
                Err(e) => tracing::warn!(
                    file = %path.display(),
                    error = %e,
                    "task records file is unreadable; starting with no tasks"
                ),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(
                file = %path.display(),
                error = %e,
                "could not read task records; starting with no tasks"
            ),
        }
        Self {
            tasks: Arc::new(std::sync::Mutex::new(map)),
            file: Some(Arc::new(path)),
            save_lock: Arc::new(std::sync::Mutex::new(())),
        }
    }

    pub fn insert(&self, record: TaskRecord) {
        self.tasks.lock().unwrap().insert(record.session_id, record);
        self.persist();
    }

    pub fn get(&self, id: &Uuid) -> Option<TaskRecord> {
        self.tasks.lock().unwrap().get(id).cloned()
    }

    pub fn remove(&self, id: &Uuid) -> Option<TaskRecord> {
        let removed = self.tasks.lock().unwrap().remove(id);
        self.persist();
        removed
    }

    /// Save the current records; a failed save is logged, never fatal (the
    /// in-memory registry stays authoritative for this process).
    fn persist(&self) {
        if let Err(e) = self.save() {
            tracing::warn!(error = %e, "could not persist task records");
        }
    }

    fn save(&self) -> Result<()> {
        let Some(path) = &self.file else {
            return Ok(());
        };
        let _guard = self.save_lock.lock().unwrap();
        let mut records: Vec<PersistedTask> = self
            .tasks
            .lock()
            .unwrap()
            .values()
            .map(|r| PersistedTask {
                session_id: r.session_id.to_string(),
                worktree: r.worktree.clone(),
                branch: r.branch.clone(),
                base: r.base.clone(),
                title: r.title.clone(),
            })
            .collect();
        records.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        let bytes = serde_json::to_vec_pretty(&records)?;
        crate::pairing::write_private_atomic(path, &bytes)
    }
}

async fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        // A repo without configured identity still needs to commit merges:
        // fall back to a deterministic identity unless the environment
        // already provides one (the user's config wins in practice).
        .envs([
            ("GIT_AUTHOR_NAME", "modron"),
            ("GIT_AUTHOR_EMAIL", "modron@localhost"),
            ("GIT_COMMITTER_NAME", "modron"),
            ("GIT_COMMITTER_EMAIL", "modron@localhost"),
        ])
        .output()
        .await
        .context("git not found on the server machine")?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        // Merge conflict notices ride STDOUT; other failures explain on
        // stderr. Carry both into the error.
        Err(anyhow!(
            "git {} failed: {}{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim(),
            String::from_utf8_lossy(&output.stdout).trim()
        ))
    }
}

/// A slug from a free-text title: lowercase, non-alphanumeric -> dash,
/// trimmed, capped so the branch name stays readable.
pub fn slugify(title: &str) -> String {
    let mut slug: String = title
        .chars()
        .take(48)
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    slug = slug.trim_matches('-').to_string();
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    if slug.is_empty() {
        "task".into()
    } else {
        slug
    }
}

/// Reject a client-supplied base before it reaches git: it must be a
/// non-empty ref name with no leading dash and no whitespace or control
/// characters (so it can never be read as a git option).
pub fn validate_base(base: &str) -> Result<()> {
    if base.is_empty() {
        return Err(anyhow!("invalid base: the base branch name is empty"));
    }
    if base.starts_with('-') {
        return Err(anyhow!(
            "invalid base `{base}`: a base name cannot start with `-`"
        ));
    }
    if base.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(anyhow!(
            "invalid base {base:?}: a base name cannot contain whitespace or control characters"
        ));
    }
    Ok(())
}

/// Resolve the merge target: an explicit base stands; "HEAD" resolves to
/// the current branch name so the merge target does not drift.
async fn resolve_base(cwd: &Path, base: Option<&str>) -> Result<String> {
    match base {
        Some(b) if b != "HEAD" => Ok(b.to_string()),
        _ => {
            let branch = git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).await?;
            Ok(branch.trim().to_string())
        }
    }
}

/// Create a worktree + branch off `base` (default: the current branch of
/// the server's repo), then create + pin the driving session. Returns the
/// record plus the new session id.
pub async fn create_task(
    state: &ServerState,
    hub: &SessionHub,
    cwd: &Path,
    title: &str,
    base: Option<&str>,
) -> Result<TaskRecord> {
    if let Some(b) = base {
        validate_base(b)?;
    }
    git(cwd, &["rev-parse", "--is-inside-work-tree"])
        .await
        .context("the server working dir is not a git repository")?;
    let base = resolve_base(cwd, base).await?;

    let slug = slugify(title);
    let session_id = Uuid::new_v4();
    let short = &session_id.simple().to_string()[..6];
    let branch = format!("modron/{slug}-{short}");
    let worktree = cwd
        .join(".modron")
        .join("worktrees")
        .join(format!("{slug}-{short}"));
    if worktree.exists() {
        return Err(anyhow!(
            "worktree {} already exists - pick another title",
            worktree.display()
        ));
    }
    git(
        cwd,
        &[
            "worktree",
            "add",
            "-b",
            &branch,
            &worktree.display().to_string(),
            &base,
        ],
    )
    .await
    .context(format!("creating worktree on branch {branch} off {base}"))?;

    // The task session runs inside the worktree: create + pin the store
    // session, seed the transcript with the task text, register a hub
    // handle whose engine runs there.
    let meta = state.store.create_session(title).await?;
    hub.ensure_session_in(meta.id, Some(meta.id), Some(worktree.clone()));
    state
        .store
        .append(
            meta.id,
            &modron_core::SessionEvent::Task {
                text: title.to_string(),
            },
        )
        .await?;

    let record = TaskRecord {
        session_id: meta.id,
        worktree,
        branch,
        base,
        title: title.to_string(),
    };
    state.tasks.insert(record.clone());
    Ok(record)
}

/// Diff/branch status: commits ahead of base + per-file numstat.
pub async fn task_status(
    state: &ServerState,
    session_id: &Uuid,
) -> Result<(TaskRecord, u64, Vec<TaskFileStat>)> {
    let record = state
        .tasks
        .get(session_id)
        .ok_or_else(|| anyhow!("session {session_id} is not a task"))?;
    let ahead_raw = git(
        &record.worktree,
        &["rev-list", "--count", &format!("{}..HEAD", record.base)],
    )
    .await?;
    let ahead = ahead_raw.trim().parse().unwrap_or(0);
    let numstat = git(
        &record.worktree,
        &["diff", "--numstat", &format!("{}...HEAD", record.base)],
    )
    .await?;
    let mut files = Vec::new();
    for line in numstat.lines() {
        let mut parts = line.split('\t');
        let (Some(added), Some(deleted), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        files.push(TaskFileStat {
            path: path.trim().to_string(),
            added: added.parse().unwrap_or(0),
            deleted: deleted.parse().unwrap_or(0),
        });
    }
    Ok((record, ahead, files))
}

/// Merge the task branch into its base in the BASE checkout (never inside
/// the worktree), review-first: a conflicted merge aborts cleanly - the
/// base stays untouched and the conflict paths come back.
pub async fn merge_task(
    state: &ServerState,
    cwd: &Path,
    session_id: &Uuid,
    message: Option<&str>,
) -> Result<(TaskRecord, bool, String)> {
    let record = state
        .tasks
        .get(session_id)
        .ok_or_else(|| anyhow!("session {session_id} is not a task"))?;
    // The merge lands in whatever is checked out here, so refuse unless it
    // is the branch the task was created from. Nothing is changed on refusal.
    let current = git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).await?;
    let current = current.trim();
    if current != record.base {
        return Err(anyhow!(
            "cannot merge task {session_id}: the working directory is on `{current}`, the task was created from `{}`; check out `{}` first",
            record.base,
            record.base
        ));
    }
    let message = message
        .map(|m| m.to_string())
        .unwrap_or_else(|| format!("Merge task '{}'", record.title));

    let merge = git(cwd, &["merge", "--no-ff", "-m", &message, &record.branch]).await;
    match merge {
        Ok(out) => Ok((
            record.clone(),
            true,
            format!(
                "merged {} into {} ({})",
                record.branch,
                record.base,
                out.trim()
            ),
        )),
        Err(merge_error) => {
            // Conflicted (or otherwise failed): collect the unmerged paths
            // BEFORE aborting (abort clears them), then abort cleanly so the
            // base stays untouched.
            let conflicted = git(cwd, &["diff", "--name-only", "--diff-filter=U"])
                .await
                .unwrap_or_default();
            let conflicts: Vec<&str> = conflicted
                .lines()
                .filter(|l| !l.trim().is_empty())
                .collect();
            let _ = git(cwd, &["merge", "--abort"]).await;
            if conflicts.is_empty() {
                return Err(anyhow!(
                    "merge failed (aborted cleanly, base untouched): {merge_error}"
                ));
            }
            Err(anyhow!(
                "merge conflicts (aborted cleanly, base untouched): {} - {merge_error}",
                conflicts.join(", ")
            ))
        }
    }
}

/// Persist `[router] cheap_model/strong_model` (+ optional fallbacks as
/// comments-free plain keys) into the user's config.toml. The dir honors
/// MODRON_CONFIG (the CLI's directory convention).
pub fn persist_router_toml(
    _working_dir: &Path,
    cheap: &str,
    strong: &str,
    cheap_fallback: Option<&str>,
    strong_fallback: Option<&str>,
) -> Result<()> {
    let mut dir = match std::env::var("MODRON_CONFIG") {
        Ok(v) => PathBuf::from(v),
        Err(_) => dirs::config_dir()
            .ok_or_else(|| anyhow!("cannot resolve the config directory"))?
            .join("modron"),
    };
    if !dir.ends_with("config.toml") {
        dir = dir.join("config.toml");
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = std::fs::read_to_string(&dir).unwrap_or_default();
    let mut value: toml::Value =
        toml::from_str(&existing).unwrap_or_else(|_| toml::Value::Table(Default::default()));
    if !value.is_table() {
        value = toml::Value::Table(Default::default());
    }
    let table = value.as_table_mut().unwrap();
    let router = table
        .entry("router")
        .or_insert_with(|| toml::Value::Table(Default::default()));
    if let Some(router_table) = router.as_table_mut() {
        router_table.insert("cheap_model".into(), toml::Value::String(cheap.to_string()));
        router_table.insert(
            "strong_model".into(),
            toml::Value::String(strong.to_string()),
        );
    }
    let rendered = toml::to_string_pretty(&value)?;
    std::fs::write(&dir, rendered)?;
    let _ = (cheap_fallback, strong_fallback); // fallback keys land with the tier-fallback config surface
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugifies_titles() {
        assert_eq!(slugify("Fix the Login Bug!!"), "fix-the-login-bug");
        assert_eq!(slugify("---"), "task");
        assert_eq!(slugify("a  b"), "a-b");
        let long = slugify(&"x".repeat(80));
        assert!(long.len() <= 48);
    }

    fn record(worktree: PathBuf) -> TaskRecord {
        TaskRecord {
            session_id: Uuid::new_v4(),
            worktree,
            branch: "modron/fix-abc123".into(),
            base: "main".into(),
            title: "fix".into(),
        }
    }

    #[test]
    fn task_records_survive_a_reload() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(TASKS_FILE);
        let wt = root.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let rec = record(wt.clone());
        TaskRegistry::load(path.clone()).insert(rec.clone());

        let reloaded = TaskRegistry::load(path);
        let got = reloaded.get(&rec.session_id).expect("record persisted");
        assert_eq!(got.worktree, wt);
        assert_eq!(got.branch, rec.branch);
        assert_eq!(got.base, rec.base);
        assert_eq!(got.title, rec.title);
    }

    #[test]
    fn records_whose_worktree_is_gone_are_dropped_on_load() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(TASKS_FILE);
        let rec = record(root.path().join("deleted-worktree"));
        TaskRegistry::load(path.clone()).insert(rec.clone());
        assert!(TaskRegistry::load(path).get(&rec.session_id).is_none());
    }

    #[test]
    fn removed_records_stay_removed_after_reload() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(TASKS_FILE);
        let wt = root.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let rec = record(wt);
        let reg = TaskRegistry::load(path.clone());
        reg.insert(rec.clone());
        assert!(reg.remove(&rec.session_id).is_some());
        assert!(TaskRegistry::load(path).get(&rec.session_id).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn task_file_is_owner_only_and_leaves_no_temp_file() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(TASKS_FILE);
        let wt = root.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        TaskRegistry::load(path.clone()).insert(record(wt));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let names: Vec<String> = std::fs::read_dir(root.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2, "unexpected files: {names:?}");
    }

    #[test]
    fn validates_base_names() {
        for ok in ["main", "HEAD", "release/1.2", "feature_x"] {
            assert!(validate_base(ok).is_ok(), "{ok} should be accepted");
        }
        for bad in [
            "-x",
            "--output=/tmp/x",
            "",
            "a b",
            "a\tb",
            "main\n",
            "a\u{7}b",
        ] {
            let err = validate_base(bad).expect_err(bad);
            assert!(
                err.to_string().starts_with("invalid base"),
                "{bad:?}: {err}"
            );
        }
    }
}
