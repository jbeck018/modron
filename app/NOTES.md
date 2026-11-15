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
