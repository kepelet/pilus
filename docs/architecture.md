# Architecture

`pilus` is three crates with a one-way dependency chain, plus a fourth that
shares the connection layer from the other side:

```
pilus (GPUI window)
  └── pilus-client (socket, handshake, event stream, Surface, UHP)
        └── pilus-protocol (messages, frames, framing)

pilus-agent (host connector) ──▶ pilus-client
```

`pilus-agent` is the host-side connector described under “Host connector”
below; it depends on `pilus-client`'s UHP surface and is never in the desktop
window's dependency chain.

`pilus-session` is the one crate outside that chain that has nothing to do with
the daemon: it reads pi's session JSONL — parse, discover by working directory,
and tail — so the window can paint a Pi conversation natively. It depends only
on `serde_json`.

Nothing in `pilus-protocol` or `pilus-client` knows about GPUI, so the connection
layer is testable headlessly (see `crates/pilus-client/tests/handshake.rs`);
`pilus-session` is likewise testable without a window
(`crates/pilus-session/tests/`).

## The native Pi Session panel

Pi renders its conversation in a terminal, but it also persists it as JSONL
under `~/.pi/agent/sessions/--<cwd>--/`. `pilus` paints that conversation as a
native panel inside the main window instead: `pilus-session` folds each line
into an ordered `Transcript` (a `message` entry's content blocks become prose,
thinking, and tool calls; tool results, shell runs, compactions, and model
changes become their own items), and `gui/session.rs` draws it, following
appended bytes on a background reader.

The reader is tolerant by design: unknown entry types and message roles are
counted and dropped rather than failing the file, so a newer pi still renders.
User turns sit on the right, assistant turns on the left, and thinking, tool
calls, and shell runs are borderless muted rows with a chevron that opens their
body (collapsed by default; no line counts). Thinking labels as `Thinking` and
renders at a smaller Markdown size. A model change is one plain muted line
(`Model changed to …`); thinking-level changes, compactions, and other
structural notes render as the same muted collapsible rows.

A tool result is paired with its call by `call_id` (in `pilus-session`). Each
stretch of thinking and tool calls between assistant replies collapses into one
`Working for 10m 3s` row (or `Working…` while it is the last thing in the
transcript, or when the entry timestamps give no span); a stretch that is only
thinking skips the wrapper and shows the `Thinking` row directly. Opening a
work row lists the thinking and the calls, and a run of consecutive calls
groups further under a prose summary (`Read 3 files, ran 1 search, ran 1
command`). Calls themselves label as verb phrases (`Read src/auth.rs`,
`Edited src/auth.rs`, `Ran cargo test`, `Searched foo`), with arguments and
result in the muted monospace family when opened.
Assistant prose is rendered as Markdown (`markdown-rs` via `gui/markdown.rs`):
headings, bold/italic/inline code, links, fenced code blocks, lists, block
quotes, tables, and rules.

