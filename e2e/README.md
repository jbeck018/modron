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
node /home/user/modron/e2e/run.mjs --only S7,W     # prefixes: S7 = S7a..S7d, W = W1..W7
```

Output is one PASS, FAIL or SKIP line per step and a summary. A failing step
writes `<scratch>/evidence-<id>.json`: serve and bridge log tails, the last
local frames, each viewer's visible text, its role=button list, console
errors, WebSocket frames, and a screenshot per viewer.

Scratch lives under `/tmp/claude-0/e2e/run-*` (printed at start). Every
process the run starts is in its own process group and is killed on exit
(SIGTERM, then SIGKILL). Re-run with no leftovers; `left-over tracked
processes` in the summary must be 0.

## CI (GitHub Actions job `e2e`, `.github/workflows/ci.yml`)

The job builds its own inputs (`modron`, `modron-app-tui`, the web bundle),
stages them under `$RUNNER_TEMP/e2e`, installs `cloud/bridge` (`npm ci`),
Playwright 1.63.0 with Chromium, and `pyte==0.8.2` into `$RUNNER_TEMP/pyte`,
then runs `node e2e/run.mjs` with:

- `E2E_SCRATCH=$RUNNER_TEMP/e2e` (short root: the daemon socket stays under
  108 bytes);
- `E2E_PYTE_DIR=$RUNNER_TEMP/pyte`;
- `E2E_NO_SKIP=1`: a skipped step (for example a stale terminal binary) fails
  the job. Unset locally, skips are reported and do not fail the run.

Any FAIL exits 1. On failure the job uploads `run-*/**/*.json`,
`run-*/**/*.log` and `run-*/**/*.png` as the `e2e-results-<run id>` artifact.

## Scenarios

| ID | What it checks |
|---|---|
| S1-S6b | Legacy relay (no portal): share URL, history, browser drives, approve, stop, two viewers, late viewers. |
| S7a-S7d | Portal mode: a serve without the host token is refused; host registration; invite roles (viewer cannot submit, approver can approve but not submit). |
| P1 | Browser opens the portal root: redirect to `/app/`, sign-in screen, wrong token refused and cleared, right token lists the registered session online, opening it shows history, the owner token is in neither the URL, localStorage nor sessionStorage. |
| P2 | Owner types a task in the portal session; the approval is answered in the browser; the run finishes. Needs P1 (same browser). |
| P3 | Owner creates a viewer invite in Settings; a fresh context (no cookies) sees the transcript and cannot submit (`forbidden`); after Revoke, the same invite URL answers 410 and shows "invite is not valid". Needs P2. |
| P4 | The serve is stopped; the bridge's test clock (`x-test-now`, `MODRON_TEST_HOOKS=1`, added by the page through `route`) is six minutes ahead; the session row shows offline and a press does not open it. Needs P2. |
| R1 | `serve --remote-control` against the bridge with its own portal, NO client sends `remote_control`: the share-url file is written, the session shows online in the owner's portal list, the owner's browser opens it and runs a task (approval answered in the browser). Independent of the other steps. |
| C1 | Provider keys from the owner's vault (PLAN 10.9): the owner stores an `openai-compat` key in a bridge with `VAULT_KEY`; a serve with `--portal-credentials` and no key in its environment fetches it and sends it to a fake endpoint (the endpoint records the Authorization header). The key is then removed and the serve restarted: the next turn goes out with no key. No file under the serve home, and neither log, holds the value. Own bridge; needs no other step. |
| P5 | Web push: a serve's approval request (registered session) reaches a subscribed endpoint on a fake push service through the local bridge; the body decrypts (RFC 8291, node:crypto) to `approval needed: <tool>`, the title is the session title, and the VAPID JWT verifies. Own bridge with VAPID secrets and `PUSH_TEST_ORIGIN`; needs no other step. |
| P6 | Cloud run (PLAN 10.10): a GitHub account (fake GitHub) and a run start (fake container records the wake body); the script then plays the container entrypoint with that body, a real `modron serve --remote-control --run-id` registers, the run list shows `running` with a `session_url` that opens the session in the browser; stop reaches the fake container. Own bridge with `VAULT_KEY`, `WAKE_TOKEN`, `CONTAINER_URL` and `GITHUB_API_ORIGIN` (test hooks). |
| H1 | Handoff between two hosts (git carrier, shared bare remote): host A runs a task whose mock script writes `demo-output.txt`, exports the session with `modron handoff export --remote origin`, and keeps serving; host B clones, imports, serves and resumes the same session, with the transcript and the file present; host A refuses a new run on the handed-off session with "was handed off to another host". Needs no other step. |
| W1 | Steer (send while an approval waits) and queue (Alt+Enter); queue chip; the queued message starts its own run. |
| W2 | `/plan`: the engine refuses gated tools (`plan phase: read-only`); no approval asked; `/build` returns. The mock never proposes a plan. |
| W3 | Settings: effort and approval mode change; the server echoes them; the status line shows them. |
| W4 | `!echo ...` asks for approval, output shows; after `/mode deny`, the shell command is refused. |
| W5 | Ctrl+O sessions list; a new session; back to the first; each shows its own transcript. |
| W6 | `/compact` on a session with history: a before/after note or "nothing to compact". |
| W7 | Agent picker: the serve's own config (`[agents.reviewer]`, written to its HOME by `startServe({ config })`) is picked; `/agent` again and "no persona" clears it. Checks the echo, a fresh snapshot without `live.persona`, and the picker's current mark on "no persona". Fails on the mark (FINDINGS F16). |
| T1 | `modron-app-tui --provider mock` under a pty with a private HOME and MODRON_URL: connected, typed task reaches the approval, Esc denies, Ctrl+C twice exits with the resume line, no server left. |
| T2-T4b | The same pty driver in other modes: T2 Esc window and deny, T3 one Ctrl+C interrupts, T4 Ctrl+J and Shift+Enter make a three-line task, T4b a trailing backslash continues the line. |

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

## Terminal scenario (T1)

T1 reads the terminal app's rendered screen through pyte, a pure-Python
terminal emulator, because the app only writes changed cells and its
text never appears as one string in the raw output. Install it once into
a directory of your choice and point the suite at it:

    pip install --target /tmp/claude-0/e2e/pyte pyte==0.8.2
    E2E_PYTE_DIR=/tmp/claude-0/e2e/pyte node e2e/run.mjs --only T1

The driver also answers the terminal queries the app sends at startup, as a
kitty-capable terminal would: `CSI ? u` gets `CSI ? 1 u` and `CSI c` gets
`CSI ? 62 c` (pump() in tui_pty.py). crossterm then enables the kitty keyboard
protocol, which lets T4 send Shift+Enter as `CSI 13;2u`.

T1 also needs a terminal binary newer than app/src
(`cd app && cargo build --bin modron-app-tui`); otherwise it is skipped.

## Live deployed bridge (live.mjs)

    E2E_LIVE_ENV=<env file> node e2e/live.mjs

The env file holds `URL=`, `OWNER_TOKEN=` and `HOST_TOKEN=` lines. Without
`E2E_LIVE_ENV` the script prints SKIP and exits 0. Tokens are never printed or
written. Each run registers sessions and invites on the worker and removes
them at the end (invite DELETE, host registration DELETE).

| ID | What it checks |
|---|---|
| L1 | `/` redirects to `/app/`, `/app/` is text/html, the wasm asset is application/wasm with a cache header |
| L2 | `/api/me` refused without a cookie; wrong token 401; owner token sets `modron_owner`; `/api/me` reports the owner |
| L3 | host PUT refused without or with a wrong bearer; with HOST_TOKEN the row is listed online for the owner |
| L4 | local `modron serve` (mock) to the bridge; owner signs in in Chromium, opens the session, sends a task, taps allow once; the mock answer shows |
| L5 | owner mints viewer and approver invites; a fresh context with the viewer link is refused a submit; the approver approves |
| L6 | reload mid-approval replays the transcript and re-shows the approval |

Transport: the Rust serve client does not read HTTPS_PROXY and trusts only the
webpki roots, so from a TLS-intercepting container it cannot dial the bridge.
live.mjs therefore runs a loopback forwarder (Node `ws` and `fetch`, from
cloud/bridge/node_modules) for the serve's tunnel and registration traffic.
The browser and every portal or invite request go straight to the deployed URL.
