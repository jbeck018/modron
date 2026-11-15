//! `edit_lines` tool — replace a line range verified by per-line hashes.
//!
//! Companion to `read` with `line_hashes: true`. Each line is addressed by its
//! 1-based number plus a 12-bit hash of its content, so the edit applies only
//! when every line in the range still matches what the model read. Any drift
//! refuses the whole edit and leaves the file untouched.

use crate::common::{arg_str, check_write_path, opt_usize, resolve_path, tool_err};
use crate::reduce::reduce_output;
use crate::validate::validate_args;
use async_trait::async_trait;
use modron_core::{Result, Tool, ToolCtx, ToolOutcome, ToolSpec};
use serde_json::{json, Value};

const MAX_BYTES: usize = 1024;
/// Characters of a drifted line shown in the refusal.
const DRIFT_TEXT_CHARS: usize = 120;

const FNV_OFFSET_BASIS: u32 = 0x811c_9dc5;
const FNV_PRIME: u32 = 0x0100_0193;

/// Hash of one line's content, rendered as exactly three lowercase hex chars.
///
/// FNV-1a 32-bit over the line bytes with trailing spaces, tabs and `\r`
/// removed, folded to 12 bits. Trailing whitespace therefore never changes the
/// hash, so editors that strip it do not invalidate anchors.
pub fn line_hash(line: &str) -> String {
    let trimmed = line.trim_end_matches([' ', '\t', '\r']);
    let mut h = FNV_OFFSET_BASIS;
    for byte in trimmed.bytes() {
        h ^= u32::from(byte);
        h = h.wrapping_mul(FNV_PRIME);
    }
    let folded = (h ^ (h >> 12) ^ (h >> 24)) & 0xfff;
    format!("{folded:03x}")
}

/// Text of one physical line, without its `\n` or `\r\n` terminator. A final
/// unterminated piece is returned unchanged.
fn line_text(piece: &str) -> &str {
    match piece.strip_suffix('\n') {
        Some(body) => body.strip_suffix('\r').unwrap_or(body),
        None => piece,
    }
}

/// Line ending of the file: CRLF when its first terminator is `\r\n`, LF
/// otherwise (including files with no terminator at all).
fn detect_eol(content: &str) -> &'static str {
    match content.find('\n') {
        Some(i) if i > 0 && content.as_bytes()[i - 1] == b'\r' => "\r\n",
        _ => "\n",
    }
}

/// Replaces an inclusive 1-based line range, verified by per-line hashes
/// from `read(line_hashes: true)`. Refuses the edit when any line in the range
/// has drifted since it was read.
pub struct EditLinesTool;

