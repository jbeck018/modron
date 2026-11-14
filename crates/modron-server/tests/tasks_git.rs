//! Worktree-task integration tests (paradev/conductor grammar): a real git
//! fixture per test - create a task (worktree + branch + pinned session),
//! commit in the worktree, read status (ahead + numstat), merge back into
//! the base, and verify a conflicted merge aborts cleanly.

use anyhow::Result;
use modron_core::testing::StaticSelector;
use modron_core::{Engine, EngineConfig, SessionStore, ToolRegistry};
use modron_server::dispatch::ServerState;
use modron_server::hub::SessionHub;
use modron_server::tasks;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A git repo fixture: `git init` + one committed file + user config.
fn git_repo(dir: &Path) -> Result<PathBuf> {
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo)?;
    let run = |args: &[&str]| -> Result<()> {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(&repo)
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@test")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@test")
            .output()?;
        assert!(
            out.status.success(),
            "git {:?} failed: {:?}",
            args,
            out.stderr
        );
        Ok(())
    };
    run(&["init", "-b", "main"])?;
    std::fs::write(repo.join("a.txt"), "line1\n")?;
    run(&["add", "a.txt"])?;
    run(&["commit", "-m", "init"])?;
    Ok(repo)
}

fn server_state(dir: &Path, repo: &Path) -> (Arc<ServerState>, std::sync::Arc<dyn SessionStore>) {
    let store: Arc<dyn SessionStore> =
        Arc::new(modron_store::JsonlSessionStore::new(dir.join("sessions")));
    let hub = SessionHub::empty();
    let mut engine_config = EngineConfig {
        working_dir: repo.to_path_buf(),
        ..EngineConfig::default()
    };
    engine_config.approvals.non_interactive = true;
    let engine = Engine {
        selector: Arc::new(StaticSelector::new("m", "mock")),
        providers: HashMap::new(),
        tools: Arc::new(ToolRegistry::new()),
        store: Arc::clone(&store),
        config: engine_config,
    };
    hub.set_engine(engine);
    let state = Arc::new(ServerState {
        hub,
        store: Arc::clone(&store),
        devices_dir: dir.join("clients"),
        version: "test".into(),
        working_dir: repo.to_path_buf(),
        tasks: tasks::TaskRegistry::new(),
        bridge_base: None,
        rc_keys: std::sync::Mutex::new(HashMap::new()),
        rc_tunnels: std::sync::Mutex::new(HashMap::new()),
    });
    (state, store)
}

/// Commit a change to a.txt inside the worktree (simulating agent work).
fn commit_in_worktree(worktree: &Path, content: &str, message: &str) {
    std::fs::write(worktree.join("a.txt"), content).unwrap();
    let out = std::process::Command::new("git")
        .args(["commit", "-am", message])
        .current_dir(worktree)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@test")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@test")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "worktree commit failed: {:?}",
        out.stderr
    );
}

#[tokio::test]
async fn task_create_status_merge_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let repo = git_repo(dir.path()).unwrap();
    let (state, _store) = server_state(dir.path(), &repo);

    let record = tasks::create_task(&state, &state.hub, &repo, "Fix the Login Bug", None)
        .await
        .unwrap();
    assert!(record.branch.starts_with("modron/fix-the-login-bug-"));
    assert!(record.worktree.is_dir(), "worktree created");
    // The session runs inside the worktree.
    let handle = state
        .hub
        .handle(&record.session_id)
        .expect("pinned session");
    assert_eq!(
        handle
            .working_dir()
            .canonicalize()
            .unwrap_or_else(|_| handle.working_dir()),
        record
            .worktree
            .canonicalize()
            .unwrap_or_else(|_| record.worktree.clone()),
        "session runs inside the worktree"
    );

    // Status before any work: clean, nothing ahead.
    let (_, ahead, files) = tasks::task_status(&state, &record.session_id)
        .await
        .unwrap();
    assert_eq!(ahead, 0);
    assert!(files.is_empty());

    // Agent work: one commit in the worktree.
    commit_in_worktree(&record.worktree, "line1\nline2\n", "add line2");
    let (record, ahead, files) = tasks::task_status(&state, &record.session_id)
        .await
        .unwrap();
    assert_eq!(ahead, 1);
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, "a.txt");
    assert_eq!(files[0].added, 1);
    assert_eq!(files[0].deleted, 0);

    // Merge back into main: base file now contains the worktree's change.
    let (_record, ok, summary) = tasks::merge_task(&state, &repo, &record.session_id, None)
        .await
        .unwrap();
    assert!(ok, "merge succeeds: {summary}");
    assert!(summary.contains("merged"), "{summary}");
    let merged = std::fs::read_to_string(repo.join("a.txt")).unwrap();
    assert_eq!(merged, "line1\nline2\n", "base checkout has the work");
    assert_eq!(record.base, "main");

    // Status after merge: nothing ahead anymore.
    let (_, ahead, _) = tasks::task_status(&state, &record.session_id)
        .await
        .unwrap();
    assert_eq!(ahead, 0, "worktree fully merged");
}

