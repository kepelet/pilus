#!/usr/bin/env bash
# Upload a packaged release to the releases bucket, so clients on that channel
# pick it up from releases.kepelet.com.
#
# Layout in the bucket (the bucket is the one `infra/alchemy.run.ts` declares):
#   pilus/stable.json   pilus/pilus-v<version>-macos-arm64.dmg[.minisig]
#   pilus/preview.json  pilus/preview/pilus-v<version>-macos-arm64.dmg[.minisig]
#
# Preview DMGs live under `preview/` because the filename carries only the
# Cargo version: beside the stable DMG it would overwrite the stable download
# for the same version. The manifest goes up last, so a client never reads a
# manifest whose DMG is not there yet.
#
# Usage: publish-release.sh <stable|preview>
#   stable   target/release/pilus-v<Cargo version>-macos-arm64.dmg plus the
#            target/release/stable.json that `just manifest stable` wrote.
#   preview  target/release/preview-feed/, as `just preview-feed` built it.
#            Its `latest.json` must carry the preview URL prefix below;
#            `just release-preview` does both.
#
# A stable DMG is immutable: clients verify it against the sha256 in a
# manifest, so replacing a published version would fail every client that has
# already read the old one. The script refuses when that version is live; bump
# `version` in Cargo.toml instead.
#
# Environment:
#   PILUS_RELEASES_BUCKET  R2 bucket name. Falls back to infra/.env (the file
#                          the Alchemy stack reads), decrypted with dotenvx.
#   PILUS_ALLOW_DIRTY=1    Publish stable from a tree with uncommitted changes.
#   LOCAL=true             Preview only: skip the minisign presence and
#                          signature checks. A client only installs the result
#                          when it is launched with LOCAL=true too.
#
# Auth is wrangler's own (`bunx wrangler login`), which is separate from the
# Alchemy profile.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

channel="${1:-}"
case "$channel" in
    stable | preview) ;;
    *) echo "usage: $0 <stable|preview>" >&2; exit 2 ;;
esac

public="https://releases.kepelet.com/pilus"

# infra/.env is dotenvx-encrypted, so a plain read would return ciphertext.
if [ -z "${PILUS_RELEASES_BUCKET:-}" ] && [ -f infra/.env ] && command -v dotenvx >/dev/null 2>&1; then
    PILUS_RELEASES_BUCKET="$(dotenvx get PILUS_RELEASES_BUCKET -f infra/.env 2>/dev/null || true)"
fi
if [ -z "${PILUS_RELEASES_BUCKET:-}" ]; then
    echo "publish-release: set PILUS_RELEASES_BUCKET (or put it in infra/.env; needs dotenvx and infra/.env.keys)" >&2
    exit 1
fi
bucket="$PILUS_RELEASES_BUCKET"

version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
if [ "$channel" = stable ]; then
    src_dir="target/release"
    manifest="target/release/stable.json"
    key_prefix="pilus"
    asset_base="$public"
    manifest_key="pilus/stable.json"
else
    src_dir="target/release/preview-feed"
    manifest="$src_dir/latest.json"
    key_prefix="pilus/preview"
    asset_base="$public/preview"
    manifest_key="pilus/preview.json"
fi

if [ ! -f "$manifest" ]; then
    echo "publish-release: no $manifest; build the $channel payload first" >&2
    exit 1
fi

# The asset URLs are baked into the manifest; shipping one that points at
# localhost would make every client's check succeed and its download fail.
if ! grep -q "\"url\": \"$asset_base/" "$manifest"; then
    echo "publish-release: $manifest does not point at $asset_base/" >&2
    exit 1
fi

if [ "$channel" = stable ]; then
    dmg="pilus-v${version}-macos-arm64.dmg"
    files=("$src_dir/$dmg")
    [ -f "$src_dir/$dmg.minisig" ] && files+=("$src_dir/$dmg.minisig")
    if [ ! -f "$src_dir/$dmg" ]; then
        echo "publish-release: no $src_dir/$dmg; run \`just package-dmg\` first" >&2
        exit 1
    fi
    # The manifest's checksum must be the DMG that is about to go up, or every
    # client rejects the download.
    want="$(shasum -a 256 "$src_dir/$dmg" | awk '{print $1}')"
    if ! grep -q "\"sha256\": \"$want\"" "$manifest"; then
        echo "publish-release: $manifest does not match the checksum of $dmg" >&2
        echo "                 rewrite it with \`just manifest stable $public\`" >&2
        exit 1
    fi
    if [ "$(sed -n 's/.*"version": "\(.*\)",/\1/p' "$manifest" | head -1)" != "$version" ]; then
        echo "publish-release: $manifest advertises a version other than $version" >&2
        exit 1
    fi
    if [ -n "$(git status --porcelain)" ] && [ "${PILUS_ALLOW_DIRTY:-}" != 1 ]; then
        echo "publish-release: working tree is dirty; commit first or set PILUS_ALLOW_DIRTY=1" >&2
        exit 1
    fi
    if curl -fsI "$public/$dmg" >/dev/null 2>&1; then
        echo "publish-release: $public/$dmg is already published" >&2
        echo "                 a stable version is immutable; bump version in Cargo.toml" >&2
        exit 1
    fi
