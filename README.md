# Modron

A terminal-first AI coding agent in Rust, built on one thesis: **the model decides
how smart the agent is; the harness decides how fast, safe, and expensive that
intelligence is to use.** Read the founding document in [PLAN.md](PLAN.md); the live
work ledger is [LEDGER.md](LEDGER.md); the configuration reference is
[docs/CONFIG.md](docs/CONFIG.md).

Status: version 0.1.3 (`[workspace.package]` in Cargo.toml). One `modron` binary
provides one-shot runs, a multi-turn REPL (`modron chat`), the terminal app
(`modron tui`, also the default when you run `modron` alone in a terminal; it starts `modron-app-tui`), a
remote-control server (`modron serve`), and an editor protocol server (`modron acp`).
Work in progress on the clients and remote access is tracked in LEDGER.md, Program U.

## What is new

One line per feature. The detail is in the linked document.

- **Steer and queue during a run.** A message sent to a running session is injected at
  the next safe point, between model turns. A queued message starts as the next run
  after a successful finish. Protocol frames `steer`, `enqueue` and `dequeue`; the NSH
  app uses them, the old terminal UI never did. See [docs/PROTOCOL_V2.md](docs/PROTOCOL_V2.md) section 2.
- **Plan phase.** In the `plan` phase every gated tool is refused and the model records
  a plan with the `plan` tool. A client then approves it (the run switches to build and
  executes the plan) or asks for a revision. The `/plan` command only adds a plan-first
  instruction to the prompt; it does not set the phase. See PROTOCOL_V2.md section 3.
- **Sub-agents (`delegate`).** A subtask runs as a child with its own transcript, an
  optional persona, and optional `background` and `isolate` modes. Several delegate calls
  in one turn can run concurrently. A child's approval requests go to the parent's approver
  while the parent run is live. Clients can steer or kill one live child. Details:
  PROTOCOL_V2.md section 4.
- **Reviewer pass and claim checks.** A run that finishes with a diff gets one tool-less
  review of that diff on `review_model` (or the `critic` role). Findings can be fed back for
  up to `review_max_rounds` rounds. Non-LLM claim checks run on the final answer.
  See CONFIG.md.
- **Permission rules.** `[permissions]` lists `allow`, `ask` and `deny` rules per tool and
  path or command pattern. Deny beats ask beats allow. See CONFIG.md.
- **Lifecycle hooks.** `[hooks]` runs shell commands before and after each tool call, and
  at the start of a run and after each turn. `pre_tool` can veto a call. See CONFIG.md.
- **Reasoning effort.** `effort` in config or `--effort` (`low`, `medium`, `high`, `xhigh`,
  `max`). It is not yet checked against a live provider. See CONFIG.md.
- **`edit_lines` and `webfetch` tools.** Both are on demand (found with `tool_search`) and
  approval-gated. `edit_lines` replaces a line range only if every line still matches the
  hash that `read` returned with `line_hashes`. `webfetch` fetches one public http(s) URL,
  refuses private and loopback addresses, follows no redirects, and refuses to run under a
  sandbox.
- **MCP over HTTP.** `[mcp.servers.<name>]` accepts a `url` (Streamable HTTP) with header
  templates `${NAME}`, alongside stdio `command` servers. See CONFIG.md.
- **Wake and notify (protocol).** A session holds at most one scheduled wake (one-shot or
  recurring) through the `wake` and `cancel_wake` frames. The server sends `notify` frames
  for done, failed, plan, wake and approval requests. Push delivery to phones is not built.
  See PROTOCOL_V2.md section 6.
- **Compaction and the context gauge.** Autocompaction summarises older history when the
  context nears its window. The `compact` frame compacts an idle session on demand, and the
  server reports the context size for the status display. The gauge is empty until the first
  run or compaction in a server process.
- **Named model roles.** `[roles]` picks models for the plan phase, sub-agents, the critic
  reviewer and compaction summaries. See CONFIG.md.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/jbeck018/modron/main/install.sh | bash
