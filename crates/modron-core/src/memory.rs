//! Persistent per-workspace memory (LEDGER Phase 1a M1, PLAN.md §4.9 user tier).
//!
//! Layout under the memory root (default `~/.modron/memory`):
//!
//! ```text
//! <memory_root>/<workspace-slug>/MEMORY.md                    index, newest first, ≤16 KiB
//! <memory_root>/<workspace-slug>/entries/<stamp>-<title>.md   one markdown file per entry
//! ```
//!
//! The workspace slug is derived from the canonicalized working directory:
//! lowercase alnum/dash segments joined by `-`, plus a short FNV-1a hash
//! suffix so truncation and lookalike paths cannot collide
//! (e.g. `users-jacob-documents-projects-modron-a1b2c3`).
//!
//! Deliberately durable-simple: std::fs only; the index (and entry files) are
//! written through a temp-file-then-rename so a crash never leaves a
//! half-written file. Directories are created lazily by [`MemoryStore::append`]
//! — read paths on a fresh workspace return empty without side effects.
//! The 16 KiB index cap drops the oldest index *lines* (with a warning); the
//! dropped entries' files stay on disk — the cap protects context size, not
//! the archive.

use crate::{Error, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// Name of the index file inside a workspace memory directory.
const INDEX_FILE: &str = "MEMORY.md";

/// Hard cap for the index file; oldest lines are dropped when exceeded.
pub const MAX_INDEX_BYTES: usize = 16 * 1024;

/// Hard cap for a single entry body, enforced at write time.
pub const MAX_ENTRY_BYTES: usize = 8 * 1024;

/// Marker appended when an entry body was truncated at [`MAX_ENTRY_BYTES`].
const TRUNCATION_MARKER: &str = "\n\n[entry truncated at 8 KiB]";

/// Longest title-derived slug used in entry file names.
const MAX_TITLE_SLUG: usize = 64;

/// Longest path-derived slug prefix before the hash suffix.
const MAX_SLUG_PREFIX: usize = 48;

/// A persisted memory entry, as returned by [`MemoryStore::append`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub title: String,
    /// `YYYY-MM-DD` (UTC) the entry was written.
    pub date: String,
    /// Path of the entry's markdown file.
    pub path: PathBuf,
}

/// Per-workspace memory store: a `MEMORY.md` index plus one markdown file per
/// entry, under `<memory_root>/<workspace-slug>`. All operations are sync
/// std::fs and cheap; callers on async runtimes accept blocking-in-async for
/// these tiny local writes (house pattern for MVP).
pub struct MemoryStore {
    root: PathBuf,
}

impl MemoryStore {
    /// Store for the workspace containing `working_dir`: the slug is derived
    /// from the canonicalized `working_dir` (falling back to an absolute,
    /// non-resolving path when canonicalization fails, e.g. dir not yet
    /// created). No directories are created here — see [`MemoryStore::append`].
    pub fn for_workspace(memory_root: &Path, working_dir: &Path) -> Result<MemoryStore> {
        if memory_root.as_os_str().is_empty() {
            return Err(Error::Session("memory root must not be empty".into()));
        }
        let canonical = std::fs::canonicalize(working_dir)
            .or_else(|_| std::path::absolute(working_dir))
            .map_err(|err| {
                io_err(
                    &err,
                    format!("cannot resolve working dir {}", working_dir.display()),
                )
            })?;
        Ok(MemoryStore {
            root: memory_root.join(workspace_slug(&canonical)),
        })
    }

    /// Directory holding this workspace's memory (`<memory_root>/<slug>`).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Workspace slug derived from the canonicalized working directory.
    pub fn slug(&self) -> &str {
        self.root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("")
    }

