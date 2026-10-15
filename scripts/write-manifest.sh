#!/usr/bin/env bash
# Write the static channel manifest clients read for an already-packaged DMG.
#
# `package-macos-dmg.sh` stays manifest-free because it cannot know where the
# DMG will be hosted; this script bakes that host into the asset URL, so run it
# once the destination is known. `preview-feed.sh` and `just manifest` share it.
#
# Usage: write-manifest.sh <dest.json> <base-url> <dmg>
#
# Environment:
#   PILUS_MANIFEST_VERSION  Version the manifest advertises. Defaults to the
#                           workspace Cargo.toml version, which is what a plain
#                           `just package-dmg` stamps into the app.
#   PILUS_MANIFEST_NOTES    Notes line; defaults to the git revision and version.
#
# A `<dmg>.minisig` beside the DMG is referenced as the asset's `signature_url`.
set -euo pipefail

if [ "$#" -ne 3 ]; then
    echo "usage: $0 <dest.json> <base-url> <dmg>" >&2
    exit 2
fi
dest="$1"
base_url="${2%/}"
dmg="$3"

root="$(cd "$(dirname "$0")/.." && pwd)"

# `0.0.0.0` is what a server binds to, never a host a client can dial: on
# another machine it resolves to that machine's own loopback, so the check
# (which reads the feed URL) succeeds but the download (which reads this
# baked-in asset URL) fails with "connection refused".
case "$base_url" in
    *//0.0.0.0 | *//0.0.0.0[:/]*)
        echo "manifest: base URL '$base_url' uses 0.0.0.0, a bind address" >&2
        echo "          pass a reachable host, e.g. http://127.0.0.1:8000" >&2
        echo "          or http://<tailnet-host>:8000 for another machine" >&2
        exit 1
        ;;
esac

if [ ! -f "$dmg" ]; then
    echo "no $dmg; run \`just package-dmg\` first" >&2
    exit 1
fi

cargo_version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)"
version="${PILUS_MANIFEST_VERSION:-$cargo_version}"
name="$(basename "$dmg")"
sha256="$(shasum -a 256 "$dmg" | awk '{print $1}')"
revision="$(git -C "$root" rev-parse --short HEAD 2>/dev/null || echo local)"
notes="${PILUS_MANIFEST_NOTES:-pilus ${version} (${revision})}"

signature_url=""
if [ -f "$dmg.minisig" ]; then
    signature_url="$base_url/$name.minisig"
fi

mkdir -p "$(dirname "$dest")"

PILUS_MANIFEST_DEST="$dest" \
PILUS_MANIFEST_VERSION="$version" \
PILUS_MANIFEST_NOTES="$notes" \
PILUS_MANIFEST_NAME="$name" \
PILUS_MANIFEST_URL="$base_url/$name" \
PILUS_MANIFEST_SHA256="$sha256" \
PILUS_MANIFEST_SIGNATURE_URL="$signature_url" \
python3 - <<'PY'
import json
import os

asset = {
    "name": os.environ["PILUS_MANIFEST_NAME"],
    "url": os.environ["PILUS_MANIFEST_URL"],
    "sha256": os.environ["PILUS_MANIFEST_SHA256"],
    "os": "macos",
    "arch": "arm64",
    "format": "dmg",
}
if os.environ.get("PILUS_MANIFEST_SIGNATURE_URL"):
    asset["signature_url"] = os.environ["PILUS_MANIFEST_SIGNATURE_URL"]

manifest = {
    "schema_version": 1,
    "version": os.environ["PILUS_MANIFEST_VERSION"],
    "notes": os.environ["PILUS_MANIFEST_NOTES"],
    "assets": [asset],
}
with open(os.environ["PILUS_MANIFEST_DEST"], "w") as handle:
    json.dump(manifest, handle, indent=2)
    handle.write("\n")
PY

echo "wrote $dest (version $version, asset $base_url/$name)"
