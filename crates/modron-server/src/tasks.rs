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

/// The server's task registry: session id -> record. Server-process-local
/// (worktrees themselves are on disk; `task_status` re-derives state).
#[derive(Clone, Default)]
pub struct TaskRegistry {
    tasks: Arc<std::sync::Mutex<std::collections::HashMap<Uuid, TaskRecord>>>,
}

impl TaskRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, record: TaskRecord) {
        self.tasks.lock().unwrap().insert(record.session_id, record);
    }

    pub fn get(&self, id: &Uuid) -> Option<TaskRecord> {
        self.tasks.lock().unwrap().get(id).cloned()
    }

    pub fn remove(&self, id: &Uuid) -> Option<TaskRecord> {
        self.tasks.lock().unwrap().remove(id)
    }
}

async fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .await
        .context("git not found on the server machine")?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(anyhow!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
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
        Err(_) => {
            // Conflicted: abort cleanly (the base stays untouched) and
            // report the conflicting paths.
            let _ = git(cwd, &["merge", "--abort"]).await;
            let conflicted = git(cwd, &["diff", "--name-only", "--diff-filter=U"])
                .await
                .unwrap_or_default();
            let conflicts: Vec<&str> = conflicted
                .lines()
                .filter(|l| !l.trim().is_empty())
                .collect();
            Err(anyhow!(
                "merge conflicts (aborted cleanly, base untouched): {}",
                conflicts.join(", ")
            ))
        }
    }
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
}
