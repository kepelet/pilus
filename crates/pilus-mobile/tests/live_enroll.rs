//! End-to-end client check against a real `luvus uhp access` gateway.
//!
//! Mirrors `pilus-agent`'s `iroh_bridge` test from the device side: the
//! connector serves a real gateway over iroh, and [`Client`] enrolls, holds
//! the token, and runs `workspace.list`/`agent.list`. Skips when no `luvus`
//! binary is installed or no session server answers.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;

use iroh_tickets::endpoint::EndpointTicket;
use pilus_agent::enroll::{AuthState, ENROLLMENT_TTL};
use pilus_agent::{AccessOptions, Gateway, p2p};
use pilus_mobile::Client;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enrolls_then_lists_workspaces_and_agents() {
    let Some(binary) = pilus_client::luvus_binary() else {
        return;
    };
    let options = AccessOptions {
        control: true,
        no_expiry: true,
        ..Default::default()
    };
    let gateway = Gateway::start(&binary, &options).expect("gateway");
    let token = gateway.pair().expect("pair");

    let dir = std::env::temp_dir().join(format!("pilus-mobile-live-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let key_path = dir.join("host-identity.key");
    let auth = Arc::new(
        AuthState::load(token, dir.join("devices.json"), &[], ENROLLMENT_TTL).expect("auth"),
    );
    let (code, _) = auth.enrollment().expect("enrollment is enabled");

    let endpoint = p2p::bind(&key_path, &p2p::RelayConfig::default(), Arc::clone(&auth))
        .await
        .expect("bind");
    let ticket = EndpointTicket::new(endpoint.addr()).to_string();
    let server = tokio::spawn({
        let auth = Arc::clone(&auth);
        let gateway = gateway.descriptor().address.clone();
        async move { p2p::run(endpoint, gateway, auth).await }
    });

    // The client is a synchronous facade over its own runtime, so drive it
    // from a plain OS thread — the same way the app calls it from a detached
    // task rather than from inside this test's runtime.
    let device_dir = dir.join("device");
    let code_for_thread = code.clone();
    let worker = std::thread::spawn(move || {
        // A fresh device is refused on the UHP surface until it enrolls.
        let client = Client::connect(&device_dir, &ticket, "", "").expect("connect");
        match client.request("workspace.list", serde_json::json!({})) {
            Err(pilus_mobile::Error::NotEnrolled) => {}
            other => panic!("an unenrolled device must fail closed, got {other:?}"),
        }

        client.enroll(&code_for_thread, "dev-test").expect("enroll");
        assert!(client.is_authorized());

        let workspaces = client
            .request("workspace.list", serde_json::json!({}))
            .expect("workspace.list");
        assert!(
            workspaces
                .get("workspaces")
                .and_then(|rows| rows.as_array())
                .is_some(),
            "workspace.list returned {workspaces}"
        );
        let agents = client
            .request("agent.list", serde_json::json!({}))
            .expect("agent.list");
        assert!(
            agents
                .get("agents")
                .and_then(|rows| rows.as_array())
                .is_some(),
            "agent.list returned {agents}"
        );

        // Resuming rebuilds the client from the enrollment file alone.
        let resumed = Client::resume(&device_dir, "", "").expect("resume");
        let workspaces = resumed
            .request("workspace.list", serde_json::json!({}))
            .expect("resumed workspace.list");
        assert!(workspaces.get("workspaces").is_some());
    });
    worker.join().expect("client worker");

    server.abort();
    let _ = std::fs::remove_dir_all(&dir);
}
