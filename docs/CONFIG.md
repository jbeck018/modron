# Configuration reference

This page lists every key the `modron` binary reads from `config.toml`, the
environment variables it reads, and where each one takes effect. The README
covers command-line flags.

Sources of truth in the code:

- `crates/modron-cli/src/config.rs`: config keys and their types.
- `crates/modron-cli/src/main.rs`: precedence, defaults and flag wiring.
- `crates/modron-core/src/permissions.rs`: permission rule syntax.
- `crates/modron-core/src/hooks.rs`: hook commands and their environment.

## File location and precedence

- Location: `~/.config/modron/config.toml`.
- `MODRON_CONFIG` overrides the location. A value that ends in `.toml` or names
  an existing file is used as the file. Any other value is a directory that
  holds `config.toml`.
- Precedence, per key: command-line flag, then environment variable, then
  `config.toml`, then built-in default. Four keys also have environment
  overrides: `default_provider`, `default_model`, `max_input_tokens` and
  `max_total_tokens` (listed in the tables below). Other keys are file-only.
  An empty environment variable counts as unset.
- A missing file is normal. A malformed file is ignored as a whole, with a
  warning, so every setting in it falls back to its default. Unknown keys are
  ignored. A non-numeric value in a numeric environment variable is ignored
  with a warning.
- These values are checked at startup, and a bad value stops the run with an
  error that names it: `effort`, every `[permissions]` rule, `[router] mode`,
  `[judge] mode`, `[kev] mode`, `[compaction] threshold_frac` (must be greater
  than 0 and less than 1), and an `[mcp.servers.<name>]` entry that sets both or
  neither of `command` and `url`, or sets `headers` without `url`.
- An unknown key under `[router.actions]` is ignored with a warning.

## Top-level keys

| Key | Type | Default | Effect |
|---|---|---|---|
| `default_provider` | string | `anthropic` | Provider used when `--provider` is not given. Accepted ids: `anthropic`, `openai-compat`, `openai-responses`, `chatgpt`, `zai`, `mock`, and the free-tier ids in the environment section. Env: `MODRON_DEFAULT_PROVIDER`. |
| `default_model` | string | see note | Model used when `--model` is not given. Env: `MODRON_DEFAULT_MODEL`. |
| `max_input_tokens` | integer | `180000` | Per-turn cap on the assembled context (pre-flight estimate). Env: `MODRON_MAX_INPUT_TOKENS`. `--max-input-tokens` overrides both. |
| `max_total_tokens` | integer | `2000000` | Cumulative token cap for one run, from provider-reported usage. Env: `MODRON_MAX_TOTAL_TOKENS`. `--max-total-tokens` overrides both. |
| `effort` | string | unset (provider default) | Reasoning effort: `low`, `medium`, `high`, `xhigh` or `max`. Sent on main-loop and escalation requests and inherited by sub-agents. Not sent on compaction, reviewer or router calls. `--effort` overrides it. |
| `reasoning_summary` | boolean | `false` | Ask the provider for a readable summary of the model's reasoning on main-loop, self-check and escalation requests, and have sub-agents inherit it. Anthropic only, and only for models that accept adaptive thinking with `display: "summarized"`: `claude-fable-5*`, `claude-mythos-5*`, `claude-opus-5*`, `claude-opus-4-8`, `claude-opus-4-7`, `claude-opus-4-6`, `claude-sonnet-5*`, `claude-sonnet-4-6` and `claude-haiku-5*` (an `anthropic.` or `anthropic/` namespace prefix is accepted). Other models, including Haiku 4.5, Sonnet 4.5 and Opus 4.5 and earlier, get no `thinking` field, as before. Other providers ignore it. This changes only whether the reasoning text is returned for display. It does not change billing, because the model thinks the same way either way. A session can switch it with a `ConfigPatch`; a client's reasoning display still needs its own opt-in. This has not been exercised against the live Anthropic API in this repository, so the exact response shape is unverified here. |
| `escalate_on_check_failure` | string (model id) | unset (off) | When the finish gate runs out of rounds with verification still failing, one extra turn runs on this model. Its answer replaces the run answer if that turn succeeds. |
| `review_model` | string (model id) | unset (no review) | When a run finishes with a diff, one tool-less review of that diff runs on this model. NeedsFix findings are fed back to the main model. Takes precedence over `[roles] critic`. |
| `review_max_rounds` | integer | `1` | How many NeedsFix rounds the reviewer may request per run. Used only with `review_model`. |

