# Modron Remote

The mobile remote-control shell (Phase 5 spike). A TSX app with a Rust crate of its own:
`App.tsx` is the app, and the directory is also a crate (`Cargo.toml`, `src/lib.rs`;
`nsh.json` `"rust": "."`) whose `#[native_module("modron-remote/native")]` exports the
`Remote` module. `Remote` wraps `mobile/rust-core` (`modron-mobile-core`, the shared
remote-client core): one `RemoteClient` on a background tokio runtime, the folded
`ViewModelSnapshot` handed to TSX as JSON so the screen renders the shared copy verbatim
(the AGENTS.md UI/UX parity contract - TSX styles, it never re-formats).

```sh
nsh run                      # this machine's backend; --target ios, android, web, macos, linux, windows or gpui
nsh check                    # cargo check of the crate, then type checks App.tsx against the declarations generated from it
nsh test                     # cargo test, then App.test.tsx, headless; --target to run it on a platform
nsh package --target macos   # or ios, android, web, linux, windows
```

The app boots in demo mode (scripted snapshot, no server). Toggle `demo off` and point at
`ws://127.0.0.1:7699` (or any remote host) to drive a live session: connect, attach,
transcript, streaming, approvals (allow once / allow for this run / deny), stop.

`nsh` describes the crate's exports with a small program of the crate and builds the app's hosts with it linked in.
TSX edits are hot updates; Rust edits rebuild and restart the app. The first build writes the crate's TypeScript declarations into
`.nsh/types/modron-remote/native.d.ts`, doc comments included.

The `nsh` commands build the app's hosts from the NSH crates `Cargo.toml` depends on: crates.io's at the version of the
`nsh` you run (keep the `nsh-*` dependencies at that version when you upgrade `nsh`), or an NSH checkout's by path.

NSH-generated files stay in `.nsh/`: runner sources in `.nsh/rust/` and default packages in `.nsh/out/`.
Cargo keeps its normal `target/` directory and respects your existing `CARGO_TARGET_DIR`.
Spike findings and the verdict live in `../SPIKE.md`.