# or from source:
cargo install --locked --git https://github.com/jbeck018/modron
```

## First run & auth

With no provider configured, the first launch opens a setup screen instead of
erroring. Before asking for a key it checks for sign-ins other agent CLIs
already hold on this machine and offers one-press reuse:

- **Codex CLI**: ChatGPT tokens in `~/.codex/auth.json`
- **Claude Code**: `~/.claude/.credentials.json`, else the macOS keychain

Enter reuses the first detected sign-in; `s` falls through to the manual
provider list. The source tool keeps its copy. If it refreshes its tokens
first, re-run onboarding to re-import.

`modron auth` manages credentials any time: `auth add` (interactive wizard),
`auth login` (Claude subscription OAuth in the browser), `auth set <provider>
[key]` (omit the key or pass `-` and it is read from stdin, so piped secrets
never touch shell history or `ps`), `auth status` (masked), and `auth logout
[provider]`. Saved keys live in `~/.config/modron/env` (mode 0600), sourced on
every launch. The `/auth` command opens the same manager mid-session.

## Daemon (persistent sessions)

```sh
modron daemon start --provider zai --model glm-5.3-flash
modron daemon status      # running? socket path?
modron daemon stop
```

The daemon runs the engine on a Unix socket (`~/.modron/daemon.sock`), so
sessions survive terminal close. RPC is newline-delimited JSON with the methods
`ping`, `send_task`, `resume_task`, `list_sessions` and `stop`.

## Diagnostics

```sh
modron doctor    # check keys, config, git, PDF support, storage, terminal
```

## Attachments (multimodal)

```sh
modron "what's in this image?" --attach screenshot.png
modron "summarize the spec" --attach spec.pdf --attach notes.md
```

Images (png/jpg/gif/webp) are sent as image content. Text and PDF files are
inlined as documents. PDF extraction uses `pdftotext` (`brew install poppler`).
`--attach` is not yet supported together with `--goal`.

## Upgrade

```sh
modron upgrade
```

Checks the latest GitHub release, downloads the platform archive and replaces
the binary. The previous binary is kept as `modron.bak`. When `modron-app-tui` is
installed next to `modron` and the archive carries it, it is replaced the same way
(kept as `modron-app-tui.bak`); older archives do not carry it and leave it alone.
The archive is checked against the release's `SHA256SUMS` (SHA-256) before anything
is extracted or replaced; a missing manifest, a missing entry, a malformed manifest or
a digest mismatch aborts the upgrade and leaves the installed binaries untouched.
This catches a corrupted or truncated download and an archive replaced without the
manifest changing. It does NOT protect against an attacker who can replace both the
archive and `SHA256SUMS` in the release; that needs signed releases, which this project
does not have yet. Downgrades are refused, and redirects are followed only to https.
`modron chat` shows
an upgrade notice when a newer release exists. The check runs at most once per
24h and is cached in `~/.modron/update-check.json`.

## Release archive and install size

Each release archive (`modron-<arch>-<os>.tar.gz`) holds `modron` and
`modron-app-tui`; `SHA256SUMS` covers the archives. `install.sh` places both in
`~/.local/bin` when the archive has the app. The terminal app is much larger than
`modron`: the spike measured a stripped, size-optimised build at 17,081,600 bytes
(about 17 MB) before any feature trimming (`spikes/nsh-tui/RESULTS.md`, Follow-up
S2c). That is a measurement, not a promise for the shipped build.

## Build

```sh
cargo build --release
```

## Quickstart (no API key needed)

Run the agent against a scripted mock provider. This exercises the full loop
(engine, approval gate, tools, session store, budget, usage table) offline:

```sh
cargo run -p modron-cli -- "list the rust files here and summarize" --provider mock
```

You get the assistant's answer on stdout, then a usage table with the estimated
cost and cache-hit rate, and a session transcript on disk. Progress (turn starts,
tool calls, compactions) streams to stderr. Add `--verbose` for debug detail.

Mutating tools (`bash`, `write`, `edit`, `edit_lines`) are **blocked by default**
(Ask mode). With a terminal attached, Ask **prompts** (`y` once, `a` for the rest
of the run, `n` to refuse). Without one it blocks with an explanation.
`--approve-all` (alias `--yolo`) lets them run without asking.

### Interactive chat

```sh
cargo run -p modron-cli -- chat --provider mock        # or any live provider
```

One session, many turns: each line is a task, answers stream as they arrive,
and the transcript (compaction and memory included) carries context across
lines. `/exit` or Ctrl-D quits. The session id prints for `--resume`. A token
line with the exchange count and the cumulative total follows each answer.

### The terminal app (`modron tui`)

`modron tui`, and bare `modron` in a terminal, start the terminal target of the NSH
app (`modron-app-tui`; see `modron app` and [app/README.md](app/README.md) for its
flags, keys and server choice). It takes over the terminal and connects to a running
`modron serve`, or starts one. The old built-in terminal UI (`crates/modron-tui`) was
deleted on 2026-11-16; git history at commit `0ac5423` has it.

Old flags map to the app: `--resume` and `--connect` pick the session and the server;
`--yolo`, `--effort`, `--goal`, `--agent`, `--sandbox`, `--allow` and `--deny` reach the
app's config. Flags with no app equivalent (`--max-turns`, `--max-cost`, `--attach`,
compaction flags) are named on stderr and ignored.

The slash commands are shared with the app (`crates/modron-view/src/slash.rs`); `/help`
lists them. Transient notes appear in the status area and expire.

## Live usage

**Anthropic** ([get a key](https://console.anthropic.com)):

```sh
export ANTHROPIC_API_KEY=sk-ant-...
cargo run -p modron-cli -- "explain what this repo does" --model claude-haiku-5-5
```

**Any OpenAI-compatible endpoint** (OpenAI, OpenRouter, DeepSeek, ...):

```sh
export OPENAI_API_KEY=sk-...
cargo run -p modron-cli -- "explain what this repo does" \
    --provider openai-compat --model gpt-5-mini
