# modron remote-control bridge

A transport-only relay for [modron](../../) remote-control sessions,
deployed on Cloudflare Workers with a Durable Object per share key.

- `modron serve` (any machine - laptop, home server, cloud box) runs
  `/remote-control <bridge url>`; it dials ONE outbound WebSocket to this
  relay (no ports opened).
- Any browser opens `https://<your-bridge>/s/<key>` and gets the minimal
  built-in viewer: transcript and streaming as a chat log (no build step,
  no dependencies).
- Frames relay verbatim - the bridge never interprets contents.
- Storage is bounded and GC'd: per-DO it keeps only the last 200 agent
  frames for late joiners, deleted by an alarm ~6h after both sides have
  disconnected; published pages expire after 7 days.

## Pages (published answers)

`POST /pages/<key>` publishes a shareable page, served at `/p/<id>`.
The agent already sends `Authorization: Bearer <key>` (see
`crates/modron-server/src/rc.rs`); the request is routed to the Durable
Object named by `<key>` and the bearer token is checked inside that DO,
so unauthenticated requests never reach storage. Bodies over 256KB are
rejected with 413, and each page lives in the publishing session DO's
storage with an `id -> key` pointer in a dedicated registry DO (written
only via DO-to-DO RPC). `/p/<id>` responses carry `CSP: default-src
'none'` and `nosniff`.

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
