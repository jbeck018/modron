# Modron app

The unified client: one NSH chat screen (`src/chat.rs`) over the shared
client core (`mobile/rust-core`), with two hosts.

- Terminal host: `src/bin/modron-app-tui.rs`, `src/session.rs`, `src/demo.rs`.
- Web host: `src/web.rs` (wasm32 only), the same link, driven by the page timer.

## Terminal

    cargo run --bin modron-app-tui                     # the local server (see below)
    cargo run --bin modron-app-tui -- --demo           # built-in demo script
    cargo run --bin modron-app-tui -- --url ws://127.0.0.1:7000 [--token T]

With no flags the app picks its server in this order:

1. `MODRON_URL` (default `ws://127.0.0.1:7699`) answers a websocket
   handshake within 500 ms: connect to it.
2. Nothing answers and the address is loopback: start `modron serve
   --listen 127.0.0.1:<port>` as a child of the app, wait up to 5 s for the
   port, connect. The app stops that child on exit (SIGTERM, SIGKILL after
   2 s), also on Ctrl+C and on panic. A server that was already running is
   never touched.
3. Otherwise (remote address, or no binary): a full-screen message with the
   reason and the two ways out. Any key exits non-zero.

`--url` connects to that server without probing; `--demo` plays the demo.

Flags:

- `--token T`: device token for the server (any mode that connects).
- `--session UUID`: attach to that session.
- `--new`: attach to nothing; the first submit starts a new session.
- Default (no session flag): the most recently updated session whose folder
  is the current directory; with none, a new session on the first submit.
- `--continue`: the most recently updated session of any folder; with none,
  a message screen that exits non-zero.
- `--provider P`, `--model M`: passed to a server the app starts.
- `--keep-server`: leave a server the app started running after exit.

Environment:

- `MODRON_URL`: the server address to probe, connect to, or start.
- `MODRON_BIN`: the `modron` binary to start. When set it must be a file
  (no fallback). Otherwise `modron` next to the app's executable, then `modron`
  on `PATH`.
- `XDG_STATE_HOME`, `HOME`: the log of a server the app starts goes to
  `$XDG_STATE_HOME/modron/app-serve.log`, or `~/.local/state/modron/app-serve.log`.
  Server output never reaches the terminal UI.

Connection line: `connected` / `connecting` / `disconnected` (the core's
words), then the address, then `started by modron-app` when the app started
the server. If the connection drops, the app shows `disconnected`, retries
every 2 s (doubling, capped at 30 s) and re-attaches to the same session.
Esc or Ctrl+C leaves while it retries.

Keys: Ctrl+C exits. Esc interrupts; y/a/n decide approvals; Enter sends,
Alt+Enter queues; `/compact` compacts.

## Web bundle

Needs the NSH CLI (0.5.1), the wasm32 target, and the wasm-bindgen CLI at
the version the runtime links (0.2.130; the build prints the name it wants).

    rustup target add wasm32-unknown-unknown
    cargo install wasm-bindgen-cli --version 0.2.130 --locked
    nsh package --target web -o dist        # writes dist/Modron/

Serve `dist/Modron/` with any static server. The page address picks the mode:

- `index.html` with no query: the page asks its own origin for `/api/me`.
  200 shows the owner's session list (the portal), 401 the sign-in screen,
  anything else (the portal is off) the built-in demo.
- `index.html?url=ws%3A%2F%2Fhost%3A7000&token=T&session=UUID`: live server.
  `url` is required for live mode; `token` and `session` are optional. A
  signed-in owner also gets the Settings share section (invite links).

The portal's sign-in token is sent once to `POST /auth` and never stored: no
query string, no storage. The list refreshes every 15 s while it shows.

The real server refuses browser Origins unless started with `--allow-origin`.

## Tests

    cargo test                                        # headless NSH + scripted server
    cargo clippy --all-targets -- -D warnings
    cargo check --lib --target wasm32-unknown-unknown
    nsh package --target web -o dist && node web-test.js

`web-test.js` uses Playwright and headless Chromium (preinstalled). It serves
`dist/Modron/` with `python3 -m http.server` and runs a scripted WebSocket
server in Node (RFC 6455 by hand). Set `WEB_TEST_DEBUG=1` to print the composer
and page text after Enter.

## Layout

- `src/chat.rs`: screen, model and key routing (target-neutral).
- `src/session.rs`: the driver (`Driver::step`), target-neutral.
- `nsh.json`: app id `dev.modron.app`, theme `theme/oculus.json`.
