# Packet E2E: local remote-control chain

Real `modron serve` (mock provider) -> real bridge Worker under
`wrangler dev --local` -> real web app in headless Chromium, plus the real
terminal app under a pty for T1. Findings are in `FINDINGS.md`.

## Prerequisites

- Linux, Node 22 (built-in `WebSocket` global), Playwright with Chromium
  preinstalled (`require('playwright')` must resolve; nothing is installed).
- Python 3 (T1 drives the terminal app with `pty`, run as `python3 -I`).
- `cloud/bridge/node_modules` present (`wrangler` 4 in `.bin`).
- Prebuilt inputs, not rebuilt by the run:
  - `/tmp/claude-0/e2e/modron`: the `modron` binary.
  - `/tmp/claude-0/e2e/app/`: the web app bundle (index.html and assets).
  - `app/target/debug/modron-app-tui` (T1 only): used only when it is newer
    than every file under `app/src`; otherwise T1 is SKIPPED with the reason.
- No Cloudflare account is used. Nothing is deployed or logged in to.

## Run

```sh
node /home/user/modron/e2e/run.mjs                 # every scenario
node /home/user/modron/e2e/run.mjs --only P1,P2    # some steps (see below)
node /home/user/modron/e2e/run.mjs --only S7,W     # prefixes: S7 = S7a..S7d, W = W1..W6
```

Output is one PASS, FAIL or SKIP line per step and a summary. A failing step
writes `<scratch>/evidence-<id>.json`: serve and bridge log tails, the last
local frames, each viewer's visible text, its role=button list, console
errors, WebSocket frames, and a screenshot per viewer.

Scratch lives under `/tmp/claude-0/e2e/run-*` (printed at start). Every
process the run starts is in its own process group and is killed on exit
(SIGTERM, then SIGKILL). Re-run with no leftovers; `left-over tracked
processes` in the summary must be 0.

## Scenarios

| ID | What it checks |
|---|---|
| S1-S6b | Legacy relay (no portal): share URL, history, browser drives, approve, stop, two viewers, late viewers. |
| S7a-S7d | Portal mode: a serve without the host token is refused; host registration; invite roles (viewer cannot submit, approver can approve but not submit). |
| P1 | Browser opens the portal root: redirect to `/app/`, sign-in screen, wrong token refused and cleared, right token lists the registered session online, opening it shows history, the owner token is in neither the URL, localStorage nor sessionStorage. |
| P2 | Owner types a task in the portal session; the approval is answered in the browser; the run finishes. Needs P1 (same browser). |
| P3 | Owner creates a viewer invite in Settings; a fresh context (no cookies) sees the transcript and cannot submit (`forbidden`); after Revoke, the same invite URL answers 410 and shows "invite is not valid". Needs P2. |
| P4 | The serve is stopped; the bridge's test clock (`x-test-now`, `MODRON_TEST_HOOKS=1`, added by the page through `route`) is six minutes ahead; the session row shows offline and a press does not open it. Needs P2. |
| W1 | Steer (send while an approval waits) and queue (Alt+Enter); queue chip; the queued message starts its own run. |
| W2 | `/plan`: the engine refuses gated tools (`plan phase: read-only`); no approval asked; `/build` returns. The mock never proposes a plan. |
| W3 | Settings: effort and approval mode change; the server echoes them; the status line shows them. |
| W4 | `!echo ...` asks for approval, output shows; after `/mode deny`, the shell command is refused. |
| W5 | Ctrl+O sessions list; a new session; back to the first; each shows its own transcript. |
| W6 | `/compact` on a session with history: a before/after note or "nothing to compact". |
| T1 | `modron-app-tui --provider mock` under a pty with a private HOME and MODRON_URL: connected, typed task reaches the approval, Esc denies, Ctrl+C twice exits with the resume line, no server left. |

Dependencies: P2 needs P1, P3 and P4 need P2, and the W steps and P1 each
start their own serve. `--only` does not start a missing step, so pick
dependencies with it.

## Design notes

- **Owner browser for the W steps.** A share link is read-only for
  configuration and shell (`docs/remote-control.md`). The W steps open the
  session through the portal bridge with the owner cookie, the way the owner
  opens it from the list. The cookie is set with `addCookies` (the sign-in is
  covered by P1 and is not repeated).
- **One model script per serve.** The mock's `rich_demo_script` is consumed
  once per process (bash, write, bash, final, then "Demo complete." forever).
  Each W step therefore starts its own serve; sessions for W are created with
  `new_session` so no run consumes the script before the step.
- **Console errors.** Any browser console error or page error fails the step
  that observes it, except the 401 and 503 probe messages. P3 also allows one
  410 message, and only when the viewer's main document answered 410 (the
  refused invite is the refusal itself; the check is `status === 410`).
- **Rate limit.** `POST /auth` allows five attempts a minute per client IP.
  The P1 sign-in and one `ownerCookie` call for the W steps share the portal
  bridge's bucket; keep the number of sign-ins low when re-running.
- **Test clock.** Only `MODRON_TEST_HOOKS=1` honours `x-test-now`. The
  portal-hooks bridge is started with it; no production bridge is affected.

## Files

- `lib.mjs`: process tracking and stop, bridge and serve launchers, the local
  protocol client (`Local`: `newSession`, `run`, `attach`, `rc`, `turnDone`),
  Playwright helpers, portal HTTP helpers, invite URL parsing.
- `run.mjs`: the scenarios, the `--only` filter and the summary.
- `tui_pty.py`: the T1 driver (pty, keys, exit status, leftover check).
- `FINDINGS.md`: failures, with reproductions and file:line pointers.

## Notes

- The web app's `/app/` is served from `public/app/`, which the bridge
  copies at dev start. Re-run after rebuilding the bundle.
- Each step has a 240 s ceiling; a hung page is reported as a failure
  instead of stalling the run.
- A full run takes several minutes; use `--only` for a quick check.
