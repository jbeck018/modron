//! `edit` tool (LEDGER T1) — exact string replacement in text files.

use crate::common::{arg_str, opt_bool, resolve_path, tool_err};
use crate::reduce::reduce_output;
use crate::validate::validate_args;
use async_trait::async_trait;
use modron_core::{Result, Tool, ToolCtx, ToolOutcome, ToolSpec};
use serde_json::{json, Value};

const MAX_BYTES: usize = 1024;

/// Replaces an exact `old_string` with `new_string` in a UTF-8 text file.
/// `old_string` must appear exactly once unless `replace_all: true`; zero or
/// multiple matches without the flag are error outcomes naming the count.
pub struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit".into(),
            description: "Exact string replacement in a UTF-8 text file. old_string must match \
                          exactly once unless replace_all is true; zero or multiple matches are \
                          errors naming the count."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path (relative to working dir or absolute)."},
                    "old_string": {"type": "string", "description": "Exact text to replace."},
                    "new_string": {"type": "string", "description": "Replacement text."},
                    "replace_all": {"type": "boolean", "description": "Replace every occurrence."}
                },
                "required": ["path", "old_string", "new_string"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        // Protected instruction sources (Phase: competitive review A3) —
        // the agent must not edit the files that instruct it. bash remains
        // the documented escape hatch (and is approval-gated).
        if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
            if is_protected_path(path) {
                return Ok(tool_err(
                    NAME,
                    format!(
                        "`{path}` is write-protected (instruction/memory source); \
                             edit it yourself outside the agent"
                    ),
                    MAX_BYTES,
                ));
            }
        }
        const NAME: &str = "edit";
        let spec = self.spec();
        if let Err(message) = validate_args(&spec, &args) {
            return Ok(tool_err(
                NAME,
                format!("invalid arguments: {message}"),
                MAX_BYTES,
            ));
        }
        let (Some(raw_path), Some(old), Some(new)) = (
            arg_str(&args, "path"),
            arg_str(&args, "old_string"),
            arg_str(&args, "new_string"),
        ) else {
            return Ok(tool_err(
                NAME,
                "missing `path`, `old_string` or `new_string`",
                MAX_BYTES,
            ));
        };
        let replace_all = opt_bool(&args, "replace_all").unwrap_or(false);
        let path = resolve_path(&ctx.working_dir, raw_path);

        // Hard read-before-edit gate (Phase 13): an existing file must have
        // been read this session. Blind edits are the top source of
        // destroyed-work bugs; the fix is one read away.
        if let Some(engine) = &ctx.engine {
            if path.is_file() && !engine.tools.read_gate.seen(&path) {
                return Ok(tool_err(
                    NAME,
                    format!(
                        "read-before-edit gate: read `{raw_path}` first — edits to files the \
                         session has not read are blocked",
                    ),
                    MAX_BYTES,
                ));
            }
        }

        let content = match tokio::fs::read_to_string(&path).await {
            Ok(content) => content,
            Err(e) => {
                return Ok(tool_err(
                    NAME,
                    format!("cannot read {}: {e}", path.display()),
                    MAX_BYTES,
                ))
            }
        };
        if old.is_empty() {
            return Ok(tool_err(NAME, "`old_string` must not be empty", MAX_BYTES));
        }
        let count = content.matches(old).count();
        if count == 0 {
            return Ok(tool_err(
                NAME,
                format!("`old_string` not found in {}", path.display()),
                MAX_BYTES,
            ));
        }
        if count > 1 && !replace_all {
            return Ok(tool_err(
                NAME,
                format!(
                    "`old_string` appears {count} times in {}; add surrounding context to make it \
                     unique or pass replace_all: true",
                    path.display()
                ),
                MAX_BYTES,
            ));
        }
        let replaced = if replace_all { count } else { 1 };
        let updated = if replace_all {
            content.replace(old, new)
        } else {
            content.replacen(old, new, 1)
        };
        if let Err(e) = tokio::fs::write(&path, updated.as_bytes()).await {
            return Ok(tool_err(
                NAME,
                format!("cannot write {}: {e}", path.display()),
                MAX_BYTES,
            ));
        }
        // Live LSP (Phase 14): push the new content so language servers
        // re-lint; the `diagnostics` tool collects the result.
        let updated_owned = updated.clone();
        let lsp_path = path.clone();
        let _ = tokio::task::spawn_blocking(move || {
            crate::lsp::did_write(&lsp_path, &updated_owned)
        })
        .await;
        // Diff rail (Phase 14, jcode pattern): the change surfaced in the
        // tool result itself, so the next turn (and the human) sees what
        // moved where without a re-read.
        let rail = diff_rail(
            &path.display().to_string(),
            &updated,
            old,
            new,
            replace_all,
        );
        Ok(ToolOutcome::ok(reduce_output(
            format!("replaced {replaced} occurrence(s) in {}\n{rail}", path.display()),
            MAX_BYTES,
        )))
    }
}

