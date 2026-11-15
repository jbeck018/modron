# Protocol v2 - additions for Program U

Status: design, 2026-11-15. Plan: PLAN.md section 10. Ledger: LEDGER.md
"Program U". This file is the contract both tracks (harness, NSH client)
build against. Each addition lands in `crates/modron-protocol/src/lib.rs`
with a doc comment stating its one UI effect before any client renders it
(AGENTS.md process rule 4).

## Compatibility rules

- `PROTOCOL_VERSION` goes 1 -> 2 once, when the first v2 frame ships. A v2
  server accepts v1 clients: every new field is `#[serde(default)]`, every
  new server->client payload is ignorable, and v1 clients never send the
  new client->server payloads.
- New enum variants are additive. Clients treat an unknown payload `type`
  as a no-op (today: `UNDECODABLE_FRAME_NOTICE`; v2 clients stay silent
  for unknown server payloads).
- Persisted `SessionEvent`s stay replayable by old binaries: new facts are
  new variants, never changed fields.

## 1. Actor and role (tandem, guests)

Every connection has an identity and a role, fixed at `hello`:

```
Role = owner | driver | approver | viewer
Actor { id: String, label: String, role: Role }
```

- `owner`: the paired device or portal owner. Everything.
- `driver`: submit, steer, queue, interrupt, approve. (Owner's other
  devices; a host that handed the session off.)
- `approver`: approve/deny and read. Invite link.
- `viewer`: read only. Invite link.

`Welcome` gains `actor: Option<Actor>` (who the server thinks you are).
Server->client frames caused by a person carry `actor: Option<Actor>`:
`ApprovalResolved`, `Steered`, `QueueChanged`, `TurnAborted`,
`ConfigChanged`, `PlanDecided`. The server enforces roles; a refused frame
answers `Error { kind: Forbidden }`.

`LiveState` gains `attached: Vec<Actor>` (presence). UI effect: a presence
row in the status surface.

## 2. Steer and queue (H1)

Two ways to talk to a running session:

- Steer: delivered to the model at the next safe boundary (between model
  turns, after pending tool results). The run continues.
- Queue: held by the server; starts a new run when the current one ends.

Client -> server:

```
Steer   { session_id, text }          // idle session: behaves as Submit
Enqueue { session_id, text }          // idle session: behaves as Submit
Dequeue { session_id, index }         // remove one queued message
```

Server -> client:

```
Steered      { text, actor }          // accepted; will be injected
QueueChanged { queued: Vec<String>, actor }
```

`LiveState` gains `queued: Vec<String>` and `steer_pending: u32`.

Engine: `EngineConfig.steer: Option<Arc<SteerInbox>>`. Injected messages
persist as `SessionEvent::Task { text }`, so replay shows them as task
echoes in order. An interrupt clears pending steers but keeps the queue.

UI effect: `Steered` shows the text as a pending task echo (dim) until its
persisted `Task` event arrives. `QueueChanged` replaces the queue chips
above the composer. Composer semantics in every client: submit while
running = steer; the alternate submit = enqueue.

## 3. Plan phase (H2)

A session has a phase: `plan` (read-only tools enforced by the engine) or
`build` (normal). The plan itself is the existing `SessionEvent::Plan`.

```
ConfigPatch.phase: Option<Phase>      // plan | build
PlanProposed { items: Vec<PlanItem> } // server -> client, run paused
DecidePlan  { session_id, decision: approve | revise, note: Option<String> }
PlanDecided { decision, actor }
```

In `plan` phase the engine refuses every mutating tool with a fixed
explanation, re-injects the current plan at the tail of each model turn
(not only on resume), and ends the run with `PlanProposed`. `approve`
flips the phase to `build` and starts a run with the plan as the task.
`revise` starts another plan run with the note.

UI effect: `PlanProposed` blocks on a decision exactly like an approval
(strip, buttons or sheet per target).

## 4. Sub-agents (H3)

- `ApprovalRequested` gains `agent: Option<String>` (the sub-agent label).
  A child's mutating call is offered to the parent session's approvers; a
  child never has more permission than its parent.
- `WireSwarmRow` gains live `turns` and `tokens` on progress (today they
  are zero until finish) and `state: running | waiting_approval | done |
  failed | killed`.
- New: `SteerAgent { session_id, agent, text }`, `KillAgent { session_id,
  agent }`, and server -> client `AgentFinished { agent, ok, brief }` (a
  push; `delegate_status` polling stays for the model).

UI effect: swarm rows update in place; a row opens the agent's own
activity panel; `AgentFinished` raises a toast when the agent is not the
focused view.

## 5. Reviewer verdict (H4)

```
SessionEvent::Review { verdict: pass | needs_fix | skipped,
                       findings: Vec<Finding>, model: String }
Finding { severity: p0..p3, path: Option<String>, line: Option<u32>,
          text: String }
```

Emitted after the finish gate and before `TurnDone`. `needs_fix` feeds the
findings back for a bounded number of rounds (config), like verify. Cheap
non-LLM claim checks (for example "N tests passed" with no test command
run) produce `Finding`s with `model: "rules"`.

UI effect: a transcript entry (`TranscriptEntry::Review`) with a one-line
verdict and an expandable findings list; grouped in modron-view.