Note on `default_model`: the engine default is `claude-sonnet-5-5`. When neither
`--model` nor `default_model` is set, the first strong-tier catalog model of the
chosen provider is used instead (`claude-opus-5-5` for `anthropic`). Without
`--provider`, the router picks the model for each turn. With `--provider`, the
run is pinned to one model for every turn.

## [permissions]

Rules that decide, per tool call, whether it runs, asks or is refused. Each key
is a list of rule strings.

| Key | Type | Default | Effect |
|---|---|---|---|
| `allow` | list of strings | `[]` | Matching calls run without a prompt, except in deny mode. |
| `ask` | list of strings | `[]` | Matching calls are sent to the approver, even for ungated tools and in yolo mode. With no approver attached, the call is refused. |
| `deny` | list of strings | `[]` | Matching calls are refused. The tool error names the rule. |

Precedence: any matching `deny`, else any matching `ask`, else any matching
`allow`, else no verdict (the approval mode decides as usual).

The order of the whole gate is: `--deny` list, then plan phase (gated tools are
refused), then `[permissions]`, then the `--allow` list and the approval mode.

Rule syntax, from `permissions.rs`:

- A rule is `tool` (every call of that tool) or `tool(pattern)`.
- Tool names contain only letters, digits, `_` and `-`.
- The subject of a call is its `command` argument for `bash`. For any other tool
  it is the first string argument named `path`, `file_path` or `pattern`.
- Glob characters: `*` matches any run of characters, except `/` for path tools.
  `**` matches anything, including `/`. `?` matches one character. Every other
  character is literal.
- A leading `./` is removed from paths before matching.
- A path that contains `..` never matches an `allow` rule.
- Bash compound commands: an `allow` pattern never matches a command that
  contains `;`, `&`, `|`, a backtick, `$(`, `>`, `<` or a newline. `deny` and
  `ask` patterns also match each segment of the command, split on `;`, `&`, `|`
  and newlines.

```toml
[permissions]
allow = ["bash(cargo test*)", "read"]
ask   = ["bash(git push*)"]
deny  = ["read(.env*)", "bash(rm -rf*)"]
```

Rules are inherited by sub-agents. The same rule set applies to sessions run
by `modron serve`, the daemon and `modron acp`.

## [hooks]

Shell commands run through `sh -c`. Each hook runs with a 10-second limit.

| Key | Type | Default | Effect |
|---|---|---|---|
| `pre_tool` | string | unset | Runs before every tool call. A non-zero exit or a timeout vetoes the call. Its stderr tail becomes the tool error the model sees. The full arguments JSON is on stdin. |
| `post_tool` | string | unset | Runs after every tool call. Observe only: a failure is logged and never blocks. |
| `session_start` | string | unset | Runs once at the start of each run. Observe only. |
| `turn_end` | string | unset | Runs once per model turn, after its tools. Observe only. |

Environment set for each hook:

| Hook | Variables |
|---|---|
| `pre_tool`, `post_tool` | `MODRON_TOOL_NAME`, `MODRON_TOOL_ARGS` (compact JSON, cut at 32 KiB), `MODRON_TOOL_ARGS_TRUNCATED` (`1` when cut), `MODRON_SESSION_ID`. `post_tool` also gets `MODRON_TOOL_EXIT_CODE` (`0` or `1`, from the tool outcome, not the shell). |
| `session_start` | `MODRON_EVENT=session_start`, `MODRON_SESSION_ID`, `MODRON_WORKING_DIR` |
| `turn_end` | `MODRON_EVENT=turn_end`, `MODRON_SESSION_ID`, `MODRON_WORKING_DIR`, `MODRON_TURN`, `MODRON_MODEL`, `MODRON_TOOL_CALLS`. Stdin is a JSON object that also carries `input_tokens` and `output_tokens`. |

Hooks do not inherit provider credentials. Variables whose names end in
`_API_KEY`, start with `ANTHROPIC_`, `OPENAI_` or `ZAI_` (this includes the
`*_BASE_URL` variables of those providers), or are exactly `GITHUB_MODELS_TOKEN`,
`MODRON_GITHUB_TOKEN`, `MODRON_DEVICE_TOKEN` or `MODRON_PUBLISH_TOKEN` are removed
from the child environment. `PATH`, `HOME`, `SHELL` and similar names are kept.
Other tokens, such as `GITHUB_TOKEN`, are kept so that tools can work.

