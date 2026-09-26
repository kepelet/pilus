# Architecture

`l3s` is three crates with a one-way dependency chain:

```
l3s (GPUI window)
  └── l3s-client (socket, handshake, event stream, Surface)
        └── l3s-protocol (messages, frames, framing)
```

Nothing in `l3s-protocol` or `l3s-client` knows about GPUI, so the connection
layer is testable headlessly (see `crates/l3s-client/tests/handshake.rs`).

## Why the client is a thin painter

A Luvus daemon renders its whole interface — workspaces, panes, the agent
sidebar, modals — into a ratatui buffer, then serializes that buffer as a grid
of cells. A display client does not re-derive layout; it receives the rendered
surface and paints it. That keeps the native client small and guarantees it
matches the daemon exactly, including features the client has never heard of.

The client's own chrome (the header strip) is the only thing it draws itself.

## Discovery

The daemon's state root is `$LUVUS_HOME`, else `~/.luvus` (`~/.luvus-dev` in a
debug build). Sockets live there:

```
<root>/luvus-client.sock                 default session render socket
<root>/luvus.sock                        JSON control socket (CLI / UHP)
<root>/sessions/<name>/luvus-client.sock named session render socket
```

`l3s` attaches to the **render** socket (`luvus-client.sock`). `$LUVUS_SESSION`
selects a session, matching the daemon's own environment convention; `--session`
and `--socket` override it. When the derived path is too long for `sun_path`,
both the daemon and `l3s` relocate it to a hashed, owner-scoped alias under the
temporary directory (`discovery::alias_for`).

When nothing answers at the socket, the daemon isn't running, so `l3s` starts
it with `luvus [--session NAME] server start` and waits for the socket
(`autostart::ensure_server`, called from `connect`). The child gets
`LUVUS_HOME` pinned to the target's state root, so it starts the same daemon
`l3s` would have attached to. Liveness is checked by dialing, not by file
existence, because a stopped server leaves its socket file behind.

## Handshake

On connect, before any version agreement exists:

```
client → Hello { version, cols, rows }
server → Welcome { version, error }
server → Ready { probe_terminal }
server → EndpointIdentity { boot_id, session }
client → TerminalColors(None)          // only when probe_terminal
```

`Welcome` is decoded before the version check so a mismatch still produces a
readable error. `l3s` paints its own surfaces and reports no terminal palette,
but it must still answer the color probe or the server will not stream frames.

After the handshake the server streams `ServerMessage`s and the client may send
`Key`, `Mouse`, `Paste`, `Resize`, and health checks at any time.

## Frames

`ServerMessage::Frame` carries a full `FrameData`: a row-major grid of
`CellData { symbol, fg, bg, mods }`. Subsequent updates are `FrameDiff`s — runs
of contiguous, same-style changed cells — that the client applies to its stored
frame. A diff whose dimensions or runs do not fit the current frame is rejected
whole; `Surface` then flags `needs_full_frame` so the window can ask for a full
one rather than paint a torn grid.

`mods` is the ratatui `Modifier` bitset (`BOLD = 1<<0`, `ITALIC = 1<<2`,
`UNDERLINED = 1<<3`, `REVERSED = 1<<6`). Colors are packed with a tag byte:

| Tag | Meaning |
| --- | --- |
| `0` | Use the client's default foreground/background. |
| `1` | ANSI palette index in the low byte. |
| `2` | 24-bit sRGB in the low three bytes. |

`gui::theme::resolve` turns any tag into a paintable color, expanding palette
indexes with the standard xterm 256-color table.

## Threading

```
GUI thread ──send──▶ outbound channel ──▶ writer thread ──▶ socket
                                                             │
GUI thread ◀──try_recv── inbound channel ◀── reader thread ◀─┘
```

`Connection::open` performs the handshake synchronously, then spawns the reader
and writer threads. The GUI thread drains `ClientEvent`s in a 16 ms poll task
and calls `cx.notify()` only when the `Surface` changed. It never blocks on IO
and never paints from a non-GUI thread.

## Client-owned sidebar

By default the daemon renders the workspaces and agents docks into the grid
like any other panel. `l3s` takes the whole left sidebar instead.

