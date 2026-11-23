#!/usr/bin/env bash
# Run the same terminal-bench tasks with modron and omp on one pinned model
# and print a side-by-side table. See README.md ("Agent comparison").
#
#   scripts/tb/compare.sh <dataset tasks dir> <output dir> [task ...]
#
# Env: MODEL (glm-5.3), ATTEMPTS (2), CONCURRENT (2, per agent),
#      AGENTS ("modron omp"), RELAY_PORT (8787), RELAY_API_KEY, RELAY_UPSTREAM,
#      MODRON_TB_BINARY, BENCH_OMP_BUNDLE, BENCH_CA_BUNDLE.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
dataset="${1:?dataset tasks dir}"
out="${2:?output dir}"
shift 2
model="${MODEL:-glm-5.3}"
attempts="${ATTEMPTS:-2}"
concurrent="${CONCURRENT:-2}"
agents="${AGENTS:-modron omp}"
port="${RELAY_PORT:-8787}"
mkdir -p "$out"

if [ "$#" -gt 0 ]; then
  tasks=("$@")
else
  mapfile -t tasks < <(grep -v '^#' "$here/compare-tasks.txt" | grep -v '^$')
fi

export MODRON_TB_BINARY="${MODRON_TB_BINARY:-$repo/target/x86_64-unknown-linux-musl/release/modron}"
export BENCH_OMP_BUNDLE="${BENCH_OMP_BUNDLE:-$out/omp-bundle.tar}"
export BENCH_RELAY_HOST_URL="http://127.0.0.1:$port"
export PYTHONPATH="$here${PYTHONPATH:+:$PYTHONPATH}"

case " $agents " in *" omp "*)
  if [ ! -f "$BENCH_OMP_BUNDLE" ]; then
    # bun plus the global install tree holding @oh-my-pi/pi-coding-agent
    # (bun install -g @oh-my-pi/pi-coding-agent).
    stage="$(mktemp -d)"
    mkdir -p "$stage/bin"
    cp "$(readlink -f "$(command -v bun)")" "$stage/bin/bun"
    cp -a "${BUN_INSTALL:-$HOME/.bun}/install/global/node_modules" "$stage/node_modules"
    tar -cf "$BENCH_OMP_BUNDLE" -C "$stage" bin node_modules
    rm -rf "$stage"
  fi ;;
esac

if ! curl -fsS "$BENCH_RELAY_HOST_URL/stats" >/dev/null 2>&1; then
  RELAY_PORT="$port" RELAY_STATS="$out/relay-stats.json" \
    nohup bun "$here/relay.mjs" >"$out/relay.log" 2>&1 &
  sleep 2
fi

task_args=()
for task in "${tasks[@]}"; do task_args+=(--task-id "$task"); done

pids=()
for agent in $agents; do
  case "$agent" in
    modron) class=ModronAgent ;;
    omp) class=OmpAgent ;;
    *) echo "unknown agent: $agent" >&2; exit 2 ;;
  esac
  # Run ids must be lowercase: they become docker compose project names.
  tb run --dataset-path "$dataset" "${task_args[@]}" \
    --agent-import-path "bench_agents:$class" \
    --agent-kwarg "model_name=$model" \
    --n-attempts "$attempts" --n-concurrent "$concurrent" \
    --output-path "$out" --run-id "$agent" >"$out/$agent.log" 2>&1 &
  pids+=("$!")
done
status=0
for pid in "${pids[@]}"; do wait "$pid" || status=1; done

python3 "$here/compare.py" "$out" $agents
exit "$status"