    /// Append an entry: writes `entries/<stamp>-<title>.md` (body capped at
    /// [`MAX_ENTRY_BYTES`]) and prepends an index line to `MEMORY.md`
    /// (newest first, capped at [`MAX_INDEX_BYTES`]).
    pub fn append(&self, title: &str, body: &str) -> Result<Entry> {
        let title = title.trim();
        if title.is_empty() {
            return Err(Error::Session(
                "memory entry title must not be empty".into(),
            ));
        }
        let entries_dir = self.root.join("entries");
        fs::create_dir_all(&entries_dir)
            .map_err(|err| io_err(&err, format!("cannot create {}", entries_dir.display())))?;

        let now = chrono::Utc::now();
        let date = now.format("%Y-%m-%d").to_string();
        let stamp = now.format("%Y%m%dT%H%M%S%3f").to_string();
        let title_slug = slugify(title, MAX_TITLE_SLUG);
        let title_slug = if title_slug.is_empty() {
            "entry".to_string()
        } else {
            title_slug
        };
        let mut file_name = format!("{stamp}-{title_slug}.md");
        let mut counter = 1u32;
        while entries_dir.join(&file_name).exists() {
            counter += 1;
            file_name = format!("{stamp}-{title_slug}-{counter}.md");
        }
        let path = entries_dir.join(&file_name);

        let trimmed_body = body.trim();
        let stored = truncate_at_char_boundary(trimmed_body, MAX_ENTRY_BYTES);
        let mut file_body = stored.to_string();
        if stored.len() < trimmed_body.len() {
            file_body.push_str(TRUNCATION_MARKER);
        }
        file_body.push('\n');
        write_atomic(&path, &file_body)?;

        let index_line = format!("- [{date} {}](entries/{file_name})", display_title(title));
        let mut lines: Vec<String> = self.read_index().lines().map(str::to_string).collect();
        lines.insert(0, index_line);
        write_atomic(&self.root.join(INDEX_FILE), &cap_index_lines(lines))?;

        tracing::debug!(file = %path.display(), "memory entry written");
        Ok(Entry {
            title: title.to_string(),
            date,
            path,
        })
    }

    /// The `MEMORY.md` index (newest first), or "" when absent/unreadable.
    pub fn read_index(&self) -> String {
        match fs::read_to_string(self.root.join(INDEX_FILE)) {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => {
                tracing::warn!(error = %err, "failed reading memory index; treating as empty");
                String::new()
            }
        }
    }

    /// Index plus the bodies of the `limit` newest entries (newest first),
    /// each under a `### <date> <title>` header. Unreadable entries are
    /// skipped with a warning.
    pub fn read_entries(&self, limit: usize) -> String {
        let index = self.read_index();
        if index.is_empty() {
            return index;
        }
        let mut out = index;
        for (label, target) in index_entries(&out).into_iter().take(limit) {
            match fs::read_to_string(self.root.join(&target)) {
                Ok(body) => {
                    let body = body.trim();
                    if !body.is_empty() {
                        out.push_str(&format!("\n### {label}\n{body}\n"));
                    }
                }
                Err(err) => {
                    tracing::warn!(target = %target, error = %err, "skipping unreadable memory entry")
                }
            }
        }
        out
    }

    /// Body of the newest entry titled `title`, `None` when not in the index.
    pub fn read_entry(&self, title: &str) -> Result<Option<String>> {
        let wanted = title.trim();
        for (label, target) in index_entries(&self.read_index()) {
            if entry_title(&label) == wanted {
                let path = self.root.join(target);
                return match fs::read_to_string(&path) {
                    Ok(body) => Ok(Some(body)),
                    Err(err) => Err(io_err(&err, format!("cannot read {}", path.display()))),
                };
            }
        }
        Ok(None)
    }

