# Modron app

The unified client: one NSH chat screen (`src/chat.rs`) over the shared
client core (`mobile/rust-core`), with two hosts.

- Terminal host: `src/bin/modron-app-tui.rs`, `src/session.rs`, `src/demo.rs`.
- Web host: `src/web.rs` (wasm32 only), the same link, driven by the page timer.

## Terminal

    cargo run --bin modron-app-tui                       # demo script
    cargo run --bin modron-app-tui -- --url ws://127.0.0.1:7000 [--token T] [--session UUID]

Ctrl+C exits. Esc interrupts; y/a/n decide approvals; Enter sends,
Alt+Enter queues; `/compact` compacts.

## Web bundle

Needs the NSH CLI (0.5.1), the wasm32 target, and the wasm-bindgen CLI at
the version the runtime links (0.2.130; the build prints the name it wants).

    rustup target add wasm32-unknown-unknown
    cargo install wasm-bindgen-cli --version 0.2.130 --locked
    nsh package --target web -o dist        # writes dist/Modron/

Serve `dist/Modron/` with any static server. The page address picks the mode:

- `index.html` with no query: the built-in demo.
- `index.html?url=ws%3A%2F%2Fhost%3A7000&token=T&session=UUID`: live server.
  `url` is required for live mode; `token` and `session` are optional.

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
