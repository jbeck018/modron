//! `read_minified` tool (Phase 13, token-efficient I/O) — collapse a file to
//! its structural skeleton: comments and block bodies elided, signatures and
//! top-level items kept, every kept line anchored to its REAL line number
//! (`[L42]`) so follow-up `read`/`edit` calls target exact regions.
//!
//! v1 is a dependency-free structural scanner (brace-depth for C-like
//! languages, indentation for Python-family). The Code Intelligence tranche
//! upgrades this to tree-sitter for exact syntax awareness; the scanner
//! stays as the always-available fallback.
//!
//! Safety property: the minified view is for ORIENTATION only — edits never
//! apply against the skeleton (they go through the gated `edit` tool on
//! real content), so a mis-counted brace can cost a token, never a file.

use crate::common::{arg_str, open_regular_file, resolve_path, tool_err, MAX_INPUT_BYTES};
use crate::reduce::reduce_output;
use crate::validate::validate_args;
use async_trait::async_trait;
use modron_core::{Result, Tool, ToolCtx, ToolOutcome, ToolSpec};
use serde_json::{json, Value};

/// Cap for the minified skeleton.
pub const MAX_BYTES: usize = 24 * 1024;

/// Collapse to the structural skeleton with real line anchors.
pub struct ReadMinifiedTool;

#[async_trait]
impl Tool for ReadMinifiedTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_minified".into(),
            description: "Read a file collapsed to its skeleton: comments and function/block \
                          bodies elided, signatures kept, every line prefixed with its real \
                          line number ([Lnnn]) for targeted read/edit calls. Args: path. Use \
                          for orientation in large files; call `read` with the line numbers \
                          for full content before editing."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path (relative to working dir or absolute)."}
                },
                "required": ["path"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        const NAME: &str = "read_minified";
        let spec = self.spec();
        if let Err(message) = validate_args(&spec, &args) {
            return Ok(tool_err(
                NAME,
                format!("invalid arguments: {message}"),
                MAX_BYTES,
            ));
        }
        let Some(raw_path) = arg_str(&args, "path") else {
            return Ok(tool_err(NAME, "missing `path`", MAX_BYTES));
        };
        let path = resolve_path(&ctx.working_dir, raw_path);
        let file = match open_regular_file(&path) {
            Ok(file) => file,
            Err(e) => {
                return Ok(tool_err(
                    NAME,
                    format!("cannot read {}: {e}", path.display()),
                    MAX_BYTES,
                ))
            }
        };
        let too_large = |bytes: u64| {
            tool_err(
                NAME,
                format!(
                    "file too large to minify ({bytes} bytes, limit {} MiB): {}",
                    MAX_INPUT_BYTES >> 20,
                    path.display()
                ),
                MAX_BYTES,
            )
        };
        let declared = file.metadata().map(|m| m.len()).unwrap_or(0);
        if declared > MAX_INPUT_BYTES {
            return Ok(too_large(declared));
        }
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        if let Err(e) = tokio::fs::File::from_std(file)
            .take(MAX_INPUT_BYTES + 1)
            .read_to_end(&mut buf)
            .await
        {
            return Ok(tool_err(
                NAME,
                format!("cannot read {}: {e}", path.display()),
                MAX_BYTES,
            ));
        }
        if buf.len() as u64 > MAX_INPUT_BYTES {
            return Ok(too_large(buf.len() as u64));
        }
        let content = match String::from_utf8(buf) {
            Ok(content) => content,
            Err(e) => {
                return Ok(tool_err(
                    NAME,
                    format!("cannot read {}: {e}", path.display()),
                    MAX_BYTES,
                ))
            }
        };
        // The gate treats a minified read as a read: it unlocks edit/write.
        if let Some(engine) = &ctx.engine {
            engine.tools.read_gate.mark_in(ctx.session_id, &path);
        }

        // Prefer the exact syntax-aware skeleton when a grammar is bundled
        // (Phase 14 semantic feature); fall back to the structural scanner.
        #[cfg(feature = "semantic")]
        let skeleton = match minify_treesitter(&content, &path.to_string_lossy()) {
            Some(s) => s,
            None => scanner_skeleton(&content, &path.to_string_lossy()),
        };
        #[cfg(not(feature = "semantic"))]
        let skeleton = scanner_skeleton(&content, &path.to_string_lossy());
        let total = content.lines().count();
        let header = format!(
            "{} — skeleton of {total} lines (bodies elided; anchors are real line numbers)",
            path.display()
        );
        Ok(ToolOutcome::ok(reduce_output(
            format!("{header}\n{skeleton}"),
            MAX_BYTES,
        )))
    }
}

