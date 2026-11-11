//! `grep` tool (LEDGER T1) — case-insensitive regex search over text files.

use crate::common::{arg_str, internal_tool_error, resolve_path, tool_err, walk_builder};
use crate::reduce::reduce_output;
use crate::validate::validate_args;
use async_trait::async_trait;
use modron_core::{Result, Tool, ToolCtx, ToolOutcome, ToolSpec};
use regex::{Regex, RegexBuilder};
use serde_json::{json, Value};
use std::path::PathBuf;

/// Maximum number of matches returned per call.
pub const MAX_MATCHES: usize = 50;
/// Per-line display cap (in chars) before truncation.
pub const MAX_LINE_CHARS: usize = 240;
const MAX_BYTES: usize = 48 * 1024;

/// Case-insensitive regex search across text files. Honors `.gitignore`,
/// skips hidden entries, vendored/build dirs and binary (NUL-containing)
/// files. Output is `path:lineno: line`, capped at 50 matches. No matches is
/// an error outcome (expected failure), matching the frozen `Tool` contract.
pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "grep".into(),
            description: "Case-insensitive regex search over text files under path (default \
                          `.`). Respects .gitignore, skips binary files. Output \
                          `path:lineno: line`, capped at 50 matches."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Rust regex, matched case-insensitively."},
                    "path": {"type": "string", "description": "File or dir to search (default `.`)."}
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        const NAME: &str = "grep";
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
        let re = match RegexBuilder::new(pattern)
            .case_insensitive(true)
            .size_limit(1 << 20)
            .build()
        {
            Ok(re) => re,
            Err(e) => {
                return Ok(tool_err(
                    NAME,
                    format!("invalid regex `{pattern}`: {e}"),
                    MAX_BYTES,
                ))
            }
        };
        let root = resolve_path(&ctx.working_dir, arg_str(&args, "path").unwrap_or("."));
        if !root.exists() {
            return Ok(tool_err(
                NAME,
                format!("path not found: {}", root.display()),
                MAX_BYTES,
            ));
        }

        // Display paths relative to the working dir, not the search root.
        let display_root = ctx.working_dir.clone();
        let matches = tokio::task::spawn_blocking(move || collect_matches(root, display_root, re))
            .await
            .map_err(|e| internal_tool_error(NAME, format!("walk failed: {e}")))?;

        if matches.is_empty() {
            return Ok(tool_err(NAME, "no matches", MAX_BYTES));
        }
        let truncated = matches.len() > MAX_MATCHES;
        let mut body = matches
            .into_iter()
            .take(MAX_MATCHES)
            .collect::<Vec<_>>()
            .join("\n");
        if truncated {
            body.push_str("\n… [more matches elided]");
        }
        Ok(ToolOutcome::ok(reduce_output(body, MAX_BYTES)))
    }
}

fn collect_matches(root: PathBuf, display_root: PathBuf, re: Regex) -> Vec<String> {
    let mut out = Vec::new();
    'outer: for entry in walk_builder(&root).build() {
        let Ok(entry) = entry else { continue };
        if entry.file_type().is_none_or(|ft| ft.is_dir()) {
            continue;
        }
        let path = entry.path().to_path_buf();
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if bytes.contains(&0) {
            continue; // binary — skip entirely
        }
        let text = String::from_utf8_lossy(&bytes);
        let rel = path
            .strip_prefix(&display_root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        for (idx, line) in text.lines().enumerate() {
            if re.is_match(line) {
                out.push(format!("{}:{}: {}", rel, idx + 1, truncate_line(line)));
                // One past the cap is enough to know truncation happened.
                if out.len() > MAX_MATCHES {
                    break 'outer;
                }
            }
        }
    }
    out
}