## [verify]

| Key | Type | Default | Effect |
|---|---|---|---|
| `command` | string | required when the section is present | Shell command for the project's checks, for example `cargo test --quiet`. Registers the `verify` tool. After a run that changed files, the finish gate re-runs it and feeds failures back, for at most two rounds. |

## [compaction]

Autocompaction summarises older history when the context grows. The CLI flags
`--no-compact` and `--compact-threshold` override the matching keys. The order
is: engine default, then this section, then flags.

| Key | Type | Default | Effect |
|---|---|---|---|
| `enabled` | bool | `true` | Turns autocompaction on or off. |
| `threshold_frac` | float | `0.8` | Compact when the estimated context exceeds this fraction of the input window. Must be greater than 0 and less than 1. |
| `min_interval_turns` | integer | `4` | Minimum number of turns between two automatic compactions. |
| `tail_messages` | integer | `6` | Number of most recent messages kept verbatim after compaction. |
| `summary_max_bytes` | integer | `2048` | Size limit for the summary text. |
| `bitmap_compaction` | bool | `true` | When `false`, keeps the summary text only and drops the bitmap attachment. |

## [sessions]

| Key | Type | Default | Effect |
|---|---|---|---|
| `ttl_days` | integer | unset (never) | At startup, deletes sessions not updated for this many days. `modron sessions vacuum` still works without it. |

## [handoff]

How a session moves between hosts (`modron handoff export|import`, see
[HANDOFF.md](HANDOFF.md)).

| Key | Type | Default | Effect |
|---|---|---|---|
| `carrier` | string | `"git"` | `"git"` moves the working tree and the session log as git refs. `"furrow"` moves the working tree (ignored files included) as a furrow snapshot in `furrow_store`; the session bundle travels inside the snapshot. |
| `furrow_store` | string | unset | Required with `carrier = "furrow"`. An `s3://bucket/prefix` URL or a local directory. Credentials are never read from this file (see the furrow variables below). Note: furrow 0.3.7's `clone` (used by import) accepts only `s3://` and `ssh://` URLs, so a local directory can be exported but not imported by the real furrow. |

Furrow carrier, in short: modron runs the `furrow` CLI found on PATH, or the
binary named by `MODRON_FURROW_BIN`. Furrow reads its own settings from the
environment, and modron passes the environment through without reading it:

- `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION` (or
  `AWS_DEFAULT_REGION`): S3 credentials and region.
- `FURROW_S3_ENDPOINT`, `FURROW_S3_PATH_STYLE`, `FURROW_S3_ALLOW_HTTP`: an
  S3-compatible endpoint (path style is automatic with a custom endpoint).
- `FURROW_RECOVERY_KEY` (read by modron too, which requires it): the 64-hex
  workspace key. Every host that exports or imports the same session must hold
  the same key. Losing it makes the store unreadable.

No value of these variables is printed or logged by modron; furrow's stderr is
passed through with any of them removed.

Serve-side option: `modron serve --handoff-on-exit` (or `MODRON_HANDOFF_ON_EXIT=1`)
exports the session that remote control serves when serve stops (ctrl+c or
SIGTERM), after its run is interrupted. It needs `--remote-control`. The export
has 30 seconds; after that serve exits without it. See [HANDOFF.md](HANDOFF.md).

## [router]

Model selection for each turn. `modron router eval` scores the selector offline.

| Key | Type | Default | Effect |
|---|---|---|---|
| `mode` | string | `learned` | `learned` uses corpus centroids with a rules fallback. `heuristic` uses rules only. Any other value stops startup. |
| `cheap_model` | string (model id) | derived | Model for the cheap tier. |
| `strong_model` | string (model id) | derived | Model for the strong tier. |
| `actions` | table of action to model id | unset | Per-action overrides. Valid keys: `chat`, `search`, `summarize`, `plan`, `edit`. Overrides win over the tier models. |

When both `cheap_model` and `strong_model` are unset, the tiers are derived from
the providers that have a key or sign-in, and an ordered fallback chain is used.
Setting either one turns that fallback chain off. The unset tier still uses its
derived model. Without any authenticated provider, the built-in pair
`claude-haiku-5-5` and `claude-sonnet-5-5` is used.

