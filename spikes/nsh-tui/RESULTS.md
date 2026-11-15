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

## Follow-up S2b

Commands run in this spike; sources are the nsh-*-0.5.1 crates in the cargo registry.
Final Cargo.toml: `nsh-platform-tui = { version = "=0.5.1", default-features = false }` and
`tokio = { version = "1", default-features = false, features = ["sync"] }`. No `[profile.release]`
(the size profile was measured and removed; see A.4).

### A. No-JavaScript build

Feature switches (Cargo.toml `[features]`):
- nsh-platform-tui: `default = ["js"]`, `js = ["nsh-engine/js"]`. Its dependency on nsh-engine is
  already `default-features = false, features = ["net"]`, so turning off its default is enough.
- nsh-engine: `default = ["js"]`, `js = ["js-core", "zlib", "crypto"]`, `js-core = ["dep:nsh-js", ...]`
  (quickjs comes in through nsh-js).
- nsh-core: no JS feature. Nothing to set.

Dependency line that works (the only change):
`nsh-platform-tui = { version = "=0.5.1", default-features = false }`

- Build: `cargo build --release`, fresh target dir: **3m 11s wall** (real 3m11.264s, user 9m53s).
  The default-feature build measured earlier in this spike took 283.4 s.
- Binary, default release profile: **48,123,552 bytes unstripped**; stripped copy (`strip -o`):
  **34,065,368 bytes**. Earlier default-feature build: 62,307,528 unstripped / 45,364,568 stripped.
- `cargo tree -e normal | grep -ci quickjs`: **0**.
- `cargo tree -e normal --prefix none | sort -u | wc -l`: **343** (default features: 497).
- Headless run passes: `startup` 1.68 ms first frame; `input` typed_readback ok, enter_submit ok,
  paste_counted ok.

Size levers (`[profile.release]` with opt-level, lto = "fat", codegen-units = 1, panic = "abort",
strip = true; each a full rebuild):
- opt-level = "z": **17,073,608 bytes** stripped, build 259 s. Smallest. Headless input run ok.
- opt-level = "s": **19,340,424 bytes** stripped, build 279 s.
- Not measured: the profile's unstripped size (strip is on), and opt-level 3 with the same LTO.
- cargo-bloat: not installed, so no per-function breakdown.
- Largest rlibs in target/release/deps (pre-link archives, from the default-profile build, `ls -S`):
  libnsh_base 106.0 MB, libnsh_core 53.6 MB, libzerocopy 15.8 MB, libsyn 12.4 MB, libsyn 12.3 MB,
  libtokio 10.6 MB, libnsh_platform_tui 11.3 MB, libdarling_core 8.7 MB, librustix 8.6 MB,
  libregex_automata 8.5 MB. nsh-base is a non-optional dependency of nsh-platform-tui; it is the
  largest single crate. Rlib size is not linked size; this is only a rank.

### B. External events (Rust host, tokens from another thread)

Found, and used in the spike (`src/app.rs`, `src/main.rs`):
- Receiving side: `nsh_core::element::spawn` (nsh-core ambient.rs:1195) runs a future on the engine
  thread under the view's owner (ends with the view). The future awaits a
  `tokio::sync::mpsc::UnboundedReceiver<String>`, which is runtime-agnostic and needs no runtime handle.
  Each token does `tokens.update` and appends to the last `rows` entry.
- Wake path: the task waker is `TaskWaker` (nsh-core element/task.rs:49-60). It is `Send + Sync` and
  calls `Ready.notify` from any thread. The engine installs that via `set_task_notify`
  (core/components.rs:1174) and `set_wake` (nsh-engine lib.rs:1849-1857, which sends `Message::Wake`
  and then calls the host wake). The host sets it with `AppOptions.wake` (platform-tui app.rs:106) or
  `AppView::set_wake_callback` (app.rs:547-548). `WakeCallback = Arc<dyn Fn() + Send + Sync>`
  (app.rs:38).
- Producer: a plain `std::thread` calling `tx.send(token)`.
- Not found in 0.5.1:
  - `SurfaceOptions.runtime`. `AppOptions` (app.rs:98-130) has no runtime field. `runtime:
    Option<tokio::runtime::Handle>` exists only on `nsh_engine::EngineConfig` (nsh-engine lib.rs:961),
    and `AppView` builds `EngineConfig` (app.rs:308-330) without setting it, so the process-shared
    runtime is used.
  - A Rust-side push channel. `HostChannel::send` (nsh-engine host_channel.rs:142) is `Send`, but it
    only reaches JS `host.on` listeners. `HostChannel::handle` is for the app's own calls.
  - `blocking` (ambient.rs:1426) and `background` (ambient.rs:1444) run work off the engine thread;
    they do not return results to a signal.
- Measured (`external <rows> <pace_us>`, 2,000 tokens, `TestBackend` 120x40):
  - Flood (pace 0): 2000/2000 shown in 6.4 ms; 3 draws; 2 wakes; 0 wait timeouts.
    Draw ms: mean 0.287, p95 0.779. Token send-to-visible: mean 6.130, p95 6.235 ms.
  - Paced (1 ms between sends): 2000/2000 in 2202.4 ms; 1096 draws; 1095 wakes; **0 wait
    timeouts**; idle draws 12. Draw ms: **mean 1.368, p95 3.177**. Token send-to-visible:
    mean 5.401, p95 9.966 ms.
- Wake without polling: yes. The host blocks in `recv_timeout` with no fixed poll interval; the wait
  is `next_frame_deadline()` or 5 s, and every wait ended on a wake (0 timeouts).

### C. Input focus after Enter

Found: `input().submit_behavior(SubmitBehavior::Submit)` (nsh-core element/host.rs:774;
`SubmitBehavior::Submit` = "Reports onSubmit and keeps the focus", ops.rs:1381-1385; resolved in
mount.rs:612-619; applied at text_input.rs:285-300). The default (`BlurAndSubmit`) unfocuses after
submit. Also works: `element_ref()` (ambient.rs:1472) on the input via `.element_ref(&field)`
(builder.rs:879), then `field.focus()` (element_ref.rs:147) inside `on_submit`.

Headless check (`NSH_SPIKE_FOCUS=<mode> app focus`: type "hello world", Enter, type "again", no Tab):
- `blur` (default): enter_submit ok; type_after_enter_no_tab **FAIL** (input empty, focus gone).
- `submit` (submit_behavior): enter_submit ok; type_after_enter_no_tab **ok**.
- `ref` (element_ref().focus() in on_submit): enter_submit ok; type_after_enter_no_tab **ok**.
