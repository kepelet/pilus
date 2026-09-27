# OTA updates

`l3s` updates itself from GitHub Releases. The pieces:

| Piece | Where |
| --- | --- |
| Release source + policy | `crates/l3s/src/update.rs` |
| GPUI entity (check → download → stage) | `crates/l3s/src/gui/updater.rs` |
| Footer badge + `l3s → Check for Updates` | `crates/l3s/src/gui/mod.rs` |
| Update window (live status + Retry) | `crates/l3s/src/gui/update_window.rs` |
| Update prefs (channel, auto-check, feed override) | `crates/l3s/src/gui/settings.rs` |
| DMG packaging | `scripts/package-macos-dmg.sh` |
| Release automation | `.github/workflows/release.yml` |

The engine is `gpui-updater-core`: a blocking check → download → verify →
install pipeline. The `Updater` entity runs it on GPUI's background executor
and reports progress through `UpdateStatus`, so the GUI thread never blocks —
the same threading rule as the daemon connection. A staged update sets the
app's restart path to the new `.app` bundle; `Restart` relaunches into it. A
failed check or download opens the native Update window (with a Retry button)
instead of a footer pill; the window observes the entity, so retries and
progress repaint live.

`gpui-updater-pre` was evaluated and rejected: it needs Rust 1.98 while this
workspace pins 1.96. The local `Updater` mirrors its shape and can be swapped
out later. Velopack remains the upgrade path if installers, deltas, or Windows
support outgrow this setup.

## Shipping a release

1. Bump `version` in the workspace `Cargo.toml`.
2. Tag and push: `vX.Y.Z` for stable, `vX.Y.Z-beta.N` for the beta channel
   (published as a GitHub pre-release, which only the beta channel reads).
3. The workflow packages `l3s-v<version>-macos-arm64.dmg` (one `l3s.app` at
   the image root), writes `SHA256SUMS`, and attaches everything to the
   release. Apple Silicon only.

Asset names are load-bearing: the client matches on the `macos`, `arm64`,
and `.dmg` substrings. Don't rename them.

## Signing and trust

Three layers, weakest first:

1. **Checksum (today).** `SHA256SUMS` ships with every release and the client
   verifies the download against it. Tampering with the DMG without the sums
   file fails; tampering with both does not.
2. **Developer ID + notarization (before announcing auto-update).** Set
   `APPLE_DEVELOPER_IDENTITY` when packaging and `NOTARY_PROFILE` to submit
   and staple. Without this, Gatekeeper quarantines the swapped-in bundle and
   the update looks like malware.
3. **Minisign (recommended).** Generate once:
   `minisign -G -p l3s.pub -s l3s.key`. Store the secret key contents as the
   `MINISIGN_SECRET_KEY` repo secret, put the public key into
   `MINISIGN_PUBLIC_KEY` in `update.rs` — verification then flips from
   `BestEffort` to `Signature` automatically and unsigned releases are
   rejected.

## Channels and overrides

- Settings carry `update_channel` (`stable`/`beta`), `auto_check_updates`
  (check on launch), and `update_feed_url`.
- `L3S_UPDATE_FEED_URL` overrides the source with a static manifest (R2, S3,
  any HTTPS host) for staging or self-hosting. Manifest shape:

```json
{
  "schema_version": 1,
  "version": "0.2.0",
  "notes": "Highlights…",
  "assets": [
    {
      "name": "l3s-v0.2.0-macos-arm64.dmg",
      "url": "https://dl.example.com/l3s-v0.2.0-macos-arm64.dmg",
      "sha256": "<hex>",
      "os": "macos",
      "arch": "arm64",
      "format": "dmg"
    }
  ]
}
```

## Local testing

```sh
# Terminal 1: serve a manifest + DMG.
python3 -m http.server 8000 --directory /tmp/feed

# Terminal 2: check without opening a window.
L3S_UPDATE_FEED_URL=http://127.0.0.1:8000/latest.json \
  cargo run -p l3s --no-default-features -- --check-updates
```

Note: install swaps the running bundle, so only download-check locally unless
the test build is disposable.

## Daemon compatibility

The OTA feed carries no protocol gate by design. `PROTOCOL_VERSION` is pinned
to the released daemon, and the handshake already rejects a mismatch with a
readable error — a client that updated past its daemon simply fails to attach
until `luvus` is updated too. Keep the two release notes in sync and say which
daemon a client release needs.