## [judge]

An optional LLM routing judge. It classifies task complexity before routing.
When `[judge]` is set, `[kev]` is ignored.

| Key | Type | Default | Effect |
|---|---|---|---|
| `mode` | string | `ambiguous` | `ambiguous` asks the judge only when the router is unsure. `always` asks it for every decision. Any other value stops startup. |
| `model` | string (model id) | the cheap-tier model | Judge model. |
| `provider` | string | the run's default provider | Provider the judge model is called through. |

## [kev]

An optional decision-model server that speaks a compatible API. Used when
`[judge]` is not set.

| Key | Type | Default | Effect |
|---|---|---|---|
| `endpoint` | string (URL) | required | Base URL of the server, for example `http://127.0.0.1:8009`. |
| `model` | string | `kev-latest` | Decision model id. |
| `api_key` | string | unset | Bearer token. It is redacted in debug output. |
| `mode` | string | `ambiguous` | Same values as `[judge] mode`. |

## `[agents.<name>]`

Named personas, selected with `--agent <name>`. The `delegate` tool also takes a
persona name in its `persona` argument.

| Key | Type | Default | Effect |
|---|---|---|---|
| `system` | string | unset | Replaces the assembled system prompt for the persona. |
| `model` | string (model id) | unset | Forces every routing decision onto this model. This is a pin. |
| `provider` | string | the catalog provider of `model` | Provider for the forced model. |
| `allow` | list of tool names | `[]` | Added to the `--allow` list. |
| `deny` | list of tool names | `[]` | Added to the `--deny` list. Deny wins. |

## `[mcp.servers.<name>]`

Each server is either a stdio child or a Streamable HTTP endpoint. Set exactly
one transport. Tools appear as `mcp__<server>__<tool>`. They are approval-gated
and kept off the prompt until found with `tool_search`.

| Key | Type | Default | Effect |
|---|---|---|---|
| `command` | string | required for stdio | Program to launch. |
| `args` | list of strings | `[]` | Arguments for `command`. |
| `env` | table | `{}` | Extra environment variables for the child process. |
| `url` | string | required for HTTP | Streamable HTTP endpoint, for example `https://host/mcp`. Replies may be JSON or SSE. |
| `headers` | table of strings | `{}` | Request headers for an HTTP server. Values may contain `${NAME}`, expanded at connect time. An unset or empty variable is an error. The error names the server, never a value. |
| `max_schema_tokens` | integer | `20000` | The server is refused if the schemas of its tools exceed this many estimated tokens. |

Keep secrets in the environment and reference them as `${NAME}`:

```toml
[mcp.servers.web]
url = "https://example.test/mcp"
headers = { Authorization = "Bearer ${MODRON_MCP_TOKEN}" }
```

`MODRON_MCP_TOKEN` is an example name chosen by you. The binary does not read it.

Not supported over HTTP: the legacy SSE transport, OAuth, and server-initiated
requests.

### Built-in preset: `playwright`

`modron mcp add playwright` writes this entry to `config.toml` (the path that
`MODRON_CONFIG` overrides, as elsewhere):

```toml
[mcp.servers.playwright]
command = "npx"
args = ["-y", "@playwright/mcp@latest"]
```

The entry is appended as a block, so existing comments and layout stay as they are.
An entry with the same name is never replaced unless you pass `--force`; that
replacement rewrites the file and drops its comments. The command needs Node.js.
`npx` fetches the package on first run. Restart modron to load the server.

## Project files from other agents (rules, commands, MCP)

Modron reads a few files other coding agents keep in a repository, so a
project set up for them works here too. Everything is read from the working
directory (`--working_dir`, default `.`; MCP trust uses the current directory).
Text only, and each file must be a regular file inside the working directory:
a symlink that points elsewhere is ignored.

**Rules (project memory).** Loaded into the system prompt under
`# Project rules (other agents' files)`, one `## <path>` heading per file:

| Source | Notes |
|---|---|
| `.cursor/rules/*.md`, `*.mdc` | sorted by file name; a leading `---` front-matter block is stripped |
| `.github/copilot-instructions.md` | |
| `GEMINI.md` | |
| `.windsurfrules` | |

