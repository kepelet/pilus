//! The sipil client against scripted iroh peers: what it asks for, how it reads
//! what comes back, and two pilus nodes talking to each other. A last test
//! drives the real Node daemon when the `../sipil` checkout is there.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use pilus_client::sipil::node::Service;
use pilus_client::sipil::{
    Activity, CallError, Conversation, Event, Failure, ItemRole, Local, LocalOptions, Next, Node,
    NodeConfig, Quote, RelayChoice, Role, SharedDaemon, Sipil, SipilError, WatchEvent,
};
use serde_json::{Value, json};

fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "pilus-sipil-{name}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// A ticket for `addr` that dials loopback. A node's own ticket lists its LAN
/// addresses, which a test machine may not route to itself.
fn loopback(addr: &iroh::EndpointAddr) -> String {
    let port = addr.ip_addrs().next().expect("a bound address").port();
    let local =
        iroh::EndpointAddr::new(addr.id).with_ip_addr(format!("127.0.0.1:{port}").parse().unwrap());
    iroh_tickets::endpoint::EndpointTicket::new(local).to_string()
}

fn node(name: &str, service: Arc<dyn Service>) -> Arc<Node> {
    Node::start(
        &NodeConfig {
            dir: scratch(name),
            relay: RelayChoice::Disabled,
        },
        service,
    )
    .expect("start a node")
}

/// A sipil daemon that answers `health` as told and everything else from a
/// queue, remembering what it was asked.
#[derive(Debug)]
struct Script {
    health: Mutex<Value>,
    replies: Mutex<VecDeque<Result<Value, Failure>>>,
    seen: Mutex<Vec<(String, Value)>>,
}

impl Script {
    fn daemon(replies: Vec<Result<Value, Failure>>) -> Arc<Self> {
        Arc::new(Self {
            health: Mutex::new(
                json!({"name": "sipil", "kind": "daemon", "version": "0.1.0", "api": 3, "allowed": true}),
            ),
            replies: Mutex::new(replies.into()),
            seen: Mutex::new(Vec::new()),
        })
    }

    fn with_health(self: &Arc<Self>, health: Value) -> Arc<Self> {
        *self.health.lock().unwrap() = health;
        Arc::clone(self)
    }

    fn seen(&self) -> Vec<(String, Value)> {
        self.seen.lock().unwrap().clone()
    }
}

impl Service for Script {
    fn handle(&self, _from: &str, method: &str, params: Value) -> Result<Value, Failure> {
        if method == "health" {
            return Ok(self.health.lock().unwrap().clone());
        }
        self.seen.lock().unwrap().push((method.to_string(), params));
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err(Failure::new("script_ended", "No reply was scripted.")))
    }
}

/// Nobody calls the client node; it only dials.
#[derive(Debug)]
struct Deaf;

impl Service for Deaf {
    fn handle(&self, _: &str, _: &str, _: Value) -> Result<Value, Failure> {
        Err(Failure::new("deaf", "not listening"))
    }
}

/// A client for a scripted daemon.
fn daemon(script: &Arc<Script>) -> (Sipil, Arc<Node>) {
    let server = node("daemon", Arc::clone(script) as Arc<dyn Service>);
    let client = node("client", Arc::new(Deaf));
    let sipil = Sipil::with_node(client, &loopback(&server.addr())).expect("a ticket is a target");
    (sipil, server)
}

fn ok(value: Value) -> Result<Value, Failure> {
    Ok(value)
}

#[test]
fn connect_accepts_a_daemon_that_allows_this_device() {
    let script = Script::daemon(vec![]);
    let (sipil, _server) = daemon(&script);
    let health = sipil.connect().unwrap();
    assert_eq!(health.version, "0.1.0");
}

#[test]
fn a_device_the_daemon_has_not_allowed_is_told_the_command_to_run() {
    let script = Script::daemon(vec![]);
    script.with_health(
        json!({"name": "sipil", "kind": "daemon", "version": "0.1.0", "api": 3, "allowed": false}),
    );
    let (sipil, _server) = daemon(&script);
    let error = sipil.connect().unwrap_err();
    assert!(error.is_unauthorized(), "{error:?}");
    assert!(
        error
            .to_string()
            .contains("npm run connect -- --device-id "),
        "{error}"
    );
}

#[test]
fn connect_refuses_a_pilus_where_a_daemon_was_wanted() {
    let script = Script::daemon(vec![]);
    script.with_health(
        json!({"name": "sipil", "kind": "pilus", "version": "0.3.0", "api": 3, "allowed": true}),
    );
    let (sipil, _server) = daemon(&script);
    assert!(matches!(sipil.connect(), Err(SipilError::WrongKind(_))));
}

#[test]
fn connect_refuses_another_program_and_another_api_revision() {
    let script = Script::daemon(vec![]);
    script.with_health(json!({"name": "nginx", "version": "1", "api": 3}));
    let (sipil, _server) = daemon(&script);
    assert!(matches!(sipil.connect(), Err(SipilError::NotSipil { .. })));

    script.with_health(json!({"name": "sipil", "version": "9", "api": 4}));
    assert_eq!(
        sipil.connect().unwrap_err(),
        SipilError::Version { found: 4 }
    );
}

#[test]
fn a_device_nobody_can_find_is_unreachable() {
    let client = node("lost", Arc::new(Deaf));
    let nobody = iroh::SecretKey::generate().public().to_string();
    let sipil = Sipil::with_node(client, &nobody).unwrap();
    assert!(matches!(
        sipil.connect(),
        Err(SipilError::Unreachable { .. })
    ));
}

#[test]
fn agents_and_sessions_are_read_as_listed() {
    let script = Script::daemon(vec![
        ok(json!({"agents": [
            {"id":"1","name":"sipil","role":"primary","status":"working","model":{"provider":"opencode-go","modelId":"kimi-k2"}},
            {"id":"9","name":"subagent Count the files","role":"subagent","status":"idle","parentId":"1"}
        ]})),
        ok(json!({"sessions": [
            {"id":"7","title":"Say hello","status":"idle","error":"401 Unauthorized: invalid API key"}
        ]})),
    ]);
    let (sipil, _server) = daemon(&script);
    let agents = sipil.agents().unwrap();
    assert_eq!(agents[0].role, Role::Primary);
    assert_eq!(agents[0].status, Activity::Working);
    assert_eq!(agents[0].model.as_ref().unwrap().model_id, "kimi-k2");
    assert_eq!(agents[1].parent_id.as_deref(), Some("1"));
    // `error` is only sent for a conversation whose latest turn failed.
    assert_eq!(agents[0].error, None);
    let sessions = sipil.sessions().unwrap();
    assert_eq!(
        (sessions[0].id.as_str(), sessions[0].title.as_str()),
        ("7", "Say hello")
    );
    assert_eq!(
        sessions[0].error.as_deref(),
        Some("401 Unauthorized: invalid API key")
    );
    let methods: Vec<_> = script
        .seen()
        .into_iter()
        .map(|(method, _)| method)
        .collect();
    assert_eq!(methods, ["agents", "sessions"]);
}

