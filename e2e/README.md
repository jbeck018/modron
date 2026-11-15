# Packet E2E: local remote-control chain

Real `modron serve` (mock provider) -> real bridge Worker under
`wrangler dev --local` -> real web app in headless Chromium. Scenarios are
steps S1 to S7 in `run.mjs`; findings are in `FINDINGS.md`.

## Prerequisites

- Linux, Node 22 (built-in `WebSocket` global), Playwright with Chromium
  preinstalled (`require('playwright')` must resolve; nothing is installed).
- `cloud/bridge/node_modules` present (`wrangler` 4 in `.bin`).
- Prebuilt inputs, not rebuilt by the run:
  - `/tmp/claude-0/e2e/modron`: the `modron` binary.
  - `/tmp/claude-0/e2e/app/`: the web app bundle (index.html and assets).
- No Cloudflare account is used. Nothing is deployed or logged in to.

## Run

```sh
node /home/user/modron/e2e/run.mjs
```

What it does:

1. Copies `/tmp/claude-0/e2e/app/` into `cloud/bridge/public/app/`
   (gitignored) and starts a legacy bridge (no secrets) on a free port.
2. Runs S1 to S6b against that bridge, one `modron serve` per scenario
   (the mock script is per process, so approvals appear only in a
   process's first run).
3. Restarts a second bridge with `OWNER_TOKEN` and `HOST_TOKEN` and runs
   S7a to S7d (portal mode).
4. Prints PASS/FAIL per step. A failing step writes evidence to
   `<scratch>/evidence-<id>.json` (serve and bridge log tails, the last
   local frames, viewer console errors, viewer WebSocket frames).

Scratch lives under `/tmp/claude-0/e2e/run-*` (printed at start). Each run
uses fresh state, so the script can be re-run. Every process it starts is
in its own process group and is killed on exit (SIGTERM, then SIGKILL).
Wrangler processes from other sessions are not touched.

## Files

- `lib.mjs`: process tracking, bridge and serve launchers, a minimal local
  protocol client (`Local`), Playwright viewer helpers, portal HTTP helpers.
- `run.mjs`: the scenarios and the summary.
- `FINDINGS.md`: the failures, with reproductions and file:line pointers.

## Notes

- The web app's `/app/` is served from `public/app/`, which the bridge
  copies at dev start. Re-run `run.mjs` after rebuilding the bundle.
- Each step has a 240 s ceiling; a hung page is reported as a failure
  instead of stalling the run.
