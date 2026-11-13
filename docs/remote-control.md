# Remote control — `modron serve`

The remote-control server (Phases 2+3) runs the Modron engine as a
long-lived process so clients anywhere (desktop, phone, another machine)
can submit tasks, watch the live stream, answer approvals, and interrupt
runs. Sessions are server-owned: a client disconnecting never touches a
running task, and a run keeps going with zero clients attached.

```
+----------------+        WebSocket (JSON frames)      +----------------------+
| remote client  | <-----------------------------------> |    modron serve      |
| (TUI/GUI/web)  |                                       |  SessionHub          |
+----------------+                                       |  - broadcast/seq     |
                                                         |  - approvals         |
+----------------+        Unix socket (NDJSON frames)    |  - interrupt         |
| local client   | <-----------------------------------> |  - per-session engine|
+----------------+                                       +----------+-----------+
                                                                    | BroadcastingStore
                                                              (persist + fan out)
```

## Wire protocol

One vocabulary for both transports: `modron-protocol::Frame`, tagged
`type` in snake_case, serialized as JSON.

- WebSocket: one JSON `Frame` per text message (default
  `ws://127.0.0.1:7699`, override with `--listen <ADDR>` or disable with
  `--listen disabled`).
- Unix socket: one JSON `Frame` per line (NDJSON) on `~/.modron/serve.sock`.

Every connection starts with a `hello` frame and gets `welcome`:

```json
-> {"id":1,"type":"hello","client":"my-client","protocol":1}
<- {"id":1,"seq":0,"type":"welcome","server_version":"0.1.2","protocol":1}
```

Server-to-client event frames carry a per-session monotonic `seq`. The
server deduplicates the snapshot/live overlap for you: an `attach` holds
the session's append lock across "load the log + read the watermark" and
drops any buffered frame at or below it, so the snapshot plus the
buffered live frames tile exactly. If the server's buffer overflows
while your socket is slow you get an explicit `error` frame (re-`attach`
to resync) instead of silently missing frames.

### The 60-second client

```json
-> {"id":2,"type":"submit","task":"fix the flaky test"}
<- {"seq":1,"session":"<uuid>","type":"event","event":{"type":"task","text":"fix the flaky test"}}
<- {"seq":2,"session":"<uuid>","type":"delta","delta":{"kind":"text","text":"looking..."}}
<- {"seq":3,"session":"<uuid>","type":"tool","activity":{"kind":"start","name":"bash"}}
<- {"seq":9,"session":"<uuid>","type":"turn_done","summary":{...}}
```

Commands: `submit` (session_id omitted starts a new session; the
submitting connection is attached automatically), `attach`
(`last_seq`-based catch-up, answered by `snapshot`), `detach`,
`interrupt`, `approve` (first responder wins), `set_config`
(model/provider/sandbox/approval mode/allow/deny/persona/goal for future
turns), `fork`, `new_session`, `list_sessions`, `delete_session`,
`create_task` / `get_task_status` / `merge_task` (worktree tasks),
`automations_list` / `automation_add` / `automation_remove`,
`remote_control` (open a tunnel), `publish_page` (share an answer),
`auth_providers_req` (provider status rows), `persist_router` (write
router defaults), `ping`. Every failure answers `error` with a
machine-readable `kind` (auth / not_found / conflict / other).

## Pairing (remote auth)

Loopback and Unix-socket connections are trusted -- no token. Any
non-loopback `--listen` bind requires a device token in the `hello`
frame.

```sh
modron pair myphone     # mint + print a 32-char token (stored 0600)
modron pair list        # paired devices
modron pair revoke myphone
```

The token lives in `~/.modron/clients/<name>.token` as
`{"name", "token", "created_at"}`. Send it with every remote hello:

```json
{"id":1,"type":"hello","client":"myphone","protocol":1,"token":"<32 chars>"}
```

Tokens are shown once at mint time; revoke stops the device at the next
hello. `modron serve` writes `~/.modron/serve.pid` and removes the socket
+ pid file on ctrl+c.

## Runbooks

### Tailscale

Simplest secure setup: bind loopback and let tailscale expose it with
identity-based access control.

```sh
modron serve --listen 127.0.0.1:7699 &
tailscale serve --bg --https=443 http://127.0.0.1:7699
```

Clients connect to `wss://<machine>.<tailnet>.ts.net`. Tailscale
terminates TLS and authenticates the tailnet identity in front of the
plain-WS listener.

Alternative: bind the tailnet IP directly (skips `tailscale serve`; the
address is non-loopback, so device tokens are REQUIRED):

```sh
modron serve --listen 100.101.102.103:7699   # your tailscale IPv4
modron pair myphone                          # token for the hello frame
```

WireGuard encryption applies either way; with the direct bind, add
`tailscale serve` ACLs or rely on the token gate.

### Cloudflare Tunnel

Run the server loopback-only and put a `cloudflared` sidecar in front
(TLS terminates at Cloudflare's edge):

```sh
modron serve --listen 127.0.0.1:7699 &

# quick tunnel (ephemeral URL, fine for trying it out):
cloudflared tunnel --url http://127.0.0.1:7699

# named tunnel (stable hostname):
cloudflared tunnel create modron
cloudflared tunnel route dns modron modron.example.com
cat > ~/.cloudflared/config.yml <<'EOF'
tunnel: modron
credentials-file: /Users/you/.cloudflared/<tunnel-id>.json
ingress:
  - hostname: modron.example.com
    service: http://127.0.0.1:7699
  - service: http_status:404
EOF
cloudflared tunnel run modron
```

Clients connect to `wss://modron.example.com`. For zero-trust access
(recommended), put a Cloudflare Access application in front of the
hostname: the browser/client authenticates to Access first, then the
token gate inside modron still applies (pair the device with
`modron pair`). Without Access, the device token is the only gate.

## Notes and limits

- Sessions and transcripts are the shared JSONL store
  (`~/.modron/sessions`): `modron sessions list`, `--resume`, and the
  TUI see everything the server creates.
- Attach snapshots carry the full persisted log plus the live view
  (streaming buffers, grouped tool activity, pending approval, cumulative
  usage, model/provider/goal labels).
- Sticky approve-rest: answering one request with `allow_all` auto-approves
  every later mutating call in that session (the TUI's `a`).
- A run whose task panics would leave the run slot occupied until restart;
  engine failures are reported as `turn_failed` frames, never slots.
- v1 wire format is JSON over WS/NDJSON; a binary codec can replace the
  framing later without touching `modron-protocol`.
