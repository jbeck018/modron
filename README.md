# Modron

A terminal-first AI coding agent in Rust, built on one thesis: **the model decides
how smart the agent is; the harness decides how fast, safe, and expensive that
intelligence is to use.** Read the founding document in [PLAN.md](PLAN.md); the live
work ledger is [LEDGER.md](LEDGER.md).

Status: **v0.1.0 — release train, ACP permissions, MCP inspection, bench** — one-shot CLI + `modron chat` REPL with
live streaming answers, interactive approvals (Ask prompts on your terminal), agent
engine loop, Anthropic + OpenAI-compatible + **Z.ai** providers (SSE streaming
proven at the wire level), 6 essential tools behind an approval gate + optional
macOS Seatbelt sandbox, lifecycle hooks (configurable in config.toml), file
checkpoints with `modron rewind`, USD spend caps, persistent workspace memory,
token-efficient autocompaction, JSONL session trees, heuristic model router, and a
keyless mock-provider end-to-end path.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/jbeck018/modron/main/install.sh | bash
# or from source:
cargo install --locked --git https://github.com/jbeck018/modron
```

## Build

```sh
cargo build --release
```

## Quickstart (no API key needed)

Run the agent against a scripted mock provider — this exercises the full loop
(engine → approval gate → tools → session store → budget → usage table) offline:

```sh
cargo run -p modron-cli -- "list the rust files here and summarize" --provider mock
```

You get the assistant's answer on stdout, then a usage table with the estimated
cost and cache-hit rate, and a session transcript on disk. Progress (turn starts,
tool calls, compactions) streams to stderr; add `--verbose` for debug detail.

Mutating tools (`bash`/`write`/`edit`) are **blocked by default** (Ask mode) —
with a terminal attached, Ask **prompts** (`y` once / `a` for the rest of the run);
without one it blocks with an explanation, and `--approve-all` overrides either way.

### Interactive chat

```sh
cargo run -p modron-cli -- chat --provider mock        # or any live provider
```

One session, many turns: each line is a task, answers stream as they arrive
(provider-reported deltas), and the transcript (compaction + memory included)
carries context across lines. `/exit` or Ctrl-D quits; the session id prints for
`--resume`. Per-exchange and cumulative token counts appear after every answer.

### The TUI (`modron tui`)

Built on the lessons the field already paid for (see LEDGER Phase 1d): like Claude
Code, it renders **inline** — completed answers and tool lines are committed to
your native scrollback via `insert_before`, so everything stays selectable and
terminal-scrollable — while only the live region (status, streaming tail, input,
approval prompt) is the managed viewport. No alternate screen, no redraw churn.

- **Esc** interrupts the running turn (partial work stays persisted and resumable)
- **y/n/a** answers inline approval prompts; `a` approves for the rest of the run
- **ctrl+o** toggles transcript detail (compact vs full tool output)
- **ctrl+c** aborts then quits; the status line shows tokens and the live tool

## Live usage

**Anthropic** ([get a key](https://console.anthropic.com)):

```sh
export ANTHROPIC_API_KEY=sk-ant-…
cargo run -p modron-cli -- "explain what this repo does" --model claude-haiku-4-5
```

**Any OpenAI-compatible endpoint** (OpenAI, OpenRouter, DeepSeek, …):

```sh
export OPENAI_API_KEY=sk-…
cargo run -p modron-cli -- "explain what this repo does" \
    --provider openai-compat --model gpt-5-mini
# Optional: export OPENAI_BASE_URL=https://openrouter.ai/api/v1
```

**Ollama (local, keyless)** — a custom `OPENAI_BASE_URL` needs no API key:

```sh
export OPENAI_BASE_URL=http://localhost:11434/v1
cargo run -p modron-cli -- "summarize this directory" \
    --provider openai-compat --model llama3.1
```

**Z.ai (GLM)** — first-class, OpenAI-compatible at `https://api.z.ai/api/paas/v4`:

```sh
export ZAI_API_KEY=…            # pay-as-you-go key: https://z.ai/manage-apikey/apikey-list
cargo run -p modron-cli -- "list the rust files here and summarize" \
    --provider zai --model glm-5.3-flash
```

Models: `glm-5.3` (flagship), `glm-5.2`, `glm-5.3-flash` (cheap/fast),
`glm-5.3-flashx`. China mainland mirror: `export ZAI_BASE_URL=https://open.bigmodel.cn/api/paas/v4`.

