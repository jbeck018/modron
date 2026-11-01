//! The `explore` tool (Phase 4): spawns a read-only subagent (cheap tier,
//! own token budget, own session) that answers a question about the codebase
//! and returns a brief. The parent's context only sees the brief — the
//! RLM-style economy from PLAN §4.7.
//!
//! The engine handle arrives via `ToolCtx::engine` (set by the engine at
//! dispatch); without it (unit tests) the tool returns an honest error.

use crate::common::{internal_tool_error, tool_err};
use crate::reduce::reduce_output;
use crate::validate::validate_args;
use modron_core::{Tool, ToolCtx, ToolOutcome, ToolSpec};

const NAME: &str = "explore";
const MAX_BYTES: usize = 10 * 1024;
const DEFAULT_BUDGET_TOKENS: u64 = 20_000;
const MAX_BUDGET_TOKENS: u64 = 100_000;

pub struct ExploreTool;

impl Default for ExploreTool {
    fn default() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl Tool for ExploreTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: NAME.to_string(),
            description: "Spawn a read-only subagent (cheap tier, its own token budget) that \
                          searches the codebase and returns a brief with file:line citations. \
                          Use for repo-wide or multi-file questions instead of burning main-loop \
                          context on raw listings."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "task": {"type": "string", "description": "The question to answer."},
                    "budget_tokens": {"type": "integer", "description": "Max tokens for the subagent (default 20000, max 100000)."}
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

        let Some(engine) = ctx.engine.as_ref() else {
            return Ok(tool_err(
                NAME,
                "no engine attached to this context — explore needs a running agent",
                MAX_BYTES,
            ));
        };

        match modron_core::subagent::explore(engine, task, budget).await {
            Ok(report) => {
                let body = reduce_output(
                    format!(
                        "{}\n\n(explore: {} tok, {} turns)",
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
                    meta: None,
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
        let outcome = ExploreTool
            .execute(
                serde_json::json!({"task": "where is X?"}),
                &ToolCtx::new("."),
            )
            .await
            .unwrap();
        assert!(outcome.is_error);
        assert!(outcome.content.contains("no engine attached"));
    }
}
