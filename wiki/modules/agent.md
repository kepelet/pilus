---
title: pilus-agent
description: Gateway lifecycle, iroh transport, enrollment, hub, and service.
tags:
  - wiki
  - module
---
# pilus-agent

## Summary

`pilus-agent` is the host-side connector. It launches a Luvus access gateway, carries its loopback bytes to remote clients over TCP or iroh/QUIC, enrolls devices, and serves a hub that fans a phone's requests out to the host and its saved SSH machines. It runs as a per-user background service so remote access survives the window closing.

## Responsibilities

- Launch `luvus uhp access`, parse its one-line descriptor, and hold the child ([lib.rs](../../crates/pilus-agent/src/lib.rs)).
- Keep the gateway paired and alive across daemon restarts and sleep ([supervisor.rs](../../crates/pilus-agent/src/supervisor.rs)).
- Forward bytes over TCP ([proxy.rs](../../crates/pilus-agent/src/proxy.rs)) or iroh ([p2p.rs](../../crates/pilus-agent/src/p2p.rs)).
- Enroll devices once with a short code and persist their `EndpointId` with a level ([enroll.rs](../../crates/pilus-agent/src/enroll.rs)).
- Serve `luvus/hub/1`: local gateway methods, SSH fan-out, `hub.*`, and `hub.gh.*` ([hub.rs](../../crates/pilus-agent/src/hub.rs)).
- Turn daemon events into topic signals ([watch.rs](../../crates/pilus-agent/src/watch.rs)).
- Install and report the background service ([service.rs](../../crates/pilus-agent/src/service.rs)) and answer the window over the admin socket ([admin.rs](../../crates/pilus-agent/src/admin.rs)).

## Public API / entry points

CLI ([main.rs](../../crates/pilus-agent/src/main.rs)):

```text
pilus-agent serve [--iroh] [--listen ADDR] [--control] [--machines] [--session NAME]
pilus-agent descriptor [--control] [--machines] [--session NAME]
pilus-agent service install|uninstall|restart|status
```

Library re-exports from [lib.rs](../../crates/pilus-agent/src/lib.rs): `Gateway`, `AccessOptions`, `Descriptor`, `proxy::serve`, `p2p`, `enroll::{AuthState, DeviceStore, Enrollment, Level, CONTROL_ALPN}`, `hub::HUB_ALPN`, `supervisor::GatewayLink`, `admin`, `service`.

## Key files

| Concern | File |
| --- | --- |
| Gateway and descriptor | `lib.rs` |
| Keep-alive loop | `supervisor.rs` |
| TCP transport | `proxy.rs` |
| iroh transport and ALPN | `p2p.rs` |
| Enrollment, levels, device store | `enroll.rs` |
| Hub methods and allow-lists | `hub.rs` |
| Topic signals | `watch.rs` |
| Service install/status | `service.rs` |
| Admin socket | `admin.rs` |

Tests: [tests/live_gateway.rs](../../crates/pilus-agent/tests/live_gateway.rs) exercises a real `luvus uhp access`; [tests/iroh_bridge.rs](../../crates/pilus-agent/tests/iroh_bridge.rs) proves the whole broker including enrollment and token use; [tests/service.rs](../../crates/pilus-agent/tests/service.rs) runs the real binary with a throwaway `LUVUS_HOME` and scrubs ambient socket env vars. All skip when the prerequisites are absent.

## Dependencies

`pilus-client` (UHP, machines, GitHub, folders), `iroh`, `serde`/`serde_json`, `anyhow`, `thiserror`. No GPUI.

## State

`~/.pilus/agent`: `identity.key`, `devices.json`, `agent.sock` (`0600` in a `0700` directory), `agent.log`. Older builds wrote under the Luvus root; first run moves identity and devices and never overwrites a file at the new path.

## Design decisions

- **Narrow contract.** The connector forwards UHP bytes unchanged; it only introduces pairing, enrollment, and authority.
- **Fail closed.** An empty gateway allow-list admits nothing; unknown methods count as writes; streaming methods are refused at every level.
- **Idempotent service install.** The window calls it on every launch, so a healthy running service is left alone rather than bounced.
- **No stdout dependency.** The window reads the service over the admin socket, which is why the connector can run under launchd/systemd with almost no environment.

## Participates in

- [Remote access and the hub](../architecture/remote-access.md)
- [Pair a phone](../flows/phone-pairing.md)
- [Switch machines](../flows/machine-switch.md)

## Related

- [Enrollment level](../concepts/enrollment-level.md)
- [pilus-mobile](./mobile.md), [iOS app](./ios-app.md)