fn truncate_line(line: &str) -> String {
    let mut s: String = line.chars().take(MAX_LINE_CHARS).collect();
    if line.chars().count() > MAX_LINE_CHARS {
        s.push('…');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    async fn run(pattern: &str, path: Option<&str>, dir: &tempfile::TempDir) -> ToolOutcome {
        let ctx = ToolCtx {
            engine: None,
            sandbox: modron_core::SandboxPolicy::Off,
            working_dir: dir.path().to_path_buf(),
            session_id: None,
        };
        let mut args = json!({"pattern": pattern});
        if let Some(p) = path {
            args["path"] = json!(p);
        }
        GrepTool.execute(args, &ctx).await.expect("execute")
    }

    async fn fixture() -> tempfile::TempDir {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::create_dir_all(dir.path().join(".hidden")).unwrap();
        std::fs::write(dir.path().join("f1.txt"), "hello world\nRUST ROCKS\nbye\n").unwrap();
        std::fs::write(dir.path().join("sub/f3.txt"), "hello again\n").unwrap();
        std::fs::write(dir.path().join(".hidden/x.txt"), "hello hidden\n").unwrap();
        std::fs::write(dir.path().join("f2.txt"), "nothing here\n").unwrap();
        std::fs::write(dir.path().join("bin.dat"), b"hello\x00binary").unwrap();
        dir
    }

    #[tokio::test]
    async fn finds_matches_case_insensitively() {
        let dir = fixture().await;
        let out = run("rust", None, &dir).await;
        assert!(!out.is_error, "{out:?}");
        assert!(out.content.contains("f1.txt:2: RUST ROCKS"), "{out:?}");
    }

    #[tokio::test]
    async fn searches_recursively_and_skips_hidden_and_binary() {
        let dir = fixture().await;
        let out = run("hello", None, &dir).await;
        assert!(!out.is_error, "{out:?}");
        assert!(out.content.contains("f1.txt:1: hello world"), "{out:?}");
        assert!(out.content.contains("sub/f3.txt:1: hello again"), "{out:?}");
        assert!(!out.content.contains(".hidden"), "hidden leaked: {out:?}");
        assert!(!out.content.contains("bin.dat"), "binary leaked: {out:?}");
    }

    #[tokio::test]
    async fn scoped_path_search() {
        let dir = fixture().await;
        let out = run("hello", Some("sub"), &dir).await;
        assert!(!out.is_error, "{out:?}");
        assert!(out.content.contains("sub/f3.txt:1"), "{out:?}");
        assert!(!out.content.contains("f1.txt"), "{out:?}");
    }

    #[tokio::test]
    async fn no_matches_is_error_outcome() {
        let dir = fixture().await;
        let out = run("zzz-not-there", None, &dir).await;
        assert!(out.is_error, "{out:?}");
        assert!(out.content.contains("no matches"), "{out:?}");
    }

    #[tokio::test]
    async fn invalid_regex_is_error_outcome() {
        let dir = fixture().await;
        let out = run("(unclosed", None, &dir).await;
        assert!(out.is_error, "{out:?}");
        assert!(out.content.contains("invalid regex"), "{out:?}");
    }

    #[tokio::test]
    async fn caps_at_50_matches_with_marker() {
        let dir = tempdir().unwrap();
        let body: String = (0..120).map(|i| format!("needle {i}\n")).collect();
        std::fs::write(dir.path().join("many.txt"), body).unwrap();
        let out = run("needle", None, &dir).await;
        assert!(!out.is_error, "{out:?}");
        let lines: usize = out.content.lines().count();
        assert_eq!(lines, 51, "50 matches + 1 marker line: {out:?}");
        assert!(out.content.contains("more matches elided"), "{out:?}");
        assert!(out.content.contains("many.txt:50:"), "{out:?}");
    }

    #[tokio::test]
    async fn long_lines_are_truncated() {
        let dir = tempdir().unwrap();
        let long = format!("needle {}\n", "y".repeat(500));
        std::fs::write(dir.path().join("long.txt"), long).unwrap();
        let out = run("needle", None, &dir).await;
        assert!(!out.is_error, "{out:?}");
        let line = out.content.lines().next().unwrap_or_default();
        // "long.txt:1: " prefix + 240 content chars + ellipsis.
        let expected = "long.txt:1: ".chars().count() + MAX_LINE_CHARS + 1;
        assert_eq!(line.chars().count(), expected, "{line}");
        assert!(line.ends_with('…'), "{line}");
    }
}