#[test]
fn a_session_can_be_started_and_spoken_to() {
    let script = Script::daemon(vec![
        ok(json!({"id":"7","title":"New session","status":"idle"})),
        ok(json!({"submissionId":"3"})),
    ]);
    let (sipil, _server) = daemon(&script);
    let session = sipil.create_session().unwrap();
    assert_eq!(session.id, "7");
    sipil
        .send_message(&session.id, "List the files", "req-1")
        .unwrap();
    assert_eq!(
        script.seen(),
        [
            ("sessions.create".to_string(), json!({})),
            (
                "message".to_string(),
                json!({"id": "7", "content": "List the files", "requestId": "req-1"})
            )
        ]
    );
}

#[test]
fn a_message_sipil_will_not_take_reports_why() {
    let script = Script::daemon(vec![Err(Failure::new(
        "no_provider",
        "Set OPENCODE_API_KEY in the environment sipil starts in.",
    ))]);
    let (sipil, _server) = daemon(&script);
    let error = sipil.send_message("1", "hi", "r").unwrap_err();
    assert_eq!(
        error.to_string(),
        "Set OPENCODE_API_KEY in the environment sipil starts in."
    );
}

#[test]
fn a_transcript_is_read_with_roles_tools_and_failures() {
    let script = Script::daemon(vec![ok(json!({"items": [
        {"id":13,"role":"user","text":"hello","tools":[]},
        {"id":14,"role":"assistant","text":"","tools":["bash"]},
        {"id":15,"role":"tool","text":"ok","tools":[]},
        {"id":17,"role":"assistant","text":"401 Unauthorized","tools":[],"error":"401 Unauthorized"}
    ]}))]);
    let (sipil, _server) = daemon(&script);
    let items = sipil.transcript("7").unwrap();
    assert_eq!(
        items.iter().map(|item| item.role).collect::<Vec<_>>(),
        [
            ItemRole::User,
            ItemRole::Assistant,
            ItemRole::Tool,
            ItemRole::Assistant
        ]
    );
    assert_eq!(items[1].tools, ["bash"]);
    assert_eq!(items[3].error.as_deref(), Some("401 Unauthorized"));
    assert_eq!(items[0].error, None);
    assert_eq!(
        script.seen(),
        [("transcript".to_string(), json!({"id": "7"}))]
    );
}

#[test]
fn a_conversation_id_cannot_smuggle_anything_into_a_call() {
    let script = Script::daemon(vec![]);
    let (sipil, _server) = daemon(&script);
    assert!(sipil.transcript("1/../providers").is_err());
    assert!(sipil.send_message("../x", "hi", "r").is_err());
    assert!(script.seen().is_empty());
}

#[test]
fn stopping_a_conversation_sends_an_abort() {
    let script = Script::daemon(vec![ok(json!({}))]);
    let (sipil, _server) = daemon(&script);
    sipil.abort("1").unwrap();
    assert_eq!(script.seen(), [("abort".to_string(), json!({"id": "1"}))]);
}

#[test]
fn a_session_reports_the_model_it_runs() {
    let script = Script::daemon(vec![ok(json!({"sessions": [
        {"id":"7","title":"t","status":"idle","model":{"provider":"opencode-go","modelId":"qwen3.8-max"}}
    ]}))]);
    let (sipil, _server) = daemon(&script);
    assert_eq!(
        sipil.sessions().unwrap()[0]
            .model
            .as_ref()
            .unwrap()
            .model_id,
        "qwen3.8-max"
    );
}

// ---- two pilus windows ----

type Heard = Arc<Mutex<Vec<Event>>>;

fn window(name: &str) -> (Arc<Local>, Heard) {
    let heard: Heard = Arc::default();
    let sink = Arc::clone(&heard);
    let dir = scratch(name);
    let db = pilus_db::Db::at(&dir);
    let local = Local::build(LocalOptions {
        directory: Arc::new(pilus_client::sipil::Directory::open(&db, &dir)),
        chats: Arc::new(pilus_client::sipil::Chats::open(&db, &dir)),
        channels: Arc::new(pilus_client::sipil::Channels::open(&db, &dir)),
        dir,
        relay: RelayChoice::Disabled,
        nickname: name.to_string(),
        reachable: true,
        notify: Arc::new(move |event| sink.lock().unwrap().push(event)),
    })
    .expect("a window");
    (local, heard)
}

#[test]
fn a_pilus_hears_only_the_people_it_added_and_remembers_who_knocked() {
    let (alice, _alice_heard) = window("alice");
    let (bob, bob_heard) = window("bob");
    let bob_as_contact = {
        alice
            .directory()
            .add(bob.id(), "Bob", Some(loopback(&bob.node().addr())))
            .unwrap();
        alice.directory().contact(bob.id()).unwrap()
    };

    // Bob has not added Alice, so her message is refused, and Bob is told she knocked.
    let error = alice
        .send(&bob_as_contact, "hello Bob", "r1", None)
        .unwrap_err();
    assert!(error.is_unauthorized(), "{error:?}");
    assert!(
        alice.chats().lines(bob.id()).is_empty(),
        "nothing was delivered"
    );
    assert!(
        bob.chats().lines(alice.id()).is_empty(),
        "a stranger's text is not kept"
    );
    assert_eq!(bob.directory().requests()[0].id, alice.id());
    assert_eq!(
        *bob_heard.lock().unwrap(),
        [Event::Request {
            from: alice.id().to_string()
        }]
    );

    // The health check says so without sending anything.
    let health = alice.check(&bob_as_contact).unwrap();
    assert!(!health.allowed);
    assert_eq!(health.kind, pilus_client::sipil::NodeKind::Pilus);

    // Bob accepts; the same words now go through, once, and are read as Alice's.
    bob.directory().add(alice.id(), "Alice", None).unwrap();
    assert!(bob.directory().requests().is_empty());
    alice
        .send(&bob_as_contact, "hello Bob", "r2", None)
        .unwrap();
    let theirs = bob.chats().lines(alice.id());
    assert_eq!(theirs.len(), 1);
    assert!(!theirs[0].mine);
    assert_eq!(theirs[0].text, "hello Bob");
    assert_eq!(bob.chats().unread(alice.id()), 1);
    let mine = alice.chats().lines(bob.id());
    assert!(mine[0].mine && mine[0].text == "hello Bob");
    assert!(bob_heard.lock().unwrap().contains(&Event::Message {
        thread: alice.id().to_string(),
        from: alice.id().to_string(),
        name: "Alice".to_string(),
        channel: None,
        text: "hello Bob".to_string(),
    }));

    // Alice added Bob too, so he can answer; and once she removes him he cannot.
    let alice_as_contact = pilus_client::sipil::Contact {
        ticket: Some(loopback(&alice.node().addr())),
        ..bob.directory().contact(alice.id()).unwrap()
    };
    bob.send(&alice_as_contact, "hi Alice", "r3", None).unwrap();
    assert_eq!(
        alice.chats().lines(bob.id()).last().unwrap().text,
        "hi Alice"
    );
    alice.directory().remove(bob.id()).unwrap();
    let error = bob
        .send(&alice_as_contact, "still there?", "r4", None)
        .unwrap_err();
    assert!(error.is_unauthorized(), "{error:?}");
}