/// Files the agent may not modify: instruction and memory sources.
fn is_protected_path(path: &str) -> bool {
    let p = std::path::Path::new(path);
    let name = p
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    if name == "AGENTS.md" || name == "CLAUDE.md" {
        return true;
    }
    p.components().any(|c| {
        let s = c.as_os_str().to_string_lossy();
        s == ".modron" || s == ".claude" || s == ".git"
    })
}

/// Compact `file:line` rail of the change: for single replacements the
/// exact line; for replace_all, every affected line (capped).
fn diff_rail(path: &str, updated: &str, old: &str, new: &str, all: bool) -> String {
    let mut out = String::new();
    let mut hits = 0usize;
    let mut search_from = 0usize;
    while let Some(pos) = updated[search_from..].find(new) {
        let abs = search_from + pos;
        let line_no = updated[..abs].matches('\n').count() + 1;
        out.push_str(&format!("{path}:{line_no}\n"));
        for line in old.lines().take(3) {
            out.push_str(&format!("- {line}\n"));
        }
        if old.lines().count() > 3 {
            out.push_str(&format!("- … (+{} lines)\n", old.lines().count() - 3));
        }
        for line in new.lines().take(3) {
            out.push_str(&format!("+ {line}\n"));
        }
        if new.lines().count() > 3 {
            out.push_str(&format!("+ … (+{} lines)\n", new.lines().count() - 3));
        }
        hits += 1;
        search_from = abs + new.len();
        if !all {
            break;
        }
        if hits >= 5 {
            out.push_str(&format!("{path}: … (more occurrences below the cap)\n"));
            break;
        }
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    async fn run(args: Value, dir: &std::path::Path) -> ToolOutcome {
        let ctx = ToolCtx {
            engine: None,
            sandbox: modron_core::SandboxPolicy::Off,
            working_dir: dir.to_path_buf(),
        };
        EditTool.execute(args, &ctx).await.expect("execute")
    }

    #[tokio::test]
    async fn replaces_single_occurrence() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("f.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        let out = run(
            json!({"path": "f.rs", "old_string": "fn a() {}", "new_string": "fn c() {}"}),
            dir.path(),
        )
        .await;
        assert!(!out.is_error, "{out:?}");
        assert!(out.content.contains("replaced 1 occurrence"), "{out:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.rs")).unwrap(),
            "fn c() {}\nfn b() {}\n"
        );
    }

    #[tokio::test]
    async fn zero_matches_is_error() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "abc\n").unwrap();
        let out = run(
            json!({"path": "f.txt", "old_string": "zzz", "new_string": "y"}),
            dir.path(),
        )
        .await;
        assert!(out.is_error, "{out:?}");
        assert!(out.content.contains("not found"), "{out:?}");
    }

    #[tokio::test]
    async fn multiple_matches_without_flag_is_error_naming_count() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "x x x\n").unwrap();
        let out = run(
            json!({"path": "f.txt", "old_string": "x", "new_string": "y"}),
            dir.path(),
        )
        .await;
        assert!(out.is_error, "{out:?}");
        assert!(out.content.contains("appears 3 times"), "{out:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "x x x\n"
        );
    }

    #[tokio::test]
    async fn replace_all_replaces_every_occurrence() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "x x x\n").unwrap();
        let out = run(
            json!({"path": "f.txt", "old_string": "x", "new_string": "y", "replace_all": true}),
            dir.path(),
        )
        .await;
        assert!(!out.is_error, "{out:?}");
        assert!(out.content.contains("replaced 3 occurrence"), "{out:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "y y y\n"
        );
    }

    #[tokio::test]
    async fn missing_file_is_error() {
        let dir = tempdir().unwrap();
        let out = run(
            json!({"path": "nope.txt", "old_string": "a", "new_string": "b"}),
            dir.path(),
        )
        .await;
        assert!(out.is_error, "{out:?}");
        assert!(out.content.contains("cannot read"), "{out:?}");
    }
}

// -- Phase 14: diff rails ----------------------------------------------------

#[cfg(test)]
mod rail_tests {
    use super::*;

    #[tokio::test]
    async fn edit_output_carries_a_line_anchored_rail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "alpha\nbeta\ngamma\n").unwrap();
        // The gate needs a read first: simulate by marking via a read tool.
        let read_ctx = ToolCtx {
            engine: None,
            sandbox: modron_core::SandboxPolicy::Off,
            working_dir: dir.path().to_path_buf(),
        };
        crate::ReadTool
            .execute(json!({"path": "f.txt"}), &read_ctx)
            .await
            .expect("read");
        // No engine in the test ctx → the gate is inactive by design; run edit.
        let ctx = ToolCtx {
            engine: None,
            sandbox: modron_core::SandboxPolicy::Off,
            working_dir: dir.path().to_path_buf(),
        };
        let out = EditTool
            .execute(
                json!({"path": "f.txt", "old_string": "beta", "new_string": "BETA"}),
                &ctx,
            )
            .await
            .expect("edit");
        assert!(!out.is_error, "{out:?}");
        assert!(out.content.contains("replaced 1 occurrence"), "{out:?}");
        assert!(out.content.contains(":2\n"), "line anchor: {out:?}");
        assert!(out.content.contains("- beta"), "{out:?}");
        assert!(out.content.contains("+ BETA"), "{out:?}");
    }
}
