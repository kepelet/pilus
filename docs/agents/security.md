# Untrusted input and local files

pilus runs outside the sandbox. These rules stop a pane, a daemon or a contact
from reaching past what it was given.

## Input from other machines

- A URL the daemon asks to open (`ServerMessage::OpenUrl`, sent by whatever runs
  in a pane, including on a remote host) reaches the system opener only when
  `surface::openable_url` allows it: http, https and mailto. `open` launches
  any registered scheme, and a `file:` URL starts an application.
- An SVG from another device (a sipil's logo) is parsed with
  `about::untrusted_svg_options`, which resolves only `data:` images.
  Otherwise usvg reads any path an `<image>` names.
- Treat a sipil daemon's allow-list like SSH keys. An allowed device can run
  that daemon's agents' shell tools (see [sipil/network.md](sipil/network.md)).

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

## Logging

Warnings and panics go to `~/.pilus/logs/pilus.log` (`logging.rs`). The log is
owner-only and is cut over at 4 MB. Set the level with
`PILUS_LOG=info|debug|trace`. Use `tracing`, not `eprintln!`, or a bundle
launched from Finder loses the output.
