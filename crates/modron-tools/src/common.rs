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

/// Write-then-rename: rename(2) within a directory is atomic, so a crash
/// or error mid-write can never truncate the target file (the old content
/// survives until the rename lands).
pub(crate) async fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let target = write_atomic_target(path).await?;
    let (tmp, mut file) = create_unique_temp(&target).await?;
    let result = async {
        use tokio::io::AsyncWriteExt;
        file.write_all(contents).await?;
        file.flush().await?;
        drop(file);
        if let Ok(meta) = tokio::fs::metadata(&target).await {
            tokio::fs::set_permissions(&tmp, meta.permissions()).await?;
        }
        tokio::fs::rename(&tmp, &target).await
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    result
}

/// Follows a symlink chain so the write lands on the real file and the link
/// itself survives. A missing final component is returned as-is (new file).
async fn write_atomic_target(path: &Path) -> std::io::Result<PathBuf> {
    const MAX_LINKS: usize = 40;
    let mut current = path.to_path_buf();
    for _ in 0..MAX_LINKS {
        match tokio::fs::symlink_metadata(&current).await {
            Ok(meta) if meta.file_type().is_symlink() => {
                let link = tokio::fs::read_link(&current).await?;
                current = if link.is_absolute() {
                    link
                } else {
                    parent_dir(&current).join(link)
                };
            }
            Ok(_) => return Ok(current),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(current),
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("too many levels of symbolic links"))
}

/// Parent directory for a path, using `.` when the path has no directory part.
fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

/// Creates a temp file next to `target` with `create_new`, so an existing
/// path (planted symlink, stale temp file) is never opened or followed. The
/// name mixes pid and a nanosecond clock and retries on collision.
async fn create_unique_temp(target: &Path) -> std::io::Result<(PathBuf, tokio::fs::File)> {
    let file_name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let dir = parent_dir(target);
    let pid = std::process::id();
    let mut attempt: u32 = 0;
    loop {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tmp = dir.join(format!(".{file_name}.modron-tmp-{pid}-{nanos}-{attempt}"));
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .await
        {
            Ok(file) => return Ok((tmp, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempt < 32 => {
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod write_atomic_tests {
    use super::write_atomic;
    use tempfile::tempdir;

    #[tokio::test]
    async fn write_atomic_does_not_follow_planted_tmp_symlink() {
        let dir = tempdir().unwrap();
        let victim = dir.path().join("victim.txt");
        std::fs::write(&victim, "precious").unwrap();
        let target = dir.path().join("x.rs");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&victim, dir.path().join("x.modron-tmp")).unwrap();

        write_atomic(&target, b"new").await.unwrap();

        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_atomic_preserves_executable_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let script = dir.path().join("run.sh");
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        write_atomic(&script, b"#!/bin/sh\necho hi\n")
            .await
            .unwrap();

        let mode = std::fs::metadata(&script).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_atomic_writes_through_symlink() {
        let dir = tempdir().unwrap();
        let real = dir.path().join("real.txt");
        std::fs::write(&real, "old").unwrap();
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink("real.txt", &link).unwrap();

        write_atomic(&link, b"new").await.unwrap();

        let meta = std::fs::symlink_metadata(&link).unwrap();
        assert!(meta.file_type().is_symlink(), "link must stay a symlink");
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new");
    }

    #[tokio::test]
    async fn write_atomic_leaves_no_temp_files_behind() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("a.txt");
        write_atomic(&target, b"one").await.unwrap();
        write_atomic(&target, b"two").await.unwrap();

        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.txt".to_string()]);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "two");
    }
}