#[tokio::test]
async fn merge_conflict_aborts_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let repo = git_repo(dir.path()).unwrap();
    let (state, _store) = server_state(dir.path(), &repo);

    let record = tasks::create_task(&state, &state.hub, &repo, "Rewrite a.txt", None)
        .await
        .unwrap();
    commit_in_worktree(&record.worktree, "from the task\n", "task rewrites");

    // Competing change on the base branch (main checkout).
    std::fs::write(repo.join("a.txt"), "from main\n").unwrap();
    let out = std::process::Command::new("git")
        .args(["commit", "-am", "main rewrites"])
        .current_dir(&repo)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@test")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@test")
        .output()
        .unwrap();
    assert!(out.status.success());

    let err = tasks::merge_task(&state, &repo, &record.session_id, None)
        .await
        .expect_err("conflicted merge must fail");
    assert!(
        err.to_string().contains("aborted cleanly"),
        "conflict must abort cleanly: {err}"
    );
    assert!(
        err.to_string().contains("CONFLICT"),
        "the conflict notice should surface: {err}"
    );

    let merged = std::fs::read_to_string(repo.join("a.txt")).unwrap();
    assert_eq!(
        merged, "from main\n",
        "conflicted merge left the base untouched"
    );
    // No merge in progress (aborted cleanly).
    assert!(!repo.join(".git").join("MERGE_HEAD").exists());
}

#[tokio::test]
async fn create_task_requires_a_git_repo() {
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let repo = dir.path().join("repo");
    let (state, _store) = server_state(dir.path(), &repo);
    let err = tasks::create_task(&state, &state.hub, &plain, "no git here", None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not a git repository"), "{err}");
}

/// Run a git command in `cwd` with the test identity; return its stdout.
fn run_git(cwd: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@test")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@test")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {:?}",
        out.stderr
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[tokio::test]
async fn merge_refuses_when_checkout_is_not_the_base() {
    let dir = tempfile::tempdir().unwrap();
    let repo = git_repo(dir.path()).unwrap();
    let (state, _store) = server_state(dir.path(), &repo);
    let record = tasks::create_task(&state, &state.hub, &repo, "Add line two", None)
        .await
        .unwrap();
    commit_in_worktree(&record.worktree, "line1\nline2\n", "add line2");

    // The server checkout moves to another branch: a merge must not land there.
    run_git(&repo, &["checkout", "-b", "other"]);
    let err = tasks::merge_task(&state, &repo, &record.session_id, None)
        .await
        .expect_err("merge must refuse a checkout that is not the base");
    let text = err.to_string();
    assert!(
        text.contains("the working directory is on `other`"),
        "{text}"
    );
    assert!(text.contains("check out `main` first"), "{text}");

    assert_eq!(
        run_git(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "other"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("a.txt")).unwrap(),
        "line1\n"
    );
    // Tracked files only: the task worktree itself lives under `.modron/`.
    assert_eq!(
        run_git(&repo, &["status", "--porcelain", "--untracked-files=no"]).trim(),
        ""
    );
    assert!(!repo.join(".git").join("MERGE_HEAD").exists());
}

#[tokio::test]
async fn merge_conflict_reports_the_conflicted_files() {
    let dir = tempfile::tempdir().unwrap();
    let repo = git_repo(dir.path()).unwrap();
    let (state, _store) = server_state(dir.path(), &repo);
    let record = tasks::create_task(&state, &state.hub, &repo, "Rewrite a.txt again", None)
        .await
        .unwrap();
    commit_in_worktree(&record.worktree, "from the task\n", "task rewrites");
    std::fs::write(repo.join("a.txt"), "from main\n").unwrap();
    run_git(&repo, &["commit", "-am", "main rewrites"]);

    let err = tasks::merge_task(&state, &repo, &record.session_id, None)
        .await
        .expect_err("conflicted merge must fail");
    let text = err.to_string();
    assert!(
        text.starts_with("merge conflicts (aborted cleanly, base untouched): a.txt"),
        "conflicted file must be listed before the abort: {text}"
    );
    assert!(text.contains("CONFLICT"), "{text}");
    assert_eq!(
        std::fs::read_to_string(repo.join("a.txt")).unwrap(),
        "from main\n"
    );
    assert!(!repo.join(".git").join("MERGE_HEAD").exists());
    assert_eq!(
        run_git(&repo, &["status", "--porcelain", "--untracked-files=no"]).trim(),
        ""
    );
}

#[tokio::test]
async fn create_task_rejects_malformed_base() {
    for bad in ["-x", "", "a b", "main\n", "ma\tin"] {
        let dir = tempfile::tempdir().unwrap();
        let repo = git_repo(dir.path()).unwrap();
        let (state, _store) = server_state(dir.path(), &repo);
        let err = tasks::create_task(&state, &state.hub, &repo, "Bad base", Some(bad))
            .await
            .expect_err("malformed base must be refused");
        assert!(err.to_string().contains("invalid base"), "{bad:?}: {err}");
        assert!(!repo.join(".modron").exists(), "no worktree for {bad:?}");
    }
}
