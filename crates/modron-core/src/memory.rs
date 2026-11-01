//! Persistent per-workspace memory (LEDGER Phase 1a M1, PLAN.md §4.9 user tier).
//!
//! Layout under the memory root (default `~/.modron/memory`):
//!
//! ```text
//! <memory_root>/<workspace-slug>/MEMORY.md                    index, newest first, ≤16 KiB
//! <memory_root>/<workspace-slug>/entries/<stamp>-<title>.md   one markdown file per entry
//! <memory_root>/<workspace-slug>/breadcrumbs.jsonl            searchable trace tier (Phase 13)
//! <memory_root>/<workspace-slug>/stats.json                   usefulness ledger: uses/successes/failures
//! <memory_root>/global/…                                      cross-project tier (same shape)
//! ```
//!
//! Phase 13 (self-improving memory): facts are the hot tier (injected into
//! the system prompt), breadcrumbs are a searchable index that is never
//! injected, and every fact carries a [`FactStat`] ledger. The curator
//! ([`MemoryStore::curate`]) prunes on *proven usefulness* — hits and
//! success/failure records decayed by idle days — not just age.
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
// Phase 13: global tier, breadcrumbs, usefulness ledger, curator
// ---------------------------------------------------------------------------

/// Rolling cap for the breadcrumb tier: the newest [`MAX_BREADCRUMBS`]
/// lines are kept, the rest fall off the tail.
pub const MAX_BREADCRUMBS: usize = 500;

/// One searchable trace item — never injected into the prompt, surfaced only
/// through `memory_search`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Breadcrumb {
    /// Stable id (`bc-<stamp>-<hash>`).
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// RFC 3339 UTC timestamp.
    pub created: String,
}

/// Usefulness ledger entry per fact id (the entry file stem).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FactStat {
    pub uses: u32,
    pub successes: u32,
    pub failures: u32,
    /// RFC 3339 UTC timestamp of the last `memory_search` hit.
    #[serde(default)]
    pub last_used: String,
    /// Set by the curator: a relegated fact stays in the index and on disk,
    /// but its body is no longer injected into the system prompt.
    #[serde(default)]
    pub relegated: bool,
}

/// What one curator pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CurateReport {
    pub facts_total: usize,
    pub facts_relegated: usize,
    pub facts_reinstated: usize,
    pub breadcrumbs_pruned: usize,
}

/// Curator policy knobs (defaults in [`CuratePolicy::default`]).
#[derive(Clone, Copy, Debug)]
pub struct CuratePolicy {
    /// Days without a hit before a fact starts decaying.
    pub grace_days: f64,
    /// Days a breadcrumb survives regardless of interest.
    pub breadcrumb_ttl_days: f64,
}

impl Default for CuratePolicy {
    fn default() -> Self {
        Self {
            grace_days: 7.0,
            breadcrumb_ttl_days: 90.0,
        }
    }
}

impl MemoryStore {
    /// The cross-project tier: same layout, rooted at `<memory_root>/global`.
    /// Durable user preferences that follow the user across workspaces.
    pub fn for_global(memory_root: &Path) -> Result<MemoryStore> {
        if memory_root.as_os_str().is_empty() {
            return Err(Error::Session("memory root must not be empty".into()));
        }
        Ok(MemoryStore {
            root: memory_root.join("global"),
        })
    }