#[test]
fn a_pilus_is_not_a_daemon() {
    let (alice, _) = window("alice-nodaemon");
    let (bob, _) = window("bob-nodaemon");
    let sipil = Sipil::with_node(bob.node(), &loopback(&alice.node().addr())).unwrap();
    assert!(matches!(sipil.connect(), Err(SipilError::WrongKind(_))));
    let error = sipil.agents().unwrap_err();
    assert_eq!(
        error.to_string(),
        "This is a pilus, not a sipil agent: it only takes chat messages."
    );
}

#[test]
fn a_retried_delivery_lands_once() {
    let (alice, _) = window("alice-retry");
    let (bob, _) = window("bob-retry");
    alice
        .directory()
        .add(bob.id(), "Bob", Some(loopback(&bob.node().addr())))
        .unwrap();
    bob.directory().add(alice.id(), "Alice", None).unwrap();
    let contact = alice.directory().contact(bob.id()).unwrap();
    alice.send(&contact, "once", "same", None).unwrap();
    alice.send(&contact, "once", "same", None).unwrap();
    assert_eq!(bob.chats().lines(alice.id()).len(), 1);
}

// ---- the real daemon ----

/// The sipil checkout beside this repository, if there is one with its
/// dependencies installed and a Node to run it.
fn sipil_checkout() -> Option<PathBuf> {
    let dir = std::env::var_os("SIPIL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../sipil"));
    let usable = dir.join("node_modules/@number0/iroh").exists()
        && std::process::Command::new("node")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success());
    usable.then_some(dir)
}

/// Start the real daemon on `home` and wait for it to print its ticket.
fn start_daemon(checkout: &std::path::Path, home: &std::path::Path) -> (Reaper, String) {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};

    let mut daemon = Command::new("node")
        .arg("src/main.ts")
        .current_dir(checkout)
        .env("SIPIL_HOME", home)
        .env("SIPIL_CWD", home)
        .env("SIPIL_RELAY", "off")
        // No key: the daemon takes it from here, and a developer's own would let a message through.
        .env_remove("OPENCODE_API_KEY")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("start sipil");
    let mut lines = BufReader::new(daemon.stdout.take().unwrap()).lines();
    let reaper = Reaper(daemon);
    let ticket = loop {
        match lines.next() {
            Some(Ok(line)) => {
                if let Some(ticket) = line.strip_prefix("ticket:") {
                    break ticket.trim().to_string();
                }
            }
            _ => panic!("sipil exited before printing its ticket"),
        }
    };
    // The daemon prints more after the ticket and again when it refuses a
    // device; a closed pipe would kill it mid-test, so it is read to the end.
    std::thread::spawn(move || lines.for_each(drop));
    (reaper, ticket)
}

#[test]
fn the_real_daemon_asks_a_stranger_to_be_allowed_and_then_serves_it() {
    use std::process::Command;

    let Some(checkout) = sipil_checkout() else {
        eprintln!("skipped: no sipil checkout with node_modules beside this repository");
        return;
    };
    let home = scratch("daemon-home");
    let (_reaper, ticket) = start_daemon(&checkout, &home);
    let client = node("real-client", Arc::new(Deaf));
    let printed: iroh_tickets::endpoint::EndpointTicket =
        ticket.parse().expect("sipil's ticket parses");
    let sipil = Sipil::with_node(Arc::clone(&client), &loopback(printed.endpoint_addr()))
        .expect("a ticket is a target");
    {
        // Not allowed yet: `health` still answers, and says so.
        let error = sipil.connect().unwrap_err();
        assert!(error.is_unauthorized(), "{error:?}");
        assert!(error.to_string().contains(client.id()), "{error}");
        let again = sipil.agents().unwrap_err();
        assert!(again.is_unauthorized(), "{again:?}");

        let allow = Command::new("node")
            .args(["src/main.ts", "allow", client.id(), "test"])
            .current_dir(&checkout)
            .env("SIPIL_HOME", &home)
            .output()
            .expect("run allow");
        assert!(allow.status.success(), "{allow:?}");

        let health = sipil.connect().unwrap();
        assert_eq!(health.name, "sipil");
        let agents = sipil.agents().unwrap();
        assert_eq!(
            (agents[0].role, agents[0].name.as_str()),
            (Role::Primary, "sipil")
        );
        // What the daemon says about itself: who, where, its agents with what
        // they are given, and a logo it drew from its own device ID.
        let about = sipil.about().unwrap();
        assert_eq!(about.name, "sipil");
        assert_eq!(about.device_id, printed.endpoint_addr().id.to_string());
        assert!(!about.hostname.is_empty() && !about.workspace.is_empty());
        assert_eq!(
            about
                .agents
                .iter()
                .map(|agent| (agent.id.as_str(), agent.role))
                .collect::<Vec<_>>(),
            [("sipil", Role::Primary), ("researcher", Role::Subagent)]
        );
        assert!(
            about
                .agents
                .iter()
                .all(|agent| !agent.instructions.is_empty())
        );
        assert!(about.agents[1].tools.iter().any(|tool| tool == "web_fetch"));
        assert!(
            about
                .logo
                .as_deref()
                .is_some_and(|svg| svg.starts_with("<svg"))
        );
        let session = sipil.create_session().unwrap();
        assert!(sipil.sessions().unwrap().iter().any(|s| s.id == session.id));
        let refused = sipil.send_message(&session.id, "hi", "r1").unwrap_err();
        assert!(
            matches!(refused, SipilError::Refused { ref code, .. } if code == "no_provider"),
            "{refused:?}"
        );

        // Paging: a page of an empty conversation has nothing before it, and a cursor
        // that is not a line is refused rather than guessed at.
        let page = sipil.transcript_page(&session.id, Some(5)).unwrap();
        assert!(page.items.is_empty() && !page.more);
        let bad = sipil.transcript_page(&session.id, Some(0)).unwrap_err();
        assert!(
            matches!(bad, SipilError::Refused { ref code, .. } if code == "bad_cursor"),
            "{bad:?}"
        );

        // Watching: the first event is a snapshot, and a pause is a pause, not an end.
        let wait = std::time::Duration::from_millis(300);
        let mut watching = sipil.watch(&session.id).unwrap();
        let Next::Event(first) = watching.next(std::time::Duration::from_secs(10)).unwrap() else {
            panic!("expected an event");
        };
        match *first {
            WatchEvent::Snapshot {
                items,
                more,
                conversation,
                ..
            } => {
                assert!(items.is_empty() && !more);
                assert_eq!(conversation.unwrap().id, session.id);
            }
            other => panic!("expected a snapshot, got {other:?}"),
        }
        assert_eq!(watching.next(wait).unwrap(), Next::Idle);
        // A conversation that is not there is the daemon's refusal, in the stream.
        let mut missing = sipil.watch("99999").unwrap();
        let error = missing
            .next(std::time::Duration::from_secs(10))
            .unwrap_err();
        assert!(
            matches!(error, SipilError::Refused { ref code, .. } if code == "unknown_conversation"),
            "{error:?}"
        );
        assert_eq!(missing.next(wait).unwrap(), Next::Ended);
        drop(watching);
        // Hanging up one watch leaves the connection usable.
        assert!(sipil.sessions().unwrap().iter().any(|s| s.id == session.id));
    }
}

