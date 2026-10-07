//! One front door to this machine and every SSH machine it manages.
//!
//! A phone should pair with the Mac it trusts, not with each host that Mac can
//! reach. The hub is that single door: an enrolled device opens a stream on
//! [`HUB_ALPN`], names a machine, and sends one UHP-shaped request. `local`
//! (or no machine) goes to this machine's gateway with its delegated token;
//! any other id is a saved SSH machine, reached the way the window reaches it,
//! through `luvus uhp proxy` over the user's own SSH.
//!
//! What a device may call depends on its [`Level`]. `Control` is exactly the
//! gateway token's `allowed_methods`; `View` is the read-only subset of that;
//! `Operate` adds [`OPERATE`] (add and start tasks, start agents, open
//! workspaces), which the gateway refuses, so the hub runs them itself, on
//! this machine through Luvus's owner socket and on a saved machine through
//! `luvus uhp proxy`. Methods that stream or block are refused at every level
//! because a stream carries one request and one reply. A remote machine's owner
//! socket would accept far more than any of this, so these allowlists are the
//! only thing standing between a paired phone and that authority.
//!
//! Operate is remote code execution on purpose, so it is opt-in per device and
//! every write a device makes is logged with its name, level, and machine
//! (never the parameters, which may hold prompts).
//!
//! Wire format, one JSON line each way: `{"id","machine"?,"method","params"?}`
//! answered by `{"id","result"}` or `{"id","error":{"code","message"}}`. The
//! one hub-only method is `hub.machines`.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use iroh::endpoint::{Connection, RecvStream, SendStream};
use iroh::protocol::{AcceptError, ProtocolHandler};
use pilus_client::{ControlClient, Error as ClientError, MachineInfo, api_socket_path_for};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Semaphore;

use crate::enroll::{AuthState, Level};
use crate::supervisor::GatewayLink;

/// The ALPN a paired device uses for everything except pairing.
pub const HUB_ALPN: &[u8] = b"luvus/hub/1";

/// The local machine's id in `hub.machines` and in a request's `machine`.
pub const LOCAL: &str = "local";

/// Methods that stream or block until an event, which one request/one reply
/// cannot express, even when the gateway's token allows them.
const REFUSED: &[&str] = &[
    "events.subscribe",
    "events.wait",
    "wait.output",
    "terminal.backend.observe",
    "terminal.backend.wait_change",
    "terminal.backend.wait_output",
    "terminal.backend.events.subscribe",
];

/// The methods only an `Operate` device may call. Each starts work that runs
/// code (a task's agent and its gate command, an agent, a shell in a new
/// workspace), which is why the gateway token does not carry them.
const OPERATE: &[&str] = &["task.add", "task.start", "agent.start", "workspace.open"];

/// Method name endings that only read.
const READ_SUFFIXES: &[&str] = &[
    ".list",
    ".get",
    ".status",
    ".snapshot",
    ".read",
    ".capabilities",
    ".layout",
    ".edges",
    ".neighbor",
    ".current",
    ".explain",
    ".tree",
    ".log",
    ".branches",
    ".query",
    ".capture",
    ".inventory",
    ".processes",
    ".validate",
    ".sessions",
    ".stats",
    ".health",
];

/// Whether `method` only reads. Everything else, including methods this list
/// has never heard of, counts as a write.
pub fn is_read(method: &str) -> bool {
    matches!(method, "ping" | "search" | "mission.refresh")
        || READ_SUFFIXES.iter().any(|suffix| method.ends_with(suffix))
}

/// What a device at `level` may call, given what the gateway's token allows.
pub fn effective_methods(level: Level, gateway: &[String]) -> Vec<String> {
    let mut methods: Vec<String> = match level {
        Level::View => gateway
            .iter()
            .filter(|method| is_read(method))
            .cloned()
            .collect(),
        Level::Control | Level::Operate => gateway.to_vec(),
    };
    if level == Level::Operate && !gateway.is_empty() {
        for method in OPERATE {
            if !methods.iter().any(|name| name == method) {
                methods.push((*method).to_string());
            }
        }
    }
    methods
}

/// How long a request line may be before the peer is cut off.
const REQUEST_LIMIT: usize = 256 * 1024;
/// How long one call, including SSH setup, may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(25);
/// How long a saved-machine listing is trusted. The CLI call that produces it
/// costs a process spawn, and a phone polls.
const CATALOG_TTL: Duration = Duration::from_secs(10);
/// Concurrent SSH calls. Each is a process and a connection; a phone polling
/// several machines must not be able to exhaust either.
const SSH_PARALLELISM: usize = 6;

