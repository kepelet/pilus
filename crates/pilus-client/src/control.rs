//! The UHP control surface, used for structured state the render socket does
//! not project — the live agent list, the status-bar widgets, and the tabs.
//!
//! The daemon exposes UHP on its control socket (`luvus.sock`) as
//! newline-delimited `{id, method, params}` requests answered by
//! `{id, result|error}`. [`ControlClient`] is the synchronous request/response
//! half; [`AgentFeed`] keeps a snapshot fresh in the background and forwards
//! commands, so the GUI never blocks on the socket.
//!
//! Refreshes are **event-driven**: the feed holds an `events.subscribe` stream
//! and re-reads the snapshots only when a structural event arrives, plus a
//! slow reconciliation so a missed event cannot leave the chrome stale.
//! Commands run on their own thread and poke the reconcile thread when they
//! land, so a click is never queued behind an in-flight refresh — a remote
//! refresh is a full SSH round trip, and a click waiting behind one reads as
//! an unresponsive tab strip.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, bounded, never, select, tick, unbounded};
use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::transport::{SshTarget, Stream};

const REMOTE_REQUEST_TIMEOUT: Duration = Duration::from_secs(12);

/// One segment of a status-bar widget.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BarSegment {
    /// Text to draw; empty for a separator.
    pub text: String,
    /// Tone name: `normal`, `muted`, `accent`, `success`, or `error`.
    pub tone: String,
    /// Whether this segment is a separator between texts.
    pub separator: bool,
}

/// One status-bar widget as reported by `ui.bar.list`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BarWidget {
    /// Stable widget key.
    pub key: String,
    /// Declared region, e.g. `bottom-right`.
    pub region: String,
    /// Its rendered segments.
    pub content: Vec<BarSegment>,
}

/// One workspace tab as reported by `tab.list`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Tab {
    /// 1-based position, as accepted by `tab.focus`/`tab.close`.
    pub index: u16,
    /// Stable tab id.
    pub id: String,
    /// Whether this tab is active.
    pub active: bool,
    /// User-assigned name, empty when unnamed.
    pub name: String,
    /// `panes`, `git`, `orch`, or `mission`.
    pub kind: String,
    /// Focused terminal cwd, or the workspace cwd when no terminal exists.
    pub cwd: String,
    /// Focused terminal's detected command/process, empty for a native view.
    pub process: String,
}

impl Tab {
    fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            index: value.get("tab")?.as_str()?.parse().ok()?,
            id: value
                .get("tab_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            active: value
                .get("active")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            name: value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            kind: value
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("panes")
                .to_string(),
            cwd: String::new(),
            process: String::new(),
        })
    }

    /// The label to draw: the tab's name, else its kind or number.
    pub fn label(&self) -> String {
        if !self.name.is_empty() {
            return self.name.clone();
        }
        match self.kind.as_str() {
            "git" => "Git".to_string(),
            "orch" => "Board".to_string(),
            "mission" => "Mission".to_string(),
            _ => self.index.to_string(),
        }
    }
}

/// One appearance theme from `theme.list`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ThemeInfo {
    /// Stable theme id, as accepted by `theme.use`.
    pub id: String,
    /// Human display name.
    pub display_name: String,
    /// One-line description.
    pub description: String,
    /// Author, empty for the Luvus team defaults.
    pub author: String,
    /// `dark`, `light`, or `terminal`.
    pub appearance: String,
    /// `built_in`, `installed`, or `virtual`.
    pub source: String,
    /// Whether this is the selected theme.
    pub active: bool,
}

impl ThemeInfo {
    fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            id: value.get("id")?.as_str()?.to_string(),
            display_name: value
                .get("display_name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            description: value
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            author: value
                .get("author")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            appearance: value
                .get("appearance")
                .and_then(Value::as_str)
                .unwrap_or("dark")
                .to_string(),
            source: value
                .get("source")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            active: value
                .get("active")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }
}

/// The theme registry: every registered theme plus the active id.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Appearance {
    /// All registered themes, in the daemon's display order.
    pub themes: Vec<ThemeInfo>,
    /// The selected theme's id.
    pub active: String,
}

/// One poll of the live agent list and the status-bar widgets.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FeedUpdate {
    /// Current live agent sessions.
    pub agents: Vec<Agent>,
    /// Current status-bar widgets.
    pub bar: Vec<BarWidget>,
    /// Current tabs of the active workspace.
    pub tabs: Vec<Tab>,
    /// Current automation definitions.
    pub automations: AutomationSummary,
    /// Current tasks.
    pub tasks: TaskSummary,
}

/// How many automation definitions exist, and how many are armed.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AutomationSummary {
    /// Definitions that will fire: everything not explicitly disabled.
    pub enabled: u32,
    /// Every definition, disabled or not.
    pub total: u32,
}

impl AutomationSummary {
    fn from_json(value: &Value) -> Self {
        let items = value
            .get("automations")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let total = items.len() as u32;
        let enabled = items.iter().filter(|item| is_armed(item)).count() as u32;
        Self { enabled, total }
    }
}

/// A definition fires unless it says otherwise, under either flag spelling.
fn is_armed(item: &Value) -> bool {
    if let Some(disabled) = item.get("disabled").and_then(Value::as_bool) {
        return !disabled;
    }
    if let Some(enabled) = item.get("enabled").and_then(Value::as_bool) {
        return enabled;
    }
    true
}

/// How many tasks exist, and how many still need attention.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TaskSummary {
    /// Tasks not in a terminal state.
    pub open: u32,
    /// Every task, open or not.
    pub total: u32,
}

impl TaskSummary {
    fn from_json(value: &Value) -> Self {
        let items = value
            .get("tasks")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let total = items.len() as u32;
        let open = items.iter().filter(|item| is_open(item)).count() as u32;
        Self { open, total }
    }
}

