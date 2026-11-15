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
  BASE="https://github.com/$REPO/releases/latest/download"
else
  BASE="https://github.com/$REPO/releases/download/$VERSION"
fi
URL="$BASE/modron-$TARGET.tar.gz"
SUMS_URL="$BASE/SHA256SUMS"

info "installing modron ($TARGET) into $INSTALL_DIR"
mkdir -p "$INSTALL_DIR"

WORKDIR_DL="$(mktemp -d "${TMPDIR:-/tmp}/modron-install.XXXXXX")"
trap 'rm -rf "$WORKDIR_DL"' EXIT
ARCHIVE="modron-$TARGET.tar.gz"
TARBALL="$WORKDIR_DL/$ARCHIVE"

sha256_tool() {
  # Prints the name of an available SHA-256 tool, or fails.
  if command -v sha256sum >/dev/null 2>&1; then
    echo sha256sum
  elif command -v shasum >/dev/null 2>&1; then
    echo "shasum -a 256"
  else
    fail "neither sha256sum nor shasum found - cannot verify the download"
  fi
}

# Extracts the SHA256SUMS line for $ARCHIVE into $2. Fails unless every
# non-blank line is "<64 lowercase hex>  <name>" or "<64 lowercase hex> *<name>"
# and $ARCHIVE is listed exactly once. Other entries are fine.
pick_sum_line() {
  awk -v f="$ARCHIVE" '
    /^[ \t]*$/ { next }
    {
      h = substr($0, 1, 64); s = substr($0, 65, 2); n = substr($0, 67)
      if (length($0) < 67 || length(h) != 64 || h !~ /^[0-9a-f]+$/ || (s != "  " && s != " *") || n == "" || substr(n, 1, 1) == " ") { bad = 1; exit }
      if (n == f) { hit++; line = $0 }
    }
    END { if (bad || hit != 1) exit 1; print line }
  ' "$1" > "$2"
}

# Try the release binary. Only "no release for this platform" (HTTP 404)
# or a missing curl falls through to the source build; a checksum
# mismatch or a broken archive is fatal - never install an unverified
# binary and never paper over a tampered one by building something else.
INSTALLED=0
if command -v curl >/dev/null 2>&1; then
  HTTP_CODE="$(curl -sSL --proto '=https' --proto-redir '=https' -o "$TARBALL" -w '%{http_code}' "$URL")" || fail "download failed: $URL"
  case "$HTTP_CODE" in
    200)
      SHA_TOOL="$(sha256_tool)"
      curl -fsSL --proto '=https' --proto-redir '=https' -o "$WORKDIR_DL/SHA256SUMS" "$SUMS_URL" \
        || fail "release has no SHA256SUMS ($SUMS_URL) - refusing to install an unverified binary"
      pick_sum_line "$WORKDIR_DL/SHA256SUMS" "$WORKDIR_DL/$ARCHIVE.sha256" \
        || fail "SHA256SUMS is malformed or does not list $ARCHIVE exactly once - refusing to install"
      (cd "$WORKDIR_DL" && $SHA_TOOL -c "$ARCHIVE.sha256" >/dev/null) \
        || fail "checksum mismatch for $ARCHIVE - refusing to install"
      tar -xzf "$TARBALL" -C "$INSTALL_DIR" modron \
        || fail "release archive could not be unpacked"
      # The terminal app is in newer archives only; its absence is not an error.
      tar -xzf "$TARBALL" -C "$INSTALL_DIR" modron-app-tui 2>/dev/null || true
      info "installed release binary (sha256 verified against SHA256SUMS)"
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
