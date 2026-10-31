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
                    "budget_tokens": {"type": "integer", "description": "Max tokens (default 30000, max 200000)."}
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

        match modron_core::subagent::delegate(engine, task, persona, tool_filter.as_deref(), budget)
            .await
        {
            Ok(report) => {
                let body = reduce_output(
                    format!(
                        "{}\n\n(delegate: {} tok, {} turns)",
                        report.brief,
                        report.usage.total(),
                        report.turns
                    ),
                    MAX_BYTES,
                );
                Ok(ToolOutcome {
                    content: body,
                    is_error: report.brief.is_empty(),
                    usage: Some(report.usage),
                })
            }
            Err(e) => Err(internal_tool_error(NAME, e.to_string())),
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
