# Terminal-Bench for Modron

Adapted from jcode's benchmark pipeline (their `run_terminal_bench_harbor.sh`,
`tb_compare.py`, `docs/TERMINAL_BENCH.md`). Their key finding worth copying:
the value is the PIPELINE (per-trial isolation, token capture, regression
gating vs a baseline harness), not any published score - they have no
aggregate TB number in-repo either.

## What this gives us

- `modron_tb_agent.py` - a terminal-bench (`tb`) agent adapter: uploads the
  musl-static Linux binary into the task container, runs it once per trial
  headless (`--yolo`, isolated HOME + sessions dir so trials never share
  memory), polls an exit marker instead of tmux prompt detection, and
  reports input/output tokens from the session transcript's Usage events.
- Same fairness rules as jcode's submission audit: fresh state per trial,
  no timeout multipliers, k >= 5 trials per task for any published number.

## Running (pilot recipe, verified 2026-11-04)

```sh
# 1. Linux binary for task containers (musl static - any glibc):
docker run --platform linux/amd64 -v "$PWD":/repo -w /repo rust:1.99 sh -c \
  'apt-get update -qq && apt-get install -y musl-tools && \
   rustup target add x86_64-unknown-linux-musl && \
   CARGO_TARGET_DIR=/repo/target-linux-musl cargo build --release \
   -p modron-cli --target x86_64-unknown-linux-musl'

# 2. Harness (PyPI has no harbor-bench; the harness CLI is `tb`):
uv tool install terminal-bench

# 3. Dataset: the registry `head` entry points at main ./tasks, which no
#    longer exists upstream (that bug also breaks `tb datasets download`);
#    use the v0.2.x branch (94 tasks incl. hello-world) via sparse clone:
git clone --depth 1 --filter=blob:none --sparse \
  https://github.com/laude-institute/terminal-bench /tmp/tb-tasks
cd /tmp/tb-tasks && git fetch --depth 1 origin dataset/terminal-bench-core/v0.2.x \
  && git checkout FETCH_HEAD && git sparse-checkout disable

# 4. Run (coding-plan keys MUST use the coding base URL):
cd /path/to/modron
source ~/.zshrc   # ZAI_API_KEY
export ZAI_BASE_URL=https://api.z.ai/api/coding/paas/v4
export MODRON_TB_BINARY=$PWD/target-linux-musl/x86_64-unknown-linux-musl/release/modron
PYTHONPATH=scripts/tb tb run \
  --dataset-path /tmp/tb-tasks/tasks \
  --task-id hello-world \
  --agent-import-path modron_tb_agent:ModronTbAgent \
  --agent-kwarg model_name=glm-5.3 \
  --n-attempts 1 --n-concurrent 1
```

PILOT STATUS 2026-11-04: the pipeline is verified end to end (binary
upload, tmux single-send exec, modron run, usage plumbing). The final
model call returned Z.ai error 1309 - the GLM Coding Plan subscription
EXPIRED. Renew at https://z.ai/subscribe (or export ANTHROPIC_API_KEY
and pass `--agent-kwarg model_name=claude-sonnet-5-5`), rerun, then
scale to the jcode TB-2.0 trio (regex-log, largest-eigenval,
cancel-async-tasks) once the registry repopulates a 2.0 dataset.

Notes:
- `runs/` holds trial artifacts (commands.txt records the typed env line
  WITH the API key) - gitignored on purpose, never commit it.
- `tb run` results land in `runs/<timestamp>/results.json` per trial
  (`is_resolved`, `failure_mode`, token totals).
