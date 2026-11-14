# Security and correctness remediation plan (2026-11-14 review)

Source: the 2026-11-14 read-only review of the whole repo (5 area reviews +
CI/infra pass). Baseline at review time: clippy clean, `cargo test --workspace
--exclude modron-desktop` green on Linux.

This file is BOTH the plan and the ledger for this effort. It is written so
that small, cheap worker agents (Haiku) can each take one packet and finish
it without needing the rest of the review in context.

---

## 1. Operating model: leader / ledger / worker

**Leader** (one strong-model session):
- Owns this file. Single-writer rule: only the leader edits the ledger table
  in section 5. Workers never touch this file.
- Picks the next packet per lane, spawns one worker per packet with the
  prompt template in section 3, in an isolated git worktree.
- Reviews every worker diff against the packet's acceptance criteria, runs
  the gates itself (does not trust a worker's "tests pass"), merges into
  `claude/repo-security-review-udrusy`, records evidence in the ledger.
- Does the packets marked `tier: L` itself (design-heavy or cross-cutting).
- Arbitrates anything a worker reports as BLOCKED.

**Worker** (Haiku, one packet, one worktree):
- Reads AGENTS.md and exactly one packet. Edits only the files listed under
  `files`. If the fix needs another file, STOP and report BLOCKED; do not
  widen scope.
- Writes the tests named in the packet first (they must fail before the fix
  and pass after), then the fix.
- Runs the gates, commits on its worktree branch, and reports using the
  report format in section 3. Does not push, does not open PRs.

**Routing rules:**
- A lane is a set of packets that touch the same files. Packets inside a
  lane run strictly in the listed order, one at a time. Different lanes run
  in parallel.
- At most 5 workers at once.
- A worker gets ONE retry with the leader's review notes. A second failure
  escalates the packet to the leader (or a Sonnet worker); record that in
  the ledger.
- `tier: H` = Haiku. `tier: L` = leader. `tier: H+` = Haiku, but the leader
  must read every line of the diff (security-critical).
- Line numbers below were correct at commit `be164e1`. Workers must locate
  code by the quoted symbol names, not by line number alone.

**Gates (every packet, from AGENTS.md):**
1. `cargo fmt --all`
2. `cargo clippy --workspace --exclude modron-desktop --all-targets -- -D warnings`
   (Linux cannot build the desktop crate; packets touching
   `crates/modron-desktop` are checked with `cargo check -p modron-desktop`
   on macOS by the leader or flagged `needs-macos` in the ledger.)
3. `cargo test --workspace --exclude modron-desktop`
4. `git status --short | grep "^??"` shows no untracked .rs file.
5. `git add` only the packet's own paths. Never `git add -A`.
6. Docs and commit messages in plain ASCII.
7. Parity contract: a packet that changes user-visible copy or interaction
   semantics lists every client it must touch; all land in one commit.

---

## 2. Design decisions already made (workers do not re-decide these)

D1. **Approval gate stays a gated list, extended** (owner decision
    2026-11-14). `MUTATING_TOOLS` becomes `["bash", "write", "edit",
    "verify", "diagnostics", "interact", "delegate"]`, and any tool whose
    name starts with `mcp__` is gated as well. `remember` stays ungated
    (accepted risk, tracked in the backlog). The existing `allow` list
    remains the way to pre-approve a tool (e.g. `--allow diagnostics`).
    Consequence: a tool added later is ungated unless it is added to this
    list, so every new tool needs an explicit gated/ungated decision.

D2. **Subagents inherit, never widen.** A child engine copies the parent's
    `sandbox`, `hooks`, `max_cost_usd`, `price_lookup`, and the UNION of the
    parent's deny list and the persona's deny list. Allow list = persona's
    allow list intersected with nothing the parent denies.

D3. **TCP is never implicitly trusted for browsers.** The WebSocket upgrade
    rejects any request that carries an `Origin` header unless it is in an
    explicit allowlist (empty by default). A new `--require-token` flag on
    `modron serve` forces `trusted = false` even on loopback. The Unix
    socket stays trusted. Docs for tailscale-serve and cloudflared use
    `--require-token`.

D4. **Share key = full 32 hex chars** of a v4 UUID (122 random bits). The
    bridge `KEY_RE` already accepts up to 64 chars.

D5. **Tunnel connections are scoped, not trusted.** A tunnel `ConnCtx` is
    pinned to its one session id and may only use: hello, ping, attach (own
    session), submit, interrupt, approve. Everything else (set_config,
    auth_set_key, automation_*, delete_session, merge_task, remote_control,
    list_sessions, fork, publish) returns an error frame of kind
    `forbidden`.

D6. **"Allow all" is per run.** Cleared when a run starts and when it ends,
    in the server hub and in every client.

D7. **Approval prompts show the essential argument first, in full on
    demand.** One shared function in modron-view produces the prompt
    summary: for `bash` the command, for `write`/`edit` the path, otherwise
    compact JSON; control and bidi characters replaced with `?`. When the
    text is truncated the summary ends with a visible ` [+N chars]` marker,
    and every client gets a "view full call" affordance (TUI: ctrl+t panel;
    desktop: detail panel; mobile: expandable sheet).

D8. **Checkpoints distinguish three states**: `Absent` (file did not
    exist), `Captured(b64)`, `Uncaptured` (too large / unreadable). Rewind
    deletes only on `Absent`; on `Uncaptured` it refuses with an actionable
    message and changes nothing.

D9. **Children never inherit model-provider secrets.** Every spawned tool
    process (bash, verify, diagnostics, interact, hooks, lsp) has these env
    vars removed, via `modron_core::hooks::is_secret_env`: names ending in
    `_API_KEY`, names starting with `ANTHROPIC_`, `OPENAI_`, `ZAI_`, and
    Modron's own `GITHUB_MODELS_TOKEN`, `MODRON_GITHUB_TOKEN`,
    `MODRON_DEVICE_TOKEN`, `MODRON_PUBLISH_TOKEN`. Generic `_TOKEN`,
    `_SECRET`, `_PASSWORD` names are deliberately KEPT (leader decision at
    D3 review): the agent's shell needs `GITHUB_TOKEN` and similar to run
    gh, git push, cargo publish and tests. MCP servers are exempt (they
    are user-configured and may need their own keys). Not yet covered: the
    sandboxed bash path (D6) and the git spawns in delegate.rs.

---

## 3. Worker prompt template

```
You are a worker on the Modron repo (Rust workspace). Do exactly one packet.

1. Read /AGENTS.md (engineering gates + parity contract).
2. Read packet <ID> in docs/SECURITY_PLAN.md, plus section 2 (decisions).
   Do not read or act on other packets.
3. Edit ONLY the files listed under `files` in the packet. If you believe
   another file must change, stop and report BLOCKED with the reason.
4. Write the packet's tests first; confirm they fail; implement; confirm
   they pass.
5. Run the gates in section 1. All must pass.
6. Commit on your current branch with message:
     fix(<area>): <packet ID> <one-line summary>
   Do not push. Do not edit docs/SECURITY_PLAN.md or LEDGER.md.
7. Final message, exactly this shape:
     PACKET: <ID>
     STATUS: DONE | BLOCKED
     COMMIT: <sha>
     FILES: <paths changed>
     TESTS ADDED: <test names>
     GATES: fmt <ok/fail> | clippy <ok/fail> | test <ok/fail>
     NOTES: <anything the leader must know; deviations; follow-ups>
```

---

## 4. Packets

Format: `ID | tier | severity` then problem, files, change, acceptance.

### Lane A - approval gate and engine (`crates/modron-core/src/engine.rs`)

Strictly serial: A1 -> A2 -> A3 -> A4 -> A5 -> A6 -> A7.

**A1 | H+ | critical - Gate diagnostics, interact, delegate and MCP tools (D1)**
- Problem: `MUTATING_TOOLS` (engine.rs, near line 30) gates only bash,
  write, edit, verify. `diagnostics` (runs `sh -c` on a model-supplied
  `command`), `interact` (tmux), `delegate`, and all `mcp__*` tools run
  with no approval in ask and deny mode.
- Files: `crates/modron-core/src/engine.rs`,
  `crates/modron-core/tests/interactivity.rs`.
- Change: add one helper `fn is_gated(name: &str) -> bool` that returns
  true for the D1 list or a `mcp__` prefix, and use it at the approval
  site (search `MUTATING_TOOLS.contains`). Every OTHER use of the constant
  (finish-gate arming, checkpoint) must keep its current behavior: give
  those a separate `FILE_MUTATING_TOOLS = ["bash", "write", "edit",
  "verify"]`. Add a doc comment on the list stating that every new tool
  needs an explicit gated/ungated decision.
- Acceptance: new tests - in Ask mode with no approver, calls named
  `diagnostics`, `interact`, `delegate`, `mcp__x__y` are each blocked;
  `read`/`grep`/`glob`/`remember` are not; `--allow diagnostics` unblocks
  diagnostics. Existing suite green.

**A2 | H+ | high - Subagents inherit sandbox, deny, hooks, cost cap (D2)**
- Problem: `subagent.rs` builds the child `EngineConfig` with
  `..EngineConfig::default()` (search `max_turns: 24`), dropping sandbox,
  hooks, parent deny, `max_cost_usd`, `price_lookup`.
- Files: `crates/modron-core/src/subagent.rs` (+ its tests module).
- Change: per D2.
- Acceptance: unit test builds a parent with sandbox=Bubblewrap,
  deny=["bash"], one pre_tool hook, max_cost_usd=Some(1.0) and asserts the
  child config has all four (deny includes "bash" plus persona denies).

**A3 | H+ | high - Three-state checkpoint; rewind never deletes an uncaptured file (D8)**
- Problem: `checkpoint_for` records `prior_content_b64: None` for both
  "did not exist" and "over 256 KB / unreadable"; `rewind_last_checkpoint`
  deletes on `None`. Also it does `std::fs::read` of a model-chosen path
  before any gate (hangs on /dev/zero or a FIFO; stores secrets in the log).
- Files: `crates/modron-core/src/engine.rs`,
  `crates/modron-core/src/rewind.rs`, `crates/modron-types/src/lib.rs`
  (Checkpoint event; add an optional `uncaptured: bool` field with
  `#[serde(default)]` so old logs still load).
- Change: (1) use `std::fs::symlink_metadata` first: if not a regular file
  or `len > SNAPSHOT_CAP`, record uncaptured=true WITHOUT reading. (2) only
  checkpoint paths that resolve inside `working_dir`. (3) move the
  `checkpoint_for` call to AFTER the deny/approval gate so a refused call
  records nothing. (4) checkpoint in every mode, not only ApproveAll.
  (5) rewind: `uncaptured` => return an error "cannot rewind <path>: the
  original was not captured (too large or unreadable); restore it from
  git", touching nothing.
- Acceptance: tests - 300 KB pre-existing file, edit, rewind => file still
  exists and error mentions "not captured"; new file, rewind => deleted;
  small file => restored; path outside working_dir => no checkpoint event;
  an old-format log (no `uncaptured` field) still parses.

**A4 | H | high - Prune unanswered tool calls on every resume**
- Problem: `prune_unanswered_tool_calls` runs only when
  `resume_from_event` is set. After an interrupt mid-tool, a normal resume
  replays an assistant `tool_use` with no `tool_result`.
- Files: `crates/modron-core/src/engine.rs`,
  `crates/modron-core/tests/engine_test.rs`.
- Change: apply the prune in the `None =>` arm too (the `reconstruct_from`
  match near the "Rewind support (C2)" comment).
- Acceptance: test - log ends with an assistant message holding a tool call
  and no result; resuming builds a transcript with no unanswered tool call.

**A5 | H | medium - Finish gate must not bypass deny/approval**
- Problem: the finish gate arms when a mutating call is merely requested
  (even if blocked) and calls `verify_tool.execute` directly, past the deny
  list, approval, and hooks.
- Files: `crates/modron-core/src/engine.rs`.
- Change: arm `mutated_this_run` only when a file-mutating call actually
  executed successfully; route the finish-gate verify through the same gate
  function as model-issued calls (deny => skip verify with a note; Ask with
  no approver => skip).
- Acceptance: tests - blocked edit does not trigger verify; `--deny verify`
  never runs verify.

**A6 | H | medium - Self-check false failure note and swallowed errors**
- Problem: (a) the self-check branch increments `verify_rounds` without
  clearing `mutated_this_run`, producing two self-check turns and a false
  "[verification still failing after 2 rounds]" note when no verify tool is
  configured; (b) once `self_check_sent` is set, any later provider error is
  swallowed and the run returns Ok.
- Files: `crates/modron-core/src/engine.rs`.
- Change: (a) clear the flag after one self-check; never append the
  "verification still failing" note unless a verify tool actually ran and
  failed. (b) swallow the provider error only on the turn immediately after
  the self-check message; later errors propagate.
- Acceptance: tests for both, using the mock provider.

**A7 | L | medium - Compaction summarises only since the last compaction**
- Problem: `run_compaction` re-summarises the whole log from event 0 and
  ignores the rewind cut; request grows until the summariser 400s.
- Files: `engine.rs`, `compact.rs`. Leader-owned (design).

### Lane B - server and remote surface (`crates/modron-server`, `cloud/bridge`, docs)

Serial: B1 -> B2 -> B3 -> B4 -> B5 -> B6 -> B7. B8 (docs) after B1.

**B1 | H+ | critical - Reject browser origins; add --require-token (D3)**
- Problem: `serve.rs` sets `trusted = bound.ip().is_loopback()`; `ws.rs`
  uses `accept_async_with_config` with no header callback. Any web page can
  open `ws://127.0.0.1:7699` and drive the agent.
- Files: `crates/modron-server/src/ws.rs`, `serve.rs`,
  `crates/modron-cli/src/main.rs` (flag plumbing only: add `--require-token`
  to the `serve` subcommand and pass it into the serve options),
  `crates/modron-server/tests/integration.rs`.
- Change: switch to `accept_hdr_async_with_config`; in the callback, if the
  request has an `Origin` header not in `options.allowed_origins` (default
  empty), respond 403. `trusted = bound.ip().is_loopback() &&
  !options.require_token`.
- Acceptance: tests - upgrade with `Origin: https://evil.example` is
  refused; upgrade with no Origin on loopback still works; with
  require_token=true a loopback hello without a token is rejected.

**B2 | H+ | high - 122-bit share key; scoped tunnel context (D4, D5)**
- Problem: `share_key()` in `rc.rs` returns 8 hex chars; tunnel frames go
  through `handle_frame(&state, &mut ctx, true, frame)` as fully trusted.
- Files: `crates/modron-server/src/rc.rs`, `dispatch.rs`,
  `crates/modron-server/tests/` (new `rc.rs` test file).
- Change: key = full `Uuid::new_v4().simple()` string. Add
  `scope: Option<SessionId>` to `ConnCtx`; when set, `handle_frame` applies
  the D5 allowlist and forces attach/submit/approve/interrupt to that
  session id. The tunnel marks its ctx authed at creation (fixes the
  30-second ping closing the tunnel) and stops logging the key.
- Acceptance: tests - key length 32; scoped ctx gets `forbidden` for
  set_config, auth_set_key, automation_add, delete_session, list_sessions;
  scoped attach to another session id is refused; a Ping on a fresh tunnel
  ctx does not return `Flow::Close`.

**B3 | H | high - "Allow all" is per run in the hub (D6)**
- Problem: `allow_rest` in `hub.rs` is set on AllowAll and never cleared.
- Files: `crates/modron-server/src/hub.rs`, `tests/hub.rs`.
- Change: `handle.allow_rest.store(false, ..)` in `start_run` next to the
  `aborted` reset, and again in `complete`.
- Acceptance: test - run 1 answers allow_all; run 2's first mutating call
  produces a new ApprovalRequested.

**B4 | H | medium - Pre-auth limits and bounded queues**
- Problem: 256 MiB message cap applies before hello; no handshake timeout;
  per-connection outbound channel is unbounded.
- Files: `crates/modron-server/src/ws.rs`, `unix.rs`.
- Change: first frame must arrive within 10 s and be <= 64 KiB (read it
  with a small-limit config, or check length before parse); outbound channel
  becomes bounded (4096) and a full queue closes that connection;
  `unix.rs` enforces the 16 MiB line cap while reading, not after.
- Acceptance: tests - 1 MiB first frame closes the connection; silent
  connection is dropped after the timeout (use tokio time pause).

**B5 | H | medium - Automations: registry out of the repo, sane intervals**
- Problem: registry is `<working_dir>/.modron/automations.json`, so a
  cloned repo can ship prompts that auto-fire; `interval_minutes: 0` is
  accepted; no overlap bound.
- Files: `crates/modron-server/src/automations.rs` (+ tests).
- Change: registry path moves to the user data dir keyed by a hash of the
  canonical working dir (`~/.modron/automations/<sha256-prefix>.json`); do
  NOT read the in-repo file. Reject interval < 5 minutes. Skip a fire while
  the previous run of the same automation is active.
- Acceptance: tests - in-repo file is ignored; interval 0 rejected; overlap
  skipped.

**B6 | H | medium - Bridge page publish auth and limits**
- Problem: `handlePagePublish` in `cloud/bridge/src/index.ts` checks
  `Authorization === "Bearer " + key` where `key` is the request path.
- Files: `cloud/bridge/src/index.ts`, `cloud/bridge/README.md`,
  `cloud/bridge/wrangler.jsonc`.
- Change: require a deploy-time secret `PUBLISH_TOKEN` (wrangler secret,
  documented in README): `Authorization: Bearer <PUBLISH_TOKEN>`; if the
  secret is unset, publishing returns 503. Reject by `Content-Length`
  before reading the body; measure size in bytes (TextEncoder). Fix the
  stale "128-bit random" / "1MB" / "256 KB" comments to match the code.
- Acceptance: `npx tsc --noEmit` clean in `cloud/bridge`; README documents
  the secret. Note in report: the Rust publish client must send the token
  (follow-up packet F5).

**B7 | H | low - tasks.rs merge correctness**
- Problem: `merge_task` runs `git merge` in whatever is checked out, not
  `record.base`; `merge --abort` runs before the unmerged-file listing so
  conflicts are always reported empty; `base` is passed to git with no
  leading-dash check.
- Files: `crates/modron-server/src/tasks.rs` (+ tests).
- Change: refuse to merge unless HEAD is `record.base` (actionable error);
  collect `git diff --name-only --diff-filter=U` BEFORE `merge --abort`;
  reject a `base` starting with `-`.
- Acceptance: tests using a temp git repo for all three.

**B8 | H | critical(doc) - Correct docs/remote-control.md**
- Depends on B1.
- Files: `docs/remote-control.md`.
- Change: tailscale-serve and cloudflared recipes use
  `modron serve --listen 127.0.0.1:7699 --require-token` plus `modron pair`;
  delete the claim that the token gate applies to a plain loopback bind
  behind a proxy; add a short "why" paragraph (a proxy makes every peer
  look like loopback). State that the share key grants submit/approve on
  one session (D5).
- Acceptance: leader read-through; plain ASCII.

### Lane C - clients and parity (`modron-view`, `modron-tui`, `modron-desktop`, `mobile`, `modron-protocol`)

Serial: C1 -> C2 -> C3 -> C4 -> C5 -> C6.

**C1 | H+ | high - Shared approval summary, no hidden arguments (D7)**
- Problem: TUI shows `short(args_json, 80)`; desktop `short(.., 80)`;
  mobile cuts at 120 then clamps to 2 lines. A long command hides its tail;
  a write hides its path.
- Files: `crates/modron-view/src/lib.rs` (new `approval_summary(name,
  args, max) -> String` + tests), `crates/modron-tui/src/lib.rs`
  (`status_line`), `crates/modron-desktop/src/app_state.rs`,
  `mobile/rust-core/src/view.rs`, `mobile/app/App.tsx` (render
  `PendingApprovalView.arguments` in an expandable block; remove the
  2-line clamp on the summary).
- Change: per D7. All three clients call the one modron-view function.
- Acceptance: modron-view tests - bash summary starts with the command;
  write/edit summary starts with the path; truncation ends with
  ` [+N chars]`; U+202E and ESC are replaced. TUI, desktop AppState and
  mobile view tests assert they render the shared string. `needs-macos`
  for the desktop check.

**C2 | H | high - TUI: "allow all" resets per run; approval state cleanup (D6)**
- Problem: `approve_rest` (`lib.rs`) and `rest_approved` (`run.rs`) are set
  once and never cleared; `pending_approval` is not cleared on done /
  failed / aborted / disconnect, and the stale modal swallows keys.
- Files: `crates/modron-tui/src/lib.rs`, `run.rs`, `tests/state.rs`.
- Change: clear both flags and `pending_approval` when a run starts and on
  TurnDone-final / failed / aborted / Disconnected, and on `/new` and
  `/mode`. In remote mode handle `ApprovalResolved` (clear the matching
  prompt) and adopt `live.pending_approval` from the snapshot.
- Acceptance: state tests - `a` in task 1 does not auto-approve task 2;
  abort clears the modal; snapshot with a pending approval shows the prompt.

**C3 | H | medium - TUI remote command bugs**
- Files: `crates/modron-tui/src/run.rs`, `tests/state.rs`.
- Change: (a) `PendingCommand::Sandbox`: unknown value => note "unknown
  sandbox `<x>` (off | seatbelt | bubblewrap)" and send nothing (today it
  maps to Off). (b) remote `/allow` and `/deny` send the full current list
  plus the new entry, not a single-element list. (c)
  `LoopCmd::RemoteControl` with no session posts a note instead of `break`.
