# Bridge and web app: protocol notes (Packet C4b)

## Viewer frames, in order

What the unified web app (app/, via mobile/rust-core) sends on
`wss://<bridge>/ws/<viewer key>` after `/s/<key>` redirects to
`/app/?url=wss://<bridge>/ws/<viewer key>`:

1. `hello` (protocol version, `token` null). Sent by `RemoteClient::connect`.
   The bridge now replays its buffered agent frames on the FIRST hello of a
   socket (see `claimFirstHello` in src/index.ts), then forwards the hello to
   the agent, which answers `welcome`. Before this change only the
   bridge-private `{"type":"sync"}` replayed.
2. No `attach`. The app attaches only when the page URL carries `session=<uuid>`
   (`session::Driver::step`, app/src/session.rs). The redirect has no `session`,
   so the app never attaches.
3. Live frames from the agent are fanned out to every viewer by the bridge, not
   just to the requester. The web app folds `Snapshot`, `Event` and `Delta`
   whether or not it attached (`fold_frame`, mobile/rust-core/src/client.rs), so
   streaming output shows.

## Mismatch (not fixed here)

- The app's session id stays unset, so a composer submit becomes
  `submit(None, ...)` and asks the server to start a NEW session. The tunnel is
  scoped to its one session (rc.rs `ConnCtx::scoped`), so that submit is
  expected to be refused. This is inferred from rc.rs, not run end to end: the
  local test has no real `modron serve` agent.
- A late viewer gets only the buffered frames. Frames above 96 KB are relayed
  live but never stored, so an attach `Snapshot` larger than that is missing
  from replay and the app shows no history until new events arrive.

Fix options, both outside cloud/bridge:

- App (preferred): when `session` is absent, take `session_id` from the first
  `Snapshot` or `Event` and attach to it, or send `attach` after `welcome`.
- Server: rc.rs could add `&session=<uuid>` to the open URL it prints, so the
  app attaches by URL like the other clients.

The bridge cannot learn the session id itself without parsing agent frames,
which the bridge does not do by design (it is transport-only).

## Not wired on the web host

Interrupt, approvals and plan decisions have no key or button hook in the web
host (see app/NOTES.md). That is an app gap, not a bridge one.
