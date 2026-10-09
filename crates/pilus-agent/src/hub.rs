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
//! hub-only methods are `hub.machines`; `hub.agents` and `hub.folders` for an
//! `Operate` device choosing what to start and where; `hub.run`, which starts an
//! agent there and prompts it in one step; and `hub.watch`, the one method whose stream stays open
//! (see [`crate::watch`]). Luvus's own `server.agent_manifests` lists every
//! agent it knows how to launch; the question a picker has is which are installed
//! on that machine, which the window answers by probing, and so does the hub.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use iroh::endpoint::{Connection, RecvStream, SendStream};
use iroh::protocol::{AcceptError, ProtocolHandler};
use pilus_client::{ControlClient, Error as ClientError, MachineInfo, api_socket_path_for};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Semaphore, mpsc};

use crate::enroll::{AuthState, Level};
use crate::supervisor::GatewayLink;
use crate::watch::{self, Slots};

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

/// How long an installed-agent probe is trusted. It can run `codex` and
/// `opencode`, which is seconds of work for a list that changes on install.
const AGENTS_TTL: Duration = Duration::from_secs(60);

/// The saved-machine listing and when it was read.
type Catalog = Option<(Instant, Vec<MachineInfo>)>;

/// Installed-agent probes by machine id, kept as configs so both the picker
/// (`hub.agents`) and the launcher (`hub.run`) share one probe.
type AgentCache = HashMap<String, (Instant, Vec<pilus_client::agents::AgentConfig>)>;

