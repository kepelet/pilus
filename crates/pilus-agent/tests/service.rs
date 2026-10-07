//! The background service, end to end: the real `pilus-agent` binary against a
//! real Luvus daemon in a throwaway state root.
//!
//! It proves the things a person relies on once Remote Access is on: the
//! service brings its own gateway up, a code minted over the admin socket
//! enrolls a device, the hub serves the local machine (and refuses what the
//! gateway refuses), and when the gateway dies the supervisor starts a new one
//! and the same enrolled device keeps working with no new code.
//!
//! Skips itself when no `luvus` binary is installed. It needs network access
//! for iroh's relay and discovery, like `iroh_bridge`.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use iroh::Endpoint;
use iroh::endpoint::presets;
use iroh_tickets::endpoint::EndpointTicket;
use pilus_agent::{CONTROL_ALPN, HUB_ALPN};
use serde_json::{Value, json};

/// Variables a `luvus` child inherits from a pane that point its clients at
/// that pane's daemon. A test that keeps them would drive the developer's live
/// session instead of the throwaway one, so every command here drops them.
const AMBIENT: &[&str] = &[
    "LUVUS_SOCKET_PATH",
    "LUVUS_API_ADDRESS",
    "LUVUS_PANE_ID",
    "LUVUS_ENV",
    "LUVUS_SESSION",
];

fn isolated<'a>(command: &'a mut Command, root: &Path) -> &'a mut Command {
    for name in AMBIENT {
        command.env_remove(name);
    }
    command.env("LUVUS_HOME", root)
}