**Workspaces.** After the handshake, `l3s` sends:

```text
ShellDockLayout { owns_workspaces: true, owns_session_chrome: true, .. }
```

That marks the connection *machine-capable* and tells the daemon to stop
drawing its workspaces dock — it leaves those cells blank and starts projecting
state instead:

- `ShellWorkspaces(Vec<ShellWorkspace>)` is the catalog: dot glyph and color,
  id, name, branch, short cwd, and active/selected/nested flags;
- `ShellDock(Some(ShellDockRect))` is the exact blanked rectangle in cell
  coordinates, plus the daemon's own row colors and whether paths are shown.

The same message claims `owns_session_chrome`: the daemon blanks the whole
session-name slot (`session_button: None`) and exposes no hit target, so the
native shell draws that top-row chrome itself.

**The agents dock.** The protocol does not project agents, but the daemon sends
its current layout as `ShellSidebars`. `l3s` drops the `agents` dock from that
layout, bumps the revision, and sends it back. The daemon then renders no
agents dock, and the workspaces slot grows to fill the sidebar — the `ShellDock`
rectangle gets taller, which is where the native panel is painted. This is
per-connection state; the daemon's own config is untouched.

**Agents.** The live agent list comes from a different surface: UHP on
`luvus.sock`, method `agent.list`. `l3s-client`'s `ControlClient` speaks the
newline-delimited `{id, method, params}` protocol. Refreshes are event-driven:
`AgentFeed` holds an `events.subscribe` stream and re-reads `agent.list`,
`ui.bar.list`, and `tab.list` only when a structural event arrives, plus a
10-second reconciliation for state no event describes (the status bar's mode).
`terminal.output_ready` is deliberately not structural — acting on it would be
a poll with extra steps. Snapshots are deduplicated against the last one sent,
because one tab open emits several structural events. `--probe-feed` drives the
same feed headlessly. Clicking an agent row focuses its pane; clicking a
workspace row sends `ShellWorkspaceFocus`.

**Top chrome and Menu.** The daemon's `« session Menu` strip is hidden by
`owns_session_chrome`, and the sidebar covers its column. One native layer owns
the whole top of the window: a bar of `HEADER_HEIGHT + one cell` that starts at
the sidebar's right edge, so the sidebar keeps a full-height column and the
chrome never competes with it. The bar carries the sidebar toggle, the target
label, the tab strip, the `+`, the Menu, and the connection status; the
daemon's tab row and the old separate app header both sit underneath it. There
is no protocol message or control-API method that opens Settings, and the
daemon's `prefix` chord did not trigger from a programmatic client, so the Menu
forwards a synthetic left click at the daemon's Menu-pill cell (derived from
the sidebar width) — the same click path the daemon already handles.

**Status bar.** The daemon's bottom line is covered by a native bar. The row
itself cannot be reclaimed (the daemon reserves it unless width-based mobile
mode is active), so covering is the only option. The right side renders the
daemon's own `ui.bar.list` widgets — mode, pane count, tab — polled alongside
the agent list; the left shows the connection and session. The daemon's
dynamic left-hand key guidance is not reproduced.

**Tabs.** The tab strip lives in the unified top chrome, covering the daemon's
tab row. State comes from `tab.list` and actions from `tab.focus`, `tab.new`,
and `tab.close`, polled alongside the other control-API calls. GPUI renders the
window itself, so the strip is drawn rather than hosted; the reference client
draws its tabs the same way. `WindowOptions::tabbing_identifier` does exist in
`gpui-pre` and the macOS backend wires it to `NSWindow.setTabbingIdentifier:`
plus `addTabbedWindow:`, so a real system tab strip is reachable — but it needs
one window (and one daemon connection) per tab, which this single-surface
client does not have.

### What the daemon still draws

The daemon is *not* a pure pane backend, and the wire protocol cannot make it
one. Only two regions are client-claimable:

| Region | Claimable? | How |
| --- | --- | --- |
| Workspaces sidebar | yes | `ShellDockLayout.owns_workspaces` |
| Session-name pill | yes | `ShellDockLayout.owns_session_chrome` |
| Tab bar (1 row) | no | always drawn; `l3s` covers it |
| Status line (1 row) | no | always drawn unless width-based mobile mode; `l3s` covers it |

