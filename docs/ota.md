# OTA updates

`pilus` updates itself from channel manifests on `releases.kepelet.com`: stable
at `stable.json`, preview/nightly at `preview.json`. A local payload can be
tested by pointing the feed URL at `http://127.0.0.1:8000/latest.json`.

| Piece | Where |
| --- | --- |
| Release source + channel policy | `crates/pilus/src/update.rs` |
| Settings (channel, feed URL, auto-check) | `crates/pilus/src/settings.rs` |
| GPUI entity (check → download → stage) | `crates/pilus/src/gui/updater.rs` |
| Build version stamp | `crates/pilus/build.rs` |
| Footer badge + `pilus → Check for Updates` | `crates/pilus/src/gui/mod.rs` |
| Settings page (channel, feed URL, auto-check) | `crates/pilus/src/gui/mod.rs` |
| Update window (live status + Retry) | `crates/pilus/src/gui/update_window.rs` |
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
| `stable` (default) | `https://releases.kepelet.com/pilus/stable.json` | — |
| `preview` | `https://releases.kepelet.com/pilus/preview.json` | `beta`, `nightly`, `canary`, `dev`, `pre-release` |

Settings carry `update_channel`, `update_feed_url`, and `auto_check_updates`;
the Settings page (`pilus → Settings → UPDATES`) edits all three and applies the
change immediately. An explicit `update_feed_url` wins over the channel
default, so a self-hosted preview manifest needs no code change.

Overrides, strongest first:

1. `--channel NAME` / `--feed-url URL` (with `--check-updates` only)
2. `PILUS_UPDATE_CHANNEL` / `PILUS_UPDATE_FEED_URL`
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
      "name": "pilus-v0.2.0-macos-arm64.dmg",
      "url": "https://dl.example.com/pilus-v0.2.0-macos-arm64.dmg",
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
   `pilus-v<version>-macos-arm64.dmg` (one `pilus.app` at the image root, beside an `Applications` link and a
   drag-to-install background), writes
   `SHA256SUMS`, and attaches everything to the GitHub release. Apple Silicon
   only.
3. Upload the DMG and write `stable.json` (or `preview.json`) on
   `releases.kepelet.com` pointing at it. The client reads those manifests,
   not GitHub.

Asset names are load-bearing: the client matches on the `macos`, `arm64`,
and `.dmg` substrings. Don't rename them.

## Stable and Nightly side by side

`just preview-feed` and `just release-preview` package the preview as its own
app, **pilus (Nightly)** (`pilus (Nightly).app`, bundle id
`pilus.kepelet.com.nightly`), so it installs beside the stable `pilus.app`
instead of over it. Each keeps its own Notification Center and TCC identity.
The OTA swap replaces whichever bundle is running, so Nightly updates itself in
place under its own name.

The two share a settings file, so the Nightly build is stamped
`PILUS_BUILD_CHANNEL=preview` and is pinned: the Settings page shows the channel
as a label, and neither the saved `update_channel` nor `PILUS_UPDATE_CHANNEL`
moves it. Override the name or id with `PILUS_APP_NAME` and `PILUS_BUNDLE_ID`.
Stable's channel switch still exists; switching stable to preview would install
Nightly's build over `pilus.app`, so use the Nightly app for previews.

## Local testing