- Acceptance: one test per item.

**C4 | H | medium - Mobile wire values, disconnect, deny-list wipe**
- Files: `mobile/app/App.tsx`, `mobile/app/App.test.tsx`,
  `mobile/rust-core/src/client.rs`, `mobile/rust-core/tests/remote_client.rs`.
- Change: (a) `APPROVAL_CYCLE`/`SANDBOX_CYCLE` send wire names
  (`approve_all`, `bubblewrap`) while the label still reads "yolo"; unknown
  label falls back to `ask`, never yolo. (b) `disconnect()` actually closes
  the socket (drop the sender held by `establish` / send an explicit close
  command) and a new connect tears down the old one. (c) `saveTools` sends
  only the list the user edited.
- Acceptance: App.test covers the wire values against a non-demo mock;
  remote_client test asserts the server side sees the close.

**C5 | H+ | medium - Clients show the real approval mode on attach**
- Problem: `LiveState` in modron-protocol carries no approval mode,
  sandbox, allow or deny; all clients default the label to "ask".
- Files: `crates/modron-protocol/src/lib.rs` (add optional fields with
  serde defaults + doc comment per AGENTS.md process rule 4),
  `crates/modron-server/src/hub.rs` (populate), desktop `app_state.rs`,
  TUI `run.rs`, mobile `view.rs`.
