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

1. The default unix socket (`~/.modron/serve.sock`, the old terminal's
   default) accepts a connection, when `MODRON_URL` is unset: connect to it.
2. `MODRON_URL` (default `ws://127.0.0.1:7699`) answers a websocket
   handshake within 500 ms: connect to it.
3. Nothing answers and the address is loopback: start `modron serve
   --listen 127.0.0.1:<port>` as a child of the app, wait up to 5 s for the
   port, connect. The app stops that child on exit (SIGTERM, SIGKILL after
   2 s), also on Ctrl+C and on panic. A server that was already running is
   never touched.
4. Otherwise (remote address, or no binary): a full-screen message with the
   reason and the two ways out. Any key exits non-zero.

`--url` connects to that server without probing; `--socket PATH` connects to
a unix socket; `--demo` plays the demo.

Flags:

- `--url`, `--socket`: connect with no probe. A plain `ws://` to a
  non-loopback host is refused unless `--insecure-lan` is given (trusted LAN
  only); the reason is shown on the message screen.
- `--token T`: device token for the server (any mode that connects). Without
  it the token comes from a `?token=` in `--url`, else `MODRON_DEVICE_TOKEN`.
- `--session UUID` (alias `--resume UUID`): attach to that session.
- `--new`: attach to nothing; the first submit starts a new session.
- Default (no session flag): the most recently updated session whose folder
  is the current directory; with none, a new session on the first submit.
- `--continue`: the most recently updated session of any folder; with none,
  a message screen that exits non-zero.
- `--provider P`, `--model M`: passed to a server the app starts.
- `--working-dir DIR`, `--sessions-dir DIR`: passed to a server the app
  starts (`modron serve` takes both); refused when attaching to a server that
  is already running.
- `--keep-server`: leave a server the app started running after exit.
- Config flags, sent to the attached session after attach (future turns
  only): `--yolo` (alias `--approve-all`), `--effort low|medium|high|xhigh|max`,
  `--goal TEXT`, `--agent NAME`, `--sandbox off|seatbelt|bwrap`, `--allow TOOL`
  and `--deny TOOL` (both repeatable). With no session attached yet, the app
  starts a new one so the config has a session to apply to.

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

Tool detail: Ctrl+T opens a panel with this turn's tool rows (the last
group's when the live turn has none), verbatim from the snapshot, scrolled to
the newest row. While it is open Up/Down move a row, PageUp/PageDown a page,
Home/End jump, and Esc or Ctrl+T closes it. On the web, a tap on the turn
summary toggles it.

Shell passthrough: a composer line starting with `!` runs the rest of the
line on the server (through the core's `shell` call, under the usual approval
rules) and never reaches the model. A bare `!` is the server's usage refusal.
Refusals and output arrive through the status line's note or error.
`!!command` is the private form: the same run, shown in the transcript with the
shared private mark, and never sent to the model (the core's
`shell_private`). Its output rows are dim; the command line is bold.

Model picker: Enter switches the session's model only. Shift+S (terminal) or
the "save as default" control (web) on the highlighted row writes that model
and its provider as the server's default for future sessions
(`persist_default_model`). A refusal (for example over a portal tunnel) shows
through the status line's error.

