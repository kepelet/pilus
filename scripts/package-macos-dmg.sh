#!/usr/bin/env bash
# Package a signed, notarized macOS DMG for OTA distribution.
#
# The updater's macOS installer expects a `.dmg` containing a single `pilus.app`
# at its root; on download it mounts the image with `hdiutil`, copies the new
# bundle over the running one with `ditto` (preserving its signature), and
# relaunches via `open`. The window Finder shows (background photo, arrow,
# Applications link) is laid out by `dmg-settings.py`; extra root entries are
# fine as long as none is a second `*.app`. Asset names must stay
# `pilus-v<version>-macos-<arch>.dmg` — the client's GitHub source matches on
# the `macos`, `<arch>`, and `.dmg` substrings.
#
# Environment:
#   APPLE_DEVELOPER_IDENTITY  Developer ID used to sign (else ad-hoc).
#   PILUS_BUNDLE_ID           Forwarded to bundle-macos.sh; defaults there to the
#                             development identifier. Release and preview builds
#                             set the production one (`pilus.kepelet.com`).
#   NOTARY_PROFILE            notarytool keychain profile; when set, the DMG
#                             is submitted for notarization and stapled.
#   MINISIGN_SECRET_KEY       path to a minisign secret key; each DMG gets a
#                             `<asset>.minisig` sidecar for signature checks.
#   MINISIGN_SECRET_KEY_CONTENT
#                             the same key as text (both lines of the key
#                             file), for when it lives in an encrypted env
#                             file. Written to a 0600 temp file for the
#                             signing step and removed after. Wins over the
#                             path when both are set.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
if [ "$(uname -m)" != "arm64" ] && [ "$(uname -m)" != "aarch64" ]; then
    echo "pilus ships Apple Silicon only; refusing on $(uname -m)" >&2
    exit 1
fi
arch="arm64"

./scripts/bundle-macos.sh

dmg="target/release/pilus-v${version}-macos-${arch}.dmg"
# A signature left from an earlier build would sit beside this DMG and fail to
# verify, so it goes with the image it belonged to.
rm -f "$dmg" "$dmg.minisig"

# `dmgbuild` writes the Finder layout (.DS_Store) directly, so packaging needs
# no GUI session or AppleScript, unlike a Finder-driven DMG. The venv lives in
# target/ and is reused across runs.
venv="target/dmg-venv"
if [ ! -x "$venv/bin/dmgbuild" ]; then
    python3 -m venv "$venv"
    "$venv/bin/pip" install --quiet "dmgbuild==1.6.7"
fi

# A multi-representation TIFF lets Finder pick the 2x background on Retina.
background="target/release/dmg-background.tiff"
tiffutil -cathidpicheck \
    crates/pilus/assets/dmg/background.png \
    crates/pilus/assets/dmg/background@2x.png \
    -out "$background" 2>/dev/null

"$venv/bin/dmgbuild" -s scripts/dmg-settings.py \
    -D app=target/release/pilus.app \
    -D background="$background" \
    "pilus" "$dmg" >/dev/null

if [ -n "${APPLE_DEVELOPER_IDENTITY:-}" ]; then
    # Gatekeeper's install assessment (`spctl -t install`) rejects an unsigned
    # image even when its contents are notarized; sign it before submission so
    # the stapled ticket covers the signed image.
    codesign --force --timestamp --sign "$APPLE_DEVELOPER_IDENTITY" "$dmg"
fi

if [ -n "${NOTARY_PROFILE:-}" ]; then
    # Gatekeeper trusts the update only when the bundle is notarized.
    xcrun notarytool submit "$dmg" --keychain-profile "$NOTARY_PROFILE" --wait
    xcrun stapler staple "$dmg"
fi

# One `<hex>  <name>` line per asset, the format `shasum -a 256` produces and
# the client's GitHub source resolves for checksum verification.
(cd target/release && shasum -a 256 "$(basename "$dmg")" > SHA256SUMS)

if [ -n "${MINISIGN_SECRET_KEY_CONTENT:-}" ]; then
    MINISIGN_SECRET_KEY="$(mktemp -t pilus-minisign)"
    trap 'rm -f "$MINISIGN_SECRET_KEY"' EXIT
    chmod 600 "$MINISIGN_SECRET_KEY"
    printf '%s\n' "$MINISIGN_SECRET_KEY_CONTENT" > "$MINISIGN_SECRET_KEY"
fi

if [ -n "${MINISIGN_SECRET_KEY:-}" ]; then
    # The client rejects an unsigned download once it carries a public key, so
    # a requested signature that cannot be made must stop the build, not ship
    # a DMG nobody can install.
    if ! command -v minisign >/dev/null 2>&1; then
        echo "a minisign key is set but \`minisign\` is not installed" >&2
        exit 1
    fi
    # Detached signature per asset: the client resolves `<asset>.minisig`
    # from the same release for signature verification.
    minisign -S -s "$MINISIGN_SECRET_KEY" -m "$dmg"
fi

echo "packaged $dmg"
cat target/release/SHA256SUMS
