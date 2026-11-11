//! `remember` tool (LEDGER Phase 1a M2) — persists durable facts for future
//! sessions in the same workspace (PLAN.md §4.9 user tier).
//!
//! Design choice: the memory *root* is injected at construction and the
//! workspace slug is derived from `ToolCtx.working_dir` at execution time
//! (not from a pre-built store), so the entry always lands in the same
//! workspace `context::load_user_memory` reads for the run. The CLI wires it
//! either via [`crate::default_registry_with_memory`] or by registering
//! `RememberTool::new(memory_root)` on a custom registry.
//!
//! Expected failures (IO problems saving the memory) are `ToolOutcome::err`
//! so the model can report them; house pattern: output goes through the
//! reduction choke point, and tiny local writes are allowed to block-in-async.

use crate::common::{arg_str, tool_err};
use crate::reduce::reduce_output;
use crate::validate::validate_args;
use async_trait::async_trait;
use modron_core::memory::MemoryStore;
use modron_core::{Result, Tool, ToolCtx, ToolOutcome, ToolSpec};
use serde_json::{json, Value};
use std::path::PathBuf;

/// Output cap for this tool's own messages (success/error), not entry bodies.
const REDUCE_CAP: usize = 1024;

/// Saves a durable fact into the workspace memory store.
pub struct RememberTool {
    memory_root: PathBuf,
}

impl RememberTool {
    pub fn new(memory_root: impl Into<PathBuf>) -> Self {
        Self {
            memory_root: memory_root.into(),
        }
    }
}