- Acceptance: tests in each client - snapshot with
  `approval_mode = approve_all` renders "yolo"; old snapshots without the
  field still parse.

**C6 | H | high - TLS for ws clients; clients can send a device token**
- Problem: workspace `tokio-tungstenite` has no TLS feature, so TUI,
  desktop and the server's bridge tunnel cannot dial `wss://`; hello
  tokens are hard-coded `None` in every client.
- Files: root `Cargo.toml` (enable `rustls-tls-webpki-roots` on
  tokio-tungstenite), `crates/modron-desktop/Cargo.toml` (use
  `.workspace = true`), `crates/modron-tui/src/remote.rs`,
  `crates/modron-desktop/src/wire.rs`, `mobile/app/App.tsx` (token field
  on the connect form), `Cargo.lock`.
- Change: token source = `MODRON_DEVICE_TOKEN` env or
  `?token=` stripped from the URL before logging (TUI/desktop); a field on
  the mobile connect form. Mobile refuses `ws://` to non-loopback hosts
  unless an explicit "allow insecure" toggle is on.
- Acceptance: `cargo deny check` still passes (leader runs it); tests -
  hello frame carries the token when set; mobile rejects
  `ws://example.com` by default.

### Lane D - tools (`crates/modron-tools`, `modron-core/src/sandbox.rs`, `hooks.rs`)

