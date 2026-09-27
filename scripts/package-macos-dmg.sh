#!/usr/bin/env bash
# Package a signed, notarized macOS DMG for OTA distribution.
#
# The updater's macOS installer expects a `.dmg` containing a single `l3s.app`
# at its root; on download it mounts the image with `hdiutil`, copies the new
# bundle over the running one with `ditto` (preserving its signature), and
# relaunches via `open`. Asset names must stay
# `l3s-v<version>-macos-<arch>.dmg` — the client's GitHub source matches on
# the `macos`, `<arch>`, and `.dmg` substrings.
#
# Environment:
#   APPLE_DEVELOPER_IDENTITY  Developer ID used to sign (else ad-hoc).
#   NOTARY_PROFILE            notarytool keychain profile; when set, the DMG
#                             is submitted for notarization and stapled.
#   MINISIGN_SECRET_KEY       path to a minisign secret key; when set and the
#                             `minisign` tool is installed, each DMG gets a
#                             `<asset>.minisig` sidecar for signature checks.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
if [ "$(uname -m)" != "arm64" ] && [ "$(uname -m)" != "aarch64" ]; then
    echo "l3s ships Apple Silicon only; refusing on $(uname -m)" >&2
    exit 1
fi
arch="arm64"

./scripts/bundle-macos.sh

stage="target/release/dmg-stage"
dmg="target/release/l3s-v${version}-macos-${arch}.dmg"
rm -rf "$stage" "$dmg"
mkdir -p "$stage"
cp -R target/release/l3s.app "$stage/l3s.app"

hdiutil create \
    -volname "l3s" \
    -srcfolder "$stage" \
    -ov -format UDZO \
    "$dmg" >/dev/null

if [ -n "${NOTARY_PROFILE:-}" ]; then
    # Gatekeeper trusts the update only when the bundle is notarized.
    xcrun notarytool submit "$dmg" --keychain-profile "$NOTARY_PROFILE" --wait
    xcrun stapler staple "$dmg"
fi

# One `<hex>  <name>` line per asset, the format `shasum -a 256` produces and
# the client's GitHub source resolves for checksum verification.
(cd target/release && shasum -a 256 "$(basename "$dmg")" > SHA256SUMS)

if [ -n "${MINISIGN_SECRET_KEY:-}" ] && command -v minisign >/dev/null 2>&1; then
    # Detached signature per asset: the client resolves `<asset>.minisig`
    # from the same release for signature verification.
    minisign -S -s "$MINISIGN_SECRET_KEY" -m "$dmg"
fi

echo "packaged $dmg"
cat target/release/SHA256SUMS