`AGENTS.md` and `CLAUDE.md` are loaded as before (first found wins) and are
not affected. Each rule file is capped at 16 KB and all rule files together
at 48 KB; a cut is marked `[truncated: ...]` in the prompt.

**Commands.** Each `.claude/commands/<name>.md` is a prompt template. A task
that starts with `/<name>` is replaced by the file body, with every
`$ARGUMENTS` replaced by the text after the command word. The expansion runs
in the engine when a task is submitted, so `modron "/triage 42"` and any client
that sends the line works. The template is capped at 16 KB. A name that is not
a file stays a plain task. Built-in commands (`/model`, `/review`, ...) are
handled by the client before a task is sent, so a project file cannot replace
them from the app. The app's composer does not list project commands yet and
rejects an unknown `/word` before sending it.

**MCP servers.** `.mcp.json` and `.cursor/mcp.json` in the workspace declare
servers in the common shape:

```json
{
  "mcpServers": {
    "docs": { "command": "docs-server", "args": ["--stdio"], "env": {} },
    "remote": { "url": "https://example.test/mcp", "headers": {} }
  }
}
```

A declared server is **never started until you trust it for this workspace**:

```sh
modron mcp list               # each server: trusted / untrusted, and its file
modron mcp trust docs         # trust one server (or: modron mcp trust --all)
modron mcp untrust            # revoke for this workspace (or: modron mcp untrust <name>)
```

Trust is stored in `mcp-trust.json` in the Modron config directory (next to
`config.toml`). Each grant is keyed by the canonical workspace path, the
server name and a hash of the server's definition. Changing the definition
(command, args, env, url, headers) needs trust again, and the server does not
start until you trust it. A name in `[mcp.servers]` always wins over a project
file with the same name; that config entry runs as before and needs no trust.
If two project files declare the same name, `.mcp.json` wins.

`modron doctor` lists the rule files, commands and project MCP servers it
found, with their trust state.

## [roles]

One optional model id per kind of work. A blank value counts as unset.

| Key | Type | Default | Effect |
|---|---|---|---|
| `plan` | string (model id) | unset | Model for main-loop turns while the run is in the `plan` phase. |
| `subagent` | string (model id) | unset | Default model for `delegate` children that have no persona model. |
| `critic` | string (model id) | unset | Reviewer model. Setting it enables one review round, unless `review_model` is set. |
| `summary` | string (model id) | unset | Model for compaction summaries. |

A pinned run ignores the `plan`, `subagent` and `summary` roles. A pin comes from
`--provider` or from a persona with `model`.

Each role model runs on the provider that serves it: the model catalog
maps the id to a provider you are signed in to, falling back to the
turn's provider when the id is unknown. To name the provider yourself,
write `provider::model` (for example `anthropic::claude-opus-5-5`). The
same form works for `review_model` and `escalate_on_check_failure`.

## Environment variables

Variables the binary reads, grouped by purpose. A variable that is not listed
here is not read by `modron`.

### Configuration and runtime

| Variable | Effect |
|---|---|
| `MODRON_CONFIG` | Config file or directory. See the top of this page. |
| `MODRON_DEFAULT_PROVIDER` | Overrides `default_provider`. |
| `MODRON_DEFAULT_MODEL` | Overrides `default_model`. |
| `MODRON_MAX_INPUT_TOKENS` | Overrides `max_input_tokens`. |
| `MODRON_MAX_TOTAL_TOKENS` | Overrides `max_total_tokens`. |
| `MODRON_GOAL_MAX_ITERATIONS` | Iteration cap for `--goal` runs. Default `10`. A non-numeric value is ignored. |
| `RUST_LOG` | Tracing filter. Replaces the default `info`, or `debug` with `--verbose`. |
| `MODRON_HANDOFF_ON_EXIT` | `modron serve`: `1` or `true` turns on `--handoff-on-exit`. |
| `MODRON_RUN_ID` | `modron serve`: the cloud run this serve belongs to (same as `--run-id`; PLAN 10.10). Letters a-z, digits and dashes, 1 to 64. The portal registration then carries it. |
| `MODRON_FURROW_BIN` | `modron handoff` with `carrier = "furrow"`: path of the furrow binary (default: `furrow` on PATH). |
| `FURROW_RECOVERY_KEY` | `modron handoff` with `carrier = "furrow"`: the workspace recovery key (required). Furrow's own variables are listed under `[handoff]`. |

### Provider credentials and endpoints

