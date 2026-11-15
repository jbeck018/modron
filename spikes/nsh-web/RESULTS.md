# NSH web target spike: results

Crates: nsh-core, nsh-base pinned =0.5.1 (CLI nsh 0.5.1, commit 224d9a7). Host: Linux x86_64,
toolchain 1.99.1 (from /home/user/modron/rust-toolchain.toml). Machine was shared, so timings are
noisy. Every number below comes from a command run in this spike. Not measured: anything not listed.

Verdict: yes, a Rust-only NSH app builds and runs for the web target with the CLI. Packaging
is a 6.5 MB static site (5.4 MB wasm, 1.95 MB gzip -9, 1.40 MB brotli as shipped by nsh). The DOM is
real HTML (290 elements, real `<input type=text>`, no canvas). Load is about 150-165 ms to the first
row in headless Chromium. Streaming at 250 tokens/s keeps rAF at 60 Hz. Typed input reaches the app.
The Rust websocket probe (modron-mobile-core) compiles for wasm32 with no errors.

## 1. Build route and cold build time

Route: the nsh CLI (`nsh package --target web`), not a bare cargo build.

Commands (run from /home/user/modron/spikes/nsh-web, with CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0):

    NSH_INSTALL_DIR=/tmp/claude-0/nsh sh <scratch>/nsh-dl/install.sh     # "Installed nsh 0.5.1 (224d9a7)"
    nsh init demo-web --template rust        # used only as a template; files copied into this spike
    rustup target add wasm32-unknown-unknown --toolchain 1.99.1-x86_64-unknown-linux-gnu
    nsh package --target web -o dist         # final, successful run

Spike layout: Cargo.toml (own `[workspace]`, deps nsh-core and nsh-base =0.5.1), nsh.json
(`"rust": "."`, `"root": "App"`), src/lib.rs (the app and a `#[native_module]` declaration that the CLI
describe step requires), .gitignore (target/, dist/, node_modules/, .nsh/).

Cold build: `nsh package --target web` real 3m40.7s (`built the web runtime of nsh-web-spike (3m 29s)`,
`packaged dist/nsh-web-spike, 6.5 MB (9.5s)`). The wasm dependency tree was cold; the native
nsh-base compile had already run in a failed attempt (below), so this is a warm-native, cold-wasm number.
`nsh component add web` was not run: packaging worked without it.

Failed attempts (verbatim first error lines, all fixed):
- `error[E0594]: cannot assign to `n`, as it is a captured variable in a `Fn` closure` (set_interval takes Fn). Fixed with Cell.
- `error[E0277]: a crate of the app (nsh.json "rust") has no #[native_module("...")] at its root`. Fixed by adding the declaration.
- `error[E0463]: can't find crate for `core`` / `the wasm32-unknown-unknown target may not be installed`. The repo pins 1.99.1, which lacked the wasm target. Fixed with rustup target add.

## 2. Output size (dist/nsh-web-spike, from `ls`/`find -printf` and gzip)

