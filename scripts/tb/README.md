# Terminal-Bench (Harbor) for Modron

Adapted from jcode's benchmark pipeline (`scripts/run_terminal_bench_harbor.sh`,
`scripts/tb_compare.py`, `docs/TERMINAL_BENCH.md` in their repo). Their key
finding worth copying: the value is the PIPELINE (per-trial isolation, token
capture, regression gating vs a baseline harness), not any published score —
they have no aggregate TB number in-repo either.

## What this gives us

- `modron_harbor_agent.py` — Harbor agent adapter: uploads the release
  binary into the task container, runs it once per trial headless
  (`--yolo`, isolated HOME + sessions dir so trials never share memory),
  and reports input/output/cache tokens from the session transcript's
  Usage events.
- Same fairness rules as jcode's submission audit: fresh state per trial,
  no timeout multipliers, k >= 5 trials per task for any published number.

## Running

```sh
cargo build --release
pip install terminal-bench 2>/dev/null || pip install harbor-bench  # Harbor CLI
harbor datasets add terminal-bench@2.0
harbor run \
  --agent scripts/tb/modron_harbor_agent.py \
  --agent-args model=glm-5.3 \
  --env-var ZAI_API_KEY=$ZAI_API_KEY \
  --task regex-log --task largest-eigenval --task cancel-async-tasks \
  -n 5 --jobs-dir /tmp/modron-tb2
```

(Pilot first with `-n 1` on a single task; their validated trio above is
where to start.) Compare runs with any results.json aggregator; jcode's
`scripts/tb_compare.py` (micro-average of per-task rewards vs a baseline
TSV with regression flags) is the model to copy once we have two harness
columns to compare.

## Competitiveness checklist (from the jcode deep-dive)

Where they are ahead and what closes the gap:

1. Bench pipeline (this file) — done for Terminal-Bench.
2. Token economics (their biggest structural edge):
   - deterministic tool ordering + static/dynamic system-prompt split for
     prompt-cache hits — TODO in modron-core engine request building;
   - deferred MCP tool schemas (on-demand references) — TODO;
   - accept_large_output context guard (withhold oversized results until
     the model opts in) — TODO in reduce.rs.
3. Parallel tool calls: a `batch` tool (1-10 concurrent subcalls) plus a
   nudge after N sequential tool rounds — TODO in modron-tools.
4. Resource benches (71 MB / 3.3 MB-per-session daemon, 19 ms startup):
   NOT worth chasing — structurally requires their daemon/session model;
   modron is process-per-session by design.

Their benchmark inventory beyond TB (headless RAM, startup latency, TUI
vs Maki, take-home cycles, memory-retrieval IR) is in
`/tmp/jcode-research/scripts/bench_*` if we ever want resource parity.
