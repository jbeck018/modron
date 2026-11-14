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
- Storage is bounded and GC'd: per session it keeps only the last 200
  agent frames as a replay buffer for late joiners, deleted by an alarm
  ~6h after both sides have disconnected. Published pages (below) are kept
  for 7 days, then removed by an alarm.

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

```sh
npx wrangler login   # once
npx wrangler deploy  # prints https://modron-bridge.<you>.workers.dev
```

Then from the machine running `modron serve`:

```
/remote-control wss://modron-bridge.<you>.workers.dev
```

Open the printed URL on any device. Trust model: possession of the share
key is the credential for watching a session (same as Claude Code share
links) - treat the URL as the secret. Publishing pages needs the separate
`PUBLISH_TOKEN` secret (see Publishing above).

## Local dev

```sh
npx wrangler dev    # serves on localhost:8787
```

## Roadmap

See ../README.md (cloud runtime phases).
