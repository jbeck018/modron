//! Context assembly — LEDGER item C3.
//!
//! Builds the budgeted system prompt (asserted < 2048 estimated tokens in
//! tests) and loads AGENTS.md / CLAUDE.md memory files from the working
//! directory (size-capped). The prompt is kept terse on purpose: prompt
//! weight is a tax on every turn (PLAN.md §3.1).

use crate::memory::MemoryStore;
use crate::ToolSpec;
use std::path::Path;

/// Memory files are capped at 8 KiB — memory is context, not a dumping ground.
pub const MAX_MEMORY_BYTES: usize = 8 * 1024;

/// Description shown per tool in the prompt is truncated to this many chars.
const TOOL_DESC_MAX_CHARS: usize = 120;

/// The stable, budgeted base prompt. No timestamps or volatile content: this
/// prefix is cache-friendly (PLAN.md §4.6).
const PROMPT_PREAMBLE: &str = "\
You are Modron, a terminal coding agent working in the user's repository.

Rules:
- Prefer deterministic tools (read, glob, grep) over guessing; never invent file contents.
- Keep tool outputs small: narrow read ranges, capped result counts, targeted queries.
- Batch independent tool calls in one turn; sequence calls that depend on earlier results.
- After editing, verify (re-read the diff, run the tests) before claiming success.
- Multi-step goals: maintain the standing plan with the `plan` tool — `set` it first (subtasks with measurable done criteria), then `update` statuses as you go. The plan is re-shown to you every turn; keep it truthful.
- Be concise: findings first, `file:line` references, no filler, no restating the task.
- If you cannot complete the task, say so plainly and stop.";

/// Build the system prompt for a turn: base identity/rules, one line per tool,
/// then the project-memory section when memory is non-empty.
///
/// Budget discipline: with a full built-in tool tier and an 8 KiB memory file
/// this stays well under 2048 estimated tokens (asserted in tests).
pub fn build_system_prompt(tools: &[ToolSpec], memory: &str) -> String {
    let mut prompt = String::from(PROMPT_PREAMBLE);
    if !tools.is_empty() {
        prompt.push_str("\n\nTools:\n");
        for tool in tools {
            prompt.push_str(&format!(
                "- {}: {}\n",
                tool.name,
                brief_description(&tool.description)
            ));
        }
    }
    prompt.push_str(&memory_section(memory));
    prompt.trim_end().to_string()
}

/// Format the project-memory block appended to the system prompt.
/// Empty input yields an empty string — no dead headers in the prompt.
pub fn memory_section(memory: &str) -> String {
    let trimmed = memory.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    format!("\n\n# Project memory (AGENTS.md/CLAUDE.md)\n{trimmed}")
}

/// Load project memory from `dir`: reads `AGENTS.md`, falling back to
/// `CLAUDE.md` (first found wins). Missing files yield an empty string;
/// oversized files are capped at [`MAX_MEMORY_BYTES`] on a char boundary.
pub fn load_memory(dir: &Path) -> String {
    for name in ["AGENTS.md", "CLAUDE.md"] {
        let path = dir.join(name);
        let Some(content) = read_context_file(&path, dir, MAX_MEMORY_BYTES) else {
            continue;
        };
        let capped = truncate_bytes(&content, MAX_MEMORY_BYTES);
        tracing::debug!(path = %path.display(), bytes = capped.len(), "loaded memory file");
        return capped.trim().to_string();
    }
    String::new()
}