Keys and base URLs are read when a provider is built. The provider is built when
its key is set, or, for `chatgpt`, when a stored sign-in exists. Saved keys
written by `modron auth set` (and by `/auth` in the app) are kept in
`~/.config/modron/env`, which is sourced at every launch.

| Provider id | Key | Base URL override | Default endpoint |
|---|---|---|---|
| `anthropic` | `ANTHROPIC_API_KEY` | `ANTHROPIC_BASE_URL` | Anthropic Messages API |
| `openai-compat` | `OPENAI_API_KEY` (optional when `OPENAI_BASE_URL` is set) | `OPENAI_BASE_URL` | `https://api.openai.com/v1`; a custom base URL makes the key optional, for keyless local servers such as Ollama. The output limit is sent as `max_completion_tokens` on `api.openai.com` and Azure OpenAI hosts, and as `max_tokens` elsewhere; a server that refuses `max_tokens` with a `max_completion_tokens` hint is retried once and the choice is kept for the session |
| `zai` | `ZAI_API_KEY` | `ZAI_BASE_URL` | `https://api.z.ai/api/paas/v4` |
| `openai-responses` | `OPENAI_API_KEY` (required) | `OPENAI_BASE_URL` | `https://api.openai.com/v1`, path `/responses`. The OpenAI Responses API with a plain API key: same request and stream code as `chatgpt`, reasoning items replayed; no ChatGPT account header, no token refresh. Its reasoning blocks are scoped to this provider id, separate from `chatgpt` |
| `chatgpt` | none; uses a stored ChatGPT sign-in | none | ChatGPT backend |

The stored ChatGPT sign-in (EXPERIMENTAL) is created by `modron auth login
--provider chatgpt` (device code in the browser), by `auth add` option `g`, or by
reusing `~/.codex/auth.json` at first run. It lives in `~/.modron/auth/openai.json`
(mode 0600) with the account id the backend requires; the access token is
refreshed with the stored refresh token. `modron auth status` shows it masked and
`modron auth logout chatgpt` deletes it. `OPENAI_API_KEY` is not touched by
either command.
| `groq` | `GROQ_API_KEY` | `GROQ_BASE_URL` | `https://api.groq.com/openai/v1` |
| `google` | `GEMINI_API_KEY` | `GOOGLE_BASE_URL` | `https://generativelanguage.googleapis.com/v1beta/openai` |
| `cerebras` | `CEREBRAS_API_KEY` | `CEREBRAS_BASE_URL` | `https://api.cerebras.ai/v1` |
| `mistral` | `MISTRAL_API_KEY` | `MISTRAL_BASE_URL` | `https://api.mistral.ai/v1` |
| `nvidia` | `NVIDIA_API_KEY` | `NVIDIA_BASE_URL` | `https://integrate.api.nvidia.com/v1` |
| `openrouter` | `OPENROUTER_API_KEY` | `OPENROUTER_BASE_URL` | `https://openrouter.ai/api/v1` |
| `github-models` | `GITHUB_MODELS_TOKEN` | `GITHUB_MODELS_BASE_URL` | `https://models.github.ai/inference` |

`--provider` accepts only `anthropic`, `openai-compat`, `openai-responses`,
`chatgpt`, `zai` and `mock`. The other ids work through `default_provider`, through `/model` in the
app and chat, and through the router, once their key is set.

Z.ai Coding Plan keys work only on the coding endpoint. Set
`ZAI_BASE_URL=https://api.z.ai/api/coding/paas/v4` for them.

### Web search keys

The `web_search` tool searches through one API. The backend is the first one whose
key is set, in this order: `BRAVE_API_KEY`, `TAVILY_API_KEY`, `EXA_API_KEY`. With no key
set the tool refuses and names the three variables. It never scrapes a search page
without a key.

| Backend | Key | Set with | Endpoint |
|---|---|---|---|
| Brave Search | `BRAVE_API_KEY` | `modron auth set brave <key>` | `https://api.search.brave.com/res/v1/web/search` |
| Tavily | `TAVILY_API_KEY` | `modron auth set tavily <key>` | `https://api.tavily.com/search` |
| Exa | `EXA_API_KEY` | `modron auth set exa <key>` | `https://api.exa.ai/search` |

