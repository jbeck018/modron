# End-to-end findings

`node e2e/run.mjs` drives a real `modron serve` (mock provider) through the
real bridge Worker (local wrangler dev) to the real web app in headless
Chromium, plus the terminal app under a pty (T1). How to run: README.md.
Status is as of 2026-11-15 (packet E2E-DIAG). Finding ids are unique; a
scenario points at the finding that explains its status.

## Status table

Full-suite results are at the end of this file (two runs).

| Scenario | Status | Finding |
|---|---|---|
| S1 legacy relay: share URL, history | PASS | F1, F2 (fixed earlier) |
| S2 browser drives the session | PASS | |
| S3 approve from the browser | PASS | |
| S4 stop from the browser | PASS | F9 (layout, fixed earlier) |
| S5 two viewers | PASS | |
| S6a late viewer, small history | PASS | |
| S6b late viewer, history over 96 KB | PASS | F4 (fixed in this packet) |
| S7a-S7d portal host, invites, approver | PASS | |
| P1-P4 portal sign-in, owner, invites, offline | PASS | |
| W1 steer and queue | PASS | F10 (fixed in this packet) |
| W2 plan phase | PASS | F10 |
| W3 settings round trip | PASS | F1 (fixed earlier) |
| W4 shell passthrough | PASS | F10 |
| W5 sessions screen over the tunnel | PASS (reframed) | F12 |
| W6 /compact | FAIL | F13 (open, not in app) |
| T1 terminal app under a pty | FAIL | F14 (open) |

## Findings

### F1. Owner role did not reach the serve (config and shell refused). FIXED (earlier commit 1205b20)
Owner cookie now carries the owner role through the tunnel. W3 and W2/W4
config and shell frames are accepted.

### F2. Steer refused while an approval waits (web). FIXED (earlier)
The client's running flag and steer-while-approving landed earlier; W1 steers
and queues.

### F3. wasm closure "invoked recursively" message. CLOSED
A symptom of an earlier panic (F5). Not seen in any run since F5 was fixed.

### F4. Late viewer cannot reach the older rows of a long transcript (web). FIXED in this packet
- Expected: a web user can scroll the transcript to its oldest row.
- Observed: the transcript is a virtual list; only visible rows are in the
  DOM (about 6.7k characters). The page had no wheel or touch handling, and
  `html, body { overflow: hidden }` stops native scrolling. Only PageUp and
  Home (with an empty composer) moved it.
- Smallest reproduction: a session with 25 turns of about 6 KB each; open the
  share link in a new browser; look for `BIGMARK-0` (no wheel input).
- Root cause (app): app/src/web.rs had no wheel or touch listener.
- Fix: app/src/web.rs `listen_scroll` (wheel and touchmove on the document)
  calls `Host::wheel` (app/src/chat.rs), which applies `ScrollStep::Lines`
  through `rows_for_pixels`. Unit test: chat.rs `wheel_tests`. Web test: web-test.js
  `(w)`. The S6b step now scrolls with the wheel the way a user does (test
  change: the old step only waited, and asserted the oldest row was in view).

### F5. wasm panic: `Instant::now` on wasm32 (session.rs). FIXED (earlier)
The driver uses the web host's clock. No panic in any run since.

### F6. Page stops sending after the first wasm panic. FIXED (consequence of F5)

### F7. Owner page sent no session list on connect. FIXED (earlier)
session.rs requests `list_sessions` on connect; the frame is sent in every
run (refused by design, see F12).

### F8. Terminal app stays on "connecting". SEE F14

### F9. Controls clipped below the fold (S4, S7d). FIXED (earlier)
Transcript height follows the window (chat.rs `fit_transcript_height`).
New web-test case `(l2)` checks every `[role=button]`, the composer, the
approval options, and the queue chips at 1280x720 and 390x844 with an
approval, a run, and two queued messages. It passes.

### F10. Blank session on the mock provider covered by the first-run credentials screen. FIXED in this packet
- Expected: a new session on `--provider mock` is usable at once.
- Observed: the credentials list (Screen::Auth) replaced the chat, so the
  composer was not in the DOM. W1, W2, W4, W5, W6 all start on a blank session
  and failed on `getByPlaceholder('Message')` or on a click.
- Root cause (app): app/src/nav.rs `Nav::first_run` treated "no provider row
  is signed in" as "nothing signed in". The mock has no row, so it needs no
  sign-in.
