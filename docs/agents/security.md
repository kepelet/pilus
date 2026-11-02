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
- A request line is capped at 16 KB and a client has two seconds to send it,
  so a stalled client cannot hold the listener.
- Never add a verb that reads files, runs a command, or returns the contents
  of a pane. `state` reports only which tab is showing and the Browser's
  address.

## Logging

Warnings and panics go to `~/.pilus/logs/pilus.log` (`logging.rs`). The log is
owner-only and is cut over at 4 MB. Set the level with
`PILUS_LOG=info|debug|trace`. Use `tracing`, not `eprintln!`, or a bundle
launched from Finder loses the output.
