# Portal design (Program U, C4/C5)

Status: design, 2026-11-15. Plan: PLAN.md section 10. Wire additions:
docs/PROTOCOL_V2.md. Code: cloud/bridge (Worker + Durable Objects), app/
(the NSH web bundle the Worker serves under /app/).

Scope: one owner, self-deployed into the owner's own Cloudflare account.
No multi-tenant concerns; every stored secret belongs to the owner.

## Identities

| Who | Credential | Can |
|---|---|---|
| Owner (browser, phone) | Session cookie from an owner sign-in | List sessions, open any, drive, approve, mint and revoke invite links, link GitHub accounts |
| Host (`modron serve`, local or cloud) | `HOST_TOKEN` bearer on the agent tunnel | Register sessions, relay frames |
| Guest | Invite link (signed, scoped, expiring) | View, or view and approve, exactly one session |

Today's share key (possession = full access to one session) stays as the
transport key between a host and its Durable Object. It stops being
something to hand out: guests get invite links, the owner gets the
directory.

## Owner sign-in (v1)

- Deploy-time secret `OWNER_TOKEN` (`wrangler secret put`). `POST /auth`
  with the token sets an `HttpOnly; Secure; SameSite=Strict` cookie
  holding a random session id stored in the Directory object (30 days,
  revocable). Constant-time compare; 5 tries per minute per IP.
- v2 (after the directory works): passkeys (WebAuthn) registered while
  signed in with the token; the token remains the recovery path.
- GitHub is NOT the sign-in. GitHub accounts are linked identities (below),
  so two GitHub accounts never mean two portal owners.

## Session directory

A single `Directory` Durable Object (SQLite):

- `sessions(id, title, host_label, host_kind, viewer_key, state,
  updated_at)`; upserted by hosts over the tunnel (`HOST_TOKEN`), removed
  on host request or after 30 days without a heartbeat; a session with
  no heartbeat for 5 minutes is listed as offline.
- `owner_sessions(cookie_id, created_at, last_seen)`.
- `invites(id, session_id, role, expires_at, revoked, created_at, uses)`.
- `identities(id, kind, login, scopes, token_enc, created_at)`.

`GET /api/sessions` (owner cookie) lists them; the app's sessions screen
renders the list and opens `/app/?url=wss://host/ws/<viewer key>`.

## Invite links

- `POST /api/invites { session_id, role: viewer | approver, ttl_hours }`
  (owner) returns `https://host/i/<id>.<sig>`; `sig` = HMAC-SHA256 over the
  id with a key derived from `OWNER_TOKEN`. The record lives in the
  Directory, so revocation is immediate (`DELETE /api/invites/<id>`).
- `GET /i/<id>.<sig>` checks signature, expiry and revocation, then
  redirects to the app with a short-lived connection ticket. The viewer
  socket carries the ticket; the session Durable Object asks the Directory
  for the role once at connect and tags the socket.
- Enforcement is in the bridge, not the client: a `viewer` socket's
  client->server frames are dropped except `hello`, `attach`, `ping`; an
  `approver` socket may also send `approve` and `decide_plan`. Everything
  else needs the owner cookie.

## Linked GitHub accounts

- One GitHub OAuth app per deployment (`GITHUB_CLIENT_ID`,
  `GITHUB_CLIENT_SECRET` as Worker secrets; callback `/auth/github`).
- While signed in, the owner can link any number of accounts; each stores
  login, scopes and the access token encrypted with a key derived from
  `OWNER_TOKEN` (AES-GCM, random nonce). Tokens never reach a browser.
- A cloud run picks one identity; the Worker injects that token into the
  container at start (C6). Local hosts keep using their own git setup.

## Provider keys (vault, C6d)

Owner-only routes in the bridge store provider API keys encrypted under a
`VAULT_KEY` Worker secret (AES-256-GCM, HKDF-derived key, fresh nonce per
write, provider id as AAD). `GET /api/host/credentials` with the `HOST_TOKEN`
bearer returns the decrypted set to a host. `modron serve --remote-control
--portal-credentials` (env `MODRON_PORTAL_CREDENTIALS=1`, set by the cloud
container) fetches it at start and after each tunnel reconnect, keeps it in
process memory only, and applies it to future turns. A key set locally in the
environment or the env file wins over a vault key. Nothing is written to the
env file or the session log; the log names only the env vars fetched. Full
scheme and routes: `cloud/bridge/README.md` ("Provider keys (vault)").

## Notifications (C5)

Status (PUSH-1): the bridge side is built (cloud/bridge/README.md "Web
push"): every Notify frame from a serve becomes a Web Push for the owner's
subscribed devices, and for approver invites on their session. VAPID keys
are Worker secrets; subscriptions live in the Directory. Approval pushes carry
the tool name only. Not yet: the web app's subscribe control and the iOS
Home Screen hint. Not yet: skipping the push while an owner socket is
attached, which this design intends.

## Order of work

1. C4c: `HOST_TOKEN` on the tunnel, Directory object, owner sign-in,
   `/api/sessions`, invite links with role enforcement. Local tests only.
2. C4d: app sessions screen and sign-in view (web), invite management.
3. C4e: linked GitHub accounts.
4. C5: presence, Web Push, replay of large snapshots for late viewers.
5. C6: cloud runs.

Deploying any of it needs the owner's `wrangler login`; nothing in this
plan is deployed from CI or by an agent.

## Cloud runs (C6): container deploy status

Status 2026-11-16: scaffold. The image and a manually triggered deploy
workflow exist (`.github/workflows/cloud-deploy.yml`, `cloud/container/`), but
no cloud run has been deployed or tested end to end yet. The container starts
`modron serve --remote-control <bridge-url>`, which opens the tunnel at start
with no client attached. The container's runtime secrets are the Worker secrets listed in
`cloud/container/README.md`; `HOST_TOKEN` on the bridge must equal the
`MODRON_HOST_TOKEN` the container receives. Nothing is deployed by CI on push.

Presence survives drops: a tunnel (however it was opened) redials with backoff
(1 s doubling to 60 s, jittered, reset after a 30 s connection) on the same
share key, re-registers its directory entry on each new connection, and stops
only when the session is deleted, the tunnel is stopped, or the server shuts
down. A 401 or 403 from the bridge is never retried (see docs/remote-control.md).
The redial state is logged, not sent to clients: the portal has no
remote-control status field yet.
