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

- Page verbs reach **loopback pages only**: http or https on `localhost`,
  `*.localhost`, `127.0.0.1` or `::1`. The script that serves them is injected
  into every page, but on any other page it does nothing and defines nothing, so
  there is nothing to call. `browser_inspect::is_loopback` and the script test
  the same thing; the script's test is the one that counts, since a page can
  navigate between the check and the call. A LAN address is not loopback:
  forward its port (`ports forward`) and open `localhost`.
- `back`, `forward` and `reload` are the page's own buttons and work on any
  page, as do `open`, `new`, `close` and `focus`. `open` still takes http and
  https only.
- **The person says yes first.** An agent asks with `browser control`; the
  banner shows its reason and waits for Allow. Until then every verb that opens,
  reads or drives a page is refused (`Request::needs_browser_control`; add a new
  Browser verb to it). A real click or keypress in the page, or the Take over
  button, ends the grant. The banner is drawn by pilus, not by the page, and
  never takes its text from the page; the agent's reason is the agent's word and
  is shown as plain text. Do not add a way to grant control without the person
  pressing Allow, such as a flag, a setting or an environment variable an agent
  can set.
- Page text is untrusted data. An agent that reads it is reading whatever the
  page says, prompts included. The window parses what a page posts field by
  field, caps it and stores it; it never acts on it beyond answering the call
  that waits.
- `ports forward` forwards only a port the machine is listening on, at the
  address it listens on, as the Forward button does. A script cannot name a
  host or a port the Ports tab would not list.
- Do not add a page verb that returns cookies or storage by name, a verb that
  reaches a non-loopback page, or a setting that turns the loopback rule off
  without a decision from the owner. `eval` on a local page can read that
  page's own cookies, as the page can.

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
- Everything above still holds across the tunnel, the loopback rule for page
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
