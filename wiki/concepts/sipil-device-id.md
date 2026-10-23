---
title: Sipil device ID
description: An iroh identity is the address and the authorization.
tags:
  - wiki
  - concept
---
# Sipil device ID

## Definition

A sipil device ID is an iroh `EndpointId`: 64 hex characters derived from a persistent Ed25519 identity key. It is both the address a node is dialed at and the identity the QUIC handshake proves. A ticket adds addresses to that identity and is accepted wherever an ID is.

## Why it matters

There are no accounts, URLs, or ports in the sipil network. Knowing a device ID grants nothing until the other side adds it, and the caller's identity is never a field in the frame — it is whatever the handshake proved. That removes a whole class of spoofing and lets authorization be a simple list. Losing the identity key changes the device ID everywhere it was added.

## Where it lives

- [node.rs](../../crates/pilus-client/src/sipil/node.rs) — the window's endpoint and its key under `~/.pilus/sipil/identity.key`.
- [wire.rs](../../crates/pilus-client/src/sipil/wire.rs) — framing and `API_VERSION`; identity is out of band.
- [contacts.rs](../../crates/pilus-client/src/sipil/contacts.rs) — the allow-list; a daemon contact is a different kind from a person.
- [p2p.rs](../../crates/pilus-agent/src/p2p.rs) and [enroll.rs](../../crates/pilus-agent/src/enroll.rs) — the connector's identity and per-device authorization.
- [mobile/client.rs](../../crates/pilus-mobile/src/client.rs) — the phone's persisted identity.

## Related

- [Sipil network](../architecture/sipil-network.md)
- [Enrollment level](./enrollment-level.md)
- [Sipil message](../flows/sipil-message.md)
