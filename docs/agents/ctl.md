# `pilus ctl`: driving a running window

`pilus ctl` sends one command to the open window, so a script or an agent can do
what a click or key would: open the right sidebar on a tab, forward a port, load
a page in the Browser, look at it and click through it.

```sh
pilus ctl sidebar open changes   # changes|history|files|ports|browser|sipil
pilus ctl sidebar close          # also: toggle
pilus ctl tab focus 2
pilus ctl state                  # what the focused window shows, as JSON
pilus ctl ping
```

## A dev server on another machine

The case this was built for: the server runs on the Mac mini, pilus on this
Mac, and an agent has to see the page. The agent runs where `pilus ctl` can
reach the window: on this Mac, or on the mini through the tunnel below.

```sh
pilus ctl ports list                       # what the active machine listens on
pilus ctl ports forward 8787               # → {"url": "http://localhost:8787", …}
pilus ctl browser open localhost:8787      # a tab of your own; answers once it has loaded
pilus ctl browser snapshot                 # the page as text, controls as @1, @2…
pilus ctl browser fill @1 "buy milk"
pilus ctl browser click @2
pilus ctl browser scroll down              # also: up|left|right|top|bottom [PX] [--in SEL]
pilus ctl browser scroll @7                # bring one element into view
pilus ctl browser drag @3 @9               # press on @3, move to @9, let go
pilus ctl browser drag .knob --by 120,0    # or move by pixels from its centre
pilus ctl browser drag --at 200,150 --by 80,0   # from a point in the viewport
pilus ctl browser wait 'li:nth-child(2)'
pilus ctl browser network --filter /api    # method, url, status, ms
pilus ctl browser console                  # console.* and uncaught errors
pilus ctl browser eval 'document.title'
pilus ctl browser tabs                     # also: new [URL], close [N], focus N
```

```sh
pilus ctl browser release                  # when done: unlocks your tabs
```

`pilus ctl` with no words prints every verb. Page verbs take `--tab N`, else
they act on the selected tab, which must be one the agent opened (see below).
`browser open URL --tab N` loads into that tab of yours; without `--tab` it
loads into the selected tab if the agent opened it, else a new one. Strings
print as written; everything else as JSON.

`ports forward` forwards the port to the same number on this Mac when it is
free, else another, and says which in `local_port`: open the `url` it returns.
It forwards only a port the machine is listening on, as the Forward button does.
On this Mac there is nothing to forward and it says so.

## Whose tab is it

A tab belongs to whoever opened it. An agent needs no yes for a tab of its own.

- An agent opens tabs of its own (`browser new`, `browser open`) and uses only
  those: every verb that opens, reads, drives, closes or selects a page checks
  the tab (`Access::permits`, in `gui/browser_control.rs`). A tab the person
  opened is refused with a message that says how to ask. `tabs`, `state`,
  `ping`, the sidebar and `ports …` stay free, and `tabs` says who opened each
  (`opened_by`: `person` or `agent`), which the agent `held`, which were `lent`
  and which are `asking`.
- **Asking for the person's tab.** `browser request N [WHY]` puts a prompt over
  the Browser's tab strip (and opens the sidebar on the Browser) naming the tab
  and the reason, with Allow and Deny, and waits. Allow *lends* the tab: the
  agent holds it at once and may use it, but it stays the person's, and goes
  back to them on `release`, Take over or when the tab is closed. Deny turns the
  agent away from that tab for 60 s. Asking for a tab it may already use answers
  at once, and so does asking again within the 60 s or 15 s refusals (deadlines
  compared when the request arrives, no timer). If the person does not decide
  within 30 s the agent's call times out but the prompt stays; a late Allow
  means the agent's next `request` returns at once. The reason is the agent's
  word: one line of plain text, never trusted. A tab with nothing in it is not
  lent. The prompt is drawn by pilus, not the page.
- In the strip, a tab an agent opened has a **dashed border**, and so has its
  page's frame. A tab an agent may use, lent ones included, wears a chip, lit
  while the agent works in it or waits on a yes. The Browser's globe in the
  sidebar goes accent while any is held, so it shows with the Browser tab
  closed. `state` reports `agent_tabs` and `held`.
- **Held means locked.** A verb that uses a tab takes the hold (`acquire`).
  While held, a native cover (`gui/browser_shield.rs`) sits over the page and
  swallows the person's clicks, scrolls and keys, so they cannot interrupt a
  form half filled, and the address bar and back/forward/reload wait. The cover
  spans the whole page, edge to edge, and paints a faint accent wash with an
  accent frame (`WASH`, `FRAME` in `browser_shield.rs`) so a held page reads as
  held; it stays see-through, so the person watches the agent work. Its **Take
  over** button, in the page's top-right corner, ends the hold, fails whatever the agent had in
  flight with "the person took over this tab", and turns the agent away from
  that tab for 15 s. The person can still close the tab.
