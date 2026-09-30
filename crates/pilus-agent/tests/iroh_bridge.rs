//! Device enrollment and UHP over iroh, against a real gateway.
//!
//! Skips itself when no `luvus` binary is installed. It needs network access
//! for iroh's address lookup and relay, so it is the one test that can be slow.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use iroh::Endpoint;
use iroh::endpoint::presets;
use iroh_tickets::endpoint::EndpointTicket;
use pilus_agent::enroll::{AuthState, CONTROL_ALPN, ENROLLMENT_TTL};
use pilus_agent::{AccessOptions, Gateway, p2p};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enrolls_a_device_then_serves_uhp() {
    let Some(binary) = pilus_client::luvus_binary() else {
        return;
    };
    let options = AccessOptions {
        control: true,
        no_expiry: true,
        ..Default::default()
    };
    let gateway = Gateway::start(&binary, &options).expect("gateway");
    let gateway_address = gateway.descriptor().address.clone();
    let token = gateway.pair().expect("pair");

    let dir = std::env::temp_dir().join(format!("pilus-agent-enroll-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let key_path = dir.join("identity.key");
    let auth = Arc::new(
        AuthState::load(token.clone(), dir.join("devices.json"), &[], ENROLLMENT_TTL).expect("auth"),
    );
    let (code, _) = auth.enrollment();

    let endpoint = p2p::bind(&key_path, None, Arc::clone(&auth))
        .await
        .expect("bind");
    let ticket = EndpointTicket::new(endpoint.addr());
    let server_gateway = gateway_address.clone();
    let server_auth = Arc::clone(&auth);
    let server = tokio::spawn(async move { p2p::run(endpoint, server_gateway, server_auth).await });

    let client = Endpoint::builder(presets::N0)
        .bind()
        .await
        .expect("client bind");
    client.online().await;
    let address = ticket.endpoint_addr().clone();

    // An unenrolled device cannot reach the UHP surface.
    assert!(
        !uhp_round_trip(&client, address.clone()).await,
        "an unenrolled device must not reach UHP"
    );

    // Enroll over the control channel, then collect the delegated token.
    let control = client
        .connect(address.clone(), CONTROL_ALPN)
        .await
        .expect("control connect");
    let (mut send, mut recv) = control.open_bi().await.expect("open enroll");
    let frame = format!("{{\"type\":\"enroll\",\"code\":\"{code}\",\"label\":\"phone\"}}\n");
    send.write_all(frame.as_bytes()).await.expect("write enroll");
    send.finish().expect("finish enroll");
    let reply = recv.read_to_end(4096).await.expect("read enroll reply");
    let reply: serde_json::Value = serde_json::from_slice(&reply).expect("enroll json");
    assert_eq!(reply["type"], "enrolled");

    let (mut send, mut recv) = control.open_bi().await.expect("open token");
    send.write_all(b"{\"type\":\"token\"}\n")
        .await
        .expect("write token");
    send.finish().expect("finish token");
    let reply = recv.read_to_end(4096).await.expect("read token reply");
    let token = serde_json::from_slice::<serde_json::Value>(&reply).expect("token json")["token"]
        .as_str()
        .expect("token")
        .to_string();

    // With the token, the same device speaks UHP on the UHP ALPN.
    let uhp = client
        .connect(address, p2p::ALPN)
        .await
        .expect("uhp connect");
    let (mut send, mut recv) = uhp.open_bi().await.expect("open uhp");
    let request = format!(
        "{{\"id\":\"1\",\"method\":\"uhp.capabilities\",\"params\":{{}},\"auth\":\"{token}\"}}\n"
    );
    send.write_all(request.as_bytes()).await.expect("write caps");
    send.finish().expect("finish caps");
    let reply = recv.read_to_end(65536).await.expect("read caps");
    let value: serde_json::Value = serde_json::from_slice(&reply).expect("caps json");
    assert_eq!(
        value["result"]["access"]["mode"], "control",
        "the paired gateway projection survives the iroh bridge"
    );

    let _ = client.close().await;
    server.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Whether a `ping` over UHP gets a response. A rejected connection errors or
/// stalls, so a timeout counts as a failure to reach the surface.
async fn uhp_round_trip(client: &Endpoint, address: iroh::EndpointAddr) -> bool {
    let Ok(connection) = client.connect(address, p2p::ALPN).await else {
        return false;
    };
    let Ok((mut send, mut recv)) = connection.open_bi().await else {
        return false;
    };
    if send
        .write_all(b"{\"id\":\"1\",\"method\":\"ping\",\"params\":{}}\n")
        .await
        .is_err()
    {
        return false;
    }
    let _ = send.finish();
    match tokio::time::timeout(Duration::from_secs(8), recv.read_to_end(4096)).await {
        Ok(Ok(bytes)) => !bytes.is_empty(),
        _ => false,
    }
}