Serial: D1 -> D2 -> D3 -> D4 -> D5 -> D6.

**D1 | H+ | medium - Safe atomic write**
- Problem: `write_atomic` in `common.rs` uses `path.with_extension(
  "modron-tmp")` (predictable, follows a planted symlink, shared between
  `a.rs` and `a.ts`) and resets the mode to 0644.
- Files: `crates/modron-tools/src/common.rs` (+ tests).
- Change: create the temp file in the same directory with a random suffix
  using `OpenOptions::create_new(true)`; copy the original file's
  permission bits onto it before rename; if the target is a symlink, write
  through to the link target's path rather than replacing the link.
- Acceptance: tests - pre-planted `x.modron-tmp` symlink is not followed;
  0755 script stays 0755; symlinked target stays a symlink.

**D2 | H | medium - bash: interrupt kills, no pipe hang, capped sandbox output**
- Files: `crates/modron-tools/src/bash.rs` (+ tests).
- Change: `kill_on_drop(true)` plus a drop guard that SIGKILLs the process
  group; after `child.wait()` give the pipe readers a 2 s grace timeout
  then stop reading; the sandboxed path uses the same capped reader instead
  of `.output()`; on the 1 MB cap append "[output truncated at 1 MB]"
  instead of reporting "terminated by signal".