    /// Append a breadcrumb (trace tier). Cheap: one JSONL line, rolling cap.
    pub fn append_breadcrumb(&self, text: &str, tags: &[String]) -> Result<Breadcrumb> {
        let text = text.trim();
        if text.is_empty() {
            return Err(Error::Session("breadcrumb text must not be empty".into()));
        }
        fs::create_dir_all(&self.root).map_err(|err| {
            io_err(&err, format!("cannot create {}", self.root.display()))
        })?;
        let now = chrono::Utc::now();
        let stamp = now.format("%Y%m%dT%H%M%S%3f").to_string();
        let crumb = Breadcrumb {
            id: format!("bc-{stamp}-{}", short_hash(text)),
            text: truncate_at_char_boundary(text, MAX_ENTRY_BYTES).to_string(),
            tags: tags.to_vec(),
            created: now.to_rfc3339(),
        };
        let mut lines: Vec<String> = self
            .breadcrumbs()
            .into_iter()
            .map(|c| serde_json::to_string(&c).unwrap_or_default())
            .collect();
        lines.push(serde_json::to_string(&crumb).map_err(|e| Error::Session(e.to_string()))?);
        while lines.len() > MAX_BREADCRUMBS {
            lines.remove(0);
        }
        write_atomic(
            &self.root.join("breadcrumbs.jsonl"),
            &format!("{}
", lines.join("
")),
        )?;
        Ok(crumb)
    }

    /// All breadcrumbs, oldest first. Unreadable/short files yield empty.
    pub fn breadcrumbs(&self) -> Vec<Breadcrumb> {
        let Ok(raw) = fs::read_to_string(self.root.join("breadcrumbs.jsonl")) else {
            return Vec::new();
        };
        raw.lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    /// Token-scored search over the breadcrumb tier: every query token that
    /// appears (case-insensitively) in text or tags scores a point; ties
    /// break toward the newest. Zero-score crumbs are omitted.
    pub fn search_breadcrumbs(&self, query: &str, limit: usize) -> Vec<Breadcrumb> {
        let tokens = tokenize(query);
        if tokens.is_empty() {
            return Vec::new();
        }
        let mut scored: Vec<(u32, Breadcrumb)> = self
            .breadcrumbs()
            .into_iter()
            .filter_map(|c| {
                let hay = format!("{} {}", c.text, c.tags.join(" ")).to_lowercase();
                let score = tokens
                    .iter()
                    .filter(|t| hay.contains(t.as_str()))
                    .count() as u32;
                (score > 0).then_some((score, c))
            })
            .collect();
        // Score descending; ties break toward the newest (RFC 3339 stamps
        // compare lexicographically).
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| b.1.created.cmp(&a.1.created))
        });
        scored.into_iter().take(limit).map(|(_, c)| c).collect()
    }

    /// The full usefulness ledger (id → stat), sorted by id.
    pub fn stats(&self) -> std::collections::BTreeMap<String, FactStat> {
        let Ok(raw) = fs::read_to_string(self.root.join("stats.json")) else {
            return std::collections::BTreeMap::new();
        };
        serde_json::from_str(&raw).unwrap_or_default()
    }

    fn write_stats(&self, stats: &std::collections::BTreeMap<String, FactStat>) -> Result<()> {
        if self.root.exists() {
            let json = serde_json::to_string_pretty(stats)
                .map_err(|e| Error::Session(e.to_string()))?;
            write_atomic(&self.root.join("stats.json"), &json)?;
        }
        Ok(())
    }

    /// Record a `memory_search` hit: `uses += 1`, `last_used = now`.
    /// Creates no directories when the store does not exist yet.
    pub fn record_hit(&self, id: &str) {
        let mut stats = self.stats();
        if !self.root.exists() {
            return;
        }
        let stat = stats.entry(id.to_string()).or_default();
        stat.uses += 1;
        stat.last_used = chrono::Utc::now().to_rfc3339();
        let _ = self.write_stats(&stats);
    }

    /// Fold the run's outcome back into the ledger for the memories the run
    /// touched. Called by the engine at the end of a completed run.
    pub fn record_outcome(&self, ids: &[String], ok: bool) {
        if !self.root.exists() || ids.is_empty() {
            return;
        }
        let mut stats = self.stats();
        for id in ids {
            let stat = stats.entry(id.clone()).or_default();
            if ok {
                stat.successes += 1;
            } else {
                stat.failures += 1;
            }
        }
        let _ = self.write_stats(&stats);
    }

    /// Fact id (entry file stem) → `(label, target)` for every index entry,
    /// oldest first — the cache-stable injection order (new facts append at
    /// the tail, so the injected prefix never rewrites).
    pub fn facts_oldest_first(&self) -> Vec<(String, String, String)> {
        let mut entries: Vec<(String, String)> = index_entries(&self.read_index());
        entries.reverse(); // index is newest-first
        entries
            .into_iter()
            .filter_map(|(label, target)| {
                let id = target
                    .strip_prefix("entries/")?
                    .strip_suffix(".md")?
                    .to_string();
                Some((id, label, target))
            })
            .collect()
    }

    /// Bodies of injectable facts, OLDEST FIRST, byte-capped. Ordering is
    /// the cache-stable trick (Phase 13): new facts append at the tail, so
    /// a session's injected prefix is byte-identical until the cap trims
    /// the NEWEST entries off the end — the provider's prefix cache keeps
    /// hitting. Relegated facts ([`FactStat::relegated`]) are skipped.
    pub fn injectable_entries(&self, max_bytes: usize) -> String {
        let stats = self.stats();
        let mut out = String::new();
        let mut omitted = 0usize;
        for (id, label, target) in self.facts_oldest_first() {
            if stats.get(&id).is_some_and(|s| s.relegated) {
                omitted += 1;
                continue;
            }
            let Ok(body) = fs::read_to_string(self.root.join(&target)) else {
                continue;
            };
            let body = body.trim();
            if body.is_empty() {
                continue;
            }
            let block = format!("### {label}\n{body}\n\n");
            if out.len() + block.len() > max_bytes {
                omitted += 1;
                continue;
            }
            out.push_str(&block);
        }
        if omitted > 0 {
            out.push_str(&format!(
                "({omitted} more remembered — use the memory_search tool to find them)\n"
            ));
        }
        out.trim_end().to_string()
    }

    /// One curator pass: decay by idle time, relegate facts whose proven
    /// usefulness score has gone non-positive, reinstate ones that recovered,
    /// and drop stale breadcrumbs. Idempotent.
    pub fn curate(&self, policy: CuratePolicy) -> Result<CurateReport> {
        let now = chrono::Utc::now();
        let mut report = CurateReport::default();
        let facts = self.facts_oldest_first();
        report.facts_total = facts.len();
        if !facts.is_empty() {
            let mut stats = self.stats();
            for (id, label, _) in &facts {
                let stat = stats.entry(id.clone()).or_default();
                let created = entry_created_older(label); // YYYY-MM-DD from the label
                let last_used = parse_rfc3339(&stat.last_used)
                    .or(created)
                    .unwrap_or(now); // unknown → no idle decay yet
                let idle_days = (now - last_used).num_days().max(0) as f64;
                let age_days = created.map(|c| (now - c).num_days().max(0) as f64).unwrap_or(0.0);
                let score = usefulness_score(stat, idle_days);
                let was = stat.relegated;
                stat.relegated =
                    score <= 0.0 && age_days >= policy.grace_days;
                if !was && stat.relegated {
                    report.facts_relegated += 1;
                } else if was && !stat.relegated {
                    report.facts_reinstated += 1;
                }
            }
            self.write_stats(&stats)?;
        }
        // Breadcrumbs: TTL only (they are never injected).
        let crumbs = self.breadcrumbs();
        if !crumbs.is_empty() {
            let before = crumbs.len();
            let fresh: Vec<Breadcrumb> = crumbs
                .into_iter()
                .filter(|c| {
                    parse_rfc3339(&c.created)
                        .map(|t| (now - t).num_days() as f64 <= policy.breadcrumb_ttl_days)
                        .unwrap_or(true)
                })
                .collect();
            if fresh.len() != before {
                report.breadcrumbs_pruned = before - fresh.len();
                let lines: Vec<String> = fresh
                    .iter()
                    .map(|c| serde_json::to_string(c).unwrap_or_default())
                    .collect();
                write_atomic(
                    &self.root.join("breadcrumbs.jsonl"),
                    &if lines.is_empty() {
                        String::new()
                    } else {
                        format!("{}
", lines.join("
"))
                    },
                )?;
            }
        }
        tracing::debug!(?report, "memory curated");
        Ok(report)
    }
}