#[test]
fn a_daemon_that_asks_to_connect_is_allowed_only_after_the_pilus_accepts() {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let Some(checkout) = sipil_checkout() else {
        eprintln!("skipped: no sipil checkout with node_modules beside this repository");
        return;
    };
    let home = scratch("pair-home");
    let (_reaper, daemon_ticket) = start_daemon(&checkout, &home);
    let (pilus, heard) = window("pair-pilus");
    let pilus_ticket = loopback(&pilus.node().addr());

    let mut connect = Reaper(
        Command::new("node")
            .args(["src/main.ts", "connect", "--ticket", &pilus_ticket])
            .current_dir(&checkout)
            .env("SIPIL_HOME", &home)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("run connect"),
    );

    let until = |what: &str, done: &mut dyn FnMut() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !done() {
            assert!(Instant::now() < deadline, "timed out: {what}");
            std::thread::sleep(Duration::from_millis(100));
        }
    };

    // The daemon knocks. Nothing is allowed on the strength of that alone.
    until("the request arrives", &mut || {
        pilus.directory().requests().iter().any(|r| r.daemon)
    });
    let request = pilus.directory().requests().remove(0);
    assert_eq!(request.label.as_deref(), Some("sipil"));
    assert!(
        request.ticket.is_some(),
        "it sends a ticket to be dialed back on"
    );
    assert!(
        heard
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, Event::Request { .. })),
        "the window is told"
    );
    let printed: iroh_tickets::endpoint::EndpointTicket =
        daemon_ticket.parse().expect("sipil's ticket parses");
    let daemon = Sipil::with_node(pilus.node(), &loopback(printed.endpoint_addr())).unwrap();
    assert!(daemon.connect().unwrap_err().is_unauthorized());
    assert!(!pilus.directory().is_contact(&request.id));

    // The person accepts: the daemon becomes a contact, and allows this pilus.
    assert_eq!(pilus.directory().accept(&request.id).unwrap(), Some(true));
    assert!(pilus.directory().is_daemon(&request.id));
    until("the command finishes", &mut || {
        connect.0.try_wait().unwrap().is_some()
    });
    assert!(connect.0.wait().unwrap().success());
    daemon.connect().expect("allowed now");
    assert_eq!(daemon.agents().unwrap()[0].name, "sipil");
}

/// Kills the daemon when the test ends, passed or not.
struct Reaper(std::process::Child);

impl Drop for Reaper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn a_pilus_that_is_not_reachable_answers_nobody() {
    let (alice, _) = window("alice-hidden");
    let (bob, _) = window("bob-hidden");
    alice
        .directory()
        .add(bob.id(), "Bob", Some(loopback(&bob.node().addr())))
        .unwrap();
    bob.directory().add(alice.id(), "Alice", None).unwrap();
    let contact = alice.directory().contact(bob.id()).unwrap();
    bob.set_reachable(false);
    assert!(matches!(
        alice.check(&contact),
        Err(SipilError::Unreachable { .. })
    ));
    assert!(alice.send(&contact, "hello?", "r1", None).is_err());
    assert!(bob.chats().lines(alice.id()).is_empty());
    bob.set_reachable(true);
    alice.send(&contact, "hello?", "r2", None).unwrap();
    assert_eq!(bob.chats().lines(alice.id()).len(), 1);
}

#[test]
fn a_call_from_a_contact_is_noted_so_the_window_need_not_keep_asking_whether_they_are_there() {
    let (alice, _) = window("alice-called");
    let (bob, _) = window("bob-called");
    let (carol, _) = window("carol-called");
    for caller in [&alice, &carol] {
        caller
            .directory()
            .add(bob.id(), "Bob", Some(loopback(&bob.node().addr())))
            .unwrap();
    }
    // Bob has Alice as a contact; Carol is a stranger to him.
    bob.directory().add(alice.id(), "Alice", None).unwrap();
    let woken = Arc::new(AtomicU32::new(0));
    let count = Arc::clone(&woken);
    bob.on_heard(move || {
        count.fetch_add(1, Ordering::Relaxed);
    });
    let ask = |caller: &Arc<Local>| caller.check(&caller.directory().contact(bob.id()).unwrap());

    ask(&carol).unwrap();
    assert!(
        bob.take_heard().is_empty(),
        "a stranger's call proves nothing"
    );
    assert_eq!(woken.load(Ordering::Relaxed), 0);

    ask(&alice).unwrap();
    ask(&alice).unwrap();
    assert_eq!(woken.load(Ordering::Relaxed), 1, "told once until taken");
    assert_eq!(bob.take_heard(), [alice.id().to_string()]);
    assert!(bob.take_heard().is_empty());

    ask(&alice).unwrap();
    assert_eq!(
        woken.load(Ordering::Relaxed),
        2,
        "and again once it was taken"
    );
    bob.take_heard();

    // A pilus that is not taking calls still learns who is up.
    bob.set_reachable(false);
    assert!(ask(&alice).is_err());
    assert_eq!(bob.take_heard(), [alice.id().to_string()]);
}

