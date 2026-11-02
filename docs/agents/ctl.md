# `pilus ctl`: driving a running window

`pilus ctl` sends one command to the open window, so a script or an agent can do
what a click or key would: open the right sidebar on a tab, load a page in the
Browser, go to a workspace tab.

```sh
pilus ctl sidebar open changes   # changes|history|files|ports|browser|sipil
pilus ctl sidebar close          # also: toggle
pilus ctl browser open localhost:3000
pilus ctl tab focus 2
pilus ctl state                  # what the focused window shows, as JSON
pilus ctl ping
```

## How it flows

- `ctl.rs` owns the wire format, the listener and the client, with no GUI types.
  `gui/ctl.rs` applies a request to the view.
- The window listens on `~/.pilus/pilus.sock` (`$PILUS_HOME/pilus.sock`). A
  client sends one line of JSON, `{"cmd":"sidebar_open","tab":"history"}`, and
  reads one line back, `{"ok":true,"data":{…}}` or `{"ok":false,"error":"…"}`.
- The listener thread parses the line and sends a `Call` down a
  `futures` channel. A task on the app's executor receives it, finds the active
  main window (any main window if Settings is in front) and runs it on the GUI
  thread. The task answers through the call's reply slot.
- The menu bar helper (`--menubar`) does not listen. A second full pilus finds
  the socket live and leaves it to the first; a socket a crash left behind is
  replaced.

## Adding a verb

1. Add a variant to `ctl::Request` and a case to `ctl::parse_args`. Variant
   order is not a wire format here (the verb is named), but a renamed `cmd` is
   a break for every script, so add rather than rename.
2. Handle it in `PilusView::apply_ctl` by calling the method the button or key
   calls. Do not reach into panel state from `gui/ctl.rs`.
3. Add it to `ctl::USAGE` and the list above.

## Rules

- Idle cost: the listener blocks in `accept` and the task is parked on a
  channel, so there is no timer and no poll. Keep it that way. A verb must not
  start one. See [idle-cost.md](idle-cost.md).
- Security: see [the control socket](security.md#the-control-socket). A verb
  gets only what a person at the keyboard could do from that surface, and a
  verb that takes a URL must not take more schemes than the surface does.
- The focused window only. A request does not pick a window.
