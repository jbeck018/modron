//! Phase 6c — the eval harness: a mini SWE-bench for the harness itself.
//!
//! Each eval task = (prompt, optional setup script, verify script). The task
//! runs the FULL engine (real model + tools) in a fresh temp workspace:
//! setup prepares it, the agent works, then `verify` runs — exit 0 = solved.
//! Completion alone is not success; the objective verifier decides. That is
//! the difference between "the model said done" and "the work is done".

use crate::{config, short, AgentSinks, Cli};
use modron_core::{Engine, RunSummary};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

/// One eval task (loaded from a committed TOML suite).
#[derive(Clone, Debug, Deserialize)]
pub struct EvalTask {
    pub id: String,
    /// What the agent is asked to do.
    pub prompt: String,
    /// Shell script preparing the workspace (before the agent runs).
    #[serde(default)]
    pub setup: Option<String>,
    /// Shell script deciding success: exit 0 = solved.
    pub verify: String,
    #[serde(default)]
    pub max_turns: Option<u32>,
    #[serde(default)]
    pub budget_tokens: Option<u64>,
}

/// The committed mini-SWE suite: five tasks covering creation, debugging
/// via test feedback, extraction, transformation, and multi-input synthesis.
pub const MINI_SWE_TOML: &str = include_str!("../../../evals/mini_swe.toml");

pub fn load_suite(toml_text: &str) -> anyhow::Result<Vec<EvalTask>> {
    #[derive(Deserialize)]
    struct Suite {
        #[serde(rename = "task", default)]
        tasks: Vec<EvalTask>,
    }
    let suite: Suite = toml::from_str(toml_text)?;
    Ok(suite.tasks)
}

#[derive(Clone, Debug, Serialize)]
pub struct EvalResult {
    pub id: String,
    pub model: String,
    pub passed: bool,
    pub turns: u32,
    pub tool_calls: u32,
    pub tokens: u64,
    pub duration_ms: u128,
    /// Verify stdout/stderr tail on failure — the debugging breadcrumb.
    pub failure_note: Option<String>,
}

/// Workspace directory name for one (task, model) pair. The model id is
/// reduced to `[A-Za-z0-9._-]` so it is filesystem-safe, and including it
/// keeps models in a `--models a,b` comparison from sharing a workspace.
pub(crate) fn workspace_dir_name(task_id: &str, model: &str) -> String {
    let safe: String = model
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("eval-{task_id}-{safe}")
}

/// Create a clean workspace for one (task, model) run. Any directory left by
/// an earlier run with the same name is removed first, so a run never starts
/// inside a solved workspace from a previous run.
pub(crate) fn prepare_workspace(
    root: &Path,
    task_id: &str,
    model: &str,
) -> std::io::Result<PathBuf> {
    let workspace = root.join(workspace_dir_name(task_id, model));
    match std::fs::symlink_metadata(&workspace) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&workspace)?,
        Ok(_) => std::fs::remove_file(&workspace)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    std::fs::create_dir_all(&workspace)?;
    Ok(workspace)
}