The header is one borderless row: the session's first prompt on the left
(truncated with an ellipsis), `host · cwd` on the right. Only the tail window
(the last 50 items) renders on first paint, so a huge session opens fast;
the rest fills in behind the scenes a chunk per frame — pinned to the bottom
while you follow the tail, or holding position when you park at the top, so
scrolling up lazy-loads history with no button in the way. Mid-viewport stays
frozen so reading never shifts. Expansion keys are absolute transcript
indices, so growing the window never misaligns them.
The composer is one
rounded panel holding the input and, below it, the meta row: a model picker
showing the catalog display name (`Muse Spark 1.3 Contributor`, falling back
to the `provider/id` slug), a thinking-level picker showing just the level
(`High`, via the shared `effort_label`), the token breakdown (`in • out •
cache-hit % (cost)`), and, on the right side only, the circular arrow send
button (Home's arrow style). The file records no context window, so none is
shown.
The model picker offers every model in pi's catalog for the endpoint (loaded
once in the background from the agent's models-store, locally or over SSH),
plus any session model the catalog does not know; choosing one sends `/model`
to the live agent. Picker chips and dropdowns are the shared composer
components Home uses (`picker_face`, `picker_menu`, `picker_row`): the same
chip face, leading glyphs, and floated option list, opening upward in the
session composer. Picker lists dismiss on an outside press or Escape. A successful send clears the draft silently; only
failures report a status. While pi is still working — the transcript ends
mid-work with no reply yet — a spinner `Working` line sits above the
composer. Sending is optimistic: the prompt appears as a bubble and the
indicator shows at once, and both persist until pi's first responding content
(an assistant message or anything it produced) lands — not just the echoed
user message. A prompt sent while pi is already working reads `Steering…`
instead, until that same first response. `Steering…` is optimistic too: it is
shown from the moment you send, so nothing waits on the file. A send while one
is in flight queues instead of vanishing, and the round button becomes a stop
square that interrupts the run through `agent.keys` (an injected Escape, what
pi's own `app.interrupt` does). The send status line is gone, so sending never
shifts the layout; only failures report.

Floating above the composer's right side are two jump buttons: the prompts
button opens a menu of every user prompt (one truncated row each) that
scrolls to the chosen message, and shows whenever a prompt exists. The
arrow-down button jumps to the tail and appears only while the tail is
unpinned. The tail pin is sticky
state, not a raw offset comparison (scroll offsets run negative downward): it
sets at the bottom and clears the moment the viewport moves without growth,
so background fill can never hijack a programmatic jump.

The composer field soft-wraps: each logical line is shaped into display
pieces that fit the field's width, breaking at spaces and mid-word for
overlong words, and caret/selection/IME map through those pieces
(`gui/text_input.rs`). A draft taller than the panel's half-height cap
scrolls inside the field.

### Sending a prompt

A sticky composer sits below the transcript as a flex sibling, so it can never
cover the last message (a draft is capped at half the panel and scrolls inside
its field). Enter (the field declines it; `submit_on_enter`) sends its text
through the endpoint's control surface as `agent.prompt`, targeting the pane
the panel found for the active workspace; a prompt is sent on a background
thread and the field clears only when the daemon accepts it. The panel is still
a view of the file pi writes — the prompt's effect appears when pi appends the
next turn.

### One path, local and remote

The panel is opened by the command palette's **Open Pi Session** or the
sidebar's **Pi Session** row for the active workspace, and it works across
every connected endpoint:

- **Locate.** The endpoint's own daemon is asked `agent.sessions` (UHP), which
  scans each supported agent's store and returns the exact `cwd` and session id
  a live agent is in. This is the same call local and remote — a machine
  reaches it through `luvus uhp proxy` over SSH — so the panel does not guess
  by newest-file.
- **Read.** UHP exposes agent session *ids*, not session file contents
  (`agent.read` returns the rendered screen). So the bytes come through
  `pilus-session`'s `Source` trait: a local `FileSource`, or an SSH `tail` for a
  machine, using the same `SshTarget` options the render bridge and folder
  picker use. Every host shell command is quoted for the machine's login shell
  and run through `/bin/sh -c`, so a host whose default shell is fish (which
  rejects `${VAR:-default}`) still lists and reads sessions. The headless
  `--probe-session [CWD|FILE]` folds the same file.

All lookups and reads run on one background thread per open panel, so the GUI
thread only ever clones an already-folded `Transcript`. If Luvus later exposes
a `agent.session.read` UHP method, only `Source` needs to change.

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
and never paints from a non-GUI thread.

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

**Right-click menus.** WORKSPACES and AGENTS rows carry the daemon's native
context menus, drawn by a floating GPUI panel mirroring the daemon's own menu
structure: close, rename, pin, path visibility, and copy path/branch for
workspaces; rename, close, pin, path visibility, and the dock's workspace
scope for agents. Destructive rows hang on the daemon's contract where one
exists. Most items ride the control API — `workspace.close`/`rename`/`pin`,
`pane.close`, `agent.name`, `worktree.create` (targeting the right-clicked
workspace's repo via its index) — and run on a background thread reported
through a menu-status channel, so a remote round trip never blocks the window.
Boards (`Open Git Tab`, `Open Task Board`, `Open Mission Control`) first focus
the workspace, then send the daemon's prefix chord (`Ctrl+Space` + `g`/`o`/`m`)
like the Mission and Orchestration nav rows. The presentational rows are
client state, exactly as they are in the daemon's own TUI: pins float the row
to the top (the workspace pin is additionally pushed through `workspace.pin`,
so the daemon's next projection reorders the catalog), path visibility drives
the second line of every row in the section (workspace paths ride the existing
`ShellDockLayout.workspace_paths` flag; agent paths are per-window state), and
the agents scope filters the dock to the visible surface's active workspace.
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
  short timer instead of holding an `events.subscribe` stream.

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
  public servers unless `--relay` names a self-hosted one.

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

The **Settings** page puts a **REMOTE ACCESS** section above **REMOTE MACHINES**,
with a `style::toggle` switch backed by `Settings.connector`, **off by default**.
Turning it on starts the connector and persists the choice; `gui::connector` owns
one process-wide child so it keeps running after the page closes and starts again
on the next launch. The instruction line stays put and the connection details
(`endpoint id`, `enroll` code, `ticket`, each with a Copy button) appear below it
only while the connector runs. Each machine row in that page gains an **Install
agent** action that stages this build's `pilus-agent` under a temporary sibling
beside the verified remote `luvus` binary and renames it into place over SSH;
a remote platform that does not match this build is refused before any bytes
move. The action intentionally does not launch an unmanaged background process:
the UI reports the exact `serve --iroh --control` command to run on that
machine, so the endpoint, ticket, and enrollment code the agent prints remain
visible to the user. The separate Remote Machines window
keeps add/enable/disable/remove.

The connector must be found before any of this works: `scripts/bundle-macos.sh`
builds and copies `pilus-agent` into `Contents/MacOS/` beside `pilus`, and
`just build`/`just run` build it too, so the app can start it as a sibling.

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
- Daemon upgrades: **Settings → UPDATES → Luvus daemon** checks
  `luvus.dev/latest.json` and installs the verified platform archive itself
  (refusing a package-manager install), then restarts through
  `luvus server restart`. The Remote Machines window's **Update** button
  re-provisions a host with `luvus machine enable <id> --install`, and its rows
  show the remote daemon version from the last probe.