/// The saved-machine listing and when it was read.
type Catalog = Option<(Instant, Vec<MachineInfo>)>;

/// The hub's shared state.
#[derive(Clone)]
pub struct Hub {
    link: Arc<GatewayLink>,
    auth: Arc<AuthState>,
    /// The Luvus session whose owner socket `Operate` calls go to.
    session: Option<String>,
    label: String,
    catalog: Arc<Mutex<Catalog>>,
    ssh: Arc<Semaphore>,
}

impl std::fmt::Debug for Hub {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Hub")
            .field("label", &self.label)
            .finish()
    }
}

impl Hub {
    /// A hub over `link`. The gateway token is read from `auth` on every call
    /// because the supervisor replaces it whenever the gateway restarts, and
    /// the same state says what each device may do.
    pub fn new(
        link: Arc<GatewayLink>,
        label: String,
        auth: Arc<AuthState>,
        session: Option<String>,
    ) -> Self {
        Self {
            link,
            auth,
            session,
            label,
            catalog: Arc::new(Mutex::new(None)),
            ssh: Arc::new(Semaphore::new(SSH_PARALLELISM)),
        }
    }

    /// Answer one request line with one reply value, for a device at `level`.
    pub async fn handle(&self, request: &Value, level: Level, device: &str) -> Value {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
        let machine = request
            .get("machine")
            .and_then(Value::as_str)
            .filter(|machine| !machine.is_empty())
            .unwrap_or(LOCAL);

        if method == "hub.machines" {
            return reply(&id, Ok(self.machines().await));
        }
        if let Some(refusal) = self.refusal(level, method) {
            if !is_read(method) {
                eprintln!(
                    "pilus-agent: audit: {device} [{}] {machine} {method} refused",
                    level.as_str()
                );
            }
            return reply(&id, Err(refusal));
        }
        let outcome =
            tokio::time::timeout(CALL_TIMEOUT, self.route(level, machine, method, params))
                .await
                .unwrap_or_else(|_| Err(failure("timeout", "the machine did not answer in time")));
        if !is_read(method) {
            eprintln!(
                "pilus-agent: audit: {device} [{}] {machine} {method} {}",
                level.as_str(),
                match &outcome {
                    Ok(_) => "ok".to_string(),
                    Err(error) => format!(
                        "error {}",
                        error.get("code").and_then(Value::as_str).unwrap_or("?")
                    ),
                }
            );
        }
        reply(&id, outcome)
    }

    /// Why a device at `level` may not run `method`, or `None` when it may.
    fn refusal(&self, level: Level, method: &str) -> Option<Value> {
        if method.is_empty() {
            return Some(failure("bad_request", "a request needs a method"));
        }
        if REFUSED.contains(&method) {
            return Some(failure(
                "unsupported",
                "streaming methods are not available through the hub",
            ));
        }
        let gateway = self.link.allowed();
        if gateway.is_empty() {
            return Some(failure(
                "unavailable",
                "the gateway has not reported what it allows yet",
            ));
        }
        if !effective_methods(level, &gateway)
            .iter()
            .any(|name| name == method)
        {
            return Some(failure(
                "forbidden",
                &format!("{method} is not allowed for a {} device", level.as_str()),
            ));
        }
        None
    }