/// Run one eval task end-to-end in a fresh workspace for `model`.
pub async fn run_eval_task(
    engine: &Engine,
    task: &EvalTask,
    model: &str,
    workspaces: &Path,
) -> anyhow::Result<EvalResult> {
    use tokio::process::Command;

    let workspace = prepare_workspace(workspaces, &task.id, model)?;

    let started = Instant::now();
    let mut failure_note = None;

    // Setup (before the agent sees the workspace).
    if let Some(setup) = &task.setup {
        let output = Command::new("bash")
            .arg("-c")
            .arg(setup)
            .current_dir(&workspace)
            .output()
            .await?;
        if !output.status.success() {
            return Ok(EvalResult {
                id: task.id.clone(),
                model: String::new(),
                passed: false,
                turns: 0,
                tool_calls: 0,
                tokens: 0,
                duration_ms: started.elapsed().as_millis(),
                failure_note: Some(format!(
                    "setup failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )),
            });
        }
    }

    // The agent works (full loop, real tools, mutations approved — the
    // verifier is the safety net and the workspace is disposable).
    let config = modron_core::EngineConfig {
        working_dir: workspace.clone(),
        // The child inherits the run's provider/model defaults — otherwise
        // routing falls back to the engine defaults and the eval never
        // reaches the configured provider.
        default_provider: engine.config.default_provider.clone(),
        default_model: engine.config.default_model.clone(),
        approvals: modron_core::ApprovalPolicy {
            mode: modron_core::ApprovalMode::ApproveAll,
            non_interactive: true,
            ..modron_core::ApprovalPolicy::default()
        },
        max_turns: task.max_turns.unwrap_or(16),
        max_total_tokens: task.budget_tokens.unwrap_or(300_000),
        ..modron_core::EngineConfig::default()
    };
    let child_engine = Engine {
        selector: Arc::clone(&engine.selector),
        providers: engine.providers.clone(),
        tools: Arc::clone(&engine.tools),
        store: Arc::clone(&engine.store),
        config,
    };
    // Agent errors (provider failures, budget) count as unresolved, not a
    // crash — the eval must produce a score row either way.
    let summary: RunSummary = match child_engine.run_task(&task.prompt).await {
        Ok(summary) => summary,
        Err(e) => {
            return Ok(EvalResult {
                id: task.id.clone(),
                model: String::new(),
                passed: false,
                turns: 0,
                tool_calls: 0,
                tokens: 0,
                duration_ms: started.elapsed().as_millis(),
                failure_note: Some(format!("agent error: {e}")),
            });
        }
    };

    // Verify (exit 0 = solved) — in the same workspace, after the agent.
    let (passed, verify_tail) = match Command::new("bash")
        .arg("-c")
        .arg(&task.verify)
        .current_dir(&workspace)
        .output()
        .await
    {
        Ok(output) => {
            let tail = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout).trim(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
            (output.status.success(), tail)
        }
        Err(e) => (false, e.to_string()),
    };
    if !passed {
        failure_note = Some(verify_tail);
    }

    let RunSummary {
        turns,
        tool_calls,
        usage,
        ..
    } = summary;
    let _ = PathBuf::from(&workspace); // workspace kept for post-mortem until process exit
    Ok(EvalResult {
        id: task.id.clone(),
        model: String::new(),
        passed,
        turns,
        tool_calls,
        tokens: usage.total(),
        duration_ms: started.elapsed().as_millis(),
        failure_note,
    })
}

/// `modron eval` (Phase 6c): run the suite (or a filter) across a model
/// list, print the SWE-bench-style scorecard, persist to
/// `~/.modron/evals/<ts>.jsonl`.
pub(crate) async fn run_evals(
    cli: &Cli,
    cfg: &config::Config,
    models_arg: Option<&str>,
    filter: Option<&str>,
) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;

    let tasks = load_suite(MINI_SWE_TOML)?;
    let tasks: Vec<_> = match filter {
        Some(f) => tasks.into_iter().filter(|t| t.id.contains(f)).collect(),
        None => tasks,
    };
    if tasks.is_empty() {
        anyhow::bail!("no eval tasks match the filter");
    }
    let models: Vec<String> = match models_arg {
        // Empty CSV segments must not become empty-string model names that
        // only fail later inside provider calls.
        Some(csv) => csv
            .split(',')
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(String::from)
            .collect(),
        None => vec![cfg
            .default_model
            .clone()
            .unwrap_or_else(|| "glm-5.3-flash".to_string())],
    };
    if models.is_empty() {
        anyhow::bail!("--models parsed to an empty list");
    }

    let mcp = crate::spawn_mcp_servers(cfg).await?;
    let stamp = chrono::Utc::now().to_rfc3339().replace([':', '+'], "-");
    let workspaces_root = std::env::temp_dir().join(format!("modron-evals-{stamp}"));
    std::fs::create_dir_all(&workspaces_root)?;
    let mut all: Vec<EvalResult> = Vec::new();

    for model in &models {
        println!("eval: {} tasks on {model}", tasks.len());
        for task in &tasks {
            // Per-model config: force the model, approve mutations, exact
            // task limits.
            let mut model_cli = cli.clone();
            model_cli.model = Some(model.clone());
            model_cli.approve_all = true;
            let agent = crate::build_agent_with(
                &model_cli,
                cfg,
                false,
                AgentSinks::for_mode(crate::RunMode::HEADLESS),
                &mcp,
            )?;
            let mut task = task.clone();
            if task.max_turns.is_none() {
                task.max_turns = Some(16);
            }
            let mut result = run_eval_task(&agent.engine, &task, model, &workspaces_root).await?;
            result.model = model.clone();
            println!(
                "{:<3} {:<24} {:>4} turns {:>3} tools {:>7} tok {:>6} ms",
                if result.passed { "✓" } else { "✗" },
                result.id,
                result.turns,
                result.tool_calls,
                result.tokens,
                result.duration_ms
            );
            if let Some(note) = &result.failure_note {
                if !result.passed {
                    println!("    └ {}", short(note, 110));
                }
            }
            all.push(result);
        }
    }

    // Scorecard per model (mini-SWE: resolved = passed the verifier).
    println!("\n=== eval scorecard (resolved = verifier exit 0) ===");
    for model in &models {
        let cases: Vec<&EvalResult> = all.iter().filter(|r| &r.model == model).collect();
        let resolved = cases.iter().filter(|r| r.passed).count();
        let tokens: u64 = cases.iter().map(|r| r.tokens).sum();
        println!(
            "{model:<20} resolved {resolved}/{} ({:.0}%) · {tokens} tok total",
            cases.len(),
            100.0 * resolved as f64 / cases.len().max(1) as f64
        );
    }

    // Persist (the accumulating eval record).
    if let Some(home) = dirs::home_dir() {
        let dir = home.join(".modron").join("evals");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("{stamp}.jsonl"));
        let mut file = tokio::io::BufWriter::new(tokio::fs::File::create(&path).await?);
        for record in &all {
            let line = serde_json::to_string(record)?;
            file.write_all(line.as_bytes()).await?;
            file.write_all(b"\n").await?;
        }
        file.flush().await?;
        println!("evals: {}", path.display());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use modron_core::testing::{MockProvider, StaticSelector};
    use modron_core::{StopReason, ToolCall, TurnResponse, Usage};

    #[tokio::test]
    async fn eval_harness_scores_a_solved_task() {
        // The mock "solves" the task: writes the file the verifier checks.
        let provider = MockProvider::new()
            .then_response(TurnResponse {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({"path": "answer.txt", "content": "42"}),
                }],
                usage: Usage {
                    input_tokens: 100,
                    output_tokens: 10,
                    cached_input_tokens: 0,
                },
                stop_reason: StopReason::ToolUse,
            })
            .then_response(TurnResponse {
                text: "done".into(),
                tool_calls: Vec::new(),
                usage: Usage {
                    input_tokens: 50,
                    output_tokens: 5,
                    cached_input_tokens: 0,
                },
                stop_reason: StopReason::EndTurn,
            });
        let mut registry = modron_core::ToolRegistry::new();
        registry.register(Arc::new(modron_tools::WriteTool));
        let engine = modron_core::Engine {
            selector: Arc::new(StaticSelector::new("mock-model", "mock")),
            providers: [(
                "mock".to_string(),
                Arc::new(provider) as Arc<dyn modron_core::ModelProvider>,
            )]
            .into_iter()
            .collect(),
            tools: Arc::new(registry),
            store: Arc::new(modron_core::testing::InMemoryStore::default()),
            config: modron_core::EngineConfig::default(),
        };
        let task = EvalTask {
            id: "answer".into(),
            prompt: "write 42 to answer.txt".into(),
            setup: None,
            verify: "grep -q 42 answer.txt".into(),
            max_turns: Some(4),
            budget_tokens: Some(50_000),
        };
        let dir = tempfile::tempdir().unwrap();
        let result = run_eval_task(&engine, &task, "mock-model", dir.path())
            .await
            .unwrap();
        assert!(result.passed, "solved: {:?}", result.failure_note);
    }

    #[tokio::test]
    async fn eval_harness_fails_an_unsolved_task() {
        // Mock says done without doing the work → verifier fails.
        let provider = MockProvider::new().then_text("I did nothing");
        let engine = modron_core::Engine {
            selector: Arc::new(StaticSelector::new("mock-model", "mock")),
            providers: [(
                "mock".to_string(),
                Arc::new(provider) as Arc<dyn modron_core::ModelProvider>,
            )]
            .into_iter()
            .collect(),
            tools: Arc::new(modron_core::ToolRegistry::new()),
            store: Arc::new(modron_core::testing::InMemoryStore::default()),
            config: modron_core::EngineConfig::default(),
        };
        let task = EvalTask {
            id: "answer".into(),
            prompt: "write 42 to answer.txt".into(),
            setup: None,
            verify: "test -f answer.txt".into(),
            max_turns: Some(2),
            budget_tokens: Some(50_000),
        };
        let dir = tempfile::tempdir().unwrap();
        let result = run_eval_task(&engine, &task, "mock-model", dir.path())
            .await
            .unwrap();
        assert!(!result.passed, "uncompleted work must fail the verifier");
        assert!(result.failure_note.is_some());
    }

    #[test]
    fn workspace_is_per_model_and_starts_clean() {
        let root = tempfile::tempdir().unwrap();
        let a = prepare_workspace(root.path(), "answer", "model-a").unwrap();
        std::fs::write(a.join("answer.txt"), "42").unwrap();

        // Model b must not see model a's solved workspace.
        let b = prepare_workspace(root.path(), "answer", "model-b").unwrap();
        assert_ne!(a, b, "each model gets its own workspace path");
        assert!(!b.join("answer.txt").exists(), "model b starts clean");

        // A re-run for model a removes the stale file from its earlier run.
        let a_again = prepare_workspace(root.path(), "answer", "model-a").unwrap();
        assert_eq!(a, a_again);
        assert!(!a_again.join("answer.txt").exists(), "stale file removed");
        assert!(a_again.is_dir());
    }

    #[test]
    fn workspace_name_sanitizes_the_model_id() {
        assert_eq!(
            workspace_dir_name("answer", "vendor/model:v1.2 beta"),
            "eval-answer-vendor_model_v1.2_beta"
        );
        assert_eq!(workspace_dir_name("t", "a-b_c.d"), "eval-t-a-b_c.d");
    }

    #[test]
    fn suite_loads_from_committed_toml() {
        let tasks = load_suite(MINI_SWE_TOML).unwrap();
        assert!(tasks.len() >= 5, "mini-SWE suite: {}", tasks.len());
        assert!(tasks.iter().all(|t| !t.verify.is_empty()));
        assert!(tasks.iter().all(|t| !t.prompt.is_empty()));
    }
}
