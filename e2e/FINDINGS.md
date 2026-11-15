# End-to-end findings

`node e2e/run.mjs` drives a real `modron serve` (mock provider) through
the real bridge Worker (local wrangler dev) to the real web app in
headless Chromium.

## 2026-11-15, first run: 2 of 11 passed

| ID | Finding | Status |
|---|---|---|
| F1 | The remote-control tunnel never subscribed to its session. It wrote its own `hello` and `attach` into the bridge sink (so viewers received them as agent frames) instead of dispatching them, so no snapshot, event or approval ever reached a viewer that did not attach by itself. | Fixed in crates/modron-server/src/rc.rs: both frames go through `handle_frame`. |
| F2 | The share URL kept the `ws://` / `wss://` scheme of the bridge base, which a browser cannot open. | Fixed: the viewer page URL is always http(s). |
| F4 | A late viewer got no history when the snapshot exceeded the relay's 96 KB buffer limit (181 KB observed). | Fixed on the server: every relayed `hello` is answered with a fresh snapshot of the tunnel's session, so history no longer depends on the relay buffer. |
| F3 | The web app logs "closure invoked recursively or after being dropped" on load. | Not reproduced after E2E-fix; see below. |

## After the fixes: 9 of 11 pass

Open:

- S4 (stop from the browser): the stop control was not clickable within
  15 s. Not yet diagnosed: the control may not be shown for a run that
  is waiting on an approval, or the mock run may end too quickly.
- S7d (approver invite approves): the approval control was not
  clickable within 15 s for an approver-role viewer. Not yet diagnosed.

Evidence for each run is written to the run's temp directory
(evidence-<id>.json and screenshots); see README.md.

## E2E-fix, 2026-11-15: 11 of 11 pass

Every viewer's console errors and page errors now fail the step that
observed them (run.mjs `checkConsole`), and web-test.js cases (a) and (b)
assert no page or console errors.

### S4 (stop not clickable): product defect, layout

Root cause: the chat body put a fixed 600 px virtual list above the
approval strip, queue/stop row and composer (app/src/chat.rs, the
`.h(px(600.0))` on the transcript). The page is `html, body { overflow:
hidden }` (packaged index.html), so nothing scrolls. At the Playwright
default viewport of 1280x720 the approval options sat at y=719 and the
queue/stop row at y=736, outside the viewport. Playwright refused the
click ("element is outside of the viewport"). The run was in fact
active: the Stop control was rendered and the snapshot said running
while the approval was pending.

Fix: the transcript height follows the window (app/src/web.rs sends
`innerHeight` on change over a channel; app/src/lib.rs drains it into a
signal; app/src/chat.rs `fit_transcript_height`: window minus 330 px,
clamped to 240..600). At 720 px: allow once y=509, stop y=543, click
lands on the stop span. Unit test: chat.rs
`transcript_leaves_the_controls_on_a_short_window`.

