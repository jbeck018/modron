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
