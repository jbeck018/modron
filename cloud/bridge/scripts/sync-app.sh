#!/bin/sh
# Copy the built web app bundle into the bridge's static assets.
# Usage (from anywhere): sh cloud/bridge/scripts/sync-app.sh
# Build the bundle first: cd app && nsh package --target web -o dist
set -eu

here=$(cd "$(dirname "$0")" && pwd)
bridge=$(dirname "$here")
src="$bridge/../../app/dist/Modron"
dst="$bridge/public/app"

if [ ! -f "$src/index.html" ]; then
  echo "sync-app: no web bundle at $src" >&2
  echo "sync-app: build it first: cd app && nsh package --target web -o dist" >&2
  exit 1
fi

rm -rf "$dst"
mkdir -p "$dst"
cp -R "$src/." "$dst/"

# Count files and bytes with POSIX tools only (wc prints a total line
# when given many files, so skip it).
find "$dst" -type f -exec wc -c {} + | awk -v dst="$dst" '$2 != "total" { n++; s += $1 } END { printf "sync-app: %d files, %d bytes -> %s\n", n, s, dst }'
