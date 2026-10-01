#!/bin/bash
# Rebuilds assets/icon/AppIcon.icns (the app bundle's icon) and assets/icon/window-icon-512.png (the icon the running
# app sets itself) from assets/icon/source.png (needs Swift and iconutil, both part of macOS/Xcode).
set -euo pipefail
cd "$(dirname "$0")/.."
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
swift scripts/make-icon.swift assets/icon/source.png "$WORK/AppIcon.iconset"
iconutil -c icns "$WORK/AppIcon.iconset" -o assets/icon/AppIcon.icns
cp "$WORK/AppIcon.iconset/window-icon-512.png" assets/icon/window-icon-512.png
echo "assets/icon/AppIcon.icns assets/icon/window-icon-512.png"
