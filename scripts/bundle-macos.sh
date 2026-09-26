#!/usr/bin/env bash
# Assemble a macOS .app bundle around the release binary so Finder, the Dock,
# and Launchpad pick up the icon, name, and bundle identity.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

version="$(sed -n 's/^version = "\(.*\)"/\1/p' crates/l3s/Cargo.toml | head -1)"
app="target/release/l3s.app"

cargo build --release -p l3s

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/l3s "$app/Contents/MacOS/l3s"
cp crates/l3s/assets/l3s.icns "$app/Contents/Resources/l3s.icns"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>l3s</string>
    <key>CFBundleDisplayName</key><string>l3s</string>
    <key>CFBundleIdentifier</key><string>dev.l3s.app</string>
    <key>CFBundleExecutable</key><string>l3s</string>
    <key>CFBundleIconFile</key><string>l3s.icns</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>${version}</string>
    <key>CFBundleVersion</key><string>${version}</string>
    <key>LSMinimumSystemVersion</key><string>13.0</string>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

# Ad-hoc sign so Gatekeeper allows a locally built bundle to run.
codesign --force --sign - "$app" >/dev/null 2>&1 || true

echo "built $app"