/// Fill each tab's cwd/process from `session.snapshot`: the focused terminal
/// pane wins, then any terminal, then the active workspace's own cwd.
fn enrich_tab_runtime(tabs: &mut [Tab], snapshot: &Value) {
    let Some(workspace) = snapshot
        .get("workspaces")
        .and_then(Value::as_array)
        .and_then(|workspaces| {
            workspaces
                .iter()
                .find(|workspace| workspace.get("active").and_then(Value::as_bool) == Some(true))
        })
    else {
        return;
    };
    let workspace_cwd = workspace
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let Some(runtime_tabs) = workspace.get("tabs").and_then(Value::as_array) else {
        return;
    };
    for tab in tabs {
        let Some(runtime) = runtime_tabs.iter().find(|runtime| {
            runtime.get("index").and_then(Value::as_u64) == Some(u64::from(tab.index))
        }) else {
            tab.cwd = workspace_cwd.to_string();
            continue;
        };
        let pane = runtime
            .get("panes")
            .and_then(Value::as_array)
            .and_then(|panes| {
                panes
                    .iter()
                    .find(|pane| {
                        pane.get("kind").and_then(Value::as_str) == Some("terminal")
                            && pane.get("focused").and_then(Value::as_bool) == Some(true)
                    })
                    .or_else(|| {
                        panes.iter().find(|pane| {
                            pane.get("kind").and_then(Value::as_str) == Some("terminal")
                        })
                    })
            });
        tab.cwd = pane
            .and_then(|pane| pane.get("cwd"))
            .and_then(Value::as_str)
            .unwrap_or(workspace_cwd)
            .to_string();
        tab.process = pane
            .and_then(|pane| pane.get("agent"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
    }
}

/// A task needs attention unless its status reached a terminal state.
fn is_open(item: &Value) -> bool {
    match item.get("status").and_then(Value::as_str) {
        Some(status) => !matches!(
            status,
            "done" | "completed" | "complete" | "merged" | "cancelled" | "canceled" | "archived"
        ),
        None => true,
    }
}

/// A pane's rectangle as fractions of its tab's pane area, from
/// `pane.layout`.
///
/// The daemon lays panes out in a fixed logical space rather than cells, so
/// the fractions hold at any grid size; a window maps them onto whatever
/// area it paints the panes in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PaneFrame {
    /// Left edge, 0.0 at the area's left.
    pub x: f32,
    /// Top edge, 0.0 at the area's top.
    pub y: f32,
    /// Width, 1.0 for the full area.
    pub width: f32,
    /// Height, 1.0 for the full area.
    pub height: f32,
}

impl PaneFrame {
    fn from_json(value: &Value) -> Option<Self> {
        let rect = value.get("rect")?;
        let size = value.get("logical_size")?;
        let field = |value: &Value, key: &str| value.get(key).and_then(Value::as_f64);
        let (area_w, area_h) = (field(size, "width")?, field(size, "height")?);
        if area_w <= 0.0 || area_h <= 0.0 {
            return None;
        }
        Some(Self {
            x: (field(rect, "x")? / area_w) as f32,
            y: (field(rect, "y")? / area_h) as f32,
            width: (field(rect, "width")? / area_w) as f32,
            height: (field(rect, "height")? / area_h) as f32,
        })
    }

    /// Whether the pane fills its tab: no split shares the area.
    pub fn is_full(&self) -> bool {
        self.x <= 0.001 && self.y <= 0.001 && self.width >= 0.999 && self.height >= 0.999
    }
}

/// One live agent session as reported by `agent.list`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Agent {
    /// The pane the agent runs in.
    pub pane: String,
    /// 1-based tab position inside the agent's workspace.
    pub tab: u16,
    /// The agent kind, e.g. `claude` or `codex`.
    pub agent: String,
    /// The agent's live alias, if the user gave it one.
    pub name: String,
    /// `working`, `blocked`, `done`, or `idle`.
    pub status: String,
    /// Stable workspace id.
    pub workspace_id: String,
    /// Workspace display name.
    pub workspace_name: String,
    /// The pane's working directory.
    pub cwd: String,
    /// The workspace's git branch, if any.
    pub branch: Option<String>,
    /// Whether this agent's pane holds focus.
    pub focused: bool,
    /// The session id the pane's agent reported via `pane.report_session`,
    /// if it runs an integration that reports one (pi's extension does).
    pub session: Option<String>,
}

impl Agent {
    fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            pane: value.get("pane")?.as_str()?.to_string(),
            tab: value
                .get("tab")
                .and_then(Value::as_str)
                .and_then(|tab| tab.parse().ok())
                .unwrap_or_default(),
            agent: value.get("agent")?.as_str()?.to_string(),
            name: value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            status: value
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("idle")
                .to_string(),
            workspace_id: value
                .get("workspace_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            workspace_name: value
                .get("workspace_name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            cwd: value
                .get("cwd")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            branch: value
                .get("branch")
                .and_then(Value::as_str)
                .map(str::to_string),
            focused: value
                .get("focused")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            session: value
                .get("session")
                .and_then(Value::as_str)
                .filter(|session| !session.is_empty())
                .map(str::to_string),
        })
    }

    /// The label to show first: the alias, else the pane's directory name.
    pub fn label(&self) -> &str {
        if !self.name.is_empty() {
            return &self.name;
        }
        self.cwd
            .rsplit('/')
            .find(|segment| !segment.is_empty())
            .unwrap_or(&self.agent)
    }
}

/// One resumable agent session found on disk, from `agent.sessions`.
///
/// The daemon scans each supported agent's own store, so this lists sessions
/// that *could* be resumed — it says nothing about which session a live pane
/// is in. That link only exists when the pane's agent reports it, surfacing
/// as [`Agent::session`]. The list is not filtered by pane: callers match
/// the `cwd` they already know.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentSession {
    /// The agent kind, e.g. `pi`.
    pub agent: String,
    /// The absolute working directory the session was recorded in.
    pub cwd: String,
    /// The agent's own session id, used to find its file on disk.
    pub session_id: String,
}

impl AgentSession {
    fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            agent: value.get("agent")?.as_str()?.to_string(),
            cwd: value
                .get("cwd")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            session_id: value.get("session_id")?.as_str()?.to_string(),
        })
    }
}

/// One worktree of a workspace's repo, from `worktree.list`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorktreeInfo {
    /// Absolute checkout path.
    pub path: String,
    /// Whether this is the repo's main checkout (not deletable).
    pub main: bool,
}

