#!/usr/bin/env bash
# Hot-reload TUI dev loop: rebuilds on any source change, then launches the
# TUI. Exit the TUI (ctrl+c) to rebuild; ctrl+c again to stop the loop.
# Requires fswatch (brew install fswatch); falls back to manual rebuilds.
set -euo pipefail
cd "$(dirname "$0")/.."

PROVIDER="${1:-mock}"
shift || true

# Build, surfacing errors AND preserving cargo's exit status — a swallowed
# failure made the loop relaunch the stale previous binary as "current".
build() {
  local out status=0
  out="$(cargo build --release 2>&1)" || status=1
  printf '%s\n' "$out" | grep -E "^(error|warning: unused)" || true
  return "$status"
}

wait_for_change() {
  if command -v fswatch >/dev/null; then
    fswatch -1 crates/*/src >/dev/null 2>&1 || true
  else
    read -rp "press enter to rebuild and relaunch…"
  fi
}

# A failed build waits for the next source change and retries — the loop
# never launches a binary the last build did not produce.
until build; do
  echo "── build failed — fix the errors; waiting for a source change ──"
  wait_for_change
done

while true; do
  ./target/release/modron tui --provider "$PROVIDER" --approve-all "$@" || true
  echo "── TUI exited — rebuilding on source changes (ctrl+c to stop) ──"
  wait_for_change
  until build; do
    echo "── build failed — fix the errors; waiting for a source change ──"
    wait_for_change
  done
done