# Optional: export OPENAI_BASE_URL=https://openrouter.ai/api/v1
```

**Ollama (local, keyless)**: a custom `OPENAI_BASE_URL` needs no API key:

```sh
export OPENAI_BASE_URL=http://localhost:11434/v1
cargo run -p modron-cli -- "summarize this directory" \
    --provider openai-compat --model llama3.1
```

**Z.ai (GLM)**: first-class, OpenAI-compatible at `https://api.z.ai/api/paas/v4`:

```sh
export ZAI_API_KEY=...          # pay-as-you-go key: https://z.ai/manage-apikey/apikey-list
cargo run -p modron-cli -- "list the rust files here and summarize" \
    --provider zai --model glm-5.3-flash
```

The built-in catalog includes `glm-5.3`, `glm-5.2` and `glm-5.3-flash`. For the
China mainland mirror, set `ZAI_BASE_URL=https://open.bigmodel.cn/api/paas/v4`.

**Coding Plan caveat:** GLM Coding Plan subscription keys are NOT interchangeable
with standard API keys. They work only on `https://api.z.ai/api/coding/paas/v4`
(OpenAI-compatible) and `https://api.z.ai/api/anthropic`, and they get 401 on the
pay-as-you-go endpoint above. Coding Plan holders:

```sh
export ZAI_API_KEY=<coding-plan-key>
export ZAI_BASE_URL=https://api.z.ai/api/coding/paas/v4
cargo run -p modron-cli -- "task" --provider zai --model glm-5.3
```