    /// Remove every entry titled `title`: deletes its index line(s) and file(s).
    /// Returns whether anything was removed.
    pub fn forget(&self, title: &str) -> Result<bool> {
        let wanted = title.trim();
        if wanted.is_empty() {
            return Ok(false);
        }
        let mut kept: Vec<&str> = Vec::new();
        let mut removed_targets: Vec<String> = Vec::new();
        let index = self.read_index();
        for line in index.lines() {
            match parse_index_line(line) {
                Some((label, target)) if entry_title(label) == wanted => {
                    removed_targets.push(target.to_string());
                }
                _ => kept.push(line),
            }
        }
        if removed_targets.is_empty() {
            return Ok(false);
        }
        let content = if kept.is_empty() {
            String::new()
        } else {
            format!("{}\n", kept.join("\n"))
        };
        write_atomic(&self.root.join(INDEX_FILE), &content)?;
        for target in &removed_targets {
            let path = self.root.join(target);
            if let Err(err) = fs::remove_file(&path) {
                if err.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(path = %path.display(), error = %err, "failed removing memory entry file");
                }
            }
        }
        tracing::debug!(count = removed_targets.len(), "memory entries forgotten");
        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Parsed index lines, newest first: `(date+title label, relative target)`.
fn index_entries(index: &str) -> Vec<(String, String)> {
    index
        .lines()
        .filter_map(|line| {
            parse_index_line(line).map(|(label, target)| (label.to_string(), target.to_string()))
        })
        .collect()
}

/// `- [<date> <title>](entries/<file>.md)` → `(<date> <title>`, `entries/<file>.md)`.
/// Unparseable (e.g. hand-edited) lines yield `None` and are preserved verbatim.
fn parse_index_line(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix("- [")?;
    let close = rest.find("](")?;
    let label = &rest[..close];
    let target = rest.get(close + 2..)?.strip_suffix(')')?;
    if !is_safe_rel_target(target) {
        return None;
    }
    Some((label, target))
}

/// Index targets must stay inside `entries/`: a hand-edited index must not be
/// able to make `forget` delete files outside the workspace memory dir.
fn is_safe_rel_target(target: &str) -> bool {
    target.starts_with("entries/") && !target.contains("..")
}

/// `label` is `{YYYY-MM-DD} {title}`; return the title part.
fn entry_title(label: &str) -> &str {
    label.get(11..).unwrap_or(label)
}

/// Index-line title: single-line, with `]` escaped so the markdown link survives.
fn display_title(title: &str) -> String {
    title
        .chars()
        .map(|ch| match ch {
            '\n' | '\r' => ' ',
            ']' => ')',
            other => other,
        })
        .collect()
}

/// Newest-first index capped at [`MAX_INDEX_BYTES`] by dropping the oldest
/// (trailing) lines. At least one line is always kept.
fn cap_index_lines(lines: Vec<String>) -> String {
    let mut kept = lines;
    let mut dropped = 0usize;
    while index_bytes(&kept) > MAX_INDEX_BYTES && kept.len() > 1 {
        kept.pop();
        dropped += 1;
    }
    if dropped > 0 {
        tracing::warn!(
            dropped,
            cap = MAX_INDEX_BYTES,
            "memory index over cap; dropped oldest entries (their files stay on disk)"
        );
    }
    if kept.is_empty() {
        String::new()
    } else {
        format!("{}\n", kept.join("\n"))
    }
}

fn index_bytes(lines: &[String]) -> usize {
    lines.iter().map(|line| line.len() + 1).sum()
}

/// Workspace slug: sanitized path segments + short hash
/// (e.g. `users-jacob-documents-projects-modron-a1b2c3`).
fn workspace_slug(canonical: &Path) -> String {
    let mut segments: Vec<String> = Vec::new();
    for component in canonical.components() {
        let slug = slugify(&component.as_os_str().to_string_lossy(), usize::MAX);
        if !slug.is_empty() {
            segments.push(slug);
        }
    }
    // Keep the leading segments within the prefix budget (drop from the end:
    // the top-level path is the most identifying part).
    let mut prefix = String::new();
    for segment in &segments {
        let candidate = if prefix.is_empty() {
            segment.clone()
        } else {
            format!("{prefix}-{segment}")
        };
        if candidate.len() > MAX_SLUG_PREFIX {
            break;
        }
        prefix = candidate;
    }
    let hash = short_hash(&canonical.to_string_lossy());
    if prefix.is_empty() {
        hash
    } else {
        format!("{prefix}-{hash}")
    }
}

/// Lowercase alnum/dash slug: any other character run collapses to a single
/// dash. Result is ASCII, at most `max` chars, possibly empty.
fn slugify(text: &str, max: usize) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' {
            if out.len() < max {
                out.push(ch.to_ascii_lowercase());
            }
        } else if !out.is_empty() && !out.ends_with('-') && out.len() < max {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// Short deterministic FNV-1a hash (24 bits, 6 hex chars). Hand-rolled because
/// the slug must stay stable across releases, which `DefaultHasher` does not
/// guarantee.
fn short_hash(s: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in s.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:06x}", hash & 0x00ff_ffff)
}

/// Write via temp file + rename so a crash never leaves a half-written file.
fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    fs::write(&tmp, contents)
        .map_err(|err| io_err(&err, format!("cannot write {}", tmp.display())))?;
    fs::rename(&tmp, path).map_err(|err| {
        let _ = fs::remove_file(&tmp);
        io_err(&err, format!("cannot finalize {}", path.display()))
    })
}

