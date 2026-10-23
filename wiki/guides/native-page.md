---
title: Add a native page
description: A page in the terminal card, its state, and its navigation entry.
tags:
  - wiki
  - guide
---
# Add a native page

## Goal

Add a full-card view (like Home, Search, Insights, Tasks, or Mission Control) that shares the terminal card instead of opening a separate window.

## Steps

1. **Create the module.** Add `gui/<page>.rs` with a `//!` header stating its role in the data flow. Register `mod <page>;` in [gui/mod.rs](../../crates/pilus/src/gui/mod.rs).
2. **Decide the data owner.** A page should own only view state. Daemon data comes from `ControlClient` methods on a worker thread; local data comes from a store; a snapshot is read, never mutated in place.
3. **Share the card.** Use `page_split` so the page sits beside the terminal and the sidebar keeps its width. Follow `Mission Control`, `Tasks`, or `Automations` for the pattern.
4. **Give it an entry point.** Add a sidebar row, command-palette action, or page menu item. If it is reachable from a context menu, route it the same way as the existing boards.
5. **Take the keyboard deliberately.** If the page edits text, add an `InputLayer` variant and release the layer on close/Esc. Editor chords belong to a key context (e.g. `MdEditor`), not to a check in the handler.
6. **Follow the idle rules.** Poll only while the page is open, through `activity::scaled` where the poll is timer-driven. Stop timers on close. Do not hold data after close if it can be someone's message.
7. **Add a capture.** Register a headless shot for the page with sample data, so `--ui-shots` renders it without a daemon.

## Relevant code

- [gui/mod.rs](../../crates/pilus/src/gui/mod.rs) — module list, `PilusView`, `InputLayer`, render.
- [page_split.rs](../../crates/pilus/src/gui/page_split.rs) — a native page sharing the card.
- [mission.rs](../../crates/pilus/src/gui/mission.rs) and [tasks.rs](../../crates/pilus/src/gui/tasks.rs) — read-only and editable page patterns.
- [right_sidebar.rs](../../crates/pilus/src/gui/right_sidebar.rs), [page_menu.rs](../../crates/pilus/src/gui/page_menu.rs) — tabs and the three-dot menu.
- [shots.rs](../../crates/pilus/src/gui/shots.rs) — capture registration.

## Gotchas

- The Home composer is `PilusView::home` state: opening Home starts it and leaving either one closes both. Reuse that pattern instead of adding a second composer.
- A page that reads transcripts, notes, or database rows must keep the same consent gate as its source (Search's `index_agents`, the Database page's read-only rule).
- A page that dials must go through `netlog` so the Process page sees it.
- Do not add a sidebar row for a diagnostic page (Process, Database are palette-only).

## Related

- [Native chrome and GPUI](../architecture/native-chrome.md)
- [pilus (GUI)](../modules/gui.md)
- [Add a UI control](./ui-control.md), [Run visual captures](./visual-captures.md)