// ---- agents are not reachable on their own ----

#[test]
fn a_pilus_hands_no_session_of_its_agents_to_a_contact() {
    let (alice, _) = window("alice-relay");
    let (bob, _) = window("bob-relay");
    alice
        .directory()
        .add(bob.id(), "Bob", Some(loopback(&bob.node().addr())))
        .unwrap();
    bob.directory().add(alice.id(), "Alice", None).unwrap();

    let research = Script::daemon(vec![]);
    let (research_client, _r) = daemon(&research);
    let research_id = research_client.target().id();
    bob.set_daemons(vec![SharedDaemon {
        id: research_id.clone(),
        name: "Researcher".to_string(),
        sipil: Sipil::with_node(bob.node(), research_client.target().text()).unwrap(),
    }]);

    // Alice is a contact of Bob's, and the daemon is connected, yet none of the
    // calls that once relayed an agent is answered, and `health` lists no agent.
    let bobs = alice.directory().contact(bob.id()).unwrap();
    assert!(alice.check(&bobs).unwrap().allowed);
    let target = pilus_client::sipil::parse_target(&loopback(&bob.node().addr())).unwrap();
    for method in ["agent.message", "agent.transcript", "agent.abort"] {
        let error = alice
            .node()
            .call(
                target.addr(),
                method,
                &json!({"agent": research_id, "requestId": "r1", "content": "hi"}),
            )
            .unwrap_err();
        assert!(
            matches!(error, CallError::Refused(ref failure) if failure.code == "unknown_method"),
            "{method}: {error:?}"
        );
    }
    assert!(research.seen().is_empty(), "the daemon was never asked");
}

// ---- channels ----

/// Three windows that have all added each other.
fn trio(name: &str) -> (Arc<Local>, Arc<Local>, Arc<Local>, Heard, Heard) {
    let (a, _) = window(&format!("{name}-a"));
    let (b, heard_b) = window(&format!("{name}-b"));
    let (c, heard_c) = window(&format!("{name}-c"));
    for (me, them, label) in [
        (&a, &b, "B"),
        (&a, &c, "C"),
        (&b, &a, "A"),
        (&b, &c, "C"),
        (&c, &a, "A"),
        (&c, &b, "B"),
    ] {
        me.directory()
            .add(them.id(), label, Some(loopback(&them.node().addr())))
            .unwrap();
    }
    (a, b, c, heard_b, heard_c)
}

#[test]
fn a_channel_reaches_every_member_who_learns_it_from_the_first_message() {
    let (a, b, c, heard_b, _heard_c) = trio("chan");
    let channel = a
        .channels()
        .create("Project X", vec![b.id().to_string(), c.id().to_string()])
        .unwrap();
    assert_eq!(channel.name, "project-x");

    let delivery = a
        .send_channel(&channel, "kickoff at ten", "r1", None)
        .unwrap();
    assert_eq!(delivery.delivered.len(), 2);
    assert!(delivery.failed.is_empty());

    // B and C had never heard of it: they now have the channel, its name and
    // the other member, and the line says who said it.
    for (me, other) in [(&b, &c), (&c, &b)] {
        let learned = me.channels().get(&channel.id).expect("the channel arrived");
        assert_eq!(learned.name, "project-x");
        assert_eq!(learned.members.len(), 2, "the author and the other member");
        assert!(learned.members.contains(&a.id().to_string()));
        assert!(learned.members.contains(&other.id().to_string()));
        assert!(
            !learned.members.contains(&me.id().to_string()),
            "never itself"
        );
        let lines = me.chats().lines(&channel.thread());
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].who.as_deref(), Some(a.id()));
        assert!(!lines[0].mine);
    }
    assert!(a.chats().lines(&channel.thread())[0].mine);
    assert!(matches!(
        &heard_b.lock().unwrap()[0],
        Event::Message { channel: Some(name), name: speaker, .. } if name == "project-x" && speaker == "A"
    ));

    // B answers in the channel; A and C both hear it.
    let channel_b = b.channels().get(&channel.id).unwrap();
    b.send_channel(&channel_b, "on my way", "r2", None).unwrap();
    assert_eq!(a.chats().lines(&channel.thread()).len(), 2);
    assert_eq!(c.chats().lines(&channel.thread()).len(), 2);

    // C leaves; a later message does not bring the channel back.
    c.channels().remove(&channel.id).unwrap();
    c.chats().forget(&channel.thread());
    a.send_channel(&channel, "still there?", "r3", None)
        .unwrap();
    assert!(c.channels().get(&channel.id).is_none());
    assert!(c.chats().lines(&channel.thread()).is_empty());
}

#[test]
fn the_owner_can_invite_to_a_channel_and_remove_from_it_and_the_removed_stay_out() {
    let (a, b, c, _heard_b, _heard_c) = trio("members");
    let (a_id, b_id, c_id) = (a.id().to_string(), b.id().to_string(), c.id().to_string());
    let channel = a.channels().create("Ops", vec![b_id.clone()]).unwrap();
    assert_eq!(channel.owner, a_id, "whoever made it owns it");
    a.send_channel(&channel, "hello", "r1", None).unwrap();
    assert_eq!(b.channels().get(&channel.id).unwrap().owner, a_id);
    assert!(c.channels().get(&channel.id).is_none(), "C was not in it");

    // A invites C: C learns the channel from the line, B learns C from it.
    let c_contact = a.directory().contact(&c_id).unwrap();
    let delivery = a.invite_to_channel(&channel.id, &c_contact, "r2").unwrap();
    assert_eq!(delivery.delivered.len(), 2);
    let learned = c.channels().get(&channel.id).expect("C was invited");
    assert_eq!(learned.owner, a_id);
    assert!(learned.members.contains(&a_id) && learned.members.contains(&b_id));
    assert!(
        b.channels()
            .get(&channel.id)
            .unwrap()
            .members
            .contains(&c_id)
    );
    assert_eq!(
        a.chats().lines(&channel.thread()).last().unwrap().text,
        "invited C"
    );

    // B is not the owner: it can do neither, and nothing changed.
    let refused = b.remove_from_channel(&channel.id, &c_id, "r3").unwrap_err();
    assert!(matches!(refused, SipilError::Refused { ref code, .. } if code == "not_owner"));
    let refused = b
        .invite_to_channel(&channel.id, &b.directory().contact(&a_id).unwrap(), "r3")
        .unwrap_err();
    assert!(matches!(refused, SipilError::Refused { ref code, .. } if code == "not_owner"));
    assert!(
        b.channels()
            .get(&channel.id)
            .unwrap()
            .members
            .contains(&c_id)
    );

    // A removes C: C is told and leaves, B drops C, and C no longer hears A.
    let delivery = a.remove_from_channel(&channel.id, &c_id, "r4").unwrap();
    assert_eq!(
        delivery.delivered.len(),
        2,
        "the one removed is told as well"
    );
    assert!(c.channels().get(&channel.id).is_none(), "C was taken out");
    assert!(
        !b.channels()
            .get(&channel.id)
            .unwrap()
            .members
            .contains(&c_id)
    );
    assert!(
        !a.channels()
            .get(&channel.id)
            .unwrap()
            .members
            .contains(&c_id)
    );
    let lines_c = c.chats().lines(&channel.thread()).len();
    let channel_a = a.channels().get(&channel.id).unwrap();
    a.send_channel(&channel_a, "just us", "r5", None).unwrap();
    assert_eq!(c.chats().lines(&channel.thread()).len(), lines_c);
    assert!(c.channels().get(&channel.id).is_none(), "and it stays out");

    // A list that still names C does not bring C back, and C can be invited again.
    a.channels().learn(
        &channel.id,
        "ops",
        std::slice::from_ref(&c_id),
        &a_id,
        &a_id,
        &a_id,
    );
    assert!(
        !a.channels()
            .get(&channel.id)
            .unwrap()
            .members
            .contains(&c_id)
    );
    a.invite_to_channel(&channel.id, &c_contact, "r6").unwrap();
    assert!(c.channels().get(&channel.id).is_some(), "invited back");
}