Pickers: `/mode`, `/sandbox`, `/effort`, `/agent` and `/theme` with no argument
open one list screen (`nav.rs` `Screen::Picker`, `picker_rows`). The current
value carries a dot. Type to filter, Up/Down move, Enter applies and closes,
Esc clears the filter, then closes. The Settings rows for approval mode,
effort, sandbox, theme and agent open the same pickers on Enter (the old
terminal's bare commands opened the same menus). `/mode yolo` and the other
arguments still apply directly. Shift+Tab still flips the approval mode.
Agent lists the server's personas: the picker opens with `personas_req`, and each
row is the core's `name - summary` text verbatim, the current one marked. Enter
sends `set_config` with that persona. Until the list arrives, or when the server
refuses it (a portal tunnel), the picker shows the shared "none" row. There is
no row that clears the persona: the server refuses an empty name.

Settings removal: `d` on a permission rule or an allow or deny entry asks
first (y or n). A rule removal sends the rest of the set through
`ConfigPatch.rules` (the setter replaces the whole set); a tool entry goes
through `set_tool_lists`. On the web, a remove control sits beside the row.

Images: Ctrl+V (terminal) reads an image from the system clipboard: `wl-paste`
or `xclip` on Linux, `osascript` (a JavaScript-for-Automation script that writes the
pasteboard's PNG, TIFF converted, to stdout) on macOS, bounded at 2 s per tool
(`src/clipboard.rs`, the tools injectable for tests). An image becomes a chip above
the composer (`[img name, size]`) that the next submit carries; Backspace on an empty
composer removes the last chip before it dequeues a message. A refused submit keeps
its chips. With no image on the clipboard, Ctrl+V pastes the clipboard's text into
the composer. On the web, the `attach` control beside the composer opens the browser's
file chooser for an image: the picked file becomes the same chip (`AttachFile`, read
with `Blob.arrayBuffer`, checked by the core's `attachment_from_bytes`). The old
terminal did not attach typed or dropped paths either, so they stay text.

Tool lists: `/allow <tool>` and `/deny <tool>` add the tool to the list for
future runs (the core's `set_tool_lists`); bare, they show both lists.
`/worktree <title>` creates a task session; bare `/worktree` and `/tasks` open the
tasks screen. `/automations` opens the automations screen.

Automations (`/automations`): the server's rows (name, every N minutes, last run),
type-to-filter (Esc clears the filter, then closes). `a` with an empty filter (or
the web `add` control) opens an inline form: name, prompt, interval minutes. Tab or
Enter moves between fields, Enter on the last field saves, Esc closes it. The
interval must be a whole number of at least 5 minutes (the server's floor, mirrored
in `view::AUTOMATION_MIN_INTERVAL_MINUTES`); a refused form stays open with its
message. `d` (or the web `delete` control) asks `delete <name>? y / n` first. The
list refreshes when the screen opens and after each change.

Tasks (`/tasks`, bare `/worktree`): the sessions that carry a branch, as
`title - branch - running|idle`. Type to filter; `n` (or `new task`) opens the title
form (Enter creates the task in its worktree and goes to chat); Enter attaches the
selected task's session and returns to chat; `m` (or `merge`) asks `merge <branch>
into its base? y / n`. The status line of the selected row is requested when the
screen opens and when the selection moves, and the core's note (the status, or the
merge result, ok or the conflict summary) shows at the foot of the screen. Each row shows the core's summary under it
(`ahead 3 - 1 file (+120 -14)`) once a status arrives. The statuses are asked
for when the screen opens, when the selection moves past the rows the last
refresh covered, and every 5 seconds while the screen shows (never while
another screen is up). The core answers ten rows per call, round-robin.

`/clear` hides the transcript rows on screen in this view only; nothing is sent. A
reconnect or a session change shows them again. Notes in the status area (the core's
`last_note`) expire after 2 s, the old terminal's toast length; `last_error` stays.

`@file` completion (terminal only): an `@` that starts a token opens the
suggestion list with workspace paths that contain the text typed after it
(case-insensitive). Up/Down choose, Tab replaces the token with `@path `,
Esc hides the list, Enter sends as typed. The walk starts at the app's working
directory, respects `.gitignore`, skips hidden entries, `target` and
`node_modules`, keeps 200 paths and stops after 400 ms, on a background thread.

The multi-line composer grows to 6 lines and then scrolls inside itself; the
transcript shrinks to match.

Keys (the same on web, where the router sees DOM keys): Enter sends;
Shift+Enter, Ctrl+J, or a line ending in `\` adds a line (the `\` is
replaced by the line break). A pasted block arrives as one input and never
submits. Alt+Enter queues. Esc denies a pending approval, else closes
suggestions, else interrupts a running turn. y/a/n decide approvals. Up/Down
recall sent inputs (last 200) on an empty or recalled one-line composer.
PageUp/PageDown page the transcript; Home/End jump when the composer is
empty; a scrolled view stays put and shows "new output below". Shift+Tab
flips the approval mode. Ctrl+C interrupts a running turn; a second press
within two seconds, or Ctrl+C when idle, quits. On exit the app prints the
session id and the `modron-app-tui --session` command to resume it. The
help screen (F1, `?`, `/help`) has three tabs, General, Commands and
Shortcuts (the old terminal's tabs). Tab and Shift+Tab, or Left and Right,
switch tabs; Up and Down, PgUp and PgDn, Home and End scroll (the wheel on
the web); Esc closes. Shortcut rows come from the key table in `src/help.rs`,
command rows from the shared slash table. `/reasoning` shows the live
reasoning tail.

Credentials (`/auth`, or the Settings auth row): the provider rows from the
core, label and status verbatim. Enter on an OAuth-capable provider starts
the server-side browser sign-in and shows its progress (stage, the URL, the
device code). Enter on a key-only provider opens a masked input; Enter saves
the key through `auth_set_key` and closes the entry, Esc closes it unsent.
The keys are never logged (`SecretKey` prints redacted) or kept. On the first
snapshot after connect, the screen opens once when no provider is signed in.
Sign-in needs a session on the server: with none attached, the call is refused.
`d` (or the web `remove` control beside a signed-in row) asks `remove <label>
credentials? y / n` and then sends `auth_remove`; a row that is not signed in
has no remove. The refreshed rows and the server's note (a refusal for a sign-in
that another tool owns, for example) arrive through the snapshot, not by a local
change.

Task commands: `/review`, `/btw <text>`, `/learn [title]` and `/plan <task>`
submit the canned task text the old terminal sent (shared in
`mobile/rust-core` `view.rs`); `/plan <task>` switches to the plan phase and
submits. `/goal <text>` sets the stop condition; `/goal` alone clears it.
`/agent <name>` sets the persona. `/exit`, `/quit` and `/q` leave the
terminal app.

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

## Status line, first screen and theme

- Context: the gauge reads `ctx 85%! (153k/180k)` from 80 percent up (the
  old TUI's `!` marker, `CONTEXT_WARNING_PCT` and `CONTEXT_WARNING_MARK` in
  `mobile/rust-core/src/view.rs`). The marker stays while the gauge is at or
  above 80 percent; it is not a once-only note. The old TUI suggests no
  `/compact`, so none is suggested.
- Header while a turn runs: a spinner frame (the old TUI's calculus spinner)
  and `esc to interrupt`. Idle with an empty composer: a hint that rotates
  every eight animation steps (the old TUI's eight hints, same words). Typing
  hides the hint. The steps come from `Host::tick_now` (`src/chat.rs`), which
  the web frame and the terminal loop call; the step period is `ANIM_PERIOD`
  (120 ms). The spinner stops when idle.
- A blank idle session shows the welcome banner (`welcome_lines`); the first
  row replaces it.
- Theme: `/theme`, or the Settings theme row, opens a picker with oculus dark,
  oculus light and system (`src/theme.rs`; the names are in the core's
  `view.rs`). Dark and light apply at once through NSH's `set_color_scheme`:
  the Oculus tokens carry both values, and the root paints the `background`
  token, so the whole page follows. "system" follows the terminal's or the
  browser's appearance. The choice is saved only when picked: the terminal
  writes `theme = "..."` to `$XDG_CONFIG_HOME/modron/app.toml` (or
  `~/.config/modron/app.toml`), the web to localStorage key `modron.theme`.
  A missing or unknown saved value means system. Tests inject no store, so
  they touch no config.
- The terminal loop wakes at the animation period while a turn runs
  (`chat::tick_deadline`), so the spinner moves during silent waits; an idle
  app has no deadline.