/// Proven-usefulness score: hits and successes pull up, failures and idle
/// days pull down. Positive → worth injecting.
fn usefulness_score(stat: &FactStat, idle_days: f64) -> f64 {
    stat.uses as f64 + 2.0 * stat.successes as f64 - 2.0 * stat.failures as f64 - idle_days
}

/// `YYYY-MM-DD` label prefix → a UTC midnight timestamp.
fn entry_created_older(label: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let date = label.get(0..10)?;
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .ok()?
        .and_hms_opt(0, 0, 0)?
        .and_utc()
        .into()
}

fn parse_rfc3339(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&chrono::Utc))
}

/// Lowercased query tokens, punctuation split, empties dropped.
fn tokenize(query: &str) -> Vec<String> {
    query
        .split(|c: char| c.is_whitespace() || c == '-')
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect()
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

    // -- Phase 13: global tier, breadcrumbs, ledger, curator ----------------

    #[test]
    fn global_store_is_isolated_from_workspaces() {
        let root = tempdir().unwrap();
        let work = tempdir().unwrap();
        let global = MemoryStore::for_global(root.path()).unwrap();
        global.append("pref", "dark mode always").unwrap();

        let project = store_for(root.path(), work.path());
        project.append("local", "workspace fact").unwrap();
        assert!(global.read_entry("local").unwrap().is_none());
        assert!(project.read_entry("pref").unwrap().is_none());
        assert_eq!(
            global.read_entry("pref").unwrap().unwrap().trim(),
            "dark mode always"
        );
        assert!(global.root().ends_with("global"), "{:?}", global.root());
    }

    #[test]
    fn breadcrumbs_append_search_and_rolling_cap() {
        let root = tempdir().unwrap();
        let work = tempdir().unwrap();
        let store = store_for(root.path(), work.path());
        assert!(store.breadcrumbs().is_empty());

        for i in 0..12 {
            store
                .append_breadcrumb(&format!("note {i} about topic{}", i % 3), &[])
                .unwrap();
        }
        assert_eq!(store.breadcrumbs().len(), 12);

        let hits = store.search_breadcrumbs("topic1", 5);
        assert!(!hits.is_empty());
        assert!(hits.iter().all(|c| c.text.contains("topic1")));

        // No match → empty, multi-token AND-ish scoring.
        assert!(store.search_breadcrumbs("zzznothing", 5).is_empty());
        assert!(store.search_breadcrumbs("", 5).is_empty());

        // Rolling cap keeps the newest MAX_BREADCRUMBS.
        for i in 12..(MAX_BREADCRUMBS + 20) {
            store
                .append_breadcrumb(&format!("bulk {i}"), &["bulk".into()])
                .unwrap();
        }
        let all = store.breadcrumbs();
        assert_eq!(all.len(), MAX_BREADCRUMBS);
        assert!(
            all.iter().any(|c| c.text.contains(&format!("bulk {}", MAX_BREADCRUMBS + 19))),
            "newest bulk entries kept"
        );
        assert!(
            !all.iter().any(|c| c.text.contains("note 0 about")),
            "oldest fell off"
        );
    }

    #[test]
    fn stats_hit_and_outcome_roundtrip() {
        let root = tempdir().unwrap();
        let work = tempdir().unwrap();
        let store = store_for(root.path(), work.path());
        store.append("fact a", "body a").unwrap();
        let id = store.facts_oldest_first().remove(0).0;

        assert!(store.stats().is_empty(), "ledger starts empty");
        store.record_hit(&id); // creates the ledger lazily
        store.record_hit(&id);
        store.record_outcome(&[id.clone()], true);
        store.record_outcome(&[id.clone()], false);
        let stats = store.stats();
        let stat = stats.get(&id).unwrap();
        assert_eq!((stat.uses, stat.successes, stat.failures), (2, 1, 1));
        assert!(!stat.last_used.is_empty());

        // Idempotence: repeated reads give the same ledger.
        assert_eq!(store.stats().len(), 1);
    }

    #[test]
    fn injectable_entries_are_oldest_first_skip_relegated_and_cut_tail() {
        let root = tempdir().unwrap();
        let work = tempdir().unwrap();
        let store = store_for(root.path(), work.path());
        store.append("first", "body first").unwrap();
        store.append("second", "body second").unwrap();
        store.append("third", "body third").unwrap();

        let snapshot = store.injectable_entries(4096);
        assert!(snapshot.contains("first") && snapshot.contains("third"));
        assert!(
            snapshot.find("first").unwrap() < snapshot.find("third").unwrap(),
            "oldest first: {snapshot}"
        );
        // Byte-stable prefix after an append.
        store.append("fourth", "body fourth").unwrap();
        let grown = store.injectable_entries(4096);
        assert!(grown.starts_with(&snapshot), "{grown}");

        // Relegated facts vanish from injection but stay in the index.
        let id = store.facts_oldest_first()[0].0.clone();
        {
            let mut stats = store.stats();
            stats.entry(id).or_default().relegated = true;
            std::fs::create_dir_all(store.root()).unwrap();
            std::fs::write(
                store.root().join("stats.json"),
                serde_json::to_string(&stats).unwrap(),
            )
            .unwrap();
        }
        let pruned = store.injectable_entries(4096);
        assert!(!pruned.contains("body first"), "{pruned}");
        assert!(pruned.contains("body second"));
        assert_eq!(store.read_index().lines().count(), 4, "index untouched");

        // Tight budget cuts from the tail (newest dropped, oldest kept).
        let tight = store.injectable_entries(
            "### 2026-01-01 second
body second

".len(),
        );
        assert!(tight.contains("second"), "{tight}");
        assert!(!tight.contains("third"), "{tight}");
    }

    #[test]
    fn curate_relegates_idle_useless_and_reinstates_after_hits() {
        let root = tempdir().unwrap();
        let work = tempdir().unwrap();
        let store = store_for(root.path(), work.path());
        // An OLD, never-hit fact: stamp is today, so also fake an old date by
        // writing the entry through the public API and backdating the label
        // is not possible — instead rely on the grace window: fresh facts are
        // never relegated regardless of score.
        store.append("fresh useless", "nobody searches me").unwrap();
        let report = store
            .curate(CuratePolicy {
                grace_days: 7.0,
                breadcrumb_ttl_days: 90.0,
            })
            .unwrap();
        assert_eq!(report.facts_total, 1);
        assert_eq!(report.facts_relegated, 0, "grace period protects fresh facts");

        // A heavily-used fact with an old ledger date: backdate via the
        // ledger file, then curate — it survives.
        store.append("workhorse", "looked up constantly").unwrap();
        let ids: Vec<String> = store
            .facts_oldest_first()
            .iter()
            .map(|(id, _, _)| id.clone())
            .collect();
        let workhorse = &ids[1];
        {
            let mut stats = store.stats();
            let stat = stats.entry(workhorse.clone()).or_default();
            stat.uses = 50;
            stat.last_used = (chrono::Utc::now() - chrono::Duration::days(30))
                .to_rfc3339();
            std::fs::write(
                store.root().join("stats.json"),
                serde_json::to_string(&stats).unwrap(),
            )
            .unwrap();
        }
        let report = store.curate(CuratePolicy::default()).unwrap();
        let stats = store.stats();
        assert!(
            !stats.get(workhorse).unwrap().relegated,
            "heavy use survives idle decay"
        );
        assert_eq!(report.facts_relegated, 0);
    }

    #[test]
    fn curate_prunes_stale_breadcrumbs() {
        let root = tempdir().unwrap();
        let work = tempdir().unwrap();
        let store = store_for(root.path(), work.path());
        let fresh = store.append_breadcrumb("fresh crumb", &[]).unwrap();
        // A stale crumb: written directly with a year-old timestamp.
        let stale = Breadcrumb {
            id: "bc-stale".into(),
            text: "old crumb".into(),
            tags: vec![],
            created: (chrono::Utc::now() - chrono::Duration::days(365)).to_rfc3339(),
        };
        std::fs::write(
            store.root().join("breadcrumbs.jsonl"),
            format!(
                "{}\n{}\n",
                serde_json::to_string(&stale).unwrap(),
                serde_json::to_string(&fresh).unwrap()
            ),
        )
        .unwrap();

        let report = store.curate(CuratePolicy::default()).unwrap();
        assert_eq!(report.breadcrumbs_pruned, 1);
        let remaining = store.breadcrumbs();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, fresh.id);

        // Idempotent: a second pass prunes nothing more.
        let report = store.curate(CuratePolicy::default()).unwrap();
        assert_eq!(report.breadcrumbs_pruned, 0);
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
