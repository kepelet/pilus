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
//! and re-reads the three snapshots only when a structural event arrives, plus
//! a slow reconciliation so a missed event cannot leave the chrome stale.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, unbounded};
use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::transport::Stream;

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

/// One poll of the live agent list and the status-bar widgets.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FeedUpdate {
    /// Current live agent sessions.
    pub agents: Vec<Agent>,
    /// Current status-bar widgets.
    pub bar: Vec<BarWidget>,
    /// Current tabs of the active workspace.
    pub tabs: Vec<Tab>,
}

/// One live agent session as reported by `agent.list`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Agent {
    /// The pane the agent runs in.
    pub pane: String,
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
}

impl Agent {
    fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            pane: value.get("pane")?.as_str()?.to_string(),
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

/// A synchronous request/response connection to the control socket.
///
/// The daemon's control socket answers exactly one request per connection, so
/// each call opens a fresh connection rather than reusing one.
pub struct ControlClient {
    socket: PathBuf,
    next_id: u64,
}

impl ControlClient {
    /// Connect to the control socket at `path`.
    pub fn connect(path: &Path) -> Result<Self> {
        Ok(Self {
            socket: path.to_path_buf(),
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
        let stream = Stream::connect(&self.socket)?;
        {
            let mut writer = &stream;
            writer.write_all(line.as_bytes())?;
            writer.write_all(b"\n")?;
            writer.flush()?;
        }

        let response = read_line(&stream)?;
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

    /// Focus the pane an agent runs in, jumping to its workspace and tab.
    pub fn focus_pane(&mut self, pane: &str) -> Result<()> {
        self.request("pane.focus", json!({ "pane": pane }))?;
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

    /// The tabs of the active workspace.
    pub fn tabs(&mut self) -> Result<Vec<Tab>> {
        let result = self.request("tab.list", json!({}))?;
        let tabs = result
            .get("tabs")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        Ok(tabs.iter().filter_map(Tab::from_json).collect())
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
}

/// A background feed that keeps a fresh snapshot and forwards commands.
pub struct AgentFeed {
    commands: Sender<Command>,
    updates: Receiver<FeedUpdate>,
}

/// How long the feed waits for a command before reconciling on its own.
///
/// Events drive normal updates; this only covers state a structural event does
/// not describe (the status bar's mode, for one) and any missed event.
const RECONCILE: Duration = Duration::from_secs(10);

/// How long to wait before resubscribing after the event stream ends.
const RESUBSCRIBE: Duration = Duration::from_millis(500);

impl AgentFeed {
    /// Start the feed for `socket`, reconnecting after failures.
    pub fn spawn(socket: PathBuf) -> Self {
        let (commands, command_rx) = unbounded();
        let (update_tx, updates) = unbounded();
        // Shared so a burst of structural events (one tab open emits several)
        // forwards a single snapshot instead of a redraw per event.
        let last: Arc<Mutex<Option<FeedUpdate>>> = Arc::new(Mutex::new(None));
        let command_updates = update_tx.clone();
        let command_last = Arc::clone(&last);
        let command_socket = socket.clone();
        thread::Builder::new()
            .name("l3s-commands".to_string())
            .spawn(move || run_commands(command_socket, command_rx, command_updates, command_last))
            .ok();
        thread::Builder::new()
            .name("l3s-events".to_string())
            .spawn(move || run_events(socket, update_tx, last))
            .ok();
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
}

/// Service one command, then reconcile because a command is a state change.
fn run_commands(
    socket: PathBuf,
    commands: Receiver<Command>,
    updates: Sender<FeedUpdate>,
    last: Arc<Mutex<Option<FeedUpdate>>>,
) {
    let mut client: Option<ControlClient> = None;
    loop {
        match commands.recv_timeout(RECONCILE) {
            Ok(command) => {
                if client.is_none() {
                    client = ControlClient::connect(&socket).ok();
                }
                if let Some(active) = client.as_mut() {
                    let outcome = match command {
                        Command::FocusPane(pane) => active.focus_pane(&pane),
                        Command::FocusTab(index) => active.focus_tab(index),
                        Command::NewTab => active.new_tab(),
                        Command::CloseTab(index) => active.close_tab(index),
                    };
                    if outcome.is_err() {
                        client = None;
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if !refresh(&socket, &mut client, &updates, &last) {
            break;
        }
    }
}

/// Hold an event subscription, refreshing the snapshot on structural changes.
fn run_events(socket: PathBuf, updates: Sender<FeedUpdate>, last: Arc<Mutex<Option<FeedUpdate>>>) {
    let mut client: Option<ControlClient> = None;
    loop {
        let mut uhp = crate::uhp::UhpClient::connect(&socket);
        let Ok(mut stream) = uhp.subscribe() else {
            thread::sleep(RESUBSCRIBE);
            continue;
        };
        if !refresh(&socket, &mut client, &updates, &last) {
            break;
        }
        loop {
            match stream.next_event() {
                Ok(Some(event)) if is_structural(&event.name) => {
                    if !refresh(&socket, &mut client, &updates, &last) {
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

/// Re-read the three snapshots and forward them. `false` means the UI is gone.
fn refresh(
    socket: &Path,
    client: &mut Option<ControlClient>,
    updates: &Sender<FeedUpdate>,
    last: &Mutex<Option<FeedUpdate>>,
) -> bool {
    if client.is_none() {
        *client = ControlClient::connect(socket).ok();
    }
    let Some(active) = client.as_mut() else {
        return true;
    };
    let snapshot = (active.agents(), active.bar_widgets(), active.tabs());
    let (agents, bar, tabs) = match snapshot {
        (Ok(agents), Ok(bar), Ok(tabs)) => (agents, bar, tabs),
        _ => {
            *client = None;
            return true;
        }
    };
    let update = FeedUpdate { agents, bar, tabs };
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
            || name.starts_with("terminal."))
}

#[cfg(test)]
mod tests {
    use super::is_structural;

    #[test]
    fn structural_events_exclude_output_ready() {
        assert!(is_structural("tab.created"));
        assert!(is_structural("pane.agent_status_changed"));
        assert!(is_structural("terminal.metadata_changed"));
        assert!(is_structural("workspace.moved"));
        assert!(is_structural("agent.hook"));
        assert!(!is_structural("terminal.output_ready"));
        assert!(!is_structural("events.resync_required"));
        assert!(!is_structural("automation.run_started"));
    }
}
