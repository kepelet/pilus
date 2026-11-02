# Pinned and recent workspaces

Read this before you change `gui/pinned.rs`, the pinned menu in the sidebar head, or
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

## Order

Pins are listed in the order of `pinned_workspaces`: new ones go last, and a
menu row can be dragged onto another to take its slot
(`Settings::move_workspace_pin`). The list is shared by every session, so a move
only compares rows of one session and leaves the others' relative order alone.
Home and the menu bar list the same order. The row opens on click rather than
mouse down, so starting a drag does not open the workspace.

## Opening one

"Pin this Workspace" is in a WORKSPACES row's context menu. Four places open a
pin, all the same way: the sidebar head's pinned menu, the ⌘O palette, and
Home's Pinned and Recent blocks.

⌘O (File > Open Pinned Workspace…) opens the command palette in its pinned mode
(`Palette::open_pinned`): the same overlay, filtered by the same fuzzy match, but
listing only the attached session's pins in pin order, a remote pin as
`name · machine`. Its field reads "Open pinned workspaces…". Enter runs `Command::OpenPinned(index)`, which calls
`open_saved_workspace`. The mode lives in the `Palette`, so every way out of the
overlay clears it, and pressing ⌘⇧P while it is open shows the commands.

⌘⇧S is the same overlay in its session mode (`Palette::open_sessions`, field
"Switch session…"): one row per Luvus session, a running one switching to it and
a stopped one starting it first (`Command::StartSession`), plus New session. The
session picker button itself sits in the tab strip beside the first tab and still
opens the session menu.

The pinned menu is a folder-bookmark button in the sidebar head before the
collapse toggle (`pinned_button`), which drops a list of
the pins below it (`pinned_menu`), built like the session menu: a transparent
catcher over the window dismisses it, and Escape closes it. A row shows the name
and, for a remote pin, the machine; its tooltip shows the folder. The button is
absent until something is pinned, and the menu closes when the sidebar collapses
or the last pin goes (`pinned_menu_shown`). A row opens the workspace and closes
the menu; a right click opens its context menu; a drag onto another row
reorders.

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
