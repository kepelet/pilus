//! End-to-end coverage for the handshake against a scripted server socket.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::os::unix::net::UnixListener;
use std::thread;
use std::time::Duration;

use pilus_client::{ClientEvent, ConnectTarget, Surface, SurfaceChange};
use pilus_protocol::{
    CellData, ClientMessage, FALLBACK_PROTOCOL_VERSION, FrameData, FrameHyperlink,
    PROTOCOL_VERSION, ServerMessage, read_message, write_message,
};

fn frame(width: u16, height: u16) -> FrameData {
    FrameData {
        width,
        height,
        cells: vec![
            CellData {
                symbol: "x".to_string(),
                fg: 0x00ff_ffff,
                bg: 0,
                mods: 0,
            };
            usize::from(width) * usize::from(height)
        ],
        // A non-empty span proves the protocol-20 field is on the wire, not
        // just a struct member that serializes as zero.
        hyperlinks: vec![FrameHyperlink {
            start: 0,
            end: 1,
            uri: "https://example.com".to_string(),
        }],
        cursor: Some((0, 0)),
        cursor_visible: true,
    }
}

#[test]
fn attaches_and_receives_a_frame() {
    let dir = std::env::temp_dir().join(format!("pilus-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let socket = dir.join("pilus-client.sock");
    let _ = std::fs::remove_file(&socket);

    let listener = UnixListener::bind(&socket).expect("bind");
    let server_socket = socket.clone();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        match read_message::<_, ClientMessage>(&mut stream).expect("hello") {
            ClientMessage::Hello { version, .. } => assert_eq!(version, PROTOCOL_VERSION),
            other => panic!("expected Hello, got {other:?}"),
        }
        write_message(
            &mut stream,
            &ServerMessage::Welcome {
                version: PROTOCOL_VERSION,
                error: None,
            },
        )
        .expect("welcome");
        write_message(
            &mut stream,
            &ServerMessage::Ready {
                probe_terminal: false,
            },
        )
        .expect("ready");
        write_message(
            &mut stream,
            &ServerMessage::EndpointIdentity {
                boot_id: 42,
                session: "default".to_string(),
            },
        )
        .expect("identity");
        write_message(&mut stream, &ServerMessage::Frame(frame(3, 1))).expect("frame");
        // Hold the link open until the client is done reading.
        thread::sleep(Duration::from_millis(250));
        let _ = std::fs::remove_file(&server_socket);
    });

    let target = ConnectTarget::socket(socket);
    let connection = pilus_client::connect(&target, 80, 24).expect("connect");
    assert_eq!(connection.identity().boot_id, 42);
    assert_eq!(connection.identity().session, "default");

    let mut surface = Surface::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut resized = false;
    while std::time::Instant::now() < deadline {
        match connection.recv_timeout(Duration::from_millis(200)) {
            Some(ClientEvent::Message(message)) => {
                if surface.apply(&message) == SurfaceChange::Resized {
                    resized = true;
                    break;
                }
            }
            Some(ClientEvent::Disconnected { reason }) => panic!("disconnected: {reason}"),
            _ => {}
        }
    }
    assert!(resized, "no frame arrived before the deadline");
    let frame = surface.frame().expect("frame");
    assert_eq!(frame.cells.len(), 3);
    assert!(frame.cursor_visible);
    assert_eq!(frame.hyperlinks.len(), 1);
    assert_eq!(frame.hyperlinks[0].uri, "https://example.com");

    server.join().expect("server thread");
}

#[test]
fn an_unsupported_protocol_names_both_versions() {
    let dir = std::env::temp_dir().join(format!("pilus-reject-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let socket = dir.join("pilus-reject.sock");
    let _ = std::fs::remove_file(&socket);

    let listener = UnixListener::bind(&socket).expect("bind");
    let server_socket = socket.clone();
    let server = thread::spawn(move || {
        // Both supported revisions are rejected, so negotiation exhausts and
        // the client reports the daemon's revision.
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().expect("accept");
            match read_message::<_, ClientMessage>(&mut stream).expect("hello") {
                ClientMessage::Hello { .. } => {}
                other => panic!("expected Hello, got {other:?}"),
            }
            write_message(
                &mut stream,
                &ServerMessage::Welcome {
                    version: PROTOCOL_VERSION + 1,
                    error: Some("protocol version mismatch".to_string()),
                },
            )
            .expect("welcome");
        }
        thread::sleep(Duration::from_millis(100));
        let _ = std::fs::remove_file(&server_socket);
    });

    let target = ConnectTarget::socket(socket);
    let error = match pilus_client::connect(&target, 80, 24) {
        Ok(_) => panic!("the handshake should have been rejected"),
        Err(error) => error,
    };
    match &error {
        pilus_client::Error::VersionMismatch { client, server } => {
            assert_eq!(*client, PROTOCOL_VERSION);
            assert_eq!(*server, PROTOCOL_VERSION + 1);
        }
        other => panic!("expected VersionMismatch, got {other:?}"),
    }
    let message = error.to_string();
    assert!(message.contains(&PROTOCOL_VERSION.to_string()), "{message}");
    assert!(
        message.contains(&(PROTOCOL_VERSION + 1).to_string()),
        "{message}"
    );

    server.join().expect("server thread");
}

#[test]
fn negotiates_down_to_the_fallback_protocol() {
    let dir = std::env::temp_dir().join(format!("pilus-negotiate-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let socket = dir.join("pilus-negotiate.sock");
    let _ = std::fs::remove_file(&socket);

    let listener = UnixListener::bind(&socket).expect("bind");
    let server_socket = socket.clone();
    let server = thread::spawn(move || {
        // The first attempt at the pinned revision is rejected, as a 0.14.2
        // daemon would.
        let (mut stream, _) = listener.accept().expect("first accept");
        match read_message::<_, ClientMessage>(&mut stream).expect("hello 20") {
            ClientMessage::Hello { version, .. } => assert_eq!(version, PROTOCOL_VERSION),
            other => panic!("expected Hello, got {other:?}"),
        }
        write_message(
            &mut stream,
            &ServerMessage::Welcome {
                version: FALLBACK_PROTOCOL_VERSION,
                error: Some("protocol version mismatch".to_string()),
            },
        )
        .expect("reject");
        drop(stream);

        // The retry at the fallback revision succeeds.
        let (mut stream, _) = listener.accept().expect("second accept");
        match read_message::<_, ClientMessage>(&mut stream).expect("hello 18") {
            ClientMessage::Hello { version, .. } => assert_eq!(version, FALLBACK_PROTOCOL_VERSION),
            other => panic!("expected Hello, got {other:?}"),
        }
        write_message(
            &mut stream,
            &ServerMessage::Welcome {
                version: FALLBACK_PROTOCOL_VERSION,
                error: None,
            },
        )
        .expect("welcome");
        write_message(
            &mut stream,
            &ServerMessage::Ready {
                probe_terminal: false,
            },
        )
        .expect("ready");
        write_message(
            &mut stream,
            &ServerMessage::EndpointIdentity {
                boot_id: 9,
                session: "default".to_string(),
            },
        )
        .expect("identity");
        thread::sleep(Duration::from_millis(200));
        let _ = std::fs::remove_file(&server_socket);
    });

    let target = ConnectTarget::socket(socket);
    let connection = pilus_client::connect(&target, 80, 24).expect("negotiated connect");
    assert_eq!(connection.identity().boot_id, 9);

    server.join().expect("server thread");
}
