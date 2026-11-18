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

// ---------------------------------------------------------------------------
// Team workspaces (team mode, see [`crate::team`]).
//
// A team run owns, under `<main>/.modron/worktrees/`:
//   - `team-<id>`            the integration worktree, on branch `team/<id>`
//   - `team-<id>-<label>`    one worktree per work item, detached
//   - `team-<id>.marker`     a file holding the pid of the run that owns them
// `<id>` is 8 hex digits. Delegate `isolate` worktrees are `<8 hex>` names and
// the user's own worktrees live elsewhere; neither matches a team name, and
// the sweep below touches only names with a marker whose pid is gone.
// ---------------------------------------------------------------------------

/// Prefix of every team workspace name (directory, item and marker).
pub const TEAM_PREFIX: &str = "team-";

/// The branch a team works on: `team/<id>`.
pub fn team_branch(id: &str) -> String {
    format!("team/{id}")
}

fn worktrees_root(main: &Path) -> PathBuf {
    main.join(".modron").join("worktrees")
}

/// Team `id`'s integration worktree.
pub fn team_dir(main: &Path, id: &str) -> PathBuf {
    worktrees_root(main).join(format!("{TEAM_PREFIX}{id}"))
}

/// The worktree of item `label` (for example `1.2`) of team `id`.
pub fn item_dir(main: &Path, id: &str, label: &str) -> PathBuf {
    worktrees_root(main).join(format!("{TEAM_PREFIX}{id}-{label}"))
}

fn marker_path(main: &Path, id: &str) -> PathBuf {
    worktrees_root(main).join(format!("{TEAM_PREFIX}{id}.marker"))
}

