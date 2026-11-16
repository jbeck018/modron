# modron cloud container

Runs `modron serve` in a Cloudflare Container against a cloned GitHub repo.
A Worker (`src/worker.ts`) wakes one container per run; the container sleeps
after 10 minutes without requests. The relay in front is `cloud/bridge`.

Status: scaffold. The image builds in CI; the wake path is written but not
deployed or tested live. Read "Known gaps" before relying on it.

## Repository secrets (Settings > Secrets and variables > Actions)

Deploy-time (used by the workflow itself):

| Secret | Purpose |
|---|---|
| `CLOUDFLARE_API_TOKEN` | Wrangler API token: Workers Scripts edit, Containers edit, Durable Objects. Required. |
| `CLOUDFLARE_ACCOUNT_ID` | Account the Worker is deployed into. Required. |

Runtime (copied by the workflow into the Worker as secrets; each is skipped
with a notice when unset):

| Repository secret | Worker secret | Purpose |
|---|---|---|
| `BRIDGE_URL` | `BRIDGE_URL` | Relay base, `wss://modron-bridge.<you>.workers.dev`. |
| `CLOUD_GITHUB_REPO` | `GITHUB_REPO` | Repo to clone, `owner/name`. |
| `CLOUD_GITHUB_TOKEN` | `GITHUB_TOKEN` | Token that can read the repo (fine-grained PAT, contents read). Used once for the clone, then unset in the agent process. |
| `MODRON_HOST_TOKEN` | `MODRON_HOST_TOKEN` | Must equal the bridge's `HOST_TOKEN` secret, for portal registration. |
| `WAKE_TOKEN` | `WAKE_TOKEN` | Bearer token for `POST /wake`. Choose a long random string. If unset, wake is disabled. |
| `ANTHROPIC_API_KEY` | `ANTHROPIC_API_KEY` | Provider key for the agent (optional). |
| `OPENAI_API_KEY` | `OPENAI_API_KEY` | Provider key for the agent (optional). |

GitHub does not allow repository secret names starting with `GITHUB_`, which
is why the two GitHub values use the `CLOUD_` prefix.

Secrets reach the container only as its start environment. They are not in
the image, not in `wrangler.jsonc`, and not printed by the workflow.

## Trigger

Actions > "Cloud container deploy" > Run workflow. Inputs:

- `environment`: default `preview`. Deploys the Worker `modron-container-<environment>`.
- `confirm`: must be exactly `deploy`.

The run first builds the image from the repo root as a gate, then runs
`wrangler deploy` in `cloud/container`, then sets the runtime secrets. Nothing
runs on push.

## What gets created in the Cloudflare account

- A Worker `modron-container-<environment>` with a Durable Object class
  `ModronAgent` (SQLite) and a container image built from `Dockerfile`.
- Worker secrets listed above. Container image storage in the account's
  Cloudflare registry, which Wrangler manages.

## Costs and limits

Containers bill for running time and resources while awake, and the
Containers feature may need beta access on the account. `max_instances` is 5.
Check current Containers pricing before deploying.

## Remove it

    cd cloud/container
    npx wrangler@4.160.0 delete --name modron-container-<environment>

Then delete the repository secrets you no longer need. The image and any
Durable Object storage are removed with the Worker.

## Known gaps

- **`modron serve` does not dial the bridge by itself.** `--bridge` only sets
  the relay base for share and publish. The tunnel starts when a client sends
  a `remote_control` frame for a live session, and that needs a session plus
  a `.modron` project marker in the working directory. Nothing in this
  scaffold sends that frame, so a container started by the Worker is not yet
  reachable through the bridge. Needs a small in-container dialer or a
  protocol-level start; see `crates/modron-server/src/dispatch.rs`
  (`Payload::RemoteControl`).
- **Build context unverified.** The Dockerfile copies from the repo root, but
  `image: ./Dockerfile` in `wrangler.jsonc` may use `cloud/container/` as the
  context. The CI gate builds from the root, so Dockerfile errors show up there.
- **Containers API shape unverified live.** Written against the Containers
  docs as summarised; `start`, `running` and `setInactivityTimeout` options
  need checking on a real deploy.
- **Workspace coupling.** The image builds `modron-cli` and runs `modron serve`.
  The terminal UI crate it used to depend on (`modron-tui`) was removed on
  2026-11-16, and `modron-cli` no longer depends on it. The image does not
  build the terminal app (`modron-app-tui`), which it does not need.
