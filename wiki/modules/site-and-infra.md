---
title: Site, infra, and release scripts
description: The Blume site, the Cloudflare stack, and the macOS release toolchain.
tags:
  - wiki
  - module
---
# Site, infra, and release scripts

## Summary

The public-facing and shipping side of the repository: a Blume/Astro marketing site and docs under `site/`, a Cloudflare stack under `infra/` that serves them and the release bucket, and shell/Python scripts under `scripts/` that build, sign, package, and publish macOS releases. The `justfile` is the command surface over all of it.

The site's docs are written in Indonesian; the architectural prose in this wiki is English and internal.

## Responsibilities

- Build the landing page and documentation into static assets (`site/dist`).
- Declare the Cloudflare Worker for `usepilus.dev` and the existing R2 bucket behind `releases.kepelet.com` with Alchemy ([alchemy.run.ts](../../infra/alchemy.run.ts)).
- Assemble the macOS `.app` bundle, DMG, and channel manifests.
- Publish stable and preview channels to the release bucket.
- Document the OTA contract and the deployment steps.

## Key files

### Site

- [site/blume.config.ts](../../site/blume.config.ts) — title, logo, theme (accent `#e99a24`, Arimo and Ioskeley Mono), explicit sidebar navigation, deployment URL.
- [site/pages/index.astro](../../site/pages/index.astro) — landing page at `/`.
- `site/docs/` — quickstart, installation, FAQ, features (UI, browser, ports, Git, GitHub, files, iOS), sipil pages, guides, blog, and changelog. Content is organized with numbered folders and rewritten by Blume.
- `site/components/`, `site/public/` — shared Astro components, CSS tokens, fonts, and images.
- `site/package.json` — `blume dev|build|preview|doctor`.

### Infra

- [infra/alchemy.run.ts](../../infra/alchemy.run.ts) — a Worker for `usepilus.dev` serving `../site/dist` with a `/` redirect fallback, and an R2 bucket observed by name (`PILUS_RELEASES_BUCKET`) with the `releases.kepelet.com` domain. State lives in a Cloudflare state-store Worker bootstrapped on first deploy.
- [infra/site/worker.ts](../../infra/site/worker.ts) — the site's only server-side code: every path outside `PUBLIC` (the landing page and its assets) answers 404.
- [docs/infra.md](../../docs/infra.md) — first-deploy steps, including the hostname conflict to clear before attaching `usepilus.dev`.

### Scripts

- [scripts/bundle-macos.sh](../../scripts/bundle-macos.sh) — assembles `pilus.app` with icon, Info.plist, and `pilus-agent` beside the main binary.
- [scripts/package-macos-dmg.sh](../../scripts/package-macos-dmg.sh) — signed, notarized DMG whose root holds one `pilus.app`, an `Applications` link, and the drag-to-install background laid out by `dmg-settings.py`.
- [scripts/dmg-background.py](../../scripts/dmg-background.py), [scripts/dmg-settings.py](../../scripts/dmg-settings.py) — Finder window layout for the DMG.
- [scripts/preview-feed.sh](../../scripts/preview-feed.sh) — a local preview payload: DMG + `latest.json` (plus `.minisig` when a signing key is set).
- [scripts/write-manifest.sh](../../scripts/write-manifest.sh) — writes a channel manifest for an already-packaged DMG once the hosting base URL is known.
- [scripts/publish-release.sh](../../scripts/publish-release.sh) — uploads DMG and manifest to the bucket; the manifest goes last so a client never reads one whose DMG is missing.

### Commands

[justfile](../../justfile) wraps the common paths: `check`, `test`, `fmt`, `clippy`, `run`, `probe`, `build`, `bundle`, `package-dmg`, `preview-feed`, `manifest`, `release-preview`, `release-stable`, `serve-preview`, `check-updates`, `site-build`, `site-dev`, `infra-plan`, `infra-deploy`.

## OTA contract

`pilus` reads a channel manifest (`stable.json` or `preview.json`) from `https://releases.kepelet.com/pilus/`. The manifest carries `schema_version`, `version`, `notes`, and assets with `name`, `url`, `sha256`, `os`, `arch`, and `format`. Asset names must stay `pilus-v<version>-macos-<arch>.dmg`, because the GitHub source matches on those substrings. Overrides are `--channel`/`--feed-url`, then `PILUS_UPDATE_CHANNEL`/`PILUS_UPDATE_FEED_URL`, then settings. See [docs/ota.md](../../docs/ota.md).

Preview DMGs live under `preview/` in the bucket because the filename carries only the Cargo version; beside the stable DMG it would overwrite it.

## Design decisions

- **The bucket is observed, not created.** It predates the stack and holds live release objects, so Alchemy declares it under its real name and leaves its contents alone.
- **The site is static.** Assets-first routing keeps the docs cheap and cacheable; the Worker only answers paths the asset layer cannot or should not serve: a 404 for every path outside `PUBLIC`, i.e. everything except the landing page and its assets. `just infra-plan`/`infra-deploy` build it first.
- **Manifests are written late.** `package-macos-dmg.sh` stays manifest-free because it cannot know the final host; `write-manifest.sh` bakes the known URL.
- **Publish the manifest last.** A client never sees a manifest pointing at a DMG that is not there yet.

## Related

- [Release and update](../flows/release-update.md)
- [pilus (GUI)](./gui.md)
- [System architecture](../architecture/system-architecture.md)
