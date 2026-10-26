# Pinned and recent workspaces

Read this before you change `gui/pinned.rs`, the PINNED WORKSPACES section, or
the pinned items in the menu bar.

Pinned and recent workspaces are folders on a machine, not open workspaces.

- They are stored as `Settings::pinned_workspaces` and `recent_workspaces`
  (the newest `RECENT_WORKSPACES`), both in the `ui_state` document, keyed by
  endpoint (`local` or a machine id) and path. A recent entry is written when
  the picker or a pin opens a workspace.
- A pin and a path are compared after expanding `~` against the machine's home,
  so the same folder written two ways is one pin.
- Both lists are namespaced by Luvus session (`SavedWorkspace::session`; `None`
  is the default session). The sidebar, Home and the menu bar list only the
  attached session's rows, because `workspace.open` goes to that session's
  daemon and a machine id only exists inside the session that saved it. The
  same folder pins once per session, and `open_saved_workspace` refuses a row
  from another session. A row saved before sessions were recorded has no
  `session` and reads as the default session's.
- `recent_workspaces` is one list shared by every session, capped at
  `RECENT_WORKSPACES`, so a busy session pushes the others' folders out of it.

## Opening one

"Pin this Workspace" is in a WORKSPACES row's context menu. Three places open a
pin, all the same way: the sidebar's PINNED WORKSPACES section (drawn only when
something is pinned), and Home's Pinned and Recent blocks.

1. Call `workspace.open` on a worker thread through the control client. This
   reaches a saved machine over SSH whether or not the window is attached to
   that machine.
2. Then call `activate_opened_endpoint`.

The window leaves its current page only after the daemon has answered, so a
failed open can be retried.

## The menu bar item

The menu bar item lists the pins under the agents (`menubar::Pinned`; a click is
a `Pick::Pinned`). With no window open, the helper opens one and hands it the
folder (`PilusView::saved_pick`). For a remote pin, the window waits for the
machine catalog before opening it.
