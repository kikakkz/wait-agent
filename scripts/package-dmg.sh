#!/usr/bin/env bash
set -euo pipefail

# Package waitagent binary into a .dmg for macOS distribution.
# Usage: ./scripts/package-dmg.sh <binary-path> <output-dmg> [version]

BINARY="${1:?missing binary path}"
OUTPUT_DMG="${2:?missing output dmg path}"
VERSION="${3:-0.1.0}"

if [[ ! -f "$BINARY" ]]; then
  echo "error: binary not found at $BINARY" >&2
  exit 1
fi

STAGING="$(mktemp -d)"
trap 'rm -rf "$STAGING"' EXIT

APP_NAME="WaitAgent"
APP_DIR="$STAGING/$APP_NAME.app"
mkdir -p "$APP_DIR/Contents/MacOS"
mkdir -p "$APP_DIR/Contents/Resources"

cp "$BINARY" "$APP_DIR/Contents/MacOS/waitagent"
chmod +x "$APP_DIR/Contents/MacOS/waitagent"

# Create Info.plist
cat > "$APP_DIR/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key>
  <string>waitagent</string>
  <key>CFBundleIdentifier</key>
  <string>com.waitagent.cli</string>
  <key>CFBundleName</key>
  <string>WaitAgent</string>
  <key>CFBundleVersion</key>
  <string>${VERSION}</string>
  <key>CFBundleShortVersionString</key>
  <string>${VERSION}</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
</dict>
</plist>
EOF

# Create a symlink to /Applications for drag-and-drop install
ln -s /Applications "$STAGING/Applications"

# Create read-write DMG first, then compress
RW_DMG="$STAGING/tmp.dmg"
hdiutil create -format UDRW -volname "$APP_NAME" \
  -fs HFS+ \
  -srcfolder "$STAGING" \
  "$RW_DMG"

# Convert to compressed read-only DMG
hdiutil convert "$RW_DMG" -format UDZO -o "$OUTPUT_DMG"

echo "created $OUTPUT_DMG"
