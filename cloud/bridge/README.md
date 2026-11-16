# modron remote-control bridge

A transport-only relay for [modron](../../) remote-control sessions,
deployed on Cloudflare Workers with a Durable Object per share key.

- `modron serve` (any machine - laptop, home server, cloud box) runs
  `/remote-control <bridge url>`; it dials ONE outbound WebSocket to this
  relay (no ports opened).
- Any browser opens `https://<your-bridge>/s/<viewer key>`, which redirects
  to the unified web app at `/app/` with the viewer socket in its `url`
  parameter. `/s/<viewer key>?classic=1` keeps the old minimal inline viewer
  as a fallback until the app is proven.
- Frames relay verbatim - the bridge never interprets contents.
- Storage is bounded and GC'd: per session it keeps only the last 200
  agent frames as a replay buffer for late joiners, deleted by an alarm
  ~6h after both sides have disconnected. Published pages (below) are kept
  for 7 days, then removed by an alarm.

## Web app (static assets)

The web app is a static bundle built by the NSH CLI in `app/`
(`nsh package --target web -o dist`, output in `app/dist/Modron/`). It is
served from Workers Static Assets at `/app/`. The bundle is not committed:

```sh
cd ../../app && nsh package --target web -o dist   # build the bundle
cd ../cloud/bridge && sh scripts/sync-app.sh       # copy into public/app/
```

`sync-app.sh` replaces `public/app/` and prints the file count and byte
total. It fails with a message if the bundle is not built. `public/app/` is
in `.gitignore`.

Limits: Workers Static Assets allow up to 25 MiB per file and 20,000 files
per version by default. These are in the bundled Wrangler source
(`MAX_ASSET_SIZE` and `MAX_ASSET_COUNT_DEFAULT`, Wrangler 4.x). The bundle is
98 files and about 12.5 MB, and its largest file, the 5.6 MB wasm, is well
under 25 MiB.

Headers, set by the Worker (`serveApp` in src/index.ts): `.wasm` files are
served as `application/wasm`, hashed files (`name.<hex>.ext`) get
`public, max-age=31536000, immutable`, and everything else, including
`index.html`, gets `no-cache`.

## Pages (published answers)

`POST /pages/<key>` publishes a shareable page, served at `/p/<id>`.
Publishing requires a deploy-time secret, `PUBLISH_TOKEN`, sent as
`Authorization: Bearer <PUBLISH_TOKEN>`. The router checks it before any
Durable Object is touched:

- `PUBLISH_TOKEN` not configured: `503` ("publishing is disabled").
- Missing or wrong token: `401`.
- Bodies over 96KB (checked by `Content-Length` first, then by UTF-8 byte
  length of the body) are rejected with `413`.

The share key in the path only selects the Durable Object that stores the
page; it is not a credential, so possessing a share key does not let anyone
publish. Each page lives in the publishing session DO's storage with an
`id -> key` pointer in a dedicated registry DO (written only via DO-to-DO
RPC). `/p/<id>` responses carry `CSP: default-src 'none'` and `nosniff`.

### Publishing

Set the secret once per deployment (Wrangler prompts for the value; it is
never written to `wrangler.jsonc` or the repository):

```sh
npx wrangler secret put PUBLISH_TOKEN
```