/// The dependency-free scanner fallback (or the whole implementation when
/// the `semantic` feature is off).
fn scanner_skeleton(content: &str, path: &str) -> String {
    if is_hash_comment_language(path) {
        minify_indented(content)
    } else {
        minify_braced(content)
    }
}

/// Python/shell-style paths: `#` starts a comment.
fn is_hash_comment_language(path: &str) -> bool {
    let lower = path.to_lowercase();
    [
        ".py", ".sh", ".bash", ".zsh", ".yaml", ".yml", ".toml", ".rb",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext))
}

/// Brace-language skeleton: track depth outside string literals; when a line
/// opens a block, keep the header, skip the body, emit a `+N lines` marker.
fn minify_braced(content: &str) -> String {
    let mut out = String::new();
    let mut depth: i32 = 0;
    let mut skip_from_depth: Option<i32> = None;
    let mut skipped = 0usize;
    let mut in_block_comment = false;

    for (idx, line) in content.lines().enumerate() {
        let lineno = idx + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue; // blank lines are not structure
        }

        // Block comments.
        if in_block_comment {
            if trimmed.contains("*/") {
                in_block_comment = false;
            }
            continue;
        }
        if trimmed.starts_with("/*") && !trimmed.contains("*/") {
            in_block_comment = true;
            continue;
        }
        if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed == "*" {
            continue;
        }

        // Inside a collapsed body: keep counting depth, emit nothing.
        if let Some(open_depth) = skip_from_depth {
            depth += brace_delta_outside_strings(line);
            if depth <= open_depth {
                skip_from_depth = None;
                out.push_str(&format!("… [+{skipped} lines elided]\n"));
                skipped = 0;
                // The closing line itself (`}` or `},`) is not interesting.
                continue;
            }
            skipped += 1;
            continue;
        }

        out.push_str(&format!("[L{lineno}] {trimmed}\n"));
        let delta = brace_delta_outside_strings(line);
        let before = depth;
        depth += delta;
        if depth > before && depth >= 1 {
            // This line opened a block: collapse its body.
            skip_from_depth = Some(before);
            skipped = 0;
        }
    }
    if skipped > 0 {
        out.push_str(&format!("… [+{skipped} lines elided]\n"));
    }
    out.trim_end().to_string()
}

/// Python-family skeleton: a line more indented than its predecessor starts
/// a block — keep the header, skip deeper lines.
fn minify_indented(content: &str) -> String {
    let mut out = String::new();
    let mut skipped = 0usize;
    let mut header_indent: Option<usize> = None;

    for (idx, line) in content.lines().enumerate() {
        let lineno = idx + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        match header_indent {
            Some(hi) if indent > hi => {
                skipped += 1;
                continue;
            }
            Some(_) if skipped > 0 => {
                out.push_str(&format!("… [+{skipped} lines elided]\n"));
            }
            Some(_) => {}
            None => {}
        }
        header_indent = if trimmed.ends_with(':') {
            Some(indent)
        } else {
            None
        };
        out.push_str(&format!("[L{lineno}] {trimmed}\n"));
    }
    if skipped > 0 {
        out.push_str(&format!("… [+{skipped} lines elided]\n"));
    }
    out.trim_end().to_string()
}

/// Net brace delta of a line, ignoring braces inside quoted strings.
fn brace_delta_outside_strings(line: &str) -> i32 {
    let mut delta = 0i32;
    let mut in_str: Option<char> = None;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match in_str {
            Some(q) => {
                if c == '\\' {
                    chars.next(); // skip the escaped char
                } else if c == q {
                    in_str = None;
                }
            }
            None => match c {
                '"' | '\'' => in_str = Some(c),
                '{' => delta += 1,
                '}' => delta -= 1,
                _ => {}
            },
        }
    }
    delta
}