Other providers (Groq, Google, Cerebras, Mistral, NVIDIA, OpenRouter, GitHub
Models) are configured by their key variable and base URL. The full table is in
[docs/CONFIG.md](docs/CONFIG.md#provider-credentials-and-endpoints).

## Useful flags

Flags apply to every subcommand (they are global). Config keys noted here are
described in [docs/CONFIG.md](docs/CONFIG.md).

| Flag | Meaning | Default |
|---|---|---|
| `--provider` | `anthropic`, `openai-compat`, `chatgpt`, `zai` or `mock` | config `default_provider`, else `anthropic` |
| `--model` | model name sent to the provider. Pins every turn only together with `--provider`; otherwise the router picks models per turn | config `default_model`; see CONFIG.md |
| `--effort` | reasoning effort: `low`, `medium`, `high`, `xhigh`, `max` | config `effort`, else provider default |
| `--approve-all` / `--yolo` | let `bash`, `write`, `edit` and the other gated tools run without asking | off (Ask mode) |
| `--allow <tool>` / `--deny <tool>` | per-run tool permissions, repeatable. Deny is checked first | none |
| `--agent <name>` | run as a persona from `[agents.*]` (prompt, model, tool lists) | none |
| `--goal "<condition>"` | keep iterating in one session until the model reports the goal met or a cap trips. Iteration cap from `MODRON_GOAL_MAX_ITERATIONS` | cap 10 |
| `--sandbox <off\|seatbelt\|bubblewrap>` | OS sandbox for mutating tool subprocesses. `seatbelt` is macOS, `bubblewrap` is Linux | `off` |
| `--max-cost <usd>` | USD spend cap for the run, enforced per turn from catalog prices | unlimited |
| `--resume[=<uuid>]` | continue a session. Bare `--resume` picks the most recent one | none |
| `--connect <target>` | drive a running `modron serve` instead of an in-process engine: `unix`, `unix:<path>` or `ws://host:port` | in-process |
| `--max-turns` | max model turns for the run | 40 |
| `--max-input-tokens` | per-turn assembled-context cap | 180000 |
| `--max-total-tokens` | cumulative token cap for the run | 2000000 |
| `--compact-threshold <0..1>` | autocompaction trigger fraction | 0.8 |
| `--no-compact` | disable autocompaction for this run | compaction on |
| `--working-dir` | directory the tools run in | `.` |
| `--sessions-dir` | session store location | `~/.modron/sessions` |
| `--attach <file>` | attach a file to the task, repeatable | none |
| `--verbose` | debug tracing to stderr | info-level progress only |

## Subcommands

```sh
modron tui                      # terminal app, modron-app-tui (bare `modron` on a terminal does the same)
modron chat                     # interactive multi-turn session
modron serve [--listen ADDR] [--bridge URL] [--require-token] [--allow-origin ORIGIN]
                                # remote-control server; see "Remote access"
modron pair [NAME]              # mint a device token; `pair list`, `pair revoke NAME`
modron acp                      # Agent Client Protocol server over stdio (editors)
modron auth add|login|set|status|logout
                                # provider credentials (API keys + OAuth);
                                #   auth set <provider> reads the key from
                                #   stdin when the key arg is omitted or '-'
modron mcp info <name>          # tools with schema costs, resources, prompts
modron mcp read <name> <uri>    # read one resource from a configured server
modron eval [--models a,b] [--filter TEXT]
                                # mini-swe evals: verifier-scored task suite
modron router eval              # score the model router offline
modron bench [--models a,b] [--list] [--delay-ms N] [--json PATH]
                                # compare models on the fixed task suite
modron usage [--last N]         # aggregate token/cost/cache-hit dashboard
modron sessions [list]          # stored sessions with tokens + est. cost
modron sessions vacuum --older-than-days N [--dry-run]
                                  # delete stale sessions ([sessions] ttl_days in config)
modron sessions restart <id>    # same session id, fresh context
modron traces <session>         # per-turn TurnTrace rows for one session
modron rewind [--session ID]    # undo the last checkpointed file mutation
modron memory list|show|forget|stats|curate
                                # inspect and forget the workspace's persistent memory
modron daemon start|status|stop # background engine (see Daemon)
modron doctor                   # diagnose the setup
modron upgrade                  # replace the binary with the latest release
```

## Memory, caching, compaction

- **Memory (two tiers):** the workspace's `AGENTS.md` is loaded into the system prompt,
  or `CLAUDE.md` when there is no `AGENTS.md`. The agent can persist durable facts with the
  `remember` tool (available on demand), and every later session in the same workspace
  sees them. Memory is inspectable and forgettable (`modron memory ...`), so nothing is silent.
- **Prompt caching:** Anthropic requests carry explicit `cache_control` breakpoints on the
  stable prefix (system and tools). The prefix is byte-stable across turns (asserted by tests),
  and the usage table shows the provider-reported cache-hit rate.
- **Autocompaction:** when the context grows past `threshold_frac` of the window, the engine
  summarises older history and continues from the summary plus the recent tail. Compaction
  usage counts against the budget. The gate in tests: post-compaction input stays within 1.5x
  of a fresh session's for the same task.

## Reliability, observability, tool tiering

- **Finish gate:** a run that mutated files does not just stop. With a `[verify]` command
  configured, the engine re-runs it and feeds failures back, for at most two rounds. With no
  verify command, one requirements-checklist turn is forced: the model re-reads the original
  task and checks each concrete detail it names (paths, exact names, ports, config formats)
  against the environment before ending.
- **TurnTrace:** every model turn appends an observability record to the session JSONL: harness
  version, provider and model, a sha256 over the prompt prefix (cache-stability auditing),
  latency, cost, usage, the finish gate's verdict, and a stop label (`end_turn`, `max_turns`,
  `spend_cap`, `budget_exhausted`). Read them with `modron traces <session>`.