#[async_trait]
impl Tool for RememberTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "remember".into(),
            description: "Persist a durable fact for future sessions in this workspace. Args: \
                          title (short label) and body (the fact, plain text). Saved to the \
                          workspace memory store; appears in the system prompt of later sessions. \
                          Use for stable preferences and project facts, not transient state."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string", "description": "Short label for the fact."},
                    "body": {"type": "string", "description": "The fact itself, plain text."},
                    "scope": {"type": "string", "enum": ["project", "global"],
                              "description": "project (default): this workspace. global: durable \
                              user preference for ALL projects."},
                    "kind": {"type": "string", "enum": ["fact", "breadcrumb"],
                              "description": "fact (default): injected hot memory. breadcrumb: a \
                              searchable trace item, never injected — use for observations and \
                              dead ends worth finding later, not preferences."},
                    "tags": {"type": "array", "items": {"type": "string"},
                              "description": "Breadcrumb tags (ignored for facts)."}
                },
                "required": ["title", "body"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        const NAME: &str = "remember";
        let spec = self.spec();
        if let Err(message) = validate_args(&spec, &args) {
            return Ok(tool_err(
                NAME,
                format!("invalid arguments: {message}"),
                REDUCE_CAP,
            ));
        }
        let Some(title) = arg_str(&args, "title") else {
            return Ok(tool_err(NAME, "missing `title`", REDUCE_CAP));
        };
        let Some(body) = arg_str(&args, "body") else {
            return Ok(tool_err(NAME, "missing `body`", REDUCE_CAP));
        };
        if title.trim().is_empty() {
            return Ok(tool_err(NAME, "`title` must not be empty", REDUCE_CAP));
        }
        if body.trim().is_empty() {
            return Ok(tool_err(NAME, "`body` must not be empty", REDUCE_CAP));
        }

        let scope = arg_str(&args, "scope").unwrap_or("project");
        let kind = arg_str(&args, "kind").unwrap_or("fact");
        let tags: Vec<String> = args
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|t| t.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        if !matches!(scope, "project" | "global") {
            return Ok(tool_err(
                NAME,
                format!("`scope` must be \"project\" or \"global\", got {scope:?}"),
                REDUCE_CAP,
            ));
        }
        if !matches!(kind, "fact" | "breadcrumb") {
            return Ok(tool_err(
                NAME,
                format!("`kind` must be \"fact\" or \"breadcrumb\", got {kind:?}"),
                REDUCE_CAP,
            ));
        }

        let store = match MemoryStore::for_workspace(&self.memory_root, &ctx.working_dir) {
            Ok(store) => store,
            Err(err) => {
                return Ok(tool_err(
                    NAME,
                    format!("cannot save memory: {err}"),
                    REDUCE_CAP,
                ))
            }
        };
        let store = if scope == "global" {
            match MemoryStore::for_global(&self.memory_root) {
                Ok(store) => store,
                Err(err) => {
                    return Ok(tool_err(
                        NAME,
                        format!("cannot save global memory: {err}"),
                        REDUCE_CAP,
                    ))
                }
            }
        } else {
            store
        };
        match kind {
            "breadcrumb" => {
                let mut crumb_tags = tags;
                crumb_tags.push(title.to_string());
                match store.append_breadcrumb(body, &crumb_tags) {
                    Ok(crumb) => Ok(ToolOutcome::ok(reduce_output(
                        format!(
                            "remembered breadcrumb {} ({scope}) — searchable via memory_search, \
                             never injected into the prompt",
                            crumb.id
                        ),
                        REDUCE_CAP,
                    ))),
                    Err(err) => Ok(tool_err(
                        NAME,
                        format!("cannot save memory: {err}"),
                        REDUCE_CAP,
                    )),
                }
            }
            _ => match store.append(title, body) {
                Ok(entry) => Ok(ToolOutcome::ok(reduce_output(
                    format!(
                        "remembered \"{}\" ({scope} fact) — saved to {} (injected into future \
                         sessions until the curator prunes it)",
                        entry.title,
                        entry.path.display()
                    ),
                    REDUCE_CAP,
                ))),
                Err(err) => Ok(tool_err(
                    NAME,
                    format!("cannot save memory: {err}"),
                    REDUCE_CAP,
                )),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    async fn run(args: Value, root: &std::path::Path, work: &std::path::Path) -> ToolOutcome {
        let ctx = ToolCtx {
            engine: None,
            sandbox: modron_core::SandboxPolicy::Off,
            working_dir: work.to_path_buf(),
            session_id: None,
        };
        RememberTool::new(root)
            .execute(args, &ctx)
            .await
            .expect("execute")
    }

    #[tokio::test]
    async fn writes_entry_into_injected_root() {
        let root = tempdir().unwrap();
        let work = tempdir().unwrap();
        let out = run(
            json!({"title": "rust version", "body": "pinned to 1.99.1"}),
            root.path(),
            work.path(),
        )
        .await;
        assert!(!out.is_error, "{out:?}");
        assert!(out.content.contains("remembered"), "{out:?}");
        assert!(out.content.contains("saved to"), "{out:?}");

        // Visible through the store: index line, body on disk.
        let store = MemoryStore::for_workspace(root.path(), work.path()).unwrap();
        let index = store.read_index();
        assert!(index.contains("rust version"), "{index}");
        let body = store.read_entry("rust version").unwrap().unwrap();
        assert_eq!(body.trim(), "pinned to 1.99.1");
    }

    #[tokio::test]
    async fn invalid_and_missing_args_are_error_outcomes() {
        let root = tempdir().unwrap();
        let work = tempdir().unwrap();
        // Missing required args.
        let out = run(json!({"title": "t"}), root.path(), work.path()).await;
        assert!(
            out.is_error && out.content.contains("missing required argument `body`"),
            "{out:?}"
        );
        let out = run(json!({}), root.path(), work.path()).await;
        assert!(out.is_error, "{out:?}");
        // Unknown key.
        let out = run(
            json!({"title": "t", "body": "b", "extra": 1}),
            root.path(),
            work.path(),
        )
        .await;
        assert!(
            out.is_error && out.content.contains("unknown argument `extra`"),
            "{out:?}"
        );
        // Wrong type.
        let out = run(json!({"title": 1, "body": "b"}), root.path(), work.path()).await;
        assert!(
            out.is_error && out.content.contains("`title` must be of type string"),
            "{out:?}"
        );
        // Empty strings are rejected, not written.
        let out = run(
            json!({"title": "  ", "body": "b"}),
            root.path(),
            work.path(),
        )
        .await;
        assert!(
            out.is_error && out.content.contains("`title` must not be empty"),
            "{out:?}"
        );
        let out = run(json!({"title": "t", "body": " "}), root.path(), work.path()).await;
        assert!(
            out.is_error && out.content.contains("`body` must not be empty"),
            "{out:?}"
        );
    }

    #[tokio::test]
    async fn separate_workspaces_get_separate_memory() {
        let root = tempdir().unwrap();
        let work_a = tempdir().unwrap();
        let work_b = tempdir().unwrap();
        run(
            json!({"title": "fact", "body": "for a"}),
            root.path(),
            work_a.path(),
        )
        .await;
        run(
            json!({"title": "fact", "body": "for b"}),
            root.path(),
            work_b.path(),
        )
        .await;
        let store_a = MemoryStore::for_workspace(root.path(), work_a.path()).unwrap();
        let store_b = MemoryStore::for_workspace(root.path(), work_b.path()).unwrap();
        assert_ne!(store_a.slug(), store_b.slug());
        assert_eq!(store_a.read_entry("fact").unwrap().unwrap().trim(), "for a");
        assert_eq!(store_b.read_entry("fact").unwrap().unwrap().trim(), "for b");
    }

    #[tokio::test]
    async fn global_scope_writes_the_cross_project_tier() {
        let root = tempdir().unwrap();
        let work = tempdir().unwrap();
        let out = run(
            json!({"title": "editor", "body": "prefers helix", "scope": "global"}),
            root.path(),
            work.path(),
        )
        .await;
        assert!(!out.is_error, "{out:?}");
        let global = MemoryStore::for_global(root.path()).unwrap();
        assert_eq!(
            global.read_entry("editor").unwrap().unwrap().trim(),
            "prefers helix"
        );
        // The project tier stays clean.
        let project = MemoryStore::for_workspace(root.path(), work.path()).unwrap();
        assert!(project.read_entry("editor").unwrap().is_none());
    }

    #[tokio::test]
    async fn breadcrumb_kind_never_touches_the_fact_index() {
        let root = tempdir().unwrap();
        let work = tempdir().unwrap();
        let out = run(
            json!({
                "title": "dead end",
                "body": "retrying flaky gateway test 3x did not help",
                "kind": "breadcrumb",
                "tags": ["tests"]
            }),
            root.path(),
            work.path(),
        )
        .await;
        assert!(!out.is_error, "{out:?}");
        assert!(out.content.contains("breadcrumb"), "{out:?}");
        let store = MemoryStore::for_workspace(root.path(), work.path()).unwrap();
        assert_eq!(store.read_index(), "", "no fact injected");
        let crumbs = store.breadcrumbs();
        assert_eq!(crumbs.len(), 1);
        assert!(crumbs[0].text.contains("gateway"));
        assert!(crumbs[0].tags.contains(&"tests".to_string()));
        assert!(
            crumbs[0].tags.contains(&"dead end".to_string()),
            "title becomes a tag"
        );
        // And it is searchable.
        assert!(!store.search_breadcrumbs("gateway", 3).is_empty());
    }

    #[tokio::test]
    async fn bad_scope_and_kind_are_rejected() {
        let root = tempdir().unwrap();
        let work = tempdir().unwrap();
        let out = run(
            json!({"title": "t", "body": "b", "scope": "universal"}),
            root.path(),
            work.path(),
        )
        .await;
        assert!(out.is_error && out.content.contains("scope"), "{out:?}");
        let out = run(
            json!({"title": "t", "body": "b", "kind": "rumor"}),
            root.path(),
            work.path(),
        )
        .await;
        assert!(out.is_error && out.content.contains("kind"), "{out:?}");
    }

    #[test]
    fn spec_description_is_under_60_words() {
        let words = RememberTool::new("/tmp/modron-test-memory")
            .spec()
            .description;
        let count = words.split_whitespace().count();
        assert!(count < 60, "spec description is {count} words: {words}");
    }
}
