//! Git worktree steps shared by `delegate` (`isolate`), the `merge_worktree`
//! tool and team mode ([`crate::team`]): create an isolated worktree under
//! `.modron/worktrees/`, commit its work, and merge that commit into the main
//! checkout with a merge commit. The git steps live here once; the tool layer
//! keeps its own path checks and wording.

use crate::hooks::scrub_tokio_command;
use std::path::{Path, PathBuf};

/// A `git` command rooted at `dir` with provider credentials removed from
/// its environment (every git spawn of this module goes through here).
fn git(dir: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("git");
    scrub_tokio_command(&mut cmd);
    cmd.current_dir(dir);
    cmd
}

/// Where the worktree `id` lives: `<main>/.modron/worktrees/<id>`.
pub fn path_for(main: &Path, id: &str) -> PathBuf {
    main.join(".modron").join("worktrees").join(id)
}

/// Create a detached worktree of `main`'s HEAD at `path`. The error is git's
/// stderr (or the spawn error).
pub async fn create(main: &Path, path: &Path) -> Result<(), String> {
    let out = git(main)
        .args(["worktree", "add"])
        .arg(path)
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Force-remove a worktree. Best effort: a worktree that cannot be removed is
/// left in place (it is reported where it is).
pub async fn remove(main: &Path, path: &Path) {
    let _ = git(main)
        .args(["worktree", "remove", "--force"])
        .arg(path)
        .output()
        .await;
}

/// Commit whatever is uncommitted in `worktree` (when there is anything) and
/// return its HEAD sha.
pub async fn commit_all(worktree: &Path) -> Result<String, String> {
    let status = git(worktree)
        .args(["status", "--porcelain"])
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if !String::from_utf8_lossy(&status.stdout).trim().is_empty() {
        let add = git(worktree)
            .args(["add", "-A"])
            .output()
            .await
            .map_err(|e| e.to_string())?;
        if !add.status.success() {
            return Err("cannot stage worktree changes".into());
        }
        let commit = git(worktree)
            .args(["commit", "-qm", "delegate work"])
            .output()
            .await
            .map_err(|e| e.to_string())?;
        if !commit.status.success()
            && !String::from_utf8_lossy(&commit.stdout).contains("nothing to commit")
        {
            return Err("cannot commit worktree changes".into());
        }
    }
    let head = git(worktree)
        .args(["rev-parse", "HEAD"])
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if !head.status.success() {
        return Err("worktree has no commits".into());
    }
    Ok(String::from_utf8_lossy(&head.stdout).trim().to_string())
}

/// The result of merging one worktree commit into the main checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    Merged,
    /// The merge did not apply and was aborted; git's report is attached.
    Conflict(String),
}

/// Merge commit `sha` into the checkout at `main` with a merge commit
/// (`--no-ff`). A merge that does not apply is aborted and reported as
/// [`MergeOutcome::Conflict`]; the agent never force-resolves.
pub async fn merge_into(main: &Path, sha: &str, message: &str) -> Result<MergeOutcome, String> {
    let merge = git(main)
        .args(["merge", "--no-ff", "-m", message, sha])
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if merge.status.success() {
        return Ok(MergeOutcome::Merged);
    }
    let _ = git(main).args(["merge", "--abort"]).output().await;
    Ok(MergeOutcome::Conflict(
        String::from_utf8_lossy(&merge.stdout).trim().to_string(),
    ))
}

/// `git diff --no-color base head` in `dir`, cut at `cap` bytes. `None` when
/// git cannot produce it.
pub async fn diff(dir: &Path, base: &str, head: &str, cap: usize) -> Option<String> {
    let out = git(dir)
        .args(["diff", "--no-color", base, head])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    if text.len() <= cap {
        return Some(text.into_owned());
    }
    let mut cut = cap;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    Some(format!("{}\n[diff truncated]", &text[..cut]))
}

/// HEAD's sha in `dir`, or `None` when there is no commit (or no git).
pub async fn head_sha(dir: &Path) -> Option<String> {
    let out = git(dir).args(["rev-parse", "HEAD"]).output().await.ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
