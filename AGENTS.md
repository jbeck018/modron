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

## UI/UX parity contract (terminal target <-> web target)

Status 2026-11-16 (Program U, PLAN.md section 10): the old terminal UI
(crates/modron-tui) is deleted. There is ONE interactive client, the NSH app
under `app/`, with two targets: a terminal binary (`modron-app-tui`, started
by `modron tui`, by bare `modron` on a terminal, or by `modron app`) and a web
bundle (`app/src/web.rs`). Shared logic lives in crates/modron-view (grouped
tool activity, transcript replay, microcopy), crates/modron-protocol (wire
vocabulary) and mobile/rust-core (client core: state, connection, attach).
`mobile/app` (TSX) is frozen: no new features, shared logic only, and it is not
a parity target. New protocol frames follow docs/PROTOCOL_V2.md.

Where the two targets CAN match, they MUST match. Agents keep them in sync by
rule, not by memory.

### Shared invariants (identical in both targets)

1. **Copy and microcopy.** Labels, hints and formatted strings come from
   modron-view or mobile/rust-core: tool summary lines, per-event rows, swarm
   rows, byte formatting, the interrupt affordance, approval decisions (allow
   once / allow all / deny), mode names (ask / yolo / deny). Grep the constants;
   do not retype them.
2. **Grouping and replay.** Tool activity is grouped by modron-view's
   ActivityTracker (one summary line per turn, plus an expandable detail
   panel). Transcripts render replay() / TranscriptEntry order. No target
   reimplements grouping or replay.
3. **Interaction semantics.** Submit sends the task; interrupt aborts the
   active run; approvals resolve on the three decisions, with allow-all sticky
   for the rest of the run; Esc backs out one screen; type-to-filter filters,
   and Esc clears the filter before closing; a model switch pins the model AND
   follows the provider; config changes apply to future turns only.
4. **Status surface.** These facts are visible at all times: connection state,
   current model and provider, cumulative tokens (and cache %), turn and tool
   counts, approval mode, running or idle.
5. **Streaming.** Assistant text streams into a visible in-flight buffer;
   reasoning streams only when the user opted in; buffers commit to history at
   TurnDone and clear on abort.
6. **State mapping.** Every protocol event has one defined effect, as documented
   in modron-protocol (Snapshot resets, Delta appends, Tool updates the tracker,
   ApprovalRequested blocks on a decision, ConfigChanged updates labels).

### Normative divergence table (terminal target vs web target)

| Concern | Terminal target (`modron-app-tui`) | Web target (`app/src/web.rs`) |
|---|---|---|
| Input | Terminal keys (Esc, Ctrl+T, Shift+Tab, Ctrl+J newline, Ctrl+V image paste, `!` and `!!` shell passthrough) | Mouse and native shortcuts; a stop control instead of Esc; an attach control for images |
| Scrollback and selection | Alt screen; PgUp/PgDn/Home/End scroll; terminal selection; wraps at terminal width | Wheel and native scrolling; reflows with the window (content model stays width-independent) |
| Approval surface | Inline status strip, y / n / a keys | Strip with buttons |
| Tool detail panel | Ctrl+T panel, Up/Down to move a row | Tap on the turn summary toggles it |
| Theming | ANSI palette, Oculus tokens | System appearance, semantic roles mapped to browser colors |
| Platform chrome | None (fullscreen terminal) | Browser chrome |
| Remote-control registration | `/remote-control <bridge-url>` slash command, prints the share URL | The same command, from the slash popup or the share control |
| Presentation of per-turn meta | Lines after each turn | Set by the web view in app/src/chat.rs; the words are the shared ones |

Divergence covers PRESENTATION and KEYS only. The words, the states, the
ordering and the semantics do not diverge. A row not listed here must match.

### Process rules for agents

1. When you change user-visible copy, grouping or state mapping, change the
   shared source (modron-view, mobile/rust-core, modron-protocol), then grep
   `app/` for the affected strings or behavior and update both targets in the
   same commit, with tests for both.
2. Never duplicate a format function or grouping rule locally; import it from
   modron-view or mobile/rust-core. If it needs to change, change it there.
3. A new interactive affordance lands in both targets in the same increment,
   or gets an explicit row in the divergence table in the same commit. No
   silent one-sided features.
4. New protocol events get a defined effect documented in modron-protocol's doc
   comment before any target renders them.
5. Regression tests guard the invariants: the app's Rust tests assert AppState
   and terminal-view projections, and `app/web-test.js` checks the web target.
   Add the equivalent wherever a new target lands.
6. Do not add a third client. A new client is a new target of `app/`.
7. Comments in app/ and mobile/rust-core that cite crates/modron-tui paths are
   history references (commit `0ac5423`); do not mass-edit them.
