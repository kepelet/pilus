---
title: Surface
description: The client-side cell grid and the out-of-band requests beside it.
tags:
  - wiki
  - concept
---
# Surface

## Definition

A `Surface` is the client-side projection of a daemon's rendered screen: the current `FrameData` grid plus the out-of-band requests and projections that arrive beside it (notifications, sounds, clipboard text, an open-URL request, the workspace catalog, and the claimed dock rectangle).

## Why it matters

The daemon owns layout, so the client's job is to hold one coherent grid and repaint only when it changed. `Surface` is also the containment boundary for server-driven side effects: a window drains them deliberately with `take_*` calls instead of letting any message act on its own. A rejected diff never mutates the grid, so a torn frame cannot be painted.

## Where it lives

[surface.rs](../../crates/pilus-client/src/surface.rs) defines `Surface`, `SurfaceChange` (`None`, `Repaint`, `Resized`), and `openable_url`. It applies messages in `apply`/`apply_owned`, tracks `needs_full_frame` and `resync_requested`, and exposes `frame`, `workspaces`, `shell_dock`, and the `take_*` drain methods. [frame.rs](../../crates/pilus-protocol/src/frame.rs) supplies the validation and diff application it relies on.

## Related

- [Daemon surface and wire protocol](../architecture/daemon-surface.md)
- [Client-owned chrome](./client-owned-chrome.md)
- [Attach and paint](../flows/attach-and-paint.md)
- [pilus-client](../modules/client.md)
