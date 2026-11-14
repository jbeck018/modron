//! `memory_search` tool (Phase 13) — token-scored search over all memory
//! tiers: project facts, global facts, and project breadcrumbs.
//!
//! The hot tier is injected into the system prompt, but the breadcrumb tier
//! exists only to be searched — this tool is its single surface. Every fact
//! it serves is recorded as a hit in the usefulness ledger
//! ([`MemoryStore::record_hit`]) and reported through
//! [`ToolOutcome::meta`] as `memory_hits`, which the engine folds back into
//! the ledger as a success/failure signal when the run completes.

use crate::common::{arg_str, tool_err};
use crate::reduce::reduce_output;
use crate::validate::validate_args;
use async_trait::async_trait;
use modron_core::memory::MemoryStore;
use modron_core::{Result, Tool, ToolCtx, ToolOutcome, ToolSpec};
use serde_json::{json, Value};
use std::path::PathBuf;

/// Cap for this tool's rendered results.
const REDUCE_CAP: usize = 4 * 1024;

/// Default number of results when the model does not say.
const DEFAULT_LIMIT: usize = 5;
const MAX_LIMIT: usize = 10;

/// Search all memory tiers: project facts, global preferences, breadcrumbs.
pub struct MemorySearchTool {
    memory_root: PathBuf,
}

impl MemorySearchTool {
    pub fn new(memory_root: impl Into<PathBuf>) -> Self {
        Self {
            memory_root: memory_root.into(),
        }
    }
}

#[async_trait]
impl Tool for MemorySearchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "memory_search".into(),
            description: "Search remembered facts, global preferences, and trace breadcrumbs \
                          from earlier sessions. Args: query (space-separated keywords), \
                          optional limit (default 5). Use before re-deriving project facts \
                          or preferences the user may already have taught modron."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Keywords to match."},
                    "limit": {"type": "integer", "description": "Max results (1-10, default 5)."}
                },
                "required": ["query"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        const NAME: &str = "memory_search";
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
        if query.trim().is_empty() {
            return Ok(tool_err(NAME, "`query` must not be empty", REDUCE_CAP));
        }

        let tokens = tokenize(query);
        if tokens.is_empty() {
            return Ok(ToolOutcome::ok(reduce_output(
                "no usable keywords in query",
                REDUCE_CAP,
            )));
        }

        let project = match MemoryStore::for_workspace(&self.memory_root, &ctx.working_dir) {
            Ok(store) => store,
            Err(err) => {
                return Ok(tool_err(
                    NAME,
                    format!("cannot open memory: {err}"),
                    REDUCE_CAP,
                ))
            }
        };
        let global = MemoryStore::for_global(&self.memory_root).ok();

        let mut project_hits: Vec<String> = Vec::new();
        let mut global_hits: Vec<String> = Vec::new();
        let mut lines: Vec<(u32, String)> = Vec::new();

        // Project facts (hot tier).
        for (id, label, target) in project.facts_oldest_first() {
            let body = std::fs::read_to_string(project.root().join(&target)).unwrap_or_default();
            let hay = format!("{label} {body}").to_lowercase();
            let score = tokens.iter().filter(|t| hay.contains(t.as_str())).count() as u32;
            if score > 0 {
                lines.push((
                    score,
                    format!("- [fact] {label}: {}", first_sentence(&body)),
                ));
                project_hits.push(id);
            }
        }
        // Global preferences.
        if let Some(store) = &global {
            for (id, label, target) in store.facts_oldest_first() {
                let body = std::fs::read_to_string(store.root().join(&target)).unwrap_or_default();
                let hay = format!("{label} {body}").to_lowercase();
                let score = tokens.iter().filter(|t| hay.contains(t.as_str())).count() as u32;
                if score > 0 {
                    lines.push((
                        score,
                        format!("- [global] {label}: {}", first_sentence(&body)),
                    ));
                    global_hits.push(id);
                }
            }
        }
        // Breadcrumbs (trace tier; no ledger — TTL-pruned only).
        for crumb in project.search_breadcrumbs(query, limit) {
            let hay = format!("{} {}", crumb.text, crumb.tags.join(" ")).to_lowercase();
            let score = tokens.iter().filter(|t| hay.contains(t.as_str())).count() as u32;
            lines.push((score, format!("- [trace] {}: {}", crumb.id, crumb.text)));
        }

        if lines.is_empty() {
            return Ok(ToolOutcome::ok(reduce_output(
                "no memories matched — nothing previously remembered covers this",
                REDUCE_CAP,
            )));
        }
        lines.sort_by_key(|a| std::cmp::Reverse(a.0));
        let rendered: String = lines
            .into_iter()
            .take(limit)
            .map(|(_, l)| l)
            .collect::<Vec<_>>()
            .join("\n");

        // Every served fact id is a ledger hit (recorded immediately, against
        // its own store) and a success candidate (the engine folds the run
        // outcome into the project ledger at completion).
        for id in &project_hits {
            project.record_hit(id);
        }
        if let Some(store) = &global {
            for id in &global_hits {
                store.record_hit(id);
            }
        }

        let mut outcome = ToolOutcome::ok(reduce_output(rendered, REDUCE_CAP));
        outcome.meta = Some(json!({
            "memory_hits": project_hits,
            "global_memory_hits": global_hits,
        }));
        Ok(outcome)
    }
}

