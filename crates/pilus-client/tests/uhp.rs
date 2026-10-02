//! The UHP observe client against a scripted server socket.
//!
//! Mirrors the daemon's contract: one request per connection for ordinary
//! calls, and an observe connection that answers with a stream acknowledgment
//! followed by `terminal.frame` events.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::thread;

use pilus_client::{UhpClient, ansi};

const GENERATION: &str = "0123456789abcdef0123456789abcdef";
const TERMINAL: &str = "abcdefabcdefabcdefabcdefabcdefab";

#[test]
fn inventories_and_streams_a_frame() {
    let dir = std::env::temp_dir().join(format!("pilus-uhp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let socket = dir.join("luvus.sock");
    let _ = std::fs::remove_file(&socket);

    let listener = UnixListener::bind(&socket).expect("bind");
    let server = thread::spawn(move || {
        // Ordinary call: one request, one response, then the socket closes.
        let (stream, _) = listener.accept().expect("accept inventory");
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let mut writer = stream;
        let mut line = String::new();
        reader.read_line(&mut line).expect("read inventory");
        let request: serde_json::Value = serde_json::from_str(&line).expect("json");
        assert_eq!(request["method"], "terminal.backend.inventory");
        let response = serde_json::json!({
            "id": request["id"],
            "result": {
                "type": "terminal_backend_inventory",
                "server_generation": GENERATION,
                "truncated": false,
                "terminals": [{
                    "terminal_id": TERMINAL,
                    "pane_id": "5",
                    "terminal_title": "fx",
                    "cwd": "/repo",
                    "workspace": { "name": "work" }
                }]
            }
        });
        writeln!(writer, "{response}").expect("write inventory");

        // Observe call: an ack, then a frame, then the connection stays open.
        let (stream, _) = listener.accept().expect("accept observe");
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let mut writer = stream;
        let mut line = String::new();
        reader.read_line(&mut line).expect("read observe");
        let request: serde_json::Value = serde_json::from_str(&line).expect("json");
        assert_eq!(request["method"], "terminal.backend.observe");
        assert_eq!(request["params"]["pane_id"], "5");
        let ack = serde_json::json!({
            "id": request["id"],
            "result": {
                "type": "terminal_backend_stream",
                "mode": "observe",
                "server_generation": GENERATION,
                "terminal_id": TERMINAL,
                "pane_id": "5",
                "sequence": 1,
                "content_revision": 3,
                "ansi": true,
                "capture_mode": "visible",
                "lines": 2,
                "frame_bytes": 65536,
                "queue_capacity": 2,
                "loss_behavior": "resync_required_then_close"
            }
        });
        writeln!(writer, "{ack}").expect("write ack");
        let frame = serde_json::json!({
            "event": "terminal.frame",
            "sequence": 1,
            "data": {
                "server_generation": GENERATION,
                "terminal_id": TERMINAL,
                "pane_id": "5",
                "content_revision": 3,
                "mode": "visible",
                "ansi": true,
                "lines": 2,
                "bytes": 21,
                "truncated": false,
                "text": "\u{1b}[0;38;5;250mhi\u{1b}[0m\n\n"
            }
        });
        writeln!(writer, "{frame}").expect("write frame");
        // Hold the stream open so EOF is not what ends the client's read.
        thread::sleep(std::time::Duration::from_millis(200));
    });

    let mut client = UhpClient::connect(&socket);
    let (generation, terminals) = client.inventory().expect("inventory");
    assert_eq!(generation, GENERATION);
    assert_eq!(terminals.len(), 1);
    assert_eq!(terminals[0].pane_id, "5");
    assert_eq!(terminals[0].title, "fx");
    assert_eq!(terminals[0].workspace, "work");

    let mut observer = client
        .observe(&generation, &terminals[0], 2, true)
        .expect("observe");
    let frame = observer
        .next_frame()
        .expect("next frame")
        .expect("a frame arrived");
    assert_eq!(frame.revision, 3);
    assert_eq!(frame.lines, 2);
    assert!(!frame.truncated);

    let screen = ansi::parse(&frame.text, frame.lines);
    assert_eq!(screen.plain(), vec!["hi", ""]);
    assert_eq!(
        screen.rows[0][0].style.fg,
        ansi::Color::Indexed(250),
        "the SGR colour survives the round trip"
    );

    server.join().expect("server thread");
}

#[test]
fn subscribes_and_reads_sequenced_events() {
    let dir = std::env::temp_dir().join(format!("pilus-uhp-events-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let socket = dir.join("luvus.sock");
    let _ = std::fs::remove_file(&socket);

    let listener = UnixListener::bind(&socket).expect("bind");
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept subscribe");
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let mut writer = stream;
        let mut line = String::new();
        reader.read_line(&mut line).expect("read subscribe");
        let request: serde_json::Value = serde_json::from_str(&line).expect("json");
        assert_eq!(request["method"], "events.subscribe");
        assert_eq!(request["params"], serde_json::json!({}));
        let ack = serde_json::json!({
            "id": request["id"],
            "result": {
                "type": "subscription_started",
                "sequence": 10,
                "replayed": 0,
                "queue_capacity": 256,
                "loss_behavior": "resync_required_then_close"
            }
        });
        writeln!(writer, "{ack}").expect("write ack");
        for (sequence, name) in [(11, "tab.created"), (12, "terminal.output_ready")] {
            let event = serde_json::json!({
                "event": name,
                "sequence": sequence,
                "data": { "tab": sequence }
            });
            writeln!(writer, "{event}").expect("write event");
        }
        thread::sleep(std::time::Duration::from_millis(200));
    });

    let mut client = UhpClient::connect(&socket);
    let mut stream = client.subscribe().expect("subscribe");
    let first = stream.next_event().expect("first").expect("an event");
    assert_eq!(first.name, "tab.created");
    assert_eq!(first.sequence, 11);
    assert_eq!(first.data["tab"], 11);
    let second = stream.next_event().expect("second").expect("an event");
    assert_eq!(second.name, "terminal.output_ready");
    assert_eq!(second.sequence, 12);

    server.join().expect("server thread");
}