    async fn route(
        &self,
        level: Level,
        machine: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, Value> {
        if machine == LOCAL {
            if OPERATE.contains(&method) {
                return self.owner(method, params).await;
            }
            let result = self.local(method, params).await?;
            return Ok(if method == "uhp.capabilities" {
                self.describe_access(level, result)
            } else {
                result
            });
        }
        let known = self.catalog().await;
        let Some(info) = known.into_iter().find(|info| info.id == machine) else {
            return Err(failure(
                "not_found",
                &format!("no saved machine `{machine}`"),
            ));
        };
        if !info.enabled {
            return Err(failure("unavailable", &format!("`{machine}` is disabled")));
        }
        let Some(target) = info.ssh_target() else {
            return Err(failure(
                "unavailable",
                &format!("`{machine}` has no verified remote binary"),
            ));
        };
        let _permit = self
            .ssh
            .acquire()
            .await
            .map_err(|_| failure("unavailable", "the hub is shutting down"))?;
        let called = method.to_string();
        tokio::task::spawn_blocking(move || {
            let mut client = ControlClient::connect_ssh(target)?;
            client.request(&called, params)
        })
        .await
        .map_err(|error| failure("internal", &error.to_string()))?
        .map_err(|error| client_failure(error, method))
    }

    async fn local(&self, method: &str, params: Value) -> Result<Value, Value> {
        let Some(address) = self.link.address() else {
            let why = self
                .link
                .error()
                .unwrap_or_else(|| "the Luvus gateway is not running".to_string());
            return Err(failure("unavailable", &why));
        };
        let request = json!({
            "id": "1",
            "method": method,
            "params": params,
            "auth": self.auth.token(),
        });
        let reply = gateway_call(&address, &request)
            .await
            .map_err(|error| failure("unavailable", &format!("gateway: {error}")))?;
        match reply.get("error") {
            Some(error) => Err(error.clone()),
            None => Ok(reply.get("result").cloned().unwrap_or(Value::Null)),
        }
    }

    /// Run an `Operate` method as this user, through Luvus's owner socket.
    ///
    /// This is the one place the hub exercises more authority than the gateway
    /// token grants. It is reached only after [`Hub::refusal`] has confirmed the
    /// device is at `Operate`.
    async fn owner(&self, method: &str, params: Value) -> Result<Value, Value> {
        let socket = api_socket_path_for(self.session.as_deref());
        let called = method.to_string();
        tokio::task::spawn_blocking(move || {
            let mut client = ControlClient::connect(&socket)?;
            client.request(&called, params)
        })
        .await
        .map_err(|error| failure("internal", &error.to_string()))?
        .map_err(|error| client_failure(error, method))
    }

    /// Rewrite the gateway's capabilities so the device sees what *it* may do:
    /// its own allowed methods and level, not the gateway token's.
    fn describe_access(&self, level: Level, mut capabilities: Value) -> Value {
        let gateway = self.link.allowed();
        if let Some(access) = capabilities
            .get_mut("access")
            .and_then(Value::as_object_mut)
        {
            access.insert(
                "allowed_methods".to_string(),
                json!(effective_methods(level, &gateway)),
            );
            access.insert("level".to_string(), json!(level.as_str()));
        }
        capabilities
    }

    /// This machine plus every saved SSH machine, as the phone should list them.
    async fn machines(&self) -> Value {
        let mut machines = vec![json!({
            "id": LOCAL,
            "label": self.label,
            "kind": "local",
            "enabled": true,
            "ready": self.link.is_up(),
            "error": self.link.error(),
        })];
        for info in self.catalog().await {
            machines.push(json!({
                "id": info.id,
                "label": info.display(),
                "kind": "ssh",
                "destination": info.destination,
                "enabled": info.enabled,
                "ready": info.enabled && info.ssh_target().is_some(),
            }));
        }
        json!({ "host": self.label, "machines": machines })
    }

    /// The saved machine profiles, cached briefly.
    async fn catalog(&self) -> Vec<MachineInfo> {
        if let Some((at, cached)) = lock(&self.catalog).as_ref()
            && at.elapsed() < CATALOG_TTL
        {
            return cached.clone();
        }
        let fresh =
            tokio::task::spawn_blocking(
                || pilus_client::list_machines(&pilus_client::config_dir()),
            )
            .await
            .unwrap_or_default();
        *lock(&self.catalog) = Some((Instant::now(), fresh.clone()));
        fresh
    }
}

impl ProtocolHandler for Hub {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let peer = connection.remote_id().to_string();
        while let Ok((send, recv)) = connection.accept_bi().await {
            // Looked up per stream, so a revoke or a lowered level applies to
            // the very next request on a connection that is already open.
            let Some(level) = self.auth.level_of(&peer) else {
                let _ = serve_refusal(send).await;
                continue;
            };
            let device = self.auth.device_name(&peer);
            let hub = self.clone();
            tokio::spawn(async move {
                let _ = serve_stream(&hub, send, recv, level, &device).await;
            });
        }
        Ok(())
    }
}

