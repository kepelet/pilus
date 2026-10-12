# pilus

**A native desktop client for a [Luvus](https://github.com/RizRiyz/luvus) daemon.**

Luvus is mission control for AI coding agents: persistent workspaces, panes,
tabs, and a live sidebar of agent state, all rendered by a daemon that keeps
running when you detach. `pilus` attaches to that daemon and paints its rendered
surface in a real window instead of a terminal — the same cells, splits, and
sidebar, without running another terminal emulator.

```
┌──────────────────────────────────────────────────────────┐
│ ● pilus · default                                  connected│
├──────────────────────────────────────────────────────────┤
│  workspaces        │  agent: claude          working      │
│  ▸ api             │  $ cargo test                        │
│  ▸ web             │  running 15 tests …                  │
│                    │                                      │
└──────────────────────────────────────────────────────────┘
```

## Status

Early. `pilus` today is a focused vertical slice:

- discovers the daemon's client socket (default or named session), starting the daemon itself when it isn't running;
- completes the version-checked handshake and probes terminal colors;
- streams the daemon's cell grid, applying full frames and sparse diffs;
- claims the whole sidebar and hides the daemon's `« session Menu` chrome,
  replacing it with native GPUI panels: workspaces from the rendered-dock
  projection and agents from the daemon's JSON control API. A collapse toggle
  and a Menu button (opens the daemon's Settings) live in the app's top bar;
- forwards keys, mouse clicks and scroll, workspace/agent selection, and
  viewport resizes back to the daemon;
- selects text over the grid natively and copies it with ⌘C;
- right-clicks a WORKSPACES or AGENTS row for the daemon's own context menu
  drawn natively: close/rename/pin/path toggle, copy path & branch, new git
  worktree, delete worktree (confirm-gated on linked checkouts), the
  git/task/mission boards, and the agents scope filter;
- covers the daemon's tab bar with a native macOS-style bar (focus, new, and
  close) driven by the `tab.*` control API;
- covers the daemon's bottom status line with a native bar whose right side
  mirrors the daemon's own `ui.bar.list` widgets (mode, pane count, tab);
- shows local and remote workspaces in one flat list, bridges every
  enabled SSH machine over `ssh -T` in parallel at startup, switches the
  visible surface between endpoints with a two-phase prepared frame, offers a
  native Open Workspace folder picker and an Add Machine window, and lists
  live agents from every connected endpoint in the AGENTS dock;
- remembers the main window's size, position, and display across launches,
  falling back to a centered window when the display is gone;
- paints the grid in a GPUI window with the ANSI palette resolved natively;

Client-native chrome that the daemon does not project (file tree, Git
dashboard, agents dock) is still painted from the daemon's cells — see
[`docs/architecture.md`](docs/architecture.md).

## Build and run

Requires Rust 1.96+, the `luvus` CLI on `PATH`, and — on macOS — either full
Xcode or the Metal toolchain. `pilus` starts the Luvus daemon itself when it
isn't already running. The GUI links a stitched Metal shader and does not
invoke the `metal` compiler at build time, so the command-line tools alone are
usually sufficient.

```sh
# Attach to the default session.
cargo run -p pilus

# Attach to a named session, or an explicit socket.
cargo run -p pilus -- --session work
cargo run -p pilus -- --socket /path/to/luvus-client.sock

# Connect once and print the first frame (no window).
cargo run -p pilus --no-default-features -- --probe

# Same, but claim the workspaces dock and print its projection.
cargo run -p pilus --no-default-features -- --probe-workspaces

# Attach to one saved machine over SSH and print its workspace catalog.
cargo run -p pilus --no-default-features -- --probe-machine mac-mini
```

### macOS app bundle

`cargo run` installs the Dock icon at runtime, but a real app wants a bundle so
Finder, the Dock, and Launchpad show the icon and name:

```sh
just bundle                 # builds target/release/pilus.app
open target/release/pilus.app
```

The bundle's icon comes from `crates/pilus/assets/pilus.icns`, generated from
`pilus-icon-1024.png` with `iconutil`. Regenerate it after changing the artwork:

```sh
sips -z 1024 1024 art.png --out crates/pilus/assets/pilus-icon-1024.png
```

## Updates

`pilus` updates itself from a release feed: `pilus → Check for Updates…` checks
on demand, a footer badge tracks download and restart-into-update, and
**Settings → UPDATES** picks the channel and feed URL. Stable builds read
`https://releases.kepelet.com/pilus/stable.json`; preview/nightly builds read
`https://releases.kepelet.com/pilus/preview.json`. See
[`docs/ota.md`](docs/ota.md) for the release process, signing, and channels.

```sh
just package-dmg    # signed Apple Silicon .dmg + SHA256SUMS
just check-updates  # exercise the feed without opening a window
just preview-feed   # DMG + latest.json the preview channel reads locally
just serve-preview  # serve that payload on 127.0.0.1:8000
```

## Layout

| Crate | Responsibility |
| --- | --- |
| `crates/pilus-protocol` | The wire vocabulary: client/server messages, cell frames, framing. |
| `crates/pilus-client` | Socket discovery, transport, handshake, the event stream, and the folded `Surface`. |
| `crates/pilus` | The GPUI window, key mapping, and native painting. |
| `crates/pilus-agent` | The host connector: enrollment, the `luvus/hub/1` front door phones talk to, and fan-out to SSH machines. |
| `crates/pilus-mobile` | The phone-side client: iroh transport, UHP hub calls, enrollment persistence, C FFI for the iOS app in `ios/`. |

The protocol crate is the only place that knows the wire format, and its
declaration order is load-bearing — see [`AGENTS.md`](AGENTS.md) before editing
it.

## License

Apache-2.0.
