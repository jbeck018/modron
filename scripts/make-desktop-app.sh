#!/usr/bin/env bash
# Assemble Modron.app from the release desktop binary: a minimal macOS
# bundle with the iron-rain app icon, so the Dock shows the brand instead
# of a generic executable. Run after `cargo build --release -p modron-desktop`.
set -euo pipefail

cd "$(dirname "$0")/.."

BIN="target/release/modron-desktop"
APP="target/release/Modron.app"
ICNS="crates/modron-desktop/assets/icon/Modron.icns"

[ -x "$BIN" ] || { echo "missing $BIN - build it first: cargo build --release -p modron-desktop" >&2; exit 1; }
[ -f "$ICNS" ] || { echo "missing $ICNS" >&2; exit 1; }

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/modron-desktop"
cp "$ICNS" "$APP/Contents/Resources/Modron.icns"
cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>Modron</string>
    <key>CFBundleDisplayName</key>
    <string>Modron</string>
    <key>CFBundleExecutable</key>
    <string>modron-desktop</string>
    <key>CFBundleIdentifier</key>
    <string>dev.modron.desktop</string>
    <key>CFBundleVersion</key>
    <string>0.1.2</string>
    <key>CFBundleShortVersionString</key>
    <string>0.1.2</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleIconFile</key>
    <string>Modron</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.developer-tools</string>
</dict>
</plist>
PLIST
echo "assembled $APP"