/// One stream: read a request line, answer it, finish.
async fn serve_stream(
    hub: &Hub,
    mut send: SendStream,
    recv: RecvStream,
    level: Level,
    device: &str,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(recv.take(REQUEST_LIMIT as u64));
    let mut line = String::new();
    let read = tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line)).await;
    let answer = match read {
        Ok(Ok(n)) if n > 0 && line.len() <= REQUEST_LIMIT => {
            match serde_json::from_str::<Value>(line.trim()) {
                Ok(request) => hub.handle(&request, level, device).await,
                Err(error) => reply(
                    &Value::Null,
                    Err(failure("bad_request", &error.to_string())),
                ),
            }
        }
        _ => reply(
            &Value::Null,
            Err(failure("bad_request", "no request received")),
        ),
    };
    send.write_all(answer.to_string().as_bytes()).await?;
    send.write_all(b"\n").await?;
    let _ = send.finish();
    Ok(())
}

/// Answer a stream from a device that has been revoked since it connected.
async fn serve_refusal(mut send: SendStream) -> std::io::Result<()> {
    let answer = reply(
        &Value::Null,
        Err(failure("forbidden", "this device is no longer paired")),
    );
    send.write_all(answer.to_string().as_bytes()).await?;
    send.write_all(b"\n").await?;
    let _ = send.finish();
    Ok(())
}

/// `{"id","result"}` or `{"id","error"}`.
fn reply(id: &Value, outcome: Result<Value, Value>) -> Value {
    match outcome {
        Ok(result) => json!({ "id": id, "result": result }),
        Err(error) => json!({ "id": id, "error": error }),
    }
}

fn failure(code: &str, message: &str) -> Value {
    json!({ "code": code, "message": message })
}

/// Fold a client error into an error object.
///
/// The control client reports a daemon refusal as `code: message`, but also
/// reports timeouts and SSH stderr as `method: detail`. A prefix equal to the
/// method that was called is therefore a transport failure, not a code.
fn client_failure(error: ClientError, method: &str) -> Value {
    let ClientError::Control(text) = error else {
        return failure("unavailable", &error.to_string());
    };
    match text.split_once(": ") {
        Some((code, message))
            if code != method
                && !code.is_empty()
                && code.chars().all(|c| c.is_ascii_lowercase() || c == '_') =>
        {
            failure(code, message)
        }
        Some((_, message)) => failure("unavailable", message),
        None => failure("unavailable", &text),
    }
}

