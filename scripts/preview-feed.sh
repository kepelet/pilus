#!/usr/bin/env bash
# Build the preview release payload: the macOS DMG plus the static manifest a
# client reads when `L3S_UPDATE_FEED_URL` points at it (localhost by default).
#
# The same generated version is stamped into the app (via `L3S_BUILD_VERSION`)
# and written to the manifest, so About names the build and the preview channel
# reports "up to date" once that build is installed.
#
# Output: target/release/preview-feed/
#   l3s-v<version>-macos-arm64.dmg          the installable artifact
#   l3s-v<version>-macos-arm64.dmg.minisig  when MINISIGN_SECRET_KEY is set
#   latest.json                             the manifest clients fetch
#
# Environment:
#   L3S_PREVIEW_BASE_URL  URL prefix the manifest's asset URLs point at
#                         (default `http://127.0.0.1:8000`).
#   L3S_PREVIEW_VERSION   Version written to the manifest. Defaults to the
#                         next patch of the workspace Cargo.toml version as a
#                         timestamped pre-release (e.g. 0.1.1-preview.<utc>), so
#                         every payload is newer than the running client and
#                         nobody has to bump Cargo.toml for a preview.
#                         Set it explicitly to publish a real version instead.
#   L3S_PREVIEW_NOTES     Manifest notes line; defaults to the git revision
#                         and the client build version.
#   SKIP_PACKAGE=1        Reuse the DMG in target/release instead of running
#                         scripts/package-macos-dmg.sh first.
#   MINISIGN_SECRET_KEY   Forwarded to the packaging script; a `.minisig`
#                         sidecar is copied and referenced when present.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

if [ "$(uname -m)" != "arm64" ] && [ "$(uname -m)" != "aarch64" ]; then
    echo "l3s ships Apple Silicon only; refusing on $(uname -m)" >&2
    exit 1
fi
arch="arm64"

version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
dmg="l3s-v${version}-macos-${arch}.dmg"

# The manifest carries a synthetic next-patch version so a preview payload is
# always newer than the running client without touching Cargo.toml. The DMG
# itself stays the plain Cargo build; only the feed's version is virtual.
core="${version%%-*}"
core="${core%%+*}"
IFS=. read -r major minor patch <<<"$core"
patch="${patch:-0}"
stamp="$(date -u +%Y%m%d%H%M%S)"
manifest_version="${L3S_PREVIEW_VERSION:-${major}.${minor}.$((patch + 1))-preview.${stamp}}"
base_url="${L3S_PREVIEW_BASE_URL:-http://127.0.0.1:8000}"
base_url="${base_url%/}"

if [ "${SKIP_PACKAGE:-}" != "1" ]; then
    # Stamp the synthetic version into the build so the bundled app reports it.
    L3S_BUILD_VERSION="$manifest_version" ./scripts/package-macos-dmg.sh
else
    echo "note: reusing an existing DMG; its embedded build version may differ" >&2
fi

if [ ! -f "target/release/$dmg" ]; then
    echo "no target/release/$dmg; run \`just package-dmg\` first" >&2
    exit 1
fi

feed_dir="target/release/preview-feed"
rm -rf "$feed_dir"
mkdir -p "$feed_dir"

# Clone when the filesystem supports it (APFS), else copy.
cp -c "target/release/$dmg" "$feed_dir/$dmg" 2>/dev/null || cp "target/release/$dmg" "$feed_dir/$dmg"
sha256="$(shasum -a 256 "$feed_dir/$dmg" | awk '{print $1}')"

signature_url=""
if [ -f "target/release/$dmg.minisig" ]; then
    cp "target/release/$dmg.minisig" "$feed_dir/$dmg.minisig"
    signature_url="$base_url/$dmg.minisig"
fi

revision="$(git rev-parse --short HEAD 2>/dev/null || echo local)"
notes="${L3S_PREVIEW_NOTES:-Preview build ${revision} (l3s ${version})}"

L3S_MANIFEST_DEST="$feed_dir/latest.json" \
L3S_MANIFEST_VERSION="$manifest_version" \
L3S_MANIFEST_NOTES="$notes" \
L3S_MANIFEST_NAME="$dmg" \
L3S_MANIFEST_URL="$base_url/$dmg" \
L3S_MANIFEST_SHA256="$sha256" \
L3S_MANIFEST_SIGNATURE_URL="$signature_url" \
python3 - <<'PY'
import json
import os

asset = {
    "name": os.environ["L3S_MANIFEST_NAME"],
    "url": os.environ["L3S_MANIFEST_URL"],
    "sha256": os.environ["L3S_MANIFEST_SHA256"],
    "os": "macos",
    "arch": "arm64",
    "format": "dmg",
}
if os.environ.get("L3S_MANIFEST_SIGNATURE_URL"):
    asset["signature_url"] = os.environ["L3S_MANIFEST_SIGNATURE_URL"]

manifest = {
    "schema_version": 1,
    "version": os.environ["L3S_MANIFEST_VERSION"],
    "notes": os.environ["L3S_MANIFEST_NOTES"],
    "assets": [asset],
}
with open(os.environ["L3S_MANIFEST_DEST"], "w") as handle:
    json.dump(manifest, handle, indent=2)
    handle.write("\n")
PY

echo "manifest version: $manifest_version (client build $version)"
echo "preview payload: $feed_dir"
ls -lh "$feed_dir" | tail -n +2
echo
echo "serve it:  just serve-preview"
echo "check it:  just check-updates --feed-url $base_url/latest.json"
echo "use it:    L3S_UPDATE_FEED_URL=$base_url/latest.json just run"
