# modron cloud container

Runs `modron serve` in a Cloudflare Container for one cloud run (PLAN 10.10).
The owner starts a run from the portal: the bridge (`cloud/bridge`) calls this
Worker's `POST /wake` with the run's environment, and the container clones the
repo, starts `modron serve --remote-control --run-id <id>`, and registers its
session with the bridge. The container sleeps after 10 minutes without
requests. `POST /stop` destroys it.

Status: the wake path, the entrypoint and the bridge side are tested locally
(`cloud/bridge/test/cloud.mjs`, the e2e scenario P6). The image is built in CI
but has not been deployed or run in a live Cloudflare Container.

## What the Worker holds

Nothing but `WAKE_TOKEN`. There is no repository, no git token, no host token
and no bridge URL in the Worker, the image, `wrangler.jsonc` or the workflow.
Those values arrive per run, in the wake body from the bridge:

| Field (wake body) | Container variable | Use |
|---|---|---|
| `run_id` | `MODRON_RUN_ID` | The run; also `--run-id` on `modron serve`. |
| `bridge_url` | `MODRON_BRIDGE_URL` | The bridge's `wss://` origin for the tunnel. |
| `host_token` | `MODRON_HOST_TOKEN` | Portal registration (equals the bridge's `HOST_TOKEN`). |
| `repo` | `MODRON_REPO` | `owner/name` to clone. |
| `branch` | `MODRON_BRANCH` | Optional branch to check out. |
| `git_token` | `MODRON_GIT_TOKEN` | The owner's GitHub token for the account (clone, handoff push). |
| `handoff_ref` | `MODRON_HANDOFF_REF` | Optional handoff to import before serving. |

Unknown fields, non-string values, over-long values and malformed repo, ref or
run id values are refused with 400 before any container starts.

## Secrets

| Where | Secret | Set by | Purpose |
|---|---|---|---|
| Container Worker | `WAKE_TOKEN` | the workflow, from the repository secret `WAKE_TOKEN` | Bearer for `/wake` and `/stop`. Unset: both answer 503. |
| Bridge Worker | `WAKE_TOKEN` | `wrangler secret put` in `cloud/bridge` | The same value; the bridge sends it. |
| Bridge Worker | `CONTAINER_URL` | `wrangler secret put` in `cloud/bridge` | This Worker's public https URL. |
| Bridge Worker | `HOST_TOKEN`, `VAULT_KEY`, `OWNER_TOKEN` | `wrangler secret put` in `cloud/bridge` | See `cloud/bridge/README.md`. |
| GitHub | a token per account | the owner, in the portal | Stored sealed in the bridge's Directory. Never a deploy secret. |

Repository secrets the workflow needs: `CLOUDFLARE_API_TOKEN`,
`CLOUDFLARE_ACCOUNT_ID`, and `WAKE_TOKEN`. The workflow no longer reads
`BRIDGE_URL`, `CLOUD_GITHUB_REPO`, `CLOUD_GITHUB_TOKEN` or `MODRON_HOST_TOKEN`:
those repository secrets can be deleted.

Provider keys are not container secrets. The owner stores them in the bridge
vault (`cloud/bridge/README.md`, "Provider keys (vault)"); the entrypoint sets
`MODRON_PORTAL_CREDENTIALS=1`, so `modron serve` fetches them from the bridge
at start and after each reconnect.

## Entrypoint

`entrypoint.sh` clones with the git token through `GIT_CONFIG_*` inside a
subshell: the header is set for that one `git` and is never on a command line,
never in `.git/config`, and never in `modron serve`'s environment. It checks out
`MODRON_BRANCH` when given and imports `MODRON_HANDOFF_REF` when given. Then it
execs:

    modron serve --listen 127.0.0.1:7699 --bridge <bridge_url> \
      --remote-control <bridge_url> --run-id <run_id> \
      --portal-credentials --handoff-on-exit

`MODRON_GIT_TOKEN` stays in serve's environment for one use: the handoff push
on exit. `modron serve` passes it to that push only, as a `GIT_CONFIG_*` header
(`crates/modron-cli/src/handoff.rs`). Every tool, hook, MCP and LSP child
process has it removed from its environment (`crates/modron-core/src/hooks.rs`,
`scrub_tokio_command`), so the agent cannot read it.

## Handoff on exit

`modron serve --handoff-on-exit` exports the run's session when it receives
SIGTERM or ctrl+c, and pushes it to `origin` with the git token (see above).
This depends on the platform delivering SIGTERM and waiting for the process
before it sleeps the container. That is unverified on a live container. The
hand-off has a 30 second limit.

## Trigger

Actions > "Cloud container deploy" > Run workflow. Inputs:

- `environment`: default `preview`. Deploys the Worker `modron-container-<environment>`.
- `confirm`: must be exactly `deploy`.

The run builds the image from the repo root as a gate, runs `wrangler deploy`
in `cloud/container`, and sets `WAKE_TOKEN`. Nothing runs on push.

## What gets created in the Cloudflare account

- A Worker `modron-container-<environment>` with a Durable Object class
  `ModronAgent` (SQLite, one per run id) and a container image built from
  `Dockerfile`.
- The Worker secret `WAKE_TOKEN`. Container image storage in the account's
  Cloudflare registry, which Wrangler manages.

## Costs and limits

Containers bill for running time and resources while awake, and the
Containers feature may need beta access on the account. `max_instances` is 5.
Check current Containers pricing before deploying.

## Remove it

    cd cloud/container
    npx wrangler@4.160.0 delete --name modron-container-<environment>

Then remove the bridge's `CONTAINER_URL` and `WAKE_TOKEN` secrets (cloud runs
answer 503 without them). The image and any Durable Object storage are removed
with the Worker.

## Known gaps

- **Containers API shape unverified live.** `start`, `running`,
  `setInactivityTimeout` and `destroy` follow the Containers docs as summarised
  (the source marks them UNVERIFIED). Check them on a real deploy.
- **A running container keeps its first environment.** A wake for a running
  run does not restart it with a new git token; re-issuing a token takes a
  stop then a wake.
- **No tunnel reconnect.** The tunnel opened at start is not redialled after a
  network drop, so restart the run (stop, then wake) to reopen it.
- **Build context unverified.** The Dockerfile copies from the repo root, but
  `image: ./Dockerfile` in `wrangler.jsonc` may use `cloud/container/` as the
  context. The CI gate builds from the root, so Dockerfile errors show up there.
- **Workspace coupling.** The image builds `modron-cli` and runs `modron serve`.
  It does not build the terminal app (`modron-app-tui`), which it does not need.