Clients that publish (the modron server's page publish) read the same
value from the environment variable `MODRON_PUBLISH_TOKEN`. If that
variable is unset or empty, publishing fails locally with an error and no
request is sent.

## Deploy (one command)

Deploying runs in your own Cloudflare login. Build and sync the web app
first (see Web app above), then:

```sh
npx wrangler login   # once, with your own account
npx wrangler deploy  # prints https://modron-bridge.<you>.workers.dev
```

Then from the machine running `modron serve`:

```
/remote-control wss://modron-bridge.<you>.workers.dev
```

Open the printed URL on any device. Trust model: possession of the share
link is the credential for watching and driving that one session - treat
the URL as the secret. Publishing pages needs the separate `PUBLISH_TOKEN`
secret (see Publishing above).

Two keys per session: the agent connects to `/agent/<agent key>` with a
secret key that never leaves the machine running modron; the share link
carries only the viewer key, a one-way hash of it (`/s/<viewer key>`).
Someone holding a share link therefore cannot connect as the agent. The
derivation is shared by `viewerKeyOf` (src/index.ts) and `viewer_key`
(crates/modron-server/src/rc.rs); deploy the bridge and upgrade modron
together, or share links will not meet their agent.

Limits: viewer frames up to 96KB; agent frames are relayed live up to
4MB (so a long session's snapshot reaches viewers) but only frames up to
96KB are kept in the replay buffer. Keepalive replies are not stored.

## Local dev

```sh
npx wrangler dev    # serves on localhost:8787
node test/local.mjs # starts wrangler dev --local on a free port and checks
                    # the redirect, the classic viewer, /app/ headers and
                    # the viewer hello replay (no Cloudflare account used)
node test/portal.mjs # portal checks against three local Workers (see Tests)
```

If `public/app/index.html` is absent, the test writes a placeholder bundle
there and removes it afterwards.

## Portal (owner sign-in, session directory, invite links)

Design: `docs/PORTAL.md`. Step 1 of that plan is in this Worker: the
secrets, the `Directory` object, owner sign-in, host registration,
`/api/sessions`, `/api/me`, invite links, role enforcement on viewer sockets,
the app's sessions screen (the list opens each session through `open_url`),
and heartbeat-based staleness and removal (see "Staleness" below), and web
push (see "Web push" below). Still to come: linked GitHub accounts (C4e) and
passkeys (v2).

### Secrets

Both are Wrangler secrets, never in `wrangler.jsonc` or the repository:

```sh
npx wrangler secret put OWNER_TOKEN   # owner sign-in; also keys invite signatures
npx wrangler secret put HOST_TOKEN    # host registration and the agent tunnel
npx wrangler secret put VAULT_KEY     # provider-key vault (see "Provider keys")
```

- `OWNER_TOKEN` unset: every `/auth`, `/api/*` and `/i/*` route answers
  `503 portal not configured`. Share links work exactly as before.
- `OWNER_TOKEN` set: invite signatures are HMAC-SHA256 over the invite id,
  keyed by HKDF-SHA256 of the token (salt `modron-portal-v1`, info
  `modron-invite-sig-v1`). Rotating the token signs every owner out and
  voids every invite link.
- `HOST_TOKEN` unset: `/agent/<key>` behaves as before (the agent key is the
  credential) and `PUT /api/host/sessions/*` answers 503.
- `HOST_TOKEN` set: `/agent/<key>` needs `Authorization: Bearer <HOST_TOKEN>`
  on the WebSocket upgrade request. The router checks it before the key or any
  Durable Object is touched (401 otherwise).

Tokens are compared in constant time and never logged. Owner cookies are
stored in the Directory as SHA-256 hashes.

### Cloud run secrets (PLAN 10.10)

Set with `npx wrangler secret put <NAME>` from this directory. Each is
optional for the rest of the portal; without them the run routes answer 503
"cloud runs are not configured" and the GitHub account routes need only
`VAULT_KEY`.

- `CONTAINER_URL`: the container worker's public https URL (for example
  `https://modron-container-preview.<you>.workers.dev`). Plain http is accepted
  only for a loopback address under `MODRON_TEST_HOOKS`.
- `WAKE_TOKEN`: bearer the bridge sends to the container worker's `/wake` and
  `/stop`. Set the same value on the container worker (the deploy workflow
  reads the repository secret `WAKE_TOKEN`).
- `VAULT_KEY` and `HOST_TOKEN`: as above. Both are required for runs.

`GITHUB_API_ORIGIN` is a test-only variable (honoured under
`MODRON_TEST_HOOKS` for a loopback address); deployments leave it unset.

### Provider keys (vault)

Provider API keys belong to the owner, not to a container deploy (PLAN 10.9).
The owner stores a key here; a host running `modron serve --remote-control
--portal-credentials` fetches the set and uses it for future turns.

- `VAULT_KEY` (Wrangler secret) unset: `GET/PUT/DELETE /api/credentials*` and
  `GET /api/host/credentials` answer `503`; nothing else changes.
- `GET /api/credentials` (owner cookie): one row per allowed provider with
  `set` and `unreadable`. Never a value.
- `PUT /api/credentials/<id>` with `{ "key": "..." }` (owner cookie and an
  `Origin` header equal to the bridge's origin): stores the key, `204`.
  `DELETE` removes it, `204` or `404`.
- Allowed ids are the auth catalog's (`anthropic`, `openai-compat`, `zai`,
  `google`, `groq`, `openrouter`, `mistral`, `cerebras`, `nvidia`,
  `github-models`); each maps to one env var (`src/vault.ts`). Other ids are
  `400`. A key is 1-512 UTF-8 bytes with no control characters.
- `GET /api/host/credentials` (bearer `HOST_TOKEN`): `{ "credentials":
  { "ANTHROPIC_API_KEY": "...", ... } }`. The only route that returns a value,
  and only to the host.
- Encryption: AES-256-GCM. The key is HKDF-SHA256 of `VAULT_KEY` (salt
  `modron-vault-v1`, info `modron-vault-aes-gcm-v1`). Each write draws a fresh
  96-bit nonce; the provider id is the additional authenticated data. The
  Directory table `credentials` stores nonce and ciphertext only. A row that
  fails to open is reported `unreadable`, and the host fetch leaves it out.
- Invites never manage keys: a request presenting `?invite=` gets `403`; a
  request with no owner cookie gets `401`.
- Values are never logged and never returned to a browser. Responses are
  `cache-control: no-store`. Rotating `VAULT_KEY` makes existing rows
  unreadable; re-enter the keys (key rotation is not built).

### Routes

| Route | Who | Result |
|---|---|---|
| `POST /auth` `{ "token" }` | anyone | 200 + `modron_owner` cookie; 401 wrong; 429 after 5 attempts a minute per client IP (`CF-Connecting-IP`), counted before the token is compared; 400 for unknown fields |
| `POST /auth/logout` | anyone | 204, revokes the presented cookie and clears it |
| `GET /` | anyone | 302 to `/app/` when `OWNER_TOKEN` is set; the plain text answer when it is not |
| `GET /api/me` | owner cookie | 200 `{ "owner": true }`; 401 without a live cookie; 503 portal off |
| `GET /api/sessions` | owner cookie | 200, list of sessions, newest first. Each row: `id`, `title`, `host_label`, `host_kind`, `viewer_key`, `state` (`"offline"` when stale), `updated_at` (ISO 8601, last heartbeat), `online` (bool), `open_url` (see below) |
| `POST /api/invites` `{ session_id, role, ttl_hours }` | owner | 201 `{ id, url, expires_at }` (`expires_at` ISO 8601) |
| `GET /api/invites[?session_id=]` | owner | 200, list with `url`, `revoked`, `uses` |
| `DELETE /api/invites/<id>` | owner | 204 (revoke; repeating it is also 204) |
| `GET /i/<id>.<sig>` | invite holder | 302 to `/app/?url=<encoded ws url>?ticket=...`; 403 bad signature; 410 expired, revoked or session no longer registered. Body is always just "invite is not valid" |
| `PUT /api/host/sessions/<session id>` | host (`HOST_TOKEN`) | 204, upsert (a repeat PUT is the heartbeat) |
| `DELETE /api/host/sessions/<session id>` | host (`HOST_TOKEN`) | 204, idempotent |
| `GET /ws/<viewer key>?ticket=` | invite holder | ticket redeemed before the upgrade; the socket gets the invite's role |
| `GET /ws/<viewer key>` | owner cookie | owner socket, not filtered |
| `GET /ws/<viewer key>` | nobody | 401 when `OWNER_TOKEN` is set; the share key alone, as before, when it is not |

Every JSON body is capped at 16 KB and every field must be on the route's
list (an unknown field is 400, because a field that could widen access must
never be silently ignored).

### What a host sends (contract for `modron serve`)

1. Dial `wss://<bridge>/agent/<agent key>` with the header
   `Authorization: Bearer <HOST_TOKEN>` (the agent key still derives the
   viewer key, see above). `crates/modron-server/src/rc.rs` sends that bearer
   on the upgrade, and on every portal PUT and DELETE below.
2. Register each session it serves:
   `PUT /api/host/sessions/<session uuid, lowercase>` with the bearer and
   `Content-Type: application/json`:

   ```json
   {
     "title": "1 to 200 characters, no control characters",
     "host_label": "1 to 100 characters, e.g. the machine name",
     "host_kind": "local or cloud",
     "viewer_key": "the viewer key: 1 to 64 characters of [a-z0-9-]; 32 hex from viewer_key(agent key)",
     "state": "a lowercase token, [a-z][a-z_]{0,31}, e.g. idle or running"
   }
   ```

   rc.rs PUTs on open, on every running-state change, and every 60 seconds
   (the heartbeat). Remove it with `DELETE` when the session ends.

### Staleness

- A session whose last PUT is more than 5 minutes old (five missed
  heartbeats) is listed with `"online": false` and `"state": "offline"`. Its
  stored state is kept, so the next heartbeat brings it back.
- A registration not refreshed for 30 days is deleted from the Directory.
- Both are computed on access. Every session read or write in the Directory
  first deletes registrations older than 30 days, so no alarm is armed and
  nothing needs re-arming. An offline session can still be invited, and an
  invite for a removed session answers 410.

### `open_url` and the owner's browser

Each `/api/sessions` row carries `open_url`, a same-origin path such as
`/app/?url=wss%3A%2F%2Fbridge.example%2Fws%2F<viewer key>` (the inner
websocket URL is URL-encoded, with the scheme from the request). The owner
opens it in the same browser: the owner cookie rides on the websocket
upgrade to the same origin, and the socket is unfiltered. Without the cookie
the same URL is 401 (when `OWNER_TOKEN` is set). Clients should use
`open_url` rather than build the URL themselves.

### Test clock (tests only)

Under `MODRON_TEST_HOOKS=1` an `x-test-now: <epoch ms>` header replaces the
clock for `PUT`/`DELETE /api/host/sessions/*` and for `GET /api/sessions`
(staleness and the 30-day sweep). Without the hook var the header is never
read, and a non-numeric value is ignored. No production client sends it.

### Viewer roles

A socket tagged with a role (invite or owner) is filtered in the
`SessionBridge` object before anything is forwarded to the agent:

| Role | Frames it may send |
|---|---|
| `viewer` | `hello`, `attach`, `detach`, `ping`, `sync` |
| `approver` | the viewer list plus `approve`, `decide_plan` |
| `owner` (cookie) | everything |

A frame outside the list is not forwarded. The sender gets
`{"type":"error","message":"not permitted for this invite","kind":"forbidden"}`
(with the request's `id` when it had one), which is `Payload::Error` with
`ErrorKind::Forbidden` in `crates/modron-protocol`. Malformed JSON from a
tagged socket is dropped with no reply, and oversized frames are dropped
under the existing size limits.

### Relayed role (`role` on frames to the serve)

Every client frame the bridge forwards to the agent carries a top-level
`role` field that names the socket's grant: `owner` (owner cookie),
`approver` or `viewer` (redeemed invite ticket). Legacy sockets (portal
off, share key only) get no `role` at all.

- Who sets it: only this bridge (`stampRelayRole` in src/portal.ts, called
  from `SessionBridge.webSocketMessage`). It first deletes any `role` the
  client sent, so a client can never choose or raise its role. A
  tagged socket's frame that is not a JSON object is dropped.
- Who trusts it: `modron serve` (crates/modron-server, `handle_relayed_frame`
  and `relay_gate` in dispatch.rs), and only on a tunnel that authenticated
  to this bridge with the host token (`MODRON_HOST_TOKEN` set on the
  serve, so `/agent/<key>` carried the bearer). Any other tunnel ignores
  `role`. The serve's per-role decisions are listed there.
- Trust rule in one line: the bridge is the only writer of `role`, and the
  serve reads it only from a host-authenticated tunnel. Bridge deployments
  without `HOST_TOKEN` accept any agent tunnel, so the serve should only
  be pointed at a bridge that has `HOST_TOKEN` set.

Tested in test/portal.mjs: a viewer frame claiming `role: "owner"` reaches
the agent as `viewer`, an owner frame claiming `viewer` arrives as `owner`,
and a legacy socket's frame arrives with no `role` key.

Ticket: 64 hex characters, 60 seconds, single use, bound to the viewer key.
Redeeming it deletes it whether or not it matches.

### Compatibility

- Neither secret set: the deployment behaves as before. Share links keep
  working, the relay does not check bearers, and nothing in `/api` is served
  beyond the 503 answers.
- `OWNER_TOKEN` set: a bare `/ws/<key>` (share link) stops opening the viewer
  socket (401). Share-link holders need an invite or an owner sign-in.
- `HOST_TOKEN` set: hosts without the bearer are refused at `/agent`. Upgrade
  `modron serve` before setting it.
- The schema changes (`Directory`, migration tag `v2`) are additive. Existing
  `SessionBridge` storage is untouched.

### Tests

`node test/portal.mjs` starts three `wrangler dev --local` instances (no
Cloudflare account): A with random `OWNER_TOKEN` and `HOST_TOKEN` passed as
`--var`, and `MODRON_TEST_HOOKS=1`; B with no portal secrets; C with the two
secrets but no test hooks. It checks the rate limit, cookie attributes, the
503 answers and legacy relay, host registration and ordering, heartbeat
staleness (offline after 5 minutes, the exact boundary), the 30-day sweep,
`/api/me`, the root redirect, `open_url` (the owner cookie opens it and its
submit reaches a fake agent; without the cookie it is refused), invite
signatures, revocation, expiry, ticket single use, role filtering fed through
a fake agent socket, the owner socket, and logout. C checks that the clock
header and `ttl_seconds` are inert without the hook var. Each instance uses a
fresh persist directory under `.wrangler/`, removed afterwards.

Expiry: invites are normally at least one hour (`ttl_hours`). With
`MODRON_TEST_HOOKS=1` (set only by the test, never in a deployment)
`POST /api/invites` also accepts `ttl_seconds` (1 to 3600), and the test waits
about 2.2 seconds for a one-second invite to lapse.

## GitHub accounts and cloud runs (PLAN 10.10)

Owner only (cookie; invites get 403; no cookie 401), `VAULT_KEY` required (503
without it), same-origin `Origin` on writes. `cache-control: no-store`.

- `GET /api/github/accounts` -> `{ accounts: [{ id, login, kind }] }`.
- `POST /api/github/accounts` `{ token }`: GitHub `GET /user` with the token;
  stores the login and the sealed token, answers 201 `{ id, login, kind: "token" }`.
  A token GitHub refuses (401 or 403) is 400 `{ error, github_status }`; other
  GitHub failures are 502. At most 10 accounts (409 past that).
- `DELETE /api/github/accounts/<id>`: 204, or 404.
- `GET /api/github/repos?account=<id>&q=<text>&page=<n>` ->
  `{ repos: [{ full_name, private, default_branch, pushed_at }], next_page }`.
  Without `q`: one GitHub page of 50, sorted by pushed. With `q`: GitHub pages
  1 to 3 at 100 each are read and filtered here (case-insensitive, `owner/name`),
  then cut into pages of 50; a filter sees at most 300 repositories.
- `GET /api/runs` -> `{ runs: [{ id, repo, account, branch, state, reason,
  created_at, session_url }] }`, newest first, at most 100.
- `POST /api/runs` `{ account, repo, branch?, handoff_ref? }` -> 201 with the
  row. `repo` must be `owner/name`; refs are checked. The run is stored as
  `starting`, then the container worker's `POST /wake` is called with bearer
  `WAKE_TOKEN` and the body `{ run_id, bridge_url, host_token, repo, branch,
  git_token, handoff_ref }` (`bridge_url` is this bridge's `wss://` origin). A
  refused wake sets `failed` with reason `wake failed: container answered status N`.
- `POST /api/runs/<id>/stop` -> the row, `stopped` (or `failed`, reason
  `stop failed: ...`). The container worker's `/stop` gets `{ run_id }`.
- `POST /api/runs/<id>/wake` -> the row, `starting` (or `failed`). Sends the
  same body as the start, so the git token and handoff ref are sent again.

Run states: `starting`, `running`, `sleeping`, `stopped`, `failed`. A host
registration (`PUT /api/host/sessions/<id>`) may carry `run_id` (`[a-z0-9-]`,
at most 64); with a known run that has not stopped or failed, the run becomes
`running` and links the session. `sleeping` is in the vocabulary but nothing
sets it yet (the container does not report back).

The git token appears only in the wake body the bridge sends to the container
worker over https. No browser-facing response carries a token; a GitHub error
is answered with its status, never its body. Outbound calls: GitHub only at
`https://api.github.com` (test override above), the container worker only at
`CONTAINER_URL`, each with a 10 second timeout.

Tests: `node test/cloud.mjs` (wrangler local, with a fake GitHub and a fake
container worker on loopback). The e2e scenario P6 runs a real
`modron serve --run-id` against this bridge.

## Web push (approvals, plans, finished runs)

Notify frames (docs/PROTOCOL_V2.md section 6: `approval`, `plan`, `done`,
`failed`, `wake`) from a serve become Web Push messages for the devices
subscribed to that session. The bridge forwards the frame unchanged to the
viewers, and in the background it sends one push per subscription:

- Payload (JSON, encrypted with RFC 8291 aes128gcm): `title` (the session
  title, or `modron` when the session is not registered), `body` (the Notify
  text exactly as the server wrote it: tool names only, never arguments),
  and `data.url` (the session's portal URL, `<origin>/app/?url=...`).
- Headers: `Authorization: vapid t=<ES256 JWT>, k=<public key>` (RFC 8292,
  12 hour token), `TTL: 300`, `Urgency: high`.
- At most one push per session and kind every 10 seconds (approval bursts
  collapse to the first one).
- A 404 or 410 from the push service deletes that subscription.
- Only https endpoints on fcm.googleapis.com, updates.push.services.mozilla.com,
  `*.push.apple.com` and `*.notify.windows.com` are accepted (400 otherwise).
  Endpoints and keys are never logged.

Secrets (Wrangler secrets, unset means `/api/push/*` answers 503 and nothing
else changes):

```sh
npx wrangler secret put VAPID_PUBLIC_KEY    # base64url, 65-byte P-256 point
npx wrangler secret put VAPID_PRIVATE_KEY   # base64url, 32-byte scalar
npx wrangler secret put VAPID_SUBJECT       # mailto:you@example.com or https://...
```

Generate the pair once with Node (WebCrypto, no packages), then store the two
printed values with `wrangler secret put`:

```sh
node -e 'crypto.subtle.generateKey({name:"ECDSA",namedCurve:"P-256"},true,["sign","verify"]).then(async k=>{const j=await crypto.subtle.exportKey("jwk",k.privateKey);const pub=Buffer.concat([Buffer.from([4]),Buffer.from(j.x,"base64url"),Buffer.from(j.y,"base64url")]);console.log("VAPID_PUBLIC_KEY="+pub.toString("base64url"));console.log("VAPID_PRIVATE_KEY="+j.d)})'
```

Routes (owner cookie, or an approver invite for its own session; viewer
invites and no credential are refused, 401 without a credential, 403 for a
viewer invite):

| Route | Result |
|---|---|
| `GET /api/push/key[?invite=<id>.<sig>]` | 200 `{ "key": <VAPID_PUBLIC_KEY> }`; 503 when VAPID is unset |
| `POST /api/push/subscribe` `{ endpoint, keys: { p256dh, auth }, invite? }` | 204; stored by a SHA-256 of the endpoint. An owner subscription gets every session; an approver invite gets its own session only |
| `DELETE /api/push/subscribe` `{ endpoint, invite? }` | 204, idempotent |
| `GET /app/sw.js` | the push receiver (show the push, a tap focuses or opens the session). Served by the Worker, so the bundle needs no extra file. No caching, no fetch handler |

Storage: the Directory's `push_subscriptions` table (additive; no new binding).

Test-only: `PUSH_TEST_ORIGIN` (a fake push service on 127.0.0.1) is honoured
only with `MODRON_TEST_HOOKS=1`, like the clock hook.

Tests (no Cloudflare account; real push services cannot be reached from
tests, so the push service is a local fake):

```sh
node --experimental-strip-types --no-warnings test/push-unit.mjs  # RFC 8291 section 5 and appendix A, RFC 8292 example, allow-list, notify parsing
node test/push.mjs   # wrangler dev x2: routes and their refusals, delivery and decryption, rate limit, 410 removal, 503 without VAPID
```

Not done: the web app's control (subscribe and unsubscribe from the
settings screen), the iOS Home Screen hint, and the rule in docs/PORTAL.md
that skips the owner's push while an owner socket is attached. The bridge
sends for every Notify it sees, rate limited as above.

## Protocol notes

`NOTES.md` lists the frames the web app sends to `/ws/<key>` and where its
session attach does not match the server (the app attaches only with a
`session` URL parameter, which the redirect does not carry).

## Roadmap

See ../README.md (cloud runtime phases).
