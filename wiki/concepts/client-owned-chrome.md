---
title: Client-owned chrome
description: Which rows the daemon gives up, which it keeps, and why.
tags:
  - wiki
  - concept
---
# Client-owned chrome

## Definition

The split between rows the daemon hands to the client and rows it keeps drawing. Only the workspaces sidebar and the session-name pill are claimable through `ShellDockLayout`; the tab bar and status line are always drawn by the daemon, so the client covers them with native chrome.

## Why it matters

It explains why the window looks like a native app but still reflows like a terminal: the daemon stays authoritative about dock geometry and theme, while the client decides how the claimed rows are painted. It also sets a hard limit on “Luvus renders only panes” — the tab bar is one row in the non-mobile layout and the status line is one row whenever `app.compact` is false, and there is no message that reclaims either. Covering is the closest possible realization; the rows stay reserved.

## Where it lives

- [message.rs](../../crates/pilus-protocol/src/message.rs) — `ShellDockLayout`, `ShellSidebars`, `ShellWorkspaces`, `ShellDockRect`.
- [surface.rs](../../crates/pilus-client/src/surface.rs) — stores the projection and the dock rect.
- [gui/mod.rs](../../crates/pilus/src/gui/mod.rs) — sends the claim, drops the agents dock, and paints the sidebar, tab strip, and status bar.
- [docs/architecture.md](../../docs/architecture.md) — the ownership table and the reasoning.

## Related

- [Daemon surface and wire protocol](../architecture/daemon-surface.md)
- [Native chrome and GPUI](../architecture/native-chrome.md)
- [Surface](./surface.md)