#[test]
fn a_member_who_is_not_the_owner_cannot_change_the_list_or_the_owner() {
    let (a, b, c, _heard_b, _heard_c) = trio("not-owner");
    let (a_id, b_id, c_id) = (a.id().to_string(), b.id().to_string(), c.id().to_string());
    let channel = a
        .channels()
        .create("Ops", vec![b_id.clone(), c_id.clone()])
        .unwrap();
    a.send_channel(&channel, "hello", "r1", None).unwrap();

    // B writes with a list that adds a stranger, and claims to own the channel.
    let stranger = "d".repeat(64);
    let mut forged = b.channels().get(&channel.id).unwrap();
    forged.members.push(stranger.clone());
    forged.owner = b_id.clone();
    b.send_channel(&forged, "I am in charge", "r2", None)
        .unwrap();
    for me in [&a, &c] {
        let seen = me.channels().get(&channel.id).unwrap();
        assert!(
            !seen.members.contains(&stranger),
            "only the owner's list counts"
        );
        assert_eq!(seen.owner, a_id);
    }
}

#[test]
fn only_members_can_speak_in_a_channel_and_a_pilus_that_was_missed_is_told() {
    let (a, b, c, _heard_b, _heard_c) = trio("speak");
    let c_id = c.id().to_string();
    let channel = a
        .channels()
        .create("Ops", vec![b.id().to_string(), c_id.clone()])
        .unwrap();
    a.send_channel(&channel, "hello", "r1", None).unwrap();
    let before = a.chats().lines(&channel.thread()).len();

    // C is taken out on both other pilus windows, but never hears of it.
    for me in [&a, &b] {
        me.channels()
            .remove_members(&channel.id, std::slice::from_ref(&c_id));
    }
    let channel_c = c.channels().get(&channel.id).expect("C still has it");
    let error = c
        .send_channel(&channel_c, "still here?", "r2", None)
        .unwrap_err();
    assert!(
        matches!(&error, SipilError::Refused { code, .. } if code == "not_member"),
        "{error:?}"
    );
    assert_eq!(
        a.chats().lines(&channel.thread()).len(),
        before,
        "nothing was heard"
    );
    assert!(
        c.channels().get(&channel.id).is_none(),
        "C learns it is out and forgets the channel"
    );
}

#[test]
fn a_channel_message_from_someone_who_is_not_a_contact_is_refused() {
    let (a, b, _c, _hb, _hc) = trio("chan-stranger");
    let (stranger, _) = window("chan-stranger-s");
    let channel = stranger
        .channels()
        .create("spam", vec![b.id().to_string()])
        .unwrap();
    stranger
        .directory()
        .add(b.id(), "B", Some(loopback(&b.node().addr())))
        .unwrap();
    let error = stranger
        .send_channel(&channel, "buy now", "r1", None)
        .unwrap_err();
    assert!(error.is_unauthorized(), "{error:?}");
    assert!(
        b.channels().list().is_empty(),
        "no channel was made for a stranger"
    );
    assert_eq!(b.directory().requests().len(), 1);
    let _ = a;
}

#[test]
fn a_channel_with_no_one_to_reach_is_a_notebook_and_a_member_who_is_down_is_named() {
    let (a, b, _c, _hb, _hc) = trio("chan-partial");
    let solo = a.channels().create("alerts", vec![]).unwrap();
    let delivery = a.send_channel(&solo, "disk at 91%", "r1", None).unwrap();
    assert!(delivery.delivered.is_empty() && delivery.failed.is_empty());
    assert_eq!(a.chats().lines(&solo.thread()).len(), 1);

    let channel = a
        .channels()
        .create("team", vec![b.id().to_string()])
        .unwrap();
    b.set_reachable(false);
    let error = a.send_channel(&channel, "hello", "r2", None).unwrap_err();
    assert!(
        matches!(error, SipilError::Refused { ref code, .. } if code == "unavailable"),
        "{error:?}"
    );
    assert!(
        a.chats().lines(&channel.thread()).is_empty(),
        "nothing is kept that nobody heard"
    );
}

// ---- mentions ----

/// A little daemon: it makes sessions, takes messages, and answers each one with
/// the next of `answers`, keeping every prompt it was sent.
#[derive(Debug)]
struct Mind {
    answers: Mutex<VecDeque<String>>,
    prompts: Mutex<Vec<String>>,
    items: Mutex<Vec<Value>>,
}

impl Service for Mind {
    fn handle(&self, _from: &str, method: &str, params: Value) -> Result<Value, Failure> {
        let mut items = self.items.lock().unwrap();
        match method {
            "health" => Ok(
                json!({"name": "sipil", "kind": "daemon", "label": "Researcher", "version": "0", "api": 3, "allowed": true}),
            ),
            "sessions.create" => Ok(json!({"id": "4", "title": "New session", "status": "idle"})),
            "transcript" => Ok(json!({ "items": *items })),
            "sessions" => Ok(json!({"sessions": [{"id": "4", "title": "t", "status": "idle"}]})),
            "message" => {
                self.prompts
                    .lock()
                    .unwrap()
                    .push(params["content"].as_str().unwrap().to_string());
                let next = items.len() as u64 + 1;
                items.push(json!({"id": next, "role": "user", "text": "(prompt)", "tools": []}));
                let answer = self.answers.lock().unwrap().pop_front().unwrap_or_default();
                items.push(
                    json!({"id": next + 1, "role": "assistant", "text": answer, "tools": []}),
                );
                Ok(json!({"submissionId": "1"}))
            }
            _ => Err(Failure::new("unknown_method", "no")),
        }
    }
}