- **Patient retries:** provider transport errors (429, 5xx, timeouts, connection errors) are
  attempted up to 5 times in total, with exponential backoff (500ms base, factor 4).
- **Empty-stream replay:** a stream that dies before emitting anything observable is replayed
  once. Once deltas have reached the terminal there is no replay, so partial output is never
  duplicated.
- **Tool tiering:** 14 essential tools stay in the request schema: `read`, `write`, `edit`,
  `bash`, `grep`, `glob`, `tool_search`, `skill`, `memory_search`, `diagnostics`, `verify`,
  `interact`, `delegate` and `plan`. The rest stay callable but off-prompt, and are found with
  `tool_search`. These include `remember`, `read_minified`, `edit_lines`, `webfetch`, `explore`,
  `delegate_status`, `merge_worktree`, and MCP tools.
- **`interact` tool:** a persistent tmux session (`start` / `send` / `capture`) for screen
  programs that bash cannot hold open, such as vim, ssh, and REPLs.

## Configuration

The optional config file is `~/.config/modron/config.toml`. Environment variables
override four of its keys, and empty values count as unset. A minimal example:

```toml
default_provider = "openai-compat"   # or MODRON_DEFAULT_PROVIDER
default_model = "gpt-5-mini"         # or MODRON_DEFAULT_MODEL
max_input_tokens = 180_000           # or MODRON_MAX_INPUT_TOKENS
max_total_tokens = 2_000_000         # or MODRON_MAX_TOTAL_TOKENS
effort = "medium"                    # low | medium | high | xhigh | max

[verify]
command = "cargo test --quiet"       # the agent runs it; the finish gate re-runs it

[permissions]
allow = ["bash(cargo test*)"]
ask   = ["bash(git push*)"]
deny  = ["read(.env*)"]

[hooks]
pre_tool = "echo $MODRON_TOOL_NAME"  # non-zero exit vetoes the tool call

[agents.debug]                       # select with --agent debug
system = "You are a debugging specialist. Reproduce first, then fix, then verify."
model = "glm-5.3"
deny = ["write", "edit"]

[mcp.servers.echo]
command = "modron-mcp-echo"
```

Every key, its default and its effect are listed in
[docs/CONFIG.md](docs/CONFIG.md). That file also covers the environment variables the
binary reads. Provider keys are read from the environment: `ANTHROPIC_API_KEY` (plus
`ANTHROPIC_BASE_URL`), `OPENAI_API_KEY` (plus `OPENAI_BASE_URL`; with a custom base URL the
key may be omitted, which is what makes Ollama keyless), and `ZAI_API_KEY` (plus
`ZAI_BASE_URL`).

Sessions are append-only JSONL transcripts: one `{uuid}.jsonl` plus a `.meta.json` per
session under `~/.modron/sessions/` (override with `--sessions-dir`). A `{uuid}.rewind`
marker stores the truncation point that a rewind created. Workspace memory lives under
`~/.modron/memory/<workspace-slug>/`.

## Clients

The terminal, web and mobile clients are views over the same session state. The
protocol they share is in [docs/PROTOCOL_V2.md](docs/PROTOCOL_V2.md).