/// Slice `s` to at most `max` bytes without splitting a UTF-8 code point.
fn truncate_at_char_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn io_err(err: &std::io::Error, what: impl std::fmt::Display) -> Error {
    Error::Session(format!("{what}: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn store_for(root: &Path, work: &Path) -> MemoryStore {
        MemoryStore::for_workspace(root, work).unwrap()
    }

    #[test]
    fn append_index_read_forget_roundtrip() {
        let root = tempdir().unwrap();
        let work = tempdir().unwrap();
        let store = store_for(root.path(), work.path());

        let entry = store.append("rust version", "pinned to 1.99.1").unwrap();
        assert_eq!(entry.title, "rust version");
        assert!(entry.path.is_file(), "{:?} should exist", entry.path);
        assert_eq!(
            entry.date,
            chrono::Utc::now().format("%Y-%m-%d").to_string()
        );

        let index = store.read_index();
        assert!(index.starts_with("- [2"), "index line format: {index}");
        assert!(
            index.contains(" rust version](entries/"),
            "index line format: {index}"
        );
        assert!(index.ends_with('\n'));

        let body = store.read_entry("rust version").unwrap().unwrap();
        assert_eq!(body.trim(), "pinned to 1.99.1");

        let entries = store.read_entries(1);
        assert!(
            entries.contains(&index),
            "read_entries starts with the index"
        );
        assert!(entries.contains("### ") && entries.contains("pinned to 1.99.1"));

        assert!(store.forget("rust version").unwrap());
        assert_eq!(store.read_index(), "");
        assert_eq!(store.read_entry("rust version").unwrap(), None);
        assert!(!entry.path.exists(), "entry file removed");
        assert!(
            !store.forget("rust version").unwrap(),
            "second forget is a no-op"
        );
    }

    #[test]
    fn two_appends_are_both_indexed_newest_first() {
        let root = tempdir().unwrap();
        let work = tempdir().unwrap();
        let store = store_for(root.path(), work.path());
        store.append("alpha", "alpha body").unwrap();
        store.append("beta", "beta body").unwrap();

        let index = store.read_index();
        assert_eq!(index.lines().count(), 2, "both appends survive: {index}");
        let (beta_pos, alpha_pos) = (index.find("beta").unwrap(), index.find("alpha").unwrap());
        assert!(beta_pos < alpha_pos, "newest first: {index}");

        let entries = store.read_entries(5);
        assert!(entries.contains("alpha body") && entries.contains("beta body"));
        assert!(entries.find("beta body").unwrap() < entries.find("alpha body").unwrap());
    }

    #[test]
    fn read_entries_respects_limit_and_missing_workspace_is_empty() {
        let root = tempdir().unwrap();
        let work = tempdir().unwrap();
        let store = store_for(root.path(), work.path());
        assert_eq!(store.read_index(), "");
        assert_eq!(store.read_entries(5), "");
        assert_eq!(store.read_entry("x").unwrap(), None);
        assert!(!store.forget("x").unwrap());
        // Reads must not create anything on disk.
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);

        for i in 0..3 {
            store
                .append(&format!("fact {i}"), &format!("body {i}"))
                .unwrap();
        }
        assert_eq!(store.read_entries(2).matches("### ").count(), 2);
        assert_eq!(store.read_entries(0).matches("### ").count(), 0);
        assert_eq!(store.read_entries(10).matches("### ").count(), 3);
    }

    #[test]
    fn same_title_twice_creates_two_files_and_forget_removes_both() {
        let root = tempdir().unwrap();
        let work = tempdir().unwrap();
        let store = store_for(root.path(), work.path());
        store.append("Same", "one").unwrap();
        store.append("Same", "two").unwrap();
        assert_eq!(store.read_index().lines().count(), 2);
        assert!(store.forget("Same").unwrap());
        assert_eq!(store.read_index(), "");
        assert_eq!(
            fs::read_dir(store.root().join("entries")).unwrap().count(),
            0,
            "both entry files removed"
        );
    }

    #[test]
    fn index_cap_drops_oldest_lines_but_keeps_files() {
        let root = tempdir().unwrap();
        let work = tempdir().unwrap();
        let store = store_for(root.path(), work.path());
        for i in 0..200 {
            let title = format!("entry{i:03} {}", "x".repeat(80));
            store.append(&title, &format!("body {i}")).unwrap();
        }
        let index = store.read_index();
        assert!(
            index.len() <= MAX_INDEX_BYTES,
            "index capped: {}",
            index.len()
        );
        assert!(index.contains("entry199"), "newest entry kept");
        assert!(!index.contains("entry000"), "oldest entry dropped");
        assert!(
            !index.lines().next().unwrap().is_empty(),
            "at least one line kept"
        );
        // The cap trims the index, not the archive.
        assert_eq!(
            fs::read_dir(store.root().join("entries")).unwrap().count(),
            200
        );
    }

    #[test]
    fn entry_body_capped_at_8kb_on_char_boundary() {
        let root = tempdir().unwrap();
        let work = tempdir().unwrap();
        let store = store_for(root.path(), work.path());
        store.append("big", &"é".repeat(6000)).unwrap(); // 12_000 bytes
        let stored = store.read_entry("big").unwrap().unwrap();
        assert!(stored.len() <= MAX_ENTRY_BYTES + TRUNCATION_MARKER.len() + 1);
        let core = stored.trim_end().strip_suffix(TRUNCATION_MARKER).unwrap();
        assert!(
            !core.is_empty() && core.chars().all(|ch| ch == 'é'),
            "clean boundary"
        );
    }

    #[test]
    fn empty_title_is_rejected() {
        let root = tempdir().unwrap();
        let work = tempdir().unwrap();
        let store = store_for(root.path(), work.path());
        assert!(store.append("   ", "body").is_err());
    }

    #[test]
    fn slug_is_stable_per_directory_and_differs_across_directories() {
        let root = tempdir().unwrap();
        let work_a = tempdir().unwrap();
        let work_b = tempdir().unwrap();
        let a1 = store_for(root.path(), work_a.path());
        let a2 = store_for(root.path(), work_a.path());
        let b = store_for(root.path(), work_b.path());
        assert_eq!(a1.slug(), a2.slug(), "same dir twice → same slug");
        assert_ne!(a1.slug(), b.slug(), "different dir → different slug");
        assert!(!a1.slug().is_empty());
    }

    #[test]
    fn slug_is_sanitized_and_hash_suffixed() {
        // Shape contract on synthetic paths (workspace_slug is pure): a
        // typical workspace path yields the PLAN.md §4.9 example shape
        // `<sanitized path>-<6 hex chars>`.
        let slug = workspace_slug(Path::new("/Users/jacob/Documents/projects/modron"));
        assert!(
            slug.starts_with("users-jacob-documents-projects-modron-"),
            "path prefix kept in full, hash suffix appended: {slug}"
        );
        let suffix = slug.rsplit('-').next().unwrap();
        assert_eq!(suffix.len(), 6, "short hash suffix: {slug}");
        assert!(suffix.chars().all(|ch| ch.is_ascii_hexdigit()), "{slug}");

        // Sanitization: case folded; spaces/underscores/colons collapse to
        // single dashes; no leading/trailing dash.
        let weird = workspace_slug(Path::new("/tmp/Foo Bar_Baz/Qux::Quux"));
        assert!(
            weird.starts_with("tmp-foo-bar-baz-qux-quux-"),
            "sanitized: {weird}"
        );
        assert!(
            weird
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-'),
            "sanitized: {weird}"
        );
        assert!(
            !weird.starts_with('-') && !weird.ends_with('-'),
            "trimmed: {weird}"
        );
        assert_eq!(
            weird,
            workspace_slug(Path::new("/tmp/Foo Bar_Baz/Qux::Quux")),
            "stable"
        );

        // Prefix budget: a deep path keeps only the leading segments (within
        // MAX_SLUG_PREFIX chars) and always ends with the hash suffix. This
        // is why a macOS tempdir (canonicalizes to /private/var/folders/…)
        // never carries deep path segments in its slug.
        let deep = workspace_slug(Path::new("/a/bb/ccc/dddd/eeeee/ffffff/ggggggg/hhhhhhhh"));
        assert!(
            deep.len() <= MAX_SLUG_PREFIX + 1 + 6,
            "prefix budget: {deep}"
        );
        assert_eq!(
            deep.rsplit('-').next().unwrap().len(),
            6,
            "hash suffix: {deep}"
        );

        // Integration: a MemoryStore on a tempdir workspace gets the same
        // sanitized, stable, hash-suffixed treatment.
        let root = tempdir().unwrap();
        let work = tempdir().unwrap();
        let nested = work.path().join("docs").join("projects").join("modron");
        std::fs::create_dir_all(&nested).unwrap();
        let store = store_for(root.path(), &nested);
        let slug = store.slug();
        assert!(
            slug.chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-'),
            "sanitized: {slug}"
        );
        assert!(
            !slug.starts_with('-') && !slug.ends_with('-'),
            "trimmed: {slug}"
        );
        assert_eq!(
            slug,
            store_for(root.path(), &nested).slug(),
            "stable for the same dir"
        );
        assert_eq!(
            slug.rsplit('-').next().unwrap().len(),
            6,
            "hash suffix: {slug}"
        );
    }
}