#[test]
fn a_mentioned_agent_reads_the_conversation_and_answers_into_it_for_everyone() {
    use pilus_client::sipil::mention;

    let (a, b, _c, _hb, _hc) = trio("mention");
    let mind = Arc::new(Mind {
        answers: Mutex::new(VecDeque::from([
            "**Rina** has a point about the cache.".to_string(),
            "Agreed on the second one.".to_string(),
        ])),
        prompts: Mutex::default(),
        items: Mutex::default(),
    });
    let server = node("mind", mind.clone() as Arc<dyn Service>);
    let researcher = Sipil::with_node(a.node(), &loopback(&server.addr())).unwrap();
    let daemon_id = researcher.target().id();

    // A direct chat between A and B: B said something, then A mentions the agent.
    let contact_b = a.directory().contact(b.id()).unwrap();
    let conversation = Conversation::Direct(contact_b.clone());
    let thread = conversation.thread();
    a.chats()
        .push_theirs(&thread, "the cache is too slow", "q1", None, None);
    a.say(&conversation, "@Researcher what do you think?", "r1", None)
        .unwrap();
    assert_eq!(
        mention::mentioned(
            "@Researcher what do you think?",
            &["Researcher".to_string(), "CoS".to_string()]
        ),
        ["Researcher"]
    );

    let speaker = |line: &pilus_client::sipil::Line| {
        if line.mine {
            "A".to_string()
        } else {
            "B".to_string()
        }
    };
    let ask = |request_id| mention::Mention {
        chats: a.chats(),
        daemon: &researcher,
        daemon_id: &daemon_id,
        agent: "Researcher",
        thread: &thread,
        conversation: "a chat between A and B",
        request_id,
    };
    let reply = mention::answer(&ask("m1"), &speaker).unwrap();
    assert_eq!(reply, "**Rina** has a point about the cache.");
    let first = mind.prompts.lock().unwrap()[0].clone();
    assert!(
        first.contains("B: the cache is too slow\nA: @Researcher what do you think?"),
        "{first}"
    );

    // The caller says the reply into the conversation, as the agent.
    a.say(&conversation, &reply, "m1-reply", Some("Researcher"))
        .unwrap();
    let theirs = b.chats().lines(a.id());
    let last = theirs.last().unwrap();
    assert_eq!(last.agent.as_deref(), Some("Researcher"));
    assert_eq!(last.text, reply);
    let mine = a.chats().lines(&thread);
    assert_eq!(mine.last().unwrap().agent.as_deref(), Some("Researcher"));

    // A second mention shows the agent only what is new, in the same session.
    b.say(
        &Conversation::Direct(b.directory().contact(a.id()).unwrap()),
        "and the second point?",
        "r2",
        None,
    )
    .unwrap();
    a.say(&conversation, "@Researcher and you?", "r3", None)
        .unwrap();
    let again = mention::answer(&ask("m2"), &speaker).unwrap();
    assert_eq!(again, "Agreed on the second one.");
    let second = mind.prompts.lock().unwrap()[1].clone();
    assert!(
        !second.contains("the cache is too slow"),
        "an old line is not shown twice: {second}"
    );
    assert!(second.contains("and the second point?") && second.contains("@Researcher and you?"));
}

#[test]
fn an_agent_answers_a_mention_only_in_a_channel_it_was_invited_into() {
    let (a, b, c, heard_a, _heard_b) = {
        let (a, heard_a) = window("guest-a");
        let (b, heard_b) = window("guest-b");
        let (c, _) = window("guest-c");
        for (me, them, label) in [
            (&a, &b, "B"),
            (&a, &c, "C"),
            (&b, &a, "A"),
            (&b, &c, "C"),
            (&c, &a, "A"),
            (&c, &b, "B"),
        ] {
            me.directory()
                .add(them.id(), label, Some(loopback(&them.node().addr())))
                .unwrap();
        }
        (a, b, c, heard_a, heard_b)
    };
    let research = Script::daemon(vec![]);
    let (research_client, _r) = daemon(&research);
    let daemon_id = research_client.target().id();
    a.set_daemons(vec![SharedDaemon {
        id: daemon_id.clone(),
        name: "Researcher".to_string(),
        sipil: Sipil::with_node(a.node(), research_client.target().text()).unwrap(),
    }]);

    let channel = a
        .channels()
        .create("project", vec![b.id().to_string(), c.id().to_string()])
        .unwrap();
    let other = a
        .channels()
        .create("elsewhere", vec![b.id().to_string()])
        .unwrap();
    // Everyone learns the channels from A's first line.
    a.send_channel(&channel, "kickoff", "r0", None).unwrap();
    a.send_channel(&other, "kickoff", "r0b", None).unwrap();

    let mentions = |heard: &Heard| {
        heard
            .lock()
            .unwrap()
            .iter()
            .filter(|event| matches!(event, Event::Mentioned { .. }))
            .cloned()
            .collect::<Vec<_>>()
    };
    assert!(
        mentions(&heard_a).is_empty(),
        "A's own line is A's business"
    );

    // Not invited yet: naming the agent does nothing, whoever names it.
    let channel_b = b.channels().get(&channel.id).unwrap();
    b.send_channel(&channel_b, "@researcher thoughts?", "r1", None)
        .unwrap();
    assert!(mentions(&heard_a).is_empty(), "an agent must be invited");

    // A invites it into one channel. Members B and C can now name it there.
    assert!(a.channels().set_agent(&channel.id, &daemon_id, true));
    b.send_channel(&channel_b, "@researcher thoughts?", "r2", None)
        .unwrap();
    assert_eq!(
        mentions(&heard_a),
        [Event::Mentioned {
            thread: channel.thread(),
            daemon: daemon_id.clone(),
            agent: "Researcher".to_string(),
        }]
    );
    let channel_c = c.channels().get(&channel.id).unwrap();
    c.send_channel(&channel_c, "@Researcher and you?", "r3", None)
        .unwrap();
    assert_eq!(mentions(&heard_a).len(), 2, "any member may name it");

    // It was not invited into the other channel, and an agent's own line never
    // starts another.
    let other_b = b.channels().get(&other.id).unwrap();
    b.send_channel(&other_b, "@Researcher here too", "r4", None)
        .unwrap();
    b.send_channel(&channel_b, "@Researcher again", "r5", Some("CoS"))
        .unwrap();
    assert_eq!(mentions(&heard_a).len(), 2);

    // A chat between two people is theirs alone: naming an agent there starts
    // nothing.
    let to_a = Conversation::Direct(b.directory().contact(a.id()).unwrap());
    b.say(&to_a, "@Researcher hello", "r6", None).unwrap();
    assert_eq!(mentions(&heard_a).len(), 2, "no agent joins a direct chat");

    // Taken out again, it hears nothing more.
    assert!(a.channels().set_agent(&channel.id, &daemon_id, false));
    b.send_channel(&channel_b, "@Researcher still?", "r7", None)
        .unwrap();
    assert_eq!(mentions(&heard_a).len(), 2);
}

