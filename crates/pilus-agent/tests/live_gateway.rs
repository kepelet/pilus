//! The connector against a real `luvus uhp access` gateway.
//!
//! Skips itself when no `luvus` binary is on the machine, so `cargo test`
//! passes in a checkout without the daemon while still proving the descriptor
//! contract, pairing, and the gateway's access projection wherever it runs.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use pilus_agent::{AccessOptions, Gateway};

#[test]
fn pairs_with_a_real_gateway_when_available() {
    let Some(binary) = pilus_client::luvus_binary() else {
        return;
    };
    let options = AccessOptions {
        control: true,
        ttl_seconds: Some(30),
        ..Default::default()
    };
    let gateway = Gateway::start(&binary, &options).expect("gateway");

    let descriptor = gateway.descriptor();
    assert_eq!(descriptor.protocol_version, "luvus-uhp 1.0");
    assert_eq!(descriptor.mode.as_deref(), Some("control"));
    assert!(descriptor.address.starts_with("127.0.0.1:"));
    assert!(descriptor.scopes.iter().any(|scope| scope == "agent"));

    // Spending the pairing code yields a token the gateway accepts.
    let mut client = gateway.client().expect("pair");
    let capabilities = client.capabilities().expect("capabilities");
    assert_eq!(capabilities.protocol_version, "luvus-uhp 1.0");
    assert_eq!(capabilities.access_mode.as_deref(), Some("control"));
    assert!(capabilities.allows("agent.list"));
    assert!(
        !capabilities.methods.is_empty(),
        "the registry is advertised"
    );
}