## 6. Pause, wake, notify (H5)

```
SessionEvent::Paused { reason: waiting_on_user | waiting_on_agent |
                       needs_auth | scheduled, until: Option<Timestamp>,
                       note: String }
Wake { session_id, at: Timestamp | every: Duration, prompt }   // client
Notify { session_id, kind: approval | done | failed | paused,
         text }                                                // server
```

`Notify` is what the hub (Durable Object) turns into a push notification
for devices that are not attached. A sleeping cloud container is restored
by `Wake`, by `Submit`/`Steer`/`Enqueue`, or by an approval decision.

UI effect: `Paused` sets the status surface to the pause reason; `Notify`
is never rendered in the transcript.

## 7. Effort and roles (H7), permissions (H8), context (H9b)

- `ConfigPatch.effort: Option<Effort>` (`low | medium | high`); status
  surface shows it next to the model.
- `ConfigPatch.roles: Option<Map<Role, String>>` for `plan`, `subagent`,
  `critic`, `summary` models.
- `ConfigPatch.rules: Option<Vec<RuleWire>>` (`effect`, `spec`), the
  `core::permissions` rule syntax. `ExplainRule { tool, args }` ->
  `Note` with the per-rule votes.
- `Compact { session_id }` runs a real compaction between runs.
  `LiveState.context: Option<ContextGauge { used, window }>` is the true
  occupancy of the next request, not cumulative tokens.

## 7a. Model catalog

```
ModelsReq {}                                   // client
Models { models: Vec<ModelWire> }              // server
ModelWire { id, provider, label, context_window, available, tier }
PersistDefaultModel { model, provider }        // client, local only
```

`ModelsReq` is answered from the router catalog; `available` is true when
the model's provider is signed in. UI effect: the model picker lists these
rows; unavailable rows are shown but cannot be selected.

Selecting a model changes the current session only. An explicit save
(`PersistDefaultModel`) writes the server's default model and provider.
It is answered by a correlated `note`, or by an `error` when:

- `model` or `provider` is empty or contains a control character;
- `provider` is unknown: it names neither an auth-registry provider nor a
  provider any catalog model uses (`unknown provider <name>`).

What the save does: `default_model` and `default_provider` are written as
top-level keys of the server's config.toml (`$MODRON_CONFIG` or the
user config directory). The file is read, those two keys are set, and
the rest of the file is kept. The result goes to a sibling temp file that
is renamed over the original, so a crash never leaves a half-written
config. A config file that exists but does not parse is an error and is
not overwritten. The engine template that FUTURE sessions start from is
updated in memory, with its selector pinned to the saved model as a
`/model` switch is. Sessions that already exist keep their model.

Who may send it: local connections only. It is refused for every relayed
role (owner, approver, viewer) over a remote-control tunnel, like
`PersistRouter`. UI effect: none until the `note` arrives.

## 7b. Shell passthrough

```
Shell { session_id, command, private }         // client, owner only
```

The composer's `!` and `!!` (`parse_shell_input` in the client core, the one
shared rule) send this frame. `private` is `false` for `!cmd` and `true`
for `!!cmd`; it defaults to `false` when absent.

- `!cmd`: the labelled output joins the conversation. The next model turn
  sees it as a user message: the label "Shell command run by the user
  with !, not a model tool call", then "$ <command>" and the output.
- `!!cmd`: the command runs and its output is shown in the transcript with
  the shared `[private]` marker, but it never reaches the model. It is
  skipped when a model turn's context is built, and it is excluded from
  the input of compaction.

The persisted entry is a `UserShell { command, output, is_error, private }`
event. Logs written before this change hold the shared output as a `Task`
event with the same text; those still replay and reach the model as before.

Who may send it: the owner, on a local connection or a trusted owner
tunnel frame. Guests (approver, viewer) and share links cannot send it.
Refused for a missing session, an empty command, or a busy session (a run
is active, Conflict).

What is checked, and only this (PLAN.md 10.2a, decision 2):

- the `bash` tool must be registered;
- `bash` must not be on the deny list (`approvals.deny`), refused with
  `tool bash denied by configuration (--deny)`;
- no permission DENY rule may match the command, including a `;`, `&&`
  or `|` segment, refused with `tool bash denied by rule <rule>`;
- no `pre_tool` hook may veto it.

What is not checked: approval mode (Ask, Deny, ApproveAll), approver
prompts, ASK and ALLOW rules, and the plan phase. The owner is never
prompted. The command still runs in the sandbox policy, with the bash
timeout and output cap, the `post_tool` hook and the pre-mutation
checkpoint. A refused command persists nothing and ends with a `note`
carrying the reason, followed by `turn_done`. A command that ran ends
with `turn_done` and no `notify`.

## 8. Host and handoff (C6)

```
LiveState.host: Option<Host { kind: local | cloud, label, state:
                              running | sleeping | handing_off }>
Handoff { session_id, to: local | cloud }      // owner only
HandoffProgress { stage, message }
```

One host owns the working directory at a time. During a handoff the
session refuses submit/steer (queue is allowed) until the new host
attaches as the agent side; every client stays attached throughout.