There is no `owns_tabs` or `owns_status` flag, no `show_tab_bar`/`hide_chrome`
config, and no message that reclaims those rows for pane content. The tab bar
is always one row in the non-mobile layout, and the status line is one row
whenever `app.compact` is false (compact is derived purely from the viewport
being at most `layout.mobile_width` columns, so it cannot be requested without
losing width, and it also forces a single full-screen pane). Covering those two
rows with native chrome is therefore the closest possible realization of
"luvus renders only panes": the daemon's pixels there are overpainted, but the
rows stay reserved.

The daemon remains authoritative about dock geometry and theme; the client only
decides how to draw the rows.

## The UHP path

The binary transport above is only half the surface. Luvus also exposes
**UHP** (Universal Harness Protocol), its public automation API, on the same
owner-only `luvus.sock` we already use for the agent/bar/tab polls. The daemon
documents the binary frame protocol as "Luvus TUI rendering and input — private
implementation detail, not public automation"; UHP is what it offers a
"future UI".

The important method is `terminal.backend.observe`: a read-only stream that
sends one complete screen replacement immediately and then only when the PTY
emits output — no polling. Crucially it does **not** stream raw bytes. Each
`terminal.frame` carries the already-emulated screen as styled text: rows
joined by `\n`, colour expressed only through SGR, with no cursor addressing,
no scrolling, and no `\r`. `terminal.backend.capture` returns the same text on
demand, and `terminal.backend.control` sends input back.

So `l3s-client` needs no VT emulator for this path — only an SGR parser. That
lives in `ansi.rs` (`ansi::parse(text, lines) -> Screen`), which folds a frame
into rows of styled runs and which `--probe-uhp` uses to print frames.
`uhp.rs` wraps the socket: one connection per ordinary request
(`UhpClient::request`, `inventory`), the event subscription (`subscribe`), and
the long-lived observe stream (`observe`), surfaced as `Observer::next_frame`.

Known limits, read from the live `terminal.backend.capabilities`:

- no `stream_cursor` in this build, so an observe frame carries no cursor
  offset — one more reason the window paints the daemon's own grid, whose
  `FrameData` carries the caret;
- at most 8 simultaneous observe/control streams, 64 KiB and 200 rows per
  frame, a two-frame queue per stream (overflow is a `resync_required`).

`--probe-uhp [--pane ID]` prints one observed terminal: inventory, observe,
parse the first frame, print its rows. `--probe-feed` does the same for the
event-driven chrome feed.

The binary transport is what the window paints. The daemon emulates the
terminal and projects finished cells; the client never parses VT output.

## Mouse and selection

Pointer positions are in window pixels; the client divides by the measured cell
advance (and the header height) to get a frame cell. What happens next depends
on the gesture:

- **Click** (left press and release without moving): the press is recorded but
  nothing is painted, so a click never flashes a highlight. On release the
  client sends `Down` then `Up` to the daemon, so clicking focuses a pane. The
  daemon's own drag-select never runs, so there is no duplicate selection.
- **Drag** (left press, move, release): handled entirely client-side. Once the
  pointer leaves the anchor cell a selection appears; the client tracks the
  anchor and the moving end, tints those cells while painting, and leaves the
  daemon out of it. ⌘C copies the selected text.
- **Clipboard**: ⌘C copies the selected text; ⌘V reads the system clipboard
  and sends `ClientMessage::Paste`, which the daemon delivers under the pane's
  own bracketed-paste rules. Neither chord reaches the pane as a key.
- **Right/middle click** and **scroll**: forwarded straight through.
- **Hover**: a `Moved` event is sent only when the pointer crosses into a new
  cell, so links and scrollbars react without a message per pixel.

Selection lives in `gui/selection.rs`: it normalizes a reversed drag, answers
`contains`, and extracts text with trailing blanks trimmed per line.

## Not implemented yet

- Native file tree and Git views: the daemon still renders these into the grid,
  so they are visible but not client-native.
- Double-click to select a word, and triple-click to select a line.
- Named-pipe transport on Windows.
- SSH/remote endpoints and the federated machine catalog.
