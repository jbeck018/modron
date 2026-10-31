//! Phase 6c — the eval harness: a mini SWE-bench for the harness itself.
//!
//! Each eval task = (prompt, optional setup script, verify script). The task
//! runs the FULL engine (real model + tools) in a fresh temp workspace:
//! setup prepares it, the agent works, then `verify` runs — exit 0 = solved.
//! Completion alone is not success; the objective verifier decides. That is
//! the difference between "the model said done" and "the work is done".

use crate::{config, short, Cli};
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

/// Run one eval task end-to-end in a fresh temp workspace.
pub async fn run_eval_task(
    engine: &Engine,
    task: &EvalTask,
    workspaces: &Path,
) -> anyhow::Result<EvalResult> {
    use tokio::process::Command;

    let workspace = workspaces.join(format!("eval-{}", task.id));
    std::fs::create_dir_all(&workspace)?;

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
        let result = run_eval_task(&engine, &task, dir.path()).await.unwrap();
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
        let result = run_eval_task(&engine, &task, dir.path()).await.unwrap();
        assert!(!result.passed, "uncompleted work must fail the verifier");
        assert!(result.failure_note.is_some());
    }

    #[test]
    fn suite_loads_from_committed_toml() {
        let tasks = load_suite(MINI_SWE_TOML).unwrap();
        assert!(tasks.len() >= 5, "mini-SWE suite: {}", tasks.len());
        assert!(tasks.iter().all(|t| !t.verify.is_empty()));
        assert!(tasks.iter().all(|t| !t.prompt.is_empty()));
    }
}