impl WorktreeInfo {
    fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            path: value.get("path")?.as_str()?.to_string(),
            main: value.get("main").and_then(Value::as_bool).unwrap_or(false),
        })
    }
}

/// The blockers behind a `worktree_in_use` refusal of an unforced
/// `worktree.remove`, in the daemon's own words, or `None` for any other error.
///
/// The daemon checks working or blocked agent panes, unfinished bound tasks
/// and leases before deleting, and names them so the operator can finish that
/// work or knowingly force past it.
pub fn worktree_blockers(error: &Error) -> Option<String> {
    let Error::Control(message) = error else {
        return None;
    };
    let detail = message.strip_prefix("worktree_in_use")?;
    let detail = detail.trim_start_matches(':').trim();
    Some(if detail.is_empty() {
        "it is still in use".to_string()
    } else {
        detail.to_string()
    })
}

/// Where a control request is sent.
///
/// The local API socket answers many connections on one process. A remote
/// machine is reached through `luvus uhp proxy`, which forwards exactly one
/// request per SSH invocation, so each call opens its own SSH child.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlTransport {
    /// The owner-local control socket.
    Socket(PathBuf),
    /// A `luvus uhp access` loopback gateway carrying UHP frames over TCP.
    Tcp(String),
    /// A remote host over the local OpenSSH client.
    Ssh(SshTarget),
}

impl ControlTransport {
    /// Open one fresh stream for a single request.
    pub(crate) fn stream(&self) -> Result<Stream> {
        match self {
            Self::Socket(path) => Stream::connect(path),
            Self::Tcp(address) => Stream::connect_tcp(address),
            Self::Ssh(target) => {
                let mut command = target.multiplexed_command(&["uhp", "proxy"]);
                Stream::spawn(&mut command)
            }
        }
    }
}

/// A synchronous request/response connection to the control socket.
///
/// The daemon's control socket answers exactly one request per connection, so
/// each call opens a fresh connection rather than reusing one.
pub struct ControlClient {
    transport: ControlTransport,
    next_id: u64,
}

impl ControlClient {
    /// Wrap an already-resolved transport.
    pub fn from_transport(transport: ControlTransport) -> Self {
        Self {
            transport,
            next_id: 0,
        }
    }

    /// Connect to the control socket at `path`.
    pub fn connect(path: &Path) -> Result<Self> {
        Ok(Self {
            transport: ControlTransport::Socket(path.to_path_buf()),
            next_id: 0,
        })
    }

    /// Connect to a remote machine's control surface over SSH.
    pub fn connect_ssh(target: SshTarget) -> Result<Self> {
        Ok(Self {
            transport: ControlTransport::Ssh(target),
            next_id: 0,
        })
    }

    /// Connect to a `luvus uhp access` loopback gateway over TCP.
    pub fn connect_tcp(address: impl Into<String>) -> Result<Self> {
        Ok(Self {
            transport: ControlTransport::Tcp(address.into()),
            next_id: 0,
        })
    }

    /// Send one method call and return its `result`.
    pub fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id = self.next_id.wrapping_add(1);
        let line = json!({
            "id": self.next_id.to_string(),
            "method": method,
            "params": params,
        })
        .to_string();
        let stream = Arc::new(self.transport.stream()?);
        {
            let mut writer = &*stream;
            writer.write_all(line.as_bytes())?;
            writer.write_all(b"\n")?;
            writer.flush()?;
        }

