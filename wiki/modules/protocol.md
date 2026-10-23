---
title: pilus-protocol
description: Wire messages, frames, framing, and protocol revisions.
tags:
  - wiki
  - module
---
# pilus-protocol

## Summary

`pilus-protocol` is the only crate that knows the binary wire format between `pilus` and a Luvus daemon. It defines the message enums, the cell-grid frame model, the theme/sound/window types, and the length-prefixed bincode framing. Nothing here knows about GPUI, sockets, or the OS, so the crate is trivially testable and safe to share with headless probes.

Its header is explicit: encoding is positional, so enum variant order and struct field order are the wire format.

## Responsibilities

- Declare `ClientMessage` and `ServerMessage`, including the machine-aware shell projection (`ShellDockLayout`, `ShellWorkspaces`, `ShellSidebars`) and surface handoff (`SurfaceInterest`, `PreparedFrame`, `PrepareSurface`).
- Declare `FrameData`, `CellData`, `DiffRun`, `FrameHyperlink`, and enforce grid invariants.
- Frame messages with a length prefix and decode them against the negotiated protocol revision.
- Name the supported protocol revisions and the hard frame ceiling.

## Public API / entry points

From [lib.rs](../../crates/pilus-protocol/src/lib.rs):

- `PROTOCOL_VERSION = 20`, `FALLBACK_PROTOCOL_VERSION = 18`, `MAX_FRAME = 64 MiB`.
- `write_message`, `write_message_counted`, `read_message`.
- Re-exports of `FrameData`, `FrameDiff`, `CellData`, `DiffRun`, `FrameHyperlink`, `set_decode_version`.
- Re-exports of `ClientMessage`, `ServerMessage`, `SurfaceInterest`.
- Re-exports of theme/window types: `TerminalColors`, `SoundSignal`, `ShellDockLayout`, `ShellDockRect`, `ShellSidebars`, `ShellWorkspace`, and friends.
- `symbol_width` for display width of a grapheme.

From [frame.rs](../../crates/pilus-protocol/src/frame.rs): `FrameData::blank`, `cell_count`, `cell`, `validate`, `apply_diff`; `CellData::blank` and `is_empty`.

## Key files

- [lib.rs](../../crates/pilus-protocol/src/lib.rs) — version constants, framing, re-exports.
- [message.rs](../../crates/pilus-protocol/src/message.rs) — the two top-level enums.
- [frame.rs](../../crates/pilus-protocol/src/frame.rs) — grid model, thread-local decode revision, invariant checks.
- [theme.rs](../../crates/pilus-protocol/src/theme.rs) — colors, sounds, shell geometry.

## Dependencies

`bincode` (serde, positional), `serde`, `crossterm` (key/mouse event types), `unicode-width`. No runtime, no GPUI.

## Invariants

- Append new enum variants and struct fields at the end; never reorder or delete.
- A structure change bumps `PROTOCOL_VERSION` and extends `crates/pilus-client/tests/handshake.rs`.
- `FrameData::validate` and `FrameData::apply_diff` are the only places that mutate a grid. A malformed diff returns an error and leaves the frame untouched.
- `MAX_FRAME` rejects an oversized announcement before any allocation proportional to it.
- The hand-written `FrameData` deserializer consults a thread-local `DECODE_VERSION`, set once per reader thread after the handshake, so one process can bridge two revisions at once.

## Participates in

- [Attach and paint](../flows/attach-and-paint.md) — handshake and frame application.
- [Daemon surface and wire protocol](../architecture/daemon-surface.md).
- [pilus-client](./client.md), which wraps it in a connection.

## Related

- [Surface](../concepts/surface.md)
- [Change the wire protocol](../guides/protocol-message.md)
