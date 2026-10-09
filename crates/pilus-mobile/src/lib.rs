//! The device-side UHP client for `pilus` mobile apps.
//!
//! This crate is the counterpart of `pilus-agent`: where the connector
//! advertises a Luvus session over iroh/QUIC, this crate is what a phone or
//! tablet links to reach it. It owns the device's persistent iroh identity,
//! the enrollment handshake on `luvus/agent/1`, and UHP request/response on
//! `luvus/uhp/1` — one bidirectional stream per request, exactly like one
//! gateway connection.
//!
//! `ffi` is the C ABI the Swift app calls; everything else is ordinary Rust
//! so the same client can drive tests on the host.

pub mod client;
mod ffi;
mod store;

pub use client::{Client, Error};
pub use store::Enrollment;

/// The ALPN for the connector's enrollment and token channel.
///
/// Mirrors `pilus_agent::enroll::CONTROL_ALPN`; the value is the wire contract
/// and must not change independently of it.
pub const CONTROL_ALPN: &[u8] = b"luvus/agent/1";

/// The ALPN carrying verbatim UHP streams to the gateway.
///
/// Mirrors `pilus_agent::p2p::ALPN`.
pub const UHP_ALPN: &[u8] = b"luvus/uhp/1";

/// The ALPN for the connector's hub surface: one request/one reply plus the
/// `hub.watch` stream, authenticated by the device's `EndpointId`.
///
/// Mirrors `pilus_agent::hub::HUB_ALPN`; the value is the wire contract.
pub const HUB_ALPN: &[u8] = b"luvus/hub/1";
