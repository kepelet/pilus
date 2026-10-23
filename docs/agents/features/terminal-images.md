# Images in the terminal

Pasting (⌘V) or dropping an image on the terminal pastes its **path** into the
focused pane. A pane cannot take pixels, but an agent or viewer in it reads a
path. The code is `crates/pilus-client/src/paste.rs` (storage) and
`crates/pilus/src/gui/paste_image.rs` (clipboard, drop, toasts).

## Where the image goes

The machine that owns the pane decides, the same split as Files-tab uploads:

| Source | Local pane | Remote pane |
| --- | --- | --- |
| Image file (drop, or copied in Finder) | its own path, no copy | piped over SSH to `~/.pilus/paste/` |
| Clipboard pixels (a screenshot) | written to `~/.pilus/paste/` | piped over SSH to `~/.pilus/paste/` |

- The remote script reports the host's absolute path back, because only the
  host knows its `$HOME`.
- The folder and files are owner-only (`umask 077` remotely, `private_dir` and
  mode `0600` locally). A screenshot is as private as the screen.
- Stored files are cleared after `KEEP_DAYS` (7) on the next paste. Nothing is
  ever replaced: names carry the time and a sequence number, and the remote
  redirect runs under `set -C`.
- Images over `MAX_PASTED_BYTES` (32 MB) are refused.

## Rules

- A clipboard that carries text is a text paste. Only a Finder file entry whose
  paths are all images, or a clipboard with an image and no text, becomes an
  image paste (`clipboard_images`).
- Storage runs on a worker (a remote host is a round trip). The path is pasted
  only if the window still shows the machine the image was stored for.
- Paths are pasted as one shell word with backslash escapes (`shell_word`),
  which sh, zsh and fish read alike, plus a trailing space.
- Only images are taken from a drop; other files are skipped with a toast.
- The drop cue is an absolute overlay on the grid, so cell geometry never moves.
