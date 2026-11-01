//! `tool_search` tool (Phase 13 dynamic discovery) — find and surface tools
//! on demand so the system prompt stays lean.
//!
//! MCP servers and other heavy tool sources register as usual but are
//! marked on-demand (hidden from the prompt). The model discovers them
//! through this tool: a keyword query returns matching tool names,
//! descriptions, and parameter shapes — enough to call them correctly
//! without paying their schema weight on every single turn.

use crate::common::{arg_str, tool_err};
use crate::reduce::reduce_output;
use crate::validate::validate_args;
use async_trait::async_trait;
use modron_core::{Result, Tool, ToolCtx, ToolOutcome, ToolSpec};
use serde_json::{json, Value};

const REDUCE_CAP: usize = 4 * 1024;
const DEFAULT_LIMIT: usize = 5;
const MAX_LIMIT: usize = 10;

/// Search every registered tool (including on-demand ones).
pub struct ToolSearchTool;

#[async_trait]
impl Tool for ToolSearchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "tool_search".into(),
            description: "Find available tools by keyword — including tools hidden from this \
                          prompt (MCP servers, skills). Args: query (keywords), optional limit. \
                          Returns each tool's name, description and parameters so you can call \
                          it directly. Search before assuming a capability is missing."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Keywords to match against tool names and descriptions."},
                    "limit": {"type": "integer", "description": "Max results (1-10, default 5)."}
                },
                "required": ["query"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        const NAME: &str = "tool_search";
        let spec = self.spec();
        if let Err(message) = validate_args(&spec, &args) {
            return Ok(tool_err(
                NAME,
                format!("invalid arguments: {message}"),
                REDUCE_CAP,
            ));
        }
        let Some(query) = arg_str(&args, "query") else {
            return Ok(tool_err(NAME, "missing `query`", REDUCE_CAP));
        };
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|v| v.clamp(1, MAX_LIMIT as u64) as usize)
            .unwrap_or(DEFAULT_LIMIT);
        let Some(engine) = &ctx.engine else {
            return Ok(tool_err(
                NAME,
                "no engine attached — tool discovery needs the registry",
                REDUCE_CAP,
            ));
        };

        let tokens: Vec<String> = query
            .split(|c: char| c.is_whitespace() || c == '-' || c == '_')
            .filter(|t| !t.is_empty())
            .map(|t| t.to_lowercase())
            .collect();
        if tokens.is_empty() {
            return Ok(tool_err(NAME, "`query` must not be empty", REDUCE_CAP));
        }

        let mut lines: Vec<(u32, String)> = Vec::new();
        for tool in engine.tools.specs() {
            let on_demand = engine.tools.is_on_demand(&tool.name);
            let hay = format!("{} {}", tool.name, tool.description).to_lowercase();
            let score = tokens
                .iter()
                .filter(|t| hay.contains(t.as_str()))
                .count() as u32;
            if score == 0 {
                continue;
            }
            let params: Vec<String> = tool
                .parameters
                .get("properties")
                .and_then(|p| p.as_object())
                .map(|props| props.keys().cloned().collect())
                .unwrap_or_default();
            lines.push((
                score,
                format!(
                    "- {}{}: {} [params: {}]",
                    tool.name,
                    if on_demand { " (on-demand)" } else { "" },
                    first_line(&tool.description),
                    if params.is_empty() { "none".to_string() } else { params.join(", ") },
                ),
            ));
        }

        if lines.is_empty() {
            return Ok(ToolOutcome::ok(reduce_output(
                format!(
                    "no tools matched {query:?} — the registry has {} tools",
                    engine.tools.specs().len()
                ),
                REDUCE_CAP,
            )));
        }
        lines.sort_by(|a, b| b.0.cmp(&a.0));
        let rendered = lines
            .into_iter()
            .take(limit)
            .map(|(_, l)| l)
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolOutcome::ok(reduce_output(rendered, REDUCE_CAP)))
    }
}

fn first_line(description: &str) -> String {
    description
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use modron_core::testing::{MockProvider, StaticSelector};
    use modron_core::{Engine, EngineConfig, SessionStore, ToolRegistry};
    use std::collections::HashMap;
    use std::sync::Arc;

    fn engine_with_tools() -> Engine {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(ToolSearchTool));
        registry.register(Arc::new(FakeMcp {
            name: "mcp__github__create_issue",
            description: "Open a GitHub issue in a repository.",
        }));
        registry.register(Arc::new(FakeMcp {
            name: "mcp__slack__post",
            description: "Post a message to a Slack channel.",
        }));
        registry.register(Arc::new(FakeMcp {
            name: "read",
            description: "Read a file from disk.",
        }));
        registry.mark_on_demand("mcp__github__create_issue");
        registry.mark_on_demand("mcp__slack__post");

        let mut providers: HashMap<String, Arc<dyn modron_core::ModelProvider>> =
            HashMap::new();
        providers.insert(
            "mock".into(),
            Arc::new(MockProvider::new()),
        );
        let store: Arc<dyn SessionStore> = Arc::new(modron_core::testing::InMemoryStore::default());
        Engine {
            selector: Arc::new(StaticSelector::new("mock-model", "mock")),
            providers,
            tools: Arc::new(registry),
            store,
            config: EngineConfig::default(),
        }
    }

    struct FakeMcp {
        name: &'static str,
        description: &'static str,
    }

    #[async_trait]
    impl Tool for FakeMcp {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.name.into(),
                description: self.description.into(),
                parameters: json!({"type": "object", "properties": {"repo": {"type": "string"}}}),
            }
        }
        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolCtx,
        ) -> Result<ToolOutcome> {
            Ok(ToolOutcome::ok("unused"))
        }
    }

    async fn run(args: Value, engine: &Engine) -> ToolOutcome {
        let ctx = ToolCtx {
            engine: Some(Arc::new(engine.clone())),
            sandbox: modron_core::SandboxPolicy::Off,
            working_dir: std::env::temp_dir(),
        };
        ToolSearchTool.execute(args, &ctx).await.expect("execute")
    }

    #[tokio::test]
    async fn finds_on_demand_tools_the_prompt_hides() {
        let engine = engine_with_tools();
        // The prompt must NOT carry the on-demand tools…
        let prompt_specs = engine.tools.specs_prompt();
        assert!(prompt_specs.iter().all(|s| s.name != "mcp__github__create_issue"));
        assert!(prompt_specs.iter().all(|s| s.name != "mcp__slack__post"));
        // …but they stay callable…
        assert!(engine.tools.get("mcp__github__create_issue").is_some());
        // …and searchable.
        let out = run(json!({"query": "github issue"}), &engine).await;
        assert!(!out.is_error, "{out:?}");
        assert!(
            out.content.contains("mcp__github__create_issue (on-demand)"),
            "{out:?}"
        );
        assert!(out.content.contains("params: repo"), "{out:?}");
        assert!(!out.content.contains("slack"), "{out:?}");
    }

    #[tokio::test]
    async fn core_tools_are_searchable_too() {
        let engine = engine_with_tools();
        let out = run(json!({"query": "read file"}), &engine).await;
        assert!(out.content.contains("- read:"), "{out:?}");
    }

    #[tokio::test]
    async fn no_match_is_clean() {
        let engine = engine_with_tools();
        let out = run(json!({"query": "zzz"}), &engine).await;
        assert!(!out.is_error && out.content.contains("no tools matched"), "{out:?}");
    }
}