- Acceptance: tests - `sleep 300 &` returns promptly; dropping the future
  kills the child (check the pid is gone); `yes | head -c 5000000` reports
  truncation with a success status.

**D3 | H+ | medium - Scrub provider secrets from child environments (D9)**
- Files: `crates/modron-tools/src/common.rs` (new `scrub_env(&mut
  Command)`), `bash.rs`, `verify.rs`, `diagnostics.rs`, `interact.rs`,
  `lsp.rs`, `crates/modron-core/src/hooks.rs`.
- Acceptance: test - with `ANTHROPIC_API_KEY=x` set in the test process,
  `bash: env` output does not contain it; `PATH` and `HOME` survive.

**D4 | H | medium - Protected-path check resolves symlinks and case**
- Problem: `is_protected_path` in `edit.rs` is lexical on the raw argument.
- Files: `crates/modron-tools/src/edit.rs`, `write.rs` if it has its own
  call site (+ tests).
- Change: canonicalize the deepest existing ancestor, re-append the rest,
  compare components case-insensitively. Also (follow-up from D1, which
  made `write_atomic` write THROUGH symlinks): `write` and `edit` refuse a
  path whose resolved target is outside `ctx.working_dir` when any
  component of the path is a symlink (a repo-shipped `notes -> ~/.bashrc`
  must not be writable through the link).
- Acceptance: tests - `docs -> .git/hooks` symlink then `docs/pre-commit`
  is refused; a symlink pointing outside the workspace is refused; `.GIT/hooks/x` and `agents.md` are refused.

**D5 | H | low - Bounded reads on model-supplied paths**
- Files: `crates/modron-tools/src/read.rs`, `grep.rs`, `minify.rs`,
  `crates/modron-core/src/context.rs` (AGENTS.md / CLAUDE.md / SKILL.md
  loaders).
- Change: refuse non-regular files via metadata (no /dev/zero, FIFOs);
  cap `read` line length (e.g. 64 KiB per line, 10 MiB total), skip files
  over 10 MiB in grep, cap minify input; context loaders read at most the
  cap with `take()` and refuse symlinks that resolve outside the repo.
  `read` on non-UTF-8 input returns "binary file" instead of "0 lines".
- Acceptance: tests - `/dev/zero` returns an error quickly for read, grep,
  minify; an `AGENTS.md -> /etc/hostname` symlink is not loaded.

