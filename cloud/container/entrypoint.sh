#!/bin/bash
# Container entrypoint (PLAN 10.10): clone the run's repo, then exec
# `modron serve` for that run. Every per-run value arrives as container
# environment, set by the container worker from the bridge's wake body:
#   MODRON_RUN_ID, MODRON_BRIDGE_URL, MODRON_HOST_TOKEN, MODRON_REPO,
#   MODRON_BRANCH (optional), MODRON_GIT_TOKEN, MODRON_HANDOFF_REF (optional).
# Nothing here prints them.
set -eu

REPO="${MODRON_REPO:?MODRON_REPO is required (owner/name)}"
BRIDGE="${MODRON_BRIDGE_URL:?MODRON_BRIDGE_URL is required (wss://host)}"
RUN_ID="${MODRON_RUN_ID:?MODRON_RUN_ID is required}"
BRANCH="${MODRON_BRANCH:-}"
HANDOFF="${MODRON_HANDOFF_REF:-}"
GIT_TOKEN="${MODRON_GIT_TOKEN:-}"

# Reject anything that is not a plain owner/name before it reaches a URL.
if ! printf '%s' "$REPO" | grep -Eq '^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$'; then
  echo "entrypoint: MODRON_REPO must look like owner/name" >&2
  exit 2
fi

# git_as_token runs one git command with the token as an HTTP header. The
# header is set through GIT_CONFIG_* inside a subshell, so it is exported to
# that one git only: never on a command line, never in .git/config, and never
# in the environment of `modron serve`.
git_as_token() {
  (
    if [ -n "$GIT_TOKEN" ]; then
      basic="$(printf 'x-access-token:%s' "$GIT_TOKEN" | base64 | tr -d '\n')"
      export GIT_CONFIG_COUNT=1
      export GIT_CONFIG_KEY_0=http.extraHeader
      export GIT_CONFIG_VALUE_0="Authorization: Basic ${basic}"
    fi
    git "$@"
  )
}

git_as_token clone --quiet "https://github.com/${REPO}.git" /workspace/repo
cd /workspace/repo

if [ -n "$BRANCH" ]; then
  git_as_token checkout --quiet "$BRANCH"
fi

# Optional handoff import (C6c): HANDOFF_REF is a `modron/handoff/<id>` ref
# or a session id from `modron handoff export` on another host. The import
# fetches from origin, so it runs with the same subshell header.
if [ -n "$HANDOFF" ]; then
  (
    if [ -n "$GIT_TOKEN" ]; then
      basic="$(printf 'x-access-token:%s' "$GIT_TOKEN" | base64 | tr -d '\n')"
      export GIT_CONFIG_COUNT=1
      export GIT_CONFIG_KEY_0=http.extraHeader
      export GIT_CONFIG_VALUE_0="Authorization: Basic ${basic}"
    fi
    modron handoff import "$HANDOFF" --remote origin
  )
fi

# On a stop signal, serve exports the session it serves for the next host
# (docs/HANDOFF.md, "Exit hand-off"). That push is the one use serve makes of
# MODRON_GIT_TOKEN: it stays in serve's environment, and modron scrubs it from
# every tool, hook and MCP child (crates/modron-core hooks.rs).
export MODRON_HANDOFF_ON_EXIT=1

# Provider keys come from the owner's bridge vault, not from this container's
# environment (PLAN 10.9): serve fetches them at start and after each reconnect.
export MODRON_PORTAL_CREDENTIALS=1
export MODRON_HOST_KIND=cloud
export MODRON_RUN_ID="$RUN_ID"

# Loopback only: nothing listens on a public interface. --bridge sets the
# relay base for share/publish. --remote-control opens the tunnel at start,
# with no client attached; --run-id makes the session it registers carry the
# run id, so the portal links the run and the session.
exec modron serve \
  --listen 127.0.0.1:7699 \
  --bridge "$BRIDGE" \
  --remote-control "$BRIDGE" \
  --run-id "$RUN_ID" \
  --portal-credentials \
  --handoff-on-exit
