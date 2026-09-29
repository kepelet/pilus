# OTA updates

`l3s` updates itself from channel manifests on `releases.kepelet.com`: stable
at `stable.json`, preview/nightly at `preview.json`. A local payload can be
tested by pointing the feed URL at `http://127.0.0.1:8000/latest.json`.

| Piece | Where |
| --- | --- |
| Release source + channel policy | `crates/l3s/src/update.rs` |
| Settings (channel, feed URL, auto-check) | `crates/l3s/src/settings.rs` |
| GPUI entity (check → download → stage) | `crates/l3s/src/gui/updater.rs` |
| Build version stamp | `crates/l3s/build.rs` |
| Footer badge + `l3s → Check for Updates` | `crates/l3s/src/gui/mod.rs` |
| Settings page (channel, feed URL, auto-check) | `crates/l3s/src/gui/mod.rs` |
| Update window (live status + Retry) | `crates/l3s/src/gui/update_window.rs` |
| DMG packaging | `scripts/package-macos-dmg.sh` |
| Preview payload (DMG + manifest) | `scripts/preview-feed.sh` |
| Release automation | `.github/workflows/release.yml` |

The engine is `gpui-updater-core`: a blocking check → download → verify →
install pipeline. The `Updater` entity runs it on GPUI's background executor
and reports progress through `UpdateStatus`, so the GUI thread never blocks —
the same threading rule as the daemon connection. Changing the channel or feed
URL rebuilds the entity's engine in place, so the Update window's observation
and the footer badge stay live. A staged update sets the app's restart path to
the new `.app` bundle; `Restart` relaunches into it. A failed check or
download opens the native Update window (with a Retry button) instead of a
footer pill.

`gpui-updater-pre` was evaluated and rejected: it needs Rust 1.98 while this
workspace pins 1.96. The local `Updater` mirrors its shape and can be swapped
out later. Velopack remains the upgrade path if installers, deltas, or Windows
support outgrow this setup.

## Channels and configuration

| Channel | Default manifest | Aliases |
| --- | --- | --- |
| `stable` (default) | `https://releases.kepelet.com/l3s/stable.json` | — |
| `preview` | `https://releases.kepelet.com/l3s/preview.json` | `beta`, `nightly`, `canary`, `dev`, `pre-release` |

Settings carry `update_channel`, `update_feed_url`, and `auto_check_updates`;
the Settings page (`l3s → Settings → UPDATES`) edits all three and applies the
change immediately. An explicit `update_feed_url` wins over the channel
default, so a self-hosted preview manifest needs no code change.

Overrides, strongest first:

1. `--channel NAME` / `--feed-url URL` (with `--check-updates` only)
2. `L3S_UPDATE_CHANNEL` / `L3S_UPDATE_FEED_URL`
3. `settings.json`

Manifest shape (served by the release host, or any HTTPS host for a local or
self-hosted feed):

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

## Shipping a release

1. Bump `version` in the workspace `Cargo.toml`.
2. Tag and push `vX.Y.Z`. The workflow packages
   `l3s-v<version>-macos-arm64.dmg` (one `l3s.app` at the image root), writes
   `SHA256SUMS`, and attaches everything to the GitHub release. Apple Silicon
   only.
3. Upload the DMG and write `stable.json` (or `preview.json`) on
   `releases.kepelet.com` pointing at it. The client reads those manifests,
   not GitHub.

Asset names are load-bearing: the client matches on the `macos`, `arm64`,
and `.dmg` substrings. Don't rename them.

## Local testing

`just preview-feed` builds the DMG and writes the matching `latest.json` into
`target/release/preview-feed/`; the packaging script names the artifact, so
the manifest checksum and URL always match it. The manifest version is
generated automatically as `<next-patch>-preview.<utc timestamp>` and stamped
into the app through `L3S_BUILD_VERSION` (`crates/l3s/build.rs`, plus the
bundle's `Info.plist`), so the running app reports exactly what the feed
advertises:

- every payload is newer than the previous one — a preview never needs a
  `Cargo.toml` bump;
- installing a payload and re-checking the same feed reports up to date;
- **About l3s** shows the stamped version.

The DMG filename keeps the Cargo version (only the embedded build version and
the manifest are stamped). `SKIP_PACKAGE=1` reuses an existing DMG without
restamping, so its embedded version can lag the manifest — fine for manifest
iteration, not for installs.

```sh
# Build the payload (no version bump needed).
just preview-feed

# Terminal 1: serve it locally.
just serve-preview

# Terminal 2: check the local payload, overriding the hosted preview feed.
just check-updates --feed-url http://127.0.0.1:8000/latest.json

# Or point the window at it for a session.
L3S_UPDATE_FEED_URL=http://127.0.0.1:8000/latest.json just run
```

Without the override, `--channel preview` and `L3S_UPDATE_CHANNEL=preview`
read the hosted `preview.json`.

The base URL passed to `just preview-feed` is baked into each asset's `url`,
so it must be a host clients can dial. `0.0.0.0` is only a bind address: on
another machine it resolves to that machine's own loopback, so the check
succeeds (it reads `update_feed_url`) while the download fails with
`connection refused`. Use `127.0.0.1` for the local machine, or the
Tailscale/LAN host for another one:

```sh
just preview-feed http://<tailnet-host>:8000
```

Set `L3S_PREVIEW_VERSION` only when you want the feed to advertise a real
version (for example when publishing a hosted preview for others).

For a hosted preview, pass the public prefix and upload the directory:

```sh
just preview-feed https://dl.example.com/preview
# → target/release/preview-feed/latest.json + the DMG (+ .minisig when signed)
```

Note: install swaps the running bundle, so only download-check locally unless
the test build is disposable.

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
   rejected. Static manifests carry the signature in the asset's `signature`
   or `signature_url` field.

## Daemon compatibility

The OTA feed carries no protocol gate by design. `PROTOCOL_VERSION` is pinned
to the released daemon, and the handshake already rejects a mismatch with a
readable error — a client that updated past its daemon simply fails to attach
until `luvus` is updated too. Keep the two release notes in sync and say which
daemon a client release needs.