/// Lowercased keyword tokens, punctuation/whitespace split.
fn tokenize(query: &str) -> Vec<String> {
    query
        .split(|c: char| c.is_whitespace() || c == '-')
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect()
}

/// First sentence of a body, single line, capped at ~160 chars.
fn first_sentence(body: &str) -> String {
    let body = body.trim();
    let sentence_end = body.find(". ").map(|i| i + 1).unwrap_or(body.len());
    let mut out: String = body[..sentence_end].to_string();
    if out.chars().count() > 160 {
        out = out.chars().take(160).collect();
        out.push('…');
    }
    out.replace('\n', " ")
}

#[cfg(test)]
mod snippet_char_tests {
    use super::first_sentence;

    #[test]
    fn sentence_cut_is_char_safe_after_multibyte_text() {
        assert_eq!(first_sentence("ééééé. Second sentence here."), "ééééé.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use modron_core::SandboxPolicy;
    use tempfile::tempdir;

    async fn run(args: Value, root: &std::path::Path, work: &std::path::Path) -> ToolOutcome {
        let ctx = ToolCtx {
            engine: None,
            sandbox: SandboxPolicy::Off,
            working_dir: work.to_path_buf(),
            session_id: None,
        };
        MemorySearchTool::new(root)
            .execute(args, &ctx)
            .await
            .expect("execute")
    }

    #[tokio::test]
    async fn finds_project_facts_and_records_hits() {
        let root = tempdir().unwrap();
        let work = tempdir().unwrap();
        let store = MemoryStore::for_workspace(root.path(), work.path()).unwrap();
        store
            .append("rust version", "pinned to 1.99.1 for seatbelt tests")
            .unwrap();
        store
            .append("unrelated", "a completely different topic")
            .unwrap();

        let out = run(json!({"query": "rust version"}), root.path(), work.path()).await;
        assert!(!out.is_error, "{out:?}");
        assert!(out.content.contains("[fact]"), "{out:?}");
        assert!(out.content.contains("rust version"), "{out:?}");
        assert!(!out.content.contains("unrelated"), "{out:?}");
        let ids = out
            .meta
            .as_ref()
            .and_then(|m| m.get("memory_hits"))
            .and_then(|v| v.as_array())
            .expect("meta.memory_hits");
        assert_eq!(ids.len(), 1, "{ids:?}");

        // The hit landed in the ledger.
        let stats = store.stats();
        let stat = stats.values().next().unwrap();
        assert_eq!(stat.uses, 1);
    }

    #[tokio::test]
    async fn searches_breadcrumbs_and_global() {
        let root = tempdir().unwrap();
        let work = tempdir().unwrap();
        let store = MemoryStore::for_workspace(root.path(), work.path()).unwrap();
        store
            .append_breadcrumb("fixed flaky test in gateway module", &["tests".into()])
            .unwrap();
        let global = MemoryStore::for_global(root.path()).unwrap();
        global
            .append("editor", "user prefers helix, never vim")
            .unwrap();

        let out = run(json!({"query": "gateway flaky"}), root.path(), work.path()).await;
        assert!(out.content.contains("[trace]"), "{out:?}");
        assert!(out.content.contains("gateway"), "{out:?}");

        let out = run(json!({"query": "helix editor"}), root.path(), work.path()).await;
        assert!(out.content.contains("[global]"), "{out:?}");
        assert!(out.content.contains("helix"), "{out:?}");
    }

    #[tokio::test]
    async fn no_match_is_a_clean_answer() {
        let root = tempdir().unwrap();
        let work = tempdir().unwrap();
        let out = run(json!({"query": "zzz nothing"}), root.path(), work.path()).await;
        assert!(!out.is_error, "{out:?}");
        assert!(out.content.contains("no memories matched"), "{out:?}");
        assert!(out.meta.is_none(), "{out:?}");
    }

    #[tokio::test]
    async fn limit_is_clamped() {
        let root = tempdir().unwrap();
        let work = tempdir().unwrap();
        let out = run(
            json!({"query": "x", "limit": 999}),
            root.path(),
            work.path(),
        )
        .await;
        assert!(!out.is_error, "{out:?}");
        // Empty store: clean no-match despite the silly limit.
        assert!(out.content.contains("no memories matched"), "{out:?}");
    }

    #[test]
    fn spec_description_is_under_60_words() {
        let words = MemorySearchTool::new("/tmp/modron-test-memory")
            .spec()
            .description;
        let count = words.split_whitespace().count();
        assert!(count < 60, "spec description is {count} words: {words}");
    }
}
