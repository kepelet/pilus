---
title: Change the wire protocol
description: Add a message or field without breaking positional encoding.
tags:
  - wiki
  - guide
---
# Change the wire protocol

## Goal

Add or extend a message between `pilus` and a Luvus daemon without silently corrupting every message after the change.

## Steps

1. **Append, never insert.** Add the variant at the end of `ClientMessage` or `ServerMessage` in [message.rs](../../crates/pilus-protocol/src/message.rs), or the field at the end of its struct. Variant index and field order are the wire format (positional bincode).
2. **Bump the revision.** Increment `PROTOCOL_VERSION` in [lib.rs](../../crates/pilus-protocol/src/lib.rs) and update its doc comment with exactly what changed. If an older daemon must stay attachable, leave `FALLBACK_PROTOCOL_VERSION` pointing at the previous layout and make the deserializer tolerate both (the hand-written `FrameData` deserializer and the thread-local `DECODE_VERSION` in [frame.rs](../../crates/pilus-protocol/src/frame.rs) are the pattern).
3. **Handle it.** Decode the message in [connection.rs](../../crates/pilus-client/src/connection.rs) or fold it in [surface.rs](../../crates/pilus-client/src/surface.rs). Side effects go through a `take_*` drain so a window decides when to act.
4. **Extend the handshake test.** Add a case to `crates/pilus-client/tests/handshake.rs` so the change is exercised against a scripted peer. If it is a shell projection, also cover the claim in the relevant probe.
5. **Check the daemon side.** `pilus-protocol` mirrors the released daemon (currently protocol 20, Luvus 0.14.3). The daemon must ship the matching change; an unmatched client meets a `Welcome` error.

## Relevant code

- [message.rs](../../crates/pilus-protocol/src/message.rs), [frame.rs](../../crates/pilus-protocol/src/frame.rs), [lib.rs](../../crates/pilus-protocol/src/lib.rs)
- [connection.rs](../../crates/pilus-client/src/connection.rs), [surface.rs](../../crates/pilus-client/src/surface.rs)
- [pilus-protocol](../modules/protocol.md), [Daemon surface and wire protocol](../architecture/daemon-surface.md)

## Gotchas

- Deleting or reordering a variant is a breaking change even if the new build compiles; it corrupts every later message.
- A field inserted in the middle of `FrameData` is a layout change, not an additive one — that is exactly why `hyperlinks` had to move the revision.
- `MAX_FRAME` is 64 MiB; a message that can exceed it needs a different shape, not a bigger ceiling.
- A malformed diff is rejected whole by `FrameData::apply_diff`; do not “best effort” apply a partial run.
- Keep the model probe (`--probe`) runnable: it is the fastest way to see whether a real daemon still agrees.

## Related

- [Change the wire protocol](../guides/protocol-message.md) is the companion to [Daemon surface and wire protocol](../architecture/daemon-surface.md).
- [Attach and paint](../flows/attach-and-paint.md)
