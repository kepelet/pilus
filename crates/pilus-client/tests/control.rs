//! The JSON control client against a scripted server socket.
//!
//! The daemon answers exactly one request per connection, so the mock accepts a
//! fresh connection per call and validates that the client does the same.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::thread;

use pilus_client::ControlClient;

#[test]
fn reads_and_commands_over_one_connection_per_request() {
    let dir = std::env::temp_dir().join(format!("pilus-control-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let socket = dir.join("luvus.sock");
    let _ = std::fs::remove_file(&socket);

    let listener = UnixListener::bind(&socket).expect("bind");
    let server = thread::spawn(move || {
        for _ in 0..15 {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut writer = stream;
            let mut line = String::new();
            reader.read_line(&mut line).expect("read request");
            let request: serde_json::Value = serde_json::from_str(&line).expect("json");
            let result = match request["method"].as_str().expect("method") {
                "agent.list" => serde_json::json!({
                    "type": "agent_list",
                    "agents": [
                        {
                            "pane": "2", "tab": "1", "agent": "claude", "name": "auth-fix",
                            "status": "working", "workspace_id": "ws1",
                            "workspace_name": "api", "cwd": "/repo/api",
                            "branch": "main", "focused": true
                        },
                        {
                            "pane": "4", "tab": "2", "agent": "codex", "name": "",
                            "status": "blocked", "workspace_id": "ws2",
                            "workspace_name": "web", "cwd": "/repo/web",
                            "branch": null, "focused": false
                        }
                    ]
                }),
                "ui.bar.list" => serde_json::json!({
                    "type": "bar_list",
                    "revision": 7,
                    "widgets": [{
                        "key": "core:runtime-status",
                        "region": "bottom-right",
                        "content": [
                            { "type": "text", "text": "NORMAL", "tone": "muted" },
                            { "type": "separator", "tone": "muted" },
                            { "type": "text", "text": "tab 2/2", "tone": "normal" }
                        ]
                    }]
                }),
                "tab.list" => serde_json::json!({
                    "type": "tab_list",
                    "tabs": [
                        { "tab": "1", "tab_id": "t1", "active": false, "name": "", "kind": "panes" },
                        { "tab": "2", "tab_id": "t2", "active": true, "name": "docs", "kind": "panes" }
                    ]
                }),
                "session.snapshot" => serde_json::json!({
                    "type": "session_snapshot",
                    "workspaces": [{
                        "id": "ws1", "cwd": "/repo", "active": true,
                        "tabs": [
                            {
                                "index": 1,
                                "panes": [
                                    { "kind": "terminal", "cwd": "/repo/api", "agent": "fish", "focused": false }
                                ]
                            },
                            {
                                "index": 2,
                                "panes": [
                                    { "kind": "terminal", "cwd": "/repo/docs", "agent": "devin", "focused": true }
                                ]
                            }
                        ]
                    }]
                }),
                "tab.focus" => {
                    assert_eq!(request["params"]["tab"], "1");
                    serde_json::json!({ "type": "ok" })
                }
                "pane.focus" => {
                    assert_eq!(request["params"]["pane"], "4");
                    serde_json::json!({ "type": "pane_focus", "pane": "4" })
                }
                "workspace.rename" => {
                    assert_eq!(request["params"]["workspace_id"], "ws1");
                    assert_eq!(request["params"]["name"], "assets");
                    serde_json::json!({ "type": "ok" })
                }
                "workspace.close" => {
                    assert_eq!(request["params"]["workspace_id"], "ws2");
                    serde_json::json!({ "type": "ok" })
                }
                "pane.close" => {
                    assert_eq!(request["params"]["pane"], "4");
                    serde_json::json!({ "type": "ok" })
                }
                "agent.name" if request["params"]["clear"] == true => {
                    assert_eq!(request["params"]["pane"], "2");
                    serde_json::json!({ "type": "agent_name", "pane": "2", "name": null })
                }
                "agent.name" => {
                    assert_eq!(request["params"]["pane"], "2");
                    assert_eq!(request["params"]["name"], "auth-fix");
                    serde_json::json!({ "type": "agent_name", "pane": "2", "name": "auth-fix" })
                }
                "worktree.create" => {
                    assert_eq!(request["params"]["workspace"], 3);
                    assert_eq!(request["params"]["branch"], "fix-branch");
                    serde_json::json!({ "type": "ok", "path": "/repo/fix-branch" })
                }
                "worktree.list" => {
                    assert_eq!(request["params"]["workspace"], 0);
                    serde_json::json!({
                        "type": "worktree_list",
                        "worktrees": [
                            { "path": "/repo", "branch": "main", "head": "abc", "main": true },
                            { "path": "/repo/fix-branch", "branch": "fix-branch", "head": "def", "main": false }
                        ]
                    })
                }
                "worktree.remove" => {
                    assert_eq!(request["params"]["path"], "/repo/fix-branch");
                    assert_eq!(request["params"]["force"], true);
                    serde_json::json!({ "type": "ok" })
                }
                other => panic!("unexpected method: {other}"),
            };
            let response = serde_json::json!({ "id": request["id"], "result": result });
            writeln!(writer, "{response}").expect("write response");
        }
    });

    let mut client = ControlClient::connect(&socket).expect("connect");

    let agents = client.agents().expect("agents");
    assert_eq!(agents.len(), 2);
    assert_eq!(agents[0].agent, "claude");
    assert_eq!(agents[0].tab, 1);
    assert_eq!(agents[1].tab, 2);
    assert_eq!(agents[0].label(), "auth-fix");
    assert!(agents[0].focused);
    // An unnamed agent falls back to its directory name.
    assert_eq!(agents[1].label(), "web");
    assert_eq!(agents[1].status, "blocked");

    let bar = client.bar_widgets().expect("bar");
    assert_eq!(bar.len(), 1);
    assert_eq!(bar[0].region, "bottom-right");
    assert_eq!(bar[0].content[0].text, "NORMAL");
    assert!(bar[0].content[1].separator);
    assert_eq!(bar[0].content[2].text, "tab 2/2");

    let tabs = client.tabs().expect("tabs");
    assert_eq!(tabs.len(), 2);
    assert_eq!(tabs[0].label(), "1");
    assert_eq!(tabs[1].label(), "docs");
    assert!(tabs[1].active);
    // session.snapshot enriched each tab's cwd and running process.
    assert_eq!(tabs[0].cwd, "/repo/api");
    assert_eq!(tabs[0].process, "fish");
    assert_eq!(tabs[1].cwd, "/repo/docs");
    assert_eq!(tabs[1].process, "devin");
    client.focus_tab(1).expect("focus tab");

    client.focus_pane("4").expect("focus pane");

    client
        .rename_workspace("ws1", "assets")
        .expect("rename workspace");
    client.close_workspace("ws2").expect("close workspace");
    client.close_pane("4").expect("close pane");
    client.name_agent("2", "auth-fix").expect("name agent");
    client.clear_agent_name("2").expect("clear agent name");
    client
        .create_worktree_for(3, "fix-branch")
        .expect("worktree for workspace");

    let worktrees = client.worktrees(0).expect("worktree list");
    assert_eq!(worktrees.len(), 2);
    assert!(worktrees[0].main);
    assert_eq!(worktrees[1].path, "/repo/fix-branch");
    assert!(!worktrees[1].main);
    client
        .remove_worktree("/repo/fix-branch", true)
        .expect("remove worktree");
    server.join().expect("server thread");
}
