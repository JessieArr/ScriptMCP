#!/usr/bin/env bash
# Assemble a Finder-launchable app without developer signing credentials.
set -euo pipefail

if [ "$#" -ne 3 ]; then
  echo "Usage: $0 <binary> <destination.app> <version>" >&2
  exit 2
fi

BINARY="$1"
APP="$2"
VERSION="$3"
# Only release-style numeric versions are embedded in the XML below.
if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "error: expected a numeric major.minor.patch version" >&2
  exit 2
fi

mkdir -p "$APP/Contents/MacOS"
cp "$BINARY" "$APP/Contents/MacOS/scriptmcp"
chmod 755 "$APP/Contents/MacOS/scriptmcp"

# Finder provides no terminal stdin. Explicitly select UI rather than letting
# the binary's automatic stdio-MCP detection choose the server.
cat > "$APP/Contents/MacOS/ScriptMCP" <<'SH'
#!/bin/sh
exec "$(dirname "$0")/scriptmcp" ui "$@"
SH
chmod 755 "$APP/Contents/MacOS/ScriptMCP"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>ScriptMCP</string>
    <key>CFBundleIdentifier</key>
    <string>com.jessiearr.scriptmcp</string>
    <key>CFBundleName</key>
    <string>ScriptMCP</string>
    <key>CFBundleDisplayName</key>
    <string>ScriptMCP</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>$VERSION</string>
    <key>CFBundleVersion</key>
    <string>$VERSION</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
PLIST
