---
title: Endpoint
description: Local plus every saved SSH machine as one window's targets.
tags:
  - wiki
  - concept
---
# Endpoint

## Definition

An endpoint is one reachable Luvus session from this window: the owner-local daemon (`Local`) or one enabled saved SSH machine. It has its own connection, `Surface`, tabs, agent list, status bar, and workspace catalog. The window shows exactly one endpoint at a time but keeps the others connected.

## Why it matters

Multi-machine support is not a separate window or a separate app: it is a set of endpoints inside one window, each with the same protocol and the same surface type. That is why a remote host needs no special rendering path — it hands back `Welcome`/`Ready`/`EndpointIdentity` exactly like the local one. It also explains the interest model: hidden endpoints are `Suspended` and stream no frames, but they still project their workspace catalog and keep their agent feeds warm.

## Where it lives

- [machines.rs](../../crates/pilus-client/src/machines.rs) — profiles, `connect_machine`, and `SshTarget` derivation.
- [gui/mod.rs](../../crates/pilus/src/gui/mod.rs) — `Endpoint`, `EndpointId`, `PendingSwitch`, the reconnect state machine.
- [transport.rs](../../crates/pilus-client/src/transport.rs) — local vs SSH streams.
- [pinned.rs](../../crates/pilus/src/gui/pinned.rs) — pinned and recent workspaces are keyed by endpoint plus path.

## Related

- [Switch machines](../flows/machine-switch.md)
- [Remote access and the hub](../architecture/remote-access.md)
- [Client-owned chrome](./client-owned-chrome.md)