/// Team ids are the 8 hex digits of a fresh uuid.
fn valid_id(id: &str) -> bool {
    id.len() == 8 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A `git` command for a synchronous caller (the team guard's `Drop`).
fn git_sync(dir: &Path, args: &[&str]) -> bool {
    let mut cmd = std::process::Command::new("git");
    crate::hooks::scrub_std_command(&mut cmd);
    cmd.current_dir(dir)
        .args(args)
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Create the integration worktree `path` on a new branch `branch` at HEAD.
pub async fn create_on_branch(main: &Path, path: &Path, branch: &str) -> Result<(), String> {
    let out = git(main)
        .args(["worktree", "add", "-b", branch])
        .arg(path)
        .arg("HEAD")
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// True when `main` has uncommitted changes to tracked files. Untracked files
/// do not count: a team leaves none of its own in the checkout.
pub async fn is_dirty(main: &Path) -> Result<bool, String> {
    let out = git(main)
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(!String::from_utf8_lossy(&out.stdout).trim().is_empty())
}

/// Fast-forward the checkout at `main` to `branch`. Refused (an error with
/// git's report) when the checkout's branch has moved or cannot fast-forward.
pub async fn fast_forward(main: &Path, branch: &str) -> Result<(), String> {
    let out = git(main)
        .args(["merge", "--ff-only", "--no-edit", branch])
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        let _ = git(main).args(["merge", "--abort"]).output().await;
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Record that this process owns team `id` (the sweep reads the pid).
pub fn write_marker(main: &Path, id: &str) -> Result<(), String> {
    std::fs::create_dir_all(worktrees_root(main)).map_err(|e| e.to_string())?;
    std::fs::write(marker_path(main, id), std::process::id().to_string()).map_err(|e| e.to_string())
}

/// Remove one worktree directory: git's removal, then the directory itself.
fn remove_dir(main: &Path, path: &Path) {
    let _ = git_sync(
        main,
        &["worktree", "remove", "--force", &path.to_string_lossy()],
    );
    if path.exists() {
        let _ = std::fs::remove_dir_all(path);
    }
}

/// Everything team `id` left: its worktrees (integration and items), its
/// branch and its marker. Synchronous, so the team guard can run it from
/// `Drop` when the run is dropped mid-flight.
pub fn remove_team(main: &Path, id: &str) {
    if !valid_id(id) {
        return;
    }
    remove_team_dirs(main, id);
    let _ = git_sync(main, &["branch", "-D", &team_branch(id)]);
    let _ = std::fs::remove_file(marker_path(main, id));
}

/// Every worktree directory of team `id` (its integration and item
/// worktrees), pruned. The branch and the marker are left to the caller.
fn remove_team_dirs(main: &Path, id: &str) {
    let prefix = format!("{TEAM_PREFIX}{id}");
    if let Ok(entries) = std::fs::read_dir(worktrees_root(main)) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let mine = name == prefix || name.starts_with(&format!("{prefix}-"));
            if mine && entry.path().is_dir() {
                remove_dir(main, &entry.path());
            }
        }
    }
    let _ = git_sync(main, &["worktree", "prune"]);
}

/// Remove team `id`'s integration worktree and its marker, keeping the branch
/// and any item worktree kept on purpose (a run that did not land).
pub fn remove_work(main: &Path, id: &str) {
    if !valid_id(id) {
        return;
    }
    remove_dir(main, &team_dir(main, id));
    let _ = git_sync(main, &["worktree", "prune"]);
    let _ = std::fs::remove_file(marker_path(main, id));
}

/// Whether process `pid` has exited. Errors on the side of "still running":
/// a pid that cannot be signalled is treated as live, so its team is kept.
#[cfg(unix)]
fn pid_gone(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only tests for the process; it sends nothing.
    let rc = unsafe { libc::kill(pid, 0) };
    rc != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// No portable liveness test here: the owner is treated as alive, so the
/// sweep removes nothing on these platforms (conservative, never destructive).
#[cfg(not(unix))]
fn pid_gone(_pid: u32) -> bool {
    false
}

/// Remove the leftovers of team runs whose owning process is gone: a marker
/// whose pid has exited takes its `team-<id>` worktrees and the marker with
/// it. The team branch is kept: a killed run never landed, so it may hold work
/// the user wants (`git branch --list 'team/*'` shows it). A marker of a live
/// process, and anything without a marker, is left alone. Returns the ids
/// whose worktrees were removed.
pub fn sweep_stale(main: &Path) -> Vec<String> {
    let mut removed = Vec::new();
    let Ok(entries) = std::fs::read_dir(worktrees_root(main)) else {
        return removed;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(id) = name
            .strip_prefix(TEAM_PREFIX)
            .and_then(|rest| rest.strip_suffix(".marker"))
        else {
            continue;
        };
        if !valid_id(id) {
            continue;
        }
        let owner = std::fs::read_to_string(entry.path())
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok());
        match owner {
            Some(pid) if pid_gone(pid) => {}
            _ => continue,
        }
        remove_team_dirs(main, id);
        let _ = std::fs::remove_file(marker_path(main, id));
        removed.push(id.to_string());
    }
    removed
}

/// Owns one team run's workspace. Dropping it while armed (the run was
/// interrupted, or it failed before landing) removes everything the team
/// created. A finished run disarms it after it has removed or kept its state.
pub struct TeamGuard {
    main: PathBuf,
    id: String,
    armed: bool,
}

impl TeamGuard {
    pub fn new(main: &Path, id: &str) -> Self {
        Self {
            main: main.to_path_buf(),
            id: id.to_string(),
            armed: true,
        }
    }

    pub fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TeamGuard {
    fn drop(&mut self) {
        if self.armed {
            remove_team(&self.main, &self.id);
        }
    }
}

#[cfg(test)]
mod team_tests {
    use super::*;

    fn git_run(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git_run(dir.path(), &["init", "-q"]);
        git_run(dir.path(), &["config", "user.email", "wt@test.invalid"]);
        git_run(dir.path(), &["config", "user.name", "wt test"]);
        std::fs::write(dir.path().join("README.md"), "base\n").unwrap();
        git_run(dir.path(), &["add", "-A"]);
        git_run(dir.path(), &["commit", "-qm", "init"]);
        dir
    }

    fn branches(dir: &Path) -> String {
        git_run(dir, &["branch", "--list", "team/*"])
    }

    /// A team `id` with its integration worktree and one item worktree.
    /// `owner` is the pid written to its marker (`None`: no marker).
    async fn make_team(main: &Path, id: &str, owner: Option<String>) {
        let work = team_dir(main, id);
        create_on_branch(main, &work, &team_branch(id))
            .await
            .unwrap();
        create(&work, &item_dir(main, id, "1.1")).await.unwrap();
        if let Some(pid) = owner {
            std::fs::create_dir_all(worktrees_root(main)).unwrap();
            std::fs::write(marker_path(main, id), pid).unwrap();
        }
    }

    #[tokio::test]
    async fn sweep_removes_only_dead_team_leftovers() {
        let main = repo();
        let root = main.path();
        // Dead run: its marker names a pid that cannot exist.
        make_team(root, "deadbeef", Some("2147483647".into())).await;
        // Live run: its marker names this process.
        make_team(root, "cafe0001", Some(std::process::id().to_string())).await;
        // Team-looking directory with no marker: not a team of ours to remove.
        create(root, &team_dir(root, "0badf00d")).await.unwrap();
        // A delegate isolate worktree and a user worktree.
        let delegate = path_for(root, "abcd1234");
        create(root, &delegate).await.unwrap();
        let user = tempfile::tempdir().unwrap();
        let user_wt = user.path().join("mine");
        create(root, &user_wt).await.unwrap();

        assert_eq!(sweep_stale(root), vec!["deadbeef".to_string()]);

        assert!(!team_dir(root, "deadbeef").exists());
        assert!(!item_dir(root, "deadbeef", "1.1").exists());
        assert!(!marker_path(root, "deadbeef").exists());
        let kept = branches(root);
        assert!(kept.contains("team/cafe0001"), "live team kept: {kept}");
        assert!(
            kept.contains("team/deadbeef"),
            "a dead run's unlanded branch is kept: {kept}"
        );
        assert!(team_dir(root, "cafe0001").exists());
        assert!(item_dir(root, "cafe0001", "1.1").exists());
        assert!(team_dir(root, "0badf00d").exists(), "no marker, no removal");
        assert!(delegate.exists(), "delegate isolate worktree untouched");
        assert!(user_wt.exists(), "user worktree untouched");
    }

    #[tokio::test]
    async fn guard_removes_an_armed_team_and_keeps_a_disarmed_one() {
        let main = repo();
        let root = main.path();
        make_team(root, "aaaa1111", None).await;
        make_team(root, "bbbb2222", None).await;
        {
            let _armed = TeamGuard::new(root, "aaaa1111");
        }
        let mut kept = TeamGuard::new(root, "bbbb2222");
        kept.disarm();
        drop(kept);
        assert!(!team_dir(root, "aaaa1111").exists());
        assert!(!item_dir(root, "aaaa1111", "1.1").exists());
        assert!(!branches(root).contains("team/aaaa1111"));
        assert!(team_dir(root, "bbbb2222").exists(), "disarmed guard keeps");
        assert!(branches(root).contains("team/bbbb2222"));
    }
}
