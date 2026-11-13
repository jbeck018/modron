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

TARBALL="$(mktemp "${TMPDIR:-/tmp}/modron.tar.gz.XXXXXX")"
trap 'rm -f "$TARBALL"' EXIT

# Try the release binary; any failure along the way (no curl, 404, or a
# tar that cannot unpack it) falls through to the source build.
INSTALLED=0
if command -v curl >/dev/null 2>&1 && curl -fsSL -o "$TARBALL" "$URL" 2>/dev/null; then
  if tar -xzf "$TARBALL" -C "$INSTALL_DIR" modron 2>/dev/null; then
    rm -f "$TARBALL"
    info "installed release binary"
    INSTALLED=1
  else
    info "release archive could not be unpacked"
  fi
fi

if [ "$INSTALLED" -ne 1 ]; then
  info "no usable release binary — building from source (requires Rust 1.99+)"
  command -v cargo >/dev/null 2>&1 || fail "cargo not found — install rustup from https://rustup.rs"
  command -v git >/dev/null 2>&1 || fail "git not found — install git to build from source"
  TMPDIR_BUILD="$(mktemp -d)"
  git clone --depth 1 "https://github.com/$REPO.git" "$TMPDIR_BUILD/modron"
  (cd "$TMPDIR_BUILD/modron" && cargo build --release)
  cp "$TMPDIR_BUILD/modron/target/release/modron" "$INSTALL_DIR/modron"
  rm -rf "$TMPDIR_BUILD"
  info "built and installed from source"
fi

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) info "note: add $INSTALL_DIR to your PATH (export PATH=\"$INSTALL_DIR:\$PATH\")" ;;
esac
info "done — run: modron --help"
