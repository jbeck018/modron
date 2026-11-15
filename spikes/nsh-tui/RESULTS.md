# NSH terminal backend spike: results

Crates: nsh-core, nsh-base, nsh-engine, nsh-platform-tui pinned to =0.5.1; ratatui =0.30.2.
Host: Linux, Rust/cargo 1.99.1. Other agents were compiling the main workspace concurrently,
so build times are noisy. Every number below comes from a command run in this spike.

Verdict: the terminal backend compiles, renders a scrolling transcript, a text input and a
status row through `AppView` + ratatui `TestBackend` (120x40), and handles keys, paste and
scrolling headlessly. It pulls a JavaScript engine (quickjs) in the default feature set.

## 1. Compile and render

- Compiles and renders. `cargo build --release` (fresh target dir, clean deps):
  **283.4 s wall** (4m42s, `cargo` reported `Finished ... in 4m 42s`).
- Release binary (`cargo build --release`, not stripped): **62,307,528 bytes** (62.3 MB).
- Release binary, stripped (`strip` on a copy): **45,364,568 bytes** (45.4 MB).
- Compile errors hit on the way: `cannot find function input in this scope` (fixed by
  `use nsh_core::element::input;`; `input` is not in `nsh_core::prelude`).

## 2. Process start to first rendered frame (5 runs, `startup` mode)

Measured with `Instant` in `main` to the first frame whose buffer contains the status row.
Values in ms: 2.27, 1.85, 1.93, 1.63, 1.67.
- min **1.63 ms**, median **1.85 ms**.
- Note: the timer starts in `main`, not at exec, so it excludes process spawn and dynamic loading.

## 3. Streaming (2,000 tokens of "tok ", one at a time, frame drawn after each)

Measured end to end per token: paste event sent to the view, then draw loop until the
status counter shows the new token count.

| transcript rows | mean | median | p95 |
|---|---|---|---|
| 0 | 2.724 ms | 2.819 ms | 4.930 ms |
| 5,000 | 1.372 ms | 1.222 ms | 2.217 ms |

Caveats:
- No Rust-side API to push model output into a running app was found in 0.5.1. `HostChannel`
  (nsh-engine) is the JS `host.on` bridge. The spike therefore feeds tokens as
  `Input::Paste`, which reaches the input's `on_paste` handler and appends to the last row.
  This measures the input-event-to-render path, not a model-output path.
- The 5,000-row run was a little faster than the 0-row run in this sample. This is not an
  explanation, only a measured difference.

## 4. Scrollback (virtual list)

- `virtual_list(count, |at| ...)` exists in nsh-core and works in the terminal backend. It
  draws a scrollbar on the right edge.
- Scroll with PageDown and PageUp. End did not scroll the list in my run (the input had focus).
  - Down: 138 pages to the last row (`row 04999`), reached: yes. Per page: mean 0.868 ms,
    median 0.364 ms, p95 2.560 ms.
  - Up: 135 pages back to `row 00000`, reached: yes. Per page: mean 0.751 ms, median 0.517 ms,
    p95 1.987 ms.
- Pitfall hit (found by testing; the virtual_list doc comment explains it): the row binding captured `at.index()` when the cell was built. Recycled cells
  then showed stale rows (rows came out in the wrong order). Reading `at.index()` inside the
  `text(move || ...)` closure fixes it. The virtual_list doc comment says cells are made once and
  rebound, so the binding must read the position reactively.
- The list needs an explicit height (`.h(px(600.0))`). `flex_1()` in a `size_full()` column
  gave a zero-height list in my layout (the status row moved to the top).

## 5. Text input

- Typed keys: `view.handle(Input::Key { key: "h", .. })` works per character; the text reads
  back on screen. Typing "hello world" read back: ok. A space is the key name `" "`; `"Space"`
  types the literal word. Needs `auto_focus(true)` on the input (or a focus event), otherwise
  keys are ignored.
- Enter submits (on_submit fires; the transcript row `> hello world` appears): ok.
- Paste: `Input::Paste` reaches `on_paste` and also inserts into the input. It only works while
  the input has focus. After Enter (submit) the input lost focus: paste was ignored until a Tab
  key moved focus back (then paste counted and the text showed: ok).
- Multi-line input: not measured. The input is single-line in this spike
  (`multiline(true)` exists in the API but was not built or tested).

## 6. Problems, dependencies, JS engine

- Errors hit (all fixed in the spike): `cannot find function input in this scope` (import).
  No runtime errors. No panics.
- Dependencies (`cargo tree`, normal edges):
  - Default features (nsh-platform-tui `default = ["js"]`): 497 unique packages, 957 tree
    lines; 12 unique `nsh-*` packages.
  - `default-features = false` on nsh-platform-tui: 343 unique packages; 0 matches for
    quickjs or nsh-js.
- JavaScript engine: yes, in the default build. `quickjs-jit-sys v0.14.2` and `nsh-js v0.5.1`
  are pulled in through nsh-engine. Turning default features off removes them. This build
  then keeps the Rust-only path. The tree was checked only, not built or run without js.
- Not measured: the binary size and build time with default features off; behavior on a real
  terminal (the spike is headless by design); `run-app` feature (not used).