**Coding Plan caveat:** GLM Coding Plan subscription keys are NOT interchangeable
with standard API keys — they only work on `https://api.z.ai/api/coding/paas/v4`
(OpenAI-compatible) / `https://api.z.ai/api/anthropic`, and get 401 on the
pay-as-you-go endpoint above. Coding Plan holders:

```sh
export ZAI_API_KEY=<coding-plan-key>
export ZAI_BASE_URL=https://api.z.ai/api/coding/paas/v4
cargo run -p modron-cli -- "task" --provider zai --model glm-5.3
```

## Useful flags

| Flag | Meaning | Default |
|---|---|---|
| `--provider` | `anthropic` \| `openai-compat` \| `zai` \| `mock` | config `default_provider`, else `anthropic` |
| `--model` | model name sent to the provider | config `default_model`, else `claude-sonnet-4-5` |
| `--approve-all` | let `bash`/`write`/`edit` execute (Ask mode blocks them otherwise) | off |
| `--sandbox seatbelt` | macOS sandbox: network denied, writes fenced to workspace + temp | off |
| `--max-cost <usd>` | USD spend cap for the run (catalog-priced, per-turn enforced) | unlimited |
| `--resume <uuid>` | continue a previous session (same session id, honors rewind points) | — |
| `--max-turns` | max model turns for the run | 40 |
| `--max-input-tokens` | per-turn assembled-context cap | 180000 |
| `--max-total-tokens` | cumulative token cap for the run | 2000000 |
| `--compact-threshold <0..1>` | autocompaction trigger fraction | 0.8 |
| `--no-compact` | disable autocompaction for this run | compaction on |
| `--working-dir` | directory the tools run in | `.` |
| `--sessions-dir` | session store location | `~/.modron/sessions` |
| `--verbose` | debug tracing to stderr | info-level progress only |

## Subcommands

```sh
modron tui                      # inline-viewport TUI: streaming, tool lines,
                                 #   esc-to-interrupt, inline y/n/a approvals
modron chat                     # interactive multi-turn session (streaming)
modron router eval                   # score the model router offline
modron usage                    # aggregate token/cost/cache-hit dashboard
modron sessions                 # list stored sessions with tokens + est. cost
modron rewind                   # undo the last checkpointed file mutation +
                                 #   mark the transcript for resume-from-truncation
modron memory list|show|forget  # inspect/forget the workspace's persistent memory
```

## Memory, caching, compaction

- **Memory (two tiers):** repo-level `AGENTS.md`/`CLAUDE.md` is loaded into the
  system prompt, and the agent can persist durable facts via the `remember` tool —
  every later session in the same workspace sees them. Memory is inspectable and
  forgettable (`modron memory …`); nothing is silent.
- **Prompt caching:** Anthropic requests carry explicit `cache_control` breakpoints
  on the stable prefix (system + tools), the prefix is byte-stable across turns
  (asserted by tests), and the usage table shows the provider-reported cache-hit rate.
- **Autocompaction:** under context pressure the engine summarizes older history
  with the cheap model and continues from `[compact state] + recent tail`. The
  measured gate: post-compaction input ≤ 1.5× a fresh session's for the same task
  (asserted in tests). Compaction usage counts against the budget governor.

## Configuration

Optional config file at `~/.config/modron/config.toml` (environment variables
override it; empty env values count as unset):