/// Reads one instruction or skill file found under `root`, for the context
/// loaders. Never fails the run: a refused or unreadable file is logged with
/// `tracing::warn!` and yields `None`, so the caller skips it.
///
/// - `metadata` (following symlinks) must report a regular file, so a
///   committed `AGENTS.md -> /dev/zero` or a FIFO is never read.
/// - The canonical path must stay inside the canonical `root`, so a symlink to
///   `~/.aws/credentials` or `/etc/...` is never sent to the model.
/// - At most `cap` bytes are read (`take(cap + 1)` detects the oversize case);
///   the text is cut on a UTF-8 boundary at `cap`.
fn read_context_file(path: &Path, root: &Path, cap: usize) -> Option<String> {
    use std::io::Read;
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            tracing::warn!(path = %path.display(), error = %err, "cannot stat context file; ignoring");
            return None;
        }
    };
    if !meta.is_file() {
        tracing::warn!(path = %path.display(), "context file is not a regular file; ignoring");
        return None;
    }
    let canonical_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let canonical = match std::fs::canonicalize(path) {
        Ok(canonical) => canonical,
        Err(err) => {
            tracing::warn!(path = %path.display(), error = %err, "cannot resolve context file; ignoring");
            return None;
        }
    };
    if !canonical.starts_with(&canonical_root) {
        tracing::warn!(
            path = %path.display(),
            target = %canonical.display(),
            "context file resolves outside its directory; ignoring"
        );
        return None;
    }
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) => {
            tracing::warn!(path = %path.display(), error = %err, "failed reading context file; ignoring");
            return None;
        }
    };
    let mut buf = Vec::new();
    if let Err(err) = file.take(cap as u64 + 1).read_to_end(&mut buf) {
        tracing::warn!(path = %path.display(), error = %err, "failed reading context file; ignoring");
        return None;
    }
    let truncated = buf.len() > cap;
    if truncated {
        buf.truncate(cap);
    }
    let valid = match std::str::from_utf8(&buf) {
        Ok(_) => buf.len(),
        // The cap may split a trailing character: keep the valid prefix.
        Err(err) if truncated && err.error_len().is_none() => err.valid_up_to(),
        Err(err) => {
            tracing::warn!(path = %path.display(), error = %err, "context file is not UTF-8; ignoring");
            return None;
        }
    };
    buf.truncate(valid);
    String::from_utf8(buf).ok()
}

/// First line of `description`, truncated to [`TOOL_DESC_MAX_CHARS`] chars.
fn brief_description(description: &str) -> String {
    let first_line = description.lines().next().unwrap_or("").trim();
    let mut out: String = first_line.chars().take(TOOL_DESC_MAX_CHARS).collect();
    if first_line.chars().count() > TOOL_DESC_MAX_CHARS {
        out.push('…');
    }
    out
}

// ---------------------------------------------------------------------------
// User (auto) memory — LEDGER Phase 1a M3, PLAN.md §4.9 user tier.
// Additive layer on top of the repo-tier AGENTS.md/CLAUDE.md loading above;
// the engine composes: with_user_memory(&base, &load_user_memory(root, dir)).
// ---------------------------------------------------------------------------

/// The whole user-memory section is capped at 4 KiB, on top of the 8 KiB
/// project-memory cap and the <2k-token prompt budget.
pub const MAX_USER_MEMORY_BYTES: usize = 4 * 1024;

/// Load the user-tier memory for a workspace: the hot facts (injection-
/// ordered oldest-first, cache-stable, usefulness-pruned via the stats
/// ledger) from the [`MemoryStore`] under `memory_root`. Returns "" when the
/// root/workspace has nothing yet or on any error — memory must never break a
/// session. Byte-stable for identical store contents (cache-friendly prefix).
pub fn load_user_memory(memory_root: &Path, working_dir: &Path) -> String {
    if !memory_root.is_dir() {
        return String::new();
    }
    let store = match MemoryStore::for_workspace(memory_root, working_dir) {
        Ok(store) => store,
        Err(err) => {
            tracing::warn!(error = %err, "failed resolving workspace memory; ignoring");
            return String::new();
        }
    };
    // Reserve room for the header inside the cap.
    let budget = MAX_USER_MEMORY_BYTES.saturating_sub(160);
    let content = store.injectable_entries(budget);
    if content.is_empty() {
        return String::new();
    }
    format!(
        "Facts remembered from earlier sessions in this workspace (use the \
         memory_search tool to find the rest):\n{content}"
    )
    .trim_end()
    .to_string()
}

/// The cross-project tier budget — durable preferences, kept tighter than
/// the project tier.
pub const MAX_GLOBAL_MEMORY_BYTES: usize = 2 * 1024;

/// Load the global (cross-project) memory tier: durable user preferences
/// that follow the user across workspaces. Same injection rules as the
/// project tier. "" when empty — no dead headers.
pub fn load_global_memory(memory_root: &Path) -> String {
    if !memory_root.is_dir() {
        return String::new();
    }
    let store = match MemoryStore::for_global(memory_root) {
        Ok(store) => store,
        Err(err) => {
            tracing::warn!(error = %err, "failed resolving global memory; ignoring");
            return String::new();
        }
    };
    let content = store.injectable_entries(MAX_GLOBAL_MEMORY_BYTES.saturating_sub(160));
    if content.is_empty() {
        return String::new();
    }
    format!("Durable preferences (global, all projects):\n{content}")
        .trim_end()
        .to_string()
}