        let response = match &self.transport {
            ControlTransport::Ssh(_) | ControlTransport::Tcp(_) => {
                read_line_within(Arc::clone(&stream), REMOTE_REQUEST_TIMEOUT)
            }
            ControlTransport::Socket(_) => read_line(&stream),
        }
        .map_err(|error| {
            if matches!(&error, Error::Control(message) if message == "timed out") {
                return Error::Control(format!("{method}: timed out"));
            }
            match stream.stderr_tail() {
                Some(tail) => Error::Control(format!("{method}: {tail}")),
                None => error,
            }
        })?;
        if response.is_empty() {
            return Err(Error::Control("control socket closed".to_string()));
        }
        let value: Value = serde_json::from_str(&response)
            .map_err(|error| Error::Control(format!("bad control response: {error}")))?;
        if let Some(error) = value.get("error") {
            let code = error.get("code").and_then(Value::as_str).unwrap_or("error");
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default();
            return Err(Error::Control(format!("{code}: {message}")));
        }
        Ok(value.get("result").cloned().unwrap_or(Value::Null))
    }

    /// The current live agent sessions.
    pub fn agents(&mut self) -> Result<Vec<Agent>> {
        let result = self.request("agent.list", json!({}))?;
        let agents = result
            .get("agents")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        Ok(agents.iter().filter_map(Agent::from_json).collect())
    }

    /// Every resumable agent session the daemon finds on disk.
    ///
    /// Unlike [`Self::agents`], which lists live panes, this is the daemon's
    /// own scan of each agent's session store (for pi, the JSONL under
    /// `~/.pi/agent/sessions`). It works over the remote proxy too, so a
    /// client can locate the exact session a remote agent is in.
    pub fn agent_sessions(&mut self) -> Result<Vec<AgentSession>> {
        let result = self.request("agent.sessions", json!({}))?;
        let sessions = result
            .get("sessions")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        Ok(sessions
            .iter()
            .filter_map(AgentSession::from_json)
            .collect())
    }

    /// Focus the pane an agent runs in, jumping to its workspace and tab.
    pub fn focus_pane(&mut self, pane: &str) -> Result<()> {
        self.request("pane.focus", json!({ "pane": pane }))?;
        Ok(())
    }

    /// Open `path` as a workspace, or focus it when it is already open.
    pub fn open_workspace(&mut self, path: &str) -> Result<()> {
        self.request("workspace.open", json!({ "path": path }))?;
        Ok(())
    }

    /// Create a worktree (and its workspace) for `branch` in the active repo.
    pub fn create_worktree(&mut self, branch: &str) -> Result<()> {
        self.request("worktree.create", json!({ "branch": branch }))?;
        Ok(())
    }

    /// Create a worktree of the workspace at `index`'s repo, not the active one.
    ///
    /// The right-click menu targets whichever WORKSPACES row was hit, which is
    /// only the active workspace by coincidence; `workspace` names the repo.
    pub fn create_worktree_for(&mut self, workspace: u16, branch: &str) -> Result<()> {
        self.request(
            "worktree.create",
            json!({ "workspace": workspace, "branch": branch }),
        )?;
        Ok(())
    }

    /// Rename a workspace's display label. The daemon trims and caps the name.
    pub fn rename_workspace(&mut self, workspace_id: &str, name: &str) -> Result<()> {
        self.request(
            "workspace.rename",
            json!({ "workspace_id": workspace_id, "name": name }),
        )?;
        Ok(())
    }

    /// The worktrees of the workspace at `index`'s repo, per `worktree.list`.
    ///
    /// The right-click menu reads this to tell a linked worktree (deletable)
    /// from a main checkout or a plain folder. An empty list on success means
    /// the folder is not a git repo.
    pub fn worktrees(&mut self, workspace: u16) -> Result<Vec<WorktreeInfo>> {
        let result = self.request("worktree.list", json!({ "workspace": workspace }))?;
        let worktrees = result.get("worktrees").and_then(Value::as_array);
        Ok(worktrees
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(WorktreeInfo::from_json)
            .collect())
    }

    /// Remove a linked worktree and (with `force`) its folder.
    ///
    /// `force` matches the daemon's own confirm path: it skips the in-use
    /// guard and runs `git worktree remove --force`, so a dirty checkout does
    /// not block the delete. Unforced, a busy worktree is refused with
    /// `worktree_in_use` (see [`worktree_blockers`]). Daemon 0.14.2
    /// nevertheless refuses a checkout holding modified or untracked files
    /// whatever `force` says, so callers discard those first. The daemon
    /// closes any workspace whose cwd was inside the removed path.
    pub fn remove_worktree(&mut self, path: &str, force: bool) -> Result<()> {
        self.request("worktree.remove", json!({ "path": path, "force": force }))?;
        Ok(())
    }

    /// Pin or unpin a workspace, floating its group to the top of the dock.
    pub fn pin_workspace(&mut self, workspace_id: &str, pinned: bool) -> Result<()> {
        self.request(
            "workspace.pin",
            json!({ "workspace_id": workspace_id, "pinned": pinned }),
        )?;
        Ok(())
    }

    /// Close a workspace and the panes inside it.
    pub fn close_workspace(&mut self, workspace_id: &str) -> Result<()> {
        self.request("workspace.close", json!({ "workspace_id": workspace_id }))?;
        Ok(())
    }

    /// Close one pane (a live agent's pane), wherever it sits in the session.
    pub fn close_pane(&mut self, pane: &str) -> Result<()> {
        self.request("pane.close", json!({ "pane": pane }))?;
        Ok(())
    }

    /// Give a live pane an addressable alias, so `agent.send` and friends can
    /// target it by name. The daemon enforces `[a-z][a-z0-9_-]{0,31}`.
    pub fn name_agent(&mut self, pane: &str, name: &str) -> Result<()> {
        self.request("agent.name", json!({ "pane": pane, "name": name }))?;
        Ok(())
    }

    /// Clear a live pane's alias, returning it to its terminal-backend label.
    pub fn clear_agent_name(&mut self, pane: &str) -> Result<()> {
        self.request("agent.name", json!({ "pane": pane, "clear": true }))?;
        Ok(())
    }

    /// The focused pane's id when it runs no agent, so a launch can reuse it
    /// instead of splitting a second pane beside a shell.
    ///
    /// A pane's own `agent` field names its detected command (`fish`, `zsh`),
    /// so membership in `agent.list` is what identifies a busy pane.
    pub fn idle_pane(&mut self) -> Result<Option<String>> {
        let result = self.request("pane.current", json!({}))?;
        let Some(pane) = result
            .get("pane")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            return Ok(None);
        };
        if self.agents()?.iter().any(|agent| agent.pane == pane) {
            return Ok(None);
        }
        Ok(Some(pane))
    }

    /// Spawn a built-in agent, waiting until its pane is ready.
    ///
    /// `pane` reuses that pane instead of splitting beside the focused one;
    /// `args` are shell-quoted onto the agent's own command line (an empty
    /// slice launches with the manifest defaults).
    pub fn start_agent(
        &mut self,
        name: &str,
        kind: &str,
        pane: Option<&str>,
        args: &[String],
    ) -> Result<()> {
        let mut params = json!({ "name": name, "kind": kind, "args": args });
        if let Some(pane) = pane {
            params["pane"] = json!(pane);
        }
        self.request("agent.start", params)?;
        Ok(())
    }

    /// Type `text` into a running agent and submit it as one prompt.
    pub fn prompt_agent(&mut self, target: &str, text: &str) -> Result<()> {
        self.request("agent.prompt", json!({ "target": target, "text": text }))?;
        Ok(())
    }

    /// Interrupt a running agent the way its own Escape key does: inject the
    /// key so pi's `app.interrupt` aborts the current run.
    pub fn interrupt_agent(&mut self, target: &str) -> Result<()> {
        self.request(
            "agent.keys",
            json!({ "target": target, "keys": ["escape"] }),
        )?;
        Ok(())
    }

    /// The current status-bar widgets.
    pub fn bar_widgets(&mut self) -> Result<Vec<BarWidget>> {
        let result = self.request("ui.bar.list", json!({}))?;
        let widgets = result
            .get("widgets")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        Ok(widgets.iter().filter_map(BarWidget::from_json).collect())
    }

    /// The tabs of the active workspace, enriched with each tab's terminal
    /// cwd and running process from `session.snapshot`.
    pub fn tabs(&mut self) -> Result<Vec<Tab>> {
        let result = self.request("tab.list", json!({}))?;
        let mut tabs: Vec<Tab> = result
            .get("tabs")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(Tab::from_json)
            .collect();
        let snapshot = self.request("session.snapshot", json!({}))?;
        enrich_tab_runtime(&mut tabs, &snapshot);
        Ok(tabs)
    }

    /// The automation definitions and their armed count.
    pub fn automation_summary(&mut self) -> Result<AutomationSummary> {
        let result = self.request("automation.list", json!({}))?;
        Ok(AutomationSummary::from_json(&result))
    }

    /// The tasks and their open count.
    pub fn task_summary(&mut self) -> Result<TaskSummary> {
        let result = self.request("task.list", json!({}))?;
        Ok(TaskSummary::from_json(&result))
    }

    /// Focus the tab at `index` (1-based).
    pub fn focus_tab(&mut self, index: u16) -> Result<()> {
        self.request("tab.focus", json!({ "tab": index.to_string() }))?;
        Ok(())
    }

    /// Open a new tab in the active workspace.
    pub fn new_tab(&mut self) -> Result<()> {
        self.request("tab.new", json!({}))?;
        Ok(())
    }

    /// Close the tab at `index` (1-based).
    pub fn close_tab(&mut self, index: u16) -> Result<()> {
        self.request("tab.close", json!({ "tab": index.to_string() }))?;
        Ok(())
    }

    /// Where `pane` sits in its tab, from `pane.layout`.
    pub fn pane_layout(&mut self, pane: &str) -> Result<PaneFrame> {
        let result = self.request("pane.layout", json!({ "pane": pane }))?;
        PaneFrame::from_json(&result)
            .ok_or_else(|| Error::Control("pane.layout: malformed rect".to_string()))
    }

    /// Split the focused pane; `direction` is `right`, `down`, or `auto`.
    pub fn split_pane(&mut self, direction: &str) -> Result<()> {
        self.request("pane.split", json!({ "direction": direction }))?;
        Ok(())
    }

    /// The theme registry and the selected theme.
    pub fn appearance(&mut self) -> Result<Appearance> {
        let result = self.request("theme.list", json!({}))?;
        let themes = result
            .get("themes")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let themes: Vec<ThemeInfo> = themes.iter().filter_map(ThemeInfo::from_json).collect();
        let active = themes
            .iter()
            .find(|theme| theme.active)
            .map(|theme| theme.id.clone())
            .unwrap_or_default();
        Ok(Appearance { themes, active })
    }

    /// Select and persist a theme by id.
    pub fn select_theme(&mut self, id: &str) -> Result<()> {
        self.request("theme.use", json!({ "id": id }))?;
        Ok(())
    }
}