`just preview-feed` builds the DMG and writes the matching `latest.json` into
`target/release/preview-feed/`; the packaging script names the artifact, so
the manifest checksum and URL always match it. The manifest version is
generated automatically as `<next-patch>-preview.<utc timestamp>` and stamped
into the app through `PILUS_BUILD_VERSION` (`crates/pilus/build.rs`, plus the
bundle's `Info.plist`), so the running app reports exactly what the feed
advertises:

- every payload is newer than the previous one — a preview never needs a
  `Cargo.toml` bump;
- installing a payload and re-checking the same feed reports up to date;
- **About pilus** shows the stamped version.

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
PILUS_UPDATE_FEED_URL=http://127.0.0.1:8000/latest.json just run
```

Without the override, `--channel preview` and `PILUS_UPDATE_CHANNEL=preview`
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

Set `PILUS_PREVIEW_VERSION` only when you want the feed to advertise a real
version (for example when publishing a hosted preview for others).

For a hosted preview, pass the public prefix and upload the directory:

```sh
just preview-feed https://dl.example.com/preview
# → target/release/preview-feed/latest.json + the DMG (+ .minisig when signed)
```

To publish a preview to the hosted channel, use `just release-preview`. It
builds the payload with asset URLs under
`https://releases.kepelet.com/pilus/preview/` and uploads it with
`scripts/publish-release.sh preview`: the DMG goes to `pilus/preview/` (so it can never
overwrite the stable DMG of the same Cargo version) and the manifest to
`pilus/preview.json`, last. It needs `bunx wrangler login` (run in `infra/`;
this is separate from the Alchemy profile) and `PILUS_RELEASES_BUCKET` in the
environment or `infra/.env`. `just release-preview-build` runs the build alone.
Check it with `just check-updates --channel preview`.

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
3. **Minisign (on).** The public key is `MINISIGN_PUBLIC_KEY` in `update.rs`, so
   verification is `Signature` and an unsigned download is rejected. The secret
   key stays out of the repo (`~/.minisign/pilus.key`); package with
   `MINISIGN_SECRET_KEY=<path to it>` and each DMG gets a `.minisig`, which the
   manifest references as `signature_url`. Packaging skips signing silently when
   the key or `minisign` is missing, so `publish-release.sh` refuses to upload a
   DMG without one. Builds before this key was set accept unsigned updates. To
   sign in CI, use a key generated with `minisign -G -W` (no password) in the
   `MINISIGN_SECRET_KEY` repo secret. Back the key up: losing it strands every
   client that has the public key.

   `LOCAL=true` is a developer-only waiver, set by hand and never by the app
   or scripts. `LOCAL=true just release-preview` skips the publish-time
   signature check (stable ignores it), and a client launched with `LOCAL=true`
   in its environment drops the public key and verifies best-effort, so it
   installs an unsigned preview. A client launched without it stays fail-closed.

   `LOCAL=true just preview-feed http://<host>:8000` also skips notarization
   while packaging, so a throwaway build for another machine does not wait on
   Apple. It is still minisign signed, so the client needs no waiver. An app
   installed by hand with a quarantine flag needs
   `xattr -dr com.apple.quarantine` on it. `package-dmg` and `release-stable`
   ignore it.

## Daemon compatibility

The OTA feed carries no protocol gate by design: `pilus` and `luvus` are
released separately, and the manifest does not name the daemon a build needs.
`PROTOCOL_VERSION` is pinned to the newest released daemon (20, Luvus 0.14.3),
and `FALLBACK_PROTOCOL_VERSION` (18, Luvus 0.14.2) is decoded too. The handshake
tries the newest revision first and, when the daemon rejects it, reconnects at
the fallback; `FrameData` decodes either shape per connection. One build
bridges both, which is what keeps the local terminal working while a fleet is
mid-upgrade: a local or remote daemon still on 0.14.2 attaches normally, and a
protocol error only surfaces when neither revision matches. The updater below
still moves everything to the pinned release when you are ready.

A protocol mismatch no longer strands the app. The main window opens before
the daemon is attached, so a stopped or incompatible daemon cannot abort the
launch: the empty grid shows the failure and the local reconnect loop keeps
retrying with capped backoff, which keeps Settings, Updates, and the Remote
Machines manager reachable. Remote machines already degrade per endpoint the
same way, so a newer local `pilus` still opens even when every daemon is old.

`pilus` updates itself through its OTA feed; the daemon gets a parallel path.
**Settings → UPDATES → Luvus daemon** has three actions:

- **Check Luvus** reads `https://luvus.dev/latest.json` and compares it to the
  installed `luvus --version`, showing `installed X · latest Y`.
- **Update Luvus** downloads the platform archive
  (`luvus-v<version>-<triple>.tar.gz`), verifies its `.sha256` sidecar through
  the same `gpui-updater-core` download/verify pipeline `pilus` uses, extracts
  `luvus`, and atomically installs it. A Homebrew, Cargo, or Nix install is
  refused so it keeps updating through its package manager; everything else
  installs in place, or at `~/.local/bin/luvus` when the only `luvus` is the
  remote-provisioning cache.
- **Restart Luvus** runs `luvus [--session NAME] server restart` so the running
  daemon loads the new binary. It is two-click because it stops every pane in
  the session.

`LUVUS_UPDATE_MANIFEST` and `LUVUS_UPDATE_RELEASE_BASE` override the manifest
and download host, matching the `luvus` CLI's own variables.

- **Saved machines.** A profile bridges the versioned binary its
  `remote_binary` names, provisioned from the local install's
  `~/.local/share/luvus/remote/<version>/…` cache. Each enabled row in
  **Remote Machines** has an **Update** button that re-provisions it with
  `luvus machine enable <id> --install`, and the row shows the remote daemon's
  version from the last `machine status` probe.

Every daemon action runs off the GUI thread and reports its result in place,
so a slow download, an offline host, or a package-manager refusal never blocks
the window.

Keep the two release notes in sync and say which daemon a client release
needs.