/// Append the labeled "## Global preferences" section to a base system
/// prompt. Empty input returns `base_system` unchanged.
pub fn with_global_memory(base_system: &str, global_memory: &str) -> String {
    let trimmed = global_memory.trim();
    if trimmed.is_empty() {
        return base_system.to_string();
    }
    format!("{base_system}\n\n## Global preferences\n{trimmed}")
}

/// Append the labeled "## Persistent memory" section to a base system prompt.
/// Empty `user_memory` returns `base_system` unchanged — no dead headers.
/// Pure string composition: byte-deterministic for identical inputs.
pub fn with_user_memory(base_system: &str, user_memory: &str) -> String {
    let trimmed = user_memory.trim();
    if trimmed.is_empty() {
        return base_system.to_string();
    }
    format!("{base_system}\n\n## Persistent memory\n{trimmed}")
}

/// Slice `s` to at most `max` bytes without splitting a UTF-8 code point.
fn truncate_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::estimate_tokens;
    use crate::memory::MemoryStore;
    use serde_json::json;

    /// A plausible built-in tool tier: ~10 tools with realistic descriptions.
    fn plausible_tools() -> Vec<ToolSpec> {
        [
            (
                "read",
                "Read a file with line ranges. Output is capped at 2000 lines / 48KB.",
            ),
            ("glob", "Find files by glob pattern, capped at 200 results."),
            (
                "grep",
                "Regex search across the repo with ripgrep semantics, capped at 50 hits.",
            ),
            (
                "bash",
                "Run a shell command with a 30s timeout; output capped at 10KB with exit code.",
            ),
            (
                "edit",
                "Replace an exact old string with a new string in a file; mismatch is an error.",
            ),
            (
                "write",
                "Write a full file, creating parent directories as needed.",
            ),
            ("list", "List a directory tree with depth and entry caps."),
            (
                "test",
                "Run the project test suite and return a summarized result.",
            ),
            (
                "git",
                "Show git status/diff/log summaries for the working tree.",
            ),
            ("fetch", "Fetch a URL and return cleaned-up markdown text."),
        ]
        .into_iter()
        .map(|(name, description)| ToolSpec {
            name: name.into(),
            description: description.into(),
            parameters: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
        })
        .collect()
    }

    #[test]
    fn system_prompt_stays_under_2048_tokens() {
        // ~3.5KB of memory plus the full tool tier must still fit the budget.
        let memory = "Build notes: run `cargo test` before answering.\n".repeat(80);
        let prompt = build_system_prompt(&plausible_tools(), &memory);
        let tokens = estimate_tokens(&prompt);
        assert!(
            tokens < 2048,
            "system prompt too expensive: {tokens} tokens, {} bytes",
            prompt.len()
        );
    }

    #[test]
    fn system_prompt_lists_tools_and_includes_memory() {
        let prompt = build_system_prompt(&plausible_tools(), "Always answer in French.");
        assert!(prompt.contains("Modron"), "identity missing");
        assert!(prompt.contains("- grep:"), "tool list missing");
        assert!(
            prompt.contains("Always answer in French."),
            "memory content missing"
        );
        assert!(prompt.contains("Project memory"), "memory header missing");
    }

    #[test]
    fn system_prompt_without_memory_has_no_memory_section() {
        let prompt = build_system_prompt(&plausible_tools(), "");
        assert!(!prompt.contains("Project memory"));
        assert!(prompt.contains("- read:"));
    }

    #[test]
    fn load_memory_prefers_agents_md() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "agents rules").unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "claude rules").unwrap();
        assert_eq!(load_memory(dir.path()), "agents rules");
    }

    #[test]
    fn load_memory_falls_back_to_claude_md() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "claude rules").unwrap();
        assert_eq!(load_memory(dir.path()), "claude rules");
    }

    #[test]
    fn load_memory_missing_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_memory(dir.path()), "");
    }

    #[test]
    fn load_memory_caps_at_8kb_on_char_boundary() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "a".repeat(20_000)).unwrap();
        assert_eq!(load_memory(dir.path()).len(), MAX_MEMORY_BYTES);

        // Multi-byte characters must not be split mid-code-point.
        std::fs::write(dir.path().join("AGENTS.md"), "é".repeat(6000)).unwrap(); // 12_000 bytes
        let memory = load_memory(dir.path());
        assert!(memory.len() <= MAX_MEMORY_BYTES);
        assert!(memory.chars().all(|c| c == 'é'));
    }

    // -- User (auto) memory — Phase 1a M3 -----------------------------------

    #[test]
    fn load_user_memory_reads_bodies_oldest_first_cache_stable() {
        let root = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let store = MemoryStore::for_workspace(root.path(), work.path()).unwrap();
        store.append("older fact", "use tabs").unwrap();
        store.append("newer fact", "rust pinned to 1.99.1").unwrap();

        let memory = load_user_memory(root.path(), work.path());
        assert!(
            !memory.contains("Persistent memory"),
            "header comes from with_user_memory"
        );
        assert!(
            memory.contains("Facts remembered from earlier sessions"),
            "{memory}"
        );
        assert!(memory.contains("newer fact") && memory.contains("rust pinned to 1.99.1"));
        assert!(memory.contains("older fact") && memory.contains("use tabs"));
        assert!(
            memory.find("older fact").unwrap() < memory.find("newer fact").unwrap(),
            "oldest first: {memory}"
        );

        // The cache-stable property (Phase 13): appending a fact must leave
        // the previous snapshot as a byte-exact prefix — only the tail grows,
        // so the provider prefix cache keeps hitting.
        store.append("third fact", "tail entry").unwrap();
        let grown = load_user_memory(root.path(), work.path());
        assert!(
            grown.starts_with(&memory),
            "previous snapshot must be a prefix:\n--- before ---\n{memory}\n--- after ---\n{grown}"
        );
        assert!(grown.ends_with("tail entry"), "{grown}");
    }

    #[test]
    fn load_user_memory_over_budget_drops_newest_not_oldest() {
        let root = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let store = MemoryStore::for_workspace(root.path(), work.path()).unwrap();
        for i in 0..5 {
            store
                .append(&format!("fact {i}"), &format!("body {i}"))
                .unwrap();
        }
        // Fill to the cap: ~3.5KB bodies total.
        for i in 0..5 {
            store
                .append(&format!("big {i}"), &format!("{} ", "x".repeat(900)))
                .unwrap();
        }
        let memory = load_user_memory(root.path(), work.path());
        assert!(
            memory.len() <= MAX_USER_MEMORY_BYTES,
            "{} bytes",
            memory.len()
        );
        assert!(
            memory.contains("fact 0") && !memory.contains("big 4"),
            "budget trims the NEWEST (tail) — the cached prefix survives: {memory}"
        );
    }

    #[test]
    fn load_global_memory_and_with_global_memory() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(load_global_memory(root.path()), "", "empty global tier");

        let global = MemoryStore::for_global(root.path()).unwrap();
        global.append("editor", "user prefers helix").unwrap();
        let section = load_global_memory(root.path());
        assert!(section.contains("Durable preferences"), "{section}");
        assert!(section.contains("helix"), "{section}");
        assert!(
            section.len() <= MAX_GLOBAL_MEMORY_BYTES,
            "{} bytes",
            section.len()
        );

        // Project tier stays isolated from global.
        let work = tempfile::tempdir().unwrap();
        let project = MemoryStore::for_workspace(root.path(), work.path()).unwrap();
        project.append("project fact", "workspace only").unwrap();
        assert!(!load_global_memory(root.path()).contains("workspace only"));
        assert!(
            !load_user_memory(root.path(), work.path()).contains("helix"),
            "global does not leak into the project tier"
        );

        assert_eq!(
            with_global_memory("BASE", "  pref  "),
            "BASE\n\n## Global preferences\npref"
        );
        assert_eq!(with_global_memory("BASE", ""), "BASE");
    }

    #[test]
    fn load_global_memory_caps_at_2kb_on_char_boundary() {
        let root = tempfile::tempdir().unwrap();
        let global = MemoryStore::for_global(root.path()).unwrap();
        for i in 0..3 {
            global
                .append(&format!("pref {i}"), &"é".repeat(1200))
                .unwrap();
        }
        let memory = load_global_memory(root.path());
        assert!(
            memory.len() <= MAX_GLOBAL_MEMORY_BYTES,
            "{} bytes",
            memory.len()
        );
        assert!(
            memory.is_char_boundary(memory.len()),
            "no split code points"
        );
    }

    #[test]
    fn load_user_memory_empty_when_nothing_or_missing_root() {
        let root = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        // Fresh root, nothing remembered: empty, and reads create nothing.
        assert_eq!(load_user_memory(root.path(), work.path()), "");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        // Root missing entirely: empty.
        assert_eq!(load_user_memory(&root.path().join("nope"), work.path()), "");
    }

    #[test]
    fn load_user_memory_caps_at_4kb_on_char_boundary() {
        let root = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let store = MemoryStore::for_workspace(root.path(), work.path()).unwrap();
        for i in 0..5 {
            store
                .append(&format!("fact {i}"), &"é".repeat(1500))
                .unwrap(); // 3000 bytes each
        }
        let memory = load_user_memory(root.path(), work.path());
        assert!(
            memory.len() <= MAX_USER_MEMORY_BYTES,
            "{} bytes",
            memory.len()
        );
        assert!(
            memory.is_char_boundary(memory.len()),
            "no split code points"
        );
    }

    #[test]
    fn with_user_memory_appends_labeled_section() {
        assert_eq!(
            with_user_memory("BASE", "  remember this  "),
            "BASE\n\n## Persistent memory\nremember this"
        );
    }

    #[test]
    fn with_user_memory_empty_is_identity_and_deterministic() {
        assert_eq!(with_user_memory("BASE", ""), "BASE");
        assert_eq!(with_user_memory("BASE", "   \n"), "BASE");
        let once = with_user_memory("BASE", "m");
        assert_eq!(once, with_user_memory("BASE", "m"), "byte-deterministic");
    }

    #[test]
    fn system_prompt_with_user_memory_stays_under_2048_tokens() {
        // Project memory the size the C3 test uses, plus a typical user
        // memory produced by a real MemoryStore: index of 10 entries + the
        // 5 newest bodies.
        let root = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let store = MemoryStore::for_workspace(root.path(), work.path()).unwrap();
        for i in 0..10 {
            let body = format!("Decision {i}: {}.", "detail ".repeat(30));
            store.append(&format!("decision {i}"), &body).unwrap();
        }
        let project = "Build notes: run `cargo test` before answering.\n".repeat(80);
        let base = build_system_prompt(&plausible_tools(), &project);
        let prompt = with_user_memory(&base, &load_user_memory(root.path(), work.path()));
        assert!(
            prompt.contains("## Persistent memory"),
            "user section present"
        );
        assert!(prompt.contains("decision 9"), "recent entry present");
        let tokens = estimate_tokens(&prompt);
        assert!(
            tokens < 2048,
            "prompt too expensive: {tokens} tokens, {} bytes",
            prompt.len()
        );
    }
}