/// One request to the gateway on a fresh connection, as UHP requires.
async fn gateway_call(address: &str, request: &Value) -> std::io::Result<Value> {
    let stream = tokio::net::TcpStream::connect(address).await?;
    let (read, mut write) = stream.into_split();
    write.write_all(request.to_string().as_bytes()).await?;
    write.write_all(b"\n").await?;
    write.flush().await?;
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    if line.trim().is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "the gateway closed without answering",
        ));
    }
    serde_json::from_str(line.trim())
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;

    /// A hub whose link is up at `address` and allows exactly `allowed`.
    fn hub_allowing(address: &str, allowed: &[&str]) -> Hub {
        let link = GatewayLink::fixed(address);
        link.set_allowed_for_test(allowed.iter().map(|name| name.to_string()).collect());
        Hub::new(link, "Test Mac".to_string(), test_auth(), None)
    }

    fn test_auth() -> Arc<AuthState> {
        let path = std::env::temp_dir().join(format!(
            "pilus-hub-test-{}-{:?}/devices.json",
            std::process::id(),
            std::thread::current().id()
        ));
        Arc::new(AuthState::load("tok".to_string(), path, &[], 60).expect("auth"))
    }

    const CONTROL: Level = Level::Control;

    #[tokio::test]
    async fn refuses_what_the_gateway_does_not_allow() {
        let hub = hub_allowing("127.0.0.1:1", &["workspace.list"]);
        let denied = hub
            .handle(
                &json!({ "id": "1", "method": "task.add", "params": {} }),
                CONTROL,
                "t",
            )
            .await;
        assert_eq!(denied["error"]["code"], "forbidden");
        assert_eq!(denied["id"], "1");
    }

    #[tokio::test]
    async fn streaming_methods_are_refused_even_when_allowed() {
        let hub = hub_allowing("127.0.0.1:1", &["events.subscribe"]);
        let refused = hub
            .handle(
                &json!({ "id": "2", "method": "events.subscribe" }),
                Level::Operate,
                "t",
            )
            .await;
        assert_eq!(refused["error"]["code"], "unsupported");
    }

    #[tokio::test]
    async fn an_unknown_gateway_allowlist_fails_closed_at_every_level() {
        let hub = Hub::new(
            GatewayLink::fixed("127.0.0.1:1"),
            "Mac".into(),
            test_auth(),
            None,
        );
        for level in [Level::View, Level::Control, Level::Operate] {
            let refused = hub
                .handle(&json!({ "id": "3", "method": "task.add" }), level, "t")
                .await;
            assert_eq!(refused["error"]["code"], "unavailable", "{level:?}");
        }
    }

    #[tokio::test]
    async fn a_view_device_cannot_write_even_what_control_can() {
        let hub = hub_allowing(
            "127.0.0.1:1",
            &["workspace.list", "agent.prompt", "agent.keys"],
        );
        let denied = hub
            .handle(
                &json!({ "id": "4", "method": "agent.prompt" }),
                Level::View,
                "t",
            )
            .await;
        assert_eq!(denied["error"]["code"], "forbidden");
        assert!(
            denied["error"]["message"]
                .as_str()
                .unwrap()
                .contains("view")
        );
    }

    #[test]
    fn effective_methods_by_level() {
        let gateway: Vec<String> = ["workspace.list", "agent.prompt", "task.retry", "agent.read"]
            .iter()
            .map(|m| m.to_string())
            .collect();
        let view = effective_methods(Level::View, &gateway);
        assert_eq!(view, vec!["workspace.list", "agent.read"]);
        assert_eq!(effective_methods(Level::Control, &gateway), gateway);
        let operate = effective_methods(Level::Operate, &gateway);
        for method in OPERATE {
            assert!(operate.iter().any(|name| name == method), "{method}");
        }
        assert!(operate.iter().any(|name| name == "agent.prompt"));
        // Nothing to extend until the gateway has said what it allows.
        assert!(effective_methods(Level::Operate, &[]).is_empty());
    }

    #[test]
    fn the_read_classifier_treats_the_unknown_as_a_write() {
        for read in [
            "workspace.list",
            "task.get",
            "mission.snapshot",
            "uhp.capabilities",
            "ping",
        ] {
            assert!(is_read(read), "{read}");
        }
        for write in [
            "task.add",
            "agent.prompt",
            "agent.keys",
            "workspace.open",
            "task.retry",
            "pane.rename",
            "something.new",
        ] {
            assert!(!is_read(write), "{write}");
        }
    }

    #[tokio::test]
    async fn local_calls_carry_the_token_and_return_the_result() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr").to_string();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let (read, mut write) = stream.into_split();
            let mut line = String::new();
            BufReader::new(read)
                .read_line(&mut line)
                .await
                .expect("read");
            let request: Value = serde_json::from_str(line.trim()).expect("json");
            assert_eq!(request["auth"], "tok");
            assert_eq!(request["method"], "workspace.list");
            write
                .write_all(b"{\"id\":\"1\",\"result\":{\"workspaces\":[]}}\n")
                .await
                .expect("write");
        });
        let hub = hub_allowing(&address, &["workspace.list"]);
        let answer = hub
            .handle(
                &json!({ "id": "9", "machine": "local", "method": "workspace.list" }),
                CONTROL,
                "t",
            )
            .await;
        assert_eq!(answer["id"], "9");
        assert_eq!(answer["result"]["workspaces"], json!([]));
        server.await.expect("server");
    }

    #[tokio::test]
    async fn a_down_gateway_is_unavailable_not_a_hang() {
        let link = GatewayLink::down();
        link.set_allowed_for_test(vec!["workspace.list".to_string()]);
        let hub = Hub::new(link, "Mac".into(), test_auth(), None);
        let answer = hub
            .handle(
                &json!({ "id": "4", "method": "workspace.list" }),
                CONTROL,
                "t",
            )
            .await;
        assert_eq!(answer["error"]["code"], "unavailable");
    }

    #[test]
    fn client_errors_keep_their_daemon_code() {
        let refusal = client_failure(
            ClientError::Control("forbidden: nope".to_string()),
            "task.add",
        );
        assert_eq!(refusal["code"], "forbidden");
        assert_eq!(refusal["message"], "nope");
        // Timeouts and SSH stderr are prefixed with the method, not a code.
        let transport = client_failure(
            ClientError::Control("workspace.list: ssh: connection refused".to_string()),
            "workspace.list",
        );
        assert_eq!(transport["code"], "unavailable");
        assert_eq!(transport["message"], "ssh: connection refused");
    }
}