- Fix: nav.rs `first_run` (a) stays quiet when the session's own provider has
  no credentials row, and (b) waits until the snapshot names the session's
  provider (T1's TUI judged an unknown provider as none). Unit tests in
  nav.rs: `first_run_stays_quiet_when_the_session_runs_on_a_provider_without_rows`,
  `first_run_waits_for_the_sessions_provider`.

### F11. Build break: shell transcript rows had no style arm (app/src/chat.rs). FIXED in this packet
- The shared view (crates/modron-view, in progress) added
  `TranscriptEntry::Shell`; `style_of` in app/src/chat.rs did not match it, so
  the web bundle did not build.
- Fix: `TranscriptEntry::Shell` is styled as a tool row (chat.rs `style_of`).

### F12. The tunnel cannot list, create or delete sessions (design). Test reframed; sessions note added
- Expected by W5 (old): a tunnel-pinned owner switches between two sessions
  from the sessions screen.
- Observed: `list_sessions` (and `new_session`) get
  `{"type":"error","kind":"forbidden","message":"this request is not available through a remote-control tunnel"}`.
  This is deliberate: crates/modron-server/src/dispatch.rs `relay_tier`
  ("Refused for every role: list / delete / create sessions", the
  `SERVER_WIDE` constant at dispatch.rs:265). A tunnel page is pinned to one session.
- Root cause: design (not a defect). The test expected a capability the tunnel
  does not offer.
- Changes: W5 now checks that Ctrl+O opens the sessions screen and shows the
  refusal. The screen used to say only "no sessions yet": app/src/nav.rs
  `NavView::sessions_note` carries the snapshot's last error into the empty
  list, and screens.rs shows it. Multi-session switching stays covered by the
  local terminal app and the portal list (P1, P2).

### F13. /compact typed on the owner page never reaches the socket (W6). OPEN, outside the app
- Expected: `/compact` sends a `compact` frame; the serve answers with a
  compaction note.
- Observed: the composer clears (the line was submitted). The app reaches
  `UiAction::Compact` and `client.compact()` returns `Ok(())` (checked with
  a temporary note in session.rs, since removed). The browser socket writes
  only `hello`, `auth_providers_req`, `list_sessions` for the whole step. The
  serve receives no compact frame. Keepalive pings also stop on that socket.
  Inbound events keep arriving. The socket works for W2, W3 and W4 (submit,
  set_config, shell are sent there).
- Correlation (unconfirmed): in W5 and W6 the local client starts a run
  before the owner page sends anything; in W1 to W4 the owner's own submit
  comes first.
- Smallest reproduction: owner page through the bridge; a run started by a
  second client; then `/compact` in the page.
- Likely location (not edited, per packet rules): mobile/rust-core/src/client.rs
  `send_frame` (around line 1231) and the connection loop (around lines
  1330-1400). The lead should instrument the outbound channel.

### F14. Terminal app stays on "connecting" under a pty (T1). OPEN
- Observed: the TUI (`app/target/debug/modron-app-tui --url ws://127.0.0.1:<port>`,
  fresh HOME) draws one frame, then no output. The header reads
  `connecting - ws://127.0.0.1:<port>`. Its threads are idle in epoll (not
  spinning). The serve it started logs "listening"; a plain WebSocket client
  gets `welcome` from the same serve and a handshake works.
- Not resolved: no connection to the serve was observed (`ss` shows nothing;
  tooling limit). The connect path is app/src/session.rs (`connect_stalled`
  at line 56, retry at lines 60-70, connect at about line 505) and
  mobile/rust-core/src/transport_native.rs. Next step: strace the TUI, or
  log `RemoteClient::connect_with_options` from the driver.
- Also seen: the first-run list was covering the TUI (F10); fixed.

### F15. Doc drift: effort levels. OPEN (docs/, outside this packet)
`docs/PROTOCOL_V2.md` section 7 lists `effort` as low, medium, high. The code
has six levels (mobile/rust-core/src/view.rs: low, medium, high, xhigh, max).

### H1. Harness left processes behind. FIXED in this packet
- Each child is in its own process group (lib.mjs `spawnLogged`).
- Playwright's Chromium is not tracked, so it survived a signal. Now every
  Chromium carries `--modron-e2e-run-<pid>` (lib.mjs `RUN_TAG`) and is killed
  by that tag (`killTagged`).
- `killEverything` runs from the exit handler, from SIGINT, SIGTERM and SIGHUP,
  from uncaught errors and rejections, and in a `finally` around the wrap-up
  (run.mjs). The summary counts tagged leftovers too.
- Verification: see the packet report (kill midway, then `ps`).

## Full-suite results

Run 1 (`node e2e/run.mjs`, after the fixes above): 20 passed, 1 failed (W6),
1 skipped (T1: binary older than the source; the T1 skip is correct behaviour,
the binary was rebuilt afterwards and T1 then failed, F14).

Run 2 (same code, after the last test fix): 20 passed, 2 failed: W6 (F13) and T1
(F14). S6b, W1 to W5, S7a to S7d, P1 to P4 and S1 to S5 pass in both runs.

Per scenario, run 1 / run 2: S1 PASS/PASS, S2 PASS/PASS, S3 PASS/PASS, S4 PASS/PASS,
S5 PASS/PASS, S6a PASS/PASS, S6b PASS/PASS, S7a-S7d PASS/PASS, P1-P4 PASS/PASS,
W1-W5 PASS/PASS, W6 FAIL/FAIL, T1 SKIP/FAIL (run 1 skipped only because cargo fmt
touched the source after the binary was built; rebuilt, then FAIL).

Other checks: `cargo test` 190 passed, 0 failed; `cargo clippy --all-targets -D warnings`
clean; `cargo check --lib --target wasm32-unknown-unknown` clean; web-test.js 26 PASS
in each of two runs (the 24 earlier cases plus (w) and (l2)).