```toml
default_provider = "openai-compat"   # or MODRON_DEFAULT_PROVIDER
default_model = "gpt-5-mini"         # or MODRON_DEFAULT_MODEL
max_input_tokens = 180_000           # or MODRON_MAX_INPUT_TOKENS
max_total_tokens = 2_000_000         # or MODRON_MAX_TOTAL_TOKENS

[hooks]                              # run around every tool call (MODRON_TOOL_* env)
pre_tool = "echo $MODRON_TOOL_NAME" # non-zero exit vetoes the tool call
post_tool = ""                       # observes; never blocks

[compaction]                         # autocompaction tuning (flags override)
threshold_frac = 0.7

[verify]                             # registers the `verify` tool: the agent runs
command = "cargo test --quiet"       #   your checks after edits and fixes failures

# Agent personas (Kilo-style custom modes): tailored prompt, forced model,
# tool permissions. Select with --agent debug.
[agents.debug]
system = "You are a debugging specialist. Reproduce first, then fix, then verify."
model = "glm-5.3"
deny = ["write", "edit"]

# Personas are also available to the agent itself via the `delegate` tool:
# large tasks become isolated subtasks run under a persona's prompt, model,
# and permissions — with the child's token spend folded into the parent's
# budget and usage reporting.

# Skills (SKILL.md open standard): drop a folder with SKILL.md into
# .claude/skills/<name>/ (or .modron/skills/<name>/) — the index appears in
# the agent's system prompt and it loads bodies via the `skill` tool.
# Repo-wide questions can also go through the `explore` subagent tool
# (read-only, cheap tier, its own token budget).

[router]                             # model routing (learned by default)
mode = "learned"                     # or "heuristic" (rules only)
cheap_model = "glm-5.3-flash"        # tier models (defaults: haiku/sonnet)
strong_model = "glm-5.3"
[router.actions]                     # per-action overrides win over tiers
plan = "glm-5.3"

# Optional: an LLM routing judge — any OpenAI-compatible provider (locally,
# e.g. Ollama with a 0.5B model) classifies task complexity before routing.
[judge]
mode = "ambiguous"                   # or "always"
provider = "openai-compat"           # + OPENAI_BASE_URL=http://localhost:11434/v1
model = "qwen2.5:0.5b"

# Optional: a Kev / TypeSafe Jev decision-model server (wire-compatible).
[kev]
endpoint = "http://127.0.0.1:8009"
# api_key = "…"

# Inspect any configured MCP server without running the agent:
#   modron mcp info <name>       # tools + schema costs, resources, prompts
# Measure the harness on a fixed 6-task suite:
#   modron bench --provider mock # (real baselines with a live provider)

# Optional: MCP servers (stdio). Tools appear as mcp__<server>__<tool>;
# each server's schema cost is counted and capped (default 20k tokens).
[mcp.servers.echo]
command = "modron-mcp-echo"

[mcp.servers.context7]
command = "npx"
args = ["-y", "@upstash/context7-mcp"]
```

Provider API keys are read from the environment: `ANTHROPIC_API_KEY`
(+ optional `ANTHROPIC_BASE_URL`), `OPENAI_API_KEY` (+ optional `OPENAI_BASE_URL`;
with a custom base URL the key may be omitted, which is what makes Ollama keyless),
`ZAI_API_KEY` (+ optional `ZAI_BASE_URL`).

Sessions are append-only JSONL transcripts — one `{uuid}.jsonl` plus a `.meta.json`
per session under `~/.modron/sessions/` (override with `--sessions-dir`); a
`{uuid}.rewind` marker stores the truncation point a rewind created. Workspace
memory lives under `~/.modron/memory/<workspace-slug>/`.

## Tests

```sh
cargo test --workspace                # everything, fully offline
cargo clippy --workspace --all-targets -- -D warnings
```

The **model router is learned**: a hashing-vectorizer over character n-grams
scores each task against corpus-trained centroids (`[router]` corpus lives in
`modron-router/src/corpus.toml` — edit and rerun `modron router eval` to
retune), falling back to rules when unsure. Routing decisions are logged with
their features for future training. The **prompt budget is a CI gate** (`crates/modron-cli/tests/budget.rs`): the
system prompt + tool specs must stay under 2048 estimated tokens with the real
registry — changes that fatten prompts fail the build (PLAN.md §3.1).

Acceptance suites live in `crates/modron-cli/tests/`: `e2e.rs` (Phase 0 — mock
provider drives the real engine + tools + store, binary subprocess run/resume),
`phase1a.rs` (memory persistence, compaction under pressure with the ≤1.5×
cheapness gate), and `phase1b.rs` (approval gate, Seatbelt network-deny, hook
veto, checkpoint rewind, Z.ai factory). No network is required anywhere; live-API
smoke is documented above but (by design) unexecuted until keys exist.

## Layout

```
crates/modron-core        traits, agent engine, budget + USD caps, approval/sandbox/hook
                           pipeline, checkpoints + rewind, autocompaction, memory, mock provider
crates/modron-providers   Anthropic Messages + OpenAI-compatible clients (SSE streaming),
                           retry/backoff, Z.ai factory
crates/modron-tools       read / glob / grep / bash / write / edit / remember + output
                           reduction + registry
crates/modron-store       JSONL session store (append-only, corrupt-line tolerant)
crates/modron-router      heuristic model selector + embedded model catalog (pricing)
crates/modron-cli         the one-shot binary, config, subcommands
```

Roadmap and architecture: [PLAN.md](PLAN.md). Phase-by-phase status:
[LEDGER.md](LEDGER.md).
