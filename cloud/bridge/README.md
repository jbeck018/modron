# modron remote-control bridge

A transport-only relay for [modron](../../) remote-control sessions,
deployed on Cloudflare Workers with a Durable Object per share key.

- `modron serve` (any machine - laptop, home server, cloud box) runs
  `/remote-control <bridge url>`; it dials ONE outbound WebSocket to this
  relay (no ports opened).
- Any browser opens `https://<your-bridge>/s/<key>` and gets a live chat
  surface: transcript, streaming, approvals (allow/deny), composer.
- Frames relay verbatim - the bridge stores nothing beyond the last 200
  agent frames (for late joiners) and never interprets contents.

## Deploy (one command)

```sh
npx wrangler login   # once
npx wrangler deploy  # prints https://modron-bridge.<you>.workers.dev
```

Then from the machine running `modron serve`:

```
/remote-control wss://modron-bridge.<you>.workers.dev
```

Open the printed URL on any device. Trust model: possession of the share
key is the credential (same as Claude Code share links) - treat the URL
as the secret.

## Local dev

```sh
npx wrangler dev    # serves on localhost:8787
```

## Roadmap

See ../README.md (cloud runtime phases).
