//! The `plan` tool (2026-11-11, the measurable-subtask primitive): the
//! model decomposes a goal into subtasks with measurable done criteria
//! and keeps statuses current as the run progresses. Each call writes a
//! [`SessionEvent::Plan`] to the session log (a full replacement — the
//! plan is a value, not a diff), so:
//! - every client renders the plan from the replay (checkbox rows),
//! - the context builder re-injects the latest plan each turn (the
//!   stays-on-track mechanism),
//! - the log becomes the evidence trail for post-run review.
//!
//! Requires `ctx.session_id` (the engine supplies it); without a session
//! the tool is a no-op error, which keeps unit tests honest.

use crate::common::tool_err;
use crate::validate::validate_args;
use modron_core::{SessionEvent, Tool, ToolCtx, ToolOutcome, ToolSpec};

const NAME: &str = "plan";
const MAX_BYTES: usize = 8 * 1024;

pub struct PlanTool;

impl Default for PlanTool {
    fn default() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl Tool for PlanTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: NAME.to_string(),
            description: "Maintain the run's standing plan: decompose the goal into subtasks \
                          with MEASURABLE done criteria and keep their statuses current. Call \
                          `set` first for any multi-step goal (one call replaces the whole \
                          plan), then `update` as work completes. Statuses: pending / active \
                          / done / blocked. The plan is re-shown to you every turn and is \
                          rendered to the user; done criteria must be objectively checkable."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "op": {
                        "type": "string",
                        "enum": ["set", "update"],
                        "description": "`set` replaces the whole plan; `update` changes one item's status."
                    },
                    "items": {
                        "type": "array",
                        "description": "`set` only: the full plan, in order.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "text": {"type": "string", "description": "The subtask, one line, imperative."},
                                "done_criteria": {"type": "string", "description": "The measurable done criterion (tests pass, endpoint returns 200, diff applies clean)."},
                                "status": {"type": "string", "enum": ["pending", "active", "done", "blocked"]}
                            },
                            "required": ["text", "done_criteria"]
                        }
                    },
                    "index": {"type": "integer", "description": "`update` only: the 0-based item to change."},
                    "status": {"type": "string", "enum": ["pending", "active", "done", "blocked"], "description": "`update` only: the new status."}
                },
                "required": ["op"]
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
        let op = args
            .get("op")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let Some(session_id) = ctx.session_id else {
            return Ok(tool_err(
                NAME,
                "no session in context - the plan tool runs inside a session only",
                MAX_BYTES,
            ));
        };
        let Some(store) = ctx.engine.as_ref().map(|e| e.store.clone()) else {
            return Ok(tool_err(NAME, "no engine handle", MAX_BYTES));
        };

        match op.as_str() {
            "set" => {
                let Some(items) = args.get("items").and_then(|v| v.as_array()) else {
                    return Ok(tool_err(NAME, "set needs items", MAX_BYTES));
                };
                if items.is_empty() {
                    return Ok(tool_err(NAME, "set needs at least one item", MAX_BYTES));
                }
                let plan: Vec<modron_core::PlanItem> = items
                    .iter()
                    .map(|item| modron_core::PlanItem {
                        text: item
                            .get("text")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        done_criteria: item
                            .get("done_criteria")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        status: item
                            .get("status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("pending")
                            .to_string(),
                    })
                    .collect();
                let count = plan.len();
                store
                    .append(session_id, &SessionEvent::Plan { items: plan })
                    .await
                    .map_err(|e| modron_core::Error::Session(e.to_string()))?;
                Ok(ToolOutcome::ok(reduce(
                    format!("plan set: {count} subtasks written"),
                    MAX_BYTES,
                )))
            }
            "update" => {
                let index = args
                    .get("index")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| modron_core::Error::Session("update needs index".into()))?;
                let status = args
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                if !matches!(status.as_str(), "pending" | "active" | "done" | "blocked") {
                    return Ok(tool_err(
                        NAME,
                        "status must be pending | active | done | blocked",
                        MAX_BYTES,
                    ));
                }
                // Read the latest plan from the log (the plan is a value;
                // the update writes the new full state).
                let events = store
                    .load(session_id)
                    .await
                    .map_err(|e| modron_core::Error::Session(e.to_string()))?;
                let mut plan = events
                    .iter()
                    .rev()
                    .find_map(|event| match event {
                        SessionEvent::Plan { items } => Some(items.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                let Some(item) = plan.get_mut(index as usize) else {
                    return Ok(tool_err(
                        NAME,
                        format!("no plan item at index {index} (set the plan first)"),
                        MAX_BYTES,
                    ));
                };
                item.status = status.clone();
                let count = plan.len();
                store
                    .append(session_id, &SessionEvent::Plan { items: plan })
                    .await
                    .map_err(|e| modron_core::Error::Session(e.to_string()))?;
                Ok(ToolOutcome::ok(reduce(
                    format!("plan item {index} -> {status} ({count} subtasks)"),
                    MAX_BYTES,
                )))
            }
            other => Ok(tool_err(
                NAME,
                format!("unknown op {other} (set | update)"),
                MAX_BYTES,
            )),
        }
    }
}

/// Cap the outcome text (same discipline as the other tools).
fn reduce(text: String, max: usize) -> String {
    crate::reduce::reduce_output(text, max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use modron_core::testing::StaticSelector;
    use modron_core::testing::{InMemoryStore, MockProvider};
    use modron_core::{SessionId, SessionStore};
    use std::collections::HashMap;
    use std::sync::Arc;

    /// A real engine whose store is the in-memory one, so the tool's
    /// read/append path is exactly the production path.
    fn engine_with_store(store: Arc<InMemoryStore>) -> Arc<modron_core::Engine> {
        let mut providers: HashMap<String, Arc<dyn modron_core::ModelProvider>> = HashMap::new();
        providers.insert("mock".into(), Arc::new(MockProvider::new()));
        Arc::new(modron_core::Engine {
            selector: Arc::new(StaticSelector::new("mock-model", "mock")),
            providers,
            tools: Arc::new(modron_core::ToolRegistry::new()),
            store,
            config: modron_core::EngineConfig::default(),
        })
    }

    fn ctx_with(engine: &Arc<modron_core::Engine>, session: SessionId) -> ToolCtx {
        ToolCtx {
            working_dir: std::path::PathBuf::from("."),
            sandbox: modron_core::SandboxPolicy::Off,
            engine: Some(Arc::clone(engine)),
            session_id: Some(session),
        }
    }

    #[tokio::test]
    async fn set_then_update_round_trips() {
        let store = Arc::new(InMemoryStore::default());
        let session = store.create_session("plan test").await.expect("session").id;
        let engine = engine_with_store(store.clone());
        let tool = PlanTool;
        let ctx = ctx_with(&engine, session);

        let outcome = tool
            .execute(
                serde_json::json!({"op": "set", "items": [
                    {"text": "add the plan tool", "done_criteria": "tool registered"},
                    {"text": "wire the tests", "done_criteria": "tests green"}
                ]}),
                &ctx,
            )
            .await
            .expect("set runs");
        assert!(!outcome.is_error);

        let outcome = tool
            .execute(
                serde_json::json!({"op": "update", "index": 1, "status": "done"}),
                &ctx,
            )
            .await
            .expect("update runs");
        assert!(!outcome.is_error);

        let events = store.load(session).await.expect("load");
        let plans: Vec<&modron_core::PlanItem> = events
            .iter()
            .filter_map(|e| match e {
                SessionEvent::Plan { items } => Some(items),
                _ => None,
            })
            .next_back()
            .map(|items| items.iter().collect())
            .unwrap_or_default();
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[1].status, "done");
        assert_eq!(plans[0].status, "pending");
        // The tool outcome meta carries nothing; the plan lives in the log.
        assert!(outcome.content.contains("-> done"));
    }

    #[tokio::test]
    async fn update_without_a_plan_is_a_tool_error() {
        let store = Arc::new(InMemoryStore::default());
        let session = store
            .create_session("plan empty")
            .await
            .expect("session")
            .id;
        let engine = engine_with_store(store.clone());
        let tool = PlanTool;
        let ctx = ctx_with(&engine, session);
        let outcome = tool
            .execute(
                serde_json::json!({"op": "update", "index": 0, "status": "done"}),
                &ctx,
            )
            .await
            .expect("runs");
        assert!(outcome.is_error);
    }
}
