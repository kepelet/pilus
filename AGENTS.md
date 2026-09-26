# Working in this repository

`l3s` is a native desktop client for a Luvus daemon. The daemon renders its
entire TUI surface to clients as a grid of styled cells; `l3s` attaches, folds
that grid into a `Surface`, and paints it in a GPUI window.

## The one rule that bites

`crates/l3s-protocol` mirrors a binary wire format encoded with
`bincode`'s positional serde encoding. In that encoding, an enum's variant
index and a struct's field order **are** the wire format. Adding a field in the
middle of a struct, reordering enum variants, or deleting a variant will
silently corrupt every message after the change.

If you must change a message:

1. append new variants and fields at the end;
2. bump `PROTOCOL_VERSION`;
3. extend `crates/l3s-client/tests/handshake.rs` so the change is exercised
   against a scripted peer.

The enabling feature in `Cargo.toml` (`runtime_shaders`) is also deliberate: it
keeps the macOS build from shelling out to the `metal` compiler, so a machine
with only the command-line tools can build the app.

## Invariants

- `FrameData::validate` and `FrameData::apply_diff` are the only places that
  mutate a grid. A malformed diff returns an error and leaves the frame
  untouched, so a client can request a full frame instead of painting a torn
  surface.
- `l3s-client` owns no rendering. It emits `ClientEvent`s and folds
  `ServerMessage`s into `Surface`; the window decides how to draw.
- The GUI thread never blocks on socket IO. Reads happen on a background reader
  thread; writes go through an unbounded channel to a writer thread.

## Commands

```sh
cargo test                    # unit + handshake integration tests
cargo run -p l3s              # the window (needs a running daemon)
cargo run -p l3s -- --probe   # headless: print the first frame
```

## Style

- Rust 2024, `unsafe_code` and `unwrap_used` are denied at the workspace level.
  The single `unsafe` call (`discovery::current_uid`) carries a `SAFETY` comment
  and a scoped `allow`.
- Comments state *why*, not *what*. Prefer a name that removes the need for the
  comment.
- Every module has a `//!` header explaining its role in the data flow.
