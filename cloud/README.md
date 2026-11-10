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

Open the printed URL on any device - transcript, streaming, approvals,
and a composer, live. Trust model: the share key IS the credential
(Claude Code share-link semantics); treat the URL as the secret.

## Roadmap

- Phase 2: a Cloudflare Sandbox/Containers runtime so `modron serve`
  itself runs in the cloud against a cloned repo (the sprite-machine
  Workspace DO pattern) - same relay in front.
- Phase 3: the web viewer moves to the nsh web target sharing
  modron-mobile-core (same protocol client in wasm).
