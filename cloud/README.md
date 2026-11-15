# modron in the cloud

Phase 1 of the cloud story is the **remote-control bridge**: a tiny
Cloudflare Worker (one Durable Object per share key) that relays the
modron protocol between `modron serve` (any machine) and any browser.

## Deploy the bridge (one command)

```sh
cd cloud/bridge
npx wrangler login    # once
npx wrangler deploy   # prints https://modron-bridge.<you>.workers.dev
```

## Open a session from anywhere

On the machine running `modron serve` (TUI or `modron chat`):

```
/remote-control wss://modron-bridge.<you>.workers.dev
```

Open the printed URL on any device. `/s/<key>` redirects to the unified web
app at `/app/`, which connects to the session's viewer socket. The minimal
inline viewer is still reachable at `/s/<key>?classic=1` as a fallback.
The web app's bundle is built in `app/` and copied into the bridge with
`cloud/bridge/scripts/sync-app.sh` before deploying (see
cloud/bridge/README.md). Trust model: the share key IS the credential
(Claude Code share-link semantics); treat the URL as the secret.
Publishing a `/pages` page additionally requires `Authorization: Bearer
<PUBLISH_TOKEN>`, a deploy-time secret set with `npx wrangler secret put
PUBLISH_TOKEN`; clients read it from `MODRON_PUBLISH_TOKEN` (see
cloud/bridge/README.md).

Deploying takes `npx wrangler deploy` from your own Cloudflare login;
nothing in this repository deploys on your behalf.

## Roadmap

- Phase 2: a Cloudflare Sandbox/Containers runtime so `modron serve`
  itself runs in the cloud against a cloned repo (the sprite-machine
  Workspace DO pattern) - same relay in front.
- Phase 3: the web viewer moves to the nsh web target sharing
  modron-mobile-core (same protocol client in wasm). Packet C4b serves that
  app from the bridge; the classic inline viewer remains as a fallback.