/// Read one newline-terminated line without reading past it.
///
/// A `BufReader` would be simpler but it owns the borrowed stream and buffers
/// bytes a later request could need; the control protocol is small, so a byte
/// loop is the honest fit.
fn read_line(stream: &Stream) -> Result<String> {
    let mut reader = stream;
    let mut bytes = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let read = reader.read(&mut byte)?;
        if read == 0 || byte[0] == b'\n' {
            break;
        }
        bytes.push(byte[0]);
    }
    String::from_utf8(bytes)
        .map_err(|error| Error::Control(format!("control response was not UTF-8: {error}")))
}

fn read_line_within(stream: Arc<Stream>, deadline: Duration) -> Result<String> {
    let (done_tx, done_rx) = bounded::<()>(0);
    let timed_out = Arc::new(AtomicBool::new(false));
    let watchdog_stream = Arc::clone(&stream);
    let watchdog_timed_out = Arc::clone(&timed_out);
    thread::Builder::new()
        .name("pilus-control-timeout".to_string())
        .spawn(move || {
            if matches!(
                done_rx.recv_timeout(deadline),
                Err(crossbeam_channel::RecvTimeoutError::Timeout)
            ) {
                watchdog_timed_out.store(true, Ordering::Release);
                watchdog_stream.terminate();
            }
        })
        .map_err(Error::Io)?;

    let response = read_line(&stream);
    drop(done_tx);
    if timed_out.load(Ordering::Acquire) {
        return Err(Error::Control("timed out".to_string()));
    }
    response
}

impl BarWidget {
    fn from_json(value: &Value) -> Option<Self> {
        let content = value.get("content").and_then(Value::as_array)?;
        Some(Self {
            key: value
                .get("key")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            region: value
                .get("region")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            content: content.iter().map(BarSegment::from_json).collect(),
        })
    }
}

impl BarSegment {
    fn from_json(value: &Value) -> Self {
        let separator = value.get("type").and_then(Value::as_str) == Some("separator");
        Self {
            text: if separator {
                String::new()
            } else {
                value
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            },
            tone: value
                .get("tone")
                .and_then(Value::as_str)
                .unwrap_or("normal")
                .to_string(),
            separator,
        }
    }
}

enum Command {
    FocusPane(String),
    FocusTab(u16),
    NewTab,
    CloseTab(u16),
    Split(String),
}

/// A background feed that keeps a fresh snapshot and forwards commands.
pub struct AgentFeed {
    commands: Sender<Command>,
    updates: Receiver<FeedUpdate>,
}

/// How much chrome a feed refreshes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FeedScope {
    /// Agents, bar, tabs, automations, and tasks: the visible surface.
    Full,
    /// Agents, bar, and tabs: a connected endpoint that is not on screen, so
    /// its agent list stays fresh and a later switch lands on current chrome
    /// instead of a stale or empty strip.
    Agents,
}

/// Reconciliation cadence for a full local feed, driven by events.
///
/// Events drive normal updates; this only covers state a structural event does
/// not describe (the status bar's mode, for one) and any missed event.
const RECONCILE: Duration = Duration::from_secs(10);

/// Reconciliation cadence for a remote feed, which has no event stream and so
/// cannot wait ten seconds for a change to surface.
const RECONCILE_REMOTE: Duration = Duration::from_secs(5);

/// Reconciliation cadence for a hidden local endpoint's feed.
const RECONCILE_AGENTS: Duration = Duration::from_secs(3);

/// The agents-only lane an active feed gets beside its snapshot tick. The
/// pin that replaces a pane's terminal rides `agent.list`; over SSH a full
/// refresh is five proxied requests — too heavy at a focus-chasing cadence —
/// but `agent.list` alone is one, and locally it is a trivial socket read.
const RECONCILE_AGENTS_FAST: Duration = Duration::from_millis(1200);

/// How long to wait before resubscribing after the event stream ends.
const RESUBSCRIBE: Duration = Duration::from_millis(500);

