---
title: pilus-client
description: Transport, handshake, Surface, UHP, sipil, and host-tool clients.
tags:
  - wiki
  - module
---
# pilus-client

## Summary

`pilus-client` is the connection layer shared by the desktop window, the host connector, and the phone client. It locates a daemon, opens a local or SSH transport, performs the version-checked handshake, streams events to the caller, folds server messages into a `Surface`, speaks UHP for structured state, and implements the sipil client. It owns no rendering.

It is also the crate that owns most of the non-daemon integration surface: `git`, `gh`, `ssh`, file reads/writes, uploads/downloads, ports, the GitHub cache, and the process/netlog diagnostics.

## Responsibilities

- **Attach** — discovery ([discovery.rs](../../crates/pilus-client/src/discovery.rs)), autostart ([autostart.rs](../../crates/pilus-client/src/autostart.rs)), transports ([transport.rs](../../crates/pilus-client/src/transport.rs)), handshake and threads ([connection.rs](../../crates/pilus-client/src/connection.rs)).
- **Project** — the live `Surface` ([surface.rs](../../crates/pilus-client/src/surface.rs)) and the `Arrival` wake signal ([arrival.rs](../../crates/pilus-client/src/arrival.rs)).
- **Control** — `UhpClient` ([uhp.rs](../../crates/pilus-client/src/uhp.rs)), `ControlClient` and event-driven `AgentFeed` ([control.rs](../../crates/pilus-client/src/control.rs)).
- **Machines** — saved SSH profiles through the `luvus` CLI ([machines.rs](../../crates/pilus-client/src/machines.rs)), folder listing ([folders.rs](../../crates/pilus-client/src/folders.rs)).
- **Workspace data** — files ([files.rs](../../crates/pilus-client/src/files.rs)), reads/writes/entries/upload/download, staged changes ([stage.rs](../../crates/pilus-client/src/stage.rs)), git status and commits ([git_status.rs](../../crates/pilus-client/src/git_status.rs), [commit.rs](../../crates/pilus-client/src/commit.rs)), diff review ([diff.rs](../../crates/pilus-client/src/diff.rs)), ports ([ports.rs](../../crates/pilus-client/src/ports.rs)).
- **GitHub** — one checkout ([github.rs](../../crates/pilus-client/src/github.rs)) and the signed-in account with its cache ([github_account.rs](../../crates/pilus-client/src/github_account.rs)).
- **Sipil** — the whole client in [sipil/](../../crates/pilus-client/src/sipil/mod.rs).
- **Diagnostics** — process counters ([process.rs](../../crates/pilus-client/src/process.rs)) and the network ring ([netlog.rs](../../crates/pilus-client/src/netlog.rs)).

## Public API / entry points

[lib.rs](../../crates/pilus-client/src/lib.rs) re-exports the intentional surface: `Connection`, `ClientEvent`, `EndpointIdentity`, `Surface`, `SurfaceChange`, `ConnectTarget`, `connect`; `ControlClient`, `ControlTransport`, `AgentFeed`, `Agent`, `Tab`, `BarWidget`; `SshTarget`, `MachineInfo`, `connect_machine`; `UhpClient`, `Observer`, `Capabilities`; `GhRepo`, `GhAccount`; `read_text`, `write_text`, `create_entry`, `upload_path`, `download_path`; `show_commit`, `set_staged`; `pilus_client::sipil::*`.

Tests: `tests/handshake.rs` (protocol negotiation), `tests/control.rs`, `tests/uhp.rs`, `tests/uhp_access.rs`, and `tests/sipil.rs` (which drives the real daemon when the [sipil](https://github.com/kepelet/sipil) checkout has `node_modules`).

## Key files

| Area | Files |
| --- | --- |
| Attach | `discovery.rs`, `autostart.rs`, `transport.rs`, `connection.rs`, `error.rs` |
| Surface | `surface.rs`, `arrival.rs`, `ansi.rs`, `uhp.rs` |
| Control | `control.rs`, `agents.rs`, `tasks.rs`, `mission.rs`, `diff.rs`, `files.rs`, `git_status.rs`, `commit.rs` |
| Machine | `machines.rs`, `folders.rs`, `read.rs`, `write.rs`, `entries.rs`, `upload.rs`, `download.rs`, `git.rs`, `stage.rs`, `ports.rs` |
| GitHub | `github.rs`, `github_account.rs` |
| Sipil | `sipil/mod.rs`, `node.rs`, `peer.rs`, `wire.rs`, `contacts.rs`, `channels.rs`, `mention.rs`, `chat.rs`, `offline.rs`, `backoff.rs`, `history.rs`, `watch.rs`, `store.rs` |
| Diagnostics | `process.rs`, `netlog.rs`, `agent_service.rs`, `connector.rs`, `relay.rs` |

## Dependencies

`pilus-protocol`, `pilus-db` (stores), `crossbeam-channel`, `serde`/`serde_json`, `bincode`, `iroh`, `iroh-tickets`, `rusqlite` (indirectly through `pilus-db`), `thiserror`, `ureq` (downloads), `unicode-width`. It does not depend on GPUI.

## Design decisions

- **One `Stream` for every transport.** Unix socket, TCP, and an SSH child look identical to the handshake and frame loop, so multi-machine support did not fork the protocol path.
- **Reader/coalescer/writer threads.** No IO on the GUI thread; bursts of frames are collapsed so painting stays responsive.
- **Structured state goes over UHP.** The render socket carries pixels; agents, tabs, bar widgets, diffs, files, and git come from JSON methods, which keeps the binary protocol small.
- **Subprocesses for host tools.** `git`, `gh`, `lsof`, `ssh`, and `tar` are run where the data lives, with arguments quoted for the remote login shell. Text a person typed reaches `gh` on stdin, never a command line.
- **The index is not here.** Transcript reading lives in `pilus-index`, which the GUI calls directly and which enforces consent at its caller.

## Participates in

- [Attach and paint](../flows/attach-and-paint.md)
- [Switch machines](../flows/machine-switch.md)
- [Sipil message](../flows/sipil-message.md)
- [Pair a phone](../flows/phone-pairing.md)

## Related

- [Daemon surface and wire protocol](../architecture/daemon-surface.md)
- [Sipil network](../architecture/sipil-network.md)
- [pilus-protocol](./protocol.md), [pilus (GUI)](./gui.md)
