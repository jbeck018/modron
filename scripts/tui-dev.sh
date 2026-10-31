#!/usr/bin/env bash
# Hot-reload TUI dev loop: rebuilds on any source change, then launches the
# TUI. Exit the TUI (ctrl+c) to rebuild; ctrl+c again to stop the loop.
# Requires fswatch (brew install fswatch); falls back to manual rebuilds.
set -euo pipefail
cd "$(dirname "$0")/.."

PROVIDER="${1:-mock}"
shift || true

build() { cargo build --release 2>&1 | grep -E "^(error|warning: unused)" || true; }

build
while true; do
  ./target/release/modron tui --provider "$PROVIDER" --approve-all "$@" || true
  echo "── TUI exited — rebuilding on source changes (ctrl+c to stop) ──"
  if command -v fswatch >/dev/null; then
    fswatch -1 crates/*/src >/dev/null 2>&1 || true
  else
    read -rp "press enter to rebuild and relaunch…"
  fi
  build
done
