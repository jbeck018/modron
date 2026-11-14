#!/bin/bash
set -eu

REPO="${GITHUB_REPO:?GITHUB_REPO is required}"
BRIDGE="${BRIDGE_URL:?BRIDGE_URL is required (wss://host)}"

# Clone the repo into the workspace. The token is passed as a one-shot
# header so it is never written to /workspace/repo/.git/config, where the
# agent's own tools could read it.
if [ -n "${GITHUB_TOKEN:-}" ]; then
  basic="$(printf 'x-access-token:%s' "$GITHUB_TOKEN" | base64 | tr -d '\n')"
  git -c "http.extraHeader=Authorization: Basic ${basic}" \
    clone "https://github.com/${REPO}.git" /workspace/repo
  unset basic
else
  git clone "https://github.com/${REPO}.git" /workspace/repo
fi
# The agent process must not inherit the token.
unset GITHUB_TOKEN
cd /workspace/repo

# Start modron serve on loopback only. --bridge sets the relay base used
# by share/publish; it does NOT open a tunnel by itself (a client sends
# /remote-control for that).
exec modron serve \
  --listen 127.0.0.1:7699 \
  --bridge "$BRIDGE" \
  2>&1
