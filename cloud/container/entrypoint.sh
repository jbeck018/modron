#!/bin/bash
set -eu

REPO="${GITHUB_REPO:?GITHUB_REPO is required}"
BRIDGE="${BRIDGE_URL:?BRIDGE_URL is required (wss://host)}"

# Clone the repo into the workspace
if [ -n "${GITHUB_TOKEN:-}" ]; then
  git clone "https://x-access-token:${TOKEN}@github.com/${REPO}.git" /workspace/repo
else
  git clone "https://github.com/${REPO}.git" /workspace/repo
fi
cd /workspace/repo

# Start modron serve. The --bridge flag registers the tunnel so remote
# clients can open /s/<key> to watch and drive this agent.
exec modron serve \
  --listen 0.0.0.0:7699 \
  --bridge "$BRIDGE" \
  2>&1
