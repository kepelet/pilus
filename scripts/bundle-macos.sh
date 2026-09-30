#!/usr/bin/env bash
# Assemble a macOS .app bundle around the release binary so Finder, the Dock,
# and Launchpad pick up the icon, name, and bundle identity.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
# `preview-feed.sh` overrides this with its synthetic preview version so the
# bundle's Info.plist matches the version the binary reports.
app_version="${PILUS_BUILD_VERSION:-$version}"
app="target/release/pilus.app"

cargo build --release -p pilus -p pilus-agent

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/pilus "$app/Contents/MacOS/pilus"
# The host connector travels with the app so "Enable this node" can find it
# beside the main binary at runtime.
cp target/release/pilus-agent "$app/Contents/MacOS/pilus-agent"
cp crates/pilus/assets/pilus.icns "$app/Contents/Resources/pilus.icns"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>pilus</string>
    <key>CFBundleDisplayName</key><string>pilus</string>
    <key>CFBundleIdentifier</key><string>dev.pilus.app</string>
    <key>CFBundleExecutable</key><string>pilus</string>
    <key>CFBundleIconFile</key><string>pilus.icns</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>${app_version}</string>
    <key>CFBundleVersion</key><string>${app_version}</string>
    <key>LSMinimumSystemVersion</key><string>13.0</string>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

# Ad-hoc sign so Gatekeeper allows a locally built bundle to run. Set
# APPLE_DEVELOPER_IDENTITY to a Developer ID for distributable builds: OTA
# updates only survive Gatekeeper when the bundle carries that identity with
# the hardened runtime. A requested identity must succeed — silently shipping
# ad-hoc would produce an update Gatekeeper rejects, so that path fails loudly.
identity="${APPLE_DEVELOPER_IDENTITY:--}"
# Sign the helper first: notarization rejects a Developer ID app whose nested
# executable is only linker-signed, and the hardened runtime must be on it too.
codesign --force --options runtime --sign "$identity" "$app/Contents/MacOS/pilus-agent"
if [ -n "$identity" ]; then
    codesign --force --options runtime --sign "$identity" "$app"
else
    codesign --force --sign - "$app" >/dev/null 2>&1 || true
fi

echo "built $app"
