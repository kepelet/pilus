# Untrusted input and local files

pilus runs outside the sandbox. These rules stop a pane, a daemon or a contact
from reaching past what it was given.

## Input from other machines

- A URL the daemon asks to open (`ServerMessage::OpenUrl`, sent by whatever runs
  in a pane, including on a remote host) reaches the system opener only when
  `surface::openable_url` allows it: http, https and mailto. `open` launches
  any registered scheme, and a `file:` URL starts an application.
- An `OSC 8` link the window opens itself on Cmd/Ctrl+click (`surface::link_at`,
  from `FrameData.hyperlinks`) passes the same `openable_url` check. A click it
  does not handle goes to the daemon as before.
- An SVG from another device (a sipil's logo) is parsed with
  `about::untrusted_svg_options`, which resolves only `data:` images.
  Otherwise usvg reads any path an `<image>` names.
- An SVG or image the Files tab opens comes from whatever machine the tree is
  on. It is parsed with the same `untrusted_svg_options` and read under a size
  bound (`pilus_client::read_bytes`) on a worker thread.
- Treat a sipil daemon's allow-list like SSH keys. An allowed device can run
  that daemon's agents' shell tools (see [sipil/network.md](sipil/network.md)).

## Push alerts

An alert carries the agent's name and the session's title, and it passes
through the push relay and Apple on the way to the phone. Do not add more to it
(prompts, file paths, transcript text) without a decision from the owner. The
relay holds the APNs key and nothing else; never log a token, title or body
there or on the host. Details in [push.md](push.md).

## Remote commands

A remote command reaches the host's login shell, which may be fish.

- Quote the command for that shell with `transport::quote_for_login_shell`,
  which puts backslashes and quotes outside the quoted runs so that `sh` and
  fish read them the same way.
- Quote the script's own words with `git::sh_quote`.
- Never build a remote command from unquoted input.

## Private files

Files under `~/.pilus` are owner-only.

- Create one with `pilus_db::create_private`.
- Replace one with `pilus_db::write_private`, which stages, syncs and then
  renames, so a crash never leaves half a file.
- `pilus_db::private_dir` keeps the folder at `0700`.
- A remote save is staged beside the file and copied over it only once every
  byte has arrived (`write::remote_save_script`).

## The control socket

`pilus ctl` (see [ctl.md](ctl.md)) lets any process that can open
`~/.pilus/pilus.sock` drive the window.

- The socket lives in the `0700` folder and is set to `0600`, so only the
  owner's processes reach it. Do not move it to `/tmp`, and do not add a TCP
  or network listener.
- Anything running as the owner can use it, including an agent in a pane. So a
  verb gets no more than the button it stands for. The Browser verb opens
  http and https only, though the address bar accepts more.
- A request line is capped at 16 KB and a client has two seconds to send it.
  At most 16 clients are served at once, so a stalled client cannot hold the
  listener or crowd out the rest.
- Never add a verb that reads files, runs a command, or returns the contents
  of a terminal pane. `state` reports only which tab is showing and the
  Browser's address.

### The Browser's page verbs

`browser snapshot`, `html`, `text`, `eval`, `click`, `fill`, `press`, `wait`,
`network` and `console` return a page's contents and run script in it. That is
more than the address bar offers, and anything running as the owner (an agent
in a pane, say) can ask. The Browser keeps the owner's sign-ins, so a script in
a page there runs as them. So:

- Page verbs reach **any address, in a tab an agent may use**, and in no other.
  The script that serves them is injected only into such a tab, so a tab the
  person keeps has nothing to call. This used to be limited to loopback pages;
  the owner lifted that so an agent can drive a staging site or a LAN address.
  What replaces it is that **a tab an agent opened is incognito**
  (`with_incognito`, set when its webview is built from `Access::opener`): no
  cookies, storage or sign-ins of the person's reach it, and none outlive it.
  So the limit moved from *where* the page is to *whose session* it runs in.
  Do not build an agent's tab on the shared store, and do not give it a
  persistent store, without the same decision from the owner.
- `back`, `forward` and `reload` are the page's own buttons and work on any
  page, as do `open`, `new`, `close` and `focus`. `open` still takes http and
  https only.
- **A tab belongs to whoever opened it.** There is no Allow button. An agent
  opens tabs of its own (`browser new`, `browser open`) and may read, drive,
  close and select only those; every Browser verb goes through
  `Access::permits` / `acquire` in `gui/browser_control.rs`, so a new verb that
  takes a tab must too. A tab the person opened is refused to it, and the
  person's tabs are the only ones whose sign-ins and open pages an agent has no
  way to reach. The one way in is `browser request N`: the prompt names the
  tab and waits for Allow, and a yes lends that one tab until it is released,
  taken back or closed. Do not add a verb, flag, setting or environment
  variable that lets an agent use a tab the person opened without that yes. The
  prompt is drawn by pilus and never takes its text from the page; the agent's
  reason is its own word and is shown as plain text.
- **An agent's tab is marked, and locked while it works.** The strip shows a
  chip on every tab an agent opened, lit while it holds the tab. A held page
  has a clear native cover (`gui/browser_shield.rs`) that swallows the
  person's mouse, wheel and keys, and a Take over button in a corner. A tab an
  agent opened has a dashed border. It is drawn by pilus, never
  injected into the page, so a page cannot remove or imitate it and it covers
  pages that run none of our script. Take over ends the hold and turns the agent
  away from that tab for 15 s.
- What this gives up, on purpose: an agent no longer needs a yes to open a
  page, so it can load any http or https address in a tab the person can see,
  read it and drive it, signed out. It can send requests from the person's
  machine and network to anything they can reach (a LAN admin page, say), and
  what it types into a page it opened is sent there. It cannot reach any tab the
  person opened, or their sign-ins, unless they lend that tab. A lent tab runs
  as them: that is what the yes is for. A page it opens by `window.open` from a
  lent tab is the agent's, so incognito.
- Page text is untrusted data. An agent that reads it is reading whatever the
  page says, prompts included. The window parses what a page posts field by
  field, caps it and stores it; it never acts on it beyond answering the call
  that waits.
- `ports forward` forwards only a port the machine is listening on, at the
  address it listens on, as the Forward button does. A script cannot name a
  host or a port the Ports tab would not list.
- Do not add a page verb that returns cookies or storage by name, a verb that
  reaches a tab an agent may not use, or a setting that gives an agent's tab
  the person's profile without a decision from the owner. `eval` can read the
  cookies of the page it runs in, as the page can; in an agent's tab those are
  the agent's own.
- **The person's notes** (the Browser's annotate buttons, `browser_notes`) are
  the person's act, so they work on any tab, including ones an agent may not
  use. Nothing about them is readable over the control socket: there is no verb
  that returns a note or the element behind it, because the element's text is a
  tab the agent has no right to. *Copy notes* is the person's click, and puts the
  text on the clipboard. Everything the page supplied in a note
  (element name, text, markup, selector) is stripped of control characters
  before it is stored, since it is pasted into a terminal and an escape sequence, or
  the end of a bracketed paste, must not ride along. The note is typed in a
  field pilus draws, never in the page. The window ignores a pick the page
  sends when the picker is not on.

