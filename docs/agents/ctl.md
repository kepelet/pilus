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
pilus ctl browser control checking the todo app   # waits for the person's yes
pilus ctl browser open localhost:8787      # answers once the page has loaded
pilus ctl browser snapshot                 # the page as text, controls as @1, @2…
pilus ctl browser fill @1 "buy milk"
pilus ctl browser click @2
pilus ctl browser wait 'li:nth-child(2)'
pilus ctl browser network --filter /api    # method, url, status, ms
pilus ctl browser console                  # console.* and uncaught errors
pilus ctl browser eval 'document.title'
pilus ctl browser tabs                     # also: new [URL], close [N], focus N
```

```sh
pilus ctl browser release                  # when done
```

`pilus ctl` with no words prints every verb. Page verbs take `--tab N`, else
they act on the selected tab. Strings print as written; everything else as JSON.

`ports forward` forwards the port to the same number on this Mac when it is
free, else another, and says which in `local_port`: open the `url` it returns.
It forwards only a port the machine is listening on, as the Forward button does.
On this Mac there is nothing to forward and it says so.

## Asking for the Browser

The Browser is the person's, so an agent asks for it. `browser control [WHY]`
puts a banner over the Browser's tab strip (it opens the sidebar on the Browser
so it is seen) and waits for Allow or Deny. Until the person allows it, every
verb that opens, reads or drives a page is refused with a message that tells
the agent how to ask: `open`, `new`, `close`, `focus`, and all of the page verbs
(`Request::needs_browser_control`). `tabs`, `state`, `ping`, the sidebar and
`ports …` stay free.

- While allowed, the banner reads "An agent is controlling the Browser" with
  **Take over**, and the Browser's globe in the sidebar goes accent so it shows
  with the Browser tab closed. `state` reports `agent_control` as `none`,
  `asking` or `granted`.
- The person takes over by pressing Take over, or by clicking or typing in the
  page: its script reports only `isTrusted` input, which the agent's synthetic
  events never are. Taking over ends the grant, fails whatever the agent had in
  flight with "the person took over the Browser", and turns the agent away for
  15 s. Deny turns it away for 60 s. Both are a deadline compared when the next
  `control` arrives, not a timer.
- The agent hands the Browser back with `browser release`.
- The grant belongs to the window's Browser, not to a tab or a client: any agent
  that reaches the window can use it, so what the banner says is all it says.
  It lasts until Take over, `release`, or quit.
- `control`'s reason is the agent's word. It is cut to one line of plain text
  and shown as text; do not trust it.

State and rules are in `gui/browser_control.rs`, which has no GUI types.

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
| `browser control` | the person, from the banner |
| `browser network`, `console`, `tabs`, `close`, `release` | the GUI thread, at once |

`browser control` is answered by the person pressing Allow or Deny. If they do
not decide within 30 s the agent's call times out but the banner stays, and a
late Allow just means the agent's next `control` returns at once.

A page that never loads or never answers is the server's 30 s timeout. A new
load (`PageLoadEvent::Started`) fails every call still waiting on the old
document, and closing a tab fails everything waiting on it.

## Looking at a page

Each page tab owns an `Inspect` (`gui/browser_inspect.rs`), shared with its
webview's handlers. `browser_inspect.js` is injected into every page at document
start. **On a loopback page** (`localhost`, `*.localhost`, `127.0.0.1`, `::1`,
http or https) it:

- streams console output, uncaught errors and requests (fetch, XHR, and the
  document and subresources from `PerformanceObserver`) over `window.ipc` into
  two rings of 500, kept in the window. A read never touches the page and the
  log survives a navigation. Static resources and the document may report no
  status: the platform does not always give one.
- defines `window.__pilus`, which the window calls through `evaluate_script`
  with an id. The page answers over IPC.

On any other page it does nothing at all: no hooks, no `__pilus`, no IPC. A
site you are signed in to costs nothing and cannot be driven. `is_loopback` and
the script apply the same test; the script's is the one that counts.

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
4. Add it to `ctl::USAGE` and the lists above.

## Rules

- Idle cost: the listener blocks in `accept` and the task is parked on a
  channel, so there is no timer and no poll. Keep it that way. A verb must not
  start one. A `wait` is a timer in the page, for as long as that call lasts
  (at most 25 s). See [idle-cost.md](idle-cost.md).
- Security: see [the control socket](security.md#the-control-socket). A verb
  gets only what a person at the keyboard could do from that surface, a verb
  that takes a URL must not take more schemes than the surface does, and page
  verbs reach loopback pages only.
- A tunnel supervisor blocks while its `ssh` runs and sleeps on a condition
  variable between attempts, so an allowed machine costs one parked thread and
  an unallowed one costs nothing. Do not add a poll to watch it.
- The focused window only. A request does not pick a window.