| file | bytes |
|---|---|
| nsh.89489f7931.wasm | 5,378,026 |
| nsh.89489f7931.wasm.gz (nsh's own) | 1,946,860 |
| nsh.89489f7931.wasm.br (nsh's own) | 1,398,355 |
| nsh.5bbd9d9dfb.js (DOM runtime) | 181,383 |
| nsh-syntaxes.ea0da56d04.bin (loaded on first code block) | 921,717 |
| nsh-emoji.7cbf0b728f.bin (loaded on demand) | 104,288 |
| index.html | 7,416 |
| browser.bddb1349e4/ (29 files of JS glue) | 213,401 |
| manifest.webmanifest | 238 |

- Plain files, excluding .br/.gz copies: 6,806,469 bytes.
- All files incl. .br/.gz copies: 12,115,019 bytes.
- wasm via `gzip -9 -c <wasm> | wc -c`: 1,946,233 bytes.
- wasm brotli: the `brotli` CLI is not installed and Python's brotli module is missing (ModuleNotFoundError), so this is the nsh-produced .br file (1,398,355 bytes), not a measurement done here.

## 3. DOM vs canvas, accessibility (Chromium, 1280x800, after 2,000 rows render)

From `measure.js`: element_count 290 (div 185, span 88, input 1, status role 1), canvas_count 0.
The input is a real `<input type="text">`. No aria-label on it, no role on it. aria-live: 2 elements.
Only role present: `status` (1). body user-select: auto (text rows are selectable by default; the row
selection itself was not exercised). Accessibility labels were not measured beyond these attributes.

## 4. Load time (navigation start to first row text, `performance.now()` at first `row 00000` in innerText, 5 runs)

- Desktop 1280x800: runs 181.4, 160.4, 166.2, 208.5, 165.7 ms. min 160.4, median 166.2.
- iPhone 13 (devices['iPhone 13']): runs 158.6, 178.1, 161.9, 173.2, 151.8 ms. min 151.8, median 161.9.

Note: the timestamp fires on text presence, not on pixel paint. Headless Chromium, local python server, fresh context per run.
Earlier run gave desktop median 151 ms and iPhone median 150.4 ms (run 1, same script before a fix).

## 5. Streaming (typed "stream" + Enter, 2,000 tokens of "tok " appended to the last row)

Stream implementation: `set_interval(Duration::from_millis(4), ...)` appends one token per tick.
Frame sampling: requestAnimationFrame deltas in the page until status shows `tokens 2000 |`.

- Total time: 8,027.9 ms (2,000 tokens, ~249 tokens/s, set by the 4 ms timer).
- Frames sampled: 480. Mean 16.67 ms, median 16.7 ms, p95 16.7 ms (60 Hz; no dropped frames).

The 8 s total is the interval period, not a rendering limit (a 4 ms period gives 250 ticks/s).
Not measured: the cost of a single token update in ms (rAF only).

## 6. Input

- Desktop: Playwright click on the input, types "hello spike", Enter. The status row reached `rows 2001`
  (the submitted row was appended to the list off-screen, so the row itself is not in innerText). Received: yes.
  After Enter the input value was cleared to "".
- iPhone 13: tap on the input focuses it: activeElement is `INPUT`, type `text`, is_text_entry true.
  Typed "hi phone" + Enter also reached the app (`rows 2001`). Viewport width 390, no horizontal scroll.
- Virtual keyboard itself is an OS surface and cannot be observed in headless Chromium; focus on a real
  text input is the proxy measured here.

## 7. Rust websocket client (modron-mobile-core) compiles for wasm32

Probe: temporary path dependency `modron-mobile-core = { path = "/home/user/modron/mobile/rust-core" }`
added to this spike's Cargo.toml, then:

    cargo check --target wasm32-unknown-unknown --lib

Result: exit 0, `Checking modron-mobile-core v0.1.3 (/home/user/modron/mobile/rust-core)`, `Finished dev profile ... in 42.03s`,
no errors and no warnings in the log. The probe dependency was then removed again (Cargo.toml restored).
Cargo.lock was regenerated by that check (320 packages, a spike artifact). The websocket transport was not run in a browser.

## 8. Missing or broken

- `nsh package --target web` needs a wasm32-unknown-unknown target on the toolchain the repo pins (see section 1).
- The app must carry an `#[native_module]` declaration even for web (CLI describe step).
- `set_interval` callbacks are `Fn`, not `FnMut`.
- nsh warns: `app id com.example.nshwebspike is a placeholder`.
- Submitted rows land at the end of the virtual list, so a transcript that is scrolled to the top does not show them without scrolling.
- Stream and input were checked only in headless Chromium. Real phone browsers and the software keyboard were not measured.
- The nsh-platform-web crate is not in the local cargo registry; the web runtime is built by the CLI, not by a direct dependency.

## Reproduce

    cd /home/user/modron/spikes/nsh-web
    nsh package --target web -o dist
    cd dist/nsh-web-spike && python3 -m http.server <port> --bind 127.0.0.1 &
    node /home/user/modron/spikes/nsh-web/measure.js http://127.0.0.1:<port>/index.html