impl AgentFeed {
    /// Start the feed for `socket`, reconnecting after failures.
    pub fn spawn(socket: PathBuf) -> Self {
        Self::spawn_transport(ControlTransport::Socket(socket), false, FeedScope::Full)
    }

    /// Start a snapshot-only feed for a remote machine.
    ///
    /// `luvus uhp proxy` carries one request per SSH invocation, so an event
    /// subscription cannot be held; the feed reconciles on a short timer
    /// instead of reacting to structural events.
    pub fn spawn_ssh(target: SshTarget) -> Self {
        Self::spawn_transport(ControlTransport::Ssh(target), true, FeedScope::Full)
    }

    /// Start a hidden endpoint's feed: its agents plus the chrome a later
    /// switch will show, so activating it lands on current state.
    pub fn spawn_agents(socket: PathBuf) -> Self {
        Self::spawn_transport(ControlTransport::Socket(socket), true, FeedScope::Agents)
    }

    /// Start a hidden remote machine's feed.
    pub fn spawn_ssh_agents(target: SshTarget) -> Self {
        Self::spawn_transport(ControlTransport::Ssh(target), true, FeedScope::Agents)
    }

    fn spawn_transport(transport: ControlTransport, poll_only: bool, scope: FeedScope) -> Self {
        let (commands, command_rx) = unbounded();
        let (pokes, pokes_rx) = unbounded::<()>();
        let (update_tx, updates) = unbounded();
        // Shared so a burst of structural events (one tab open emits several)
        // forwards a single snapshot instead of a redraw per event.
        let last: Arc<Mutex<Option<FeedUpdate>>> = Arc::new(Mutex::new(None));
        let reconcile_updates = update_tx.clone();
        let reconcile_last = Arc::clone(&last);
        let reconcile_transport = transport.clone();
        let interval = match (&scope, &transport) {
            // A hidden remote feed reads three snapshots over one SSH child
            // each, so it earns the slower cadence of the other remote polls.
            (FeedScope::Agents, ControlTransport::Ssh(_)) => RECONCILE_REMOTE,
            (FeedScope::Agents, _) => RECONCILE_AGENTS,
            (FeedScope::Full, _) if poll_only => RECONCILE_REMOTE,
            (FeedScope::Full, _) => RECONCILE,
        };
        thread::Builder::new()
            .name("pilus-reconcile".to_string())
            .spawn(move || {
                run_reconcile(
                    reconcile_transport,
                    scope,
                    interval,
                    pokes_rx,
                    reconcile_updates,
                    reconcile_last,
                )
            })
            .ok();
        let command_transport = transport.clone();
        thread::Builder::new()
            .name("pilus-commands".to_string())
            .spawn(move || run_commands(command_transport, command_rx, pokes))
            .ok();
        // Only the full local feed holds an event subscription; a hidden
        // endpoint's feed reconciles on its timer instead.
        if scope == FeedScope::Full
            && let ControlTransport::Socket(socket) = transport
        {
            thread::Builder::new()
                .name("pilus-events".to_string())
                .spawn(move || run_events(socket, update_tx, last))
                .ok();
        }
        Self { commands, updates }
    }

    /// Return the latest snapshot, if a newer one has arrived.
    pub fn try_recv(&self) -> Option<FeedUpdate> {
        self.updates.try_recv().ok()
    }

    /// Ask the daemon to focus this agent's pane.
    pub fn focus_pane(&self, pane: &str) {
        let _ = self.commands.send(Command::FocusPane(pane.to_string()));
    }

    /// Ask the daemon to focus the tab at `index`.
    pub fn focus_tab(&self, index: u16) {
        let _ = self.commands.send(Command::FocusTab(index));
    }

    /// Ask the daemon to open a new tab.
    pub fn new_tab(&self) {
        let _ = self.commands.send(Command::NewTab);
    }

    /// Ask the daemon to close the tab at `index`.
    pub fn close_tab(&self, index: u16) {
        let _ = self.commands.send(Command::CloseTab(index));
    }

    /// Ask the daemon to split the focused pane (`right`/`down`/`auto`).
    pub fn split(&self, direction: &str) {
        let _ = self.commands.send(Command::Split(direction.to_string()));
    }
}

/// Service queued commands on their own thread, then poke the reconcile
/// thread once.
///
/// A command must never wait behind an in-flight refresh: a remote refresh
/// is a full SSH round trip, so a click arriving mid-refresh would look
/// unresponsive. Draining a queued burst into one reconcile also keeps rapid
/// tab clicks to a single snapshot read.
fn run_commands(transport: ControlTransport, commands: Receiver<Command>, pokes: Sender<()>) {
    while let Ok(command) = commands.recv() {
        let mut client = ControlClient::from_transport(transport.clone());
        serve(&mut client, command);
        // A burst of queued commands shares one reconcile pass.
        while let Ok(extra) = commands.try_recv() {
            serve(&mut client, extra);
        }
        // A command is a state change; reconcile reads it back.
        pokes.send(()).ok();
    }
}

/// Run one command against `client`, reporting nothing: a failed command is
/// covered by the reconcile pass that follows it.
fn serve(client: &mut ControlClient, command: Command) {
    let _ = match command {
        Command::FocusPane(pane) => client.focus_pane(&pane),
        Command::FocusTab(index) => client.focus_tab(index),
        Command::NewTab => client.new_tab(),
        Command::CloseTab(index) => client.close_tab(index),
        Command::Split(direction) => client.split_pane(&direction),
    };
}

