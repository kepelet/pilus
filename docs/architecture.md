# Architecture

`pilus` is three crates with a one-way dependency chain, plus two that share
the connection layer from the other sides:

```
pilus (GPUI window)
  └── pilus-client (socket, handshake, event stream, Surface, UHP)
        └── pilus-protocol (messages, frames, framing)

pilus-agent (host connector) ──▶ pilus-client
```

`pilus-agent` is the host-side connector described under “Host connector”
below; it depends on `pilus-client`'s UHP surface and is never in the desktop
window's dependency chain. `pilus-mobile` sits on the phone side of that same
door: it enrolls over iroh, speaks the hub methods below, and exposes a C FFI
the iOS app (`ios/`) links against.

Nothing in `pilus-protocol` or `pilus-client` knows about GPUI, so the connection
layer is testable headlessly (see `crates/pilus-client/tests/handshake.rs`).

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

`pilus` attaches to the **render** socket (`luvus-client.sock`). `$LUVUS_SESSION`
selects a session, matching the daemon's own environment convention; `--session`
and `--socket` override it. When the derived path is too long for `sun_path`,
both the daemon and `pilus` relocate it to a hashed, owner-scoped alias under the
temporary directory (`discovery::alias_for`).

When nothing answers at the socket, the daemon isn't running, so `pilus` starts
it with `luvus [--session NAME] server start` and waits for the socket
(`autostart::ensure_server`, called from `connect`). The child gets
`LUVUS_HOME` pinned to the target's state root, so it starts the same daemon
`pilus` would have attached to. Liveness is checked by dialing, not by file
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
readable error. `pilus` paints its own surfaces and reports no terminal palette,
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
and never paints from a non-GUI thread. The task runs every 16 ms only while
something moves on screen (a tween, a toast, the cursor's fade); idle it ticks every
50 ms in front and 250 ms behind another app, and a daemon frame wakes it at once.

## Client-owned sidebar

By default the daemon renders the workspaces and agents docks into the grid
like any other panel. `pilus` takes the whole left sidebar instead.

**Workspaces.** After the handshake, `pilus` sends:

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
its current layout as `ShellSidebars`. `pilus` drops the `agents` dock from that
layout, bumps the revision, and sends it back. The daemon then renders no
agents dock, and the workspaces slot grows to fill the sidebar — the `ShellDock`
rectangle gets taller, which is where the native panel is painted. This is
per-connection state; the daemon's own config is untouched.

**Agents.** The live agent list comes from a different surface: UHP on
`luvus.sock`, method `agent.list`. `pilus-client`'s `ControlClient` speaks the
newline-delimited `{id, method, params}` protocol. Refreshes are event-driven:
`AgentFeed` holds an `events.subscribe` stream and re-reads `agent.list`,
`ui.bar.list`, and `tab.list` only when a structural event arrives, plus a
10-second reconciliation for state no event describes (the status bar's mode).
`terminal.output_ready` is deliberately not structural — acting on it would be
a poll with extra steps. Snapshots are deduplicated against the last one sent,
because one tab open emits several structural events. `--probe-feed` drives the
same feed headlessly. Clicking an agent row focuses its pane; clicking a
workspace row sends `ShellWorkspaceFocus`.

**Mission Control.** The Mission Control and Automations nav rows open a
native page (`gui/mission.rs`) in place of the daemon's `mission` tab. It
reads the daemon's own dashboard model with `mission.snapshot` every two
seconds while open (faster while a usage scan runs), and asks for a
`mission.refresh` scan when it opens, when the scope changes, and on
Refresh. The snapshot omits terminal text, so the line a blocked agent is
waiting on comes from `agent.read`. Row actions are UHP calls on a
background thread: `agent.prompt` (answer), `agent.keys` (approve,
interrupt), `agent.fork`, `pane.close`, and `agent.resume`, which is matched
back to `agent.sessions` by agent, workspace folder, and position because
the snapshot carries no session ids. Automations open a detail pane from
`automation.get`/`history`/`preview` with run and pause controls. Dismissing
a resumable session has no daemon API, so it is hidden on this client only.

**Right sidebar.** Cmd+B (View → Toggle Right Sidebar, or the panel button at
the tab strip's far edge) opens a column beside the terminal card holding three
tabs, Changes, History, and Files (`gui/right_sidebar.rs`). The daemon knows nothing of it: the
window reserves the column's width from the card, so the daemon sees a smaller
viewport and reflows once when the column opens, closes, or finishes a drag.
The width tween and the edge drag hold the viewport still until they settle,
as the left sidebar's tween does. Opening takes the keyboard
(`RightSidebar::focused`); a press on the terminal or Esc in the column hands
it back, and the focused column draws the one accent edge. Only Changes
reads keys, so choosing the History tab hands the keyboard back to the terminal.
Visibility, width, and the open tab persist in `settings.json`. The column is a
tab strip, the open tab, and (on Changes) a footer. Only the open tab polls
the daemon; a tab that comes back on screen asks for a fresh read.

**Changes.** A native, read-only review (`gui/diff.rs`, with its layout and
navigation rules in `gui/diff_model.rs`) over the daemon's `diff.*` methods.
The daemon owns the Git scan, the bounded patch parser, and the notes, and
answers for its active workspace, so the review follows the sidebar selection.
It polls `diff.list` and `diff.note.list` every 1.5 seconds on a background
thread while the column is open, fetches the selected file with `diff.get`
(`include_patch`) only when that file's fingerprint changes, and folds the patch
into source rows (hunk headers and lines) that the cursor and note anchors
address; Split and Stack are two layouts of the same rows. Auto splits only
while each side keeps about 56 columns, so the column normally stacks.

The header has two rows: what the review holds (branch, files changed, open
notes, with Refresh and the file filter), then the open file's folder and name
and the layout button. Below it the file list sits above the patch, separated
by a divider that drags vertically; its height is kept in `settings.json`. A
bar over the patch carries the change counts, a *Viewed* checkbox, and an
icon-only comment button. The footer shows the open file's path and, while
there are open notes, *Send to agent (n)*. If the column is dragged wide
enough (640px), the list moves beside the patch instead.

Hovering a line's number shows a `+` that opens the comment form on that line
(shift extends from the cursor's line); the comment icon, `n`, and Edit open it
too. Opening the form scrolls it into view over two frames, because a row just
inserted into the list has no measured height until it has been laid out once.

The rows sit in a virtualized variable-height `list`, so a long line wraps
inside its pane. Notes are written, edited, resolved, deleted, and handed to a
live agent (`diff.note.send`) through UHP calls that never touch the checkout.
The daemon has no method for the viewed flag, so Viewed is client state keyed
by the file's fingerprint.

**History.** The History tab, the repository's history (`gui/git_tab.rs`; data in `pilus-client`'s
`git_status.rs` and `commit.rs`) is two panes with a divider that drags
vertically (its height is kept in `settings.json`): the commit list on top,
and what the picked commit changed below. What is changed but not yet
committed is Changes', so the History tab does not repeat it.

The header names the branch with `↑ahead ↓behind` against its upstream and the
stash count. The list is the newest 50 commits (subject, up to two branch or
tag labels with the checked-out branch emphasised, hash, author, age), with
*Show more* adding 50 while the last read filled the page. The picked commit
is the panel pill, and the newest is picked when nothing is. The detail shows
the commit's message, author, date, and totals, then each file's hunks in a
virtualized list that reuses Changes' gutters and line washes, read-only and
stacked. Each file's header is a chevron that folds the file down to that header
and opens it again; the rows are laid out again with the file's patch left out,
and the next commit opens with every file expanded.

The history comes from the daemon: a background thread polls `git.status` and
`git.log` every 2 seconds while the tab is open, and the tab redraws only when
the answer changed. The daemon has no call for what a commit changed (its
`diff.*` methods describe the working tree), so a click runs `git show`
through `pilus_client::show_commit`, off the GUI thread, in the repository
`diff.list` reports and on the active machine: here, or over its saved SSH
target, where every argument is quoted for the remote shell. A machine reached
any other way says commit contents are unavailable. The read is bounded (2 MiB
of output, 4000 changed lines, 2000 characters a line) and marks the commit
truncated past that; a merge shows its changes against the first parent; the
hash is checked to be hexadecimal before it reaches Git. A pick the new
history no longer holds (another workspace) is dropped for the newest commit
of the new one. A repository with no commits yet shows an empty list: a failed
log read counts as an empty history. Nothing here stages, commits, or checks
anything out.

**Files.** The Files tab (`gui/files_tab.rs`; data in `pilus-client`'s `files.rs`)
draws the tree the Luvus TUI shows. The daemon owns one tree per active node and
shares it with the TUI: `files.tree` returns the visible rows already flattened
with a depth, so the tab follows the sidebar's selection and is native on
remote machines too. A background thread polls it every 2 seconds while the tab
is open, so a change made in the TUI appears here.

The daemon can open a folder but not close one. `files.reveal` expands only the
*ancestors* of a path, so opening a closed folder reveals a child name no real
file carries (the daemon does not check the path exists). Opening therefore
changes the TUI's tree too. Closing is this window's own state: a set of folded
paths that hides the rows below a folder (`visible_rows`) and never reaches the
daemon. A fold is forgotten once the daemon shows the folder closed, so a
folder the TUI closes and reopens comes back open, and a folder the daemon has
closed always expands on click. A click on a file calls `files.open` with the
`preview` target (a double click uses `pane`); the file shows in the terminal
card, and the tab never reads file contents. A folder being opened shows three
skeleton rows under it, with its chevron already turned. The daemon answers an
expand before it has read the folder, so the first reply has the folder open
and empty: the rows stay, and the tab asks again every 120 ms, until the
children arrive, the daemon reports the folder closed, an error lands, or 3
seconds pass (an empty folder). Clicking the folder meanwhile does nothing.

The tree rows carry no Git state, so each read also calls `diff.list` (the
Diff's own change list; a failure reads as nothing changed) and
`pilus-client`'s `FileChanges` keys it by absolute path. A changed file shows
its status letter at the trailing edge, colored as in Changes
(`diff::status_glyph`), and every folder above a change shows a dot. A click
on a changed file opens the Changes tab on that file instead of previewing it.
The header names the whole directory, `~`-shortened against the endpoint's
home, ahead of the refresh button.
Each answer carries a sequence
number so a slow poll cannot overwrite the tree an expand returned. Rows are
drawn with `style::tree_row` (catalogued in the playground) in a virtualized
list.

**Right-click menus.** WORKSPACES and AGENTS rows carry the daemon's native
context menus, drawn by a floating GPUI panel mirroring the daemon's own menu
structure: close, rename, pin, path visibility, and copy path/branch for
workspaces; rename, close, pin, path visibility, and the dock's workspace
scope for agents. Destructive rows hang on the daemon's contract where one
exists. Most items ride the control API — `workspace.close`/`rename`/`pin`,
`pane.close`, `agent.name`, `worktree.create` (targeting the right-clicked
workspace's repo via its index) — and run on a background thread reported
through a menu-status channel, so a remote round trip never blocks the window.
Boards first focus the workspace. `Open Git Tab` then sends the daemon's
prefix chord (`Ctrl+Space` + `g`); `Open Task Board` and `Open Mission
Control` open the native Tasks and Mission Control pages instead, the latter
pinned to that workspace. The presentational rows are
client state, exactly as they are in the daemon's own TUI: pins float the row
to the top (the workspace pin is additionally pushed through `workspace.pin`,
so the daemon's next projection reorders the catalog), path visibility drives
the second line of every row in the section and is hidden until the user
picks **Show Path** (both toggles persist in the client's `settings.json`;
the workspace one is additionally mirrored to the daemon through
`ShellDockLayout.workspace_paths`), and the agents scope filters the dock to
the visible surface's active workspace. Machine names stay out of the sidebar
rows and are named in the row tooltip instead.
**Delete Worktree** is the one destructive row, so it is gated and confirmed:
`worktree.list` is probed once per workspace when its menu opens (successful
answers cached per endpoint + stable workspace id; the row is added only after
a positive answer, and a failed probe is never cached so the next open
retries), and choosing it swaps the prompt for a read-only confirm modal.
Running it checks, like the daemon's own TUI, that no *other* open workspace or
agent pane lives inside the checkout, then `worktree.remove { path, force }`
removes the confirmed checkout root and folder; the daemon closes the workspace
whose cwd was inside it, so the row disappears on the next projection. The first
confirm never forces: it asks git (`status --porcelain`, locally or over SSH)
whether the checkout holds modified or untracked files, then sends
`worktree.remove` with `force: false`. The daemon answers `worktree_in_use`,
naming working or blocked agent panes, unfinished bound tasks and leases, when
the worktree is busy. Either finding holds the delete for a second, stronger
confirm ("Delete Worktree Anyway") that lists the blockers and the lost
changes. Only that answer discards work (`git reset --hard` and `clean`, since
the daemon refuses a dirty checkout whatever `force` says) and retries with
`force: true`, which stops the panes in the removed workspace. A clean, idle
worktree deletes on one confirm; an unreadable git state aborts instead of
being taken for clean.

Renames and aliases also update their projected row optimistically, and the
authoritative projection confirms them a tick later.

**Last tab.** The daemon closes a workspace when its final tab closes, so the
client holds back its own last-tab close (⌘W / the tab ×) with a status hint.
Exiting the shell itself still closes the workspace; that is daemon behavior.

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
| Tab bar (1 row) | no | always drawn; `pilus` covers it |
| Status line (1 row) | no | always drawn unless width-based mobile mode; `pilus` covers it |

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

## Machines

Saved SSH profiles turn one window into several endpoints. A profile is
owner-local to the local named session (`machines.json` under the state root)
and is read with `luvus machine list --json`; the CLI remains the authority for
provisioning and reachability, so `pilus` never installs or replaces a remote
binary itself.

Each enabled machine becomes one endpoint:

- **Transport.** `pilus-client` spawns `ssh -T -o BatchMode=yes …
  <destination> <remote_binary> [--session NAME] remote-client-bridge` and
  treats the child's stdin/stdout as the same `Stream` a local socket uses
  (`transport::SshTarget`, `machines::connect_machine`). Stderr is drained on a
  helper thread and folded into handshake errors, and the last `Connection`
  clone kills the SSH child so a reader parked in `read` unblocks.
- **Auto-connect.** Every enabled profile is bridged at startup and whenever
  the catalog gains or re-enables one. Each bridge is its own thread, so the
  whole catalog comes up in parallel; an attempt is recorded, so the slow
  catalog poll never restarts a healthy host, retries a failed one, or probes
  over SSH while every enabled machine is already connected.
- **Reconnect.** A dropped bridge is retried with capped exponential backoff
  and jitter (1 s doubling to a 30 s ceiling), one attempt per endpoint and
  none while one is in flight. Retries continue until a link sticks; a host
  that answers its probe again has its backoff budget reset so recovery is
  prompt. The owner-local daemon is an endpoint too: a stopped server is
  re-attached with backoff and `ensure_server` restarts it, so Local recovers
  on its own. A reconnect of the endpoint already on screen is swapped in
  place, so its surface is never blanked.
- **Sidebar and manager.** Workspaces across Local and every connected machine
  render as one flat list (a remote row carries a small server glyph and the
  machine name) rather than grouped headers; agents stay grouped by endpoint
  below it. A monitor button beside the settings gear opens a native Remote
  Machines window that lists profiles with reachability and shells out to
  `luvus machine add/enable/disable/remove`, bumping a shared revision so every
  open window reloads the catalog at once.
- **Open Workspace.** `⌘⇧O`, the File menu, and the `+` all open a native
  folder picker instead of the daemon's own. A host dropdown at the top with an
  online/offline dot switches which endpoint is browsed (Local or any enabled
  machine) without changing the visible surface. Local reads the filesystem
  directly; a machine lists directories over the same SSH client (`cd`, `$PWD`,
  and `find -print0`, NUL-separated). A leading `~` is expanded on the browsed
  host, so a workspace path the daemon shortens to `~` still resolves. The list
  leads with Home and `..`; `←` goes up, `→` steps back through visited
  directories, and `⇧.` toggles hidden entries. Opening the chosen folder is one
  `workspace.open` control request to that endpoint, while **Open with new
  worktree** generates a branch name and issues `worktree.create`, so the daemon
  still owns workspace and worktree creation.
- **Agents.** Every connected endpoint also runs a hidden-endpoint feed (one
  `agent.list`, `tab.list`, and `ui.bar.list` per reconciliation, no full
  chrome refresh), so the AGENTS dock lists live agents from Local and each
  hidden machine, grouped by endpoint, and the tabs and status bar a later
  switch swaps in are already current instead of stale or empty.
  The row whose pane currently holds focus is highlighted (optimistically on
  click, then confirmed by `agent.list`). Clicking one focuses its pane,
  activating the owning endpoint first when it is hidden.
- **Surface.** The remote server hands back `Welcome`/`Ready`/`EndpointIdentity`
  exactly like the local one, so the handshake, frame loop, and workspace
  projection are unchanged.
- **Interest.** Inactive endpoints are sent `SurfaceInterest::Suspended`: their
  workspace catalog (`ShellWorkspaces`) still arrives, so their rows render,
  but they stream no frames and own no input, cursor, or PTY size.
- **Switch.** Selecting an endpoint sends `SurfaceInterest::Prepared` and a
  ticket-tagged `PrepareSurface`; the matching `PreparedFrame` is committed by
  swapping the endpoints' surfaces, then `SurfaceInterest::Active` and a
  `Resize` go to the new one. The clicked row highlights optimistically until
  the handoff commits or every stage of the switch ends without one, so a
  slow SSH round trip never reads as a dead click. A stalled handoff commits
  after a bounded timeout only when the target holds a frame — a timed-out
  destination never blanks a usable surface — and re-selecting the visible
  endpoint cancels it. Window resizes are pushed to every bridge, not only the
  active one, so a hidden machine's panes and PTYs stay at the window's size
  and switching to it never visibly reflows the terminal. (The events exist
  in `ServerMessage`/`ClientMessage`: `PreparedFrame`, `SurfaceInterest`,
  `PrepareSurface`.)
- **Chrome.** The active endpoint's tabs, agents, and bar widgets come from its
  own control surface. Local uses the Unix control socket; a machine reaches
  `luvus uhp proxy` through the same SSH options, one request per invocation.
  Because `uhp proxy` forwards a single request, a remote feed reconciles on a
  short timer instead of holding an `events.subscribe` stream. An active feed
  also runs an agents-only lane every ~1.2s — one request where a full pass
  is five.

There is no daemon-side machine method: the window owns every SSH process, and
two clients attached to the same local session load the same profiles without
sharing a bridge.

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

So `pilus-client` needs no VT emulator for this path — only an SGR parser. That
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

## Host connector

`pilus-agent` is the *transport provider* the UHP docs leave to the
application. The daemon opens no TCP listener, and its access gateway binds
loopback only, so a remote client needs a host-side process to launch the
gateway, carry its bytes, and — later — authorize devices. That process is the
connector.

Its contract is narrow, and deliberately so:

- `Gateway::start` launches `luvus [--session NAME] uhp access [--machines]
  [--control] [--ttl N] [--no-expiry]`, reads the one-line descriptor, and
  keeps the child alive. Dropping the gateway kills the child and revokes the
delegated token.
- `Descriptor::parse` reads that line against the installed
  `access.descriptor` schema: `protocol` for the version, `endpoint.host/port`
  for the address, `pairing.code`, `authority.{mode,scopes,expires_at}` for the
  granted authority, and `features.machines`. Additive fields are ignored, so a
  newer daemon does not need a new connector. `tests/live_gateway.rs` exercises
  a real gateway when one is installed.
- `proxy` is one transport: a TCP listener that forwards each accepted
  connection to the gateway loopback, one upstream connection per client to
  match UHP's one-request-per-connection contract. The connector never parses
  or rewrites UHP frames past the pairing frame.
- `p2p` is the other, and the one a phone uses: an iroh/QUIC endpoint
  (`ALPN = b"luvus/uhp/1"`) whose identity is a persistent Ed25519 key under
  the state root. Each accepted bidirectional stream is bridged to its own
  gateway connection, so one QUIC stream is one UHP request. An `EndpointHooks`
  allow-list rejects peers by `EndpointId` after the handshake, and `serve`
  prints an `EndpointTicket` a device scans to enroll. Relays default to n0's
  public servers unless `relay.url` in `~/.pilus/settings.json` (or a
  repeatable `--relay`, which wins) names a dedicated or self-hosted one. A
  managed relay from services.iroh.computer also needs `relay.secret`, the
  project's API secret: it is not a locator, it only lets the endpoint mint the
  relay token its key is bound to, so the URLs are still required. A secret
  alone keeps the n0 relays and adds the token. The file is `0600` because it
  can hold the secret, and a `relay` block that fails to parse stops the agent
  instead of falling back to the public relays:

  ```json
  { "relay": { "secret": "…", "url": ["https://…"] } }
  ```

`pilus-client` gained the seam this needs: `Stream::connect_tcp` and
`ControlTransport::Tcp`, plus `UhpClient::{connect_tcp, with_auth,
capabilities}`. `auth` rides every envelope when a token is set, and
`Capabilities` parses the `access.mode`/`allowed_methods` projection a paired
gateway adds. These are additive: the owner socket path is unchanged.

`tests/live_gateway.rs` proves the descriptor, pairing, and capability
projection against a real `luvus uhp access`, and `tests/iroh_bridge.rs` proves
the whole broker: an unenrolled device is refused on the UHP ALPN, enrolls over
the control ALPN with the printed code, receives the delegated token, and then
calls `uhp.capabilities` with it. Both skip when no `luvus` binary is installed.

A gateway's pairing code is one-use and short-lived, so a device cannot carry
it. `enroll` takes over: the connector pairs once with the gateway, keeps the
delegated token, and hands it to devices it has admitted. A device proves
itself once with a short code shown on the host over the `luvus/agent/1`
control ALPN (`{"type":"enroll","code":…}`), and its iroh `EndpointId` is
persisted as the authorization from then on. Each successful new-device
enrollment rotates the host-visible code, so the spent code is dead the moment
one endpoint lands; the host prints the fresh code for the next device.
`{"type":"token"}` returns the cached token for an enrolled peer; authorized
peers can also list and remove enrollments with `{"type":"devices"}` and
`{"type":"revoke"}`. Passing `--allow` with at least one device ID makes the
run strict: only the configured IDs are authorized, the persisted store is
never read, and enrollment is disabled. The UHP ALPN rejects unauthorized
peers; when enrollment is enabled, the control ALPN stays open so a new device
can present its code. The connector runs the gateway `--no-expiry`, because it
pairs once and must hold that token for its whole run.

The connector's own state lives in `~/.pilus/agent` (`$PILUS_HOME/agent`), never
under the Luvus state root: `identity.key` (the host's iroh identity, which paired
phones have pinned), `devices.json`, the admin socket `agent.sock`, and
`agent.log`. Older builds wrote these to `<luvus root>/agent`; `serve` and
`service install` move `identity.key` and `devices.json` across on first run and
never overwrite a file already at the new path.

### Running as a background service

Remote access is only worth having if it is there when the window is not, so
the connector runs as a per-user service, not as a child of the window.
`pilus-agent service install` writes a LaunchAgent (macOS) or a user systemd
unit (Linux) that runs `pilus-agent serve --iroh --control --service` at login
and restarts it on failure; `service uninstall` removes it and `service status`
reports it. Nothing needs root. `install` is idempotent and leaves a running,
unchanged service alone, because the window calls it on every launch and
bouncing the service would drop every connected device. The service gets a
small explicit `PATH` and `LUVUS_BIN`/`LUVUS_HOME` recorded at install time,
since launchd and systemd hand services almost no environment.

`serve --iroh` supervises its own gateway (`supervisor.rs`). It starts
`luvus uhp access`, pairs, publishes the token to `AuthState`, and when the
gateway child exits (daemon restart, sleep and wake) starts another with capped
backoff. Enrolled devices fetch the token on demand, so none re-enrolls. If the
Luvus daemon is not answering `ping`, the supervisor runs `server start` (at
most once a minute), because after a reboot nothing else has. The iroh handlers
read the gateway address per stream, so a restart needs no handler rebuilt, and
while the gateway is down a stream gets a UHP-shaped `unavailable` error rather
than a stall. `--service` also keeps enrollment codes out of the log.

The identity key, `devices.json`, the admin socket and the log all live under
`<state root>/agent/`. The identity and the device list persist, so the
endpoint id and every enrolled device survive restarts. The **enrollment code
does not**: it exists only in memory, lasts ten minutes, and is replaced after
each enrollment. The service therefore never needs a restart to hand one out;
`new_code` on the admin socket mints a fresh one on demand.

A running service keeps the binary it started with, and an update replaces the
file under the same path, which `install` rightly leaves alone. So `status`
reports `restart_needed` (the executable on disk is newer than the process), and
the window, at launch, runs `pilus-agent service restart` when it is set
(`launchctl kickstart -k`, or `systemctl --user restart`). Devices redial by
themselves. On Linux a user unit stops with the last login session unless
lingering is on, so `status` also carries `linger` (from `loginctl`), and Settings
shows `loginctl enable-linger` when it is false. Windows has no service
implementation; Settings says so instead of showing a switch.

### The admin socket

`<state root>/agent/agent.sock` (`0600` in a `0700` directory) answers one JSON
request per connection: `status` (endpoint id, current ticket, gateway health,
enrolled devices, never a code), `new_code` (a code plus the
`pilus://pair/<ticket>?code=…` link a QR encodes; it carries no `name`, so the
device labels itself), `set_level`, `revoke`,
and `retry`, which cuts the supervisor's backoff short and starts the Luvus server
now (the window's *Start Luvus*; a person asking overrides the once-a-minute
limit). It is the only way the window reads the service, and it is why the window needs no stdout.
`pilus_client::agent_service` is the client; `gui/connector.rs` caches its answer
from a poller thread, so the GUI thread never touches the socket or runs the
installer.

### The hub

A phone should pair with the machine it trusts, not with every host that
machine can reach. The hub (`hub.rs`, ALPN `luvus/hub/1`) is that one front
door. An enrolled device sends `{"id","machine"?,"method","params"?}` and gets
`{"id","result"}` or `{"id","error":{"code","message"}}`; `hub.machines` lists
this machine plus every saved SSH machine. Four more methods are the hub's own:
`hub.agents`, `hub.folders`, `hub.run` and `hub.watch`, described below. `local` (the default) goes to the
gateway with its delegated token. Any other id is a saved machine, reached the
way the window reaches it: `luvus uhp proxy` over the user's own SSH. Luvus's
`--machines` gateway flag is only the catalog (`machine.list/status/sessions`),
not a router, which is why the fan-out lives here.

What a device may call depends on its **level**, stored per device in
`devices.json` (a file without levels reads as `control`, so existing pairings
keep working):

- **view**: the read-only subset of what the gateway allows (lists, status,
  snapshots, reads; anything the classifier has not heard of counts as a write).
- **control**: the gateway token's `allowed_methods`, plus the diff review
  writes the gateway refuses (`diff.refresh` and `diff.note.add`, `edit`,
  `resolve`, `reopen`, `remove`, `send`), which the hub runs itself through the
  owner socket. Prompt and interrupt agents, retry tasks, focus things, and
  review a diff. None of the review calls runs code: a note is plain text in
  the daemon's store, and `diff.note.send` types into an agent pane the way
  `agent.prompt` already does.
- **operate**: control, plus `task.add`, `task.start`, `agent.start` and
  `workspace.open`. The gateway refuses these, so the hub runs them itself: on
  this machine through Luvus's owner socket, on a saved machine through
  `luvus uhp proxy` over SSH (whose owner socket accepts them natively).

Operate is remote code execution by design (a task's agent and gate command, an
agent, a shell in a new workspace), and on an SSH machine it is exactly as
powerful as sitting at that machine. So it is opt-in per device, and a level can
only be **raised by a live enrollment code at that level**, which exists only on
the host: *Add device* with "Allow this device to start work" ticked. The
admin socket can only **lower** a level (Settings: the View / Control / Operate
control on a paired device), and a lowered or revoked device is refused on the very next request
of a connection that is already open, because the level is looked up per stream.
Re-enrolling an already-enrolled device with a live code at another level
re-levels it; presenting no code or a dead one changes nothing, so a plain
re-pair never promotes. The hub rewrites `uhp.capabilities` so each device sees
its own allowed methods and `level`, not the gateway token's.

Every request, local or remote, is checked against these lists, and an empty
gateway allowlist **fails closed** at every level. Methods that stream or block
(`events.subscribe`, `wait.*`, `terminal.backend.observe`, …) are refused at
every level, because a stream carries one request and one reply, with one
exception the hub makes itself.

**Diff review on the phone.** The daemon's gateway carries only the three
read calls (`diff.list`, `diff.get`, `diff.note.list`), so every device sees
the diff and `control` and `operate` devices can also write and send notes.
The iOS app learns which from the `allowed_methods` the hub rewrites into
`uhp.capabilities`: `AppStore.canViewDiff` shows the review button on an
agent's screen, and `canReviewDiff` turns on tap-to-comment, resolve, edit,
delete, and *Send to agent*. `DiffReviewView` is a native SwiftUI list of the
changed files by layer and a per-file patch (a virtualized `LazyVStack`, long
lines wrapping) that decodes the daemon's replies leniently, so an unknown
layer or note kind never fails a list. A first tap on a line anchors a note
and a second extends it on one side (deletions anchor to the old file,
everything else to the new one); notes hang under their last line, and notes
whose code moved show detached at the end. Like the desktop tab, the review
follows the workspace the daemon has active, so when the agent's folder is not
under the reviewed repo the screen says so and offers *Show this agent's
workspace*, which runs `workspace.focus` (the Mac's own window follows, which
is why it is a button). The screen re-reads every five seconds while it is up.
Editing or deleting a note sends the revision the notes were read at as
`if_revision`; on `revision_conflict` the app reloads, and goes again only when
that note is unchanged, so a note edited on the Mac is shown instead of
overwritten.

**Lost replies on the phone.** A connection can drop after a request was sent,
and the phone cannot tell whether it ran. `pilus-mobile` reports that as
`unconfirmed` and resends only reads and calls carrying an `idempotency_key`
(`hub.run`, which the hub dedupes for ten minutes, `automation.create` and
`automation.run`). A prompt, key press, note, or workspace open is never
resent: the app keeps the user's text, says it may have landed, and reads the
state back. Errors reach Swift as `{code, message, dispatch}` so the app
branches on the daemon's codes (`revision_conflict`, `agent_not_ready`,
`stale_server`) rather than on message text. Terminal writes carry the
`server_generation`/`terminal_id`/`pane_id` identity, `agent.prompt` carries
`terminal_id`, and a pane that now holds another terminal is refused, never
followed.
Sending lists the machine's live agents, the one the review was opened from
first, and covers one note, one file's open notes, or all open notes.

`hub.folders` (operate only) lists the child folders of a path on a machine (`~` by
default), marking git repositories, so a phone can choose a workspace to open with
`workspace.open` instead of typing a path. It uses `pilus_client::folders`, the same
lister as the window's Open Workspace picker (locally, or one `ssh` round trip
running a POSIX script). Opening focuses the new workspace on the Mac: `workspace.open`
has no no-focus option, unlike `task.start`.

`hub.run` (operate only) starts an agent in a workspace and gives it a prompt, in one
request: the same steps as the window's launcher (`workspace.open` to focus it, reuse
the idle pane, `agent.start`, `agent.prompt`) on a single control connection, with one
audit line (`hub.run`; the prompt is never logged). The agent gets an alias of its own
(`phone-<kind>-<workspace tail>`), and a later `hub.run` for the same kind and workspace
prompts that agent instead of starting another. The alias is what finds it, because
Luvus can label a process as a different kind (a Pi pane reads as `opencode`). It
focuses the workspace on the Mac, since an agent spawns beside the focused pane.
`model` and `effort` params resolve to launch flags on the machine that owns the
catalog (`AgentConfig::launch_args`), so a kind without one launches on its
defaults. `terminal` is the one **pseudo-kind**: instead of `agent.start` it
opens a fresh shell tab in the workspace and types the prompt through
`pane.send_input`, so a device can run plain commands without pretending a kind
exists the daemon does not know.

**GitHub.** Luvus has a GitHub screen of its own (prefix + g) but exposes none of it over UHP: of
its methods only `git.status`, `git.branches`, `git.log` and `git.open` touch git, and
none lists an issue or a pull request. So `pilus` asks the GitHub CLI itself, on the
machine that owns the checkout, as it already asks `git`, `lsof` and `ssh`.
`pilus_client::GhRepo` is the one implementation: a checkout directory plus an optional
`SshTarget`, and every call is one `gh` process run from a small POSIX script (so the
login shell does not matter, `~` expands on the host, and `PATH` gains the usual install
directories that a Dock-launched app or a non-interactive SSH shell lacks). Text a
person typed reaches `gh` on stdin through `--body-file -`, never on a command line.
`probe` says which setup step is missing (`missing`, `signed_out`, `no_repo`) as a
sentence, rather than failing; the host needs nothing but a signed-in `gh`.

- **Window.** The GitHub tab in the right column (`gui/github_tab.rs`) follows the active
  workspace's directory: issues and pull requests (open, closed, merged), each with its
  description and comments, a comment field, and **Submit this branch…**, which proposes a
  title from the commits and pushes only that branch if the remote lacks its commits
  (the form says so). A branch that already has an open pull request shows it instead of
  creating a second. The Home composer's **Issue** chip lists the target workspace's open
  issues, read when the picker opens, and attaches one: `Issue::attach_to` appends it to
  the prompt (`GitHub issue #n: title`, url, body to 8,000 characters), and an attached
  issue makes the prompt optional. That function is the seam for a later "Send to agent"
  on an issue or pull request.
- **Phone.** `pilus-agent`'s hub answers `hub.gh.status`, `hub.gh.issues`, `hub.gh.issue`,
  `hub.gh.pulls`, `hub.gh.pull`, `hub.gh.pull_draft` (a **Control** device may read) and
  `hub.gh.issue_comment`, `hub.gh.pull_comment`, `hub.gh.pull_create` (an **Operate**
  device only: they act as the host's GitHub account, and every one is audited without its
  text). All take the checkout's `path` and a `machine`, locally or over SSH like
  `hub.folders`. They are advertised in the level's method list, which is how the app
  decides whether to show GitHub at all. The mobile client retries only the reads, because
  a resent comment would be posted twice. The iOS app has a GitHub sheet on an agent's
  screen and an issue picker in both launch composers.

`hub.agents` (operate only) lists the agents **installed** on a machine, for a
picker: the same probe the window's launch composer uses (`agents::local`, or
`agents::remote` in one SSH round trip), cached for a minute. Each entry carries
its model catalog (`models` with names, efforts, and defaults, plus
`default_model`) so a remote picker can offer models without probing the
machine itself; kinds that report none simply hide the picker. A separate
`pseudo_kinds` list advertises the kinds `hub.run` accepts beyond real adapters
(`terminal`) — a client gates its picker on it, since a connector that predates
it would forward the name to `agent.start` and fail with an unknown-kind error.
Luvus's own
`server.agent_manifests` is not a substitute: it names every agent Luvus knows how
to launch (23 of them), installed or not. The picked kind goes to `task.start` or
`agent.start` as `agent`.

`hub.watch` is the one method whose stream stays open (`watch.rs`). It subscribes
to the machine's Luvus event feed and sends the device **topic signals**
(`{"event":"changed","topics":["tasks","mission"]}`), never events: an event
carries a task's prompt or a pane's cwd, so relaying it would be a second,
unaudited way to read everything, whereas a topic reveals nothing and any enrolled
device may watch. The device then refetches the affected list through the ordinary
calls, where its level applies. Terminal output events (one per burst of output)
become the `output` topic plus the ids of the panes that wrote (`"panes":["3"]`),
never their text, so a device looking at one agent's screen knows when to re-read
it with `agent.read`, which its level governs; no list is refetched for `output`.
The iOS app refetches only the lists a signal names, for the paired machine only
(saved SSH machines refresh on a 45 s timer), re-reads an agent's screen when its
pane is named, resyncs everything on reconnect or when `seq` goes backwards (the
daemon restarted), and waits 60 s for a frame before calling the stream dead.
Signals are coalesced to one per 300 ms, an idle stream pings every
20 s, and each ping rechecks that the device is still enrolled, so a revoked
device's open stream is cut. A device holds at most two streams and the machine
eight, because the gateway token's rate limit (120 requests a minute) is shared by
every device. Only the paired machine streams: a saved SSH machine is reached one
request at a time, so its device keeps polling. Every write a
device makes, and every refusal of one, is logged as
`audit: <device> [<level>] <machine> <method> <outcome>`; parameters are never
logged because they may hold prompts. SSH calls are capped at six at a time and
the machine catalog is cached for ten seconds. A service started by launchd
inherits `SSH_AUTH_SOCK`, so keys loaded into the user's agent work; a key that
needs a passphrase prompt will not.

### Settings

The **Settings** page puts a **REMOTE ACCESS** section above **REMOTE MACHINES**,
with a `style::toggle` switch backed by `Settings.connector`, **off by default**.
Switching it on installs and starts the service; off stops and removes it.
Quitting pilus does **not** stop it. While it runs the section shows its health
(`Running`; a **Restart** button runs `service restart` so a new build is picked up
without switching Remote access off and on; `Waiting for Luvus`, with what to do and a **Start Luvus** button; or
`Luvus not found`, where retrying cannot help), an **Add device** button
and the paired devices, each showing its level and when it paired (UTC), as a segmented View / Control / Operate control (only lower levels can be picked; raising takes a new code) with **Unpair**. Add device mints a code and shows it as a
QR (`style::qr_code`, always dark on light whatever the theme, because a scanner
needs the contrast), with Copy rows for the code and the link, the *Allow this device to start work*
choice (off every time; toggling it mints a new code, since the level is part of
the code), an expiry countdown, and a **Done** button; the card hides itself when
the code expires. Below it, a **SERVICE LOG**
disclosure that tails the last lines of `agent.log` (polled only while it is open)
with a Copy button.
Each machine row in that page carries enable/disable/remove and the Luvus upgrade actions (Update, then Restart,
or Stop & upgrade for a daemon too old to handshake); the **Add Machine** button
opens a window that holds only the add form, which keeps what was typed when the
add fails.

The connector must be found before any of this works: `scripts/bundle-macos.sh`
builds and copies `pilus-agent` into `Contents/MacOS/` beside `pilus`, and
`just build`/`just run` build it too, so the app can start it as a sibling. The
service's plist records that path, so moving or replacing the app is picked up
at the next launch, when `install` rewrites the plist.

`tests/service.rs` runs the real binary against a real Luvus daemon in a
throwaway `LUVUS_HOME`, with every ambient `LUVUS_SOCKET_PATH`/`LUVUS_API_ADDRESS`
scrubbed from the children (a pane exports them, and they override `LUVUS_HOME`,
so without the scrub the test would drive the developer's live session): it proves the gateway comes up by itself, that a code
minted over the admin socket enrolls a device, that the hub serves the local
machine and refuses what the gateway refuses, and that after the gateway is
killed the same enrolled device is served again with no new code. It also covers
the levels: each device is told its own methods, operate adds a task and opens a
workspace in the daemon, control and view are refused the same calls, lowering
applies at once, and a level cannot be raised from the admin socket.

## sipil: people and agents over iroh

sipil is the second thing pilus talks to besides a Luvus daemon, and it is not
the Luvus protocol. Every sipil is a node on one iroh protocol, whether it is the
durable agent daemon (`../sipil`) or another pilus window, so a pilus can chat
with a person and drive an agent through the same code path.

```
pilus window ──┐                       ┌── sipil daemon (Node, @number0/iroh)
  Node (iroh)  ├─ ALPN pilus/sipil/1 ──┤     agents, sessions
               │  one JSON call/stream └── pilus window (PeerService)
               │                             health, chat.send
```

A call is one bidirectional QUIC stream: the caller writes
`{"method","params"?}` and finishes, the answerer writes `{"result"}` or
`{"error":{"code","message"}}` and finishes (`pilus_client::sipil::wire`; the
daemon's side is `sipil/src/transport.ts` and `rpc.ts`). The caller is the device
ID the handshake proved. `health` is open and reports `kind` (`daemon` or
`pilus`) and whether the asker is allowed; everything else is refused with
`unauthorized` unless the asker is on the allow-list. Revision 2 of the protocol
replaced the daemon's HTTP API (revision 1), which is gone.

- **Device ID.** An iroh `EndpointId`, 64 hex characters, from an identity key
  (`~/.pilus/sipil/identity.key` for a window, `~/.sipil/identity.key` for the
  daemon). It is what a person sends to be added, and what `npm run connect` takes.
  A ticket adds addresses and is accepted wherever an ID is; a bare ID is found
  through iroh's discovery. Relays follow `relay.url` in `~/.pilus/settings.json`
  (the window ignores `relay.secret`) and `SIPIL_RELAY` for the daemon.
- **Authorization.** On a daemon, `peers.json` in its data directory, edited with
  `npm run allow|revoke|peers` and read on every connection, so a revoked device
  is refused on its next call. A pilus gets onto it by being asked: `sipil
  connect` sends `pair.request` (kind, label, ticket) to the pilus, which holds it
  as a request until its person accepts, answering `pending`, `accepted` or
  `declined` each time the daemon asks again; the daemon allows the pilus only on
  `accepted`. On a window, the contact list, where an accepted daemon is a
  contact of its own kind. Neither is a
  password: knowing a device ID grants nothing until the other side adds it.
- **Chat.** `chat.send` carries a `requestId`, so a retried delivery is one
  message. Each side keeps its own thread (`chats.jsonl`, one record per line); a sender stores its line
  only after the receiver took it. A stranger's message is dropped and the
  stranger becomes a request in the window.
- **Watching.** `watch {id}` is the one call that answers many times: the daemon
  keeps the stream open and writes one JSON object per line, `{"event": ...}` for
  each thing that happens in the conversation, then `{"result": {}}` when it ends.
  The first event is a `snapshot`; `item`, `delta`, `live`, `status` and `subagent`
  apply on top of it, and `pilus_client::sipil::Watched` is the reducer that holds
  the result. The window opens one stream for the conversation it is on
  (`gui/sipil.rs`, `stream_conversation`), reopens it when it breaks or goes silent
  for 45 seconds (the daemon pings every 15), and shows the answer being written
  from `SipilState::live`. A running call's output arrives as `output` events
  (the end of what it printed, replacing the last; the snapshot lists the calls
  running), kept in `SipilState::running`: the call's header shows its last line,
  and opened it shows the tail, until the call's result line arrives. While a stream delivers, the poller does not read that
  conversation's transcript; it still refreshes the lists. A daemon from before the
  call answers `unknown_method` and is polled as before. `transcript` and
  `sessions` also take paging params (`before`/`limit`, `after`/`limit`) and answer
  `more`. The chat page shows a "Load older messages" bar above the log while the
  daemon has lines before the first one held; `pilus_client::sipil::history` keeps
  older pages when a newer one arrives, so a conversation that grows while open
  leaves no gap, and the list inserts the rows in front so the reader stays on the
  line they were on. The session list is not paged yet: the window shows the first
  200 sessions.
- **Agents.** A person may run several daemons, each with a named agent
  (`SIPIL_NAME`). A pilus does not hand an agent to a contact: it answers no
  `agent.*` call and `health` lists none. An agent takes part only in a channel
  its person invited it into, and runs only when a member names it.
- **Channels.** `chan.send {channel, name, members, requestId, content, as?}` is
  `chat.send` for a room. The author's pilus sends it to each member that is a
  contact; a pilus that takes one for a channel it does not know creates it from
  the name and members the message carries, and one that left a channel drops
  the message. Each member keeps its own copy of the thread (`#<id>`). The same
  call changes membership: `added` (device IDs a line invites, whom its readers may
  list again) and `removed` (IDs it takes out, who leave, and whom the others drop
  and keep out). The message carries `owner`, the device that made the channel;
  only the owner's lists and changes count, and a message from someone who is not in
  a known channel's list is refused with `not_member`.
- **Mentions.** In a channel, `@Researcher` in a line the user sends makes this pilus ask its
  own daemon's agent to read the conversation (the lines it has not seen, with who
  said each, in one session per conversation) and say its answer back with
  `as: "Researcher"`, provided the person invited that agent into the channel
  (kept on this device only, in `channels.json`). The same happens when any member
  names an invited agent in a line they send (`PeerService::invite_agents`); an
  agent that was not invited, or another agent's line, starts nothing. A direct
  chat between two people is never handed to an agent.
- **Absence.** A peer that does not answer is dialed on a growing delay
  (`pilus_client::sipil::Backoff`) and then left alone; a daemon is shown offline, a
  contact stays offline. What brings one back is news, not polling: a call from that
  device (any call from a known contact counts as it announcing itself, and a sipil
  daemon makes one to every allowed device when it starts), the person opening or
  writing to it, or the app coming to the front. Presence is only checked while the
  app is in front.
- **Lifetime.** The window's node lives and dies with the window. Reachability is
  `Settings::sipil_online`; moving the node into `pilus-agent` is the way to
  receive while the window is closed.

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

- Creating, renaming, or deleting files from the Files tab; it browses and
  opens only.
- Double-click to select a word, and triple-click to select a line.
- Named-pipe transport on Windows.
- Daemon upgrades: **Settings → ABOUT LUVUS** checks
  `luvus.dev/latest.json` and installs the verified platform archive itself
  (refusing a package-manager install), then restarts through
  `luvus server restart`. The **Update Luvus** button on a machine's Settings row
  re-provisions a host with `luvus machine enable <id> --install`, and its rows
  show the remote daemon version from the last probe.