Keys are read from the environment first, then from the portal credential overlay,
the same order the providers use. `modron auth set` saves them to
`~/.config/modron/env` (mode 0600). They are sent only in request headers and never
appear in tool output. The tool takes `query`, `max_results` (default 5, at most 10),
and optional `allowed_domains` and `blocked_domains` lists, which match a domain and
its subdomains. Results are returned as numbered entries marked as untrusted web content.

### Remote access and publishing

| Variable | Read by | Effect |
|---|---|---|
| `MODRON_HOST_TOKEN` | `modron serve` | Bearer token sent on the bridge tunnel. Needed when the bridge has `HOST_TOKEN` set. Never shown in error text. |
| `MODRON_HOST_KIND` | `modron serve` | `cloud` marks the session's host as cloud. Any other value, or unset, means local. |
| `MODRON_REMOTE_CONTROL` | `modron serve` | Bridge URL (`ws://` or `wss://`) to open remote control against at start, as `--remote-control`. The flag wins when both are set. Empty counts as unset. |
| `MODRON_PORTAL_CREDENTIALS` | `modron serve` | `1` or `true` turns on `--portal-credentials`: the owner's provider keys are fetched from the bridge vault (needs `--remote-control` and `MODRON_HOST_TOKEN`), held in memory only, and used for future turns. A key set in the environment or the env file wins. Set by the cloud container. |
| `MODRON_PUBLISH_TOKEN` | `modron serve` (`/share`) | Bearer token for publishing a page to the bridge. Without it, publishing fails before any request is sent. |
| `MODRON_DEVICE_TOKEN` | `modron --connect ws://...` | Device token for a `modron serve` that requires one. A `token` URL parameter wins over it. |

### Upgrade

| Variable | Read by | Effect |
|---|---|---|
| `MODRON_GITHUB_TOKEN` | `modron upgrade` | GitHub token for release downloads. Tried before `GITHUB_TOKEN` and the `gh` keyring. |
| `GITHUB_TOKEN` | `modron upgrade` | Fallback GitHub token for release downloads. |

### Terminal app (`modron app`)

| Variable | Read by | Effect |
|---|---|---|
| `MODRON_APP_BIN` | `modron app` | Path to the `modron-app-tui` binary. When set (non-empty) it must be a file; there is no fallback to the other lookup places. |
| `MODRON_BIN` | `modron-app-tui`, set by `modron app` | The `modron` binary the app starts as its server. `modron app` sets it to the running binary unless it is already set. |

### Variables set by modron

These are set for hooks and tool-hook children. Do not set them yourself. See
[Hooks](#hooks) above: `MODRON_EVENT`, `MODRON_SESSION_ID`, `MODRON_WORKING_DIR`,
`MODRON_TURN`, `MODRON_MODEL`, `MODRON_TOOL_CALLS`, `MODRON_TOOL_NAME`,
`MODRON_TOOL_ARGS`, `MODRON_TOOL_ARGS_TRUNCATED` and `MODRON_TOOL_EXIT_CODE`.

## Sandbox (command line only)

There is no `sandbox` key. Use `--sandbox off|seatbelt|bubblewrap`. The default
is `off`, where the approval gate is the only control. `seatbelt` is for macOS.
`bubblewrap` is for Linux and needs `bwrap` on the `PATH`. A value that the
platform does not support stops startup. The `webfetch` and `web_search` tools
refuse to run while any sandbox is active.

## Files and directories

| Path | Purpose |
|---|---|
| `~/.config/modron/config.toml` | Configuration file. |
| `~/.config/modron/env` | Saved provider keys, mode 0600, sourced at launch. |
| `~/.modron/sessions/` | Session transcripts, one JSONL file per session (`--sessions-dir`). |
| `~/.modron/memory/` | Workspace memory. Sits beside the sessions directory. |
| `~/.modron/daemon.sock` | Daemon socket. |
| `~/.modron/serve.sock` | Unix socket of `modron serve`. |
| `~/.modron/bench/*.jsonl` | `modron bench` baselines. |
| `~/.modron/evals/*.jsonl` | `modron eval` records, one row per task run (`edit_tool`, edit calls, edit errors and retries per row). |
| `~/.modron/logs/modron.log` | Log file for interactive `modron` runs, which keeps stderr clear. |
| `~/.modron/update-check.json` | Cache for the once-a-day update check. |
| `.modron/worktrees/` | Git worktrees for `delegate` with `isolate`, under the working directory. |
