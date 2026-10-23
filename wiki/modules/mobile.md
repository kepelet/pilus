---
title: pilus-mobile
description: Device-side iroh client, enrollment persistence, and the C FFI.
tags:
  - wiki
  - module
---
# pilus-mobile

## Summary

`pilus-mobile` is the device-side client for the phone app. It is the counterpart of `pilus-agent`: it owns the phone's persistent iroh identity, the enrollment handshake on `luvus/agent/1`, and UHP request/response on `luvus/uhp/1`, with each bidirectional stream carrying one request exactly like one gateway connection. It also speaks the hub ALPN `luvus/hub/1`.

The crate is ordinary Rust so it can be tested on the host, plus one FFI module that the Swift app links.

## Responsibilities

- Own the iroh endpoint and a private tokio runtime behind a blocking facade.
- Enroll with a host using a short code, persist the enrollment, and refresh the delegated token.
- Send hub requests (one JSON line each way) and hold the `hub.watch` stream.
- Track lost replies (`unconfirmed`) so the app resends only idempotent calls.
- Expose a C ABI with opaque handles and JSON strings.

## Public API / entry points

[lib.rs](../../crates/pilus-mobile/src/lib.rs) re-exports `Client`, `Error`, `Enrollment` and the ALPN constants `CONTROL_ALPN` (`luvus/agent/1`), `UHP_ALPN` (`luvus/uhp/1`), `HUB_ALPN` (`luvus/hub/1`). These mirror the connector's constants and are wire contracts.

[ffi.rs](../../crates/pilus-mobile/src/ffi.rs) is the C ABI: every entry point is `extern "C"` and blocking, with `pilus_client_free` / `pilus_string_free` owning returned memory and `pilus_last_error` reading a thread-local message. `unsafe` is lifted only here, with every pointer checked.

## Key files

- [client.rs](../../crates/pilus-mobile/src/client.rs) — endpoint, connections per ALPN, request timeouts, watch idle limit.
- [store.rs](../../crates/pilus-mobile/src/store.rs) — the identity key and enrollment record on device.
- [ffi.rs](../../crates/pilus-mobile/src/ffi.rs) — the C surface.
- [ios/Pilus/PilusClient.swift](../../ios/Pilus/PilusClient.swift) — the Swift wrapper that calls it from `Task.detached`.

## Dependencies

`iroh`, `iroh-tickets`, `serde`/`serde_json`, `tokio` (private runtime), `thiserror`. It does not depend on `pilus-client`'s render side; the hub protocol is JSON, not bincode.

## Timeouts

- 15 s for one connect or pairing/token exchange.
- 30 s for one UHP or hub request, longer than the hub's own 25 s so a call the hub is still running is not abandoned.
- 60 s of silence on `hub.watch` before the stream is considered dead (the hub pings every 20 s).

## Design decisions

- **Blocking facade, async inside.** Swift should not see `async`; the runtime is an implementation detail.
- **One stream per request.** This matches the gateway's contract and makes retries and cancellation straightforward.
- **The app owns retry policy.** The Rust client reports `unconfirmed`; Swift resends only reads and calls carrying an idempotency key.
- **Wire constants are duplicated on purpose.** The ALPN strings are part of the contract, not a shared dependency; both sides name them and a test keeps them aligned.

## Participates in

- [Pair a phone](../flows/phone-pairing.md)
- [Remote access and the hub](../architecture/remote-access.md)

## Related

- [pilus-agent](./agent.md)
- [iOS app](./ios-app.md)
- [Enrollment level](../concepts/enrollment-level.md)