/// Bytes read from each SKILL.md for the index (frontmatter only).
const MAX_SKILL_INDEX_BYTES: usize = 64 * 1024;

/// SKILL.md index (Phase 4): `- name: description` per skill found under
/// `<dir>/.modron/skills/*/SKILL.md` and `<dir>/.claude/skills/*/SKILL.md`.
/// The system prompt carries only this index; bodies load via the `skill`
/// tool on demand. Byte-capped at 2KB.
pub fn load_skills_index(working_dir: &std::path::Path) -> String {
    let mut entries: Vec<(String, String)> = Vec::new();
    for root in [
        working_dir.join(".modron").join("skills"),
        working_dir.join(".claude").join("skills"),
    ] {
        let Ok(read_dir) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in read_dir.flatten() {
            // Only the frontmatter is indexed, so a small cap is enough.
            let Some(text) =
                read_context_file(&entry.path().join("SKILL.md"), &root, MAX_SKILL_INDEX_BYTES)
            else {
                continue;
            };
            let (name, description) = parse_skill_frontmatter(&text);
            entries.push((
                name.unwrap_or_else(|| entry.file_name().to_string_lossy().to_string()),
                description.unwrap_or_else(|| "(no description)".to_string()),
            ));
        }
    }
    // Built-in methodology skills ship with the binary; a repo skill with
    // the same name overrides its line (and its body, at load time).
    for (name, description, _) in crate::builtin_skills::BUILTIN_SKILLS {
        if !entries.iter().any(|(n, _)| n == name) {
            entries.push((name.to_string(), description.to_string()));
        }
    }
    let mut out = entries
        .iter()
        .map(|(name, description)| format!("- {name}: {description}"))
        .collect::<Vec<_>>()
        .join("\n");
    if out.len() > 3072 {
        let cut: String = out.chars().take(3072).collect();
        out = format!("{cut}…");
    }
    out
}

