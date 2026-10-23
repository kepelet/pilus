---
title: pilus (GUI crate)
description: The GPUI window, native pages, settings, updater, and macOS integration.
tags:
  - wiki
  - module
---
# pilus (GUI crate)

## Summary

The `pilus` crate is the desktop application: the GPUI window, the native pages and chrome, settings, logging, the OTA updater, the macOS integration, and the launch probes. It is the top of the dependency chain and the only crate that depends on GPUI. Most of its 80k+ lines live under `src/gui/`.

## Responsibilities

- Run the window and its tick loop ([gui/mod.rs](../../crates/pilus/src/gui/mod.rs)).
- Paint the daemon grid, handle input, and own client-side selection ([selection.rs](../../crates/pilus/src/gui/selection.rs), [keys.rs](../../crates/pilus/src/gui/keys.rs)).
- Draw the native sidebar, top chrome, right column, and pages.
- Keep one shared state per integration: endpoints and surfaces, sipil ([sipil.rs](../../crates/pilus/src/gui/sipil.rs)), the connector ([connector.rs](../../crates/pilus/src/gui/connector.rs)), agent feeds ([agent_watch.rs](../../crates/pilus/src/gui/agent_watch.rs)), GitHub.
- Persist preferences and per-item state ([settings.rs](../../crates/pilus/src/settings.rs)).
- Update itself and the daemon ([update.rs](../../crates/pilus/src/update.rs), [updater.rs](../../crates/pilus/src/gui/updater.rs), [luvus_update.rs](../../crates/pilus/src/luvus_update.rs), [update_window.rs](../../crates/pilus/src/gui/update_window.rs)).
- Integrate with macOS: app icon, menu bar item, Dock, notifications, vibrancy ([app_icon.rs](../../crates/pilus/src/gui/app_icon.rs), [menubar.rs](../../crates/pilus/src/gui/menubar.rs), [dock.rs](../../crates/pilus/src/gui/dock.rs), [notify.rs](../../crates/pilus/src/gui/notify.rs), [glass.rs](../../crates/pilus/src/gui/glass.rs)).

## Public API / entry points

- [main.rs](../../crates/pilus/src/main.rs) and [cli.rs](../../crates/pilus/src/cli.rs) — launch modes and probes.
- [init.rs](../../crates/pilus/src/init.rs) — first-run alignment of the daemon config with the surface pilus draws.
- [logging.rs](../../crates/pilus/src/logging.rs) — `~/.pilus/logs/pilus.log`, owner-only, cut over at 4 MB, `PILUS_LOG=info|debug|trace`.
- `--ui-shots` / `--ui-shots-main` with the `visual-test` feature ([shots.rs](../../crates/pilus/src/gui/shots.rs)).

## Key files

The module list at the top of [gui/mod.rs](../../crates/pilus/src/gui/mod.rs) is the authoritative index. Grouped:

| Group | Modules |
| --- | --- |
| Window shell | `mod.rs`, `style.rs`, `theme.rs`, `scheme.rs`, `fonts.rs`, `text_input.rs`, `toast.rs`, `palette.rs`, `page_menu.rs`, `page_split.rs`, `picker.rs` |
| Sidebar | `pinned.rs`, `sidebar_menu.rs`, `right_sidebar.rs`, `sipil_panel.rs`, `github.rs` |
| Terminal card | `selection.rs`, `pane_frames.rs`, `pane_chrome.rs`, `box_drawing.rs`, `highlight.rs`, `cursor_blink.rs`, `toolbar_fade.rs` |
| Pages | `overview.rs`, `overview_peek.rs`, `home.rs`, `search.rs`, `stats.rs`, `tasks.rs`, `task_compose.rs`, `orchestration.rs`, `mission.rs`, `automations.rs`, `diff.rs`, `diff_model.rs`, `git_tab.rs`, `files_tab.rs`, `github_page.rs`, `ports_tab.rs`, `browser_tab.rs`, `browser_data.rs`, `process.rs`, `database.rs`, `session_view.rs`, `session_ops.rs` |
| Markdown | `markdown.rs`, `md_view.rs`, `md_notes.rs`, `md_notes_view.rs`, `md_select.rs` |
| Sipil | `sipil.rs`, `sipil_chat.rs`, `sipil_social.rs`, `sipil_panel.rs` |
| System | `activity.rs`, `perf.rs`, `dock.rs`, `menubar.rs`, `background.rs`, `notify.rs`, `sound.rs`, `glass.rs`, `app_icon.rs`, `connector.rs`, `machines.rs`, `agent_watch.rs`, `attention.rs`, `updater.rs`, `update_window.rs`, `about.rs`, `appearances.rs`, `ui_playground.rs`, `shots.rs` |

## Dependencies

`pilus-client`, `pilus-db`, `pilus-index`, `pilus-protocol` (re-exported through the client), `gpui` (pinned `gpui-pre`), `gpui_platform` with `runtime_shaders`, `gpui-wry` and `wry` for the Browser tab, `objc2*` and `block2` for macOS, `rusqlite` transitively, `serde`/`serde_json`, `markdown`, `syntect`, `image`, `ureq`.

## Design decisions

- **The window owns state per feature in one place.** There is one `SipilState`, one connector cache, one agent feed per endpoint; pages read snapshots and never mutate shared state directly.
- **Pages share the card.** Mission Control, Tasks, Automations, Diff, and the rest open inside the terminal card through `page_split`, so the terminal chrome and the sidebar stay put.
- **Settings split by ownership.** Preferences go to `settings.json`; per-item window state goes to `pilus.db`'s `ui_state`. See [Persistence and data ownership](../architecture/persistence.md).
- **Idle is a feature.** Every timer, tick, and tween has a budget; `PILUS_PERF` and the Process page make regressions visible. See [Idle cost](../concepts/idle-cost.md).
- **Visual captures are the UI test.** `visual-test` renders every surface headlessly; changes ship after a capture review. See [Run visual captures](../guides/visual-captures.md).

## Participates in

Every flow in this wiki terminates here: [Attach and paint](../flows/attach-and-paint.md), [Switch machines](../flows/machine-switch.md), [Sipil message](../flows/sipil-message.md), [Search, stats, and resume](../flows/session-search.md), [Release and update](../flows/release-update.md).

## Related

- [Native chrome and GPUI](../architecture/native-chrome.md)
- [pilus-client](./client.md)
- [Add a native page](../guides/native-page.md), [Add a UI control](../guides/ui-control.md)
