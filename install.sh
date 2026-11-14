#!/usr/bin/env bash
# Modron installer. Prefers a GitHub release binary for your platform;
# falls back to building from source when no release matches.
set -euo pipefail

REPO="jbeck018/modron"
INSTALL_DIR="${MODRON_INSTALL_DIR:-$HOME/.local/bin}"

info() { printf '\033[1;34mmodron\033[0m %s\n' "$1"; }
fail() { printf '\033[1;31merror:\033[0m %s\n' "$1" >&2; exit 1; }

platform() {
  local os arch
  os="$(uname -s)"; arch="$(uname -m)"
  case "$os" in
    Darwin) os="apple-darwin" ;;
    Linux) os="unknown-linux-gnu" ;;
    *) fail "unsupported OS: $os" ;;
  esac
  case "$arch" in
    arm64|aarch64) arch="aarch64" ;;
    x86_64) arch="x86_64" ;;
    *) fail "unsupported architecture: $arch" ;;
  esac
  echo "$arch-$os"
}

TARGET="$(platform)"
VERSION="${MODRON_VERSION:-latest}"
if [ "$VERSION" = "latest" ]; then
  URL="https://github.com/$REPO/releases/latest/download/modron-$TARGET.tar.gz"
else
  URL="https://github.com/$REPO/releases/download/$VERSION/modron-$TARGET.tar.gz"
fi

info "installing modron ($TARGET) into $INSTALL_DIR"
mkdir -p "$INSTALL_DIR"

WORKDIR_DL="$(mktemp -d "${TMPDIR:-/tmp}/modron-install.XXXXXX")"
trap 'rm -rf "$WORKDIR_DL"' EXIT
ARCHIVE="modron-$TARGET.tar.gz"
TARBALL="$WORKDIR_DL/$ARCHIVE"

sha256_check() {
  # Verifies "<hash>  <file>" lines in $1 against files in the cwd.
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum -c "$1"
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 -c "$1"
  else
    fail "neither sha256sum nor shasum found - cannot verify the download"
  fi
}

# Try the release binary. Only "no release for this platform" (HTTP 404)
# or a missing curl falls through to the source build; a checksum
# mismatch or a broken archive is fatal - never install an unverified
# binary and never paper over a tampered one by building something else.
INSTALLED=0
if command -v curl >/dev/null 2>&1; then
  HTTP_CODE="$(curl -sSL -o "$TARBALL" -w '%{http_code}' "$URL")" || fail "download failed: $URL"
  case "$HTTP_CODE" in
    200)
      curl -fsSL -o "$TARBALL.sha256" "$URL.sha256" \
        || fail "release has no checksum file ($URL.sha256) - refusing to install an unverified binary"
      (cd "$WORKDIR_DL" && sha256_check "$ARCHIVE.sha256" >/dev/null) \
        || fail "checksum mismatch for $ARCHIVE - refusing to install"
      tar -xzf "$TARBALL" -C "$INSTALL_DIR" modron \
        || fail "release archive could not be unpacked"
      info "installed release binary (sha256 verified)"
      INSTALLED=1
      ;;
    404) info "no release binary for $TARGET ($VERSION)" ;;
    *) fail "unexpected HTTP $HTTP_CODE fetching $URL" ;;
  esac
else
  info "curl not found - cannot fetch a release binary"
fi

if [ "$INSTALLED" -ne 1 ]; then
  info "building from source (requires Rust 1.99+)"
  command -v cargo >/dev/null 2>&1 || fail "cargo not found - install rustup from https://rustup.rs"
  command -v git >/dev/null 2>&1 || fail "git not found - install git to build from source"
  SRC="$WORKDIR_DL/src"
  if [ "$VERSION" = "latest" ]; then
    git clone --depth 1 "https://github.com/$REPO.git" "$SRC"
  else
    git clone --depth 1 --branch "$VERSION" "https://github.com/$REPO.git" "$SRC"
  fi
  # -p modron-cli: the CLI only (the desktop crate needs GUI system libs).
  (cd "$SRC" && cargo build --release --locked -p modron-cli)
  cp "$SRC/target/release/modron" "$INSTALL_DIR/modron"
  info "built and installed from source"
fi

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) info "note: add $INSTALL_DIR to your PATH (export PATH=\"$INSTALL_DIR:\$PATH\")" ;;
esac
info "done - run: modron --help"
