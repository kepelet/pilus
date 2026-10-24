#!/usr/bin/env bash
# Pin the Luvus release pilus installs on first run and caps daemon updates at.
#
# Rewrites crates/pilus/luvus.pin with the version and the SHA-256 of each
# platform archive. Every archive is downloaded and hashed here, and the hash
# must match the release's own `.sha256` sidecar, so a pin is never written
# from a sidecar alone. Run it after trying pilus against that Luvus release.
#
# Usage: pin-luvus.sh <version>      e.g. pin-luvus.sh 0.14.4
#
# Environment:
#   LUVUS_UPDATE_RELEASE_BASE  Download base (default: the Luvus GitHub releases).
set -euo pipefail

if [ "$#" -ne 1 ]; then
    echo "usage: $0 <version>" >&2
    exit 2
fi
version="${1#v}"
case "$version" in
    *[!0-9A-Za-z.-]* | "") echo "not a version: $1" >&2; exit 2 ;;
esac

root="$(cd "$(dirname "$0")/.." && pwd)"
pin="$root/crates/pilus/luvus.pin"
base="${LUVUS_UPDATE_RELEASE_BASE:-https://github.com/RizRiyz/luvus/releases/download}"
triples=(aarch64-apple-darwin x86_64-apple-darwin aarch64-unknown-linux-musl x86_64-unknown-linux-musl)

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

lines=""
for triple in "${triples[@]}"; do
    stem="luvus-v${version}-${triple}"
    echo "checking $stem" >&2
    curl -fsSL "$base/v$version/$stem.tar.gz" -o "$work/$stem.tar.gz"
    published="$(curl -fsSL "$base/v$version/$stem.sha256" | awk '{print tolower($1)}')"
    actual="$(shasum -a 256 "$work/$stem.tar.gz" | awk '{print $1}')"
    if [ "${#actual}" -ne 64 ] || [ "$actual" != "$published" ]; then
        echo "$stem: archive hashes to $actual but the sidecar says $published" >&2
        exit 1
    fi
    lines+="$triple $actual"$'\n'
done

{
    sed -n '/^#/p' "$pin"
    echo "version $version"
    printf '%s' "$lines"
} > "$work/luvus.pin"
mv "$work/luvus.pin" "$pin"
echo "pinned Luvus $version in ${pin#"$root"/}" >&2