**D6 | L | medium - Sandbox covers verify/diagnostics/interact/lsp; harden profiles**
- bubblewrap: add `--new-session`, `--die-with-parent`, `--unshare-pid`,
  make `/lib64` bind conditional. Seatbelt: escape the workspace path, put
  the profile outside the writable temp dir. Carried over from D2: the
  sandboxed bash path still uses `.output()` inside
  `modron-core/src/sandbox.rs` (`run_seatbelt` / `run_bubblewrap`), so it
  has no output cap, no truncation marker and no kill-on-interrupt; make
  those return a spawned child that bash.rs drains with `spawn_capture` /
  `settle_readers`. Leader-owned (needs macOS and
  a bwrap host to test).

### Lane E - providers, auth, store

E1 -> E2 -> E3 serial (providers). E4 -> E5 serial (auth). E6 -> E7 serial
(store). The three sub-lanes are independent of each other.

**E1 | H | medium - Streaming must not die at 120 s**
- Problem: `build_client` in `providers/src/http.rs` sets a total
  `.timeout(REQUEST_TIMEOUT)` on the client that also drives SSE.
- Files: `crates/modron-providers/src/http.rs`, `anthropic.rs`,
  `openai_compat.rs`, `openai_responses.rs`.
- Change: remove the client-wide total timeout; apply
  `RequestBuilder::timeout(REQUEST_TIMEOUT)` on non-streaming calls only;
  streaming loops wrap each chunk read in a 90 s idle
  `tokio::time::timeout` that maps to the existing `timeout: ...` error
  prefix; cap the SSE line buffer at 8 MiB (error beyond that).
- Acceptance: tests - a fixture stream that trickles for longer than the
  old limit (tokio paused time) completes; a stalled stream errors with the
  `timeout:` prefix; an endless line errors at the cap.

**E2 | H | medium - Retry 429/5xx on streaming before any output; honor Retry-After**
- Files: `crates/modron-providers/src/retry.rs`, `http.rs` (carry
  `Retry-After` seconds into the error text in a parseable form).
- Change: in the streaming retry arm, replay when nothing was emitted and
  the error is transport death OR HTTP 429/500/502/503/529. Backoff =
  max(policy backoff, Retry-After capped at 60 s) with +/-20% jitter.
- Acceptance: tests - 429 then success on a streamed turn succeeds; 400 is
  not retried; no replay once a delta was emitted.

**E3 | H | medium - Anthropic usage accounting**
- Problem: `input_tokens` excludes cached tokens and
  `cache_creation_input_tokens` is never parsed, so budgets under-count;
  cache % is computed differently per provider.
- Files: `crates/modron-providers/src/anthropic.rs`,
  `crates/modron-types/src/lib.rs` (Usage: document that `input` INCLUDES
  cached and cache-write tokens for every provider; `total()` uses
  saturating add), `crates/modron-view/src/lib.rs` (`cache_hit_pct` =
  cached / input).
- Acceptance: fixture tests for both Anthropic parse sites; view test for
  the percentage; existing streaming fixtures updated.

**E4 | H | medium - Keep the refresh token when a refresh omits it; redact errors**
- Files: `crates/modron-providers/src/anthropic.rs`,
  `crates/modron-core/src/auth.rs`.
- Change: on refresh, if the response has no `refresh_token`, keep the old
  one (as the ChatGPT path does). Remove response/file bodies from the
  `token response missing access_token: {body}` error and the two poll-body
  errors (report field names only). Replace `#[derive(Debug)]` on
  `TokenSet`, `LoginSession`, `ChatGptAuth`, `DetectedCredentials` with
  manual impls that print `<redacted>`.
- Acceptance: tests - refresh without refresh_token preserves the old one;
  `format!("{:?}", token_set)` contains no token text.