/// Exact syntax-aware skeleton via tree-sitter (Phase 14 `semantic`
/// feature). Keeps the first row of definition-like nodes (functions,
/// types, classes) and of container children (methods inside impl/trait),
/// eliding bodies; falls back to `None` when no grammar matches the path
/// or the parse fails.
#[cfg(feature = "semantic")]
fn minify_treesitter(content: &str, path: &str) -> Option<String> {
    let language = grammar_for(path)?;
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).ok()?;
    let tree = parser.parse(content, None)?;
    let mut rows: Vec<usize> = Vec::new();
    walk_keep(tree.root_node(), &mut rows);

    let lines: Vec<&str> = content.lines().collect();
    let mut out = String::new();
    let mut prev_kept: Option<usize> = None; // last emitted real row
    for &row in rows.iter().filter(|r| **r < lines.len()) {
        let trimmed = lines[row].trim();
        if trimmed.is_empty() || prev_kept == Some(row) {
            // Blank, or a second definition on a line already emitted.
            continue;
        }
        if let Some(prev) = prev_kept {
            // Saturating: two definitions on one row give `row == prev`.
            let gap = row.saturating_sub(prev + 1);
            if gap > 0 {
                out.push_str(&format!("… [+{gap} lines elided]\n"));
            }
        }
        out.push_str(&format!("[L{}] {}\n", row + 1, trimmed));
        prev_kept = Some(row);
    }
    if let Some(prev) = prev_kept {
        let gap = lines.len().saturating_sub(prev + 1);
        if gap > 0 {
            out.push_str(&format!("… [+{gap} lines elided]\n"));
        }
    }
    Some(out.trim_end().to_string())
}

/// Nodes whose FIRST row is kept (their bodies are elided — children are
/// not descended into).
#[cfg(feature = "semantic")]
const DEFINITION_KINDS: &[&str] = &[
    // rust
    "function_item",
    "struct_item",
    "enum_item",
    "union_item",
    "type_item",
    "const_item",
    "static_item",
    "macro_definition",
    "foreign_mod_item",
    // python
    "function_definition",
    "decorated_definition",
    "class_definition",
    // javascript/typescript
    "function_declaration",
    "generator_function_declaration",
    "class_declaration",
    "method_definition",
    "lexical_declaration",
    "variable_declaration",
    // go
    "function_declaration",
    "method_declaration",
    "type_declaration",
];

/// Containers we descend into to find their members (impl blocks, traits,
/// modules, classes).
#[cfg(feature = "semantic")]
const CONTAINER_KINDS: &[&str] = &[
    "source_file",
    "impl_item",
    "trait_item",
    "mod_item",
    "declaration_list", // rust bodies we land in only via containers
    "module",           // python
    "class_body",       // js
    "program",          // js
    "declaration_list_go",
];

