# l3s

**A native desktop client for a [Luvus](https://github.com/RizRiyz/luvus) daemon.**

Luvus is mission control for AI coding agents: persistent workspaces, panes,
tabs, and a live sidebar of agent state, all rendered by a daemon that keeps
running when you detach. `l3s` attaches to that daemon and paints its rendered
surface in a real window instead of a terminal — the same cells, splits, and
sidebar, without running another terminal emulator.

```
┌──────────────────────────────────────────────────────────┐
│ ● l3s · default                                  connected│
├──────────────────────────────────────────────────────────┤
│  workspaces        │  agent: claude          working      │
│  ▸ api             │  $ cargo test                        │
│  ▸ web             │  running 15 tests …                  │
│                    │                                      │
└──────────────────────────────────────────────────────────┘
```

## Status

Early. `l3s` today is a focused vertical slice:

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
- covers the daemon's tab bar with a native macOS-style bar (focus, new, and
  close) driven by the `tab.*` control API;
- covers the daemon's bottom status line with a native bar whose right side
  mirrors the daemon's own `ui.bar.list` widgets (mode, pane count, tab);
- paints the grid in a GPUI window with the ANSI palette resolved natively.

Client-native chrome that the daemon does not project (file tree, Git
dashboard, agents dock) is still painted from the daemon's cells — see
[`docs/architecture.md`](docs/architecture.md).

## Build and run

Requires Rust 1.96+, the `luvus` CLI on `PATH`, and — on macOS — either full
Xcode or the Metal toolchain. `l3s` starts the Luvus daemon itself when it
isn't already running. The GUI links a stitched Metal shader and does not
invoke the `metal` compiler at build time, so the command-line tools alone are
usually sufficient.

```sh
# Attach to the default session.
cargo run -p l3s

# Attach to a named session, or an explicit socket.
cargo run -p l3s -- --session work
cargo run -p l3s -- --socket /path/to/luvus-client.sock

# Connect once and print the first frame (no window).
cargo run -p l3s --no-default-features -- --probe

# Same, but claim the workspaces dock and print its projection.
cargo run -p l3s --no-default-features -- --probe-workspaces
```

### macOS app bundle

`cargo run` installs the Dock icon at runtime, but a real app wants a bundle so
Finder, the Dock, and Launchpad show the icon and name:

```sh
just bundle                 # builds target/release/l3s.app
open target/release/l3s.app
```

The bundle's icon comes from `crates/l3s/assets/l3s.icns`, generated from
`l3s-icon-1024.png` with `iconutil`. Regenerate it after changing the artwork:

```sh
sips -z 1024 1024 art.png --out crates/l3s/assets/l3s-icon-1024.png
```

## Layout

| Crate | Responsibility |
| --- | --- |
| `crates/l3s-protocol` | The wire vocabulary: client/server messages, cell frames, framing. |
| `crates/l3s-client` | Socket discovery, transport, handshake, the event stream, and the folded `Surface`. |
| `crates/l3s` | The GPUI window, key mapping, and native painting. |

The protocol crate is the only place that knows the wire format, and its
declaration order is load-bearing — see [`AGENTS.md`](AGENTS.md) before editing
it.

## License

Apache-2.0.
