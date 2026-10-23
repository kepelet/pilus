# pilus

`pilus` is a native macOS client (Rust, GPUI) for a Luvus daemon. It attaches
to the daemon, folds the daemon's streamed grid of styled cells into a
`Surface`, paints it, and draws native chrome around it.

## Build and check

The Rust toolchain is pinned in `.mise.toml`. Tasks live in the `justfile`:

```sh
just check                      # cargo check --workspace
just test                       # unit + handshake integration tests
just clippy                     # warnings are errors
cargo run -p pilus              # the window (needs a running daemon)
cargo run -p pilus -- --probe   # headless: print the first frame
```

Keep the `runtime_shaders` feature in `Cargo.toml`. It stops the macOS build
from shelling out to the `metal` compiler, so a machine with only the
command-line tools can still build the app.

## Rules for every change

- `unsafe_code` and `unwrap_used` are denied workspace-wide. Each `unsafe`
  block carries a `SAFETY` comment and a scoped `#[allow(unsafe_code)]`.
- Every module opens with a `//!` header that explains its role in the data flow.
  Comments say *why*, not *what*.
- Report trouble with `tracing`, never `eprintln!`. A bundle launched from
  Finder has no stderr (see [security](docs/agents/security.md#logging)).
- The GUI thread never blocks on IO. Sockets, SSH, `gh`, the sipil node and
  disk scans run on worker threads.
- In `crates/pilus-protocol`, field order and variant order are the wire
  format. Read [protocol](docs/agents/protocol.md) before touching it.

## Read before you change…

| Area | Doc |
| --- | --- |
| `pilus-protocol`, frames, the client/window split | [protocol.md](docs/agents/protocol.md) |
| `pilus.db`, `index.db`, `settings.json`, anything under `~/.pilus` | [persistence.md](docs/agents/persistence.md) |
| URLs, SVGs, remote shell commands, private files, logs | [security.md](docs/agents/security.md) |
| Timers, polling, animation, energy mode, the Process page | [idle-cost.md](docs/agents/idle-cost.md) |
| Any drawn element; screenshots before shipping UI | [ui.md](docs/agents/ui.md) |
| Sipil: iroh wire, identity, allow-lists | [sipil/network.md](docs/agents/sipil/network.md) |
| Sipil: contacts, channels, `@agent` mentions | [sipil/people-and-channels.md](docs/agents/sipil/people-and-channels.md) |
| Sipil: daemons, work board, offline queue, backoff | [sipil/daemons.md](docs/agents/sipil/daemons.md) |
| Sipil: sidebar rows, Sipil tab, chat page | [sipil/chat-page.md](docs/agents/sipil/chat-page.md) |
| Home page and its idle-agent panel | [features/home.md](docs/agents/features/home.md) |
| Search, Insights, the session page | [features/search-and-insights.md](docs/agents/features/search-and-insights.md) |
| Files tab, editor, ⌘P, ⌘F, markdown notes | [features/files-editor.md](docs/agents/features/files-editor.md) |
| Pinned and recent workspaces | [features/workspaces.md](docs/agents/features/workspaces.md) |
| GitHub Repositories section | [features/github.md](docs/agents/features/github.md) |

For background rather than rules, see `docs/architecture.md` and `wiki/OVERVIEW.md`.
