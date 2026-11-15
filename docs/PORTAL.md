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
  on host request or after 30 days without a heartbeat.
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

## Notifications (C5)

The session object turns `notify` frames into Web Push for the owner's
registered devices when no owner socket is attached (VAPID keys as Worker
secrets; subscriptions in the Directory). Approval pushes carry the tool
name only.

## Order of work

1. C4c: `HOST_TOKEN` on the tunnel, Directory object, owner sign-in,
   `/api/sessions`, invite links with role enforcement. Local tests only.
2. C4d: app sessions screen and sign-in view (web), invite management.
3. C4e: linked GitHub accounts.
4. C5: presence, Web Push, replay of large snapshots for late viewers.
5. C6: cloud runs.

Deploying any of it needs the owner's `wrangler login`; nothing in this
plan is deployed from CI or by an agent.
