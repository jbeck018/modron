# AGENTS.md — operating rules for coding agents working on Modron

Modron is a terminal-first Rust coding agent (8+ crate workspace, single
`modron` binary). Architecture and phase history live in PLAN.md and
LEDGER.md; frozen cross-crate contracts live in crates/modron-core/src/lib.rs.

## Engineering gates (every increment, no exceptions)

1. `cargo fmt --all` before any commit.
2. `cargo clippy --workspace --all-targets -- -D warnings` clean.
3. `cargo test --workspace` green. Never ship a suite regression.
4. Before committing a change that adds modules: `git status --short | grep "^??"`
   — an untracked .rs file fails CI fmt ("does not exist") even though local
   tests passed.
5. Never `git add -A` in a multi-agent workspace; add only your own paths.
6. Internal path dependencies carry inline versions
   (`modron-core = { path = "../modron-core", version = "0.1.0" }`) —
   cargo-deny's wildcard ban rejects version-less path deps.
7. Docs and commit messages in plain ASCII.

## UI/UX parity contract (TUI <-> desktop <-> mobile)

The TUI (crates/modron-tui), the desktop app (crates/modron-desktop), and the
mobile clients (mobile/) are views over the same session state. Where they
CAN match, they MUST match; agents keep them in sync by rule, not by memory.
The shared substrate is crates/modron-view (grouped tool activity, transcript
replay) and crates/modron-protocol (wire vocabulary) — logic goes there, never
forked per client.

### Shared invariants (must be identical in every client)

1. **Copy and microcopy.** Labels, hints, and formatted strings come from
   modron-view or shared constants: tool summary lines (`⏺ 21 tools · glob
   ×9 · read ×8 …`), per-event rows (`✓ read — 2.1 KB`, `● bash …`), swarm
   rows (`⏵ [label] done · 3 turns · 1.2k tok · last: read`), byte
   formatting (`human_bytes`), interrupt affordance ("esc to interrupt" /
   the GUI's stop control), approval decisions (allow once / allow all /
   deny), mode names (ask / yolo / deny), theme-independent wording.
2. **Grouping semantics.** Tool activity is grouped exactly as
   modron-view::ActivityTracker does (one collapsed summary line per turn +
   an expandable detail panel); transcripts render replay()/TranscriptEntry
   order (task echo, assistant text, tool summary, swarm block, model and
   usage notes). No client reimplements grouping or replay.
3. **Interaction semantics.** Submit sends the task; interrupt aborts the
   active run; approvals resolve on the three decisions with allow-all
   sticky for the rest of the run; modal/list behaviors match (Esc backs out
   one screen; type-to-filter filters; Esc clears the filter before
   closing); /model-equivalent model switches pin the model AND follow the
   provider; config changes apply to future turns only.
4. **Status surface.** The same facts are visible at all times: connection
   state, current model/provider, cumulative tokens (and cache %), turn and
   tool counts, approval mode, running/idle.
5. **Streaming behavior.** Assistant text streams into a visible in-flight
   buffer; reasoning streams only when the user opted in; buffers commit to
   history at TurnDone and clear on abort.
6. **State mapping.** Every protocol event has one defined UI effect per the
   modron-protocol docs (Snapshot resets, Delta appends, Tool updates the
   tracker, ApprovalRequested blocks on a decision, ConfigChanged updates
   labels).

### Normative divergence table (allowed differences — anything not listed here must match)

| Concern | TUI | Desktop / mobile |
|---|---|---|
| Input | Terminal keys (esc, ctrl+t, shift+tab, `\` line continuation, `!` shell passthrough, `@file`) | Mouse + native shortcuts; a stop button instead of esc; `!`/`@` optional |
| Scrollback & selection | Alt-screen + internal scrollback, pgup/pgdn, push-time wrapping at terminal width, terminal mouse selection | Native scrolling/selection; reflows with window (content model stays width-independent) |
| Approval surface | Inline status strip, y/n/a keys | Strip/buttons (desktop), sheet (mobile) |
| Tool detail panel | ctrl+t overlay, click-to-open | Panel/drawer, click-to-open |
| Theming | ANSI/ratatui palette themes | System appearance + semantic roles mapped to native colors |
| Platform chrome | None (fullscreen terminal) | Native menus, dock, safe areas (mobile), virtual keyboard handling |
| Layout | Single column, status line + live strip | Sidebar + panes / mobile stack |
| Per-turn meta (model switches, usage notes) | Dim scrollback lines after each turn | Desktop: centered divider chip on model CHANGE only, per-turn usage folded into the status bar; mobile: usage notes |

Divergence covers PRESENTATION and KEYS only. The words, the states, the
ordering, and the semantics do not diverge.

### Process rules for agents

1. When you change user-visible copy, grouping, or state mapping, grep both
   clients for the affected strings/behavior and update them in the same
   commit, with tests in both.
2. Never duplicate a format function or grouping rule locally; import it
   from modron-view. If it needs to change, change it there.
3. A new interactive affordance lands in every client in the same increment,
   or gets an explicit row added to the divergence table above in the same
   commit. No silent one-sided features.
4. New protocol events get a defined AppState/TUI effect documented in
   modron-protocol's doc comment before any client renders them.
5. Visual regression tests guard the invariants (the TUI already asserts its
   design in view tests; the desktop app asserts AppState projections; add
   the equivalent wherever a new client lands).