### The tunnel to a host

An agent on an SSH machine can only reach the window if pilus carries the socket
there (`ssh -R`, see [ctl.md](ctl.md#agents-on-a-host)). That widens who can
drive the window from the owner's processes on this Mac to every process of the
owner's account on that host. So:

- It is **off for every machine** and turns on only when the person ticks the
  box for that machine. The tunnel only lets an agent reach the window; the
  Browser still waits for Allow. Never enable it from a `pilus ctl` verb, a setting a
  script can write, or a default: an agent that could switch it on could
  open its own way in.
- Everything above still holds across the tunnel, the per-tab rules for page
  verbs included. A host gets no more than a pane on this Mac gets.
- The socket lives in `~/.pilus` on the host, made `0700` before the socket
  exists, under the name `pilus-remote.sock`, so it never replaces a window of
  the host's own. Only the one name is ever removed there.
- `-R` paths are split on `:` with no quoting, so a path holding one is
  refused rather than parsed into a different forward.
- Quitting pilus ends every tunnel. A crash can leave an `ssh` behind that
  holds an unlinked socket; the next tunnel replaces the file.

## Logging

Warnings and panics go to `~/.pilus/logs/pilus.log` (`logging.rs`). The log is
owner-only and is cut over at 4 MB. Set the level with
`PILUS_LOG=info|debug|trace`. Use `tracing`, not `eprintln!`, or a bundle
launched from Finder loses the output.