/// Keep the snapshot fresh: once immediately, then after every poke (a
/// command just changed state) or tick (a structural event no event stream
/// described, and any the stream missed).
fn run_reconcile(
    transport: ControlTransport,
    scope: FeedScope,
    interval: Duration,
    pokes: Receiver<()>,
    updates: Sender<FeedUpdate>,
    last: Arc<Mutex<Option<FeedUpdate>>>,
) {
    // Publish one snapshot immediately; remote feeds have no event stream to
    // trigger the first refresh.
    if !refresh(&transport, scope, &updates, &last) {
        return;
    }
    let ticker = tick(interval);
    // An active feed gets an agents-only lane beside the snapshot tick: a
    // focused pi's pin rides `agent.list`. Remote feeds have no event stream
    // at all, and a local focus change may not emit one either — five
    // proxied requests are too heavy at the cadence a pane switch needs,
    // but one cheap `agent.list` is not. Hidden feeds keep their own
    // cadence: nobody is looking at them.
    let agents_ticker = match &scope {
        FeedScope::Full => tick(RECONCILE_AGENTS_FAST),
        _ => never(),
    };
    loop {
        select! {
            // A dead poke channel means the command thread — and with it the
            // feed — is gone; exiting here also keeps a disconnected channel
            // (always ready) from spinning this loop.
            recv(pokes) -> result => {
                if result.is_err() {
                    return;
                }
            }
            recv(ticker) -> _ => {},
            recv(agents_ticker) -> _ => {
                if !refresh_agents(&transport, &updates, &last) {
                    break;
                }
                continue;
            }
        }
        if !refresh(&transport, scope, &updates, &last) {
            break;
        }
    }
}

/// Refresh only the agent list and merge it into the held snapshot, so the
/// update stays whole for the GUI. One proxied request where a full pass is
/// five; a failed read keeps the last good snapshot, same as [`refresh`].
fn refresh_agents(
    transport: &ControlTransport,
    updates: &Sender<FeedUpdate>,
    last: &Mutex<Option<FeedUpdate>>,
) -> bool {
    let Some(agents) = ControlClient::from_transport(transport.clone())
        .agents()
        .ok()
    else {
        return true;
    };
    let Ok(mut guard) = last.lock() else {
        return true;
    };
    let Some(snapshot) = guard.as_mut() else {
        // Nothing published yet; the opening refresh lands one momentarily.
        return true;
    };
    if snapshot.agents == agents {
        return true;
    }
    snapshot.agents = agents;
    updates.send(snapshot.clone()).is_ok()
}

