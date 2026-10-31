//! Shared helpers for the built-in tools: path resolution, argument
//! extraction, walking configuration, and the standard error-outcome shape.

use crate::reduce::reduce_output;
use modron_core::{Error, ToolOutcome};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Directories never descended into by `glob`/`grep` (on top of `.gitignore`
/// and hidden entries).
pub(crate) const SKIPPED_DIRS: &[&str] = &["target", "node_modules", "vendor"];

/// Resolves `raw` against `working_dir` unless it is already absolute.
pub(crate) fn resolve_path(working_dir: &Path, raw: &str) -> PathBuf {
    let p = Path::new(raw);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        working_dir.join(p)
    }
}

/// Extracts an optional non-negative integer argument. Accepts integral
/// floats (`2000.0`) because some providers serialize integers that way.
pub(crate) fn opt_usize(args: &Value, key: &str) -> Option<usize> {
    args.get(key)
        .and_then(Value::as_f64)
        .filter(|f| *f >= 0.0 && f.fract() == 0.0)
        .map(|f| f as usize)
}

/// Like [`opt_usize`] but for `u64` arguments (e.g. `timeout_ms`).
pub(crate) fn opt_u64(args: &Value, key: &str) -> Option<u64> {
    args.get(key)
        .and_then(Value::as_f64)
        .filter(|f| *f >= 0.0 && f.fract() == 0.0)
        .map(|f| f as u64)
}

/// Extracts an optional boolean argument (e.g. `overwrite`, `replace_all`).
pub(crate) fn opt_bool(args: &Value, key: &str) -> Option<bool> {
    args.get(key).and_then(Value::as_bool)
}

/// Extracts a required string argument (validated upstream; `None` is handled
/// defensively, never panics).
pub(crate) fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

/// Standard tool-error outcome: prefixed with the tool name and passed
/// through the reduction choke point like every other output.
pub(crate) fn tool_err(
    tool: &str,
    message: impl std::fmt::Display,
    max_bytes: usize,
) -> ToolOutcome {
    ToolOutcome::err(reduce_output(format!("{tool}: {message}"), max_bytes))
}

/// A `WalkBuilder` used by `glob` and `grep`: skips hidden entries, honors
/// `.gitignore` even outside git repositories, and never descends into
/// build/vendor directories.
pub(crate) fn walk_builder(root: &Path) -> ignore::WalkBuilder {
    let mut builder = ignore::WalkBuilder::new(root);
    builder.hidden(true).git_ignore(true).require_git(false);
    builder.filter_entry(|entry| {
        if entry.depth() == 0 {
            return true;
        }
        let skip = entry.file_type().is_some_and(|ft| ft.is_dir())
            && SKIPPED_DIRS
                .iter()
                .any(|d| entry.file_name().to_string_lossy() == *d);
        !skip
    });
    builder
}

/// Builds a gitignore-syntax matcher for a user-supplied glob pattern.
/// Only positive patterns match; `Match::Whitelist` (negated globs) is not
/// treated as a match.
pub(crate) fn gitignore_matcher(
    root: &Path,
    pattern: &str,
) -> std::result::Result<ignore::gitignore::Gitignore, String> {
    let mut builder = ignore::gitignore::GitignoreBuilder::new(root);
    builder
        .add_line(None, pattern)
        .map_err(|e| format!("invalid glob pattern `{pattern}`: {e}"))?;
    builder
        .build()
        .map_err(|e| format!("invalid glob pattern `{pattern}`: {e}"))
}

/// Maps an internal failure to the frozen tool error ("the tool could not
/// run"), as opposed to [`ToolOutcome::err`] which covers expected failures.
pub(crate) fn internal_tool_error(tool: &str, message: impl std::fmt::Display) -> Error {
    Error::Tool {
        tool: tool.to_string(),
        message: message.to_string(),
    }
}
