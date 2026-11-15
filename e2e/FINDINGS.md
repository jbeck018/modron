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
| W6 /compact | PASS | F13 (closed: test read the wrong connection) |
| T1 terminal app under a pty | PASS | F14 (closed: test read raw bytes, not the screen) |

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

### F13. /compact on the owner page (W6). CLOSED: the test read the wrong connection
- Reported: `/compact` returned Ok but no frame reached the socket, and pings
  stopped (FINDINGS of the earlier packet).
- Not reproduced. Native (mobile/rust-core/tests/connection_sends.rs): run
  frames (task, delta, tool, approval requested and resolved, turn done) before
  the first command; a snapshot over 260 KB; an `error` frame; a server close
  and reconnect. In each, `compact`, `send_message` and keepalive pings reach
  the server. A send on a dead connection returns `ClientError::Closed`.
- Browser trace (temporary console.error lines in client.rs `send_frame` and
  the run loop, removed again): `send_frame live=true ok=true`, then the loop
  writes the frame (`sent`), then the server reply with `"id":3` arrives. The
  page shows "compacted: 4019 -> 4791 tokens". The keepalive ping fired at 30 s.
  The writer did not die.
- Root cause (test, e2e/run.mjs W6): the step waited for the `note` on the
  LOCAL client's stream (`lx.waitFor`). The compact reply is correlated to the
  requesting connection (crates/modron-server/src/dispatch.rs `compact`: "The
  reply is the correlated Note"), so only the browser receives it.
- Root cause of the "never reached the socket" reading: the evidence was
  truncated. run.mjs kept the first 40 socket frames and the last 15 console
  errors, so the compact frame was out of the window.
- Changes: W6 now waits for the note in the browser page and checks that the
  browser socket sent a `compact` frame. The evidence keeps every outbound
  frame plus the first 20 and last 60 inbound, and the last 40 console errors.
- Hardening (mobile/rust-core/src/client.rs `send_frame`): a refused send now
  records "not sent: connection closed" in `last_error`, not only in the return
  value. Tests: connection_sends.rs `a_send_on_a_dead_connection_is_an_error_and_says_why`
  and `a_send_after_reconnect_reaches_the_new_socket`.
- Observation (not a client defect): the browser socket saw two welcome and
  two snapshot frames after the hello. The core ignores the second welcome.

### F14. Terminal app stays on "connecting" under a pty (T1). CLOSED: the test read raw bytes, not the screen
- Reported: 1 in 3 runs reached "connected"; the stall retry did not help.
- Not an app defect. Driver trace (MODRON_APP_TRACE, app/src/session.rs
  `trace`): every run goes Connecting -> Connected about 20 ms after start, and
  the snapshot reaches the host (`pump snapshot label="connected ..."`).
- Root cause (e2e/tui_pty.py): "connected" was searched in the raw byte stream.
  ratatui writes only the cells that changed, so the header change from
  "connecting" to "connected" is never a contiguous string in that stream. Raw
  check: 0 of 10 runs. Rendered-screen check (pyte): 10 of 10 runs connected at
  about 0.05 s. The "1 in 3" figure was the raw-stream artefact.
- Fix: tui_pty.py judges every check on the rendered screen (pyte, a pure-Python
  terminal emulator kept outside the repo: /tmp/claude-0/e2e/pyte, or
  E2E_PYTE_DIR). A missing pyte fails the step with a clear message.
- The stall retry (app/src/session.rs CONNECT_STALL) is kept. It never fired
  in these runs.
- Hook kept: `MODRON_APP_TRACE=<path>` writes the plan, the driver start, each
  connection-state change, each snapshot pumped, and reconnects.
- Not done: a headless app test for the screen state. The regression guard is
  the T1 screen check.

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

Packet CONNECT (after W6 and T1 closed), two full runs (`node e2e/run.mjs`):
run A 22/22 passed, run B 22/22 passed; left-over tracked processes 0 in both.
All 22 steps (S1-S7d, P1-P4, W1-W6, T1) PASS in both runs; no SKIP.
web-test.js: 26 PASS, 0 FAIL in each of two runs.
Core: `cargo test -p modron-mobile-core` 68 + 5 (connection_sends) + 37 passed;
clippy -D warnings clean; wasm32 check clean. App: `cargo test` 190 passed,
0 failed; clippy -D warnings clean; wasm32 lib check clean.
T1 by pty (e2e/tui_pty-equivalent driver, rendered screen): 10 of 10 runs
connected in about 0.05 s (raw-byte check before the fix: 0 of 10).