/// (name, description) from SKILL.md frontmatter.
pub fn parse_skill_frontmatter(text: &str) -> (Option<String>, Option<String>) {
    let mut lines = text.lines();
    if lines.next() != Some("---") {
        return (None, None);
    }
    let mut name = None;
    let mut description = None;
    for line in lines {
        if line == "---" {
            break;
        }
        if let Some(v) = line.strip_prefix("name:") {
            name = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("description:") {
            description = Some(v.trim().to_string());
        }
    }
    (name, description)
}

#[cfg(test)]
mod skills_tests {
    use super::*;

    #[test]
    fn skills_index_lists_and_parses_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let skill = dir.path().join(".claude").join("skills").join("deploy");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: deploy\ndescription: Ship it safely.\n---\nbody",
        )
        .unwrap();
        let index = load_skills_index(dir.path());
        assert!(index.contains("- deploy: Ship it safely."), "{index}");
        assert_eq!(
            parse_skill_frontmatter("---\nname: a\n---\nrest"),
            (Some("a".into()), None)
        );
        assert_eq!(parse_skill_frontmatter("no frontmatter"), (None, None));
    }

    #[test]
    fn no_skills_dir_still_lists_builtins() {
        let dir = tempfile::tempdir().unwrap();
        let index = load_skills_index(dir.path());
        // Built-in methodology skills ship with the binary.
        assert!(index.contains("subagent-driven-development"), "{index}");
        assert!(index.contains("test-driven-development"), "{index}");
        // A repo skill overrides its built-in twin's line exactly once.
        let skill = dir
            .path()
            .join(".modron")
            .join("skills")
            .join("brainstorming");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: brainstorming\ndescription: OURS.\n---\nbody",
        )
        .unwrap();
        let index = load_skills_index(dir.path());
        assert_eq!(index.matches("brainstorming").count(), 1, "{index}");
        assert!(index.contains("OURS."), "{index}");
    }

    #[test]
    fn normal_agents_md_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "Run cargo fmt.\n").unwrap();
        assert_eq!(load_memory(dir.path()), "Run cargo fmt.");
    }

    #[test]
    fn oversize_regular_memory_file_is_capped_while_reading() {
        let dir = tempfile::tempdir().unwrap();
        let body = "y".repeat(MAX_MEMORY_BYTES * 4);
        std::fs::write(dir.path().join("AGENTS.md"), body).unwrap();
        assert_eq!(load_memory(dir.path()).len(), MAX_MEMORY_BYTES);
    }

    #[cfg(unix)]
    #[test]
    fn agents_md_symlink_outside_repo_is_not_loaded() {
        let repo = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("credentials");
        std::fs::write(&secret, "aws_secret_access_key = hunter2").unwrap();
        std::os::unix::fs::symlink(&secret, repo.path().join("AGENTS.md")).unwrap();
        assert_eq!(load_memory(repo.path()), "");
    }

    #[cfg(unix)]
    #[test]
    fn agents_md_symlink_to_dev_zero_is_not_loaded_and_does_not_hang() {
        let repo = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/dev/zero", repo.path().join("AGENTS.md")).unwrap();
        assert_eq!(load_memory(repo.path()), "");
    }

    #[cfg(unix)]
    #[test]
    fn refused_agents_md_falls_back_to_claude_md() {
        let repo = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/dev/zero", repo.path().join("AGENTS.md")).unwrap();
        std::fs::write(repo.path().join("CLAUDE.md"), "claude rules").unwrap();
        assert_eq!(load_memory(repo.path()), "claude rules");
    }

    #[cfg(unix)]
    #[test]
    fn skill_index_skips_device_symlink_and_returns_quickly() {
        let repo = tempfile::tempdir().unwrap();
        let skill = repo.path().join(".claude").join("skills").join("evil");
        std::fs::create_dir_all(&skill).unwrap();
        std::os::unix::fs::symlink("/dev/zero", skill.join("SKILL.md")).unwrap();
        let index = load_skills_index(repo.path());
        assert!(!index.contains("- evil:"), "{index}");
        assert!(index.contains("subagent-driven-development"), "{index}");
    }
}