#[async_trait]
impl Tool for EditLinesTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit_lines".into(),
            description:
                "Replace a line range verified by per-line hashes from read(line_hashes=true). \
                          Rejects the edit if the file changed."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path (relative to working dir or absolute)."},
                    "start_line": {"type": "integer", "description": "1-based first line to replace."},
                    "end_line": {"type": "integer", "description": "Last line to replace (inclusive)."},
                    "expected_hashes": {"type": "array", "items": {"type": "string"}, "description": "One hash per line in the range, as shown by read(line_hashes=true)."},
                    "new_text": {"type": "string", "description": "Replacement for the whole range; empty deletes it."}
                },
                "required": ["path", "start_line", "end_line", "expected_hashes", "new_text"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        const NAME: &str = "edit_lines";
        // Protected instruction sources and workspace containment, exactly as
        // `edit` applies them (see `common::check_write_path`).
        let resolved = match args.get("path").and_then(|v| v.as_str()) {
            Some(raw) => match check_write_path(&ctx.working_dir, raw) {
                Ok(path) => Some(path),
                Err(message) => return Ok(tool_err(NAME, message, MAX_BYTES)),
            },
            None => None,
        };
        let spec = self.spec();
        if let Err(message) = validate_args(&spec, &args) {
            return Ok(tool_err(
                NAME,
                format!("invalid arguments: {message}"),
                MAX_BYTES,
            ));
        }
        let (Some(raw_path), Some(path)) = (arg_str(&args, "path"), resolved) else {
            return Ok(tool_err(NAME, "missing `path`", MAX_BYTES));
        };
        let (Some(start), Some(end), Some(new_text)) = (
            opt_usize(&args, "start_line"),
            opt_usize(&args, "end_line"),
            arg_str(&args, "new_text"),
        ) else {
            return Ok(tool_err(
                NAME,
                "missing `start_line`, `end_line` or `new_text`",
                MAX_BYTES,
            ));
        };
        let Some(hashes) = args.get("expected_hashes").and_then(Value::as_array) else {
            return Ok(tool_err(NAME, "missing `expected_hashes`", MAX_BYTES));
        };
        let Some(expected) = hashes
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<&str>>>()
        else {
            return Ok(tool_err(
                NAME,
                "`expected_hashes` must be an array of strings",
                MAX_BYTES,
            ));
        };

        // Read-before-edit gate (Phase 13), as in `edit`.
        if let Some(engine) = &ctx.engine {
            let gate_key = resolve_path(&ctx.working_dir, raw_path);
            if gate_key.is_file() && !engine.tools.read_gate.seen_in(ctx.session_id, &gate_key) {
                return Ok(tool_err(
                    NAME,
                    format!(
                        "read-before-edit gate: read `{raw_path}` first — edits to files the \
                         session has not read are blocked"
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

        let pieces: Vec<&str> = content.split_inclusive('\n').collect();
        let total = pieces.len();
        if start == 0 || end < start {
            return Ok(tool_err(
                NAME,
                format!("invalid range {start}..={end}: start_line must be >= 1 and end_line >= start_line"),
                MAX_BYTES,
            ));
        }
        if end > total {
            return Ok(tool_err(
                NAME,
                format!(
                    "range {start}..={end} is out of range: {} has {total} line(s)",
                    path.display()
                ),
                MAX_BYTES,
            ));
        }
        let count = end - start + 1;
        if expected.len() != count {
            return Ok(tool_err(
                NAME,
                format!(
                    "`expected_hashes` has {} entr{} for {count} line(s) in {start}..={end}; \
                     pass exactly one hash per line",
                    expected.len(),
                    if expected.len() == 1 { "y" } else { "ies" }
                ),
                MAX_BYTES,
            ));
        }

        // Verify every line in the range before touching the file.
        let mut drift = Vec::new();
        for (offset, piece) in pieces[start - 1..end].iter().enumerate() {
            let text = line_text(piece);
            let found = line_hash(text);
            if found != expected[offset] {
                let shown: String = text.chars().take(DRIFT_TEXT_CHARS).collect();
                drift.push(format!(
                    "line {}: expected {}, found {found}: {shown}",
                    start + offset,
                    expected[offset]
                ));
            }
        }
        if !drift.is_empty() {
            return Ok(tool_err(
                NAME,
                format!(
                    "hash mismatch in {}: the file changed since it was read; no change made\n{}\n\
                     re-read the file with line_hashes=true",
                    path.display(),
                    drift.join("\n")
                ),
                MAX_BYTES,
            ));
        }

        // Rebuild the file: original prefix and suffix keep their terminators,
        // the new lines take the file's line ending.
        let eol = detect_eol(&content);
        let had_final_newline = content.ends_with('\n');
        let body = new_text.replace("\r\n", "\n");
        let mut new_lines: Vec<&str> = if body.is_empty() {
            Vec::new()
        } else {
            body.split('\n').collect()
        };
        if body.ends_with('\n') {
            new_lines.pop();
        }
        let mut updated = String::with_capacity(content.len() + new_text.len());
        for piece in &pieces[..start - 1] {
            updated.push_str(piece);
        }
        for line in &new_lines {
            updated.push_str(line);
            updated.push_str(eol);
        }
        for piece in &pieces[end..] {
            updated.push_str(piece);
        }
        if !had_final_newline && updated.ends_with(eol) {
            updated.truncate(updated.len() - eol.len());
        }

        if let Err(e) = crate::common::write_atomic(&path, updated.as_bytes()).await {
            return Ok(tool_err(
                NAME,
                format!("cannot write {}: {e}", path.display()),
                MAX_BYTES,
            ));
        }
        // Live LSP (Phase 14), as in `edit`: skipped while the sandbox is on.
        if matches!(ctx.sandbox, modron_core::SandboxPolicy::Off) {
            let updated_owned = updated.clone();
            let lsp_path = path.clone();
            let lsp_root = ctx.working_dir.clone();
            let _ = tokio::task::spawn_blocking(move || {
                crate::lsp::did_write(&lsp_path, &updated_owned, &lsp_root)
            })
            .await;
        }
        let old_lines: Vec<&str> = pieces[start - 1..end]
            .iter()
            .map(|p| line_text(p))
            .collect();
        let rail = range_rail(&path.display().to_string(), start, &old_lines, &new_lines);
        Ok(ToolOutcome::ok(reduce_output(
            format!(
                "replaced {count} line(s) at {start}..={end} with {} line(s) in {}\n{rail}",
                new_lines.len(),
                path.display()
            ),
            MAX_BYTES,
        )))
    }
}

/// Diff rail for a line range (Phase 14 style): the anchor `file:start`, then
/// up to three removed and three added lines, each group capped with a count.
fn range_rail(path: &str, start: usize, old: &[&str], new: &[&str]) -> String {
    let mut out = format!("{path}:{start}\n");
    for line in old.iter().take(3) {
        out.push_str(&format!("- {line}\n"));
    }
    if old.len() > 3 {
        out.push_str(&format!("- … (+{} lines)\n", old.len() - 3));
    }
    for line in new.iter().take(3) {
        out.push_str(&format!("+ {line}\n"));
    }
    if new.len() > 3 {
        out.push_str(&format!("+ … (+{} lines)\n", new.len() - 3));
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
            session_id: None,
        };
        EditLinesTool.execute(args, &ctx).await.expect("execute")
    }

    fn hashes(lines: &[&str]) -> Value {
        Value::Array(lines.iter().map(|l| json!(line_hash(l))).collect())
    }

    #[test]
    fn hash_is_three_lowercase_hex_chars_and_ignores_trailing_whitespace() {
        let h = line_hash("fn main() {}");
        assert_eq!(h.len(), 3, "{h}");
        assert!(h
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_eq!(h, line_hash("fn main() {}"), "stable across calls");
        assert_eq!(
            h,
            line_hash("fn main() {}  \t\r"),
            "trailing whitespace ignored"
        );
        assert_ne!(line_hash("abc"), line_hash("abd"), "content matters");
        assert_eq!(line_hash(""), line_hash("   "), "blank lines hash alike");
    }

    #[tokio::test]
    async fn replaces_verified_range() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc\n").unwrap();
        let out = run(
            json!({"path": "f.txt", "start_line": 2, "end_line": 2,
                   "expected_hashes": hashes(&["b"]), "new_text": "B"}),
            dir.path(),
        )
        .await;
        assert!(!out.is_error, "{out:?}");
        assert!(
            out.content.contains("replaced 1 line(s) at 2..=2"),
            "{out:?}"
        );
        assert!(out.content.contains(":2\n- b\n+ B"), "rail: {out:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "a\nB\nc\n"
        );
    }

    #[tokio::test]
    async fn multi_line_grow_and_shrink() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("g.txt"), "a\nb\nc\n").unwrap();
        let out = run(
            json!({"path": "g.txt", "start_line": 2, "end_line": 2,
                   "expected_hashes": hashes(&["b"]), "new_text": "x\ny\nz"}),
            dir.path(),
        )
        .await;
        assert!(!out.is_error, "{out:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("g.txt")).unwrap(),
            "a\nx\ny\nz\nc\n"
        );
        let out = run(
            json!({"path": "g.txt", "start_line": 2, "end_line": 5,
                   "expected_hashes": hashes(&["x", "y", "z", "c"]), "new_text": "only"}),
            dir.path(),
        )
        .await;
        assert!(!out.is_error, "{out:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("g.txt")).unwrap(),
            "a\nonly\n"
        );
    }

    #[tokio::test]
    async fn empty_new_text_deletes_range() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("d.txt"), "a\nb\nc\n").unwrap();
        let out = run(
            json!({"path": "d.txt", "start_line": 2, "end_line": 2,
                   "expected_hashes": hashes(&["b"]), "new_text": ""}),
            dir.path(),
        )
        .await;
        assert!(!out.is_error, "{out:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("d.txt")).unwrap(),
            "a\nc\n"
        );
    }

    #[tokio::test]
    async fn drift_is_rejected_and_file_untouched() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("h.txt"), "a\nb\nc\n").unwrap();
        let out = run(
            json!({"path": "h.txt", "start_line": 1, "end_line": 2,
                   "expected_hashes": [line_hash("a"), line_hash("stale")], "new_text": "X"}),
            dir.path(),
        )
        .await;
        assert!(out.is_error, "{out:?}");
        assert!(
            out.content.contains(&format!(
                "line 2: expected {}, found {}: b",
                line_hash("stale"),
                line_hash("b")
            )),
            "{out:?}"
        );
        assert!(
            out.content
                .contains("re-read the file with line_hashes=true"),
            "{out:?}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("h.txt")).unwrap(),
            "a\nb\nc\n"
        );
    }

    #[tokio::test]
    async fn wrong_hash_count_is_rejected() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("n.txt"), "a\nb\n").unwrap();
        let out = run(
            json!({"path": "n.txt", "start_line": 1, "end_line": 2,
                   "expected_hashes": [line_hash("a")], "new_text": "X"}),
            dir.path(),
        )
        .await;
        assert!(out.is_error, "{out:?}");
        assert!(out.content.contains("1 entry for 2 line(s)"), "{out:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("n.txt")).unwrap(),
            "a\nb\n"
        );
    }

    #[tokio::test]
    async fn out_of_range_and_inverted_ranges_are_rejected() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("r.txt"), "a\nb\nc\n").unwrap();
        let out = run(
            json!({"path": "r.txt", "start_line": 3, "end_line": 4,
                   "expected_hashes": hashes(&["c", "x"]), "new_text": "X"}),
            dir.path(),
        )
        .await;
        assert!(out.is_error, "{out:?}");
        assert!(out.content.contains("out of range"), "{out:?}");
        let out = run(
            json!({"path": "r.txt", "start_line": 0, "end_line": 1,
                   "expected_hashes": hashes(&["a"]), "new_text": "X"}),
            dir.path(),
        )
        .await;
        assert!(out.is_error, "{out:?}");
        let out = run(
            json!({"path": "r.txt", "start_line": 2, "end_line": 1,
                   "expected_hashes": [], "new_text": "X"}),
            dir.path(),
        )
        .await;
        assert!(out.is_error, "{out:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("r.txt")).unwrap(),
            "a\nb\nc\n"
        );
    }

    #[tokio::test]
    async fn crlf_and_missing_final_newline_are_preserved() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("c.txt"), "a\r\nb\r\nc").unwrap();
        let out = run(
            json!({"path": "c.txt", "start_line": 2, "end_line": 2,
                   "expected_hashes": hashes(&["b"]), "new_text": "x\ny"}),
            dir.path(),
        )
        .await;
        assert!(!out.is_error, "{out:?}");
        assert_eq!(
            std::fs::read(dir.path().join("c.txt")).unwrap(),
            b"a\r\nx\r\ny\r\nc"
        );
    }

    #[tokio::test]
    async fn editing_last_line_keeps_missing_final_newline() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("l.txt"), "a\nb").unwrap();
        let out = run(
            json!({"path": "l.txt", "start_line": 2, "end_line": 2,
                   "expected_hashes": hashes(&["b"]), "new_text": "B\n"}),
            dir.path(),
        )
        .await;
        assert!(!out.is_error, "{out:?}");
        assert_eq!(std::fs::read(dir.path().join("l.txt")).unwrap(), b"a\nB");
    }

    #[tokio::test]
    async fn protected_path_is_refused() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "a\n").unwrap();
        let out = run(
            json!({"path": "AGENTS.md", "start_line": 1, "end_line": 1,
                   "expected_hashes": hashes(&["a"]), "new_text": "b"}),
            dir.path(),
        )
        .await;
        assert!(out.is_error, "{out:?}");
        assert!(out.content.contains("write-protected"), "{out:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("AGENTS.md")).unwrap(),
            "a\n"
        );
    }
}