/// Hold an event subscription, refreshing the snapshot on structural changes.
fn run_events(socket: PathBuf, updates: Sender<FeedUpdate>, last: Arc<Mutex<Option<FeedUpdate>>>) {
    loop {
        let mut uhp = crate::uhp::UhpClient::connect(&socket);
        let Ok(mut stream) = uhp.subscribe() else {
            thread::sleep(RESUBSCRIBE);
            continue;
        };
        if !refresh(
            &ControlTransport::Socket(socket.clone()),
            FeedScope::Full,
            &updates,
            &last,
        ) {
            break;
        }
        loop {
            match stream.next_event() {
                Ok(Some(event)) if is_structural(&event.name) => {
                    if !refresh(
                        &ControlTransport::Socket(socket.clone()),
                        FeedScope::Full,
                        &updates,
                        &last,
                    ) {
                        return;
                    }
                }
                // Resync means the subscription lost events: resubscribe.
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        thread::sleep(RESUBSCRIBE);
    }
}

/// Re-read every snapshot concurrently and forward the combined result.
///
/// The full scope's reads are independent one-request connections; issuing
/// them in parallel turns the chrome-refresh latency from the sum of five
/// round trips into the slowest one, which is what keeps the tab strip in step
/// with a tab switch. `false` means the UI is gone.
fn refresh(
    transport: &ControlTransport,
    scope: FeedScope,
    updates: &Sender<FeedUpdate>,
    last: &Mutex<Option<FeedUpdate>>,
) -> bool {
    let Some(update) = fetch_update(transport, scope) else {
        return true;
    };
    let Ok(mut guard) = last.lock() else {
        return true;
    };
    if guard.as_ref() == Some(&update) {
        return true;
    }
    *guard = Some(update.clone());
    drop(guard);
    updates.send(update).is_ok()
}

/// Read the requested snapshots in parallel.
///
/// `None` means at least one read failed; the caller keeps the last good
/// snapshot and retries on the next event or reconciliation tick.
fn fetch_update(transport: &ControlTransport, scope: FeedScope) -> Option<FeedUpdate> {
    if scope == FeedScope::Agents {
        // A hidden endpoint caches the chrome the swap will show (tabs, the
        // status bar) alongside its agents, so switching to it never lands on
        // a stale or empty strip while its full feed warms up.
        return thread::scope(|threads| -> Option<FeedUpdate> {
            let agents =
                threads.spawn(|| ControlClient::from_transport(transport.clone()).agents());
            let tabs = threads.spawn(|| ControlClient::from_transport(transport.clone()).tabs());
            let bar =
                threads.spawn(|| ControlClient::from_transport(transport.clone()).bar_widgets());
            Some(FeedUpdate {
                agents: agents.join().ok()?.ok()?,
                tabs: tabs.join().ok()?.ok()?,
                bar: bar.join().ok()?.ok()?,
                ..FeedUpdate::default()
            })
        });
    }
    thread::scope(|threads| -> Option<FeedUpdate> {
        let agents = threads.spawn(|| ControlClient::from_transport(transport.clone()).agents());
        let bar = threads.spawn(|| ControlClient::from_transport(transport.clone()).bar_widgets());
        let tabs = threads.spawn(|| ControlClient::from_transport(transport.clone()).tabs());
        let automations =
            threads.spawn(|| ControlClient::from_transport(transport.clone()).automation_summary());
        let tasks =
            threads.spawn(|| ControlClient::from_transport(transport.clone()).task_summary());
        Some(FeedUpdate {
            agents: agents.join().ok()?.ok()?,
            bar: bar.join().ok()?.ok()?,
            tabs: tabs.join().ok()?.ok()?,
            automations: automations.join().ok()?.ok()?,
            tasks: tasks.join().ok()?.ok()?,
        })
    })
}

/// Whether an event can change what the native chrome shows.
///
/// `terminal.output_ready` fires once per output burst and is excluded: acting
/// on it would be a poll with extra steps.
fn is_structural(name: &str) -> bool {
    name != "terminal.output_ready"
        && (name.starts_with("agent.")
            || name.starts_with("pane.")
            || name.starts_with("tab.")
            || name.starts_with("workspace.")
            || name.starts_with("automation.")
            || name.starts_with("task.")
            || name.starts_with("terminal."))
}

#[cfg(test)]
mod tests {
    use super::{
        Agent, AutomationSummary, PaneFrame, Tab, TaskSummary, enrich_tab_runtime, is_structural,
        read_line_within, worktree_blockers,
    };
    use crate::error::Error;
    use crate::transport::Stream;
    use serde_json::json;
    use std::process::Command;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[test]
    fn pane_frame_maps_the_logical_rect_to_fractions() {
        let whole = json!({
            "logical_size": { "width": 10000, "height": 10000 },
            "rect": { "x": 0, "y": 0, "width": 10000, "height": 10000 }
        });
        assert!(PaneFrame::from_json(&whole).expect("frame").is_full());
        let right = json!({
            "logical_size": { "width": 10000, "height": 10000 },
            "rect": { "x": 5000, "y": 0, "width": 5000, "height": 10000 }
        });
        let frame = PaneFrame::from_json(&right).expect("frame");
        assert!(!frame.is_full());
        assert!((frame.x - 0.5).abs() < f32::EPSILON);
        assert!((frame.width - 0.5).abs() < f32::EPSILON);
        // A degenerate area is no layout at all.
        let empty = json!({
            "logical_size": { "width": 0, "height": 0 },
            "rect": { "x": 0, "y": 0, "width": 0, "height": 0 }
        });
        assert_eq!(PaneFrame::from_json(&empty), None);
    }

    #[test]
    fn agent_parses_the_reported_session() {
        let reported = Agent::from_json(&json!({
            "pane": "7",
            "agent": "pi",
            "session": "c97a2b18-3c8d-4e2f-9a1b-0d2f4a6b8c10",
        }))
        .expect("agent");
        assert_eq!(
            reported.session.as_deref(),
            Some("c97a2b18-3c8d-4e2f-9a1b-0d2f4a6b8c10")
        );
        let silent = Agent::from_json(&json!({
            "pane": "7",
            "agent": "pi",
            "session": null,
        }))
        .expect("agent");
        assert_eq!(silent.session, None);
        // An agent that never reports omits the key or sends an empty one.
        let missing = Agent::from_json(&json!({"pane": "7", "agent": "pi"})).expect("agent");
        assert_eq!(missing.session, None);
        let empty =
            Agent::from_json(&json!({"pane": "7", "agent": "pi", "session": ""})).expect("agent");
        assert_eq!(empty.session, None);
    }

    #[test]
    fn busy_worktree_refusals_expose_their_blockers() {
        let busy = Error::Control("worktree_in_use: agent pi is working in pane 7".to_string());
        assert_eq!(
            worktree_blockers(&busy),
            Some("agent pi is working in pane 7".to_string())
        );
        let bare = Error::Control("worktree_in_use".to_string());
        assert_eq!(
            worktree_blockers(&bare),
            Some("it is still in use".to_string())
        );
        let other = Error::Control("git_error: fatal: not a working tree".to_string());
        assert_eq!(worktree_blockers(&other), None);
        assert_eq!(worktree_blockers(&Error::Control("timed out".into())), None);
    }

    #[test]
    fn tabs_enrich_from_the_active_workspace_snapshot() {
        let mut tabs = vec![
            Tab {
                index: 1,
                ..Tab::default()
            },
            Tab {
                index: 2,
                ..Tab::default()
            },
            Tab {
                index: 3,
                ..Tab::default()
            },
        ];
        let snapshot = json!({
            "workspaces": [
                { "cwd": "/other", "active": false, "tabs": [] },
                {
                    "cwd": "/repo",
                    "active": true,
                    "tabs": [
                        {
                            "index": 1,
                            "panes": [
                                { "kind": "terminal", "cwd": "/repo/api", "agent": "fish", "focused": false },
                                { "kind": "terminal", "cwd": "/repo/web", "agent": "node", "focused": true }
                            ]
                        },
                        {
                            "index": 2,
                            "panes": [
                                { "kind": "terminal", "cwd": "/repo/docs", "agent": "devin", "focused": false }
                            ]
                        },
                        {
                            "index": 3,
                            "panes": [
                                { "kind": "git" }
                            ]
                        }
                    ]
                }
            ]
        });
        enrich_tab_runtime(&mut tabs, &snapshot);
        // The focused terminal wins over the first one.
        assert_eq!(tabs[0].cwd, "/repo/web");
        assert_eq!(tabs[0].process, "node");
        // No focused pane falls back to the first terminal.
        assert_eq!(tabs[1].cwd, "/repo/docs");
        assert_eq!(tabs[1].process, "devin");
        // A native-view tab (no terminal pane) shows the workspace cwd.
        assert_eq!(tabs[2].cwd, "/repo");
        assert_eq!(tabs[2].process, "");
    }

    #[test]
    fn structural_events_exclude_output_ready() {
        assert!(is_structural("tab.created"));
        assert!(is_structural("pane.agent_status_changed"));
        assert!(is_structural("terminal.metadata_changed"));
        assert!(is_structural("workspace.moved"));
        assert!(is_structural("agent.hook"));
        assert!(is_structural("automation.run_started"));
        assert!(is_structural("task.created"));
        assert!(!is_structural("terminal.output_ready"));
        assert!(!is_structural("events.resync_required"));
    }

    #[test]
    fn automation_summary_counts_armed() {
        let value = json!({
            "automations": [
                {"name": "nightly"},
                {"name": "paused", "disabled": true},
                {"name": "legacy", "enabled": false},
            ]
        });
        assert_eq!(
            AutomationSummary::from_json(&value),
            AutomationSummary {
                enabled: 1,
                total: 3
            }
        );
        assert_eq!(
            AutomationSummary::from_json(&json!({})),
            AutomationSummary::default()
        );
    }

    #[test]
    fn task_summary_counts_open() {
        let value = json!({
            "tasks": [
                {"title": "wip", "status": "working"},
                {"title": "queued"},
                {"title": "finished", "status": "done"},
                {"title": "dropped", "status": "cancelled"},
            ]
        });
        assert_eq!(
            TaskSummary::from_json(&value),
            TaskSummary { open: 2, total: 4 }
        );
    }

    #[test]
    fn remote_control_read_times_out_and_terminates_stream() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 30"]);
        let stream = Arc::new(Stream::spawn(&mut command).expect("spawn stream"));
        let started = Instant::now();
        let result = read_line_within(stream, Duration::from_millis(200));
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