- The agent lets go with `browser release [N]` (one tab, else all of its tabs,
  and a lent one returns to the person).
  Nothing expires on a timer, so an agent that forgets leaves its tab locked
  until the person presses Take over or closes it.
- A tab opened by a link in a page (`window.open`) belongs to the agent if the
  agent held that page, else to the person.
- `browser control [WHY]` is kept so older agents keep running. It answers
  `granted` at once and changes nothing.
- Where there is no AppKit the cover is not drawn. A real click or keypress in
  the page, which its script reports (`isTrusted`; the agent's synthetic events
  never are), then takes the tab back instead.
- The rules: [the control socket](security.md#the-control-socket). Ownership
  state has no GUI types, in `gui/browser_control.rs`.

## Agents on a host

The window listens on a unix socket, which does not cross machines. An agent
running on a host (a pane on the Mac mini) would find no `pilus.sock` there. So,
per SSH machine and only when the person asks, pilus publishes the socket on the
host with a reverse tunnel:

1. Tick **Let agents on `<machine>` drive this window** in the Ports tab while
   that machine is active. It is off for every machine until then, and saved in
   `Settings::ctl_tunnels`.
2. pilus keeps `ssh -N -R ~/.pilus/pilus-remote.sock:<here>/pilus.sock` open to
   that machine and brings it back after a dropped link (2 s, 5 s, 15 s, 30 s,
   then every minute). The Ports tab shows `On`, `Connecting…` or why it is
   down.
3. On the host, `pilus ctl …` finds the socket by itself: its own window's
   `pilus.sock` if one answers, else `pilus-remote.sock`.
   `PILUS_CTL_SOCKET=/path` overrides both.

The host needs the `pilus` binary, only for its `ctl` client. pilus installs
nothing on a host. Between Macs of one architecture, copy the app's binary:

```sh
scp /Applications/pilus.app/Contents/MacOS/pilus mini:~/.local/bin/pilus
```

`cargo build -p pilus --no-default-features` builds the client alone, for a
host that is not a Mac.

The socket has its own name so that a host running a pilus window of its own is
never touched. If two Macs tunnel to one host, the last one to connect owns the
socket. Ending the tunnel leaves the socket file behind; nothing answers on it,
and the next tunnel removes it. See
[the tunnel's trust](security.md#the-tunnel-to-a-host).

## How it flows

- `ctl.rs` owns the wire format, the listener and the client, with no GUI types.
  `gui/ctl.rs` applies a request to the view.
- The window listens on `~/.pilus/pilus.sock` (`$PILUS_HOME/pilus.sock`). A
  client sends one line of JSON, `{"cmd":"sidebar_open","tab":"history"}`, and
  reads one line back, `{"ok":true,"data":{…}}` or `{"ok":false,"error":"…"}`.
  Page verbs are `{"cmd":"browser_page","tab":2,"op":"click","selector":"#go"}`.
- The listener thread accepts and starts a thread for each client (at most 16).
  That thread parses the line and sends a `Call` down a `futures` channel. A
  task on the app's executor receives it, finds the active main window (any
  main window if Settings is in front) and runs it on the GUI thread.
- A verb answers at once, or it keeps the call's `Replier` and returns `None`.
  A `Replier` clones and may be answered from any thread; the first answer wins.
  The client thread waits up to 30 s. If every clone is dropped unanswered, the
  client is told at once instead of timing out.
- Tunnels (`gui/ctl_tunnel.rs`, `pilus_client::CtlTunnel`) belong to the app,
  not a window, and `ctl_tunnel::ensure` reconciles them with the allowed list
  whenever the machine catalog or the list changes.
- The menu bar helper (`--menubar`) does not listen. A second full pilus finds
  the socket live and leaves it to the first; a socket a crash left behind is
  replaced.

### Who answers a late verb

| Verb | Answered by |
| --- | --- |
| `browser open`, `new`, `reload`, `focus` of an unbuilt tab | the webview's page-load handler, when the load finishes |
| `browser snapshot`, `click`, `eval`, `wait`, … | the page, over `window.ipc`, tagged with the call's id |
| `ports list`, `ports forward` | the worker thread that ran `ssh` |
| `browser request` | the person, from the prompt |
| `browser network`, `console`, `tabs`, `close`, `release` | the GUI thread, at once |

A page that never loads or never answers is the server's 30 s timeout. A new
load (`PageLoadEvent::Started`) fails every call still waiting on the old
document, and closing a tab fails everything waiting on it.

## Looking at a page

Each page tab owns an `Inspect` (`gui/browser_inspect.rs`), shared with its
webview's handlers. `browser_inspect.js` is injected into every page at document
start **of every tab an agent may use**: one it opened, with any http or https
address, or one the person lent it (injected at the moment they allow it, so it
sees only what happens from then on). It:

- streams console output, uncaught errors and requests (fetch, XHR, and the
  document and subresources from `PerformanceObserver`) over `window.ipc` into
  two rings of 500, kept in the window. A read never touches the page and the
  log survives a navigation. Static resources and the document may report no
  status: the platform does not always give one.
- defines `window.__pilus`, which the window calls through `evaluate_script`
  with an id. The page answers over IPC.

A tab the person opened and keeps gets no script: no hooks, no `__pilus`, so a
site they read costs nothing and cannot be driven. `inspectable` in `tabs` says
whether a tab has it.

A tab an agent opened is also **incognito** (`with_incognito`): its cookies and
storage are its own and are dropped with it, so an agent that opens a site sees
it signed out, and cannot reach the person's sign-ins by loading one. To work on
a page as the person, ask for their tab with `browser request`.

### Plain-http hosts (`lab`)

The address bar and `browser open` send a bare dotted name to `https`, and only
`localhost`, IPs and `*.localhost` to `http`. A full `http://trakteer.lab` is
always honoured, but the web view must also be allowed plain http: in a bundle,
App Transport Security refuses it for non-local names, and a signed Info.plist
cannot change at run time. So `PILUS_HTTP_HOSTS="lab,intranet.corp" just bundle`
(commas or spaces) writes an `NSExceptionDomains` entry, with subdomains, for
each name. Empty by default, so a shipped build keeps ATS whole. There is no
settings list: type the `http://`. Unverified on a live window.

### Scrolling and the agent's cursor

`browser scroll` moves the page, or with `--in` one scrollable element, by a
step (`PX`, else 80% of the viewport) or to its top or bottom; with only a
selector it brings that element into view. The answer carries `x`, `y`,
`max_y`, `max_x`, `viewport`, `at_top` and `at_bottom`, so an agent knows when
another step would move nothing. Scrolling is `behavior: 'instant'` whatever
the page's CSS says.

`browser drag FROM TO` (or `FROM --by DX,DY`) presses on the centre of `FROM`,
moves in 8 steps to the centre of `TO` (or the offset point) and lets go.
`--at X,Y` replaces `FROM` with a point in the viewport (CSS pixels, from its
top-left), for a canvas, a map or a slider with no handle to name: whatever
`elementFromPoint` finds there takes the press, and the answer names it. A
plain element gets `pointerdown`/`mousedown`, the moves, then
`pointerup`/`mouseup`, which is what sliders and most sortable lists listen
for. An element with `draggable="true"` gets the HTML5 sequence instead
(`dragstart`, `drag`, `dragenter`/`dragover`, `drop`, `dragend`), because a
real browser swaps the mouse events for those once a drag begins; `drop` is sent
only when the page cancels `dragover`, as a real one is, and the answer says
`dropped`. The drop point must be on screen (`scroll` first). Synthetic events
carry no OS drag, so a page that needs a real file or system drag cannot be
driven this way.

`click`, `fill`, `scroll` and `drag` drive a **virtual cursor** in the page
(`cursor` in `browser_inspect.js`): a pointer that glides to the target, dips
and sends a ring out on a click. It is only for the person watching, so:

- it sits in a closed shadow root, `pointer-events: none`, built from DOM calls
  and the Web Animations API alone, so a page's CSS and CSP can neither style
  nor block it, and it never takes part in the page's own events;
- a click waits for the glide (at most about 0.7 s) before the events fire, so
  the cursor is where the click lands;
- it is up whenever the tab is held and on screen, not only while an action
  runs. `browser_sync` tells the page (`__pilus.cursor(on)`) when that changes:
  it appears in the middle of the viewport, stays where the last action left
  it, and fades out on release, Take over, or when the Browser is hidden. A new
  document starts without it, so the script reports `ready` and the window asks
  again (`Inspect::take_fresh`). A tab that is not on screen but is driven
  (the Browser closed) falls back to leaving 3 s after the last action. Nothing
  here is a timer on an idle page. `prefers-reduced-motion` skips the glide and
  the ring;
- an action does not depend on it: any failure drawing it is swallowed.

Two things an agent trips on, both seen on a live window:

- `open` returns when the document has loaded, not when the page has drawn what
  it fetches, so a `snapshot` straight after can miss it. Follow with `wait`.
- The title in `open`'s answer can be empty: WebKit reports it after the load
  finishes. `tabs` has it a moment later.

Notes for changing it:

- `eval` splices the code into a function the window builds
  (`__pilus.eval(id, async () => (code))`), so a page's CSP, which stops the
  page's own `eval`, does not apply. A syntax error fails the whole
  `evaluate_script`; the completion callback then gets an empty string, which is
  how the window knows the script never ran. Every script therefore ends in a
  string.
- wry queues scripts until the first load has committed and drops the callback
  of a queued one. A script sent that early still runs; only the early failure
  report is lost. `open` waits for the load, so a script sent after it is not
  queued.
- Page text is data. Fields are read one by one, capped, and kept; nothing a
  page sends changes what the window does beyond answering the call that waits.
- Only the selected page tab builds its webview. A tab opened by a link stays
  `pending` in `browser tabs` until `browser focus N`.
- `snapshot` numbers controls as `@N` and keeps the elements, not a selector, so
  a page that re-renders leaves a stale handle that says so.

## Notes from the person

The other direction: the person points at something on a page and tells the
agent. Two buttons follow the address bar. The pointer square turns the picker
on; the three dots (with a note count) open a native menu whose first row, Notes,
toggles the tray. Code: `gui/browser_notes.rs` and `browser_notes.js`, with the
buttons and tray in `gui/browser_tab.rs`.

The same menu holds the tab's other settings, none of which an agent can read or
change. **Viewport** (Responsive, 16:10, 4:3, Mobile, Tablet) puts the page in a
smaller card, centred and fitted to the room the column leaves
(`gui/browser_viewport.rs`). Mobile and Tablet stop at their real size, 390×844
and 768×1024. It resizes the page only: no user agent, touch or pixel ratio.
It is per tab and ends when the tab closes. **Open in Default Browser** passes
the address through `openable_url` first, so a page on any scheme but
http/https is refused. **Copy URL** copies it. The menu is an `NSMenu` because a
GPUI dropdown would paint under the webview.

1. The picker is injected on demand, not at document start, so a page the
   person only reads runs none of it. It outlines what is under the pointer,
   swallows the click so the page does not act on it, and posts the element's
   selector, name, text and start of its markup. ↑ picks the container, ↓ goes
   back, Esc stops. It works on any tab, one an agent may not use included.
2. The note is typed in a field pilus draws under the address bar, not in the
   page. Enter or Add saves it and the element gets a numbered dashed badge,
   redrawn on scroll and resize, and again after each load.
3. **Copy notes** puts all of the tab's notes on the clipboard, with no
   trailing newline, and keeps them (Clear drops them). The person pastes them
   into whichever agent they mean. They are Markdown-ish text: the page, then
   per note its number, the element, the person's words and a `selector:` the
   agent can pass to `browser click`. The message says which words are the
   page's. Pasting straight into the focused pane was tried first; it was
   dropped because the focused pane is not always the agent the person means.
   Choosing an agent from the AGENTS list would bring it back.

There is deliberately no verb to read notes; see
[security](security.md#the-control-socket). Notes live with the tab and go with
it when it closes.

## Adding a verb

1. Add a variant to `ctl::Request` (a page verb is a `ctl::PageOp`) and a case
   to `ctl::parse_args`. Variant order is not a wire format here (the verb is
   named), but a renamed `cmd` is a break for every script, so add rather than
   rename.
2. Handle it in `PilusView::apply_ctl` by calling the method the button or key
   calls. Do not reach into panel state from `gui/ctl.rs`. If it finishes
   later, return `None` and hand the `Replier` to whoever knows.
3. A page verb also needs an op in `browser_inspect.js` and a case in
   `browser_inspect::call_script`.
   A verb that touches a tab gets it from `ctl_agent_hold` (or `ctl_agent_index`
   when it must not lock the person out, as `close` does), never from
   `ctl_page_index` alone: that is what keeps an agent out of the person's tabs.
4. Add it to `ctl::USAGE` and the lists above.

## Rules

- Idle cost: the listener blocks in `accept` and the task is parked on a
  channel, so there is no timer and no poll. Keep it that way. A verb must not
  start one. A `wait` is a timer in the page, for as long as that call lasts
  (at most 25 s). See [idle-cost.md](idle-cost.md).
- Security: see [the control socket](security.md#the-control-socket). A verb
  gets only what a person at the keyboard could do from that surface, a verb
  that takes a URL must not take more schemes than the surface does, and page
  verbs drive only tabs an agent opened, or was lent, and an agent's tab never
  carries the person's sign-ins.
- A tunnel supervisor blocks while its `ssh` runs and sleeps on a condition
  variable between attempts, so an allowed machine costs one parked thread and
  an unallowed one costs nothing. Do not add a poll to watch it.
- The focused window only. A request does not pick a window.