**E5 | H | low - Env-file writes: validate and atomic**
- Files: `crates/modron-core/src/auth.rs`.
- Change: reject keys/values containing `\n`, `\r`, `"`, `` ` ``, `$`;
  write via temp file + rename with 0600 from creation. Callback listener:
  accept in a loop until a request with the expected `state` arrives, with
  a 5-minute overall timeout; bind `[::1]` as well as `127.0.0.1`; return
  the "Authorized" page only on success.
- Acceptance: tests - newline in a key is rejected; a stray request before
  the real callback does not abort login.

**E6 | H | medium - Session files are private**
- Files: `crates/modron-store/src/lib.rs`.
- Change: sessions dir 0700; transcript and meta files 0600 from creation
  (`OpenOptionsExt::mode`); on open, tighten existing files.
- Acceptance: unix-only test on the modes.

**E7 | H | medium - Atomic appends across processes**
- Files: `crates/modron-store/src/lib.rs`.
- Change: build `line + "\n"` in one buffer and issue a single `write_all`
  on an `O_APPEND` handle; unique temp name for the meta file
  (`<id>.meta.json.<pid>.<rand>.tmp`); recompute the event count from the
  file when the on-disk length differs from the cached length.
- Acceptance: test - two store instances over one dir, interleaved appends
  from two threads, every line parses and none are lost.

**E8 | L | medium - Cross-process OAuth refresh lock**
- File lock around refresh + re-read the token file before refreshing.
  Leader-owned.

### Lane F - CLI (`crates/modron-cli`)

Serial: F1 -> F2 -> F3 -> F4 -> F5.

**F1 | H | high - Rewind marker lifecycle**
- Problem: `<id>.rewind` is written by `rewind` / `sessions restart`, read
  on every `--resume=<id>`, never cleared; later work is discarded on each
  resume. Bare `--resume` ignores it.
- Files: `crates/modron-cli/src/main.rs` (+ its tests module).
- Change: store a RANGE in the marker instead of a single cut: the marker
  file holds `<cut> <end>` where `end` is the event count at rewind time.
  On resume, the CLI builds the event list as `events[..cut]` followed by
  `events[end..]` (abandoned range skipped, later work kept). An old
  one-number marker is upgraded on first read: `end` = the event count at
  that moment, and the file is rewritten in the two-number form. Bare `--resume` resolves the latest session id
  and honors its marker the same way.
- If the engine only accepts a single `resume_from_event` cut and cannot
  take a skip range without editing `modron-core`, STOP and report BLOCKED;
  the leader will add a `resume_skip_range` field in lane A after A4.
- Acceptance: test - rewind, resume + run, resume again: the second resume
  sees the first resumed run's events and does not see the abandoned ones.

**F2 | H | medium - Eval workspaces are per model and clean**
- Files: `crates/modron-cli/src/evals.rs`.
- Change: workspace dir `eval-<task>-<sanitized model>`; remove it before
  each run.
- Acceptance: test - a file left by model a is absent for model b.

**F3 | H | low - CLI panics and small bugs**
- Files: `crates/modron-cli/src/main.rs`.
- Change: clamp `--older-than-days` / `ttl_days` before
  `Duration::days`; replace `&h[..16]` with a char-safe truncation; guard
  `events[usage_baseline..]` with `.get(..)`; `bench` deletes
  `bench-marker.txt` only if it created it; `--attach` with `--resume` is
  honored or rejected with an error; range-check
  `[compaction].threshold_frac` from config; `mark_on_demand` for MCP tools
  uses the registered `mcp__srv__tool` name.
- Acceptance: one test per item.

**F4 | H | medium - ACP cancel and tool-call ids**
- Files: `crates/modron-cli/src/acp.rs`.
- Change: handle the `session/cancel` notification by aborting the running
  prompt task; completion sends `tool_call_update` with the SAME
  `call-<n>` id used at start; unknown `sessionId` on prompt returns a
  JSON-RPC error; forward queued updates as they arrive (use a tokio mpsc
  as a `select!` arm instead of draining a std mpsc after other arms fire).
- Acceptance: tests driving the ACP loop over in-memory pipes.

**F5 | H | low - Publish client sends the bridge PUBLISH_TOKEN**
- Depends on B6. Files: the publish call site in `modron-cli` /
  `modron-server` (worker locates via `"/pages/"`). Token from
  `MODRON_PUBLISH_TOKEN`.

### Lane G - infra (no Rust; fully parallel with everything)

G1..G5 touch disjoint files and may run concurrently.

**G1 | H | medium - cloud/container/entrypoint.sh**
- Change: use `${GITHUB_TOKEN}`; clone via
  `git -c http.extraHeader="Authorization: Bearer $GITHUB_TOKEN" clone`
  so the token is not stored in `.git/config`, then `unset GITHUB_TOKEN`
  before `exec`; replace `--listen 0.0.0.0:7699` with `127.0.0.1:7699`;
  fix the comment about `--bridge` (it does not open a tunnel).
- Acceptance: `bash -n` and `shellcheck` clean (if available).

**G2 | H | low - cloud/container/Dockerfile builds**
- Change: `COPY mobile/rust-core ./mobile/rust-core`; `rustup target add
  x86_64-unknown-linux-musl` + `musl-tools`; `--locked`; run as a non-root
  user in the final stage. Remove the `SessionBridge` DO binding from
  `cloud/container/wrangler.jsonc` (the worker does not export it).
- Acceptance: `docker build` succeeds if docker is available; otherwise
  report "not built" honestly.

**G3 | H | medium - install.sh verifies checksums**
- Change: download `<archive>.sha256` and verify with `shasum -a 256 -c` /
  `sha256sum -c`; a mismatch is fatal (no source fallback). Source fallback
  only on 404, prints why, checks out the release tag when `MODRON_VERSION`
  is set, and builds with `cargo build --release --locked -p modron-cli`.
  Do not hide curl/tar stderr.
- Acceptance: `bash -n`; manual test notes in the report.

**G4 | H | medium - Workflow hardening**
- Files: `.github/workflows/ci.yml`, `release.yml`.
- Change: `permissions: contents: read` at the top of both; in release.yml
  grant `contents: write` only on the jobs that upload; pin every
  third-party action to a full commit SHA with the tag in a trailing
  comment (worker must look the SHAs up, not invent them - if it cannot
  reach GitHub, report BLOCKED); pass `${{ matrix.* }}` values through
  `env:` instead of interpolating into `run:` scripts.
- Acceptance: leader validates YAML and the SHAs.

**G5 | H | low - Untrack ignored files**
- Change: `git rm --cached .DS_Store crates/modron-cli/demo-output.txt`.

---

## 5. Ledger (leader-only)

Statuses: TODO -> IN_PROGRESS -> REVIEW -> DONE | BLOCKED | ESCALATED.

| ID | Tier | Sev | Status | Worker | Commit | Evidence / notes |
|----|------|-----|--------|--------|--------|------------------|
| A1 | H+ | critical | DONE | | a34ca85 | Haiku. Leader follow-up: gated calls stay concurrent when they would not prompt. 3 new tests. |
| A2 | H+ | high | DONE | | 8af1cb9 | Haiku. 3 new tests. |
| A3 | H+ | high | DONE | | 59d380c | Haiku. Checkpoint recorded in execute_tool after the gates, every mode; fails closed if it cannot be stored. 7 new tests. |
| A4 | H | high | TODO | | | |
| A5 | H | medium | TODO | | | |
| A6 | H | medium | TODO | | | |
| A7 | L | medium | TODO | | | |
| B1 | H+ | critical | DONE | | bebc4b3 | Haiku, one BLOCKED round (scope widened to tests/common/mod.rs). Leader follow-up: --allow-origin flag. 5 new tests. |
| B2 | H+ | high | DONE | | 4805603 | Haiku, one BLOCKED round (leader added ErrorKind::Forbidden to scope). Leader follow-up: Detach allowed on scoped ctx. 8 new tests. |
| B3 | H | high | DONE | | a0e7f27 | Leader. allow_rest cleared in start_run; hub test extended. |
| B4 | H | medium | TODO | | | |
| B5 | H | medium | TODO | | | |
| B6 | H | medium | TODO | | | |
| B7 | H | low | TODO | | | |
| B8 | H | critical | DONE | | d6a5337 | Leader. |
| C1 | H+ | high | DONE | | 4d0ea46 | Haiku. Desktop and App.tsx edits NOT compiled (needs-macos, no node_modules). 7 new tests on Linux crates. |
| C2 | H | high | DONE | | d1c1f43 | Haiku. Remote end-to-end driver untested; 10 new tests. Known: remote `a` can race to AllowOnce (re-prompt, not a bypass). |
| C3 | H | medium | TODO | | | |
| C4 | H | medium | TODO | | | |
| C5 | H+ | medium | TODO | | | after B3; needs-macos |
| C6 | H | high | TODO | | | needs-macos; cargo-deny |
| D1 | H+ | medium | DONE | | 4e4b6cc | Haiku. 4 new tests. Follow-up added to D4 (write-through symlink outside workspace). |
| D2 | H | medium | DONE | | 1a279c2 | Haiku, PARTIAL: sandboxed path deferred to D6 (needs modron-core sandbox.rs). 4 new tests. |
| D3 | H+ | medium | TODO | | | |
| D4 | H | medium | TODO | | | |
| D5 | H | low | TODO | | | |
| D6 | L | medium | TODO | | | |
| E1 | H | medium | DONE | | 8bb9ea5 | Haiku. Idle tests use real short timers, not paused time. 4 new tests. |
| E2 | H | medium | DONE | | 357dfab | Haiku. Streams retry 429/500/502/503/529 only; turn() retries all 5xx. 10 new tests. |
| E3 | H | medium | TODO | | | |
| E4 | H | medium | TODO | | | |
| E5 | H | low | TODO | | | |
| E6 | H | medium | TODO | | | |
| E7 | H | medium | TODO | | | |
| E8 | L | medium | TODO | | | |
| F1 | H | high | TODO | | | may escalate |
| F2 | H | medium | TODO | | | |
| F3 | H | low | TODO | | | |
| F4 | H | medium | TODO | | | |
| F5 | H | low | TODO | | | after B6 |
| G1 | H | medium | DONE | | db6b845 | Leader. bash -n ok; not run in a container. |
| G2 | H | low | DONE | | db6b845 | Leader. NOT built (no docker daemon). |
| G3 | H | medium | DONE | | 2e475c9 | Leader. Good and bad checksum paths tested against a local fake release. |
| G4 | H | medium | DONE | | 4852bfa | Leader. SHAs from git ls-remote; YAML parses; workflows not run. |
| G5 | H | low | DONE | | 728a8e7 | Leader. |

Wave 1 gate evidence (leader, 2026-11-14): fmt clean, clippy clean, 658
tests pass on `cargo test --workspace --exclude modron-desktop` after a
full rebuild.

Wave 2 gate evidence (leader): fmt clean, clippy clean, 700 tests pass.

Lesson from wave 1: workers must NOT share one CARGO_TARGET_DIR across
worktrees - it produced a stale modron-core artifact and a false test
failure in the leader's workspace run. Each worker uses its own target
dir; the leader touches all sources before the merge gates.

## 6. Suggested wave order

Lanes are independent, so each wave is "the next packet of every lane".

- Wave 1: A1, B1, C1, D1, E1 (+ G1-G5 as filler; they never conflict)
- Wave 2: A2, B2, C2, D2, E2 (+ E4, E6 since auth and store sub-lanes are
  free), B8 once B1 is merged
- Wave 3: A3, B3, C3, D3, E3 (+ E5, E7), F1
- Wave 4: A4, B4, C4, D4, F2
- Wave 5: A5, B5, C5, D5, F3
- Wave 6: A6, B6, C6, F4, then B7, F5
- Leader packets A7, D6, E8 run alongside from wave 2 on.

Cross-lane file overlaps the leader must sequence by hand:
- B1 touches `crates/modron-cli/src/main.rs` (flag only): merge B1 before
  starting F1.
- A3 and E3 both touch `crates/modron-types/src/lib.rs`: merge A3 first.
- C5 touches `hub.rs`: start only after B3 is merged.
- D3 touches `modron-core/src/hooks.rs`; D5 touches
  `modron-core/src/context.rs`: neither is in lane A's file set, no
  conflict, but rebase before merge.

## 7. Not in this plan (backlog)

Accepted risk (owner decision): `remember` is ungated, so an injected
instruction can persist a global memory into future system prompts.
Revisit if memory poisoning is observed.

Low severity: speculative-read staleness within a mixed turn; auth-failure substring
match on "401"/"403"; memory store locking; process-wide ReadGate; LSP
protocol details (didChange version, URI encoding, initialize wait);
delegate error labels; `@` completion byte/char offset; minify line-count
underflow; request id 0 reuse; stale state on session delete; attach
watermark race; desktop single approval slot; desktop gen-UI placeholder
submit; mobile handshake generation check and keepalive; Z.ai login
without PKCE; cross-host redirect forwarding `x-api-key`. Promote any of
these into a packet when a lane runs dry.