/// The hub's shared state.
#[derive(Clone)]
pub struct Hub {
    link: Arc<GatewayLink>,
    auth: Arc<AuthState>,
    /// The Luvus session whose owner socket `Operate` calls go to.
    session: Option<String>,
    label: String,
    catalog: Arc<Mutex<Catalog>>,
    installed: Arc<Mutex<AgentCache>>,
    ssh: Arc<Semaphore>,
    watchers: Arc<Slots>,
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
            installed: Arc::default(),
            ssh: Arc::new(Semaphore::new(SSH_PARALLELISM)),
            watchers: Arc::default(),
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
        if matches!(method, "hub.agents" | "hub.folders" | "hub.run") {
            // These only read, but they exist to feed `agent.start`, `task.start`
            // and `workspace.open`, so a device that cannot start work has no use
            // for them, and a folder listing is not something to hand out freely.
            let outcome = if level < Level::Operate {
                Err(failure(
                    "forbidden",
                    &format!("{method} is not allowed for a {} device", level.as_str()),
                ))
            } else {
                let work = async {
                    match method {
                        "hub.agents" => self.agents(machine).await,
                        "hub.folders" => self.folders(machine, &params).await,
                        _ => self.run(machine, &params).await,
                    }
                };
                tokio::time::timeout(CALL_TIMEOUT, work)
                    .await
                    .unwrap_or_else(|_| {
                        Err(failure("timeout", "the machine did not answer in time"))
                    })
            };
            // `hub.run` starts work, so it is audited like every other write (the
            // prompt it carries is never logged).
            if method == "hub.run" {
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
            return reply(&id, outcome);
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

    /// Hold `send` open and relay this machine's changes to the device as topic
    /// signals. Only this machine has an event feed to relay: a saved SSH machine
    /// is reached one request at a time, so its device keeps polling.
    async fn serve_watch(
        &self,
        mut send: SendStream,
        request: &Value,
        peer: &str,
        device: &str,
    ) -> std::io::Result<()> {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let machine = request
            .get("machine")
            .and_then(Value::as_str)
            .filter(|machine| !machine.is_empty())
            .unwrap_or(LOCAL);
        let refuse = |code: &str, message: &str| reply(&id, Err(failure(code, message)));
        let opened = async {
            if machine != LOCAL {
                return Err(refuse(
                    "unsupported",
                    "live updates are only available for the machine that is paired",
                ));
            }
            let slot = self
                .watchers
                .acquire(peer)
                .map_err(|why| refuse("busy", why))?;
            let events = self
                .subscribe()
                .await
                .map_err(|error| reply(&id, Err(error)))?;
            Ok((slot, events))
        }
        .await;
        let (slot, events) = match opened {
            Ok(opened) => opened,
            Err(answer) => {
                watch::send(&mut send, &answer).await?;
                let _ = send.finish();
                return Ok(());
            }
        };
        watch::send(
            &mut send,
            &reply(
                &id,
                Ok(json!({ "watching": LOCAL, "heartbeat_secs": watch::HEARTBEAT.as_secs() })),
            ),
        )
        .await?;
        let stopped = send.stopped();
        let auth = Arc::clone(&self.auth);
        let who = peer.to_string();
        let reason = watch::relay(
            events,
            &mut send,
            watch::FLUSH,
            watch::HEARTBEAT,
            async {
                let _ = stopped.await;
            },
            move || auth.level_of(&who).is_some(),
        )
        .await;
        drop(slot);
        eprintln!("pilus-agent: watch closed for {device}: {reason}");
        let _ = watch::send(&mut send, &json!({ "event": "closed", "reason": reason })).await;
        let _ = send.finish();
        Ok(())
    }

    /// Open the gateway's event feed and hand back its lines.
    ///
    /// The first line is the subscription's own reply; an error there (a token
    /// that may not subscribe, a gateway at its connection limit) is returned
    /// as the failure rather than relayed as if it were an event.
    async fn subscribe(&self) -> Result<mpsc::Receiver<String>, Value> {
        let Some(address) = self.link.address() else {
            let why = self
                .link
                .error()
                .unwrap_or_else(|| "the Luvus gateway is not running".to_string());
            return Err(failure("unavailable", &why));
        };
        let unavailable =
            |error: std::io::Error| failure("unavailable", &format!("gateway: {error}"));
        let stream = tokio::net::TcpStream::connect(&address)
            .await
            .map_err(unavailable)?;
        let (read, mut write) = stream.into_split();
        let request = json!({
            "id": "1",
            "method": "events.subscribe",
            "params": {},
            "auth": self.auth.token(),
        });
        write
            .write_all(format!("{request}\n").as_bytes())
            .await
            .map_err(unavailable)?;
        let mut lines = BufReader::new(read);
        let mut first = String::new();
        let started = tokio::time::timeout(Duration::from_secs(10), lines.read_line(&mut first))
            .await
            .map_err(|_| failure("timeout", "the gateway did not start the event feed"))?
            .map_err(unavailable)?;
        let answer: Value = serde_json::from_str(first.trim()).unwrap_or(Value::Null);
        if started == 0 || answer.get("result").is_none() {
            return Err(answer
                .get("error")
                .cloned()
                .unwrap_or_else(|| failure("unavailable", "the gateway refused the event feed")));
        }
        let (tx, rx) = mpsc::channel(256);
        tokio::spawn(async move {
            // The write half stays open for as long as the feed is read:
            // closing it would end the subscription.
            let _keep_open = write;
            let mut line = String::new();
            loop {
                line.clear();
                match lines.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        // A full queue means this device cannot keep up; ending
                        // the feed makes it resync instead of seeing stale signals.
                        if tx.try_send(std::mem::take(&mut line)).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Ok(rx)
    }

    /// Start `params.agent` in a workspace and give it `params.prompt`, or give the
    /// prompt to the one a phone already started there.
    ///
    /// The window's launcher does the same steps (`launch_home_agent`): an agent
    /// spawns beside the focused pane, so the workspace is opened (focused) first,
    /// the idle pane is reused, and the prompt goes to the pane once it is ready.
    /// Doing it here as one request keeps the sequence on a single connection and
    /// one audit line, instead of four calls from a page that may drop mid-way.
    /// The agent gets an alias of our own, so a second message finds it again
    /// even when Luvus labels the process as another kind.
    async fn run(&self, machine: &str, params: &Value) -> Result<Value, Value> {
        let text = |key: &str| {
            params
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or_default()
                .to_string()
        };
        let (kind, prompt, path, workspace_id, model, effort) = (
            text("agent"),
            params
                .get("prompt")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            text("path"),
            text("workspace_id"),
            text("model"),
            text("effort"),
        );
        let bad = |message: &str| failure("bad_request", message);
        if kind.is_empty()
            || !kind
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(bad(
                "`agent` must name an agent kind, like `pi` or `claude`",
            ));
        }
        if prompt.trim().is_empty() && kind != "terminal" {
            return Err(bad("`prompt` is empty"));
        }
        if path.is_empty() {
            return Err(bad("`path` must be the workspace's directory"));
        }
        // Display names become the agent's launch flags here, on the machine
        // that knows the catalog; a kind without one launches on its defaults.
        let args = if model.is_empty() {
            Vec::new()
        } else {
            self.installed_agents(machine)
                .await
                .unwrap_or_default()
                .iter()
                .find(|config| config.kind == kind)
                .map(|config| config.launch_args(&model, &effort))
                .unwrap_or_default()
        };
        let launch = move |mut client: ControlClient| {
            if kind == "terminal" {
                launch_terminal(&mut client, &path, &workspace_id, &prompt)
            } else {
                launch_agent(&mut client, &kind, &path, &workspace_id, &prompt, &args)
            }
        };
        if machine == LOCAL {
            let socket = api_socket_path_for(self.session.as_deref());
            return tokio::task::spawn_blocking(move || launch(ControlClient::connect(&socket)?))
                .await
                .map_err(|error| failure("internal", &error.to_string()))?
                .map_err(|error| client_failure(error, "agent.start"));
        }
        let known = self.catalog().await;
        let target = known
            .into_iter()
            .find(|info| info.id == machine && info.enabled)
            .and_then(|info| info.ssh_target())
            .ok_or_else(|| failure("not_found", &format!("no usable machine `{machine}`")))?;
        let _permit = self
            .ssh
            .acquire()
            .await
            .map_err(|_| failure("unavailable", "the hub is shutting down"))?;
        tokio::task::spawn_blocking(move || launch(ControlClient::connect_ssh(target)?))
            .await
            .map_err(|error| failure("internal", &error.to_string()))?
            .map_err(|error| client_failure(error, "agent.start"))
    }

    /// The child folders of `params.path` (default `~`) on `machine`, for choosing
    /// a workspace to open.
    async fn folders(&self, machine: &str, params: &Value) -> Result<Value, Value> {
        let path = params
            .get("path")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .unwrap_or("~")
            .to_string();
        let listing = if machine == LOCAL {
            tokio::task::spawn_blocking(move || pilus_client::folders::list_local(&path))
                .await
                .map_err(|error| failure("internal", &error.to_string()))?
        } else {
            let known = self.catalog().await;
            let target = known
                .into_iter()
                .find(|info| info.id == machine && info.enabled)
                .and_then(|info| info.ssh_target())
                .ok_or_else(|| failure("not_found", &format!("no usable machine `{machine}`")))?;
            let _permit = self
                .ssh
                .acquire()
                .await
                .map_err(|_| failure("unavailable", "the hub is shutting down"))?;
            tokio::task::spawn_blocking(move || pilus_client::folders::list_remote(&target, &path))
                .await
                .map_err(|error| failure("internal", &error.to_string()))?
        }
        .map_err(|why| failure("unavailable", &why))?;
        Ok(json!({
            "machine": machine,
            "root": listing.root,
            "parent": pilus_client::folders::parent_of(&listing.root),
            "folders": listing
                .folders
                .iter()
                .map(|folder| json!({ "name": folder.name, "path": folder.path, "git": folder.git }))
                .collect::<Vec<_>>(),
        }))
    }

    /// The agents installed on `machine`, as a picker should list them.
    async fn agents(&self, machine: &str) -> Result<Value, Value> {
        let found = self.installed_agents(machine).await?;
        Ok(json!({
            "machine": machine,
            // Kinds `hub.run` accepts beyond real agent adapters; clients gate
            // their picker entries on this instead of offering kinds an older
            // connector would forward to `agent.start` and fail.
            "pseudo_kinds": ["terminal"],
            "agents": found
                .iter()
                .map(|agent| json!({
                    "kind": agent.kind,
                    "label": agent.label,
                    "default_model": agent.default_model,
                    "models": agent
                        .models
                        .iter()
                        .map(|model| json!({
                            "name": model.name,
                            "efforts": model.efforts,
                            "default_effort": model.default_effort,
                            "is_default": model.is_default,
                        }))
                        .collect::<Vec<_>>(),
                }))
                .collect::<Vec<_>>(),
        }))
    }

    /// The installed-agent probe for `machine`, trusted for [`AGENTS_TTL`].
    async fn installed_agents(
        &self,
        machine: &str,
    ) -> Result<Vec<pilus_client::agents::AgentConfig>, Value> {
        if let Some((at, cached)) = lock(&self.installed).get(machine)
            && at.elapsed() < AGENTS_TTL
        {
            return Ok(cached.clone());
        }
        let found = if machine == LOCAL {
            tokio::task::spawn_blocking(pilus_client::agents::local)
                .await
                .map_err(|error| failure("internal", &error.to_string()))?
        } else {
            let known = self.catalog().await;
            let target = known
                .into_iter()
                .find(|info| info.id == machine && info.enabled)
                .and_then(|info| info.ssh_target())
                .ok_or_else(|| failure("not_found", &format!("no usable machine `{machine}`")))?;
            let _permit = self
                .ssh
                .acquire()
                .await
                .map_err(|_| failure("unavailable", "the hub is shutting down"))?;
            tokio::task::spawn_blocking(move || pilus_client::agents::remote(&target))
                .await
                .map_err(|error| failure("internal", &error.to_string()))?
        };
        lock(&self.installed).insert(machine.to_string(), (Instant::now(), found.clone()));
        Ok(found)
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
            let peer = peer.clone();
            tokio::spawn(async move {
                let _ = serve_stream(&hub, send, recv, level, &peer, &device).await;
            });
        }
        Ok(())
    }
}

/// One stream: read a request line, answer it, finish. A `hub.watch` request
/// is the exception: its stream stays open and carries signals until it ends.
async fn serve_stream(
    hub: &Hub,
    mut send: SendStream,
    recv: RecvStream,
    level: Level,
    peer: &str,
    device: &str,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(recv.take(REQUEST_LIMIT as u64));
    let mut line = String::new();
    let read = tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line)).await;
    let answer = match read {
        Ok(Ok(n)) if n > 0 && line.len() <= REQUEST_LIMIT => {
            match serde_json::from_str::<Value>(line.trim()) {
                Ok(request)
                    if request.get("method").and_then(Value::as_str) == Some("hub.watch") =>
                {
                    return hub.serve_watch(send, &request, peer, device).await;
                }
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

/// The alias a phone-started agent gets: stable for one kind in one workspace.
fn phone_alias(kind: &str, workspace_id: &str, path: &str) -> String {
    let anchor = if workspace_id.is_empty() {
        path
    } else {
        workspace_id
    };
    let tail: String = anchor
        .chars()
        .rev()
        .filter(char::is_ascii_alphanumeric)
        .take(6)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("phone-{kind}-{}", tail.to_lowercase())
}

/// The blocking half of [`Hub::run`], on one control connection.
fn launch_agent(
    client: &mut ControlClient,
    kind: &str,
    path: &str,
    workspace_id: &str,
    prompt: &str,
    args: &[String],
) -> Result<Value, ClientError> {
    let alias = phone_alias(kind, workspace_id, path);
    let running = client.agents()?;
    if let Some(agent) = running.iter().find(|agent| agent.name == alias) {
        client.prompt_agent(&agent.pane, prompt)?;
        return Ok(json!({ "started": false, "name": alias, "pane": agent.pane }));
    }
    client.open_workspace(path)?;
    // A fresh workspace opens with one shell pane; reuse it so the launch does
    // not split a second, empty terminal beside the agent.
    let pane = client.idle_pane().ok().flatten();
    client.start_agent(&alias, kind, pane.as_deref(), args)?;
    if let Some(pane) = pane.as_deref() {
        client.focus_pane(pane).ok();
    }
    // Target the pane directly: a fresh agent's alias may not be registered yet.
    let target = pane.unwrap_or_else(|| alias.clone());
    client.prompt_agent(&target, prompt)?;
    Ok(json!({ "started": true, "name": alias, "pane": target }))
}

/// The `terminal` pseudo-kind: a fresh shell tab in the workspace, with the
/// prompt typed as its first command when one was given.
fn launch_terminal(
    client: &mut ControlClient,
    path: &str,
    _workspace_id: &str,
    prompt: &str,
) -> Result<Value, ClientError> {
    client.open_workspace(path)?;
    client.new_tab()?;
    // The new tab takes focus, so the current idle pane is its fresh shell.
    let pane = client.idle_pane().ok().flatten();
    if let Some(pane) = pane.as_deref() {
        client.focus_pane(pane).ok();
        if !prompt.is_empty() {
            client.send_input(pane, &format!("{prompt}\n"))?;
        }
    }
    Ok(json!({ "started": true, "name": "terminal", "pane": pane }))
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

    #[tokio::test]
    async fn listing_installed_agents_is_for_devices_that_can_start_work() {
        let hub = hub_allowing("127.0.0.1:1", &["workspace.list"]);
        for level in [Level::View, Level::Control] {
            let denied = hub
                .handle(&json!({ "id": "5", "method": "hub.agents" }), level, "t")
                .await;
            assert_eq!(denied["error"]["code"], "forbidden", "{level:?}");
        }
        // Operate gets an answer for this machine, whatever happens to be installed.
        let answer = hub
            .handle(
                &json!({ "id": "6", "method": "hub.agents" }),
                Level::Operate,
                "t",
            )
            .await;
        assert_eq!(answer["result"]["machine"], "local");
        assert!(answer["result"]["agents"].is_array());
        assert_eq!(answer["result"]["pseudo_kinds"], json!(["terminal"]));
        // A machine that was never saved is not probed.
        let unknown = hub
            .handle(
                &json!({ "id": "7", "machine": "nope", "method": "hub.agents" }),
                Level::Operate,
                "t",
            )
            .await;
        assert_eq!(unknown["error"]["code"], "not_found");
    }

    #[tokio::test]
    async fn folders_are_listed_for_operate_devices_only() {
        let base = std::env::temp_dir().join(format!("pilus-hub-folders-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("proj/.git")).expect("proj");
        std::fs::create_dir_all(base.join("plain")).expect("plain");
        let hub = hub_allowing("127.0.0.1:1", &["workspace.list"]);
        let ask = json!({
            "id": "f", "method": "hub.folders", "params": { "path": base.to_str().unwrap() }
        });
        for level in [Level::View, Level::Control] {
            let denied = hub.handle(&ask, level, "t").await;
            assert_eq!(denied["error"]["code"], "forbidden", "{level:?}");
        }
        let listed = hub.handle(&ask, Level::Operate, "t").await;
        assert_eq!(listed["result"]["root"], base.to_str().unwrap());
        assert_eq!(listed["result"]["folders"][0]["name"], "plain");
        assert_eq!(listed["result"]["folders"][1]["git"], true);
        assert!(listed["result"]["parent"].is_string());
        let missing = hub
            .handle(
                &json!({ "id": "g", "method": "hub.folders", "params": { "path": "/no/such/dir" } }),
                Level::Operate,
                "t",
            )
            .await;
        assert_eq!(missing["error"]["code"], "unavailable");
        let unknown = hub
            .handle(
                &json!({ "id": "h", "machine": "nope", "method": "hub.folders" }),
                Level::Operate,
                "t",
            )
            .await;
        assert_eq!(unknown["error"]["code"], "not_found");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_phone_alias_is_stable_per_kind_and_workspace() {
        let a = phone_alias("pi", "workspace_dad0a34a34adfe0658439fa000c3cde8", "/x");
        assert_eq!(
            a,
            phone_alias("pi", "workspace_dad0a34a34adfe0658439fa000c3cde8", "/y")
        );
        assert_ne!(
            a,
            phone_alias("claude", "workspace_dad0a34a34adfe0658439fa000c3cde8", "/x")
        );
        assert_ne!(
            a,
            phone_alias("pi", "workspace_00000000000000000000000000000001", "/x")
        );
        assert!(a.starts_with("phone-pi-") && a.is_ascii());
        assert!(phone_alias("pi", "", "/Users/me/p r").starts_with("phone-pi-"));
    }

    #[tokio::test]
    async fn running_an_agent_is_for_operate_devices_and_needs_real_arguments() {
        let hub = hub_allowing("127.0.0.1:1", &["workspace.list"]);
        let run = |params: Value| json!({ "id": "r", "method": "hub.run", "params": params });
        let ok = json!({ "agent": "pi", "prompt": "hi", "path": "/tmp" });
        for level in [Level::View, Level::Control] {
            let denied = hub.handle(&run(ok.clone()), level, "t").await;
            assert_eq!(denied["error"]["code"], "forbidden", "{level:?}");
        }
        for bad in [
            json!({ "agent": "", "prompt": "hi", "path": "/tmp" }),
            json!({ "agent": "p i; rm", "prompt": "hi", "path": "/tmp" }),
            json!({ "agent": "pi", "prompt": "  ", "path": "/tmp" }),
            json!({ "agent": "pi", "prompt": "hi" }),
        ] {
            let refused = hub.handle(&run(bad.clone()), Level::Operate, "t").await;
            assert_eq!(refused["error"]["code"], "bad_request", "{bad}");
        }
        let unknown = hub
            .handle(
                &json!({ "id": "r", "machine": "nope", "method": "hub.run", "params": ok }),
                Level::Operate,
                "t",
            )
            .await;
        assert_eq!(unknown["error"]["code"], "not_found");
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
