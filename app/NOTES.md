# Web target notes (Packet C4a)

## Result

- Build: `nsh package --target web -o dist` succeeds and writes `dist/Modron/`
  (the app name, not `dist/`). 98 files, 12,529,967 bytes in total.
  - `nsh.c30214878b.wasm` 5,556,766 bytes; `.gz` 2,054,116; `.br` 1,512,669.
  - `nsh-syntaxes.ea0da56d04.bin` 921,717 bytes (loaded on first code block).
- `cargo check --lib --target wasm32-unknown-unknown`: clean.
- `web-test.js` (a) demo: passes. (b) live: passes; see flakiness below.

## Driver on the web

`web::start()` runs on the page. It reads `?url=`, `token=`, `session=` from
`window.location.search()` (form-encoded, as `URLSearchParams` reads it). It
calls `session::Driver::start`, which calls `RemoteClient::connect`. That is
fire-and-forget: the core spawns the connection with `spawn_local`, so the
driver never blocks. A `setInterval` of 20 ms (`session::TICK`) calls
`Live::pump` and `Driver::step`. Demo mode pushes `demo::Player` frames from
`Date.now()`. Both feed the same `chat::link` snapshot channel that the
terminal host uses.

The screen is untouched: `App` takes `CLIENT`, or on wasm starts `web::start()`.

## Gaps

- No key hook on the web host. Esc (interrupt), y/a/n (approval), y/r (plan),
  and Alt+Enter (queue) are not wired; `Host::take_key` is ready for a web key
  hook. AGENTS.md wants a stop button and approval buttons instead; that is not
  built. The composer, Enter-to-send and streaming work.
- Reconnect after a drop is out of scope, as on the terminal.
- Only the wasm build's headless Chromium run was measured. No visual check of the
  oculus theme: `nsh.json` `"theme": "theme/oculus.json"` is accepted by
  the packager (no `nsh.json: theme` error), and `grep` did not find the
  font name in the text output. I did not check whether it applies.

## Flakiness (live test)

About 3 of ~17 live runs ended with "no submit frame". Two of those were under
a concurrent test loop. One was sequential, before a 300 ms settle before Enter
was added. The last 5 sequential runs all passed. Root cause not identified.
My guess is a race in NSH's composer: the DOM value shows `hi` before the
engine applies the change, so the submit reads an empty draft and
`action_is_empty` drops it. Not verified. Diagnostics: `WEB_TEST_DEBUG=1`.

## Dependency note

`nsh-core` pulls `nsh-base`, which pulls `nsh-platform-tui` with default
features. Our direct dependency must use the same features (`features =
["run-app"]`, defaults kept). With `default-features = false` cargo built two
copies of `nsh-platform-tui`, and the test code's `Input` no longer matched
the crate's `Input` (E0308 in tests/chat.rs and tests/live.rs).

## Toolchain

- `nsh package` needed the wasm-bindgen CLI, because the app calls JS.
  Installed with `cargo install wasm-bindgen-cli --version 0.2.130 --locked
  --root /tmp/claude-0/wbg` and pointed `NSH_WASM_BINDGEN` at it.
- Cold build times: the crate 1m 11s, the web runtime 4m 55s.

# Packet C3d: web keys, activity rows, theme

## Keys

One rule, `chat::route_key` (pure, unit-tested), shared by both hosts. The
terminal calls it from `Host::filter`; the web calls it from a capture-phase
`keydown` listener on `document` (`web::listen_keys`). A consumed key is
`preventDefault`ed so it does not reach the composer. Letters reach the
composer whenever no approval or plan is pending.

The web also shows tappable controls (`WEB_CONTROLS` in `chat.rs`, web only):
Stop while `running`, the approval and plan options (labels from the core),
and Queue beside the composer. Each is `role("button")` with `on_press`.

New user-visible string on web: "stop" (Stop control) and "queue" (Queue
control). Neither exists in the core or modron-view; both need a shared home.

## Activity rows

Rendered verbatim from the snapshot, in core order: the live turn summary
(`turn_summary_line`) and the latest tool rows (`tool_lines`, last 6), then
sub-agent rows (`subagent_rows`), then `last_note` and `last_error`. Tool
rows are coloured by their status mark (success / destructive / tool).

## Theme

`theme/oculus.json` reaches both targets:

