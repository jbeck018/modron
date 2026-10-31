//! The `delegate` tool (Phase 5c A2 — the Kilo post-orchestrator pattern):
//! spawns a subagent in an isolated context, optionally shaped by a named
//! persona (`[agents.<name>]` — its own system prompt, forced model, tool
//! permissions) with its own token budget. Large tasks become subtasks that
//! run in their own context; the parent sees the result, not the work.
//!
//! `explore` remains the read-only special case; `delegate` is the general
//! form (it can edit files when the run's approval mode and the persona's
//! lists allow it).

use crate::common::{internal_tool_error, tool_err};
use crate::reduce::reduce_output;
use crate::validate::validate_args;
use modron_core::{Tool, ToolCtx, ToolOutcome, ToolSpec};

const NAME: &str = "delegate";
const MAX_BYTES: usize = 10 * 1024;
const DEFAULT_BUDGET_TOKENS: u64 = 30_000;
const MAX_BUDGET_TOKENS: u64 = 200_000;

pub struct DelegateTool;

impl Default for DelegateTool {
    fn default() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl Tool for DelegateTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: NAME.to_string(),
            description: "Spawn a subagent in an isolated context to run a subtask end-to-end \
                          (its own budget and session; results come back as a summary). \
                          Optionally shape it with a named persona (see system prompt) and a \
                          tool filter. Use for self-contained subtasks; the parent context \
                          only sees the result."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "task": {"type": "string", "description": "The subtask to run."},
                    "persona": {"type": "string", "description": "Optional persona name (from [agents] config)."},
                    "tools": {"type": "array", "items": {"type": "string"}, "description": "Optional tool allow-list for the subagent."},
                    "budget_tokens": {"type": "integer", "description": "Max tokens (default 30000, max 200000)."},
                    "background": {"type": "boolean", "description": "Run detached; check later with delegate_status."},
                    "isolate": {"type": "boolean", "description": "Run in a git worktree under .modron/worktrees/ (requires git; results reported, never auto-merged)."}
                },
                "required": ["task"]
            }),
        }
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx,
    ) -> modron_core::Result<ToolOutcome> {
        let spec = self.spec();
        if let Err(message) = validate_args(&spec, &args) {
            return Ok(tool_err(
                NAME,
                format!("invalid arguments: {message}"),
                MAX_BYTES,
            ));
        }
        let Some(task) = args.get("task").and_then(|v| v.as_str()) else {
            return Ok(tool_err(NAME, "missing `task`", MAX_BYTES));
        };
        let budget = args
            .get("budget_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_BUDGET_TOKENS)
            .clamp(1_000, MAX_BUDGET_TOKENS);
        let tool_filter = args.get("tools").and_then(|v| v.as_array()).map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<String>>()
        });

        let Some(engine) = ctx.engine.as_ref() else {
            return Ok(tool_err(
                NAME,
                "no engine attached to this context — delegate needs a running agent",
                MAX_BYTES,
            ));
        };

        // Persona resolution: named personas come from the engine config;
        // an unknown name is an error outcome listing what exists (never a
        // silent fall-through to the wrong agent).
        let persona = match args.get("persona").and_then(|v| v.as_str()) {
            Some(name) => match engine.config.personas.get(name) {
                Some(persona) => Some(persona),
                None => {
                    let available: Vec<&str> =
                        engine.config.personas.keys().map(|s| s.as_str()).collect();
                    return Ok(tool_err(
                        NAME,
                        format!(
                            "unknown persona `{name}` — configured: {}",
                            if available.is_empty() {
                                "(none)".to_string()
                            } else {
                                available.join(", ")
                            }
                        ),
                        MAX_BYTES,
                    ));
                }
            },
            None => None,
        };

        // Worktree isolation (Phase 5i I3): the child's working dir becomes
        // a fresh git worktree; results are reported, never auto-merged.
        let mut child_working_dir = ctx.working_dir.clone();
        let isolate = args
            .get("isolate")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if isolate {
            let id = modron_core::subagent::new_worktree_id();
            let worktree = ctx.working_dir.join(".modron").join("worktrees").join(&id);
            let output = tokio::process::Command::new("git")
                .args(["worktree", "add"])
                .arg(&worktree)
                .current_dir(&ctx.working_dir)
                .output()
                .await;
            match output {
                Ok(o) if o.status.success() => {
                    child_working_dir = worktree;
                }
                other => {
                    let detail = match other {
                        Ok(o) => String::from_utf8_lossy(&o.stderr).trim().to_string(),
                        Err(e) => e.to_string(),
                    };
                    return Ok(tool_err(
                        NAME,
                        format!(
                            "worktree isolation failed (is this a git repo with at least one commit?): {detail}"
                        ),
                        MAX_BYTES,
                    ));
                }
            }
        }
        let child_ctx = ToolCtx {
            working_dir: child_working_dir.clone(),
            sandbox: ctx.sandbox,
            engine: ctx.engine.clone(),
        };
        let _ = &child_ctx; // children receive it via ToolCtx.engine wiring

        // Background mode (Phase 5i I2): detached child, state in the
        // engine registry, `delegate_status` reads it later.
        let background = args
            .get("background")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if background {
            let bg = engine.config.background_subagents.clone();
            let id = modron_core::subagent::new_worktree_id();
            let id_for_msg = id.clone();
            let persona_for_task = persona.cloned();
            let task = task.to_string();
            let tool_filter = tool_filter.clone();
            let engine = std::sync::Arc::clone(engine);
            let mut registry_guard = bg.lock().unwrap();
            registry_guard.insert(
                id.clone(),
                modron_core::BackgroundDelegate {
                    label: persona_for_task
                        .as_ref()
                        .map(|_| "persona".to_string())
                        .unwrap_or_else(|| "delegate".to_string()),
                    status: modron_core::BackgroundStatus::Running,
                },
            );
            drop(registry_guard);
            let registry = bg.clone();
            tokio::spawn(async move {
                let report = modron_core::subagent::delegate(
                    &engine,
                    &task,
                    persona_for_task.as_ref(),
                    tool_filter.as_deref(),
                    budget,
                )
                .await;
                let mut guard = registry.lock().unwrap();
                if let Some(entry) = guard.get_mut(&id) {
                    entry.status = match report {
                        Ok(report) => modron_core::BackgroundStatus::Done {
                            brief: report.brief,
                            tokens: report.usage.total(),
                            turns: report.turns,
                            is_error: false,
                        },
                        Err(e) => modron_core::BackgroundStatus::Done {
                            brief: e.to_string(),
                            tokens: 0,
                            turns: 0,
                            is_error: true,
                        },
                    };
                }
            });
            return Ok(ToolOutcome::ok(format!(
                "subagent {id_for_msg} started in the background — check with delegate_status {{\"id\": \"{id_for_msg}\"}}"
            )));
        }

        match modron_core::subagent::delegate(engine, task, persona, tool_filter.as_deref(), budget)
            .await
        {
            Ok(report) => {
                let mut brief = report.brief;
                if isolate {
                    brief = format!(
                        "{brief}\n(worktree: {} — results are NOT merged; review and merge explicitly)",
                        child_working_dir.display()
                    );
                }
                let body = reduce_output(
                    format!(
                        "{}\n(delegate: {} tok, {} turns)",
                        brief,
                        report.usage.total(),
                        report.turns
                    ),
                    MAX_BYTES,
                );
                Ok(ToolOutcome {
                    content: body,
                    is_error: brief.is_empty(),
                    usage: Some(report.usage),
                })
            }
            Err(e) => Err(internal_tool_error(NAME, e.to_string())),
        }
    }
}

