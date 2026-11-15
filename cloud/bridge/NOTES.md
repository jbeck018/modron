# Bridge and web app: protocol notes (Packets C4b, C5a)

## Viewer frames, in order

What the unified web app (app/, via mobile/rust-core) sends on
`wss://<bridge>/ws/<viewer key>` after `/s/<key>` redirects to
`/app/?url=wss://<bridge>/ws/<viewer key>` (or after an invite or an
`open_url` from `/api/sessions` opens the same socket):

1. `hello` (protocol version, `token` null). Sent by `RemoteClient::connect`.
   The bridge replays its buffered agent frames on the FIRST hello of a
   socket (see `claimFirstHello` in src/index.ts), then forwards the hello to
   the agent. The agent answers `welcome`, and then (rc.rs, since the
   snapshot change) an `Attach` snapshot of the tunnel's one session, so a
   late viewer gets full history from the server. Only the bridge-private
   `{"type":"sync"}` replays without a forward.
2. `attach` only when the page URL carries `session=<uuid>`
   (`session::Driver::step`, app/src/session.rs). The redirect has no
   `session`, so the app does not send `attach`. It does not need to: the
   server's snapshot after each hello is the attach. Owner and invite
   sockets are the same here.
3. Live frames from the agent are fanned out to every viewer by the bridge, not
   just to the requester. The web app folds `Snapshot`, `Event` and `Delta`
   whether or not it attached (`fold_frame`, mobile/rust-core/src/client.rs), so
   streaming output shows.

## Relayed role (what the serve may trust)

Each client frame forwarded to the agent carries `role` (owner, approver or
viewer) for a tagged socket and no `role` for a legacy socket. The bridge
removes any client-sent `role` first (`stampRelayRole`, src/portal.ts), so
the field is only ever what the bridge granted. The serve trusts it only on
a tunnel that sent the host token. Frames that are not JSON objects are
dropped for tagged sockets (also for owner sockets, which used to pass them
through unchanged). Test: test/portal.mjs, "relayed role" checks.

## Replay and ordering (current behaviour)

The bridge keeps the last 200 agent frames (each up to 96 KB) per session.
On a socket's first `hello`, or on any `sync`, it sends that buffer to the
socket. Ordering guarantees, in src/index.ts `webSocketMessage`:

- The replay is written with `ws.send` before the hello is forwarded with
  `agent.send`. Every agent answer (welcome, the Attach snapshot) is a later
  event, so it reaches the viewer after the replay.
- The replay read (`storage.list`) runs under the Durable Object input gate:
  it is one consistent prefix of the stored agent stream. No append is half
  done, and no later frame is mixed into it.
- A live frame that reached a socket before that socket's hello is not held
  back. The replay can therefore repeat it. The snapshot that follows the
  hello resets the client (Snapshot resets; Delta appends), so the repeat is
  transient and cannot leave a stale view behind.
- Only the first hello replays per socket, which bounds the replay work.
- Frames above 96 KB are relayed live but never stored, so they are not in
  the replay. The server's snapshot after hello is what brings them back.

## Late viewers (was: "only the buffered frames")

Earlier revisions said a late viewer gets only the buffered frames. That is
no longer true: the server answers every relayed hello with a fresh snapshot
(rc.rs), and the replay is only a convenience that makes the first screen
appear before that snapshot arrives. Dropping the replay would not lose
history. Keeping it does no harm, for the ordering reasons above.

## Mismatch (not fixed here)

- The app's session id stays unset, so a composer submit becomes
  `submit(None, ...)` and asks the server to start a NEW session. The tunnel is
  scoped to its one session (rc.rs `ConnCtx::scoped`), so that submit is
  expected to be refused. This is inferred from rc.rs, not run end to end: the
  local test has no real `modron serve` agent.
- Resolved by the server snapshot after hello (see "Late viewers" above): a
  viewer no longer depends on the replay for history, so a frame above 96 KB,
  which the replay never stores, still reaches it live.

Fix options for the submit mismatch, both outside cloud/bridge:

- App (preferred): when `session` is absent, take `session_id` from the first
  `Snapshot` or `Event` and attach to it, or send `attach` after `welcome`.
- Server: rc.rs could add `&session=<uuid>` to the open URL it prints, so the
  app attaches by URL like the other clients.

This section was written before the Packet C5a snapshot change; the submit
mismatch was not re-checked in that packet.

The bridge cannot learn the session id itself without parsing agent frames,
which the bridge does not do by design (it is transport-only).

## Not wired on the web host

Interrupt, approvals and plan decisions have no key or button hook in the web
host (see app/NOTES.md). That is an app gap, not a bridge one.