- Web: the packager applies `nsh.json` `"theme"`. `web-test.js` (f) reads the
  task row's computed colour and gets the oculus primary, rgb(217, 165, 32),
  with no install from the app. The app does not install tokens on web.
- Terminal: `nsh.json` is not read, so `App` calls `lib::install_theme()`
  (native only) on the engine thread. Tokens are then drawn through NSH's
  terminal palette (`palette::ink`), so no ANSI is hard-coded. The headless
  test `prompt_marker_takes_a_theme_colour` checks the marker is not Reset.

Semantic names used in code: foreground, muted-foreground, primary, tool,
success, warning, destructive, border (where used).

# Packet C3e: loose ends (unified app, client core, shared view)

- Share links: the core adopts the first session a frame names when none is
  attached (`accept_session`, mobile/rust-core/src/client.rs). A `Snapshot`
  names it; `Event`/`Delta` frames carry it in the envelope. Frames for a
  second session are ignored after an adoption (debug log). `attach`,
  `new_session`, `fork_session`, `create_task` and `TaskCreated` clear the
  adoption, so their answers switch sessions. The driver attaches only when
  the wanted session is not already attached. Web-test (g) and the headless
  share-link tests cover it.
- Refused calls: a core call that fails locally, and a server `Error` frame,
  both land in the snapshot's `last_error` (`RemoteClient::note_local_error`).
- Backspace on an empty composer removes the last queued message
  (`route_key`, `Dequeue`). On the web, each queue chip is a button that
  sends `Dequeue` for its position. The chip removes itself when the server's
  `QueueChanged` arrives.
- Shared copy in modron-view: `STOP_LABEL`, `QUEUE_LABEL`,
  `COMPOSER_PLACEHOLDER`; `compact_tokens` (moved from the client view).

Literal copy still in app/src, not from the core or modron-view:
- chat.rs: queue chip format `[n] text`, steer echo `> text`, the key hints
  `y a n` / `y r` and their `   ` joiner (terminal only).
- demo.rs: demo fixture rows and `running` / `idle` (the core has the same
  words inline in view.rs, with no shared constant).

# Parity packet D1 (terminal daily-use gaps)

