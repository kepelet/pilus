# Luvus protocol and the client layer

Read this before you change `crates/pilus-protocol`, the connection code in
`crates/pilus-client`, or the path frames take into the window.

Sipil is a separate protocol (JSON over iroh) and does not involve
`pilus-protocol`. It is covered in [sipil/network.md](sipil/network.md).

## The wire format is positional

`crates/pilus-protocol` mirrors a binary wire format encoded with `bincode`'s
positional serde encoding. In that encoding, an enum's variant index and a
struct's field order **are** the wire format. Adding a field in the middle of a
struct, reordering enum variants, or deleting a variant silently corrupts every
message after the change.

To change a message:

1. append new variants and fields at the end;
2. bump `PROTOCOL_VERSION`;
3. extend `crates/pilus-client/tests/handshake.rs` so the change is exercised
   against a scripted peer.

## Invariants

- `FrameData::validate` and `FrameData::apply_diff` are the only places that
  mutate a grid. A malformed diff returns an error and leaves the frame
  untouched, so a client can request a full frame instead of painting a torn
  surface.
- `pilus-client` owns no rendering. It emits `ClientEvent`s and folds
  `ServerMessage`s into `Surface`; the window decides how to draw.
- The GUI thread never blocks on socket IO. Reads happen on a background reader
  thread; writes go through an unbounded channel to a writer thread.
- Daemon frames wake the window's tick loop through `Arrival`, so output does
  not wait for the next idle tick (see [idle-cost.md](idle-cost.md)).
- A URL the daemon asks to open is filtered first (see
  [security.md](security.md)).