else
    shopt -s nullglob
    files=("$src_dir"/*.dmg "$src_dir"/*.minisig)
fi

# A client built with a minisign public key rejects any download without a
# signature, so an unsigned upload would be a release nobody can install.
# Packaging skips signing without a word when the key or tool is missing.
#
# `LOCAL=true` waives the gate for a preview, so a machine without the secret
# key or `minisign` can still publish. Stable never takes the waiver: a stable
# DMG is immutable, so an unsigned one could not be replaced.
skip_signature=0
if [ "${LOCAL:-}" = "true" ] && [ "$channel" = preview ]; then
    skip_signature=1
    echo "publish-release: LOCAL=true, skipping the minisign check for this preview" >&2
fi
if [ "$skip_signature" = 0 ] && ! grep -q 'MINISIGN_PUBLIC_KEY: Option<&str> = None' crates/pilus/src/update.rs; then
    for file in "${files[@]}"; do
        case "$file" in *.dmg) [ -f "$file.minisig" ] || {
            echo "publish-release: $file has no .minisig, but the client requires one" >&2
            echo "                 set MINISIGN_SECRET_KEY and package again" >&2
            exit 1
        } ;; esac
    done
    # Verify against the key the client carries, not just that a file exists:
    # a signature from another build or key is a release nobody can install.
    pubkey="$(sed -n 's/.*MINISIGN_PUBLIC_KEY: Option<&str> =.*Some("\(.*\)").*/\1/p; s/^ *Some("\(RW[^"]*\)");/\1/p' crates/pilus/src/update.rs | head -1)"
    if [ -z "$pubkey" ] || ! command -v minisign >/dev/null 2>&1; then
        echo "publish-release: need minisign and a readable public key in update.rs to verify" >&2
        exit 1
    fi
    for file in "${files[@]}"; do
        case "$file" in *.dmg)
            minisign -V -P "$pubkey" -m "$file" -x "$file.minisig" >/dev/null || {
                echo "publish-release: $file.minisig does not verify against the client's key" >&2
                exit 1
            } ;; esac
    done
    if ! grep -q '"signature_url"' "$manifest"; then
        echo "publish-release: $manifest has no signature_url; rewrite it after signing" >&2
        exit 1
    fi
fi

put() {
    # `--remote`: wrangler's R2 object commands can default to a local
    # simulator, which would "succeed" without uploading anything.
    (cd infra && bunx wrangler r2 object put "$bucket/$1" --remote \
        --file "$root/$2" --content-type "$3" --cache-control "$4")
}

for file in "${files[@]}"; do
    name="$(basename "$file")"
    type="application/x-apple-diskimage"
    case "$name" in *.minisig) type="text/plain" ;; esac
    echo "uploading $key_prefix/$name"
    put "$key_prefix/$name" "$file" "$type" "public, max-age=300"
done

echo "uploading $manifest_key"
put "$manifest_key" "$manifest" "application/json" "no-cache"

# Read it back through the public hostname, the way a client will.
live="$(curl -fs "$public/$(basename "$manifest_key")" || true)"
live_version="$(printf '%s' "$live" | sed -n 's/.*"version": "\(.*\)",/\1/p' | head -1)"
want_version="$(sed -n 's/.*"version": "\(.*\)",/\1/p' "$manifest" | head -1)"
echo
if [ "$live_version" = "$want_version" ]; then
    echo "published $channel $want_version: $public/$(basename "$manifest_key")"
else
    echo "uploaded, but $public/$(basename "$manifest_key") reports '${live_version:-nothing}', not $want_version" >&2
    echo "(a cached copy may take a moment; re-check with curl)" >&2
fi
echo "check: just check-updates --channel $channel"