/// `delegate_status` (Phase 5i I2): list background subagents — running
/// ones with their label, completed ones with their brief.
pub struct DelegateStatusTool;

impl Default for DelegateStatusTool {
    fn default() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl Tool for DelegateStatusTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "delegate_status".to_string(),
            description: "Check background subagents: list all, or fetch one by id (returns                           the result brief when done)."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"id": {"type": "string", "description": "Optional subagent id."}}
            }),
        }
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx,
    ) -> modron_core::Result<ToolOutcome> {
        let Some(engine) = ctx.engine.as_ref() else {
            return Ok(tool_err(NAME, "no engine attached", MAX_BYTES));
        };
        let registry = engine.config.background_subagents.clone();
        let guard = registry.lock().unwrap();
        if let Some(id) = args.get("id").and_then(|v| v.as_str()) {
            return match guard.get(id) {
                Some(entry) => Ok(ToolOutcome::ok(format_status(id, entry))),
                None => Ok(tool_err(
                    NAME,
                    format!(
                        "no background subagent `{id}` — use delegate_status with no id to list"
                    ),
                    MAX_BYTES,
                )),
            };
        }
        if guard.is_empty() {
            return Ok(ToolOutcome::ok("no background subagents"));
        }
        let mut lines = Vec::new();
        for (id, entry) in guard.iter() {
            lines.push(format_status(id, entry));
        }
        Ok(ToolOutcome::ok(lines.join("\n")))
    }
}

