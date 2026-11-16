#!/bin/bash
# Container entrypoint: clone the repo, then exec `modron serve`.
# Secrets arrive only as container environment variables (set by the
# Worker from its secrets). Nothing here prints them.
set -eu

REPO="${GITHUB_REPO:?GITHUB_REPO is required (owner/name)}"
BRIDGE="${BRIDGE_URL:?BRIDGE_URL is required (wss://host)}"

# Reject anything that is not a plain owner/name before it reaches a URL.
if ! printf '%s' "$REPO" | grep -Eq '^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$'; then
  echo "entrypoint: GITHUB_REPO must look like owner/name" >&2
  exit 2
fi

# Clone into the workspace. The token goes to git through the environment
# (GIT_CONFIG_* as one extra header), not on the command line, so it does
# not appear in `ps` output, and it is never written to .git/config.
if [ -n "${GITHUB_TOKEN:-}" ]; then
  basic="$(printf 'x-access-token:%s' "$GITHUB_TOKEN" | base64 | tr -d '\n')"
  GIT_CONFIG_COUNT=1 \
  GIT_CONFIG_KEY_0=http.extraHeader \
  GIT_CONFIG_VALUE_0="Authorization: Basic ${basic}" \
    git clone --quiet "https://github.com/${REPO}.git" /workspace/repo
  unset basic
else
  git clone --quiet "https://github.com/${REPO}.git" /workspace/repo
fi

cd /workspace/repo

# Optional handoff import (C6c): HANDOFF_REF is a `modron/handoff/<id>` ref
# or a session id from `modron handoff export` on another host. The import
# fetches from origin, so the token is still set for it, then it is dropped.
if [ -n "${HANDOFF_REF:-}" ]; then
  if [ -n "${GITHUB_TOKEN:-}" ]; then
    basic="$(printf 'x-access-token:%s' "$GITHUB_TOKEN" | base64 | tr -d '\n')"
    GIT_CONFIG_COUNT=1 \
    GIT_CONFIG_KEY_0=http.extraHeader \
    GIT_CONFIG_VALUE_0="Authorization: Basic ${basic}" \
      modron handoff import "$HANDOFF_REF" --remote origin
    unset basic
  else
    modron handoff import "$HANDOFF_REF" --remote origin
  fi
fi

# The agent process must not inherit the token.
unset GITHUB_TOKEN

# On a stop signal, serve exports the session it serves for the next host
# (docs/HANDOFF.md, "Exit hand-off"). The git export pushes to origin, which
# needs credentials this script no longer holds (see cloud/container/README.md).
export MODRON_HANDOFF_ON_EXIT=1

# Loopback only: nothing listens on a public interface. --bridge sets the
# relay base for share/publish. --remote-control opens the tunnel at start,
# with no client attached, for the repo's session (see cloud/container/README.md).
exec modron serve \
  --listen 127.0.0.1:7699 \
  --bridge "$BRIDGE" \
  --remote-control "$BRIDGE"
