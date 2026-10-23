---
title: Wiki Log
description: Append-only audit trail of wiki generation and refresh runs.
---

# Wiki Log

Append-only audit trail. Add one dated entry per generation or refresh run, recording the profile, the `source_commit` it was anchored to, and the coverage. The codebase-wiki skill describes the entry shape.

## 2026-10-23: generate

- Profile: internal/standard
- source_commit: c9e44d0
- Coverage: full repository survey (7 Rust crates, iOS app, site, infra, scripts, docs); 6 architecture pages, 9 module pages, 6 flows, 8 concepts, 5 guides; OVERVIEW stamped and every page linked from it.
- Pages: [Overview](./OVERVIEW.md), [System architecture](./architecture/system-architecture.md), [Daemon surface](./architecture/daemon-surface.md), [Native chrome](./architecture/native-chrome.md), [Remote access](./architecture/remote-access.md), [Sipil network](./architecture/sipil-network.md), [Persistence](./architecture/persistence.md), [Protocol](./modules/protocol.md), [Client](./modules/client.md), [GUI](./modules/gui.md), [Agent](./modules/agent.md), [Mobile](./modules/mobile.md), [DB](./modules/db.md), [Index](./modules/index.md), [iOS app](./modules/ios-app.md), [Site and infra](./modules/site-and-infra.md), [Attach and paint](./flows/attach-and-paint.md), [Switch machines](./flows/machine-switch.md), [Pair a phone](./flows/phone-pairing.md), [Sipil message](./flows/sipil-message.md), [Search and stats](./flows/session-search.md), [Release and update](./flows/release-update.md), [Surface](./concepts/surface.md), [UHP](./concepts/uhp.md), [Endpoint](./concepts/endpoint.md), [Sipil device ID](./concepts/sipil-device-id.md), [Enrollment level](./concepts/enrollment-level.md), [Client-owned chrome](./concepts/client-owned-chrome.md), [Idle cost](./concepts/idle-cost.md), [Migrations](./concepts/migrations.md), [Change the wire protocol](./guides/protocol-message.md), [Add a UI control](./guides/ui-control.md), [Add a native page](./guides/native-page.md), [Add a persisted setting](./guides/persisted-setting.md), [Run visual captures](./guides/visual-captures.md)
