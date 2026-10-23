---
title: UHP
description: The daemon's structured JSON control surface, separate from the frame stream.
tags:
  - wiki
  - concept
---
# UHP

## Definition

UHP (Universal Harness Protocol) is Luvus's public automation API on the owner-only control socket (`luvus.sock`). It is newline-delimited JSON: `{id, method, params}` in, `{id, result}` or `{id, error}` out. It is separate from the binary render protocol that carries the cell grid.

## Why it matters

The render protocol deliberately carries only pixels plus a small shell projection. Everything structured — the live agent list, tabs, status-bar widgets, diffs, file trees, git state, tasks, Mission Control, automations — comes through UHP. That keeps the binary format small and lets a feature land without a `PROTOCOL_VERSION` bump. The same surface is what `pilus-agent` forwards to phones, with per-device allow-lists layered on top.

## Where it lives

- [uhp.rs](../../crates/pilus-client/src/uhp.rs) — `UhpClient`, `Observer`, `EventStream`, `Capabilities`; one request per connection for ordinary calls, plus the event subscription and the observe stream.
- [control.rs](../../crates/pilus-client/src/control.rs) — `ControlClient` (typed methods) and `AgentFeed` (event-driven snapshots, deduplicated, with a slow reconciliation).
- Typed read models beside it: `diff.rs`, `files.rs`, `git_status.rs`, `mission.rs`, `tasks.rs`, `agents.rs`.
- The connector's hub ([hub.rs](../../crates/pilus-agent/src/hub.rs)) rewrites `uhp.capabilities` per device so a phone sees its own level's methods.

## Related

- [Daemon surface and wire protocol](../architecture/daemon-surface.md)
- [Enrollment level](./enrollment-level.md)
- [pilus-client](../modules/client.md)
