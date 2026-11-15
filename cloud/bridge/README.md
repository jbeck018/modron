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
```

If `public/app/index.html` is absent, the test writes a placeholder bundle
there and removes it afterwards.

## Protocol notes

`NOTES.md` lists the frames the web app sends to `/ws/<key>` and where its
session attach does not match the server (the app attaches only with a
`session` URL parameter, which the redirect does not carry).

## Roadmap

See ../README.md (cloud runtime phases).