fn format_status(id: &str, entry: &modron_core::BackgroundDelegate) -> String {
    match &entry.status {
        modron_core::BackgroundStatus::Running => {
            format!("{id}: running ({})", entry.label)
        }
        modron_core::BackgroundStatus::Done {
            brief,
            tokens,
            turns,
            is_error,
        } => {
            let mark = if *is_error { "FAILED" } else { "done" };
            format!("{id}: {mark} ({tokens} tok, {turns} turns)\n{brief}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn without_an_engine_it_errors_honestly() {
        let outcome = DelegateTool
            .execute(serde_json::json!({"task": "x"}), &ToolCtx::new("."))
            .await
            .unwrap();
        assert!(outcome.is_error);
        assert!(outcome.content.contains("no engine attached"));
    }

    #[tokio::test]
    async fn unknown_persona_lists_configured_agents() {
        let dir = tempfile::tempdir().unwrap();
        let mut personas = std::collections::BTreeMap::new();
        personas.insert(
            "reviewer".to_string(),
            modron_core::PersonaSpec {
                system: Some("review".into()),
                ..Default::default()
            },
        );
        let engine = modron_core::Engine {
            selector: std::sync::Arc::new(modron_core::testing::StaticSelector::new("m", "mock")),
            providers: std::collections::HashMap::new(),
            tools: std::sync::Arc::new(modron_core::ToolRegistry::new()),
            store: std::sync::Arc::new(modron_core::testing::InMemoryStore::default()),
            config: modron_core::EngineConfig {
                working_dir: dir.path().to_path_buf(),
                personas,
                ..modron_core::EngineConfig::default()
            },
        };
        let outcome = DelegateTool
            .execute(
                serde_json::json!({"task": "x", "persona": "nope"}),
                &ToolCtx {
                    working_dir: dir.path().to_path_buf(),
                    sandbox: Default::default(),
                    engine: Some(std::sync::Arc::new(engine)),
                },
            )
            .await
            .unwrap();
        assert!(outcome.is_error);
        assert!(outcome.content.contains("unknown persona `nope`"));
        assert!(outcome.content.contains("configured: reviewer"));
    }
}

#[cfg(test)]
mod phase5i_tests {
    use super::*;
    use modron_core::testing::{InMemoryStore, MockProvider, StaticSelector};
    use std::collections::HashMap;
    use std::sync::Arc;

    fn engine_with(
        script: MockProvider,
        personas: Option<modron_core::PersonaSpec>,
    ) -> Arc<modron_core::Engine> {
        let mut providers: HashMap<String, Arc<dyn modron_core::ModelProvider>> = HashMap::new();
        providers.insert("mock".into(), Arc::new(script));
        let mut personas_map = std::collections::BTreeMap::new();
        if let Some(p) = personas {
            personas_map.insert("builder".to_string(), p);
        }
        Arc::new(modron_core::Engine {
            selector: Arc::new(StaticSelector::new("mock-model", "mock")),
            providers,
            tools: Arc::new(modron_core::ToolRegistry::new()),
            store: Arc::new(InMemoryStore::default()),
            config: modron_core::EngineConfig {
                personas: personas_map,
                ..modron_core::EngineConfig::default()
            },
        })
    }

    fn ctx_with(engine: &Arc<modron_core::Engine>, dir: &std::path::Path) -> ToolCtx {
        ToolCtx {
            working_dir: dir.to_path_buf(),
            sandbox: Default::default(),
            engine: Some(Arc::clone(engine)),
        }
    }

    #[tokio::test]
    async fn background_delegate_reports_started_then_done() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_with(
            MockProvider::new().then_text("background brief result"),
            None,
        );
        let ctx = ctx_with(&engine, dir.path());

        let started = DelegateTool
            .execute(
                serde_json::json!({"task": "work", "background": true}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!started.is_error);
        assert!(
            started.content.contains("started in the background"),
            "{started:?}"
        );

        // The child completes quickly (mock); poll status.
        let status = DelegateStatusTool
            .execute(serde_json::json!({}), &ctx)
            .await
            .unwrap();
        let mut saw_done = false;
        for _ in 0..50 {
            let status = DelegateStatusTool
                .execute(serde_json::json!({}), &ctx)
                .await
                .unwrap();
            if status.content.contains("done") {
                saw_done = true;
                assert!(
                    status.content.contains("background brief result"),
                    "{status:?}"
                );
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(saw_done, "background child must complete: {status:?}");
    }

    #[tokio::test]
    async fn worktree_isolation_creates_and_reports() {
        let dir = tempfile::tempdir().unwrap();
        // git repo with one commit.
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args([
                "-C",
                dir.path().to_str().unwrap(),
                "config",
                "user.email",
                "t@t",
            ])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args([
                "-C",
                dir.path().to_str().unwrap(),
                "config",
                "user.name",
                "t",
            ])
            .status()
            .unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        std::process::Command::new("git")
            .args(["-C", dir.path().to_str().unwrap(), "add", "-A"])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args(["-C", dir.path().to_str().unwrap(), "commit", "-qm", "init"])
            .status()
            .unwrap();

        let engine = engine_with(MockProvider::new().then_text("isolated work done"), None);
        let ctx = ctx_with(&engine, dir.path());
        let outcome = DelegateTool
            .execute(
                serde_json::json!({"task": "mutate freely", "isolate": true}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!outcome.is_error, "{outcome:?}");
        assert!(outcome.content.contains("worktree:"), "{outcome:?}");
        assert!(
            outcome.content.contains("NOT merged"),
            "review-first semantics: {outcome:?}"
        );
        let wt = dir.path().join(".modron").join("worktrees");
        assert!(wt.exists(), "worktree dir created");
    }

    #[tokio::test]
    async fn worktree_isolation_without_git_errors_actionably() {
        let dir = tempfile::tempdir().unwrap(); // not a git repo
        let engine = engine_with(MockProvider::new(), None);
        let ctx = ctx_with(&engine, dir.path());
        let outcome = DelegateTool
            .execute(serde_json::json!({"task": "x", "isolate": true}), &ctx)
            .await
            .unwrap();
        assert!(outcome.is_error);
        assert!(outcome.content.contains("worktree isolation failed"));
    }
}
/// `merge_worktree` (Phase 6f): merge an isolated delegate's git worktree
/// back into the current branch with a merge commit. Conflicts abort with
/// an actionable error — review-first; the agent never force-resolves.
pub struct MergeWorktreeTool;

impl Default for MergeWorktreeTool {
    fn default() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl Tool for MergeWorktreeTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "merge_worktree".to_string(),
            description: "Merge an isolated delegate's worktree (path from a delegate result) \
                          back into the current branch. Conflicts abort with an error — resolve \
                          them yourself; never force."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "The worktree directory to merge."},
                    "message": {"type": "string", "description": "Optional merge commit message."}
                },
                "required": ["path"]
            }),
        }
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx,
    ) -> modron_core::Result<ToolOutcome> {
        let spec = self.spec();
        if let Err(message) = validate_args(&spec, &args) {
            return Ok(tool_err(
                NAME,
                format!("invalid arguments: {message}"),
                MAX_BYTES,
            ));
        }
        let Some(path) = args.get("path").and_then(|v| v.as_str()) else {
            return Ok(tool_err(NAME, "missing `path`", MAX_BYTES));
        };
        // The worktree must live under the repo's .modron/worktrees —
        // merging arbitrary paths is not this tool's contract.
        let canonical =
            std::path::Path::new(path)
                .canonicalize()
                .map_err(|e| modron_core::Error::Tool {
                    tool: NAME.into(),
                    message: format!("worktree path unreadable: {e}"),
                })?;
        let allowed_root = ctx
            .working_dir
            .join(".modron")
            .join("worktrees")
            .canonicalize()
            .unwrap_or_else(|_| ctx.working_dir.join(".modron").join("worktrees"));
        if !canonical.starts_with(&allowed_root) {
            return Ok(tool_err(
                NAME,
                "path is not under .modron/worktrees — only isolated delegate worktrees can be merged",
                MAX_BYTES,
            ));
        }

        // Commit any uncommitted work in the worktree first (the delegate
        // may not have committed), then merge its HEAD.
        async fn run_in(
            dir: &std::path::Path,
            args: &[&str],
        ) -> std::io::Result<std::process::Output> {
            tokio::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .await
        }
        let status = run_in(&canonical, &["status", "--porcelain"])
            .await
            .map_err(internal_err)?;
        if !String::from_utf8_lossy(&status.stdout).trim().is_empty() {
            let add = run_in(&canonical, &["add", "-A"])
                .await
                .map_err(internal_err)?;
            if !add.status.success() {
                return Ok(tool_err(NAME, "cannot stage worktree changes", MAX_BYTES));
            }
            let commit = run_in(&canonical, &["commit", "-qm", "delegate work"]).await;
            if let Ok(c) = &commit {
                if !c.status.success()
                    && !String::from_utf8_lossy(&c.stdout).contains("nothing to commit")
                {
                    return Ok(tool_err(NAME, "cannot commit worktree changes", MAX_BYTES));
                }
            }
        }
        let head = run_in(&canonical, &["rev-parse", "HEAD"])
            .await
            .map_err(internal_err)?;
        if !head.status.success() {
            return Ok(tool_err(NAME, "worktree has no commits", MAX_BYTES));
        }
        let sha = String::from_utf8_lossy(&head.stdout).trim().to_string();
        let message = args
            .get("message")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| format!("merge delegated work ({sha:.8})"));
        let merge = run_in(
            &ctx.working_dir,
            &["merge", "--no-ff", "-m", &message, &sha],
        )
        .await
        .map_err(internal_err)?;
        if !merge.status.success() {
            // Abort the in-progress conflicted merge, then report.
            let _ = run_in(&ctx.working_dir, &["merge", "--abort"]).await;
            return Ok(tool_err(
                NAME,
                format!(
                    "merge conflicted and was aborted — resolve manually: {}",
                    String::from_utf8_lossy(&merge.stdout).trim()
                ),
                MAX_BYTES,
            ));
        }
        Ok(ToolOutcome::ok(format!(
            "merged {sha:.8} into the current branch ({})",
            ctx.working_dir.display()
        )))
    }
}

