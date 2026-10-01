#!/bin/bash
# Builds Live Subtitle.app outside the source tree (iCloud-synced folders break codesign) and signs it.
#   scripts/bundle.sh [output-dir]      default output: ~/Applications
#   CODESIGN_IDENTITY   signing identity; use a stable one so the Screen Recording grant survives rebuilds
set -euo pipefail
cd "$(dirname "$0")/.."
OUT="${1:-$HOME/Applications}"
IDENTITY="${CODESIGN_IDENTITY:--}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

CARGO_TARGET_DIR="$WORK/target" cargo build --release
APP="$WORK/Live Subtitle.app"
mkdir -p "$APP/Contents/MacOS"
cp "$WORK/target/release/live-subtitle" "$APP/Contents/MacOS/live-subtitle"
mkdir -p "$APP/Contents/Resources"
cp assets/icon/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>local.live-subtitle</string>
<key>CFBundleName</key><string>Live Subtitle</string>
<key>CFBundleDisplayName</key><string>Live Subtitle</string>
<key>CFBundleExecutable</key><string>live-subtitle</string>
<key>CFBundleIconFile</key><string>AppIcon</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>0.1.0</string>
<key>LSMinimumSystemVersion</key><string>13.0</string>
<key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
codesign --force --sign "$IDENTITY" "$APP"
mkdir -p "$OUT"
rm -rf "$OUT/Live Subtitle.app"
ditto --noextattr --noqtn "$APP" "$OUT/Live Subtitle.app"
codesign --force --sign "$IDENTITY" "$OUT/Live Subtitle.app"
echo "$OUT/Live Subtitle.app"
