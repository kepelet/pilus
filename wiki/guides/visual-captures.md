---
title: Run visual captures
description: Render every surface with the visual-test feature and review the PNGs.
tags:
  - wiki
  - guide
---
# Run visual captures

## Goal

Render every native surface headlessly and review the PNGs before shipping a UI change.

## Steps

1. **Run the standalone captures.**

   ```sh
   cargo run -p pilus --features visual-test -- --ui-shots
   ```

   This renders the windows that need no daemon: About, Appearances, Add Machine, Update, UI Playground, and sample pages.
2. **Run the main-window captures.**

   ```sh
   cargo run -p pilus --features visual-test -- --ui-shots-main
   ```

   This captures the main window and its overlays with sample data; it does not require a running daemon for the sample paths.
3. **Read the output.** PNGs land in `/tmp/pilus-shots`.
4. **Exercise scaling.** `PILUS_SHOT_FONT_SIZE=15` overrides the Interface size and is the quickest way to catch chrome that ignores the setting. `PILUS_SHOT_WIDTH` narrows the main window to exercise compact layouts; `PILUS_SHOT_RADIUS` and `PILUS_SHOT_OPACITY` do the same for corners and translucency.
5. **Add a capture for a new surface.** Register it in [shots.rs](../../crates/pilus/src/gui/shots.rs), with sample data and no network or real database. The capture runs on the main thread (macOS requires it before handing out a Metal device) and sets reduce-motion so fades are caught at rest.

## Relevant code

- [shots.rs](../../crates/pilus/src/gui/shots.rs) — capture list, output directory, environment overrides.
- [cli.rs](../../crates/pilus/src/cli.rs) — `--ui-shots` and `--ui-shots-main` behind `visual-test`.
- Sample-data helpers used by captures: `search_show_sample`, `database_show_sample`, `sipil::preview`. They never touch the real index, database, or identity key.

## Gotchas

- `visual-test` is not part of a normal build; shipping code must not depend on it outside `#[cfg(feature = "visual-test")]`.
- A capture that dials, writes the identity key, or opens the real `pilus.db` is a bug: use the preview/sample path instead.
- With motion on, a capture catches fades half-way. The harness sets `reduce_motion`; do not undo it.
- A capture proves pixels, not behavior: pair it with the headless probes (`--probe`, `--probe-uhp`, `--probe-feed`) when the change is in the data path.

## Related

- [Native chrome and GPUI](../architecture/native-chrome.md)
- [Add a UI control](./ui-control.md), [Add a native page](./native-page.md)
- [pilus (GUI)](../modules/gui.md)