// ---- replies and handed-off work ----

#[test]
fn a_reply_carries_the_line_it_answers_to_a_chat_and_to_every_member_of_a_channel() {
    let (a, b, c, _hb, _hc) = trio("reply");
    let quote = Quote::of("B", &format!("ship it\n{}", "x".repeat(400)));
    assert!(quote.text.ends_with('…') && quote.text.chars().count() <= 241);

    // A direct chat: both sides keep the quote with the line.
    let to_b = a.directory().contact(b.id()).unwrap();
    a.send_replying(&to_b, "agreed", "r1", None, Some(&quote))
        .unwrap();
    assert_eq!(a.chats().lines(b.id())[0].reply.as_ref(), Some(&quote));
    assert_eq!(b.chats().lines(a.id())[0].reply.as_ref(), Some(&quote));

    // A channel: the same, for the author and for each member.
    let channel = a
        .channels()
        .create("team", vec![b.id().to_string(), c.id().to_string()])
        .unwrap();
    a.say_replying(
        &Conversation::Channel(channel.clone()),
        "me too",
        "r2",
        None,
        Some(&quote),
    )
    .unwrap();
    for window in [&a, &b, &c] {
        let line = window.chats().lines(&channel.thread()).pop().unwrap();
        assert_eq!(line.reply.as_ref(), Some(&quote), "{}", window.id());
    }

    // Nothing is made up for a plain line, and a malformed quote is ignored.
    a.send(&to_b, "plain", "r3", None).unwrap();
    assert_eq!(b.chats().lines(a.id()).last().unwrap().reply, None);
    let target = pilus_client::sipil::parse_target(&loopback(&b.node().addr())).unwrap();
    a.node()
        .call(
            target.addr(),
            "chat.send",
            &json!({"requestId": "r4", "content": "odd", "reply": {"who": 7}}),
        )
        .unwrap();
    assert_eq!(b.chats().lines(a.id()).last().unwrap().reply, None);
}

/// A daemon whose agent hands work to a subagent before it answers.
#[derive(Debug, Default)]
struct Delegating {
    items: Mutex<Vec<Value>>,
}

impl Service for Delegating {
    fn handle(&self, _from: &str, method: &str, _params: Value) -> Result<Value, Failure> {
        let mut items = self.items.lock().unwrap();
        match method {
            "health" => Ok(
                json!({"name": "sipil", "kind": "daemon", "label": "CoS", "version": "0", "api": 3, "allowed": true}),
            ),
            "sessions.create" => Ok(json!({"id": "4", "title": "New session", "status": "idle"})),
            "sessions" => Ok(json!({"sessions": [{"id": "4", "title": "t", "status": "idle"}]})),
            "transcript" => Ok(json!({ "items": *items })),
            "message" => {
                let next = items.len() as u64 + 1;
                items.push(json!({"id": next, "role": "user", "text": "(prompt)", "tools": []}));
                items.push(json!({"id": next + 1, "role": "assistant", "text": "", "tools": ["subagent"],
                    "calls": [{"id": "call-1", "name": "subagent", "summary": "How does X work?", "args": "{}",
                        "subagent": {"conversationId": "9", "agent": "researcher", "task": "How does X work?"}}]}));
                items.push(
                    json!({"id": next + 2, "role": "tool", "text": "X works by Y.", "tools": [],
                    "callId": "call-1", "name": "subagent"}),
                );
                items.push(json!({"id": next + 3, "role": "assistant", "text": "It works by Y.", "tools": []}));
                Ok(json!({"submissionId": "1"}))
            }
            _ => Err(Failure::new("unknown_method", "no")),
        }
    }
}

#[test]
fn an_agent_handing_work_off_while_answering_leaves_a_note_only_its_owner_keeps() {
    use pilus_client::sipil::mention;

    let (a, b, _c, _hb, _hc) = trio("handoff");
    let server = node("handoff-daemon", Arc::new(Delegating::default()));
    let cos = Sipil::with_node(a.node(), &loopback(&server.addr())).unwrap();
    let daemon_id = cos.target().id();
    let channel = a
        .channels()
        .create("team", vec![b.id().to_string()])
        .unwrap();
    let conversation = Conversation::Channel(channel.clone());
    let thread = conversation.thread();
    a.say(&conversation, "@CoS look into X", "r1", None)
        .unwrap();

    let reply = mention::answer(
        &mention::Mention {
            chats: a.chats(),
            daemon: &cos,
            daemon_id: &daemon_id,
            agent: "CoS",
            thread: &thread,
            conversation: "the channel #team",
            request_id: "m1",
        },
        &|_| "A".to_string(),
    )
    .unwrap();
    assert_eq!(reply, "It works by Y.");
    a.say(&conversation, &reply, "m1-reply", Some("CoS"))
        .unwrap();

    let lines = a.chats().lines(&thread);
    let note = lines
        .iter()
        .find_map(|line| line.delegation.clone())
        .expect("the hand-off is noted in the thread");
    assert_eq!(
        (
            note.conversation.as_str(),
            note.agent.as_str(),
            note.task.as_str()
        ),
        ("9", "researcher", "How does X work?")
    );
    assert_eq!(note.daemon, daemon_id);
    let at = |text: &str| lines.iter().position(|line| line.text == text).unwrap();
    assert!(
        at("@CoS look into X") < at(&note.text()) && at(&note.text()) < at("It works by Y."),
        "the note comes between the question and the answer"
    );

    // B sees the answer but never the note: it is a link into A's daemon.
    let theirs = b.chats().lines(&channel.thread());
    assert!(theirs.iter().any(|line| line.text == "It works by Y."));
    assert!(
        theirs
            .iter()
            .all(|line| line.delegation.is_none() && !line.text.contains("talking with"))
    );
}