- **NSH app (`app/`).** The one interactive client, with two targets: a terminal binary
  (`modron-app-tui`, started by `modron tui`, bare `modron` on a terminal, or `modron app`)
  and a web bundle. Both connect to a running `modron serve`, share their logic with
  `crates/modron-view` and `mobile/rust-core`, and follow the parity contract in
  AGENTS.md. `mobile/app` (TSX) is frozen. Open items are listed in LEDGER.md.

  Run it with a server up:

  ```sh
  modron serve --listen 127.0.0.1:7000          # loopback clients need no token
  cd app && cargo run --bin modron-app-tui -- --url ws://127.0.0.1:7000 [--token T] [--session UUID]
  ```

  `modron app` starts the terminal app from an installed `modron`: it finds
  `modron-app-tui` through `MODRON_APP_BIN`, then next to the `modron` binary, then on
  `PATH`, sets `MODRON_BIN` to that `modron` so the app starts this server, and passes
  every later argument through unchanged (`modron app --model x -- --demo`). Without
  the app binary it exits with code 2 and says how to get it. Plain `modron` still
  starts the built-in UI.

  In the terminal app, Enter sends (steers while a run is active), Alt+Enter queues, Esc
  interrupts, y/a/n decide approvals, and `/compact` compacts. The web bundle needs the NSH
  CLI (0.5.1), the wasm32 target and wasm-bindgen CLI 0.2.130:

  ```sh
  cd app && nsh package --target web -o dist    # writes dist/Modron/
  ```

  Serve `dist/Modron/` with any static server. `index.html` with no query runs the built-in
  demo. `index.html?url=ws%3A%2F%2Fhost%3A7000&token=T&session=UUID` runs live against a
  server. The real server refuses browser origins unless it was started with
  `--allow-origin <origin>`. See [app/README.md](app/README.md).
- **Desktop app:** removed. Its brand assets live in `app/assets/`, and the design tokens are
  in `app/theme/oculus.json`.
- **Mobile (`mobile/app`):** frozen. It is a TypeScript client over the shared core in
  `mobile/rust-core`. See [mobile/SPIKE.md](mobile/SPIKE.md) for the earlier spike.

## Remote access

- **`modron serve`** runs the session server. It listens on a WebSocket at `--listen`
  (default `127.0.0.1:7699`; `disabled` keeps only the Unix socket, `~/.modron/serve.sock`).
  Sessions live in the server, so they keep running with no client attached.
- **Who may connect.** Loopback clients are trusted. Any other address needs a paired device
  token: `modron pair <name>` mints one, `modron pair list` lists them, and `modron pair revoke
  <name>` revokes one. `--require-token` asks for a token on every connection, even on loopback.
  Browser origins are refused unless named with `--allow-origin` (exact match, repeatable).
- **Drive a server from a client.** `modron --connect ws://host:port` (or `--connect unix`)
  starts the terminal app or chat against a running server, and `--resume` reattaches to a session.
- **Share a session.** In a client connected to a server, `/remote-control <bridge url>` opens a
  tunnel through a bridge and prints a share URL. Open it on any device. The share link is the
  credential for that one session: anyone who has it can watch and drive the session, so treat it
  as a secret. `/s/<key>` on the bridge redirects to the web app at `/app/`. `/s/<key>?classic=1`
  keeps the older inline viewer.
- **Share access.** The owner portal, which is opt-in, adds owner sign-in, invite links with a
  viewer or approver role, and a session list for the owner. With `OWNER_TOKEN` set, a bare share
  link stops opening a viewer socket, and guests need an invite or an owner sign-in.
  [docs/PORTAL.md](docs/PORTAL.md) is the design. Linked GitHub accounts and Web Push are not built
  yet (ledger C4e and C5).
- **Publishing pages.** `/share` (alias `/publish`) publishes the last answer as a page through the
  bridge. The server needs `--bridge <URL>` and the client needs `MODRON_PUBLISH_TOKEN`.
- **The bridge is yours.** The Cloudflare bridge is a Worker in [cloud/bridge](cloud/bridge). You
  deploy it into your own Cloudflare account with `npx wrangler deploy` after your own
  `wrangler login`. Nothing in this repository deploys on your behalf, and nothing is hosted for
  you. See [cloud/README.md](cloud/README.md) and [cloud/bridge/README.md](cloud/bridge/README.md).

## Benchmarks

`modron bench` compares models on a fixed task suite. `--list` prints the curated free-tier
model list. `modron eval` scores verifier-checked mini-swe tasks (see `evals/mini_swe.toml`).
`scripts/tb` adapts Terminal-Bench: a headless in-container agent adapter, a per-task scorecard
(`score.py`), and a baseline TSV regression gate. Setup and pilot recipe:
[scripts/tb/README.md](scripts/tb/README.md).

