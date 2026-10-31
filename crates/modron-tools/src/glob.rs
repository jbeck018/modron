//! `glob` tool (LEDGER T1) — gitignore-aware file listing.

use crate::common::{arg_str, gitignore_matcher, tool_err, walk_builder};
use crate::reduce::reduce_output;
use crate::validate::validate_args;
use async_trait::async_trait;
use ignore::gitignore::Gitignore;
use modron_core::{Result, Tool, ToolCtx, ToolOutcome, ToolSpec};
use serde_json::{json, Value};
use std::path::PathBuf;

/// Maximum number of paths returned per call.
pub const MAX_ENTRIES: usize = 100;
const MAX_BYTES: usize = 48 * 1024;

/// Lists file paths (relative to the working dir) matching a gitignore-style
/// glob pattern, e.g. `src/**/*.rs`. Honors `.gitignore` (even outside git
/// repositories), skips hidden entries and `target`/`node_modules`/`vendor`
/// directories. Results are capped at 100 paths with an elision marker.
pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "glob".into(),
            description: "List files matching a gitignore-style glob (e.g. `src/**/*.rs`) under \
                          the working dir. Respects .gitignore; skips hidden and \
                          target/node_modules/vendor dirs. Capped at 100 paths."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Glob pattern in gitignore syntax."}
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        const NAME: &str = "glob";
        let spec = self.spec();
        if let Err(message) = validate_args(&spec, &args) {
            return Ok(tool_err(
                NAME,
                format!("invalid arguments: {message}"),
                MAX_BYTES,
            ));
        }
        let Some(pattern) = arg_str(&args, "pattern") else {
            return Ok(tool_err(NAME, "missing `pattern`", MAX_BYTES));
        };
        let root = ctx.working_dir.clone();
        if !root.exists() {
            return Ok(tool_err(NAME, "working dir does not exist", MAX_BYTES));
        }
        let matcher = match gitignore_matcher(&root, pattern) {
            Ok(matcher) => matcher,
            Err(message) => return Ok(tool_err(NAME, message, MAX_BYTES)),
        };

        // The walk is sync/blocking; park it on the blocking pool (see rustdoc).
        let walk_root = root.clone();
        let mut paths = tokio::task::spawn_blocking(move || collect_matches(walk_root, matcher))
            .await
            .map_err(|e| crate::common::internal_tool_error(NAME, format!("walk failed: {e}")))?;
        paths.sort();

        if paths.is_empty() {
            return Ok(tool_err(
                NAME,
                format!("no files match `{pattern}`"),
                MAX_BYTES,
            ));
        }
        let truncated = paths.len() > MAX_ENTRIES;
        paths.truncate(MAX_ENTRIES);
        let mut body = paths.join("\n");
        if truncated {
            body.push_str("\n… [more entries elided]");
        }
        Ok(ToolOutcome::ok(reduce_output(body, MAX_BYTES)))
    }
}

fn collect_matches(root: PathBuf, matcher: Gitignore) -> Vec<String> {
    let mut out = Vec::new();
    for entry in walk_builder(&root).build() {
        let Ok(entry) = entry else { continue };
        let is_dir = entry.file_type().is_some_and(|ft| ft.is_dir());
        if is_dir || !matcher.matched(entry.path(), is_dir).is_ignore() {
            continue;
        }
        let rel = entry.path().strip_prefix(&root).unwrap_or(entry.path());
        out.push(rel.to_string_lossy().into_owned());
        // One past the cap is enough to know truncation happened.
        if out.len() > MAX_ENTRIES {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    async fn run(pattern: &str, dir: &tempfile::TempDir) -> ToolOutcome {
        let ctx = ToolCtx {
            engine: None,
            sandbox: modron_core::SandboxPolicy::Off,
            working_dir: dir.path().to_path_buf(),
        };
        GlobTool
            .execute(json!({"pattern": pattern}), &ctx)
            .await
            .expect("execute")
    }

    async fn fixture() -> tempfile::TempDir {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/deep")).unwrap();
        std::fs::create_dir_all(dir.path().join(".hidden")).unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "").unwrap();
        std::fs::write(dir.path().join("src/deep/b.rs"), "").unwrap();
        std::fs::write(dir.path().join("README.md"), "").unwrap();
        std::fs::write(dir.path().join("ignored.rs"), "").unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.rs\n").unwrap();
        std::fs::write(dir.path().join(".hidden/x.rs"), "").unwrap();
        std::fs::write(dir.path().join("target/c.rs"), "").unwrap();
        dir
    }

    #[tokio::test]
    async fn matches_respecting_gitignore_and_hidden() {
        let dir = fixture().await;
        let out = run("*.rs", &dir).await;
        assert!(!out.is_error, "{out:?}");
        assert!(out.content.contains("src/a.rs"), "{out:?}");
        assert!(out.content.contains("src/deep/b.rs"), "{out:?}");
        assert!(
            !out.content.contains("ignored.rs"),
            "gitignored file leaked: {out:?}"
        );
        assert!(
            !out.content.contains(".hidden"),
            "hidden file leaked: {out:?}"
        );
        assert!(
            !out.content.contains("target/c.rs"),
            "vendored/build dir leaked: {out:?}"
        );
        assert!(!out.content.contains("README.md"));
    }

    #[tokio::test]
    async fn anchored_pattern_scopes_match() {
        let dir = fixture().await;
        let out = run("src/*.rs", &dir).await;
        assert!(!out.is_error, "{out:?}");
        assert!(out.content.contains("src/a.rs"), "{out:?}");
        assert!(!out.content.contains("src/deep/b.rs"), "{out:?}");
    }

    #[tokio::test]
    async fn no_matches_is_error_outcome() {
        let dir = fixture().await;
        let out = run("*.zig", &dir).await;
        assert!(out.is_error, "{out:?}");
        assert!(out.content.contains("no files match"), "{out:?}");
    }

    #[tokio::test]
    async fn malformed_pattern_is_literal_and_yields_error_outcome() {
        // Gitignore semantics: an unclosed `[` is a literal name, so this is
        // a valid-but-hopeless pattern; no matches is an error outcome.
        let dir = fixture().await;
        let out = run("[unclosed", &dir).await;
        assert!(out.is_error, "{out:?}");
        assert!(out.content.contains("no files match"), "{out:?}");
    }

    #[tokio::test]
    async fn caps_at_100_entries_with_marker() {
        let dir = tempdir().unwrap();
        for i in 0..250 {
            std::fs::write(dir.path().join(format!("f{i:04}.txt")), "").unwrap();
        }
        let out = run("*.txt", &dir).await;
        assert!(!out.is_error, "{out:?}");
        let lines: usize = out.content.lines().count();
        assert_eq!(lines, 101, "100 entries + 1 marker line: {out:?}");
        assert!(out.content.contains("more entries elided"), "{out:?}");
    }
}
