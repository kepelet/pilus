---
title: Enrollment level
description: View, control, operate, and who may raise them.
tags:
  - wiki
  - concept
---
# Enrollment level

## Definition

An enrollment level is how much a paired phone may do on a host: `view`, `control`, or `operate`. It is stored per device in `devices.json` and checked on every stream, so a change applies to an already-open connection on its next request.

## Why it matters

A phone is a remote door into a machine that runs coding agents. The level is the whole authorization model: `view` reads, `control` prompts agents and reviews diffs, `operate` starts agents, tasks, and shells — remote code execution by design. Because operate is that powerful, it is opt-in per device and can only be **raised** by a live enrollment code minted at that level on the host. The admin socket the window uses can only lower it. An empty gateway allow-list fails closed at every level.

## Where it lives

- [enroll.rs](../../crates/pilus-agent/src/enroll.rs) — `Level`, the device store, `CONTROL_ALPN`, code TTL and rotation.
- [hub.rs](../../crates/pilus-agent/src/hub.rs) — `REVIEW` and `OPERATE` sets, per-stream level lookup, method allow-lists, capability rewriting.
- [admin.rs](../../crates/pilus-agent/src/admin.rs) — `set_level` (lower only), `revoke`, `new_code`.
- [ios/Pilus/AppStore.swift](../../ios/Pilus/AppStore.swift) — `level` and `allowedMethods` gate what the app offers.

## Related

- [Remote access and the hub](../architecture/remote-access.md)
- [Pair a phone](../flows/phone-pairing.md)
- [Sipil device ID](./sipil-device-id.md)
