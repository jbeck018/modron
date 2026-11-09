# mobile/SPIKE.md - the nsh spike (Phase 5, mobile track)

Opened/closed 2026-11-09. Track 1 deliverable: `mobile/rust-core`
(`modron-mobile-core`), the shared remote-client core. Track 2 deliverable:
this spike - `mobile/app`, a real nsh app wired to that core.

## Verdict: GO on nsh

nsh v0.4.0 installs, scaffolds, type-checks, headless-tests, runs the
macOS AppKit target, and packages a signed .app - with our Rust core
linked in and driving the UI. TSX is the first-class authoring path (hot
reload), and the mixed template (TSX + Rust crate) is exactly the shape
the plan wanted: the shared Rust core holds protocol/connection/view-model
logic; the shell renders.

One real defect found (iOS packaging, below) - it blocks the simulator
pass at v0.4.0 but not the verdict; macOS and Android packaging are
unaffected.

## Version pinning, install path

- `nsh 0.4.0 (e626a8d)` - install script pinned: `NSH_VERSION=v0.4.0 curl
  -fsSL https://nsh.dev/install.sh | sh` (also `brew install
  longbridge/tap/nsh`). Installed to `~/.nsh`, linked to `~/.local/bin/nsh`.
  No npm/cargo install path exists; source builds use `cargo build -p
  nsh-cli` from a github checkout.
- The app pins the matching library crates: `nsh-core = "0.4.0"`,
  `nsh-base = "0.4.0"` in `mobile/app/Cargo.toml` (keep these at the CLI's
  version when upgrading).
- Prereqs found on this machine: Rust (already), Xcode 27.1 with the iOS
  26.5 simulator runtime (present), `rustup target add
  aarch64-apple-ios-sim` (had to add it for the iOS build).

## Scaffold layout

`nsh init app --template mixed --name "Modron Remote"` produced:

```
mobile/app/
  App.tsx            # the screens (TSX, hot reload)
  App.test.tsx       # headless tests (nsh/test)
  nsh.json           # app metadata; "rust": "." links the crate
  Cargo.toml         # its own workspace root (empty [workspace] table)
  src/lib.rs         # #[native_module("modron-remote/native")] -> Remote module
  .nsh/              # generated: tsconfig, .d.ts for the crate's exports, runners (gitignored)
  target/            # cargo (gitignored)
```

- The Rust side exports ONE module: `Remote` - a thin bridge over
  `modron-mobile-core::RemoteClient` held in a static, driven by a
  dedicated tokio runtime on its own thread. Data methods return JSON
  strings (`snapshot_json()`, `take_new_entries_json()`,
  `list_sessions_json()`); command methods take plain args
  (`connect/attach/submit/interrupt/approve/select_model/new_session/
  reconnect/detach/disconnect`). `demo_snapshot_json()` returns a scripted
  `ViewModelSnapshot` for the demo mode (no server).
- The `#[module]` macro rewrites exported fn bodies (it mangles `?` and
  Result plumbing): keep exported fns as one-line calls into plain helper
  functions outside the `#[module] impl` block. Generated TS declarations
  land in `.nsh/types/modron-remote/native.d.ts` with camelCase names
  (`snapshotJson()`), doc comments included; errors become thrown/rejected
  promises.

## Screens (what rendered)

`App.tsx`, two routes on a native `NavStack`:

- Sessions screen: connect bar (URL input, defaults to
  `ws://127.0.0.1:7699` via the core's `default_url`), demo toggle,
  session list (`snapshot.sessions`).
- Chat screen: connection header (`connection_label`, the shared
  `status_line`, turn/tool counts, seq), stop control while running, the
  transcript (`snapshot.history[].lines` rendered verbatim, styled by
  entry kind), streaming text with caret + reasoning line when present,
  note/error lines, approval sheet (`pending_approval` + the three shared
  decision labels), composer (Input + send).

Evidence (all captured on this machine, 2026-11-09):

- `nsh check` - `checked nsh-app-modron-remote; built the Rust crate
  nsh-app-modron-remote` (cargo check + TS typecheck against generated
  declarations).
- `nsh test` - 4/4 headless tests pass: sessions screen renders, chat
  screen renders the shared copy (status_line `claude-opus-5 - 18.4k tok -
  cache 71% - ask`, task echo, tool summary line, usage note, streaming
  caret, decision labels), demo approval resolves (sheet disappears),
  composer/stop present.
- `nsh run . --target macos` - builds the crate + bundles App.tsx (17.8 KB
  js / 75.7 KB bytecode) and opens the AppKit window (a windowed app logs
  nothing by itself; the headless runner is the render evidence).

## Perf notes

- TSX hot-update bundle: ~14 KB js, ~25 ms to bundle (dev). Release
  bytecode: 10.5 KB, 30 ms. First-frame/120fps claims from nsh.dev were
  not independently measured here; scroll/streaming perf on device is
  still unmeasured (below).

## Bundle size (the M5a gate)

- `nsh package . --target macos` -> `Modron Remote.app`, ad hoc signed:
  55 MB total. That is the full release runner (AppKit host + QuickJS +
  our Rust core statically linked); the app-specific payload inside is
  the ~10 KB bytecode bundle. nsh's packaging model ships the runtime
  inside each app; per-app payload stays tiny.

## Friction log

1. `nsh-platform-ios 0.4.0` on crates.io is broken: its
   `src/lib.rs` does `mod speech;` pointing at
   `../../nsh-platform-macos/src/speech.rs`, a path outside the published
   package, so `nsh package --target ios-simulator` (and `--target ios`)
   fails compiling from the registry. **FIXED, VERIFIED 2026-11-09** (see
   below). Upstream main has since moved speech into a shared
   `nsh-platform-apple` crate (so the next published release should drop
   the defect); until then run `scripts/nsh-ios-fix.sh` once per machine.

   ### The workaround hunt (what works and what doesn't)

   - **[patch.crates-io] in mobile/app/Cargo.toml: DOES NOT APPLY.** The
     per-target runner manifests under `mobile/app/.nsh/rust/<target>/`
     are what cargo builds for `nsh package`, and nsh **regenerates them
     from templates on every run** (verified by hash: an edited runner
     manifest reverts during the next `nsh package`), so a patch placed
     there is wiped and one placed in the app manifest never reaches the
     runner's independent resolution. Patching a vendored fixed copy of
     the crate is therefore dead on arrival.
   - **Editing the generated runner manifest: wiped every run** (same
     regeneration fact).
   - **Building nsh-cli from a github checkout (SPIKE's original
     suggestion): works** but costs a full CLI source build and an
     out-of-band toolchain swap.
   - **THE FIX (shipped): one symlink in the cargo registry.** The failing
     path resolves to `<registry-src>/nsh-platform-macos/src/speech.rs`;
     `scripts/nsh-ios-fix.sh` links the unversioned name to the extracted
     `nsh-platform-macos-0.4.0/` package (cargo does not re-verify
     extracted packages, so the link persists across builds). Verified end
     to end: `nsh package . --target ios-simulator` -> runner built in
     1m1s -> `Modron Remote.app` (54MB) packaged -> installed and launched
     on the iPhone 17 Pro simulator (iOS 26.5), process stayed alive
     (pid confirmed via launchctl).
2. The `#[module]` macro rewrites exported fn bodies - `?` and
   Result-returning calls inside them fail to compile in surprising ways.
   Pattern that works: exported fns stay one-line, real logic in plain
   helpers.
3. Reactive model is Solid-like: components run once. Conditional UI must
   live in JSX expressions or `<If when={...}>` (global), not in component
   bodies (`if !x() { return null }` never re-evaluates). Cost us one
   debugging pass on the approval sheet.
4. No `key` prop on lists; use the global `<Each of={...}>` for identity.
   (We only needed `If`.)
5. `numberOfLines` exists in TextProps but the class form
   (`line-clamp-2`) is the documented path; `keyboardType` values are
   lowercase (`url`, not `URL`).
6. Theme tokens: no `*-destructive-foreground` pair - use
   `bg-destructive` + `text-background`.
7. TSX errors on `import { Scroll } from "nsh/base"` - Scroll lives in
   "nsh"; Input/NavStack live in "nsh/base". Mixed element homes are
   confusing at first.
8. Headless `nsh test` needs an explicit settle wait (`setTimeout` ~200ms)
   after presses before re-reading the screen.
9. The runtime took 8m37s to build the macOS release runner ONCE
   (nsh-platform crates); rebuilds after that are seconds. Budget CI time
   for the first release build per target.

## Parity conformance (AGENTS.md UI/UX parity contract)

Shared strings the spike screen renders verbatim (all produced by
`modron-mobile-core`, zero formatting in TSX):

- Status line: `status_line` = `<model> - <tok> tok - cache <pct>% -
  <mode>` (TUI status-strip words), plus `connection_label`
  (connected/connecting/disconnected), `running_label` (running/idle),
  `approval_mode_label` (ask/yolo/deny).
- Transcript rows: `history[].lines` from `modron_view::replay` +
  `entry_lines` - task echo (`<task text>` with the accent color, the
  `>` echo prefix), assistant/user text, `ToolSummary.line` (the
  collapsed `N tools - name xN` line), swarm rows (`[label] done -
  3 turns - ...` style), usage notes (TUI token/cache arithmetic),
  model/compaction notes.
- Tool strip: `tool_lines` (the `name - 2.1 KB` done rows and the
  in-flight `name ...` rows via `ToolEvent::summary`), `turn_summary_line`,
  `subagent_rows` (`SwarmRow::summary`).
- Approvals: `pending_approval.options` = the three decisions with the
  shared labels (allow once / allow for this run / deny); allow-all is
  sticky server-side; the sheet is the mobile divergence-table surface.
- Stop control replaces "esc to interrupt" (divergence table: a stop
  button instead of esc).

Interaction semantics encoded in the core API (Track 1) and used by the
shell: submit sends; interrupt aborts; approvals resolve on the three
decisions; `select_model` pins the model AND follows the provider (full
ConfigPatch); config changes are future-turn only (server semantics,
labels update immediately); snapshot resets, Delta appends, buffers commit
at TurnDone and clear on abort. Modal hooks: the shell's back-out is
`stack.pop()` (one screen); type-to-filter is not wired in the spike (the
sessions list is short) - the core exposes plain state fields so a filter
clears before close when wired.

Mobile divergences taken (all presentation, per the divergence table):
sheets instead of the TUI's inline strip, safe-area insets
(`env(safe-area-inset-*)` padding), native keyboard via `<Input>`, native
NavStack pages, no `!` shell passthrough, no ctx/spinner segments (TUI
terminal affordances). Words, states, ordering, semantics: shared.

## Exact next steps for the real-device pass

1. ~~Fix the iOS runner build~~ DONE 2026-11-09, no nsh rebuild needed:
   run `scripts/nsh-ios-fix.sh` once per machine (idempotent), then
   `rustup target add aarch64-apple-ios-sim` (done on this machine).
   VERIFIED: packaged + installed + launched in the installed iOS 26.5
   simulator (`nsh package app --target ios-simulator --run`).
2. `rustup target add aarch64-apple-ios-sim` (done on this machine), then
   `nsh package app --target ios-simulator --run` - launches in the
   installed iOS 26.5 simulator.
3. Device pass: `nsh package app --target ios` (unsigned .app without
   `--sign`), then with signing: `nsh keygen` / `nsh certify` and the
   USER must run, with their Apple Developer identity:
   - `nsh package app --target ios --sign "<identity>"` (or install the
     unsigned bundle via Xcode Devices & Simulator / sideloadly).
   - Android analog: `nsh package app --target android` (`.apk`;
     Android SDK + a debug keystore needed - not attempted on this Mac).
4. Point the app at a live server: run `modron-server` (Phase 2 tranche)
   on the LAN, set the URL field to `ws://<host>:7699`, toggle demo off,
   connect; then exercise attach/resume (kill the server mid-run,
   reconnect) and approvals end to end.
5. Measure on-device: first frame, 120fps scroll over a long transcript,
   keyboard-inset behavior (the composer already pads with
   `env(safe-area-inset-bottom)`; the keyboard frame may need
   `keyboardFrame` handling), and the .ipa bundle size for the M5a gate.
6. When the bridge hardens: replace `snapshot_json()` polling with
   incremental `take_new_entries_json()` in the poll loop and consider a
   `Callback`-based push from the recv loop instead of the 250ms interval.