Note for later: a signal may not be set from the page timer (outside the
engine's reactive runtime): nsh panics with "no reactive runtime". The
channel avoids that.

### S7d (approver approve): same root cause

The approver invite page has the same bottom rows, so "allow once" was
clipped by the same fixed height. S7d passes after the layout fix. I did
not capture the pre-fix approver page, so this attribution rests on the
identical symptom and the S4 measurement, not on a before/after capture.
The bridge's PERMITTED_FRAMES were not the cause: the approver's
`approve` frame was forwarded and resolved the approval (the scenario
checks approval_resolved on the local side).

### F3 (wasm closure message): not reproduced

No "closure invoked recursively" message appears with the current build
in any run (11 e2e scenarios, web-test.js a to k, console errors captured
for every viewer). The message is wasm-bindgen's report for a Closure
called after a panic left its borrow flag set, so it is a symptom of an
earlier panic inside a web closure (web.rs tick or keydown listener).
The first panic is not identified. The two closures keep `forget()`
because they live for the page's life (one per page, not per render).
Action if it returns: capture the console message location
(`m.location()`) and the first pageerror before it.

### Left for the lead

None in crates/ or mobile/ were needed for these three items. The app
build consumes crates/modron-view, which was briefly mid-edit (a
non-exhaustive match on ToolActivity); the bundle was built once it
compiled and includes the in-progress crate changes present at that time.

## E2E-2, 2026-11-15: portal, invites, steer/queue, plan, settings, shell, sessions, compact, terminal

Run: `node e2e/run.mjs` (all steps). Evidence: `/tmp/claude-0/e2e/run-*/evidence-<id>.json`
(page text, role=button list, console errors, socket frames, serve and bridge logs).
Result table: see the summary at the end of this section.

### Results

| ID | Result | Where it stands |
|---|---|---|
| S1-S5, S6a, S7a-S7d | PASS | unchanged |
| S6b | FAIL | regressed since the 11-of-11 run: page shows 6660 chars and no `BIGMARK-0`. Not diagnosed (see F4). |
| P1 portal sign-in through the UI | PASS | |
| P2 owner drives through the portal | PASS | |
| P3 invite from the UI, revoke | PASS | the 410 console message is allowed only when the main document is 410 |
| P4 offline marking | PASS | test clock through `route` (x-test-now) |
| W1 steer and queue | FAIL | F2 (no `steered`), F5 (wasm panic). |
| W2 plan phase | FAIL | F3: `/plan` refused by the serve, so no plan run |
| W3 settings round trip | FAIL | F3: `set_config` refused |
| W4 shell passthrough | FAIL | F1 and F3: `shell` refused by the serve; F5 panic |
| W5 sessions screen | FAIL | F7: no frames sent from the page; list stays "no sessions yet" |
| W6 compact | FAIL | F7: `/compact` never sent |
| T1 terminal app under a pty | FAIL | F8: stays on "connecting"; first-run screen does not close |

### F1. Owner is treated as a share link by the serve (config, shell)

- Scenario: W2, W3, W4 (owner browser through the portal bridge).
- Expected: the portal owner has "everything" (docs/PORTAL.md, Viewer roles table: `owner (cookie)`
  may send everything). PROTOCOL_V2.md section 1: `owner` role, `Actor` on each frame.
- Observed: `set_config` (phase, effort, approval_mode) and `shell` get
  `{"type":"error","message":"this request is not allowed on a share link","kind":"forbidden"}`.
  The same error is returned for the owner socket and for a share-link socket.
- Smallest reproduction: `OWNER_TOKEN` and `HOST_TOKEN` on a bridge with `MODRON_TEST_HOOKS=1`; serve
  with `MODRON_HOST_TOKEN` and `remote_control`; sign in (`POST /auth`); open
  `/app/?url=ws://<bridge>/ws/<viewer key>` with the cookie; type `/effort low`. Error comes back.
- Diagnosis: the serve scopes every relayed connection to its session. `crates/modron-server/src/dispatch.rs:213-232`
  allows only Submit / Steer / Interrupt / approve-type frames for a scoped connection and answers
  `_ => Err("this request is not allowed on a share link")` at `dispatch.rs:232`. The bridge
  never tells the serve which role a viewer socket has: `cloud/bridge/src/index.ts:438-453` tags the
  socket (`role:viewer`), and the forwarded frame carries no actor (no `actor` field under
  `cloud/bridge/src`). So an owner socket and a share socket look the same to the serve.
- Also: the owner cannot run `!` shell passthrough from the portal (W4), and docs/remote-control.md:169
  says a share link cannot change configuration. Decide whether the owner path should carry the
  owner role to the serve (PROTOCOL_V2 section 1), then fix dispatch.

### F2. Steer is not sent while a run waits on an approval (web)

- Scenario: W1.
- Expected: text sent while a run is active is a steer (PROTOCOL_V2.md section 2: "submit while
  running = steer"); the server answers `steered`.
- Observed: the browser sent `{"type":"submit", ...,"task":"e2e steer text"}`; the server answered
  `{"type":"error","message":"session busy (a run is active)"}`. The header read `idle` while the
  approval strip was on screen.
- Smallest reproduction: live page on a session with a run that asks for approval (mock, first run);
  type a second message and press Enter before answering.
- Diagnosis (candidate, not confirmed): `mobile/rust-core/src/client.rs:750-762` chooses steer only
  when the client's `running` flag is set (`st.vm.running`). The flag was not set while the approval
  was pending. Check where `running` is set in the client's event handling.

### F3. Config and shell from the owner path are refused (consequence of F1)

W2 (`/plan`), W3 (effort, approval mode) and W4 (`!echo`) all end at F1. The plan refusal
(`plan phase: read-only`) and the shell approval were never reached, so their behaviour is not
verified. Re-run after F1 is fixed.

### F4. S6b (late viewer, history over 96 KB) fails in this run

- Expected (previous run): earlier rows visible to a late viewer (F4 fix).
- Observed: page shows 6660 characters and no `BIGMARK-0`. Not diagnosed. Check whether the
  transcript shows only the visible window of the virtual list (app/src/chat.rs) before concluding
  that the replay is missing rows.

### F5. A wasm panic kills the web page (`Instant::now` on wasm32)

- Scenario: W1, W3, W4, W5 (the owner page). The first console error is
  `panicked at .../std/src/sys/time/unsupported.rs:13: time not implemented on this platform`, then
  `RefCell already borrowed` (app/src/web.rs:354) and `closure invoked recursively or after being
  dropped`. After that the page sends no more frames.
- Diagnosis: `app/src/session.rs:26` imports `std::time::Instant` in the target-neutral driver and
  calls it on the web path: `session.rs:386` (reconnect due time), `session.rs:394` (`Stage::Listing`
  deadline, evaluated while the session list is empty) and `session.rs:458`. On
  `wasm32-unknown-unknown`, `Instant::now()` panics. The web host has its own clock (`app/src/web.rs:73`
  `now()`), so the driver needs that clock. This is the first panic behind the "closure invoked
  recursively" message that E2E-fix left open (F3 in the first section).
- Smallest reproduction: open a live page without a `session=` parameter (the owner page after the
  portal list, or W1's owner page); the panic appears within a few steps. Not pinned to one call site
  (the panic report has no caller frame).

### F6. The page stops sending after its first wasm panic

Consequence of F5: after the panic, typed text and clicks produce no frames (W1 Steered, W4 shell).

### F7. Owner page sends nothing: sessions list and compact (W5, W6)

- Expected: Ctrl+O on a live page asks for the session list; "new session" and `/compact` send frames.
- Observed: the page's socket sent only `hello`: no `list_sessions`, no `new_session`, no `compact`,
  and no keepalive `ping`. The list shows "no sessions yet" although the session exists.
- Diagnosis (partial): `app/src/session.rs:456-461` requests the list only when `resume != New`, and
  `app/src/session.rs:58` makes `New` the default. A live page with no `session=` parameter therefore
  never asks the server for sessions. The missing frames for the new-session click and `/compact`
  are not explained; the driver may have stopped ticking (see F5).

### F8. Terminal app stays on "connecting" under a pty (T1)

- Observed: the header reads `connecting | ws://127.0.0.1:<port> | started by modron-app`; the app's
  own serve logs `listening on ws://127.0.0.1:<port>` (`app-serve.log`). It reached `connected` in
  one of three runs. On a fresh HOME the first-run provider list opens (`app/src/nav.rs:499-509`,
  `Screen::Auth`), and Esc did not close it in five presses.
- Not diagnosed. Check the connect path in `app/src/session.rs` (connect and retry) and whether Esc
  is routed on `Screen::Auth` (`app/src/chat.rs` key router).

### F9. Doc drift: effort levels

`docs/PROTOCOL_V2.md` section 7 lists `ConfigPatch.effort` as `low | medium | high`. The code has six
levels (`mobile/rust-core/src/view.rs:362-370`: low, medium, high, xhigh, max). Update the doc.

### Harness notes

- A console error is attributed to the step that is running when it is checked. F5 errors that
  appear after a step ends are reported in the next step (W3 reports the W1 panic).
- The mock's `rich_demo_script` is consumed once per process, so every step that needs a tool
  approval or a refusal uses its own serve (see README).

### Run status at hand-back

The full run (`/tmp/claude-0/e2e/full-run.log`) was still in W3 to W6 and T1 when this section was
written; it ends with an `EXIT` line and kills its own processes. The W and T1 rows above come from the
isolated runs (`--only W`, `--only T1`); the full run's W3 to T1 lines should be read from its log.
S6b failed in the full run and is not diagnosed (F4).
