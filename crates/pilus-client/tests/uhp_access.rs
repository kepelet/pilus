//! The UHP client against a paired `luvus uhp access` gateway over TCP.
//!
//! Proves the transport seam the host agent depends on: UHP frames over a TCP
//! loopback, a delegated `auth` token on every envelope, and the capability
//! projection a gateway adds.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::thread;

use pilus_client::UhpClient;

const TOKEN: &str = "luvus_tok_test";

#[test]
fn pairs_over_tcp_and_reads_capabilities() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("addr").to_string();

    let server = thread::spawn(move || {
        // `uhp.capabilities`: assert the delegated token rode the envelope,
        // then answer with the access projection a gateway adds.
        let (stream, _) = listener.accept().expect("accept capabilities");
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let mut writer = stream;
        let mut line = String::new();
        reader.read_line(&mut line).expect("read capabilities");
        let request: serde_json::Value = serde_json::from_str(&line).expect("json");
        assert_eq!(request["method"], "uhp.capabilities");
        assert_eq!(request["auth"], TOKEN);
        let response = serde_json::json!({
            "id": request["id"],
            "result": {
                "protocol": { "name": "luvus-uhp", "major": 1, "minor": 0 },
                "methods": ["uhp.capabilities", "agent.list"],
                "access": {
                    "mode": "control",
                    "allowed_methods": ["uhp.capabilities", "agent.list"],
                    "limits": { "connections": 16, "requests_per_minute": 120 }
                }
            }
        });
        writeln!(writer, "{response}").expect("write capabilities");

        // An ordinary call also carries the token.
        let (stream, _) = listener.accept().expect("accept ping");
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let mut writer = stream;
        let mut line = String::new();
        reader.read_line(&mut line).expect("read ping");
        let request: serde_json::Value = serde_json::from_str(&line).expect("json");
        assert_eq!(request["method"], "ping");
        assert_eq!(request["auth"], TOKEN);
        let response = serde_json::json!({ "id": request["id"], "result": { "type": "ok" } });
        writeln!(writer, "{response}").expect("write ping");
    });

    let mut client = UhpClient::connect_tcp(address).with_auth(TOKEN);
    let capabilities = client.capabilities().expect("capabilities");
    assert_eq!(capabilities.protocol_version, "luvus-uhp 1.0");
    assert_eq!(capabilities.access_mode.as_deref(), Some("control"));
    assert_eq!(capabilities.methods, vec!["uhp.capabilities", "agent.list"]);
    assert!(capabilities.allows("agent.list"));
    assert!(!capabilities.allows("terminal.backend.control"));

    let result = client.request("ping", serde_json::json!({})).expect("ping");
    assert_eq!(result["type"], "ok");

    server.join().expect("server thread");
}

#[test]
fn owner_endpoint_allows_every_method() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("addr").to_string();

    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let mut writer = stream;
        let mut line = String::new();
        reader.read_line(&mut line).expect("read");
        let request: serde_json::Value = serde_json::from_str(&line).expect("json");
        // No `auth` on the owner endpoint, and no access projection.
        assert!(request.get("auth").is_none());
        let response = serde_json::json!({
            "id": request["id"],
            "result": { "protocol_version": "luvus-uhp 1.0", "methods": [] }
        });
        writeln!(writer, "{response}").expect("write");
    });

    let mut client = UhpClient::connect_tcp(address);
    let capabilities = client.capabilities().expect("capabilities");
    assert_eq!(capabilities.access_mode, None);
    assert!(capabilities.allows("terminal.backend.control"));

    server.join().expect("server thread");
}