/// Kills the agent and the throwaway daemon even when an assertion fails.
struct Fixture {
    root: PathBuf,
    luvus: PathBuf,
    agent: Child,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = Command::new("/bin/kill")
            .arg("-TERM")
            .arg(self.agent.id().to_string())
            .status();
        let _ = self.agent.wait();
        let _ = isolated(&mut Command::new(&self.luvus), &self.root)
            .args(["server", "stop"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn admin(path: &Path, request: &Value) -> Option<Value> {
    let mut stream = UnixStream::connect(path).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    writeln!(stream, "{request}").ok()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    serde_json::from_str(line.trim()).ok()
}

fn wait_for<T>(what: &str, limit: Duration, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(found) = probe() {
            return found;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(250));
    }
}

async fn one_line(
    client: &Endpoint,
    address: &iroh::EndpointAddr,
    alpn: &[u8],
    request: &Value,
) -> Value {
    let connection = client
        .connect(address.clone(), alpn)
        .await
        .expect("connect");
    let (mut send, mut recv) = connection.open_bi().await.expect("open");
    send.write_all(format!("{request}\n").as_bytes())
        .await
        .expect("write");
    send.finish().expect("finish");
    let bytes = recv.read_to_end(4 * 1024 * 1024).await.expect("read");
    serde_json::from_slice(&bytes).expect("json reply")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn service_enrolls_serves_the_hub_and_survives_a_gateway_restart() {
    let Some(luvus) = pilus_client::luvus_binary() else {
        return;
    };
    // A short path: unix socket paths top out near a hundred bytes.
    let root = PathBuf::from(format!("/tmp/pa-svc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    let started = isolated(&mut Command::new(&luvus), &root)
        .args(["server", "start"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("luvus server start");
    assert!(
        started.success(),
        "could not start a throwaway Luvus server"
    );

    let identity = root.join("agent/identity.key");
    let agent = isolated(&mut Command::new(env!("CARGO_BIN_EXE_pilus-agent")), &root)
        .arg("serve")
        .args(["--iroh", "--control", "--service"])
        .arg("--identity")
        .arg(&identity)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn agent");
    let fixture = Fixture {
        root: root.clone(),
        luvus,
        agent,
    };
    let socket = root.join("agent/agent.sock");

    // The gateway comes up on its own and the status says so.
    let status = wait_for("the gateway to come up", Duration::from_secs(40), || {
        admin(&socket, &json!({ "type": "status" }))
            .filter(|status| status["gateway"]["up"] == true)
    });
    assert_eq!(status["type"], "status");
    assert!(
        status.get("code").is_none() && !status.to_string().contains("enroll"),
        "status must never carry a code"
    );
    let ticket: EndpointTicket = status["ticket"]
        .as_str()
        .expect("ticket")
        .parse()
        .expect("ticket parses");
    let address = ticket.endpoint_addr().clone();

    let client = Endpoint::builder(presets::N0).bind().await.expect("client");
    client.online().await;

    // Unenrolled: the hub is closed to it.
    let refused = client.connect(address.clone(), HUB_ALPN).await;
    let blocked = match refused {
        Err(_) => true,
        Ok(connection) => match connection.open_bi().await {
            Err(_) => true,
            Ok((mut send, mut recv)) => {
                let _ = send
                    .write_all(b"{\"id\":\"1\",\"method\":\"hub.machines\"}\n")
                    .await;
                let _ = send.finish();
                tokio::time::timeout(Duration::from_secs(5), recv.read_to_end(4096))
                    .await
                    .map_or(true, |read| read.map_or(true, |bytes| bytes.is_empty()))
            }
        },
    };
    assert!(blocked, "an unenrolled device must not reach the hub");

    // Mint a code on demand and enroll with it.
    let minted = admin(&socket, &json!({ "type": "new_code" })).expect("new_code");
    assert_eq!(minted["type"], "code");
    let link = minted["link"].as_str().expect("link");
    assert!(
        link.starts_with("pilus://pair/endpoint") && link.contains("code="),
        "{link}"
    );
    let code = minted["code"].as_str().expect("code");
    let enrolled = one_line(
        &client,
        &address,
        CONTROL_ALPN,
        &json!({ "type": "enroll", "code": code, "label": "test phone" }),
    )
    .await;
    assert_eq!(enrolled["type"], "enrolled");

    // The hub lists this machine and serves read calls through the gateway.
    let machines = one_line(
        &client,
        &address,
        HUB_ALPN,
        &json!({ "id": "1", "method": "hub.machines" }),
    )
    .await;
    assert_eq!(machines["result"]["machines"][0]["id"], "local");
    assert_eq!(machines["result"]["machines"][0]["ready"], true);
    let list = one_line(
        &client,
        &address,
        HUB_ALPN,
        &json!({ "id": "2", "method": "workspace.list" }),
    )
    .await;
    assert_eq!(list["result"]["type"], "workspace_list", "{list}");

    // And refuses what the gateway's token never allowed.
    let denied = one_line(
        &client,
        &address,
        HUB_ALPN,
        &json!({ "id": "3", "method": "task.add", "params": { "title": "x" } }),
    )
    .await;
    assert_eq!(denied["error"]["code"], "forbidden", "{denied}");
    let unknown = one_line(
        &client,
        &address,
        HUB_ALPN,
        &json!({ "id": "4", "machine": "no-such-machine", "method": "workspace.list" }),
    )
    .await;
    assert_eq!(unknown["error"]["code"], "not_found", "{unknown}");

    // Kill the gateway. The supervisor starts another and pairs again; the same
    // enrolled device needs no new code.
    let children = Command::new("/usr/bin/pgrep")
        .arg("-P")
        .arg(fixture.agent.id().to_string())
        .output()
        .expect("pgrep");
    let child = String::from_utf8_lossy(&children.stdout).trim().to_string();
    assert!(!child.is_empty(), "the agent should own a gateway child");
    for pid in child.lines() {
        let _ = Command::new("/bin/kill").arg("-KILL").arg(pid).status();
    }
    // The status can still read "up" for a beat after the kill, so judge
    // recovery by what matters: the hub answering through the new gateway.
    let deadline = Instant::now() + Duration::from_secs(40);
    let again = loop {
        let reply = one_line(
            &client,
            &address,
            HUB_ALPN,
            &json!({ "id": "5", "method": "workspace.list" }),
        )
        .await;
        if reply["result"]["type"] == "workspace_list" {
            break reply;
        }
        assert!(
            Instant::now() < deadline,
            "the hub never recovered: {reply}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    assert_eq!(again["id"], "5", "recovered without re-enrolling");

    // --- Levels. The device above enrolled at control; two more join at the
    // other levels, each with a code minted at that level.
    let enroll_at = |level: &'static str| {
        let socket = socket.clone();
        let address = address.clone();
        async move {
            let device = Endpoint::builder(presets::N0).bind().await.expect("device");
            device.online().await;
            let minted =
                admin(&socket, &json!({ "type": "new_code", "level": level })).expect("code");
            assert_eq!(minted["level"], level);
            let enrolled = one_line(
                &device,
                &address,
                CONTROL_ALPN,
                &json!({ "type": "enroll", "code": minted["code"], "label": level }),
            )
            .await;
            assert_eq!(enrolled["type"], "enrolled");
            device
        }
    };
    let operate = enroll_at("operate").await;
    let view = enroll_at("view").await;

    // Each device is told what it may do, not what the gateway's token may.
    let methods = |reply: &Value| -> Vec<String> {
        reply["result"]["access"]["allowed_methods"]
            .as_array()
            .expect("allowed_methods")
            .iter()
            .filter_map(|m| m.as_str().map(str::to_string))
            .collect()
    };
    let caps = |device: &Endpoint| {
        let (device, address) = (device.clone(), address.clone());
        async move {
            one_line(
                &device,
                &address,
                HUB_ALPN,
                &json!({ "id": "c", "method": "uhp.capabilities" }),
            )
            .await
        }
    };
    let control_caps = caps(&client).await;
    assert_eq!(control_caps["result"]["access"]["level"], "control");
    assert!(!methods(&control_caps).iter().any(|m| m == "task.add"));
    let operate_caps = caps(&operate).await;
    assert_eq!(operate_caps["result"]["access"]["level"], "operate");
    assert!(methods(&operate_caps).iter().any(|m| m == "task.add"));
    let view_caps = caps(&view).await;
    assert_eq!(view_caps["result"]["access"]["level"], "view");
    assert!(
        !methods(&view_caps).iter().any(|m| m == "agent.prompt"),
        "view is read-only"
    );
    assert!(methods(&view_caps).iter().any(|m| m == "workspace.list"));

    // Operate starts work, through the owner socket, as the user.
    let workspace_id = list["result"]["workspaces"][0]["workspace_id"].clone();
    let added = one_line(
        &operate,
        &address,
        HUB_ALPN,
        &json!({ "id": "o1", "method": "task.add", "params": { "title": "from the phone", "workspace_id": workspace_id } }),
    )
    .await;
    assert!(
        added.get("error").is_none(),
        "operate may add a task: {added}"
    );
    let tasks = one_line(
        &operate,
        &address,
        HUB_ALPN,
        &json!({ "id": "o2", "method": "task.list" }),
    )
    .await;
    assert!(
        tasks["result"]["tasks"]
            .to_string()
            .contains("from the phone"),
        "the task really exists in the daemon: {tasks}"
    );
    std::fs::create_dir_all(root.join("opened")).expect("dir");
    let opened = one_line(
        &operate,
        &address,
        HUB_ALPN,
        &json!({ "id": "o3", "method": "workspace.open", "params": { "path": root.join("opened") } }),
    )
    .await;
    assert!(
        opened.get("error").is_none(),
        "operate may open a workspace: {opened}"
    );

    // The same calls from the lower levels are refused.
    for (device, who) in [(&client, "control"), (&view, "view")] {
        let refused = one_line(
            device,
            &address,
            HUB_ALPN,
            &json!({ "id": "x", "method": "task.add", "params": { "title": "nope", "workspace_id": workspace_id } }),
        )
        .await;
        assert_eq!(refused["error"]["code"], "forbidden", "{who}: {refused}");
    }
    let view_write = one_line(
        &view,
        &address,
        HUB_ALPN,
        &json!({ "id": "v", "method": "agent.prompt", "params": { "target": "1", "text": "x" } }),
    )
    .await;
    assert_eq!(view_write["error"]["code"], "forbidden", "{view_write}");
    let view_read = one_line(
        &view,
        &address,
        HUB_ALPN,
        &json!({ "id": "v2", "method": "workspace.list" }),
    )
    .await;
    assert_eq!(view_read["result"]["type"], "workspace_list", "{view_read}");

    // A level can be lowered from the host, never raised from it, and the change
    // applies to the very next request on a connection that is already open.
    let status = admin(&socket, &json!({ "type": "status" })).expect("status");
    let operate_id = status["devices"]
        .as_array()
        .expect("devices")
        .iter()
        .find(|d| d["level"] == "operate")
        .expect("operate device")["endpoint_id"]
        .as_str()
        .expect("id")
        .to_string();
    let raised = admin(
        &socket,
        &json!({ "type": "set_level", "endpoint_id": operate_id, "level": "operate" }),
    );
    assert!(
        raised.is_none()
            || raised
                .as_ref()
                .is_some_and(|r| r["type"] == "level" && r["changed"] == false)
    );
    let view_id = status["devices"]
        .as_array()
        .expect("devices")
        .iter()
        .find(|d| d["level"] == "view")
        .expect("view device")["endpoint_id"]
        .as_str()
        .expect("id")
        .to_string();
    // `admin` hides error replies behind `None`, so check the refusal by effect.
    let _ = admin(
        &socket,
        &json!({ "type": "set_level", "endpoint_id": view_id, "level": "operate" }),
    );
    let still_view = admin(&socket, &json!({ "type": "status" })).expect("status");
    assert!(
        still_view["devices"]
            .as_array()
            .expect("devices")
            .iter()
            .any(|d| d["endpoint_id"] == view_id.as_str() && d["level"] == "view"),
        "a level cannot be raised from the host's admin socket"
    );
    let lowered = admin(
        &socket,
        &json!({ "type": "set_level", "endpoint_id": operate_id, "level": "control" }),
    )
    .expect("lower");
    assert_eq!(lowered["changed"], true);
    let after = one_line(
        &operate,
        &address,
        HUB_ALPN,
        &json!({ "id": "o4", "method": "task.add", "params": { "title": "too late", "workspace_id": workspace_id } }),
    )
    .await;
    assert_eq!(
        after["error"]["code"], "forbidden",
        "lowering takes effect at once: {after}"
    );

    // A fresh operate code raises the same device again.
    let raise = admin(&socket, &json!({ "type": "new_code", "level": "operate" })).expect("code");
    let enrolled = one_line(
        &operate,
        &address,
        CONTROL_ALPN,
        &json!({ "type": "enroll", "code": raise["code"], "label": "operate" }),
    )
    .await;
    assert_eq!(enrolled["type"], "enrolled");
    let again_caps = caps(&operate).await;
    assert_eq!(
        again_caps["result"]["access"]["level"], "operate",
        "a live code raises a level"
    );

    // Revoking closes the door again.
    let devices = admin(&socket, &json!({ "type": "status" })).expect("status");
    let id = devices["devices"][0]["endpoint_id"]
        .as_str()
        .expect("device")
        .to_string();
    let revoked = admin(&socket, &json!({ "type": "revoke", "endpoint_id": id })).expect("revoke");
    assert_eq!(revoked["removed"], true);

    let _ = client.close().await;
    drop(fixture);
}
