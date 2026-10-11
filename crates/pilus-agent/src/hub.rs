//! One front door to this machine and every SSH machine it manages.
//!
//! A phone should pair with the Mac it trusts, not with each host that Mac can
//! reach. The hub is that single door: an enrolled device opens a stream on
//! [`HUB_ALPN`], names a machine, and sends one UHP-shaped request. `local`
//! (or no machine) goes to this machine's gateway with its delegated token;
//! any other id is a saved SSH machine, reached the way the window reaches it,
//! through `luvus uhp proxy` over the user's own SSH.
//!
//! What a device may call depends on its [`Level`]. `Control` is the gateway
//! token's `allowed_methods` plus [`REVIEW`] (write and send diff review
//! notes); `View` is the read-only subset of the gateway's; `Operate` adds
//! [`OPERATE`] (add and start tasks, start agents, open workspaces). The
//! gateway refuses both extra sets, so the hub runs them itself, on this
//! machine through Luvus's owner socket and on a saved machine through
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
//!
//! `hub.run` takes an optional `idempotency_key`. A phone whose reply was lost
//! cannot tell whether the agent started, and resending would start a second
//! one; the same key within [`REPLAY_TTL`] returns the first outcome instead.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use iroh::endpoint::{Connection, RecvStream, SendStream};
use iroh::protocol::{AcceptError, ProtocolHandler};
use pilus_client::{
    ControlClient, Error as ClientError, GhRepo, MachineInfo, NewPull, StateFilter,
    api_socket_path_for,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{OnceCell, Semaphore, mpsc};

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

/// The GitHub reads, run with `gh` in a checkout on the machine. A `Control`
/// device may call them: they show what the host's own GitHub login can see in
/// a repository the device already reaches, and change nothing.
const GH_READS: &[&str] = &[
    "hub.gh.status",
    "hub.gh.issues",
    "hub.gh.issue",
    "hub.gh.pulls",
    "hub.gh.pull",
    "hub.gh.pull_draft",
];

/// The GitHub writes. They act as the host's GitHub account (a comment, or a
/// pull request after pushing the branch), which is more than the gateway
/// token grants, so like `hub.run` they need an `Operate` device and are
/// audited without their text.
const GH_WRITES: &[&str] = &[
    "hub.gh.issue_comment",
    "hub.gh.pull_comment",
    "hub.gh.pull_create",
];

/// How long a pull request creation may take: a push, then `gh pr create`.
const GH_CREATE_TIMEOUT: Duration = Duration::from_secs(120);
/// Most items a list call returns.
const GH_LIST_MAX: u64 = 100;

/// The diff review calls a `Control` device may make beyond the gateway's own
/// list, which carries only `diff.list`, `diff.get`, and `diff.note.list`.
///
/// None of them runs code. Notes are plain text in the daemon's own store, and
/// `diff.note.send` types into an agent pane exactly as `agent.prompt` does,
/// which `Control` already has. They are audited like every write, without
/// their parameters, since a note is as private as a prompt.
const REVIEW: &[&str] = &[
    "diff.refresh",
    "diff.note.add",
    "diff.note.edit",
    "diff.note.resolve",
    "diff.note.reopen",
    "diff.note.remove",
    "diff.note.send",
];

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
    // Nothing to extend until the gateway has said what it allows: an empty
    // list fails closed at every level.
    if gateway.is_empty() {
        return methods;
    }
    // The `hub.gh.*` methods are answered by the hub itself, never forwarded;
    // they are listed so a device can tell which of them its level carries.
    let extras: &[&[&str]] = match level {
        Level::View => &[],
        Level::Control => &[REVIEW, GH_READS],
        Level::Operate => &[REVIEW, OPERATE, GH_READS, GH_WRITES],
    };
    for method in extras.iter().copied().flatten() {
        if !methods.iter().any(|name| name == method) {
            methods.push((*method).to_string());
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

/// How long a keyed `hub.run` outcome is replayed. Long enough to outlast a
/// phone reconnecting after a dropped reply, short enough that the map stays
/// small.
const REPLAY_TTL: Duration = Duration::from_secs(600);

/// The longest `idempotency_key` accepted, matching the daemon's own limit.
const REPLAY_KEY_MAX: usize = 128;

/// How long an installed-agent probe is trusted. It can run `codex` and
/// `opencode`, which is seconds of work for a list that changes on install.
const AGENTS_TTL: Duration = Duration::from_secs(60);

/// The saved-machine listing and when it was read.
type Catalog = Option<(Instant, Vec<MachineInfo>)>;

/// Installed-agent probes by machine id, kept as configs so both the picker
/// (`hub.agents`) and the launcher (`hub.run`) share one probe.
type AgentCache = HashMap<String, (Instant, Vec<pilus_client::agents::AgentConfig>)>;

/// Outcomes of keyed requests, so a retry with the same key shares the first
/// attempt's result instead of running the work again.
///
/// Each key owns a [`OnceCell`], so a retry that arrives while the first
/// attempt is still running waits for it rather than racing it. Failures are
/// forgotten: a request that did not run may be tried again under its key.
#[derive(Default)]
struct Replays {
    cells: Mutex<ReplayCells>,
}

type ReplayCells = HashMap<String, (Instant, Arc<OnceCell<Result<Value, Value>>>)>;

impl Replays {
    async fn once(
        &self,
        key: String,
        work: impl Future<Output = Result<Value, Value>>,
    ) -> Result<Value, Value> {
        let cell = {
            let mut cells = lock(&self.cells);
            cells.retain(|_, (at, _)| at.elapsed() < REPLAY_TTL);
            Arc::clone(
                &cells
                    .entry(key.clone())
                    .or_insert_with(|| (Instant::now(), Arc::default()))
                    .1,
            )
        };
        let outcome = cell.get_or_init(|| work).await.clone();
        if outcome.is_err() {
            lock(&self.cells).remove(&key);
        }
        outcome
    }
}

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
    replays: Arc<Replays>,
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
            replays: Arc::default(),
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
        if method.starts_with("hub.gh.") {
            let outcome = self.gh_call(level, device, machine, method, &params).await;
            return reply(&id, outcome);
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
                        _ => self.run_once(device, machine, &params).await,
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
            if OPERATE.contains(&method) || REVIEW.contains(&method) {
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
        .map_err(|error| client_failure(&error, method))
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

    /// Run an `Operate` or [`REVIEW`] method as this user, through Luvus's
    /// owner socket.
    ///
    /// This is the one place the hub exercises more authority than the gateway
    /// token grants. It is reached only after [`Hub::refusal`] has confirmed the
    /// device's level carries the method.
    async fn owner(&self, method: &str, params: Value) -> Result<Value, Value> {
        let socket = api_socket_path_for(self.session.as_deref());
        let called = method.to_string();
        tokio::task::spawn_blocking(move || {
            let mut client = ControlClient::connect(&socket)?;
            client.request(&called, params)
        })
        .await
        .map_err(|error| failure("internal", &error.to_string()))?
        .map_err(|error| client_failure(&error, method))
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

    /// [`Hub::run`], once per `idempotency_key` for this device and machine.
    async fn run_once(&self, device: &str, machine: &str, params: &Value) -> Result<Value, Value> {
        let key = params
            .get("idempotency_key")
            .and_then(Value::as_str)
            .filter(|key| !key.is_empty());
        let Some(key) = key else {
            return self.run(machine, params).await;
        };
        if key.len() > REPLAY_KEY_MAX {
            return Err(failure(
                "bad_request",
                "`idempotency_key` is longer than 128 bytes",
            ));
        }
        self.replays
            .once(
                format!("{device}\u{1f}{machine}\u{1f}{key}"),
                self.run(machine, params),
            )
            .await
    }

    /// Start `params.agent` in a new workspace tab and give it `params.prompt`
    /// after the daemon reports that the agent is ready.
    ///
    /// The prompt retry continues on the same blocking control connection after
    /// the started result has reached the phone, so a slow agent does not make
    /// `hub.run` look hung.
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
        let launch_params = (kind, path, workspace_id, prompt, args);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        if machine == LOCAL {
            let socket = api_socket_path_for(self.session.as_deref());
            let (kind, path, workspace_id, prompt, args) = launch_params;
            drop(tokio::task::spawn_blocking(
                move || match ControlClient::connect(&socket) {
                    Ok(client) => launch_for_hub(
                        client,
                        &kind,
                        &path,
                        &workspace_id,
                        &prompt,
                        &args,
                        ready_tx,
                    ),
                    Err(error) => {
                        let _ = ready_tx.send(Err(client_failure(&error, "agent.start")));
                    }
                },
            ));
            return ready_rx
                .await
                .map_err(|_| failure("internal", "hub.run launch task ended early"))?;
        }
        let known = self.catalog().await;
        let target = known
            .into_iter()
            .find(|info| info.id == machine && info.enabled)
            .and_then(|info| info.ssh_target())
            .ok_or_else(|| failure("not_found", &format!("no usable machine `{machine}`")))?;
        let permit = self
            .ssh
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| failure("unavailable", "the hub is shutting down"))?;
        let (kind, path, workspace_id, prompt, args) = launch_params;
        drop(tokio::task::spawn_blocking(move || {
            // Keep the SSH slot through prompt retries so a slow agent cannot
            // let a phone exhaust the hub's process and connection budget.
            let _permit = permit;
            match ControlClient::connect_ssh(target) {
                Ok(client) => launch_for_hub(
                    client,
                    &kind,
                    &path,
                    &workspace_id,
                    &prompt,
                    &args,
                    ready_tx,
                ),
                Err(error) => {
                    let _ = ready_tx.send(Err(client_failure(&error, "agent.start")));
                }
            }
        }));
        ready_rx
            .await
            .map_err(|_| failure("internal", "hub.run launch task ended early"))?
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

    /// One `hub.gh.*` call: check the device's level, run `gh` on `machine`,
    /// and audit a write.
    async fn gh_call(
        &self,
        level: Level,
        device: &str,
        machine: &str,
        method: &str,
        params: &Value,
    ) -> Result<Value, Value> {
        let write = GH_WRITES.contains(&method);
        if !write && !GH_READS.contains(&method) {
            return Err(failure("unsupported", &format!("unknown method {method}")));
        }
        let needed = if write {
            Level::Operate
        } else {
            Level::Control
        };
        if level < needed {
            return Err(failure(
                "forbidden",
                &format!("{method} is not allowed for a {} device", level.as_str()),
            ));
        }
        let timeout = if method == "hub.gh.pull_create" {
            GH_CREATE_TIMEOUT
        } else {
            CALL_TIMEOUT
        };
        let outcome = tokio::time::timeout(timeout, self.gh_run(machine, method, params))
            .await
            .unwrap_or_else(|_| Err(failure("timeout", "GitHub did not answer in time")));
        if write {
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
        outcome
    }

    async fn gh_run(&self, machine: &str, method: &str, params: &Value) -> Result<Value, Value> {
        let path = params
            .get("path")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| failure("bad_request", "`path` must be the checkout's directory"))?
            .to_string();
        // Fail on the request's shape before a process or an SSH hop is spent.
        let call = GhCall::parse(method, params)?;
        let (target, _permit) = if machine == LOCAL {
            (None, None)
        } else {
            let known = self.catalog().await;
            let target = known
                .into_iter()
                .find(|info| info.id == machine && info.enabled)
                .and_then(|info| info.ssh_target())
                .ok_or_else(|| failure("not_found", &format!("no usable machine `{machine}`")))?;
            let permit = self
                .ssh
                .acquire()
                .await
                .map_err(|_| failure("unavailable", "the hub is shutting down"))?;
            (Some(target), Some(permit))
        };
        let repo = GhRepo::new(target, path);
        tokio::task::spawn_blocking(move || call.run(&repo))
            .await
            .map_err(|error| failure("internal", &error.to_string()))?
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

/// A validated `hub.gh.*` request, ready to run on a blocking thread.
enum GhCall {
    Status,
    Issues(StateFilter, u32),
    Issue(u64),
    IssueComment(u64, String),
    Pulls(StateFilter, u32),
    Pull(u64),
    PullComment(u64, String),
    PullDraft,
    PullCreate(NewPull),
}

impl GhCall {
    fn parse(method: &str, params: &Value) -> Result<Self, Value> {
        let bad = |message: &str| failure("bad_request", message);
        let text = |key: &str| {
            params
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let number = || {
            params
                .get("number")
                .and_then(Value::as_u64)
                .filter(|number| *number > 0)
                .ok_or_else(|| bad("`number` must be an issue or pull request number"))
        };
        let filter = StateFilter::from_wire(&text("state"));
        let limit = params
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(30)
            .clamp(1, GH_LIST_MAX) as u32;
        let comment = || {
            let body = text("body");
            if body.trim().is_empty() {
                Err(bad("`body` is empty"))
            } else {
                Ok(body)
            }
        };
        Ok(match method {
            "hub.gh.status" => Self::Status,
            "hub.gh.issues" => Self::Issues(filter, limit),
            "hub.gh.issue" => Self::Issue(number()?),
            "hub.gh.issue_comment" => Self::IssueComment(number()?, comment()?),
            "hub.gh.pulls" => Self::Pulls(filter, limit),
            "hub.gh.pull" => Self::Pull(number()?),
            "hub.gh.pull_comment" => Self::PullComment(number()?, comment()?),
            "hub.gh.pull_draft" => Self::PullDraft,
            "hub.gh.pull_create" => {
                let (title, base) = (text("title"), text("base"));
                if title.trim().is_empty() {
                    return Err(bad("`title` is empty"));
                }
                if base.trim().is_empty() {
                    return Err(bad("`base` must name the branch to merge into"));
                }
                Self::PullCreate(NewPull {
                    title,
                    body: text("body"),
                    base,
                    draft: params
                        .get("draft")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                })
            }
            other => return Err(failure("unsupported", &format!("unknown method {other}"))),
        })
    }

    /// Run it. `gh`'s own refusals come back as `gh_failed` with its message,
    /// so a phone can show why.
    fn run(self, repo: &GhRepo) -> Result<Value, Value> {
        let failed = |message: String| failure("gh_failed", &message);
        match self {
            Self::Status => {
                let state = repo.probe();
                Ok(json!({
                    "state": state.wire(),
                    "repo": state.repo(),
                    "advice": state.advice(),
                }))
            }
            Self::Issues(filter, limit) => repo
                .issues(filter, limit)
                .map(|issues| json!({ "issues": issues }))
                .map_err(failed),
            Self::Issue(number) => repo
                .issue(number)
                .map(|detail| json!(detail))
                .map_err(failed),
            Self::IssueComment(number, body) => repo
                .comment_issue(number, &body)
                .map(|url| json!({ "url": url }))
                .map_err(failed),
            Self::Pulls(filter, limit) => repo
                .pulls(filter, limit)
                .map(|pulls| json!({ "pulls": pulls }))
                .map_err(failed),
            Self::Pull(number) => repo
                .pull(number)
                .map(|detail| json!(detail))
                .map_err(failed),
            Self::PullComment(number, body) => repo
                .comment_pull(number, &body)
                .map(|url| json!({ "url": url }))
                .map_err(failed),
            Self::PullDraft => repo.pull_draft().map(|draft| json!(draft)).map_err(failed),
            Self::PullCreate(new) => repo
                .create_pull(&new)
                .map(|created| json!(created))
                .map_err(failed),
        }
    }
}

/// The base alias a phone-started agent gets before collision allocation.
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

const ALIAS_MAX_LEN: usize = 32;

/// Pick a daemon-valid alias that is not already in `taken`.
fn unique_alias(base: &str, taken: &[String]) -> String {
    let mut normalized: String = base
        .chars()
        .map(|character| character.to_ascii_lowercase())
        .filter(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || *character == '-'
                || *character == '_'
        })
        .collect();
    while normalized
        .chars()
        .next()
        .is_some_and(|character| !character.is_ascii_lowercase())
    {
        normalized.remove(0);
    }
    if normalized.is_empty() {
        normalized.push_str("phone");
    }
    normalized.truncate(ALIAS_MAX_LEN);
    if !taken.iter().any(|name| name == &normalized) {
        return normalized;
    }
    for suffix in 2.. {
        let suffix = format!("-{suffix}");
        let prefix_len = ALIAS_MAX_LEN.saturating_sub(suffix.len());
        let prefix: String = normalized.chars().take(prefix_len).collect();
        let candidate = format!("{prefix}{suffix}");
        if !taken.iter().any(|name| name == &candidate) {
            return candidate;
        }
    }
    unreachable!("the alias suffix search has an unbounded counter")
}

/// The blocking half of [`Hub::run`], on one control connection.
///
/// It always opens a new tab and returns before the separate prompt retry
/// begins, so the phone can navigate to the started pane immediately.
fn launch_agent(
    client: &mut ControlClient,
    kind: &str,
    path: &str,
    workspace_id: &str,
    args: &[String],
) -> Result<Value, ClientError> {
    let taken = client
        .agents()?
        .into_iter()
        .map(|agent| agent.name)
        .collect::<Vec<_>>();
    let alias = unique_alias(&phone_alias(kind, workspace_id, path), &taken);
    client.open_workspace(path)?;
    client.new_tab()?;
    let pane = client.idle_pane()?;
    client.start_agent(&alias, kind, pane.as_deref(), args)?;
    if let Some(pane) = pane.as_deref() {
        client.focus_pane(pane)?;
    }
    let target = pane.unwrap_or_else(|| alias.clone());
    Ok(json!({ "started": true, "name": alias, "pane": target }))
}

/// Launch an agent, report its pane, then keep the connection for readiness
/// retries without delaying the response that tells the phone it started.
fn launch_for_hub(
    mut client: ControlClient,
    kind: &str,
    path: &str,
    workspace_id: &str,
    prompt: &str,
    args: &[String],
    ready_tx: tokio::sync::oneshot::Sender<Result<Value, Value>>,
) {
    let launch = if kind == "terminal" {
        launch_terminal(&mut client, path, workspace_id)
    } else {
        launch_agent(&mut client, kind, path, workspace_id, args)
    };
    let mut result = match launch {
        Ok(result) => result,
        Err(error) => {
            let _ = ready_tx.send(Err(client_failure(&error, "agent.start")));
            return;
        }
    };
    result["prompt"] = json!(if prompt.is_empty() { "none" } else { "queued" });
    let target = result
        .get("pane")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if ready_tx.send(Ok(result)).is_err() {
        return;
    }
    if prompt.is_empty() {
        return;
    }
    let delivery = if kind == "terminal" {
        client.send_input(&target, &format!("{prompt}\n"))
    } else {
        retry_agent_prompt(
            || client.prompt_agent(&target, prompt),
            std::thread::sleep,
            Instant::now,
        )
    };
    if let Err(error) = delivery {
        let failure = client_failure(&error, "agent.prompt");
        let code = failure
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("unavailable")
            .to_string();
        eprintln!("pilus-agent: hub.run prompt for {target} not delivered: {code}");
    }
}

const PROMPT_RETRY_INTERVAL: Duration = Duration::from_millis(750);
const PROMPT_RETRY_DEADLINE: Duration = Duration::from_secs(90);

/// Retry only readiness failures, with injectable time for daemon-free tests.
fn retry_agent_prompt<F, S, C>(mut prompt: F, mut sleep: S, mut now: C) -> Result<(), ClientError>
where
    F: FnMut() -> Result<(), ClientError>,
    S: FnMut(Duration),
    C: FnMut() -> Instant,
{
    let deadline = now() + PROMPT_RETRY_DEADLINE;
    loop {
        match prompt() {
            Ok(()) => return Ok(()),
            Err(error) => {
                let failure = client_failure(&error, "agent.prompt");
                let code = failure
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("unavailable");
                if code != "agent_not_ready" || now() >= deadline {
                    return Err(error);
                }
                sleep(PROMPT_RETRY_INTERVAL);
                if now() >= deadline {
                    return Err(error);
                }
            }
        }
    }
}

/// The `terminal` pseudo-kind: a fresh shell tab in the workspace, with the
/// prompt typed as its first command when one was given.
fn launch_terminal(
    client: &mut ControlClient,
    path: &str,
    _workspace_id: &str,
) -> Result<Value, ClientError> {
    client.open_workspace(path)?;
    client.new_tab()?;
    // The new tab takes focus, so the current idle pane is its fresh shell.
    let pane = client.idle_pane()?;
    if let Some(pane) = pane.as_deref() {
        client.focus_pane(pane)?;
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
fn client_failure(error: &ClientError, method: &str) -> Value {
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
        None => failure("unavailable", text),
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
    async fn github_reads_need_control_and_writes_need_operate() {
        let hub = hub_allowing("127.0.0.1:1", &["workspace.list"]);
        let ask =
            |method: &str, params: Value| json!({ "id": "g", "method": method, "params": params });
        let code = |reply: &Value| reply["error"]["code"].as_str().unwrap_or("").to_string();

        let read = ask("hub.gh.issues", json!({ "path": "/" }));
        assert_eq!(
            code(&hub.handle(&read, Level::View, "t").await),
            "forbidden"
        );

        let write = ask(
            "hub.gh.issue_comment",
            json!({ "path": "/", "number": 1, "body": "hi" }),
        );
        for level in [Level::View, Level::Control] {
            assert_eq!(
                code(&hub.handle(&write, level, "t").await),
                "forbidden",
                "{level:?}"
            );
        }

        // A Control device gets past the gate to the request's own checks.
        for (method, params) in [
            ("hub.gh.issues", json!({})),
            ("hub.gh.issue", json!({ "path": "/" })),
            ("hub.gh.pull", json!({ "path": "/", "number": 0 })),
        ] {
            let reply = hub.handle(&ask(method, params), Level::Control, "t").await;
            assert_eq!(code(&reply), "bad_request", "{method}");
        }
        // Operate: an empty comment and an untitled pull request are refused
        // before anything runs.
        let empty = ask(
            "hub.gh.pull_comment",
            json!({ "path": "/", "number": 1, "body": "  " }),
        );
        assert_eq!(
            code(&hub.handle(&empty, Level::Operate, "t").await),
            "bad_request"
        );
        let untitled = ask(
            "hub.gh.pull_create",
            json!({ "path": "/", "base": "main", "title": " " }),
        );
        assert_eq!(
            code(&hub.handle(&untitled, Level::Operate, "t").await),
            "bad_request"
        );

        let unknown = hub
            .handle(
                &json!({ "id": "h", "machine": "nope", "method": "hub.gh.status", "params": { "path": "/" } }),
                Level::Control,
                "t",
            )
            .await;
        assert_eq!(code(&unknown), "not_found");
    }

    #[tokio::test]
    async fn github_status_explains_a_folder_that_is_not_a_repo() {
        let dir = std::env::temp_dir().join(format!("pilus-hub-gh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let hub = hub_allowing("127.0.0.1:1", &["workspace.list"]);
        let reply = hub
            .handle(
                &json!({ "id": "s", "method": "hub.gh.status", "params": { "path": dir.to_str().unwrap() } }),
                Level::Control,
                "t",
            )
            .await;
        // `gh` reports its auth state before it can resolve the directory.
        let state = reply["result"]["state"].as_str().unwrap_or_default();
        assert!(
            matches!(state, "missing" | "no_repo" | "signed_out"),
            "{reply}"
        );
        assert!(reply["result"]["advice"].is_string(), "{reply}");
        let _ = std::fs::remove_dir_all(&dir);
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

    #[test]
    fn unique_alias_keeps_a_free_base() {
        assert_eq!(unique_alias("phone-pi-abcd", &[]), "phone-pi-abcd");
    }

    #[test]
    fn unique_alias_adds_a_collision_suffix() {
        let taken = vec!["phone-pi-abcd".to_string(), "phone-pi-abcd-2".to_string()];
        assert_eq!(unique_alias("phone-pi-abcd", &taken), "phone-pi-abcd-3");
    }

    #[test]
    fn unique_alias_truncates_before_a_suffix_and_stays_valid() {
        let base = "phone-claude-this-is-a-very-long-agent-alias";
        let alias = unique_alias(base, &[]);
        assert!(alias.len() <= ALIAS_MAX_LEN);
        assert!(
            alias
                .chars()
                .enumerate()
                .all(|(index, character)| if index == 0 {
                    character.is_ascii_lowercase()
                } else {
                    character.is_ascii_lowercase()
                        || character.is_ascii_digit()
                        || character == '_'
                        || character == '-'
                })
        );
        let collision = unique_alias(base, std::slice::from_ref(&alias));
        assert!(collision.len() <= ALIAS_MAX_LEN);
        assert!(collision.ends_with("-2"));
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

    #[tokio::test]
    async fn a_keyed_outcome_is_shared_and_a_failure_is_forgotten() {
        let replays = Replays::default();
        let runs = std::sync::atomic::AtomicUsize::new(0);
        let work = |outcome: Result<Value, Value>| {
            runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move { outcome }
        };
        let first = replays
            .once("k".into(), work(Ok(json!({ "pane": "7" }))))
            .await;
        let again = replays
            .once("k".into(), work(Ok(json!({ "pane": "8" }))))
            .await;
        assert_eq!(first, Ok(json!({ "pane": "7" })));
        assert_eq!(again, first, "the retry replays the first answer");

        let other = replays
            .once("other".into(), work(Ok(json!({ "pane": "9" }))))
            .await;
        assert_eq!(other, Ok(json!({ "pane": "9" })), "keys are independent");

        let failed = replays
            .once("f".into(), work(Err(failure("timeout", "slow"))))
            .await;
        assert!(failed.is_err());
        let retried = replays
            .once("f".into(), work(Ok(json!({ "pane": "1" }))))
            .await;
        assert_eq!(
            retried,
            Ok(json!({ "pane": "1" })),
            "a failure may be tried again"
        );
    }

    #[tokio::test]
    async fn an_overlong_idempotency_key_is_refused() {
        let hub = hub_allowing("127.0.0.1:1", &["workspace.list"]);
        let params = json!({
            "agent": "pi", "prompt": "hi", "path": "/tmp",
            "idempotency_key": "k".repeat(REPLAY_KEY_MAX + 1),
        });
        let refused = hub
            .handle(
                &json!({ "id": "r", "method": "hub.run", "params": params }),
                Level::Operate,
                "t",
            )
            .await;
        assert_eq!(refused["error"]["code"], "bad_request");
    }

    #[test]
    fn effective_methods_by_level() {
        let gateway: Vec<String> = [
            "workspace.list",
            "agent.prompt",
            "task.retry",
            "agent.read",
            "terminal.backend.type_literal",
            "terminal.backend.send_key",
        ]
        .iter()
        .map(|m| m.to_string())
        .collect();
        let view = effective_methods(Level::View, &gateway);
        assert_eq!(view, vec!["workspace.list", "agent.read"]);
        // Control adds the review calls and the GitHub reads, nothing else.
        let control = effective_methods(Level::Control, &gateway);
        assert_eq!(control[..gateway.len()], gateway[..]);
        assert_eq!(control.len(), gateway.len() + REVIEW.len() + GH_READS.len());
        for method in GH_READS {
            assert!(control.iter().any(|name| name == method), "{method}");
            assert!(!view.iter().any(|name| name == method), "{method}");
        }
        for method in GH_WRITES {
            assert!(!control.iter().any(|name| name == method), "{method}");
        }
        for method in REVIEW {
            assert!(control.iter().any(|name| name == method), "{method}");
            assert!(!view.iter().any(|name| name == method), "{method}");
        }
        for method in OPERATE {
            assert!(!control.iter().any(|name| name == method), "{method}");
        }
        let operate = effective_methods(Level::Operate, &gateway);
        for method in OPERATE.iter().chain(GH_READS).chain(GH_WRITES) {
            assert!(operate.iter().any(|name| name == method), "{method}");
        }
        assert!(operate.iter().any(|name| name == "agent.prompt"));
        for method in ["terminal.backend.type_literal", "terminal.backend.send_key"] {
            assert!(operate.iter().any(|name| name == method));
            assert!(!OPERATE.contains(&method));
        }
        // Nothing to extend until the gateway has said what it allows.
        assert!(effective_methods(Level::Operate, &[]).is_empty());
    }

    #[tokio::test]
    async fn reviewing_is_for_control_devices_and_reading_is_for_everyone() {
        let hub = hub_allowing("127.0.0.1:1", &["diff.list", "diff.get", "diff.note.list"]);
        for level in [Level::View, Level::Control, Level::Operate] {
            assert!(hub.refusal(level, "diff.list").is_none(), "{level:?}");
            assert!(hub.refusal(level, "diff.note.list").is_none(), "{level:?}");
        }
        for method in REVIEW {
            let view = hub.refusal(Level::View, method).expect("view is refused");
            assert_eq!(view["code"], "forbidden", "{method}");
            for level in [Level::Control, Level::Operate] {
                assert!(hub.refusal(level, method).is_none(), "{level:?} {method}");
            }
        }
        // The gateway has not reported yet, so nothing is granted.
        let waiting = Hub::new(
            GatewayLink::fixed("127.0.0.1:1"),
            "Mac".into(),
            test_auth(),
            None,
        );
        let refused = waiting
            .refusal(Level::Control, "diff.note.add")
            .expect("refused");
        assert_eq!(refused["code"], "unavailable");
    }

    #[test]
    fn review_writes_are_audited_as_writes_and_reads_are_not() {
        for method in REVIEW {
            assert!(!is_read(method), "{method}");
        }
        for method in ["diff.list", "diff.get", "diff.note.list"] {
            assert!(is_read(method), "{method}");
        }
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
            &ClientError::Control("forbidden: nope".to_string()),
            "task.add",
        );
        assert_eq!(refusal["code"], "forbidden");
        assert_eq!(refusal["message"], "nope");
        // Timeouts and SSH stderr are prefixed with the method, not a code.
        let transport = client_failure(
            &ClientError::Control("workspace.list: ssh: connection refused".to_string()),
            "workspace.list",
        );
        assert_eq!(transport["code"], "unavailable");
        assert_eq!(transport["message"], "ssh: connection refused");
    }

    #[test]
    fn prompt_retry_succeeds_after_transient_not_ready_errors() {
        let origin = Instant::now();
        let elapsed = std::cell::Cell::new(Duration::ZERO);
        let mut attempts = 0;
        let result = retry_agent_prompt(
            || {
                attempts += 1;
                if attempts < 3 {
                    Err(ClientError::Control(
                        "agent_not_ready: still starting".to_string(),
                    ))
                } else {
                    Ok(())
                }
            },
            |delay| {
                assert_eq!(delay, PROMPT_RETRY_INTERVAL);
                elapsed.set(elapsed.get() + delay);
            },
            || origin + elapsed.get(),
        );
        assert!(result.is_ok());
        assert_eq!(attempts, 3);
    }

    #[test]
    fn prompt_retry_stops_on_non_retryable_error() {
        let mut attempts = 0;
        let result = retry_agent_prompt(
            || {
                attempts += 1;
                Err(ClientError::Control("forbidden: no".to_string()))
            },
            |_| panic!("non-retryable errors must not sleep"),
            Instant::now,
        );
        assert!(result.is_err());
        assert_eq!(attempts, 1);
    }

    #[test]
    fn prompt_retry_stops_at_its_deadline() {
        let origin = Instant::now();
        let elapsed = std::cell::Cell::new(Duration::ZERO);
        let mut attempts = 0;
        let result = retry_agent_prompt(
            || {
                attempts += 1;
                Err(ClientError::Control(
                    "agent_not_ready: still starting".to_string(),
                ))
            },
            |delay| {
                assert_eq!(delay, PROMPT_RETRY_INTERVAL);
                elapsed.set(elapsed.get() + Duration::from_secs(30).max(delay));
            },
            || origin + elapsed.get(),
        );
        assert!(result.is_err());
        assert!(attempts >= 3);
        assert!(elapsed.get() >= PROMPT_RETRY_DEADLINE);
    }
}