#[cfg(feature = "semantic")]
fn walk_keep(node: tree_sitter::Node, rows: &mut Vec<usize>) {
    let kind = node.kind();
    if DEFINITION_KINDS.contains(&kind) {
        rows.push(node.start_position().row);
        return; // body elided
    }
    // `mod foo;` (declaration, no body) keeps its row; `mod foo { … }`
    // descends to its items.
    if kind == "mod_item" && !has_body(node) {
        rows.push(node.start_position().row);
        return;
    }
    if !CONTAINER_KINDS.contains(&kind) {
        // Non-container, non-definition at this level: keep use/import
        // statements; everything else is body/detail.
        if kind.contains("use") || kind.contains("import") {
            rows.push(node.start_position().row);
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_keep(child, rows);
    }
}

/// Does this node own a `{ … }` body child (rust declaration_list)?
#[cfg(feature = "semantic")]
fn has_body(node: tree_sitter::Node) -> bool {
    let mut cursor = node.walk();
    let mut has = false;
    for child in node.children(&mut cursor) {
        if child.kind() == "declaration_list" {
            has = true;
            break;
        }
    }
    has
}

#[cfg(feature = "semantic")]
fn grammar_for(path: &str) -> Option<tree_sitter::Language> {
    let lower = path.to_lowercase();
    let ext = lower.rsplit('.').next()?;
    match ext {
        "rs" => Some(tree_sitter_rust::LANGUAGE.into()),
        "py" | "pyi" => Some(tree_sitter_python::LANGUAGE.into()),
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" => {
            Some(tree_sitter_javascript::LANGUAGE.into())
        }
        "go" => Some(tree_sitter_go::LANGUAGE.into()),
        _ => None,
    }
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
        ReadMinifiedTool.execute(args, &ctx).await.expect("execute")
    }

    #[tokio::test]
    async fn rust_file_collapses_bodies_and_keeps_anchors() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            "// a comment\nmod foo;\n\npub struct Config {\n    pub a: u32,\n    pub b: String,\n}\n\npub fn main() {\n    let x = 1;\n    let y = 2;\n    println!(\"{}\", x + y);\n}\n",
        )
        .unwrap();
        let out = run(json!({"path": "lib.rs"}), dir.path()).await;
        assert!(!out.is_error, "{out:?}");
        let c = &out.content;
        assert!(c.contains("[L2] mod foo;"), "{c}");
        assert!(c.contains("[L4] pub struct Config {"), "{c}");
        assert!(c.contains("[L9] pub fn main() {"), "{c}");
        assert!(c.contains("lines elided"), "{c}");
        assert!(!c.contains("println!"), "bodies elided: {c}");
        assert!(!c.contains("// a comment"), "comments elided: {c}");
        assert!(!c.contains("pub a: u32"), "fields elided: {c}");
    }

    #[tokio::test]
    async fn python_file_collapses_indented_blocks() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("app.py"),
            "# top comment\nimport sys\n\ndef main(args):\n    total = 0\n    for a in args:\n        total += int(a)\n    return total\n\nclass Runner:\n    def go(self):\n        return 1\n",
        )
        .unwrap();
        let out = run(json!({"path": "app.py"}), dir.path()).await;
        let c = &out.content;
        assert!(c.contains("[L2] import sys"), "{c}");
        assert!(c.contains("[L4] def main(args):"), "{c}");
        assert!(c.contains("[L10] class Runner:"), "{c}");
        assert!(!c.contains("total += int(a)"), "{c}");
        assert!(c.contains("lines elided"), "{c}");
    }

    #[tokio::test]
    async fn anchors_match_real_line_numbers() {
        let dir = tempdir().unwrap();
        let body = format!("{}\nfn later() {{\n    hidden()\n}}\n", "\n".repeat(50));
        // 50 newlines + the format string's own \n → `fn later()` is line 52.
        std::fs::write(dir.path().join("later.rs"), body).unwrap();
        let out = run(json!({"path": "later.rs"}), dir.path()).await;
        assert!(out.content.contains("[L52] fn later() {"), "{out:?}");
    }

    #[tokio::test]
    async fn braces_inside_strings_do_not_confuse_depth() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("s.rs"),
            "fn fmt() {\n    let s = \"}}}{{{\";\n    let t = '{';\n}\nfn next() {\n}\n",
        )
        .unwrap();
        let out = run(json!({"path": "s.rs"}), dir.path()).await;
        let c = &out.content;
        assert!(c.contains("[L1] fn fmt() {"), "{c}");
        assert!(c.contains("[L5] fn next() {"), "{c}");
        assert!(
            !c.contains("}}}"),
            "string content elided with the body: {c}"
        );
    }

    #[tokio::test]
    async fn go_file_skeleton_via_treesitter() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("main.go"),
            "package main\n\nimport \"fmt\"\n\nfunc main() {\n\tfmt.Println(\"hi\")\n}\n\nfunc helper(x int) int {\n\treturn x * 2\n}\n",
        )
        .unwrap();
        let out = run(json!({"path": "main.go"}), dir.path()).await;
        let c = &out.content;
        assert!(
            c.contains("[L1] package main;") || c.contains("package main"),
            "{c}"
        );
        assert!(
            c.contains("[L5] func main() {") || c.contains("func main()"),
            "{c}"
        );
        assert!(c.contains("helper"), "{c}");
        assert!(!c.contains("Println"), "body elided: {c}");
    }

    #[tokio::test]
    async fn two_definitions_on_one_line_have_no_gap_marker() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("one.rs"), "fn a() {} fn b() {}\n").unwrap();
        let out = run(json!({"path": "one.rs"}), dir.path()).await;
        assert!(!out.is_error, "{out:?}");
        assert!(!out.content.contains("lines elided"), "{}", out.content);
        assert_eq!(out.content.matches("[L1]").count(), 1, "{}", out.content);
    }

    #[tokio::test]
    async fn missing_file_is_error() {
        let dir = tempdir().unwrap();
        let out = run(json!({"path": "nope.rs"}), dir.path()).await;
        assert!(out.is_error, "{out:?}");
    }

    #[tokio::test]
    async fn device_file_is_refused_quickly() {
        let dir = tempdir().unwrap();
        let out = run(json!({"path": "/dev/zero"}), dir.path()).await;
        assert!(out.is_error, "{out:?}");
        assert!(out.content.contains("not a regular file"), "{out:?}");
    }

    #[tokio::test]
    async fn oversize_file_is_refused() {
        let dir = tempdir().unwrap();
        let body = "fn a() {}\n".repeat(11 * 1024 * 1024 / 10 + 1);
        std::fs::write(dir.path().join("huge.rs"), body).unwrap();
        let out = run(json!({"path": "huge.rs"}), dir.path()).await;
        assert!(out.is_error, "{out:?}");
        assert!(out.content.contains("file too large to minify"), "{out:?}");
    }
}