- Tool detail panel (Ctrl+T) and the composer cap: the panel is a fixed
  window of 12 rows plus a title, the transcript height subtracts the panel
  and the composer's growth (`chat::transcript_px`). The composer's cap is NSH
  `max_h` on the multiline input (the style's `maxHeight`), with the cap
  computed from 6 lines of 18 logical pixels. Not visually checked on the
  terminal grid (px to rows conversion): the headless test checks only the
  arithmetic.
- `!command` is a `UiAction::Shell` through the core's `shell`. The old TUI
  echoes `❯ !cmd` into the transcript and prints the output as dim rows; the
  app does not echo (the core's answer arrives as a note or an error).
- `@file` is terminal only. On the web `@` stays plain text: the browser has
  no access to the server's workspace. The walk is `files.rs` (native only,
  `ignore` crate as in the root workspace). The index is built once per
  process from the current directory, so a file added later is not listed
  until restart. The first `@` shows an empty list until the walk ends (about
  a keystroke later).
- Shared suggestion list: `suggest::for_draft` yields the names and rows for
  both kinds; the view and the router call the same function, so the list and
  the keys cannot drift.
- Unix socket: the default socket is probed with `UnixStream::connect`, and
  only when `MODRON_URL` is unset (an explicit URL keeps its own probe).
- Startup config is sent as `SetConfig` frames after attach, in the flag
  order (approval, sandbox, effort, goal, persona, tool lists). With no
  session yet, the app creates one first; that is a change from `--new`'s
  lazy start, and only happens when a config flag is given.
- Strings with no shared home yet: "no tool activity yet" (Ctrl+T note), and
  the `!`, `@file` and Ctrl+T help rows.

## APP-7 remaining parity

Done: one list picker (`nav.rs` `Screen::Picker`, `picker_rows`) for /mode,
/sandbox, /effort, /agent and /theme; Settings Enter opens the pickers (the
old terminal's Settings screen had no per-row cycling; its bare commands
opened menus, so Enter does too), and Left/Right cycling is gone; Shift+Tab
still flips the mode. Permission-rule and allow/deny removal with `d` and a
y/n confirmation (`ConfigPatch.rules` through `set_config`, `set_tool_lists`).
Help has the old terminal's three tabs (General, Commands, Shortcuts), with
scrolling and a wheel on the web. Themes: oculus dark, oculus light and
system (`src/theme.rs`), saved to the config file or localStorage only when
picked. The terminal loop ticks the spinner while a turn runs. Tests: the
resume line on exit (`exit_lines`), the /reasoning and /thinking toggle
mapping, the tick deadline (none when idle), the picker, removal, help tab
and theme paths, and web-test (l) for the theme.

Not done:
- The spinner period is the app's 120 ms (`ANIM_PERIOD`); the old terminal's
  poll is 50 ms (`crates/modron-tui/src/run.rs`), and its frame rate was not
  read. Not checked in a pty.
- The web `/theme` picker is checked headless only; the terminal picker is
  checked by the headless harness, not in a pty.
- Flake: web-test (s) (credentials: `auth_set_key sent the wrong key`) failed
  in the first full run after the picker change and passed in the next three;
  not diagnosed (the key entry's typing is the suspect).

# Packet APP-8: persona picker, task summaries, credential removal

Closed (the two gaps above are gone):
- Agent picker: `/agent` and the Settings agent row send `personas_req` when
  the picker opens (`UiAction::ListPersonas`); the rows are the core's
  `PersonaRowView.text` verbatim, the current one marked, Enter sends
  `SetPersona` (a `set_config` frame). "none" stays for an empty list. No
  clear row: the server refuses an empty persona name (hub.rs `persona`).
- Tasks: rows come from `snapshot.task_rows` (text and summary). The timer
  (`Nav::task_refresh_due`, driven by `Host::tick` through the host's loopback)
  asks `refresh_task_statuses` every 5 s while the Tasks screen is on top and
  never elsewhere; `Host::tasks_open` keeps the terminal loop waking for it.
  The core round-robins ten rows per call, so `Nav.task_covered` is an
  approximation: it starts at the first batch on open and advances per call,
  and a move past it asks for the next batch.
- Credentials: `d` (terminal) or the web `remove` control on a signed-in row
  asks `credential_remove_confirm_text` y / n, then `auth_remove` through the
  session's attached session. No optimistic change: the rows come back in the
  snapshot.

Tests: headless (nav unit tests for picker rows, the timer, batch coverage and
removal; live.rs render tests for the summary and the persona rows), scripted
server (`agent_picker_asks_for_personas_and_enter_sends_the_chosen_one`,
`d_on_a_signed_in_provider_asks_then_y_sends_auth_remove`), web cases (s2, s3).

Note for headless tests: a navigation step (Enter on a slash command) reaches
the screen only when the host's actions are pumped (`Harness::action`), since
`Host::try_action` applies nav steps; the scripted driver does this in `pump`.

# Packet APP-9: approval guard, newline keys, scroll, commands, e2e terminal modes

## Done
- Approval type-ahead guard in `chat::route_key` (terminal and web). Window
  300 ms on the app clock, started when the prompt's request id (or the plan)
  first shows; a queued second approval gets its own window. Decision keys
  y/a/n and Esc (approval), y/r (plan) are consumed. Old TUI lib.rs:962-973
  has no visible cue, so none is shown. Tests: `app/src/chat.rs` router tests,
  `app/tests/chat.rs` (`a_decision_key_inside_the_window...`, queued, restart,
  plan), `app/tests/live.rs` (`y_sends_the_approval_decision_over_the_wire`
  waits past the window), `web-test.js` (e2e) and `waitPastPromptGuard` in
  `e2e/lib.mjs` (every browser approval tap waits the window on the page clock).
- Ctrl+J, trailing backslash, Shift+Enter: keyboard flags pushed only when
  `supports_keyboard_enhancement` says yes, popped on every exit path next to
  bracketed paste. Unit test over the ordered steps.
- Esc on an idle empty composer returns a scrolled transcript to the tail;
  "N lines above" hint via the shared `scrolled_up_hint`.
- `/allow` / `/deny` bare open Settings on the list; `/auth <provider>` jumps to
  its row; `/sandbox bubblewrap` = bwrap.
- `/reasoning` on also calls `set_reasoning_summary(true)`; the live test checks
  the wire request. A refusal shows through `last_error`.
- Untested DONE rows (APP_PARITY.md): 1 to 5 and 9 have app tests in
  `tests/chat.rs` (caret, Delete, Up/Down, /clear, /exit, context gauge). 6 to 8
  (generative UI, turn-end usage, swarm summary) are covered by the existing
  `tests/live.rs::turn_end_usage_swarm_and_generative_rows_render_from_the_core_entries`.
  10 (protocol handshake refusal) is `a_server_one_protocol_ahead_is_refused...`.
  11 (10 KiB `!command` cap) lives in the core: `crates/modron-core/src/engine.rs`
  (the cap test near line 7300); an app test would only check a frame the server
  already truncated, so none was added.
- Persona clear: the agent picker shows `PERSONA_CLEAR_NOTE` (shared, in
  `mobile/rust-core/src/view.rs`). Server change needed to allow clearing: accept
  an empty persona name in the persona setter (`crates/` side) and have the app
  send it as the "none" row.
- E2E: T2 (Esc in the window ignored, past it the mock's next tool prompt
  follows the denial), T3 (one Ctrl+C interrupts, app stays, second quits),
  T4 (Ctrl+J draft, stored by the serve as one two-line task), T4b (trailing
  backslash, same check). T4 and T4b read the serve's session store
  (`<home>/.modron/sessions/*.jsonl`): the local client's `attach` did not
  return the TUI's session.

## Open
- Browser `pageerror: closure invoked recursively or after being dropped`
  appeared intermittently in the e2e viewers (T4 check once; S5 once, in
  viewerB, at approval resolution). Not reproduced by `web-test.js` (33/33 in
  three runs, no console errors). Not diagnosed.
- Parallel chat tests were flaky because the harness released its serial
  lock before the view dropped (field order). Fixed; the single-thread pin
  is gone. A few chat.rs unit tests still touch the globals without a lock.

# Packet PUSH-2: web push (Settings row, web only)

## Result
- Settings shows "notify this device: off|on|unavailable" on the web host.
  Tap subscribes or unsubscribes. The state rules and bodies are pure and
  unit-tested (`src/push.rs`); the browser calls are in `src/web.rs`, with
  `Reflect` and `JsFuture` only, so no closure is created and none can outlive
  its owner. Labels are shared in `modron-mobile-core` view.rs (`PUSH_*`,
  `push_line`).
- The row is hidden unless the key route answers 200 for a non-viewer and the
  browser has `serviceWorker` and `PushManager`. An iOS tab without them shows
  "unavailable" with the Home Screen hint. A blocked permission leaves the row
  off and shows the shared reason under it.
- Request bodies match `cloud/bridge/src/index.ts` exactly: subscribe sends
  `endpoint` and `keys` (`p256dh`, `auth`), plus `invite` only for an approver;
  unsubscribe sends `endpoint`, plus `invite` for an approver. The bridge
  refuses unknown fields, so nothing else is sent.

## Gap: an approver invite cannot reach the push routes from the page
- `GET /i/<id>.<sig>` redirects to `/app/?url=<ws url with ticket>` and does
  not carry the invite token. The page never sees `<id>.<sig>`, so an invited
  approver's page probes with no invite, gets 401, and the row stays hidden.
  The `?invite=` path works (tested), but the portal never produces that
  address. Fix in cloud/bridge (`handleInviteLink`): add `&invite=<id>.<sig>`
  to the redirect. Not changed here (cloud/ is outside this packet).

## Decisions
- Permission is not part of the row state. A blocked permission keeps the
  row "off" and the reason stays under it (not the 2 s note, which expires).
- The toggle is `UiAction::TogglePush`, routed like the portal actions (web
  host only, `is_portal`). The terminal never sends it and never shows the row.
- The probe runs only where the browser could show the row (`supported` or an
  iOS tab), so other browsers make no request to the key route.
- The iOS check is the user agent (iPhone, iPad, iPod) with
  `navigator.standalone` not true. It is an approximation of "iOS Safari".
- Viewer-invite test: the key route answers 403, which Chromium logs as a
  console error. That one message is expected in that test only; the global
  allowed set (401, 429, 503) is unchanged.

## Not done or not verified
- No real push service was reached. The browser tests stub
  `serviceWorker`, `PushManager` and `Notification`, and the bridge is a
  scripted site. Delivery to a device and the service worker's `push` handler
  are not exercised.
- The bridge-side redirect gap above.