Free-tier providers are registered when their key is set (`GROQ_API_KEY`,
`GEMINI_API_KEY`, `CEREBRAS_API_KEY`, `MISTRAL_API_KEY`, `GITHUB_MODELS_TOKEN`,
`NVIDIA_API_KEY`, `OPENROUTER_API_KEY`; see [docs/CONFIG.md](docs/CONFIG.md)).
`modron bench --models a,b` names the models to compare.

## Tests

```sh
cargo test --workspace                # the Rust workspace, fully offline
cargo clippy --workspace --all-targets -- -D warnings
```

`app/` is its own Cargo workspace. From inside it, run `cargo test`, then
`cargo check --lib --target wasm32-unknown-unknown` for the web target, and
`node web-test.js` after `nsh package --target web` (see [app/README.md](app/README.md)).

The **model router is learned**: a hashing-vectorizer over character n-grams scores each task
against corpus-trained centroids (the corpus lives in `crates/modron-router/src/corpus.toml`;
edit it and rerun `modron router eval` to retune), falling back to rules when unsure. Routing
decisions are logged with their features for future training. The **prompt budget is a CI gate**
(`crates/modron-cli/tests/budget.rs`): the system prompt and tool specs must stay under 2048
estimated tokens with the real registry.

Acceptance suites live in `crates/modron-cli/tests/`: `e2e.rs` (mock provider drives the real
engine, tools and store, as a binary subprocess), `phase1a.rs` (memory persistence and compaction
under pressure), `phase1b.rs` (approval gate, Seatbelt network-deny, hook veto, checkpoint rewind)
and `phase1c.rs`, `phase2a.rs`, `phase3a.rs` and `acp_e2e.rs` for later phases. No network is
required for any of them.

The local remote-control chain (`modron serve` with the mock provider, the bridge under
`wrangler dev --local`, and the web app in headless Chromium) is in [e2e/](e2e) and is documented
in [e2e/README.md](e2e/README.md).

## Layout

```
crates/modron-core        agent engine, budget and USD caps, approval, permission rules,
                           hooks, sandbox, checkpoints, autocompaction, memory, auth,
                           sub-agents, reviewer, goal loop
crates/modron-types       session and transcript data types (split out of core so wasm
                           targets can use them)
crates/modron-protocol    remote-control wire frames (see docs/PROTOCOL_V2.md)
crates/modron-view        shared client view-models: tool activity grouping, transcript
                           replay, shared labels
crates/modron-providers   Anthropic Messages, OpenAI-compatible, ChatGPT Responses and
                           free-tier providers; retry and backoff
crates/modron-tools       built-in tools (read, write, edit, edit_lines, bash, grep, glob,
                           webfetch, interact, remember, memory_search, diagnostics, skill,
                           explore, delegate, plan, verify), output reduction, tiered registry
crates/modron-store       JSONL session store (append-only, corrupt-line tolerant)
crates/modron-router      model selection, learned and heuristic routers, model catalog (pricing)
crates/modron-mcp         MCP client over stdio and Streamable HTTP
crates/modron-server      remote-control server (modron serve): session hub, bridge tunnel,
                           wakes, publishing
crates/modron-cli         the modron binary: CLI, config, daemon, onboarding, subcommands

app/                      NSH client: terminal and web targets (see app/README.md)
mobile/                   frozen mobile client (app/ TSX) and the shared rust-core
cloud/                    Cloudflare bridge (Worker and Durable Objects), and the container
e2e/                      local end-to-end chain: serve, bridge, web app
spikes/                   measurements behind the client decisions (nsh-tui, nsh-web)
docs/                     PROTOCOL_V2.md, PORTAL.md, CONFIG.md, SECURITY_PLAN.md, remote-control.md
evals/                    mini-swe eval task suite
scripts/                  Terminal-Bench adapter (tb), dev helpers
packaging/                Homebrew formula
```

Roadmap and architecture: [PLAN.md](PLAN.md). Phase-by-phase status:
[LEDGER.md](LEDGER.md).