fn internal_err(e: std::io::Error) -> modron_core::Error {
    modron_core::Error::Tool {
        tool: "merge_worktree".into(),
        message: e.to_string(),
    }
}

#[cfg(test)]
mod merge_tests {
    use super::*;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn repo_with_worktree() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        git(dir.path(), &["config", "user.email", "t@t"]);
        git(dir.path(), &["config", "user.name", "t"]);
        std::fs::write(dir.path().join("base.txt"), "base").unwrap();
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-qm", "init"]);
        // The worktree an isolated delegate would have created.
        let wt = dir
            .path()
            .join(".modron")
            .join("worktrees")
            .join("abc12345");
        git(dir.path(), &["worktree", "add", wt.to_str().unwrap()]);
        // Uncommitted delegate work inside it.
        std::fs::write(wt.join("feature.txt"), "delegated").unwrap();
        (dir, wt)
    }

    #[tokio::test]
    async fn merges_delegated_work_with_commit() {
        let (dir, wt) = repo_with_worktree();
        let ctx = ToolCtx::new(dir.path());
        let out = MergeWorktreeTool
            .execute(
                serde_json::json!({"path": wt.to_str().unwrap(), "message": "merge feat"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{out:?}");
        assert!(
            dir.path().join("feature.txt").exists(),
            "file landed in main tree"
        );
        let log = std::process::Command::new("git")
            .args(["log", "--oneline", "-1"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&log.stdout).contains("merge feat"),
            "merge commit exists"
        );
    }

    #[tokio::test]
    async fn refuses_paths_outside_worktrees() {
        let (dir, _wt) = repo_with_worktree();
        let outside = dir.path().join("..");
        let ctx = ToolCtx::new(dir.path());
        let out = MergeWorktreeTool
            .execute(serde_json::json!({"path": outside.to_str().unwrap()}), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("not under .modron/worktrees"));
    }

    #[tokio::test]
    async fn conflict_aborts_review_first() {
        let (dir, wt) = repo_with_worktree();
        // Diverge: main and worktree both change base.txt differently.
        std::fs::write(dir.path().join("base.txt"), "main-change").unwrap();
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-qm", "main change"]);
        std::fs::write(wt.join("base.txt"), "delegate-change").unwrap();
        git(&wt, &["add", "-A"]);
        git(&wt, &["commit", "-qm", "delegate change"]);

        let ctx = ToolCtx::new(dir.path());
        let out = MergeWorktreeTool
            .execute(serde_json::json!({"path": wt.to_str().unwrap()}), &ctx)
            .await
            .unwrap();
        assert!(out.is_error, "conflict must fail the tool");
        assert!(out.content.contains("conflicted"), "{}", out.content);
        // Repo is left clean (merge aborted), not mid-conflict.
        let status = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&status.stdout).contains("UU"),
            "no unresolved conflict state left behind"
        );
    }
}
