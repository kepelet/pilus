//! The native Pi Session panel.
//!
//! Pi renders its conversation through a terminal, but persists it as JSONL.
//! This panel paints that conversation natively inside the main window — user
//! turns on the right, assistant turns on the left, and thinking, tool calls,
//! tool results, and shell runs as collapsed cards — folding the session with
//! `pilus-session` and following it live. A floating composer sends prompts
//! back to the live agent through the endpoint's control surface.
//!
//! The session comes from the id the pane's pi extension reported to the
//! daemon, then its bytes are read through a [`Source`]: a local file, or an
//! SSH `tail` for a machine. All reads, lookups, and prompt sends happen on
//! background threads, so the GUI thread only ever applies item deltas to a
//! folded [`Transcript`].

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui::{
    Anchor, AnyElement, Context, Div, ElementId, Entity, FollowMode, Keystroke, ListAlignment,
    ListOffset, ListState, Stateful, anchored, deferred, div, list, point, prelude::*, px, relative,
};
use pilus_client::{AgentModel, ControlClient, SshTarget};
use pilus_session::{
    ContextUsage, DeliverAs, Error as SessionError, Item, ItemKind, PiState, RpcCommand, RpcEvent,
    Sidecar, Source, Tail, ToolEvent, ToolOutcome, ToolPhase, Transcript, TranscriptMeta, Usage,
    agent_dir, live_items, models_from, parse_event, socket_path,
};
use serde_json::Value;

use crate::settings::Settings;

use super::PilusView;
use super::style::{self, Fonts};
use super::text_input::TextInput;
use super::theme::Theme;

/// Lines kept in an expanded card, so a 50 KB tool dump cannot drown the
/// window.
const EXPANDED_LINES: usize = 400;
/// Widest the transcript column grows. A reading measure stops improving
/// past this, so wider windows center the column instead of stretching it.
const TRANSCRIPT_MAX_W: f32 = 760.0;
/// The composer runs wider than the transcript: the prompt is the page's
/// anchor, not one entry in its column.
const COMPOSER_MAX_W: f32 = 880.0;
/// The jump buttons hang beside the composer, so the dock reserves that
/// much on both sides and the panel itself stays centered in the pane.
const JUMP_GUTTER: f32 = 38.0;
/// Air between the panel and the buttons, so the cluster never touches it.
const JUMP_GAP: f32 = 8.0;
/// How often a local session is re-read.
const LOCAL_POLL: Duration = Duration::from_millis(300);
/// How often a remote session is re-read; every poll is an SSH round trip.
const REMOTE_POLL: Duration = Duration::from_millis(1500);

/// How a collapsed section's body renders when opened.
#[derive(Clone, Copy)]
enum Section {
    /// Markdown prose at the given scale (thinking uses a smaller one).
    Markdown(Fonts),
    /// Monospace machine text: tool calls, results, and shell runs.
    Mono,
}

/// What a section header's click toggles.
#[derive(Clone, Copy)]
enum Toggle {
    /// One section, keyed by its item index.
    Item(usize),
    /// A run of tool calls, keyed by its first item index.
    Group(usize),
    /// A run of work between assistant replies, keyed by its first item index.
    Work(usize),
}

/// One band of the composer's dock: the reservation that keeps the panel
/// centered in the pane, then that band's content in the panel's place.
///
/// The row grows rightward, so a band that is not the panel leaves the
/// gutter to whatever shares its row.
fn dock_slot(content: impl IntoElement) -> Div {
    div()
        .flex()
        .flex_row()
        .items_start()
        .gap(px(JUMP_GAP))
        .child(div().w(px(JUMP_GUTTER - JUMP_GAP)).flex_shrink_0())
        .child(div().flex_1().min_w(px(0.0)).child(content))
}

/// Which composer picker is open, if any.
#[derive(Clone, Copy, PartialEq)]
enum PickerKind {
    Model,
    Thinking,
}

/// Thinking levels pi accepts for `/thinking`, from its settings reference.
const THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];
/// How long after the last file growth the session still counts as active.
/// Past this, a trailing work item is a stale tail (interrupted session),
/// not live work. Generous: quiet stretches (a long generation writes
/// nothing until it completes) must not flip a live turn to idle.
const ACTIVE_WINDOW: Duration = Duration::from_secs(90);
/// Visible rows in a composer picker; longer lists scroll. Row metrics come
/// from the shared composer picker menu.
const PICKER_MAX_ROWS: usize = 8;

/// Where a session's bytes live, and how to reach its daemon.
#[derive(Clone)]
enum Endpoint {
    /// A file on this machine, with the control socket used to locate it.
    Local { api_socket: PathBuf, cwd: String },
    /// A machine, reached over its own SSH transport.
    Remote { target: SshTarget, cwd: String },
}

/// A changed span of the transcript plus the fold's other state. A
/// metadata-only change carries an empty `items` at `items.len()`, so the
/// window still refreshes its usage and model readouts without a full
/// transcript clone per tick. Boxed through [`Update`] because `meta`
/// dwarfs the other variants.
struct Delta {
    /// The lowest item index the update replaces.
    from: usize,
    /// The replacement items: `items[from..]` on the reader's fold.
    items: Vec<Item>,
    /// Every non-item field of the fold.
    meta: TranscriptMeta,
}

/// Which request an RPC `response` answers, so the panel can route the
/// result without re-parsing the command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequestKind {
    Prompt,
    Abort,
    SetModel,
    SetThinking,
    Models,
    State,
}

/// What the reader thread sends back to the window.
enum Update {
    Delta(Box<Delta>),
    /// The pi model catalog for the endpoint, for the model picker.
    Catalog(Vec<AgentModel>),
    /// A read or lookup failure, shown in place of the transcript.
    Error(String),
    /// The extension's RPC socket answered a hello.
    RpcUp,
    /// The RPC channel dropped; the file tail is the source of truth again.
    RpcDown,
    /// pi pushed a state snapshot.
    RpcState(Box<PiState>),
    /// pi's streaming partial message, already folded into items.
    RpcStream(Vec<Item>),
    /// The streaming turn ended; the file is about to hold it.
    RpcStreamEnd,
    /// A tool run started, produced output, or finished.
    RpcTool(ToolEvent),
    /// The reply to a command this window sent.
    RpcResult {
        kind: RequestKind,
        result: Result<Option<Value>, String>,
    },
}

/// Reader tails parked across panel switches, so returning to a session
/// reads only the bytes appended while it was away. Held in an `Arc` because
/// the reader thread takes a clone when it spawns; the key is the source
/// label — a local path, or `destination:path` for a remote file.
#[derive(Clone, Default)]
struct SessionCache {
    inner: Arc<Mutex<VecDeque<(String, Tail)>>>,
}

/// Tails kept open at once; switching further evicts the least recent.
const CACHED_TAILS: usize = 4;

impl SessionCache {
    /// Take the tail parked under `key`.
    fn take(&self, key: &str) -> Option<Tail> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let index = inner.iter().position(|(name, _)| name == key)?;
        inner.remove(index).map(|(_, tail)| tail)
    }

    /// Park a tail under `key`, most recent first, evicting the oldest when
    /// the cache is full.
    fn put(&self, key: &str, tail: Tail) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.retain(|(name, _)| name != key);
        inner.push_front((key.to_string(), tail));
        while inner.len() > CACHED_TAILS {
            inner.pop_back();
        }
    }
}

/// Which transcript rows were open in one session, restored when the panel
/// returns to it after looking at another.
#[derive(Clone, Default)]
struct ExpandState {
    expanded: HashSet<usize>,
    groups: HashSet<usize>,
    work_open: HashSet<usize>,
}

/// Expansion states remembered at once: enough to flip between panes without
/// keeping one entry for every session ever opened.
const EXPAND_STATE_CAP: usize = 16;

/// The turn pi is streaming right now, overlaid on the transcript until the
/// file catches up. `ended_at` marks `stream_end`: the file tail lags a
/// beat, so the overlay lingers until a delta lands or the linger expires.
#[derive(Default)]
struct LiveTurn {
    items: Vec<Item>,
    /// A running tool's `(name, output)`; absent between runs.
    tool: Option<(String, String)>,
    ended_at: Option<Instant>,
}

/// How long a finished live turn waits for the file to land before it is
/// dropped as stale.
const LIVE_LINGER: Duration = Duration::from_secs(3);

/// Item-index base for the live overlay, far past any real index, so expand
/// keys on the overlay never collide with the transcript's.
const LIVE_BASE: usize = 1_000_000;

/// Output lines a running tool shows, newest last.
const LIVE_TOOL_LINES: usize = 12;

/// Whether a live turn has outlived its welcome: the stream ended more than
/// `LIVE_LINGER` ago and no tool is still running.
fn live_expired(live: &LiveTurn, now: Instant) -> bool {
    live.tool.is_none()
        && live
            .ended_at
            .is_some_and(|at| now.duration_since(at) > LIVE_LINGER)
}

/// Where a composer submit goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    /// Over the extension socket, with the delivery pi wants for a busy turn.
    Rpc(Option<DeliverAs>),
    /// Through the pane's terminal prompt (`agent.prompt`).
    Terminal,
}

/// Route a submit: slash commands only exist in pi's TUI editor, and a
/// socket that is down leaves the terminal path as the only channel. Enter
/// steers a busy turn; Alt+Enter queues behind it.
fn route(text: &str, rpc_up: bool, busy: bool, alt: bool) -> Route {
    if text.starts_with('/') || !rpc_up {
        return Route::Terminal;
    }
    Route::Rpc(if busy {
        Some(if alt {
            DeliverAs::FollowUp
        } else {
            DeliverAs::Steer
        })
    } else {
        None
    })
}

/// The context chip's text: `ctx 42% · 84k/1M`, or `ctx –/1M` while pi has
/// not re-counted after a compaction.
fn ctx_label(usage: &ContextUsage) -> String {
    match usage.tokens {
        Some(tokens) => {
            let percent = usage
                .percent
                .map_or_else(
                    || {
                        let window = usage.context_window.max(1) as f64;
                        tokens as f64 / window * 100.0
                    },
                    |percent| percent * 100.0,
                )
                .round();
            format!(
                "ctx {percent:.0}% · {}/{}",
                fmt_count(tokens),
                fmt_count(usage.context_window)
            )
        }
        None => format!("ctx –/{}", fmt_count(usage.context_window)),
    }
}

/// Whether the context chip warns: at or above 80% full.
fn ctx_warns(usage: &ContextUsage) -> bool {
    usage.percent.unwrap_or(0.0) >= 0.8
}

/// What a transcript row is, for [`build_rows`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowKind {
    /// One item rendered on its own.
    Item,
    /// A run of thinking items, each collapsed on its own row.
    Thinking,
    /// A stretch of work collapsed under a `Working for …` header.
    Work,
}

/// One list entry: a contiguous range of transcript items.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Row {
    start: usize,
    end: usize,
    kind: RowKind,
}

/// Fold the transcript into list rows: a maximal run of `is_work` items is
/// one row (`Thinking` when it is only thinking, `Work` otherwise); every
/// other item is a row of its own.
fn build_rows(items: &[Item]) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut index = 0;
    while index < items.len() {
        let start = index;
        if is_work(&items[index].kind) {
            while index < items.len() && is_work(&items[index].kind) {
                index += 1;
            }
            let run = &items[start..index];
            let kind = if run
                .iter()
                .all(|item| matches!(item.kind, ItemKind::Thinking { .. }))
            {
                RowKind::Thinking
            } else {
                RowKind::Work
            };
            rows.push(Row {
                start,
                end: index,
                kind,
            });
        } else {
            index += 1;
            rows.push(Row {
                start,
                end: index,
                kind: RowKind::Item,
            });
        }
    }
    rows
}

/// A fresh list state anchored at the tail: the first layout walks up from
/// the end and only measures what is visible.
fn new_list_state() -> ListState {
    let list = ListState::new(0, ListAlignment::Top, px(600.0));
    list.set_follow_mode(FollowMode::Tail);
    list
}

/// The Pi Session panel's state, embedded in [`PilusView`].
pub struct SessionPanel {
    open: bool,
    /// The panel stands in for a pi pane's terminal: Escape does not close
    /// it, so there is no way to strand the pane behind the card.
    pinned: bool,
    title: String,
    error: String,
    /// The endpoint the session lives on, reused to prompt the live agent.
    endpoint: Option<Endpoint>,
    /// The live agent's pane id or alias, the target `agent.prompt` needs.
    target: Option<String>,
    /// A prompt is in flight.
    sending: bool,
    /// The composer's status line: sending, sent, or a failure.
    status: String,
    /// The window theme, set each frame before rendering.
    theme: Theme,
    /// Shared settings, for the Interface family and size.
    settings: Rc<RefCell<Settings>>,
    transcript: Transcript,
    /// The virtualized transcript: only the rows near the viewport are in
    /// the element tree.
    list: ListState,
    /// The rows the list was last told about.
    rows: Vec<Row>,
    /// Whether the list's last entry is the optimistic-prompt bubble.
    list_pending: bool,
    /// The transcript changed since the last [`SessionPanel::sync_list`].
    rows_dirty: bool,
    /// The pi model catalog for this endpoint, loaded once in the
    /// background; the picker falls back to session models until it lands.
    catalog: Vec<AgentModel>,
    /// Item indices the user has expanded from their collapsed default.
    expanded: HashSet<usize>,
    /// Group indices (a run of tool calls, keyed by its first item) expanded.
    groups: HashSet<usize>,
    /// Work groups (a stretch of thinking and tool calls between assistant
    /// replies, keyed by its first item) expanded.
    work_open: HashSet<usize>,
    /// Which composer picker is open, if any.
    picker: Option<PickerKind>,
    /// The picker an outside press just dismissed, so the following click on
    /// its own chip stays closed instead of reopening it.
    picker_dismissed: Option<PickerKind>,
    /// Whether the in-flight send clears the composer on success; picker
    /// commands (`/model`, `/thinking`) leave the draft alone.
    send_clears_input: bool,
    /// A sent prompt shown optimistically until pi appends it, with the
    /// transcript length at send time so a stale same-text match cannot
    /// clear it early.
    pending_prompt: Option<String>,
    pending_from: usize,
    /// Whether the floating prompts menu is open, and whether an outside
    /// press just dismissed it (so its own button's click stays closed).
    prompts_open: bool,
    prompts_dismissed: bool,
    /// A sent prompt is still waiting for pi's first responding content.
    /// The optimistic indicator persists through this, not just the send.
    awaiting_response: bool,
    /// The optimistic indicator reads `Steering…`: the prompt went out while
    /// pi was already working.
    pending_steering: bool,
    /// When the session file last grew. Delivery implies growth (the reader
    /// only forwards new bytes), so recency here means live work — unlike a
    /// trailing work item, which may be a stale interrupted tail.
    last_growth: Option<Instant>,
    /// Item count at the last delivery, so growth marks the session active.
    last_seen: usize,
    /// Earliest item index pending deltas rewrote, so `sync_list` remeasures
    /// a row an in-place change does not move the prefix diff to.
    changed_from: Option<usize>,
    /// Tails parked between switches, shared with the reader thread.
    cache: SessionCache,
    /// Expansion states saved under `expand_key` across switches.
    expand_cache: HashMap<String, ExpandState>,
    /// The key the current transcript's expansion saves under: the reported
    /// session id, else the explicit path, else the endpoint cwd.
    expand_key: Option<String>,
    /// Commands for the extension's socket; `Some` whenever a session id was
    /// reported, even while the channel itself is reconnecting.
    rpc_tx: Option<Sender<RpcCommand>>,
    /// Whether the socket is live right now; state pushes only count then.
    rpc_up: bool,
    /// pi's last reported state; `None` while RPC is down, so every
    /// consumer's `Option` check is the fallback switch. `pub(super)` so
    /// the shot harness can paint the context chip deterministically.
    pub(super) pi_state: Option<PiState>,
    /// The streaming turn overlaid on the transcript while the file lags.
    live: Option<LiveTurn>,
    /// Whether the list's last entry is the live overlay.
    list_live: bool,
    rx: Receiver<Update>,
    tx: Sender<Update>,
    send_rx: Receiver<Result<(), String>>,
    send_tx: Sender<Result<(), String>>,
    stop_rx: Receiver<Result<(), String>>,
    stop_tx: Sender<Result<(), String>>,
    /// Prompts waiting for the in-flight send to land, oldest first, each
    /// with whether it clears the composer. A second send queues instead of
    /// vanishing behind the `sending` guard.
    outbox: VecDeque<(String, bool)>,
    stop: Arc<AtomicBool>,
}

impl Default for SessionPanel {
    fn default() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let (send_tx, send_rx) = std::sync::mpsc::channel();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel();
        Self {
            open: false,
            pinned: false,
            title: String::new(),
            error: String::new(),
            endpoint: None,
            target: None,
            sending: false,
            status: String::new(),
            theme: Theme::default(),
            settings: Rc::new(RefCell::new(Settings::default())),
            transcript: Transcript::default(),
            list: new_list_state(),
            rows: Vec::new(),
            list_pending: false,
            rows_dirty: false,
            catalog: Vec::new(),
            picker: None,
            picker_dismissed: None,
            send_clears_input: true,
            pending_prompt: None,
            pending_from: 0,
            prompts_open: false,
            prompts_dismissed: false,
            awaiting_response: false,
            pending_steering: false,
            last_growth: None,
            expanded: HashSet::new(),
            groups: HashSet::new(),
            work_open: HashSet::new(),
            last_seen: 0,
            changed_from: None,
            cache: SessionCache::default(),
            expand_cache: HashMap::new(),
            expand_key: None,
            rpc_tx: None,
            rpc_up: false,
            pi_state: None,
            live: None,
            list_live: false,
            rx,
            tx,
            send_rx,
            send_tx,
            stop_rx,
            stop_tx,
            outbox: VecDeque::new(),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl SessionPanel {
    /// Whether the panel should paint in the card.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Open the panel for a local cwd (or an explicit session file).
    ///
    /// `--session-ui` only: an explicit file is read as-is, a cwd resolves
    /// to its newest session. Live panes go through [`Self::open_local_session`]
    /// with the id the extension reported.
    pub fn open_local(
        &mut self,
        api_socket: PathBuf,
        spec: String,
        title: String,
        target: Option<String>,
        cx: &mut Context<PilusView>,
    ) {
        // An explicit file bypasses discovery entirely.
        let explicit = PathBuf::from(&spec);
        let endpoint = if explicit.is_file() {
            Endpoint::Local {
                api_socket,
                cwd: String::new(),
            }
        } else {
            Endpoint::Local {
                api_socket,
                cwd: expand_local(&spec),
            }
        };
        self.open_with(
            endpoint,
            explicit.is_file().then_some(explicit),
            None,
            title,
            target,
            cx,
        );
    }

    /// Open the panel for a local pane whose session id the pi extension
    /// reported to the daemon.
    pub fn open_local_session(
        &mut self,
        api_socket: PathBuf,
        cwd: String,
        session_id: String,
        title: String,
        pane: String,
        cx: &mut Context<PilusView>,
    ) {
        self.open_with(
            Endpoint::Local { api_socket, cwd },
            None,
            Some(session_id),
            title,
            Some(pane),
            cx,
        );
    }

    /// Open the panel for a remote pane whose session id the pi extension
    /// reported to that machine's daemon.
    pub fn open_remote_session(
        &mut self,
        target: SshTarget,
        cwd: String,
        session_id: String,
        title: String,
        pane: String,
        cx: &mut Context<PilusView>,
    ) {
        self.open_with(
            Endpoint::Remote { target, cwd },
            None,
            Some(session_id),
            title,
            Some(pane),
            cx,
        );
    }

    /// Start a reader thread for `endpoint` and reset the panel's state.
    fn open_with(
        &mut self,
        endpoint: Endpoint,
        explicit: Option<PathBuf>,
        session_id: Option<String>,
        title: String,
        target: Option<String>,
        cx: &mut Context<PilusView>,
    ) {
        // Park the outgoing session's expansion under its key so a switch
        // back lands on the same open rows. `mem::take` clears the sets; the
        // generic clears below then cover a first open, where nothing was
        // parked.
        if let Some(key) = self.expand_key.take() {
            self.expand_cache.insert(
                key,
                ExpandState {
                    expanded: std::mem::take(&mut self.expanded),
                    groups: std::mem::take(&mut self.groups),
                    work_open: std::mem::take(&mut self.work_open),
                },
            );
            // Bound the map: an arbitrary entry goes once it is full.
            if self.expand_cache.len() > EXPAND_STATE_CAP
                && let Some(stale) = self.expand_cache.keys().next().cloned()
            {
                self.expand_cache.remove(&stale);
            }
        }
        self.close();
        self.rotate_channels();
        self.open = true;
        self.title = title.clone();
        self.error.clear();
        self.status.clear();
        self.sending = false;
        self.transcript = Transcript::default();
        self.expanded.clear();
        self.groups.clear();
        self.work_open.clear();
        // A session id identifies its expansion; a standalone file its path,
        // and a bare cwd spec (`--session-ui`) the cwd itself.
        let expand_key = session_id
            .clone()
            .or_else(|| {
                explicit
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned())
            })
            .or_else(|| match &endpoint {
                Endpoint::Local { cwd, .. } | Endpoint::Remote { cwd, .. } => {
                    (!cwd.is_empty()).then(|| cwd.clone())
                }
            });
        if let Some(state) = expand_key
            .as_ref()
            .and_then(|key| self.expand_cache.get(key))
            .cloned()
        {
            self.expanded = state.expanded;
            self.groups = state.groups;
            self.work_open = state.work_open;
        }
        self.expand_key = expand_key;
        self.list = new_list_state();
        self.rows.clear();
        self.list_pending = false;
        self.list_live = false;
        self.rows_dirty = false;
        self.changed_from = None;
        self.rpc_tx = None;
        self.rpc_up = false;
        self.pi_state = None;
        self.live = None;
        self.catalog.clear();
        self.picker = None;
        self.picker_dismissed = None;
        self.pending_prompt = None;
        self.pending_from = 0;
        self.prompts_open = false;
        self.prompts_dismissed = false;
        self.outbox.clear();
        self.awaiting_response = false;
        self.pending_steering = false;
        self.last_growth = None;
        self.last_seen = 0;
        self.target = target;
        self.endpoint = Some(endpoint.clone());
        self.stop = Arc::new(AtomicBool::new(false));
        let stop = self.stop.clone();
        let tx = self.tx.clone();
        let cache = self.cache.clone();
        let catalog_endpoint = endpoint.clone();
        let reader_session_id = session_id.clone();
        std::thread::spawn(move || {
            reader_loop(endpoint, explicit, reader_session_id, cache, tx, stop)
        });
        // A reported session id also gets the extension's RPC channel when
        // it can be reached; `--session-ui` has no id and stays file-only.
        if let Some(id) = session_id {
            let (command_tx, command_rx) = std::sync::mpsc::channel();
            self.rpc_tx = Some(command_tx);
            let rpc_endpoint = catalog_endpoint.clone();
            let rpc_tx = self.tx.clone();
            let rpc_stop = self.stop.clone();
            std::thread::spawn(move || {
                rpc_loop(rpc_endpoint, id, rpc_tx, command_rx, rpc_stop);
            });
        }
        // The model catalog (pi's models-store, locally or over SSH) arrives
        // separately; the picker uses session models until it lands.
        let catalog_tx = self.tx.clone();
        std::thread::spawn(move || {
            let configs = match &catalog_endpoint {
                Endpoint::Local { .. } => pilus_client::agents::local(),
                Endpoint::Remote { target, .. } => pilus_client::agents::remote(target),
            };
            if let Some(pi) = configs.iter().find(|config| config.kind == "pi") {
                let _ = catalog_tx.send(Update::Catalog(pi.models.clone()));
            }
        });
        cx.notify();
    }

    /// Pin or unpin the panel. Pinned, it replaces a pi pane's terminal:
    /// Escape is swallowed instead of closing it.
    pub fn set_pinned(&mut self, pinned: bool) {
        self.pinned = pinned;
    }

    /// Swap every channel the panel drains for fresh ones. Dropping the old
    /// receivers makes the previous session's threads fail their next send —
    /// a stale delta, RPC event, catalog, or prompt/interrupt result can
    /// never land in the new session's transcript or status.
    fn rotate_channels(&mut self) {
        let (tx, rx) = std::sync::mpsc::channel();
        self.tx = tx;
        self.rx = rx;
        let (send_tx, send_rx) = std::sync::mpsc::channel();
        self.send_tx = send_tx;
        self.send_rx = send_rx;
        let (stop_tx, stop_rx) = std::sync::mpsc::channel();
        self.stop_tx = stop_tx;
        self.stop_rx = stop_rx;
    }

    /// Close the panel and stop its reader thread.
    pub fn close(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.open = false;
        self.pinned = false;
        self.last_seen = 0;
    }

    /// Drain the reader thread and any prompt result.
    ///
    /// Returns whether a repaint is warranted. Clears the composer on a
    /// successful send.
    pub fn poll(&mut self, input: &Entity<TextInput>, cx: &mut Context<PilusView>) -> bool {
        let mut changed = false;
        while let Ok(update) = self.rx.try_recv() {
            match update {
                Update::Delta(delta) => {
                    self.apply_delta(delta.from, delta.items, delta.meta);
                }
                Update::Catalog(catalog) => {
                    self.catalog = catalog;
                }
                Update::Error(error) => self.error = error,
                Update::RpcUp => {
                    self.rpc_up = true;
                }
                Update::RpcDown => {
                    // State and live data came from the socket; with it gone
                    // the file tail's heuristics take over again.
                    self.rpc_up = false;
                    self.pi_state = None;
                    self.live = None;
                }
                Update::RpcState(state) => {
                    self.pi_state = Some(*state);
                }
                Update::RpcStream(items) => {
                    let live = self.live.get_or_insert_with(LiveTurn::default);
                    live.items = items;
                    live.ended_at = None;
                    self.bump_live();
                }
                Update::RpcStreamEnd => {
                    if let Some(live) = &mut self.live {
                        live.ended_at = Some(Instant::now());
                    }
                    self.bump_live();
                }
                Update::RpcTool(event) => {
                    let name = event.tool_name.clone();
                    let output = event.output.clone();
                    match event.phase {
                        ToolPhase::End => {
                            if let Some(live) = &mut self.live {
                                live.tool = None;
                            }
                        }
                        ToolPhase::Start | ToolPhase::Update => {
                            let live = self.live.get_or_insert_with(LiveTurn::default);
                            live.tool = Some((name, output));
                        }
                    }
                    self.bump_live();
                }
                Update::RpcResult { kind, result } => {
                    self.rpc_result(kind, result);
                }
            }
            changed = true;
        }
        // A finished live turn waits on the file; past the linger it is stale.
        if self.live.as_ref().is_some_and(|live| live_expired(live, Instant::now())) {
            self.live = None;
            changed = true;
        }
        while let Ok(result) = self.send_rx.try_recv() {
            self.sending = false;
            match result {
                Ok(()) => {
                    self.status.clear();
                    // Picker commands leave the draft alone.
                    if self.send_clears_input {
                        input.update(cx, |input, cx| input.clear(cx));
                    }
                }
                Err(error) => {
                    self.status = format!("couldn't send: {error}");
                    self.pending_prompt = None;
                    self.awaiting_response = false;
                    self.pending_steering = false;
                }
            }
            // A queued prompt goes out on the freed channel, oldest first.
            if let Some((text, clear)) = self.outbox.pop_front() {
                self.send_text(text, clear, cx);
            }
            changed = true;
        }
        while let Ok(result) = self.stop_rx.try_recv() {
            if let Err(error) = result {
                self.status = format!("couldn't stop: {error}");
            }
            changed = true;
        }
        changed
    }

    /// Apply one reader delta: items at or after `from` are replaced
    /// wholesale, and the fold's metadata is adopted.
    fn apply_delta(&mut self, from: usize, items: Vec<Item>, meta: TranscriptMeta) {
        // Past the initial delivery, anything arriving is file growth: the
        // session is alive right now.
        if self.last_seen > 0 {
            self.last_growth = Some(Instant::now());
        }
        let from = from.min(self.transcript.items.len());
        self.transcript.items.truncate(from);
        self.transcript.items.extend(items);
        // The file caught up with a finished live turn; the overlay is stale.
        if self.live.as_ref().is_some_and(|live| live.ended_at.is_some()) {
            self.live = None;
        }
        self.transcript.set_meta(meta);
        self.last_seen = self.transcript.items.len();
        self.rows_dirty = true;
        // The splice has to reach back to the earliest item the deltas
        // rewrote — an outcome can land on a call rows above the tail.
        self.changed_from = Some(self.changed_from.map_or(from, |earlier| earlier.min(from)));
        self.error.clear();
    }

    /// The reply to a socket command: a failed prompt drops the optimistic
    /// bubble, the models reply refills the picker, and any refusal lands on
    /// the status line.
    fn rpc_result(&mut self, kind: RequestKind, result: Result<Option<Value>, String>) {
        match (kind, result) {
            (RequestKind::Prompt, Err(error)) => {
                self.status = format!("couldn't send: {error}");
                self.pending_prompt = None;
                self.awaiting_response = false;
                self.pending_steering = false;
            }
            (RequestKind::Models, Ok(Some(data))) => {
                self.catalog = models_from(&data)
                    .iter()
                    .map(|model| AgentModel {
                        provider: model.provider.clone(),
                        id: model.id.clone(),
                        name: model.name.clone(),
                        ..AgentModel::default()
                    })
                    .collect();
            }
            (_, Err(error)) => self.status = error,
            (_, Ok(_)) => {}
        }
    }

    /// Queue a command on the extension socket. A dead channel only means
    /// the reader thread is gone, which the reconnect path already reports.
    fn send_rpc_command(&self, command: RpcCommand) {
        if let Some(tx) = &self.rpc_tx {
            let _ = tx.send(command);
        }
    }

    /// The list index of the live overlay: after the rows and the
    /// optimistic bubble.
    fn live_index(&self) -> usize {
        self.rows.len() + usize::from(self.pending_prompt.is_some())
    }

    /// Remeasure the overlay's row after its content changed.
    fn bump_live(&mut self) {
        let index = self.live_index();
        self.list.remeasure_items(index..index + 1);
    }

    /// Handle a keystroke. Returns whether it was consumed.
    pub fn key(
        &mut self,
        keystroke: &Keystroke,
        input: &Entity<TextInput>,
        cx: &mut Context<PilusView>,
    ) -> bool {
        match keystroke.key.as_str() {
            "escape" => {
                // Open menus eat the first Escape in topmost order; the next
                // one closes the panel — unless it is pinned, where Escape
                // does nothing so the pane's terminal stays replaced.
                if self.picker.take().is_some() {
                    self.picker_dismissed = None;
                } else if self.prompts_open {
                    self.prompts_open = false;
                    self.prompts_dismissed = false;
                } else if !self.pinned {
                    self.close();
                }
                true
            }
            "enter" => {
                self.submit(input, cx, keystroke.modifiers.alt);
                true
            }
            "end" => {
                self.follow_tail();
                true
            }
            _ => false,
        }
    }

    /// Send the composer's text to the live agent: over the extension
    /// socket when it is up, through the pane's terminal otherwise. Alt
    /// held on Enter queues a follow-up behind a busy turn.
    fn submit(&mut self, input: &Entity<TextInput>, cx: &mut Context<PilusView>, alt: bool) {
        let text = input.read(cx).value().trim().to_string();
        match route(&text, self.rpc_up, self.is_working(), alt) {
            Route::Rpc(deliver_as) => self.send_rpc(text, deliver_as, input, cx),
            Route::Terminal => self.send_text(text, true, cx),
        }
    }

    /// Send a prompt over the extension socket. The composer clears at once
    /// and the bubble stays optimistic until the file echoes the prompt.
    fn send_rpc(
        &mut self,
        text: String,
        deliver_as: Option<DeliverAs>,
        input: &Entity<TextInput>,
        cx: &mut Context<PilusView>,
    ) {
        if text.is_empty() {
            return;
        }
        self.set_optimistic(&text);
        self.pending_from = self.transcript.items.len();
        self.pending_steering = deliver_as.is_some();
        self.follow_tail();
        input.update(cx, |input, cx| input.clear(cx));
        self.send_rpc_command(RpcCommand::Prompt {
            text,
            deliver_as,
        });
        cx.notify();
    }

    /// Send a picker command (`/model`, `/thinking`) without touching the
    /// composer's draft. The socket takes the typed command; the terminal
    /// path only remains for when it is down.
    fn send_command(&mut self, command: String, cx: &mut Context<PilusView>) {
        self.picker = None;
        self.picker_dismissed = None;
        if self.rpc_up {
            if let Some(argument) = command.strip_prefix("/model ") {
                let (provider, model) = argument.split_once('/').unwrap_or(("", argument));
                self.send_rpc_command(RpcCommand::SetModel {
                    provider: provider.to_string(),
                    model: model.to_string(),
                });
                return;
            }
            if let Some(level) = command.strip_prefix("/thinking ") {
                self.send_rpc_command(RpcCommand::SetThinking {
                    level: level.to_string(),
                });
                return;
            }
        }
        self.send_text(command, false, cx);
    }

    /// Send `text` through the endpoint's control surface.
    fn send_text(&mut self, text: String, clear_input: bool, cx: &mut Context<PilusView>) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        if self.sending {
            // A send is already in flight; queue behind it instead of
            // dropping the prompt. A composer prompt shows optimistically
            // at once; picker commands stay invisible as usual.
            if clear_input {
                self.set_optimistic(&text);
            }
            self.outbox.push_back((text, clear_input));
            cx.notify();
            return;
        }
        let Some(endpoint) = self.endpoint.clone() else {
            return;
        };
        let Some(target) = self.target.clone() else {
            self.status = "no live pi agent in this workspace to prompt".to_string();
            cx.notify();
            return;
        };
        self.sending = true;
        self.send_clears_input = clear_input;
        if clear_input {
            self.set_optimistic(&text);
            self.pending_from = self.transcript.items.len();
            self.follow_tail();
        }
        let tx = self.send_tx.clone();
        std::thread::spawn(move || {
            let client = match &endpoint {
                Endpoint::Local { api_socket, .. } => ControlClient::connect(api_socket),
                Endpoint::Remote { target: ssh, .. } => ControlClient::connect_ssh(ssh.clone()),
            };
            let result = client
                .and_then(|mut client| client.prompt_agent(&target, &text))
                .map_err(|error| error.to_string());
            let _ = tx.send(result);
        });
        cx.notify();
    }

    /// Show a composer send optimistically: the bubble, the indicator, and
    /// `Steering…` when pi is genuinely already working.
    fn set_optimistic(&mut self, text: &str) {
        // Steering means pi is mid-turn *now*: our own prompt is still
        // unanswered, or the transcript ends in work the file is still
        // growing into. A merely recent file is not enough — that only says
        // the session is alive — and a stale trailing work item (an
        // interrupted session) is not a turn either.
        self.pending_steering = self.pending_prompt.is_some()
            || self.awaiting_response
            || (self.work_in_flight() && self.recently_active());
        self.pending_prompt = Some(text.to_string());
        self.awaiting_response = true;
    }

    /// Interrupt the live agent the way its own Escape key does. The
    /// optimistic state clears at once; the transcript catches up.
    fn stop(&mut self, cx: &mut Context<PilusView>) {
        if self.rpc_up {
            self.send_rpc_command(RpcCommand::Abort);
            self.pending_prompt = None;
            self.awaiting_response = false;
            self.pending_steering = false;
            cx.notify();
            return;
        }
        let Some(endpoint) = self.endpoint.clone() else {
            return;
        };
        let Some(target) = self.target.clone() else {
            self.status = "no live pi agent in this workspace to stop".to_string();
            cx.notify();
            return;
        };
        self.pending_prompt = None;
        self.awaiting_response = false;
        self.pending_steering = false;
        let tx = self.stop_tx.clone();
        std::thread::spawn(move || {
            let client = match &endpoint {
                Endpoint::Local { api_socket, .. } => ControlClient::connect(api_socket),
                Endpoint::Remote { target: ssh, .. } => ControlClient::connect_ssh(ssh.clone()),
            };
            let result = client
                .and_then(|mut client| client.interrupt_agent(&target))
                .map_err(|error| error.to_string());
            let _ = tx.send(result);
        });
        cx.notify();
    }

    /// Toggle a composer picker from its chip.
    ///
    /// A press outside the picker already closed it in the capture phase; in
    /// that case this click is a toggle-off and must not reopen it. A click
    /// on the *other* chip still opens it directly.
    fn toggle_picker(&mut self, kind: PickerKind) {
        if self.picker_dismissed.take() == Some(kind) || self.picker == Some(kind) {
            self.picker = None;
        } else {
            self.picker = Some(kind);
        }
    }

    /// Close the open picker from an outside press, remembering which one so
    /// its own chip's click stays closed.
    fn dismiss_picker(&mut self) {
        self.picker_dismissed = self.picker.take();
    }

    /// Toggle one card between collapsed and expanded.
    pub(super) fn toggle(&mut self, index: usize) {
        if !self.expanded.remove(&index) {
            self.expanded.insert(index);
        }
        // Remeasure the row in place, so expanding or collapsing it never
        // reads as the row vanishing.
        let row = self.row_of(index);
        self.list.remeasure_items(row..row + 1);
    }

    /// Scroll one absolute item into view, leaving the tail pin behind.
    fn jump_to(&mut self, index: usize) {
        self.sync_list();
        if index < self.transcript.items.len() {
            self.list.scroll_to(ListOffset {
                item_ix: self.row_of(index),
                offset_in_item: px(0.0),
            });
        }
    }

    /// Re-pin the list to the tail: follow new content and anchor at the end.
    fn follow_tail(&mut self) {
        self.list.set_follow_mode(FollowMode::Tail);
    }

    /// The list row holding absolute item `index`, or the last row when the
    /// index lands past the end.
    fn row_of(&self, item: usize) -> usize {
        self.rows
            .partition_point(|row| row.start <= item)
            .saturating_sub(1)
            .min(self.rows.len().saturating_sub(1))
    }

    /// Fold the transcript into rows and tell the list what changed.
    ///
    /// The last entry is the optimistic-prompt bubble while one is in
    /// flight. A shrink means the transcript was rewound or replaced, so the
    /// list resets; otherwise only the rows from the first difference on are
    /// remeasured — and the old last row always is, because a tool outcome
    /// attaches to it in place and a trailing `Working…` flips to
    /// `Working for …` when something follows it.
    fn sync_list(&mut self) {
        let pending = self.pending_prompt.is_some();
        let live = self.live.is_some();
        if self.rows_dirty {
            let new = build_rows(&self.transcript.items);
            let old_count =
                self.rows.len() + self.list_pending as usize + self.list_live as usize;
            let new_count = new.len() + pending as usize + live as usize;
            let old_coverage = self.rows.last().map_or(0, |row| row.end);
            let new_coverage = new.last().map_or(0, |row| row.end);
            if new_coverage < old_coverage {
                self.list.reset(new_count);
            } else {
                let first = self.splice_start(&new);
                self.list.splice(first..old_count, new_count - first);
            }
            self.changed_from = None;
            self.rows = new;
            self.rows_dirty = false;
        } else {
            // Only the tail entries changed: swap the count after the rows
            // in place so the transcript never reflows.
            let old_tail = self.list_pending as usize + self.list_live as usize;
            let new_tail = pending as usize + live as usize;
            if pending != self.list_pending || live != self.list_live {
                self.list.splice(
                    self.rows.len()..self.rows.len() + old_tail,
                    new_tail,
                );
            }
        }
        self.list_pending = pending;
        self.list_live = live;
    }

    /// The row a splice starts at: the first row whose range changed, moved
    /// back to the row holding the earliest item the deltas rewrote — an
    /// outcome attaching to an earlier call does not move the prefix diff.
    /// The old last row always remeasures so a trailing `Working…` flips.
    fn splice_start(&self, new: &[Row]) -> usize {
        let mut first = self
            .rows
            .iter()
            .zip(new.iter())
            .position(|(old, new)| old != new)
            .unwrap_or_else(|| self.rows.len().min(new.len()));
        if let Some(changed) = self.changed_from {
            first = first.min(self.row_of(changed));
        }
        first.min(self.rows.len().saturating_sub(1))
    }

    /// Toggle one tool-call group.
    pub(super) fn toggle_group(&mut self, index: usize) {
        if !self.groups.remove(&index) {
            self.groups.insert(index);
        }
        let row = self.row_of(index);
        self.list.remeasure_items(row..row + 1);
    }

    /// Toggle one run of work between assistant replies.
    pub(super) fn toggle_work(&mut self, index: usize) {
        if !self.work_open.remove(&index) {
            self.work_open.insert(index);
        }
        let row = self.row_of(index);
        self.list.remeasure_items(row..row + 1);
    }

    /// A chip: a small muted label on a subtle pill.
    fn chip(&self, label: String, color: gpui::Rgba, fonts: Fonts) -> impl IntoElement {
        let theme = self.theme;
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .px_2()
            .py(px(1.0))
            .rounded_md()
            .bg(theme.panel_dim)
            .text_size(px(fonts.label))
            .text_color(color)
            .child(label)
    }

    /// A user turn on the right, in a chat bubble. The optimistic pending
    /// prompt renders through the same bubble until pi appends the real one.
    fn user_bubble(&self, text: &str, images: usize, fonts: Fonts) -> AnyElement {
        let theme = self.theme;
        let mut body = div()
            .flex()
            .flex_col()
            .gap_2()
            .px_4()
            .py_3()
            .rounded_lg()
            .bg(theme.panel)
            .text_size(px(fonts.body))
            .text_color(theme.panel_fg)
            .child(text.to_string());
        if images > 0 {
            body = body.child(self.chip(format!("{images} image(s)"), theme.muted, fonts));
        }
        div()
            .flex()
            .flex_row()
            .justify_end()
            .child(body.max_w(relative(0.72)))
            .into_any_element()
    }

    /// One transcript item, keyed by its index for expand state.
    fn item(
        &self,
        index: usize,
        item: &Item,
        fonts: Fonts,
        cx: &mut Context<PilusView>,
    ) -> AnyElement {
        let theme = self.theme;
        let expanded = self.expanded.contains(&index);
        match &item.kind {
            ItemKind::User { text, images } => self.user_bubble(text, *images, fonts),
            ItemKind::Assistant { text, .. } => div()
                .flex()
                .flex_col()
                .max_w(relative(0.86))
                .child(super::markdown::render(text, theme, fonts, &self.settings))
                .into_any_element(),
            ItemKind::Thinking { text } => self.collapsible(
                index,
                "Thinking".to_string(),
                theme.muted,
                text.clone(),
                // Reasoning reads as an aside: smaller than the reply.
                Section::Markdown(aside_fonts(fonts)),
                expanded,
                fonts,
                cx,
            ),
            ItemKind::ToolCall {
                name,
                arguments,
                outcome,
                ..
            } => self.tool_call(index, name, arguments, outcome.as_ref(), fonts, cx),
            ItemKind::ToolResult {
                name,
                text,
                images,
                is_error,
                ..
            } => {
                let label = if *is_error {
                    format!("error · {name}")
                } else {
                    format!("result · {name}")
                };
                let color = if *is_error { theme.alert } else { theme.muted };
                let mut body = text.clone();
                if *images > 0 {
                    if !body.is_empty() {
                        body.push('\n');
                    }
                    body.push_str(&format!("{images} image(s)"));
                }
                self.collapsible(
                    index,
                    label,
                    color,
                    body,
                    Section::Mono,
                    expanded,
                    fonts,
                    cx,
                )
            }
            ItemKind::Bash {
                command,
                output,
                exit_code,
                cancelled,
                truncated,
            } => {
                let status = if *cancelled {
                    "cancelled".to_string()
                } else {
                    match exit_code {
                        Some(code) => format!("exit {code}"),
                        None => "exit ?".to_string(),
                    }
                };
                let color = if *cancelled || *exit_code != Some(0) {
                    theme.alert
                } else {
                    theme.muted
                };
                let mut body = format!("$ {command}");
                if !output.is_empty() {
                    body.push('\n');
                    body.push_str(output);
                }
                if *truncated {
                    body.push_str("\n… output truncated");
                }
                self.collapsible(
                    index,
                    format!("bash · {status}"),
                    color,
                    body,
                    Section::Mono,
                    expanded,
                    fonts,
                    cx,
                )
            }
            ItemKind::Note { title, text } => {
                // The thinking level lives in the composer's picker, and the
                // model in its own chip, so those notes add nothing here.
                if title == "thinking" {
                    return div().into_any_element();
                }
                // A model change is one line, no chevron: the picker already
                // shows the current model.
                if title == "model" {
                    return div()
                        .flex()
                        .flex_col()
                        .w_full()
                        .max_w(relative(0.86))
                        .child(
                            div()
                                .py_2()
                                .text_size(px(fonts.label))
                                .text_color(theme.muted)
                                .child(format!("Model changed to {}", truncate(text, 240))),
                        )
                        .into_any_element();
                }
                // Structural notes (thinking changes, compactions) read like
                if text.is_empty() {
                    return div()
                        .flex()
                        .flex_col()
                        .w_full()
                        .max_w(relative(0.86))
                        .child(
                            div()
                                .py_2()
                                .text_size(px(fonts.label))
                                .font_weight(gpui::FontWeight::BOLD)
                                .text_color(theme.muted)
                                .child(capitalize(title)),
                        )
                        .into_any_element();
                }
                self.collapsible(
                    index,
                    capitalize(title),
                    theme.muted,
                    text.clone(),
                    Section::Markdown(aside_fonts(fonts)),
                    expanded,
                    fonts,
                    cx,
                )
            }
        }
    }

    /// A muted, clickable section row: `label ›`, the chevron right after the
    /// label so the row hugs its text instead of spanning the width.
    #[allow(clippy::too_many_arguments)]
    fn section_header(
        &self,
        id: ElementId,
        label: String,
        label_color: gpui::Rgba,
        expanded: bool,
        toggle: Toggle,
        fonts: Fonts,
        cx: &mut Context<PilusView>,
    ) -> Stateful<Div> {
        let theme = self.theme;
        div()
            .id(id)
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .py_2()
            .cursor_pointer()
            .on_click(cx.listener(move |this: &mut PilusView, _, _, cx| {
                match toggle {
                    Toggle::Item(index) => this.session.toggle(index),
                    Toggle::Group(index) => this.session.toggle_group(index),
                    Toggle::Work(index) => this.session.toggle_work(index),
                }
                cx.notify();
            }))
            .child(
                div()
                    .text_size(px(fonts.label))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(label_color)
                    .child(label),
            )
            .child(
                div()
                    .text_size(px(fonts.label))
                    .text_color(theme.muted)
                    .child(if expanded { "⌄" } else { "›" }),
            )
    }

    /// A muted section: a clickable header with a chevron, and a body shown
    /// only when opened. No card chrome — just text.
    #[allow(clippy::too_many_arguments)]
    fn collapsible(
        &self,
        index: usize,
        label: String,
        label_color: gpui::Rgba,
        body: String,
        section: Section,
        expanded: bool,
        fonts: Fonts,
        cx: &mut Context<PilusView>,
    ) -> AnyElement {
        let theme = self.theme;
        let header = self.section_header(
            ("session-section", index).into(),
            label,
            label_color,
            expanded,
            Toggle::Item(index),
            fonts,
            cx,
        );
        let mut column = div()
            .flex()
            .flex_col()
            .w_full()
            .max_w(relative(0.86))
            .child(header);
        if expanded {
            let block = match section {
                Section::Mono => div()
                    .pb_2()
                    .overflow_hidden()
                    .font_family(style::mono_family(&self.settings))
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .whitespace_normal()
                    .child(clip_lines(&body, EXPANDED_LINES))
                    .into_any_element(),
                Section::Markdown(body_fonts) => div()
                    .pb_2()
                    .child(super::markdown::render(
                        &body,
                        Theme {
                            fg: theme.muted,
                            ..theme
                        },
                        body_fonts,
                        &self.settings,
                    ))
                    .into_any_element(),
            };
            column = column.child(block);
        }
        column.into_any_element()
    }

    /// One tool call: a parsed summary header and, when opened, its arguments
    /// and result in monospace.
    fn tool_call(
        &self,
        index: usize,
        name: &str,
        arguments: &Value,
        outcome: Option<&ToolOutcome>,
        fonts: Fonts,
        cx: &mut Context<PilusView>,
    ) -> AnyElement {
        let theme = self.theme;
        let expanded = self.expanded.contains(&index);
        let is_error = outcome.map(|outcome| outcome.is_error).unwrap_or(false);
        let color = if is_error { theme.alert } else { theme.muted };
        let header = self.section_header(
            ("session-tool", index).into(),
            tool_summary(name, arguments),
            color,
            expanded,
            Toggle::Item(index),
            fonts,
            cx,
        );
        let mut column = div()
            .flex()
            .flex_col()
            .w_full()
            .max_w(relative(0.86))
            .child(header);
        if expanded {
            let details = tool_details(name, arguments, outcome);
            column = column.child(
                div()
                    .pb_2()
                    .overflow_hidden()
                    .font_family(style::mono_family(&self.settings))
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .whitespace_normal()
                    .child(clip_lines(&details, EXPANDED_LINES)),
            );
        }
        column.into_any_element()
    }

    /// A run of consecutive tool calls, collapsed to one row.
    fn tool_group(
        &self,
        start: usize,
        calls: &[Item],
        fonts: Fonts,
        cx: &mut Context<PilusView>,
    ) -> AnyElement {
        // A lone call needs no wrapper.
        if let [item] = calls
            && let ItemKind::ToolCall {
                name,
                arguments,
                outcome,
                ..
            } = &item.kind
        {
            return self.tool_call(start, name, arguments, outcome.as_ref(), fonts, cx);
        }
        let theme = self.theme;
        let expanded = self.groups.contains(&start);
        let header = self.section_header(
            ("session-group", start).into(),
            summarize_calls(calls),
            theme.muted,
            expanded,
            Toggle::Group(start),
            fonts,
            cx,
        );
        let mut column = div()
            .flex()
            .flex_col()
            .w_full()
            .max_w(relative(0.86))
            .child(header);
        if expanded {
            let mut inner = div().flex().flex_col().gap_1().pl_4();
            for (offset, item) in calls.iter().enumerate() {
                if let ItemKind::ToolCall {
                    name,
                    arguments,
                    outcome,
                    ..
                } = &item.kind
                {
                    inner = inner.child(self.tool_call(
                        start + offset,
                        name,
                        arguments,
                        outcome.as_ref(),
                        fonts,
                        cx,
                    ));
                }
            }
            column = column.child(inner);
        }
        column.into_any_element()
    }

    /// A stretch of thinking and tool calls between assistant replies,
    /// collapsed under a `Working for …` header.
    fn work_group(
        &self,
        start: usize,
        run: &[Item],
        trailing: bool,
        fonts: Fonts,
        cx: &mut Context<PilusView>,
    ) -> AnyElement {
        let theme = self.theme;
        let expanded = self.work_open.contains(&start);
        let header = self.section_header(
            ("session-work", start).into(),
            work_label(run, trailing),
            theme.muted,
            expanded,
            Toggle::Work(start),
            fonts,
            cx,
        );
        let mut column = div()
            .flex()
            .flex_col()
            .w_full()
            .max_w(relative(0.86))
            .child(header);
        if expanded {
            let mut inner = div().flex().flex_col().gap_2().pl_4();
            let mut index = 0;
            while index < run.len() {
                if matches!(run[index].kind, ItemKind::ToolCall { .. }) {
                    let group_start = index;
                    while index < run.len() && matches!(run[index].kind, ItemKind::ToolCall { .. })
                    {
                        index += 1;
                    }
                    inner = inner.child(self.tool_group(
                        start + group_start,
                        &run[group_start..index],
                        fonts,
                        cx,
                    ));
                } else {
                    inner = inner.child(self.item(start + index, &run[index], fonts, cx));
                    index += 1;
                }
            }
            column = column.child(inner);
        }
        column.into_any_element()
    }

    /// The header strip: identity on the left, live counters on the right.
    /// One row: the first prompt on the left, the host and cwd on the right.
    /// Model, cost, and usage live in the composer now.
    fn header(&self, fonts: Fonts) -> Div {
        let theme = self.theme;
        div()
            .flex()
            .flex_shrink_0()
            .flex_row()
            .items_center()
            .gap_3()
            .h(px(40.0))
            .px_4()
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .text_size(px(fonts.body))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(theme.fg)
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(self.first_prompt()),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .max_w(relative(0.4))
                    .overflow_hidden()
                    .font_family(style::mono_family(&self.settings))
                    .text_size(px(fonts.label))
                    .text_color(theme.muted)
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(self.host_cwd()),
            )
    }

    /// The session's first prompt as one line, or the pane title when the
    /// transcript has no user message yet.
    fn first_prompt(&self) -> String {
        let prompt = self
            .transcript
            .items
            .iter()
            .find_map(|item| match &item.kind {
                ItemKind::User { text, .. } => Some(text.clone()),
                _ => None,
            });
        match prompt {
            Some(text) => {
                let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
                truncate(&one_line, 240)
            }
            None if self.title.is_empty() => self.transcript.header.id.clone(),
            None => self.title.clone(),
        }
    }

    /// `host · cwd` for the header's right column.
    fn host_cwd(&self) -> String {
        match &self.endpoint {
            Some(Endpoint::Local { cwd, .. }) => {
                let cwd = if cwd.is_empty() {
                    self.transcript.header.cwd.clone()
                } else {
                    cwd.clone()
                };
                format!("local · {cwd}")
            }
            Some(Endpoint::Remote { target, cwd }) => {
                format!("{} · {cwd}", target.destination)
            }
            None => String::new(),
        }
    }

    /// The scrollable transcript, or the empty/error placeholder.
    fn body(&self, fonts: Fonts, cx: &mut Context<PilusView>) -> AnyElement {
        let theme = self.theme;
        if !self.error.is_empty() {
            return div()
                .flex_1()
                .min_h(px(0.0))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .px_6()
                .text_center()
                .text_size(px(fonts.body))
                .text_color(theme.muted)
                .child("This session could not be read.")
                .child(
                    div()
                        .text_size(px(fonts.label))
                        .text_color(theme.alert)
                        .child(self.error.clone()),
                )
                .into_any_element();
        }
        if self.transcript.items.is_empty() {
            return div()
                .flex_1()
                .min_h(px(0.0))
                .flex()
                .items_center()
                .justify_center()
                .px_6()
                .text_center()
                .text_size(px(fonts.body))
                .text_color(theme.muted)
                .child("Waiting for the first message…")
                .into_any_element();
        }
        // The virtualized list is a flex sibling of the composer, so the
        // composer can never cover the last message.
        list(
            self.list.clone(),
            cx.processor(move |this: &mut PilusView, ix: usize, _window, cx| {
                this.session.row_element(ix, fonts, cx)
            }),
        )
        .flex_1()
        .min_h(px(0.0))
        .w_full()
        .pt_6()
        .pb_2()
        .into_any_element()
    }

    /// One list entry: a transcript row wrapped to the reading measure, or
    /// the optimistic prompt as the last entry. Out-of-range indices render
    /// empty rather than panic, since the callback runs a frame after splice.
    fn row_element(&mut self, ix: usize, fonts: Fonts, cx: &mut Context<PilusView>) -> AnyElement {
        let Some(row) = self.rows.get(ix).copied() else {
            return self.tail_element(ix, fonts, cx);
        };
        let items = &self.transcript.items;
        let content = match row.kind {
            RowKind::Item => self.item(row.start, &items[row.start], fonts, cx),
            // A run that is only thinking needs no `Working…` wrapper; the
            // thinking rows stand on their own.
            RowKind::Thinking => {
                let mut column = div().flex().flex_col().gap_4();
                for (offset, item) in items[row.start..row.end].iter().enumerate() {
                    column = column
                        .child(div().flex_shrink_0().child(self.item(
                            row.start + offset,
                            item,
                            fonts,
                            cx,
                        )));
                }
                column.into_any_element()
            }
            RowKind::Work => self.work_group(
                row.start,
                &items[row.start..row.end],
                row.end == items.len(),
                fonts,
                cx,
            ),
        };
        div()
            .w_full()
            .flex()
            .flex_row()
            .justify_center()
            .px_6()
            .pb_4()
            .child(
                div()
                    .w_full()
                    .max_w(px(TRANSCRIPT_MAX_W))
                    .child(content),
            )
            .into_any_element()
    }

    /// The entries after the transcript rows, in order: the optimistic
    /// prompt bubble, then the live turn overlay.
    fn tail_element(&mut self, ix: usize, fonts: Fonts, cx: &mut Context<PilusView>) -> AnyElement {
        let frame = |content: AnyElement| {
            div()
                .w_full()
                .flex()
                .flex_row()
                .justify_center()
                .px_6()
                .pb_4()
                .child(
                    div()
                        .w_full()
                        .max_w(px(TRANSCRIPT_MAX_W))
                        .child(content),
                )
                .into_any_element()
        };
        let extra = ix.saturating_sub(self.rows.len());
        if extra == 0
            && let Some(prompt) = self.pending_prompt.clone()
        {
            return frame(self.user_bubble(&prompt, 0, fonts));
        }
        if extra == usize::from(self.pending_prompt.is_some()) && self.live.is_some() {
            return frame(self.live_element(fonts, cx));
        }
        div().into_any_element()
    }

    /// The live turn overlay: pi's streaming message rendered like the
    /// transcript's, plus a running tool's output tail. Item indices start
    /// at [`LIVE_BASE`] so expand keys never collide with real items.
    fn live_element(&mut self, fonts: Fonts, cx: &mut Context<PilusView>) -> AnyElement {
        let theme = self.theme;
        let Some(live) = &self.live else {
            return div().into_any_element();
        };
        let mut column = div().flex().flex_col().gap_4();
        for (offset, item) in live.items.iter().enumerate() {
            column = column.child(
                div()
                    .flex_shrink_0()
                    .child(self.item(LIVE_BASE + offset, item, fonts, cx)),
            );
        }
        if let Some((name, output)) = &live.tool {
            // The last few lines of a running tool, tail-style, in the
            // transcript's muted mono face.
            let lines: Vec<&str> = output.lines().collect();
            let tail = lines
                .iter()
                .skip(lines.len().saturating_sub(LIVE_TOOL_LINES))
                .copied()
                .collect::<Vec<_>>()
                .join("\n");
            column = column.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(theme.panel_dim)
                    .child(
                        div()
                            .text_size(px(fonts.label))
                            .text_color(theme.muted)
                            .child(format!("{name} · running")),
                    )
                    .child(
                        div()
                            .font_family(style::mono_family(&self.settings))
                            .text_size(px(fonts.secondary))
                            .text_color(theme.muted)
                            .whitespace_normal()
                            .child(tail),
                    ),
            );
        }
        column.into_any_element()
    }

    /// The sticky prompt composer, a flex sibling below the transcript.
    ///
    /// One rounded panel holds the input and, below it, the meta row: model
    /// and thinking pickers (changes go to the live agent as `/model` and
    /// `/thinking` commands), the token breakdown, and the send arrow.
    fn composer(
        &self,
        input: &Entity<TextInput>,
        fonts: Fonts,
        cx: &mut Context<PilusView>,
    ) -> Div {
        let theme = self.theme;
        input.update(cx, |input, _cx| {
            input.set_style(theme, style::chrome_font(&self.settings), px(fonts.body));
        });
        let mut bar = div()
            .flex()
            .flex_col()
            .rounded_lg()
            .bg(theme.panel)
            .child(
                div().flex().flex_row().px_3().pt_3().child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .min_h(px(72.0))
                        .child(input.clone()),
                ),
            )
            .child(self.meta_row(fonts, cx));
        if !self.status.is_empty() {
            bar = bar.child(
                div()
                    .px_1()
                    .text_size(px(fonts.label))
                    .text_color(theme.muted)
                    .child(self.status.clone()),
            );
        }
        // A cap keeps a very long draft from crowding the transcript; the
        // field scrolls internally past it. The jump buttons ride beside the
        // panel, top aligned with it.
        //
        // Each band gets its own row: a band above the panel (the working
        // indicator) would otherwise push the buttons up with it, and they
        // must stay on the panel's top edge.
        let mut dock = div()
            .w_full()
            .max_w(px(COMPOSER_MAX_W + 2.0 * JUMP_GUTTER))
            .flex()
            .flex_col();
        if self.is_working() {
            dock = dock.child(dock_slot(self.working_row(fonts)));
        }
        let mut panel_row = dock_slot(bar);
        // The prompts jumper shows whenever there is a prompt to jump to;
        // the tail arrow only while unpinned.
        let has_prompts = self
            .transcript
            .items
            .iter()
            .any(|item| matches!(item.kind, ItemKind::User { .. }));
        if has_prompts || self.prompts_open {
            panel_row = panel_row.child(self.jump_cluster(fonts, cx));
        }
        // The dock centers in the pane, so the panel itself stays centered
        // while the buttons hang in the gutter beside it.
        dock = dock.child(panel_row);
        div()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .max_h(relative(0.5))
            .px_4()
            .pt_1()
            .pb_4()
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .justify_center()
                    .child(dock),
            )
    }

    /// Whether the scroll-to-bottom floater is needed: the tail is not
    /// pinned.
    fn needs_jump_button(&self) -> bool {
        !self.list.is_following_tail()
    }

    /// The composer's round button: a stop square while pi is working (it
    /// interrupts the run like pi's own Escape), the send arrow otherwise.
    fn send_button(&self, cx: &mut Context<PilusView>) -> Stateful<Div> {
        let theme = self.theme;
        let stopping = self.is_working();
        // A filled square reads as "stop" at 15px where the stroked outline
        // reads as a rounded box, so the glyph carries weight the send arrow
        // does not need.
        let glyph: &'static [u8] = if stopping {
            include_bytes!("../../assets/icons/square-fill.svg")
        } else {
            include_bytes!("../../assets/icons/arrow-up.svg")
        };
        div()
            .id("session-send")
            .flex()
            .items_center()
            .justify_center()
            .w(px(30.0))
            .h(px(30.0))
            // The solid glyph needs air inside the rim that the thin arrow
            // could spare.
            .p_1()
            .flex_shrink_0()
            .rounded_full()
            .bg(theme.bg)
            .cursor_pointer()
            .hover(move |style| style.bg(theme.border))
            .child(super::icon(glyph, 15.0, theme.panel_fg))
            .on_click(cx.listener(move |this: &mut PilusView, _, _, cx| {
                if stopping {
                    this.session.stop(cx);
                } else {
                    let input = this.session_prompt.clone();
                    this.session.submit(&input, cx, false);
                }
            }))
    }

    /// The jump buttons beside the composer: the prompts menu over a row
    /// with the prompts button and the scroll-to-bottom arrow.
    fn jump_cluster(&self, fonts: Fonts, cx: &mut Context<PilusView>) -> AnyElement {
        let theme = self.theme;
        let circle = |id: &'static str, glyph: &'static [u8]| {
            div()
                .id(id)
                .flex()
                .items_center()
                .justify_center()
                .w(px(30.0))
                .h(px(30.0))
                .flex_shrink_0()
                .rounded_full()
                .bg(theme.panel)
                .cursor_pointer()
                .hover(move |style| style.bg(theme.border))
                .child(super::icon(glyph, 15.0, theme.panel_fg))
        };
        // A narrow column beside the panel; the menu leaves it, so opening
        // one never widens the dock.
        let mut cluster = div()
            .relative()
            .w(px(30.0))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .items_center();
        if self.prompts_open {
            cluster = cluster.child(
                div()
                    .absolute()
                    .bottom(relative(1.0))
                    .right(px(0.0))
                    .pb_2()
                    .child(self.prompts_menu(fonts, cx)),
            );
        }
        let mut row = div().flex().flex_col().items_center().gap_2();
        row = row.child(
            circle(
                "session-prompts-button",
                include_bytes!("../../assets/icons/list-checks.svg"),
            )
            .on_click(cx.listener(|this: &mut PilusView, _, _, cx| {
                this.session.toggle_prompts();
                cx.notify();
            })),
        );
        // The tail arrow rides below the prompts button: it is the one that
        // brings the jumper back to where the conversation is now.
        if self.needs_jump_button() {
            row = row.child(
                circle(
                    "session-jump-bottom",
                    include_bytes!("../../assets/icons/arrow-down.svg"),
                )
                .on_click(cx.listener(|this: &mut PilusView, _, _, cx| {
                    this.session.follow_tail();
                    cx.notify();
                })),
            );
        }
        cluster.child(row).into_any_element()
    }

    /// The prompts menu: every user prompt on one truncated row; choosing
    /// one scrolls it into view.
    fn prompts_menu(&self, fonts: Fonts, cx: &mut Context<PilusView>) -> AnyElement {
        let theme = self.theme;
        let prompts: Vec<(usize, String)> = self
            .transcript
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| match &item.kind {
                ItemKind::User { text, .. } => Some((index, truncate(first_line(text), 48))),
                _ => None,
            })
            .collect();
        let rows = prompts.len().clamp(1, PICKER_MAX_ROWS);
        let list_h = super::picker_rows_h(rows).min(super::PICKER_MENU_MAX_H);
        let mut list = super::picker_menu("pilus-session-prompts", theme)
            .max_h(px(list_h))
            .on_mouse_down_out(cx.listener(|this: &mut PilusView, _, _, cx| {
                this.session.dismiss_prompts();
                cx.notify();
            }));
        if prompts.is_empty() {
            list = list.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .h(px(super::PICKER_ROW_H))
                    .px_3()
                    .text_size(px(fonts.body))
                    .text_color(theme.muted)
                    .child("No prompts yet"),
            );
        }
        for (index, label) in prompts {
            list = list.child(super::picker_row(
                ("session-prompt-option", index),
                label,
                false,
                theme,
                fonts.body,
                cx.listener(move |this: &mut PilusView, _, _, cx| {
                    this.session.prompts_open = false;
                    this.session.prompts_dismissed = false;
                    // Scrolling to a row leaves the tail behind by itself.
                    this.session.jump_to(index);
                    cx.notify();
                }),
            ));
        }
        list.into_any_element()
    }

    /// Toggle the prompts menu from its button. An outside press already
    /// closed it in the capture phase; then the click stays closed.
    fn toggle_prompts(&mut self) {
        if self.prompts_dismissed {
            self.prompts_dismissed = false;
            self.prompts_open = false;
        } else {
            self.prompts_open = !self.prompts_open;
        }
    }

    /// Close the prompts menu from an outside press.
    fn dismiss_prompts(&mut self) {
        self.prompts_dismissed = self.prompts_open;
        self.prompts_open = false;
    }

    /// Whether the session file has grown recently: live work, not a stale
    /// interrupted tail.
    fn recently_active(&self) -> bool {
        self.last_growth
            .is_some_and(|at| at.elapsed() < ACTIVE_WINDOW)
    }

    /// Whether pi is still working: while the socket is up its `busy` is
    /// exact; the heuristics below are the fallback for when it is down.
    fn is_working(&self) -> bool {
        if let Some(state) = &self.pi_state {
            return state.busy;
        }
        if self.sending || self.pending_prompt.is_some() || self.awaiting_response {
            return true;
        }
        // A steer outlives the first thing pi does about it: the indicator
        // rides until the steered turn answers.
        if self.pending_steering {
            return true;
        }
        self.work_in_flight()
    }

    /// Whether the transcript ends mid-work: pi's last word was a tool call,
    /// its result, or its reasoning, so a turn is running.
    fn work_in_flight(&self) -> bool {
        self.transcript
            .items
            .last()
            .is_some_and(|item| is_work(&item.kind))
    }

    /// Whether the steered turn is over: the last thing pi wrote after it is
    /// prose or bookkeeping, with no work still queued.
    ///
    /// The first tool call of a reply is not the end of the turn, so the
    /// optimistic `awaiting_response` window — which clears on any responding
    /// content — cannot decide this. A finished turn still *contains* tool
    /// calls, so the tail's last entry is what settles it; notes count as the
    /// turn ending, because a compaction or a model change is what a turn
    /// leaves behind, never something it is still doing.
    fn steered_turn_answered(&self) -> bool {
        let Some(tail) = self.transcript.items.get(self.pending_from..) else {
            return false;
        };
        // Entries before pi echoed the steer back belong to the turn it
        // interrupted.
        let after_steer = match tail
            .iter()
            .position(|item| matches!(item.kind, ItemKind::User { .. }))
        {
            Some(echo) => &tail[echo + 1..],
            None => tail,
        };
        // Nothing after the steer yet: the turn is still on its way.
        after_steer.last().is_some_and(|item| !is_work(&item.kind))
    }

    /// The `(spinner) Working` line above the composer, or `Steering…` when
    /// the pending prompt went out while pi was already working.
    ///
    /// The frame advances with transcript growth, so it moves while pi works
    /// without needing a repaint timer.
    fn working_row(&self, fonts: Fonts) -> Div {
        const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let frame = FRAMES[self.transcript.items.len() % FRAMES.len()];
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_1()
            .pb_1()
            .text_size(px(fonts.label))
            .text_color(self.theme.muted)
            .child(frame)
            .child(if self.pending_steering {
                "Steering…"
            } else {
                "Working"
            })
    }

    /// Model and thinking pickers, token stats, and the send arrow below the
    /// input. The right side holds only the send button.
    fn meta_row(&self, fonts: Fonts, cx: &mut Context<PilusView>) -> Div {
        let theme = self.theme;
        let transcript = &self.transcript;
        let mut row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .px_2()
            .pt_1()
            .pb_2();
        row = row.child(self.picker_chip(
            "session-model-picker",
            include_bytes!("../../assets/icons/cpu.svg"),
            self.model_display(),
            PickerKind::Model,
            fonts,
            cx,
        ));
        let thinking = match self.current_thinking() {
            Some(level) => pilus_client::effort_label(&level),
            None => "Thinking".to_string(),
        };
        row = row.child(self.picker_chip(
            "session-thinking-picker",
            include_bytes!("../../assets/icons/gauge.svg"),
            thinking,
            PickerKind::Thinking,
            fonts,
            cx,
        ));
        if let Some(context) = self
            .pi_state
            .as_ref()
            .and_then(|state| state.context.as_ref())
        {
            let color = if ctx_warns(context) {
                theme.alert
            } else {
                theme.muted
            };
            row = row.child(
                div()
                    .pl_2()
                    .text_size(px(fonts.label))
                    .text_color(color)
                    .whitespace_nowrap()
                    .child(ctx_label(context)),
            );
        }
        if let Some(stats) = usage_stats(&transcript.usage) {
            row = row.child(
                div()
                    .pl_2()
                    .text_size(px(fonts.label))
                    .text_color(theme.muted)
                    .whitespace_nowrap()
                    .child(stats),
            );
        }
        row = row.child(div().flex_1());
        row = row.child(self.send_button(cx));
        row
    }

    /// The model chip's label: the catalog's display name (`Muse Spark 1.3
    /// Contributor`) when the current `provider/id` slug is known, else the
    /// slug itself. The extension's report is exact; the transcript fold is
    /// the fallback while the socket is down.
    fn model_display(&self) -> String {
        if let Some(model) = self
            .pi_state
            .as_ref()
            .and_then(|state| state.model.as_ref())
        {
            return if model.name.is_empty() {
                model.id.clone()
            } else {
                model.name.clone()
            };
        }
        let current = self.transcript.model.clone().unwrap_or_default();
        if current.is_empty() {
            return "model".to_string();
        }
        for model in &self.catalog {
            let value = if model.provider.is_empty() {
                model.id.clone()
            } else {
                format!("{}/{}", model.provider, model.id)
            };
            if value == current && !model.name.is_empty() {
                return model.name.clone();
            }
        }
        current
    }

    /// The `provider/id` slug the model picker marks current, from the
    /// extension's report when it is up.
    fn current_model_slug(&self) -> String {
        if let Some(model) = self
            .pi_state
            .as_ref()
            .and_then(|state| state.model.as_ref())
        {
            return if model.provider.is_empty() {
                model.id.clone()
            } else {
                format!("{}/{}", model.provider, model.id)
            };
        }
        self.transcript.model.clone().unwrap_or_default()
    }

    /// The thinking level the picker marks current, from the extension's
    /// report when it is up.
    fn current_thinking(&self) -> Option<String> {
        self.pi_state
            .as_ref()
            .and_then(|state| state.thinking.clone())
            .or_else(|| self.transcript.thinking_level.clone())
    }

    /// One picker chip, drawn with the composer face Home uses: a leading
    /// glyph, the value, and a chevron, highlighting while open.
    fn picker_chip(
        &self,
        id: &'static str,
        glyph: &'static [u8],
        label: String,
        kind: PickerKind,
        fonts: Fonts,
        cx: &mut Context<PilusView>,
    ) -> Stateful<Div> {
        let theme = self.theme;
        let open = self.picker == Some(kind);
        let fg = if open { theme.panel_fg } else { theme.muted };
        let mut chip = super::picker_face(
            id,
            super::icon(glyph, 14.0, fg).into_any_element(),
            label,
            220.0,
            open,
            // The chips sit on the composer's panel fill, so the open chip
            // takes the window background to stand out from it.
            theme.bg,
            theme,
            fonts.secondary,
            cx.listener(move |this: &mut PilusView, _, _, cx| {
                this.session.toggle_picker(kind);
                cx.notify();
            }),
        );
        if open {
            chip = chip.relative().child(
                div()
                    .absolute()
                    .top(px(0.0))
                    .left(px(0.0))
                    .child(self.picker_list(kind, fonts, cx)),
            );
        }
        chip
    }

    /// The open picker's options, floated above its chip.
    ///
    /// Rows are a fixed height and the list an exact pixel height, so the
    /// anchored offset opens it upward without shifting the composer.
    fn picker_list(
        &self,
        kind: PickerKind,
        fonts: Fonts,
        cx: &mut Context<PilusView>,
    ) -> AnyElement {
        let theme = self.theme;
        let current_model = self.current_model_slug();
        let current_thinking = self.current_thinking().unwrap_or_default();
        // One option per model: the catalog's `provider/id` value under its
        // display name, then any session model the catalog does not know.
        // Each entry is `(label, /model argument, is-current)`.
        let options: Vec<(String, String, bool)> = match kind {
            PickerKind::Model => {
                let mut options = Vec::new();
                for model in &self.catalog {
                    let value = if model.provider.is_empty() {
                        model.id.clone()
                    } else {
                        format!("{}/{}", model.provider, model.id)
                    };
                    if value.is_empty() || options.iter().any(|(_, known, _)| *known == value) {
                        continue;
                    }
                    let label = if model.name.is_empty() {
                        value.clone()
                    } else {
                        model.name.clone()
                    };
                    options.push((label, value.clone(), value == current_model));
                }
                for model in &self.transcript.models {
                    if options.iter().any(|(_, known, _)| *known == *model) {
                        continue;
                    }
                    options.push((model.clone(), model.clone(), *model == current_model));
                }
                options
            }
            PickerKind::Thinking => THINKING_LEVELS
                .iter()
                .map(|level| {
                    (
                        level.to_string(),
                        level.to_string(),
                        *level == current_thinking,
                    )
                })
                .collect(),
        };
        // One row even when empty, so the placeholder still measures right.
        let rows = options.len().clamp(1, PICKER_MAX_ROWS);
        let list_h = super::picker_rows_h(rows).min(super::PICKER_MENU_MAX_H);
        let mut list = super::picker_menu("pilus-session-picker", theme)
            .max_h(px(list_h))
            .on_mouse_down_out(cx.listener(|this: &mut PilusView, _, _, cx| {
                this.session.dismiss_picker();
                cx.notify();
            }));
        if options.is_empty() {
            list = list.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .h(px(super::PICKER_ROW_H))
                    .px_3()
                    .text_size(px(fonts.body))
                    .text_color(theme.muted)
                    .child("No models available"),
            );
        }
        for (index, (label, value, current)) in options.iter().enumerate().take(PICKER_MAX_ROWS) {
            let argument = value.clone();
            list = list.child(super::picker_row(
                ("session-picker-option", index),
                label.clone(),
                *current,
                theme,
                fonts.body,
                // A choice goes to the live agent as a slash command and
                // surfaces in the transcript when pi records the change.
                cx.listener(move |this: &mut PilusView, _, _, cx| {
                    let command = match kind {
                        PickerKind::Model => format!("/model {argument}"),
                        PickerKind::Thinking => format!("/thinking {argument}"),
                    };
                    this.session.send_command(command, cx);
                    cx.notify();
                }),
            ));
        }
        deferred(
            anchored()
                .anchor(Anchor::TopLeft)
                .offset(point(px(0.0), px(-(list_h + 6.0))))
                .snap_to_window_with_margin(px(8.0))
                .child(list),
        )
        .with_priority(1)
        .into_any_element()
    }

    /// The whole panel: header, transcript, sticky composer.
    pub fn render(
        &mut self,
        theme: Theme,
        settings: &Rc<RefCell<Settings>>,
        input: &Entity<TextInput>,
        cx: &mut Context<PilusView>,
    ) -> AnyElement {
        self.theme = theme;
        self.settings = settings.clone();
        let fonts = Fonts::resolve(settings);
        // Drop the optimistic prompt once pi appends the real user message
        // (only matches at or after the send point, so resending the same
        // text cannot clear the new bubble early).
        if let Some(prompt) = self.pending_prompt.clone() {
            let landed = self.transcript.items.iter().skip(self.pending_from).any(
                |item| matches!(&item.kind, ItemKind::User { text, .. } if text.trim() == prompt),
            );
            if landed {
                self.pending_prompt = None;
            }
        }
        // The optimistic indicator persists until pi's first responding
        // content — an assistant message or anything it produced — lands.
        if self.awaiting_response {
            let responded = self
                .transcript
                .items
                .iter()
                .skip(self.pending_from)
                .any(|item| {
                    matches!(
                        &item.kind,
                        ItemKind::Assistant { .. }
                            | ItemKind::Thinking { .. }
                            | ItemKind::ToolCall { .. }
                            | ItemKind::ToolResult { .. }
                            | ItemKind::Bash { .. }
                    )
                });
            if responded {
                self.awaiting_response = false;
            }
        }
        // The steering label rides until pi answers the steer. Clearing it
        // with `awaiting_response` hid it on the first tool call, while the
        // agent was still several turns from done.
        if self.pending_steering && self.steered_turn_answered() {
            self.pending_steering = false;
        }
        // After the optimistic bookkeeping, so a landed prompt drops its
        // entry in the same frame its real bubble appears.
        self.sync_list();
        let header = self.header(fonts);
        let body = self.body(fonts, cx);
        let composer = self.composer(input, fonts, cx);
        div()
            .flex()
            .flex_col()
            .size_full()
            .min_h(px(0.0))
            .child(header)
            .child(body)
            .child(composer)
            .into_any_element()
    }
}

/// The background reader: resolve the session, then tail it until stopped.
///
/// A reported session id can precede its file — pi creates the file lazily,
/// at the first user message — so a resolution miss is retried on the poll
/// interval rather than raised as an error. Only real IO and SSH failures
/// surface.
fn reader_loop(
    endpoint: Endpoint,
    explicit: Option<PathBuf>,
    session_id: Option<String>,
    cache: SessionCache,
    tx: Sender<Update>,
    stop: Arc<AtomicBool>,
) {
    let interval = match endpoint {
        Endpoint::Local { .. } => LOCAL_POLL,
        Endpoint::Remote { .. } => REMOTE_POLL,
    };
    let (mut tail, key) = loop {
        match open_tail(&endpoint, explicit.clone(), session_id.as_deref(), &cache) {
            Ok(Some(opened)) => break opened,
            Ok(None) => {
                if sleep_sliced(interval, &stop) {
                    return;
                }
            }
            Err(error) => {
                let _ = tx.send(Update::Error(error.to_string()));
                return;
            }
        }
    };
    // Send the initial fold immediately so the panel is not blank for a tick.
    if !send_delta(&tx, &tail, 0) {
        cache.put(&key, tail);
        return;
    }
    // A reused tail's catch-up poll may have marked an index that the full
    // snapshot already covers; drain it so the next delta is not inflated.
    tail.take_changed_from();
    while !stop.load(Ordering::Relaxed) {
        if sleep_sliced(interval, &stop) {
            break;
        }
        match tail.poll() {
            Ok(true) => {
                // A metadata-only fold marks nothing, so `from` lands at the
                // end and the delta carries `meta` alone.
                let from = tail
                    .take_changed_from()
                    .unwrap_or_else(|| tail.transcript().items.len());
                if !send_delta(&tx, &tail, from) {
                    break;
                }
            }
            Ok(false) => {}
            Err(error) => {
                let _ = tx.send(Update::Error(error.to_string()));
                return;
            }
        }
    }
    // The panel moved on; park the tail so a switch back reads only the
    // bytes appended in between. An error exit above parks nothing: the
    // tail can no longer be trusted.
    cache.put(&key, tail);
}

/// Push one delta: every item at or after `from`, plus the fold's metadata.
/// Returns false when the window has gone away.
fn send_delta(tx: &Sender<Update>, tail: &Tail, from: usize) -> bool {
    let transcript = tail.transcript();
    let from = from.min(transcript.items.len());
    tx.send(Update::Delta(Box::new(Delta {
        from,
        items: transcript.items[from..].to_vec(),
        meta: transcript.meta(),
    })))
    .is_ok()
}

/// The node one-liner that relays a remote socket onto stdio. `shell` pins
/// /bin/sh and the sidecar publishes pi's own node binary, so this runs
/// wherever the extension does.
const RELAY_JS: &str = "const s=require(\"net\").connect(process.argv[1]);\
     process.stdin.pipe(s);s.pipe(process.stdout);\
     s.on(\"error\",()=>process.exit(1));s.on(\"close\",()=>process.exit(0));";

/// Wait between reconnect attempts: `/reload` restarts pi's socket, and a
/// pi without the v2 extension should not spin the thread.
const RPC_BACKOFF: Duration = Duration::from_secs(2);
/// How long the RPC loop waits on a queued command before checking events
/// and the stop flag.
const RPC_COMMAND_WAIT: Duration = Duration::from_millis(50);

/// A bidirectional RPC stream plus the relay child to kill on teardown.
struct RpcConn {
    lines: BufReader<Box<dyn Read + Send>>,
    writer: Box<dyn Write + Send>,
    child: Option<Child>,
}

/// One socket line folded for the loop: stream messages become items here,
/// on the reader thread, so the GUI never sees pi's raw JSON.
enum Incoming {
    Event(RpcEvent),
    Stream(Vec<Item>),
    /// The socket closed or errored; the session is over.
    Closed,
}

/// Which request a `response` answers.
fn request_kind(command: &RpcCommand) -> RequestKind {
    match command {
        RpcCommand::Prompt { .. } => RequestKind::Prompt,
        RpcCommand::Abort => RequestKind::Abort,
        RpcCommand::SetModel { .. } => RequestKind::SetModel,
        RpcCommand::SetThinking { .. } => RequestKind::SetThinking,
        RpcCommand::Models => RequestKind::Models,
        RpcCommand::State => RequestKind::State,
    }
}

/// Connect to the extension socket at `path` (a temp path in tests).
fn connect_local(path: &Path) -> Result<RpcConn, String> {
    let stream = UnixStream::connect(path).map_err(|error| error.to_string())?;
    let reader = stream
        .try_clone()
        .map_err(|error| error.to_string())?;
    Ok(RpcConn {
        lines: BufReader::new(Box::new(reader)),
        writer: Box::new(stream),
        child: None,
    })
}

/// Reach the extension socket on a machine: read the sidecar pi wrote for
/// the socket's path and its node binary, then relay stdio onto the socket.
fn connect_remote(target: &SshTarget, session_id: &str) -> Result<RpcConn, String> {
    if !safe_session_id(session_id) {
        return Err(format!("pi reported an unusable session id `{session_id}`"));
    }
    let output = target
        .shell(&format!(
            "cat \"${{PI_CODING_AGENT_DIR:-$HOME/.pi/agent}}/pilus/{session_id}.json\""
        ))
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!("no extension socket on {}", target.destination));
    }
    let sidecar = Sidecar::parse(&String::from_utf8_lossy(&output.stdout))
        .ok_or_else(|| format!("unreadable extension sidecar on {}", target.destination))?;
    let mut child = target
        .shell(&format!(
            "exec {} -e {} {}",
            shell_quote(&sidecar.exec_path),
            shell_quote(RELAY_JS),
            shell_quote(&sidecar.socket),
        ))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| error.to_string())?;
    let writer = child
        .stdin
        .take()
        .ok_or_else(|| "the relay opened without stdin".to_string())?;
    let reader = child
        .stdout
        .take()
        .ok_or_else(|| "the relay opened without stdout".to_string())?;
    Ok(RpcConn {
        lines: BufReader::new(Box::new(reader)),
        writer: Box::new(writer),
        child: Some(child),
    })
}

/// The extension channel's owner: connect, run one session, and reconnect
/// on failure. A missing socket just means pi has not loaded the extension
/// yet — the file tail keeps the panel honest meanwhile.
fn rpc_loop(
    endpoint: Endpoint,
    session_id: String,
    update: Sender<Update>,
    commands: Receiver<RpcCommand>,
    stop: Arc<AtomicBool>,
) {
    // One receiver survives every reconnect; the Mutex only exists because
    // `Receiver` is not `Sync`, never because two threads compete for it.
    let commands = Mutex::new(commands);
    while !stop.load(Ordering::Relaxed) {
        let conn = match &endpoint {
            Endpoint::Local { .. } => {
                connect_local(&socket_path(&agent_dir(), &session_id))
            }
            Endpoint::Remote { target, .. } => connect_remote(target, &session_id),
        };
        match conn {
            Ok(conn) => rpc_session(conn, &update, &commands, &stop),
            Err(_) => {
                if sleep_sliced(RPC_BACKOFF, &stop) {
                    return;
                }
            }
        }
    }
}

/// Run one connected session until the socket drops or the panel stops.
///
/// A reader thread folds socket lines into [`Incoming`]; this loop drains
/// them between 50ms command waits so events and replies interleave the
/// way pi sends them.
fn rpc_session(
    conn: RpcConn,
    update: &Sender<Update>,
    commands: &Mutex<Receiver<RpcCommand>>,
    stop: &AtomicBool,
) {
    let RpcConn {
        lines,
        mut writer,
        mut child,
    } = conn;
    let (incoming_tx, incoming) = std::sync::mpsc::channel::<Incoming>();
    std::thread::spawn(move || {
        for line in lines.lines() {
            let Ok(line) = line else {
                break;
            };
            let incoming = match parse_event(&line) {
                Some(RpcEvent::Stream(message)) => Incoming::Stream(live_items(&message)),
                Some(event) => Incoming::Event(event),
                None => continue,
            };
            if incoming_tx.send(incoming).is_err() {
                return;
            }
        }
        let _ = incoming_tx.send(Incoming::Closed);
    });
    let mut up = false;
    let mut next_id = 0u64;
    let mut pending: HashMap<String, RequestKind> = HashMap::new();
    'session: loop {
        let command = commands
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .recv_timeout(RPC_COMMAND_WAIT);
        match command {
            Ok(command) => {
                next_id += 1;
                let id = next_id.to_string();
                if writeln!(writer, "{}", command.to_line(&id)).is_err() {
                    break;
                }
                pending.insert(id, request_kind(&command));
            }
            Err(RecvTimeoutError::Timeout) => {}
            // The panel went away.
            Err(RecvTimeoutError::Disconnected) => break,
        }
        while let Ok(incoming) = incoming.try_recv() {
            match incoming {
                Incoming::Closed => break 'session,
                Incoming::Stream(items) => {
                    if update.send(Update::RpcStream(items)).is_err() {
                        break 'session;
                    }
                }
                Incoming::Event(RpcEvent::Hello { .. }) => {
                    up = true;
                    if update.send(Update::RpcUp).is_err() {
                        break 'session;
                    }
                    // Refill the picker's catalog from the live agent.
                    next_id += 1;
                    let id = next_id.to_string();
                    if writeln!(writer, "{}", RpcCommand::Models.to_line(&id)).is_err() {
                        break 'session;
                    }
                    pending.insert(id, RequestKind::Models);
                }
                Incoming::Event(RpcEvent::State(state)) => {
                    if update.send(Update::RpcState(state)).is_err() {
                        break 'session;
                    }
                }
                Incoming::Event(RpcEvent::StreamEnd) => {
                    if update.send(Update::RpcStreamEnd).is_err() {
                        break 'session;
                    }
                }
                Incoming::Event(RpcEvent::Stream(_)) => {
                    // The reader folds streams into items; unreachable.
                }
                Incoming::Event(RpcEvent::Tool(tool)) => {
                    if update.send(Update::RpcTool(*tool)).is_err() {
                        break 'session;
                    }
                }
                Incoming::Event(RpcEvent::Response {
                    id,
                    ok,
                    error,
                    data,
                }) => {
                    let Some(kind) = pending.remove(&id) else {
                        continue;
                    };
                    let result = if ok {
                        Ok(data)
                    } else {
                        Err(error.unwrap_or_else(|| "request failed".to_string()))
                    };
                    if update.send(Update::RpcResult { kind, result }).is_err() {
                        break 'session;
                    }
                }
            }
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
    }
    if let Some(mut child) = child.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    // Only a session that got up announces it is down; a failed connect is
    // invisible to the panel.
    if up {
        let _ = update.send(Update::RpcDown);
    }
}

/// Sleep `total` in short slices so `stop` is seen quickly — a quick A→B→A
/// switch must not wait out a whole poll interval to reuse its parked tail.
/// Returns whether the sleep ended on `stop`.
fn sleep_sliced(total: Duration, stop: &AtomicBool) -> bool {
    const SLICE: Duration = Duration::from_millis(25);
    let deadline = Instant::now() + total;
    loop {
        if stop.load(Ordering::Relaxed) {
            return true;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        std::thread::sleep(remaining.min(SLICE));
    }
}

/// Resolve and open the session's byte source.
///
/// `Ok(None)` is "not yet": the pane reported a session id pi has not
/// written a file for. The reader retries it on the poll interval. The
/// returned key is the tail's cache slot; a parked tail under it is reused
/// after a `poll` catches it up on the bytes appended while it sat.
fn open_tail(
    endpoint: &Endpoint,
    explicit: Option<PathBuf>,
    session_id: Option<&str>,
    cache: &SessionCache,
) -> Result<Option<(Tail, String)>, SessionError> {
    fn open_or_reuse(
        cache: &SessionCache,
        key: String,
        open: impl FnOnce() -> Result<Tail, SessionError>,
    ) -> Result<(Tail, String), SessionError> {
        if let Some(mut tail) = cache.take(&key) {
            tail.poll()?;
            return Ok((tail, key));
        }
        Ok((open()?, key))
    }

    match endpoint {
        Endpoint::Local { cwd, .. } => {
            let path = match explicit {
                Some(path) => Some(path),
                None => resolve_local(cwd, session_id),
            };
            match path {
                Some(path) => open_or_reuse(cache, path.to_string_lossy().into_owned(), || {
                    Tail::open(&path)
                })
                .map(Some),
                None => Ok(None),
            }
        }
        Endpoint::Remote { target, .. } => {
            let Some(path) = resolve_remote(target, session_id)? else {
                return Ok(None);
            };
            open_or_reuse(
                cache,
                format!("{}:{path}", target.destination),
                || {
                    Tail::with_source(Box::new(SshSource {
                        target: target.clone(),
                        path,
                    }))
                },
            )
            .map(Some)
        }
    }
}

/// Find the local session file for the reported id, or the cwd's newest
/// when no id is known (`--session-ui <cwd>` only).
fn resolve_local(cwd: &str, session_id: Option<&str>) -> Option<PathBuf> {
    let root = pilus_session::sessions_root();
    match session_id {
        Some(id) => pilus_session::find_by_id(&root, cwd, id)
            .or_else(|| pilus_session::find_any_by_id(&root, id)),
        None => pilus_session::latest(&root, cwd),
    }
}

/// Whether `id` is safe to interpolate into a remote glob: the same
/// character set the daemon's `session_id` schema accepts.
fn safe_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'))
}

/// Find the remote session file over SSH for the reported id.
///
/// `Ok(None)` means the glob matched nothing: pi has not created the file
/// yet.
fn resolve_remote(
    target: &SshTarget,
    session_id: Option<&str>,
) -> Result<Option<String>, SessionError> {
    let glob = match session_id {
        Some(id) => {
            if !safe_session_id(id) {
                return Err(SessionError::Source(format!(
                    "pi reported an unusable session id `{id}`"
                )));
            }
            format!("*_{id}.jsonl")
        }
        None => "*.jsonl".to_string(),
    };
    let command = format!(
        "root=\"${{PI_CODING_AGENT_SESSION_DIR:-$HOME/.pi/agent/sessions}}\"; \
         ls -t \"$root\"/*/{glob} 2>/dev/null | head -n1"
    );
    let output = target
        .shell(&command)
        .output()
        .map_err(|error| SessionError::Source(error.to_string()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(SessionError::Source(if stderr.is_empty() {
            format!("could not list sessions on {}", target.destination)
        } else {
            stderr
        }));
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        return Ok(None);
    }
    Ok(Some(path))
}

/// A session file on another machine, read through one SSH command per poll.
struct SshSource {
    target: SshTarget,
    path: String,
}

impl Source for SshSource {
    fn byte_len(&mut self) -> Result<u64, SessionError> {
        let command = format!("wc -c < {}", shell_quote(&self.path));
        let output = self
            .target
            .shell(&command)
            .output()
            .map_err(|error| SessionError::Source(error.to_string()))?;
        if !output.status.success() {
            return Err(SessionError::Source(self.failure(&output.stderr)));
        }
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .map_err(|_| SessionError::Source("could not size the remote session".to_string()))
    }

    fn read_from(&mut self, offset: u64) -> Result<Vec<u8>, SessionError> {
        // `tail -c +N` is 1-based, so the first unread byte is `offset + 1`.
        let command = format!("tail -c +{} {}", offset + 1, shell_quote(&self.path));
        let output = self
            .target
            .shell(&command)
            .output()
            .map_err(|error| SessionError::Source(error.to_string()))?;
        if !output.status.success() {
            return Err(SessionError::Source(self.failure(&output.stderr)));
        }
        Ok(output.stdout)
    }

    fn label(&self) -> String {
        format!("{}:{}", self.target.destination, self.path)
    }
}

impl SshSource {
    /// The error text for a failed remote command.
    fn failure(&self, stderr: &[u8]) -> String {
        let stderr = String::from_utf8_lossy(stderr).trim().to_string();
        if stderr.is_empty() {
            format!(
                "could not read {} on {}",
                self.path, self.target.destination
            )
        } else {
            stderr
        }
    }
}

/// A body clipped to `max` lines, marked when clipped.
fn clip_lines(text: &str, max: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max {
        return text.to_string();
    }
    let mut shown = lines[..max].join("\n");
    shown.push_str("\n… (truncated)");
    shown
}

/// Whether an item is part of a stretch of work between assistant replies.
fn is_work(kind: &ItemKind) -> bool {
    matches!(
        kind,
        ItemKind::Thinking { .. }
            | ItemKind::ToolCall { .. }
            | ItemKind::ToolResult { .. }
            | ItemKind::Bash { .. }
    )
}

/// The header for a work run: `Working for 10m 3s`, or `Working…` while it is
/// still the last thing in the transcript (or the span is under a second).
fn work_label(run: &[Item], trailing: bool) -> String {
    let duration = if trailing { None } else { work_duration(run) };
    match duration {
        Some(millis) if millis >= 1000 => format!("Working for {}", format_duration(millis)),
        _ => "Working…".to_string(),
    }
}

/// The wall-clock span of a work run, from its first to its last entry.
fn work_duration(run: &[Item]) -> Option<i64> {
    let first = parse_iso_millis(&run.first()?.timestamp)?;
    let last = parse_iso_millis(&run.last()?.timestamp)?;
    Some((last - first).max(0))
}

/// Parse pi's ISO-8601 entry timestamp (`2024-12-03T14:00:04.000Z`) to Unix
/// milliseconds. Returns `None` for an unexpected shape, so a label degrades
/// to `Working…` rather than showing a wrong duration.
fn parse_iso_millis(timestamp: &str) -> Option<i64> {
    if timestamp.len() < 19 {
        return None;
    }
    let field = |range: std::ops::Range<usize>| timestamp.get(range)?.parse::<i64>().ok();
    let year = field(0..4)?;
    let month = field(5..7)?;
    let day = field(8..10)?;
    let hour = field(11..13)?;
    let minute = field(14..16)?;
    let second = field(17..19)?;
    let millis = timestamp
        .get(20..23)
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(0);
    let days = days_from_civil(year, month, day);
    Some((((days * 24 + hour) * 60 + minute) * 60 + second) * 1000 + millis)
}

/// Days since the Unix epoch for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// `45s`, `10m 3s`, or `1h 2m`.
fn format_duration(millis: i64) -> String {
    let seconds = millis / 1000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

/// The smaller muted scale for asides: thinking and structural notes.
fn aside_fonts(fonts: Fonts) -> Fonts {
    Fonts {
        body: fonts.secondary,
        secondary: fonts.label,
        label: fonts.label,
    }
}

/// `branch summary` to `Branch summary`.
fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// `in 5.9M • out 449.9k • cache 96% ($1.532)`, or `None` before any usage
/// arrives.
///
/// The session file records input, output, and cache reads/writes (plus
/// cost), but no context window, so that is the whole breakdown.
fn usage_stats(usage: &Usage) -> Option<String> {
    if usage.total == 0 {
        return None;
    }
    let mut stats = format!(
        "in {} • out {}",
        fmt_count(usage.input),
        fmt_count(usage.output)
    );
    let seen = usage.input + usage.cache_read;
    if seen > 0 {
        let hit = (usage.cache_read as f64 / seen as f64 * 100.0).round() as u64;
        stats.push_str(&format!(" • cache {hit}%"));
    }
    stats.push_str(&format!(" ({})", fmt_cost(usage.cost)));
    Some(stats)
}

/// `$1.532`, `$0.0168`: four decimals without trailing zeros.
fn fmt_cost(cost: f64) -> String {
    format!("${cost:.4}")
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
}

/// `4790` stays, `12_300` becomes `12.3k`, `2_400_000` becomes `2.4M`.
fn fmt_count(value: u64) -> String {
    if value < 1000 {
        value.to_string()
    } else if value < 1_000_000 {
        format!("{}k", short_div(value as f64 / 1000.0))
    } else {
        format!("{}M", short_div(value as f64 / 1_000_000.0))
    }
}

/// One decimal, without the `.0`.
fn short_div(value: f64) -> String {
    format!("{value:.1}").trim_end_matches(".0").to_string()
}

/// A short, tool-aware label for a tool call, in prose: `Read src/auth.rs`.
fn tool_summary(name: &str, arguments: &Value) -> String {
    let text = |key: &str| arguments.get(key).and_then(Value::as_str).unwrap_or("");
    match name {
        "read" => with_target("Read", text("path"), name, arguments),
        "edit" => with_target("Edited", text("path"), name, arguments),
        "write" => with_target("Wrote", text("path"), name, arguments),
        "bash" => with_target("Ran", first_line(text("command")), name, arguments),
        "grep" | "find" | "glob" => {
            let target = text("pattern");
            let target = if target.is_empty() {
                text("path")
            } else {
                target
            };
            with_target("Searched", target, name, arguments)
        }
        _ => match first_string_arg(arguments) {
            Some(value) => format!("{name} · {}", truncate(&value, 60)),
            None => name.to_string(),
        },
    }
}

/// `Read src/auth.rs`, falling back to the old `name · detail` shape when
/// the key argument is missing.
fn with_target(verb: &str, target: &str, name: &str, arguments: &Value) -> String {
    if target.is_empty() {
        return match first_string_arg(arguments) {
            Some(value) => format!("{name} · {}", truncate(&value, 60)),
            None => name.to_string(),
        };
    }
    format!("{verb} {}", truncate(target, 80))
}

/// What a tool call did, for prose group summaries.
#[derive(Clone, Copy, PartialEq)]
enum CallKind {
    Read,
    Edit,
    Write,
    Command,
    Search,
    Other,
}

fn call_kind(name: &str) -> CallKind {
    match name {
        "read" => CallKind::Read,
        "edit" => CallKind::Edit,
        "write" => CallKind::Write,
        "bash" => CallKind::Command,
        "grep" | "find" | "glob" => CallKind::Search,
        _ => CallKind::Other,
    }
}

/// `Read 3 files, ran 1 search, ran 1 command`: a prose count of a run of
/// tool calls for the group header.
fn summarize_calls(calls: &[Item]) -> String {
    let mut counts = [0usize; 6];
    for call in calls {
        if let ItemKind::ToolCall { name, .. } = &call.kind {
            counts[call_kind(name) as usize] += 1;
        }
    }
    let phrases = [
        phrase("Read", counts[CallKind::Read as usize], "file", "files"),
        phrase("Edited", counts[CallKind::Edit as usize], "file", "files"),
        phrase("Wrote", counts[CallKind::Write as usize], "file", "files"),
        phrase(
            "Ran",
            counts[CallKind::Search as usize],
            "search",
            "searches",
        ),
        phrase(
            "Ran",
            counts[CallKind::Command as usize],
            "command",
            "commands",
        ),
        phrase("Used", counts[CallKind::Other as usize], "tool", "tools"),
    ];
    let mut summary = String::new();
    for phrase in phrases.into_iter().flatten() {
        if !summary.is_empty() {
            summary.push_str(", ");
            summary.push_str(&lower_first(&phrase));
        } else {
            summary.push_str(&phrase);
        }
    }
    if summary.is_empty() {
        format!("{} tool calls", calls.len())
    } else {
        summary
    }
}

/// `Read 3 files`, or `None` when there is nothing to count.
fn phrase(verb: &str, count: usize, singular: &str, plural: &str) -> Option<String> {
    if count == 0 {
        return None;
    }
    let noun = if count == 1 { singular } else { plural };
    Some(format!("{verb} {count} {noun}"))
}

/// `Read` to `read`.
fn lower_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// A tool call's arguments and result, formatted for the monospace body.
fn tool_details(name: &str, arguments: &Value, outcome: Option<&ToolOutcome>) -> String {
    let text = |key: &str| {
        arguments
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let mut body = match name {
        "bash" => {
            let mut body = String::new();
            if let Some(command) = text("command") {
                body.push_str(&format!("$ {command}"));
            }
            if let Some(timeout) = arguments.get("timeout").and_then(Value::as_u64) {
                body.push_str(&format!("\ntimeout: {timeout}s"));
            }
            body
        }
        "read" => {
            let mut body = text("path").unwrap_or_default();
            let limit = number_arg(arguments.get("limit"));
            let offset = number_arg(arguments.get("offset"));
            if limit != "-" || offset != "-" {
                body.push_str(&format!("\nlimit: {limit}   offset: {offset}"));
            }
            body
        }
        "edit" => {
            let mut body = text("path").unwrap_or_default();
            if let Some(edits) = arguments.get("edits").and_then(Value::as_array) {
                for (index, edit) in edits.iter().enumerate() {
                    body.push_str(&format!("\n\nedit {}:", index + 1));
                    if let Some(old) = edit.get("oldText").and_then(Value::as_str) {
                        body.push_str(&format!("\n- {old}"));
                    }
                    if let Some(new) = edit.get("newText").and_then(Value::as_str) {
                        body.push_str(&format!("\n+ {new}"));
                    }
                }
            }
            body
        }
        "write" => {
            let mut body = text("path").unwrap_or_default();
            if let Some(content) = text("content") {
                body.push_str("\n\n");
                body.push_str(&content);
            }
            body
        }
        _ => serde_json::to_string_pretty(arguments).unwrap_or_default(),
    };
    if let Some(outcome) = outcome
        && !outcome.text.is_empty()
    {
        if !body.trim().is_empty() {
            body.push_str("\n\n");
        }
        body.push_str(&outcome.text);
    }
    body
}

/// The first non-empty string value in an argument object.
fn first_string_arg(arguments: &Value) -> Option<String> {
    let object = arguments.as_object()?;
    object
        .values()
        .find_map(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// An argument's integer as text, or `-` when absent.
fn number_arg(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_i64)
        .map(|number| number.to_string())
        .unwrap_or_else(|| "-".to_string())
}

/// The first line of `text`, for a one-line command preview.
fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or(text).trim()
}

/// Quote a path for a remote shell command.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Clamp a long single-line note so one entry cannot fill the window.
fn truncate(text: &str, max: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// Expand a leading `~` locally; a session file is a local path.
fn expand_local(path: &str) -> String {
    pilus_client::expand_local(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_marks_clipped_text() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdefghij", 4), "abcd…");
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("/tmp/a b.jsonl"), "'/tmp/a b.jsonl'");
        assert_eq!(shell_quote("/tmp/it's.jsonl"), "'/tmp/it'\\''s.jsonl'");
    }

    #[test]
    fn clip_lines_marks_the_overflow() {
        let text = "a\nb\nc\nd";
        assert_eq!(clip_lines(text, 10), text);
        assert_eq!(clip_lines(text, 2), "a\nb\n… (truncated)");
    }

    fn push(panel: &mut SessionPanel, kind: ItemKind) {
        let id = panel.transcript.items.len().to_string();
        panel.transcript.items.push(Item {
            id,
            timestamp: String::new(),
            kind,
        });
        panel.rows_dirty = true;
    }

    /// [User, Thinking, Thinking, Assistant, ToolCall, ToolResult, Bash,
    /// Assistant, Thinking]: the grouping `body()` used to do inline.
    fn grouped_transcript(panel: &mut SessionPanel) {
        push(
            panel,
            ItemKind::User {
                text: "prompt".to_string(),
                images: 0,
            },
        );
        push(
            panel,
            ItemKind::Thinking {
                text: "a".to_string(),
            },
        );
        push(
            panel,
            ItemKind::Thinking {
                text: "b".to_string(),
            },
        );
        push(
            panel,
            ItemKind::Assistant {
                text: "checking".to_string(),
                model: None,
            },
        );
        push(
            panel,
            ItemKind::ToolCall {
                call_id: "c1".to_string(),
                name: "read".to_string(),
                arguments: Value::Null,
                outcome: None,
            },
        );
        push(
            panel,
            ItemKind::ToolResult {
                call_id: "c1".to_string(),
                name: "read".to_string(),
                text: String::new(),
                images: 0,
                is_error: false,
            },
        );
        push(
            panel,
            ItemKind::Bash {
                command: "ls".to_string(),
                output: String::new(),
                exit_code: Some(0),
                cancelled: false,
                truncated: false,
            },
        );
        push(
            panel,
            ItemKind::Assistant {
                text: "done".to_string(),
                model: None,
            },
        );
        push(
            panel,
            ItemKind::Thinking {
                text: "c".to_string(),
            },
        );
    }

    #[test]
    fn rows_group_work_runs_like_the_transcript() {
        let mut panel = SessionPanel::default();
        grouped_transcript(&mut panel);
        assert_eq!(
            build_rows(&panel.transcript.items),
            vec![
                Row {
                    start: 0,
                    end: 1,
                    kind: RowKind::Item
                },
                Row {
                    start: 1,
                    end: 3,
                    kind: RowKind::Thinking
                },
                Row {
                    start: 3,
                    end: 4,
                    kind: RowKind::Item
                },
                Row {
                    start: 4,
                    end: 7,
                    kind: RowKind::Work
                },
                Row {
                    start: 7,
                    end: 8,
                    kind: RowKind::Item
                },
                Row {
                    start: 8,
                    end: 9,
                    kind: RowKind::Thinking
                },
            ]
        );
    }

    #[test]
    fn row_of_finds_the_row_holding_an_item() {
        let mut panel = SessionPanel::default();
        grouped_transcript(&mut panel);
        panel.sync_list();
        assert_eq!(panel.row_of(5), 3);
        assert_eq!(panel.row_of(0), 0);
        assert_eq!(panel.row_of(8), 5);
    }

    #[test]
    fn appending_items_keeps_the_list_in_step() {
        let mut panel = SessionPanel::default();
        grouped_transcript(&mut panel);
        panel.sync_list();
        assert_eq!(panel.list.item_count(), panel.rows.len());
        // More items, including extending the trailing work run in place.
        push(
            &mut panel,
            ItemKind::ToolCall {
                call_id: "c2".to_string(),
                name: "read".to_string(),
                arguments: Value::Null,
                outcome: None,
            },
        );
        push(
            &mut panel,
            ItemKind::Assistant {
                text: "more".to_string(),
                model: None,
            },
        );
        panel.sync_list();
        assert_eq!(panel.list.item_count(), panel.rows.len());
        // The optimistic prompt is the extra last entry.
        panel.pending_prompt = Some("queued".to_string());
        panel.sync_list();
        assert_eq!(panel.list.item_count(), panel.rows.len() + 1);
        panel.pending_prompt = None;
        panel.sync_list();
        assert_eq!(panel.list.item_count(), panel.rows.len());
        // A rewind resets the list to the new rows.
        panel.transcript.items.truncate(3);
        panel.rows_dirty = true;
        panel.sync_list();
        assert_eq!(panel.list.item_count(), panel.rows.len());
    }

    #[test]
    fn jumping_to_a_prompt_scrolls_to_its_row_and_stops_following() {
        let mut panel = SessionPanel::default();
        for index in 0..120 {
            push(
                &mut panel,
                ItemKind::User {
                    text: format!("prompt {index}"),
                    images: 0,
                },
            );
        }
        panel.sync_list();
        assert!(panel.list.is_following_tail());
        panel.jump_to(5);
        assert_eq!(panel.list.logical_scroll_top().item_ix, 5);
        assert!(!panel.list.is_following_tail());
        panel.follow_tail();
        assert!(panel.list.is_following_tail());
    }

    #[test]
    fn session_id_only_allows_the_safe_charset() {
        assert!(safe_session_id("c97a2b18-3c8d-4e2f-9a1b-0d2f4a6b8c10"));
        for bad in ["a b", "x;rm", "../x", "a/b", ""] {
            assert!(!safe_session_id(bad), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn steering_rides_until_the_turn_answers() {
        let mut panel = SessionPanel::default();
        let push = |panel: &mut SessionPanel, kind: ItemKind| {
            let id = panel.transcript.items.len().to_string();
            panel.transcript.items.push(Item {
                id,
                timestamp: String::new(),
                kind,
            });
        };
        panel.pending_steering = true;
        panel.pending_from = 0;
        // Nothing landed after the steer yet.
        assert!(!panel.steered_turn_answered());
        // pi echoing the steer back is not an answer.
        push(
            &mut panel,
            ItemKind::User {
                text: "steer".to_string(),
                images: 0,
            },
        );
        push(
            &mut panel,
            ItemKind::Thinking {
                text: "reconsidering".to_string(),
            },
        );
        assert!(!panel.steered_turn_answered());
        assert!(panel.is_working());
        // Prose mid-turn still leaves tool work queued behind it.
        push(
            &mut panel,
            ItemKind::Assistant {
                text: "checking".to_string(),
                model: None,
            },
        );
        push(
            &mut panel,
            ItemKind::ToolCall {
                call_id: "c1".to_string(),
                name: "read".to_string(),
                arguments: Value::Null,
                outcome: None,
            },
        );
        push(
            &mut panel,
            ItemKind::ToolResult {
                call_id: "c1".to_string(),
                name: "read".to_string(),
                text: String::new(),
                images: 0,
                is_error: false,
            },
        );
        assert!(!panel.steered_turn_answered());
        assert!(panel.is_working());
        // The reply the turn ends on settles it, notes and all.
        push(
            &mut panel,
            ItemKind::Assistant {
                text: "done".to_string(),
                model: None,
            },
        );
        push(
            &mut panel,
            ItemKind::Note {
                title: "compaction".to_string(),
                text: String::new(),
            },
        );
        assert!(panel.steered_turn_answered());
    }

    #[test]
    fn steering_needs_a_turn_that_is_actually_running() {
        let mut panel = SessionPanel::default();
        let work = |panel: &mut SessionPanel, kind: ItemKind| {
            let id = panel.transcript.items.len().to_string();
            panel.transcript.items.push(Item {
                id,
                timestamp: String::new(),
                kind,
            });
        };
        // pi mid-turn, writing as it goes: this send steers.
        panel.last_growth = Some(Instant::now());
        work(
            &mut panel,
            ItemKind::ToolCall {
                call_id: "c1".to_string(),
                name: "read".to_string(),
                arguments: Value::Null,
                outcome: None,
            },
        );
        panel.set_optimistic("mid-work");
        assert!(panel.pending_steering);

        // The reply landed and the turn is over; the next send starts a turn
        // of its own, however recently the file moved.
        panel.transcript.items.push(Item {
            id: "a0".to_string(),
            timestamp: String::new(),
            kind: ItemKind::Assistant {
                text: "done".to_string(),
                model: None,
            },
        });
        panel.pending_prompt = None;
        panel.awaiting_response = false;
        panel.last_growth = Some(Instant::now());
        panel.set_optimistic("fresh");
        assert!(!panel.pending_steering);
        assert!(panel.is_working(), "our own turn is still unanswered");

        // A work tail the file has not touched in a while is an interrupted
        // session, not a running turn.
        panel.pending_prompt = None;
        panel.awaiting_response = false;
        panel.last_growth = None;
        panel.set_optimistic("after a crash");
        assert!(!panel.pending_steering);
    }

    #[test]
    fn notes_settle_a_steer_with_no_reply() {
        let mut panel = SessionPanel::default();
        panel.pending_steering = true;
        panel.pending_from = 0;
        // Nothing landed after the steer yet: still on its way.
        assert!(!panel.steered_turn_answered());
        panel.transcript.items.push(Item {
            id: "u0".to_string(),
            timestamp: String::new(),
            kind: ItemKind::User {
                text: "steer".to_string(),
                images: 0,
            },
        });
        assert!(!panel.steered_turn_answered());
        // A compaction is what a turn leaves behind, not work it is doing.
        panel.transcript.items.push(Item {
            id: "n0".to_string(),
            timestamp: String::new(),
            kind: ItemKind::Note {
                title: "compaction".to_string(),
                text: String::new(),
            },
        });
        assert!(panel.steered_turn_answered());
    }

    #[test]
    fn counts_and_usage_stats_format_compactly() {
        use pilus_session::Usage;
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(4790), "4.8k");
        assert_eq!(fmt_count(12_000), "12k");
        assert_eq!(fmt_count(2_400_000), "2.4M");
        assert_eq!(fmt_cost(1.532), "$1.532");
        assert_eq!(fmt_cost(0.0168), "$0.0168");
        assert_eq!(fmt_cost(0.0), "$0");
        assert_eq!(usage_stats(&Usage::default()), None);
        let usage = Usage {
            input: 2000,
            output: 500,
            cache_read: 8000,
            cache_write: 0,
            total: 10_500,
            cost: 0.0168,
        };
        assert_eq!(
            usage_stats(&usage).as_deref(),
            Some("in 2k • out 500 • cache 80% ($0.0168)")
        );
    }

    #[test]
    fn model_chip_prefers_the_catalog_display_name() {
        use pilus_client::AgentModel;
        let mut panel = SessionPanel::default();
        panel.transcript.model = Some("opencode-go/muse-spark".to_string());
        // No catalog yet: the slug itself.
        assert_eq!(panel.model_display(), "opencode-go/muse-spark");
        panel.catalog.push(AgentModel {
            provider: "opencode-go".to_string(),
            id: "muse-spark".to_string(),
            name: "Muse Spark 1.3 Contributor".to_string(),
            ..Default::default()
        });
        assert_eq!(panel.model_display(), "Muse Spark 1.3 Contributor");
        // An unknown slug still shows itself.
        panel.transcript.model = Some("other/model".to_string());
        assert_eq!(panel.model_display(), "other/model");
    }

    #[test]
    fn capitalizes_note_titles() {
        assert_eq!(capitalize("compaction"), "Compaction");
        assert_eq!(capitalize("branch summary"), "Branch summary");
        assert_eq!(capitalize(""), "");
    }

    #[test]
    fn parses_iso_timestamps_and_formats_durations() {
        let start = parse_iso_millis("2024-12-03T14:00:04.000Z").expect("parse");
        let end = parse_iso_millis("2024-12-03T14:10:07.000Z").expect("parse");
        assert_eq!(end - start, 603_000);
        assert_eq!(format_duration(603_000), "10m 3s");
        assert_eq!(format_duration(45_000), "45s");
        assert_eq!(format_duration(3_723_000), "1h 2m");
        assert_eq!(parse_iso_millis("not a timestamp"), None);
    }

    #[test]
    fn work_label_uses_duration_or_falls_back() {
        let event = |timestamp: &str| Item {
            id: "x".to_string(),
            timestamp: timestamp.to_string(),
            kind: ItemKind::Thinking {
                text: "t".to_string(),
            },
        };
        let run = vec![
            event("2024-12-03T14:00:00.000Z"),
            event("2024-12-03T14:00:05.000Z"),
        ];
        assert_eq!(work_label(&run, false), "Working for 5s");
        // A trailing run is still in progress.
        assert_eq!(work_label(&run, true), "Working…");
        // A single item has no span to measure.
        assert_eq!(work_label(&run[..1], false), "Working…");
    }

    #[test]
    fn tool_summary_reads_known_tools() {
        assert_eq!(
            tool_summary("read", &serde_json::json!({"path": "src/auth.rs"})),
            "Read src/auth.rs"
        );
        assert_eq!(
            tool_summary("edit", &serde_json::json!({"path": "src/auth.rs"})),
            "Edited src/auth.rs"
        );
        // A multi-line command previews its first line.
        assert_eq!(
            tool_summary(
                "bash",
                &serde_json::json!({"command": "cargo test\ncargo build"})
            ),
            "Ran cargo test"
        );
        assert_eq!(
            tool_summary("grep", &serde_json::json!({"pattern": "foo"})),
            "Searched foo"
        );
        // A missing key argument falls back to the `name · detail` shape.
        assert_eq!(
            tool_summary("read", &serde_json::json!({"other": "x"})),
            "read · x"
        );
    }

    #[test]
    fn group_headers_summarize_what_the_calls_did() {
        let call = |name: &str| Item {
            id: "x".to_string(),
            timestamp: String::new(),
            kind: ItemKind::ToolCall {
                call_id: String::new(),
                name: name.to_string(),
                arguments: Value::Null,
                outcome: None,
            },
        };
        let calls = vec![
            call("read"),
            call("read"),
            call("read"),
            call("grep"),
            call("bash"),
        ];
        assert_eq!(
            summarize_calls(&calls),
            "Read 3 files, ran 1 search, ran 1 command"
        );
        assert_eq!(summarize_calls(&[call("edit")]), "Edited 1 file");
    }

    #[test]
    fn tool_details_include_args_and_outcome() {
        let outcome = ToolOutcome {
            text: "contents".to_string(),
            images: 0,
            is_error: false,
        };
        let details = tool_details(
            "read",
            &serde_json::json!({"path": "src/auth.rs", "offset": 0, "limit": 120}),
            Some(&outcome),
        );
        assert!(details.contains("src/auth.rs"));
        assert!(details.contains("limit: 120"));
        assert!(details.contains("offset: 0"));
        assert!(details.contains("contents"));

        let details = tool_details(
            "bash",
            &serde_json::json!({"command": "ls", "timeout": 30}),
            None,
        );
        assert!(details.contains("$ ls"));
        assert!(details.contains("timeout: 30s"));
    }

    fn user(index: usize, text: &str) -> Item {
        Item {
            id: index.to_string(),
            timestamp: String::new(),
            kind: ItemKind::User {
                text: text.to_string(),
                images: 0,
            },
        }
    }

    #[test]
    fn deltas_apply_like_a_full_resend() {
        let mut panel = SessionPanel::default();
        let mut items = vec![user(0, "m0"), user(1, "m1"), user(2, "m2")];

        // The first delivery is the whole snapshot.
        panel.apply_delta(0, items.clone(), TranscriptMeta::default());
        assert_eq!(panel.transcript.items, items);

        // An append only carries the new tail.
        items.push(user(3, "m3"));
        panel.apply_delta(3, items[3..].to_vec(), TranscriptMeta::default());
        assert_eq!(panel.transcript.items, items);

        // An in-place rewrite resends from the item it touched.
        items[1] = user(1, "edited");
        panel.apply_delta(1, items[1..].to_vec(), TranscriptMeta::default());
        assert_eq!(panel.transcript.items, items);

        // A rewind resends from 0.
        let fresh = vec![user(0, "only")];
        panel.apply_delta(0, fresh.clone(), TranscriptMeta::default());
        assert_eq!(panel.transcript.items, fresh);
    }

    #[test]
    fn an_inplace_change_splices_from_its_row() {
        let mut panel = SessionPanel::default();
        grouped_transcript(&mut panel);
        panel.sync_list();
        assert_eq!(panel.list.item_count(), 6);

        // A delta that rewrote item 5 (a call inside the Work row) does not
        // move the row diff, but the splice still reaches back to row 3.
        panel.changed_from = Some(5);
        let new = build_rows(&panel.transcript.items);
        assert_eq!(panel.splice_start(&new), 3);
    }

    /// A source that never produces bytes, standing in for a session file.
    struct Empty;

    impl Source for Empty {
        fn byte_len(&mut self) -> pilus_session::Result<u64> {
            Ok(0)
        }
        fn read_from(&mut self, _offset: u64) -> pilus_session::Result<Vec<u8>> {
            Ok(Vec::new())
        }
        fn label(&self) -> String {
            "test".to_string()
        }
    }

    #[test]
    fn the_session_cache_evicts_its_oldest_tail() {
        let cache = SessionCache::default();
        for index in 0..CACHED_TAILS + 2 {
            cache.put(
                &format!("session-{index}"),
                Tail::with_source(Box::new(Empty)).expect("tail"),
            );
        }
        // The first two inserts were pushed out.
        assert!(cache.take("session-0").is_none());
        assert!(cache.take("session-1").is_none());
        for index in 2..CACHED_TAILS + 2 {
            assert!(cache.take(&format!("session-{index}")).is_some());
        }
        assert!(cache.take("session-2").is_none());
    }

    #[test]
    fn submits_route_between_the_socket_and_the_terminal() {
        assert_eq!(route("/compact", true, false, false), Route::Terminal);
        assert_eq!(route("hi", true, false, false), Route::Rpc(None));
        assert_eq!(
            route("hi", true, true, false),
            Route::Rpc(Some(DeliverAs::Steer))
        );
        assert_eq!(
            route("hi", true, true, true),
            Route::Rpc(Some(DeliverAs::FollowUp))
        );
        assert_eq!(route("hi", false, false, false), Route::Terminal);
    }

    #[test]
    fn working_follows_the_socket_state_when_it_is_up() {
        let mut panel = SessionPanel {
            pi_state: Some(PiState {
                busy: true,
                ..PiState::default()
            }),
            ..SessionPanel::default()
        };
        assert!(panel.is_working());
        panel.pi_state = Some(PiState::default());
        assert!(!panel.is_working());

        // Down: the optimistic and transcript heuristics decide.
        panel.pi_state = None;
        assert!(!panel.is_working());
        panel.pending_prompt = Some("queued".to_string());
        assert!(panel.is_working());
    }

    #[test]
    fn the_context_chip_formats_occupancy() {
        let usage = ContextUsage {
            tokens: Some(86_000),
            context_window: 1_000_000,
            percent: Some(0.421),
        };
        assert_eq!(ctx_label(&usage), "ctx 42% · 86k/1M");
        let uncounted = ContextUsage {
            tokens: None,
            context_window: 1_048_576,
            percent: None,
        };
        assert_eq!(ctx_label(&uncounted), "ctx –/1M");
        assert!(ctx_warns(&ContextUsage {
            tokens: None,
            context_window: 1,
            percent: Some(0.82),
        }));
        assert!(!ctx_warns(&usage));
    }

    #[test]
    fn the_live_turn_rides_the_list_until_the_file_catches_up() {
        let mut panel = SessionPanel::default();
        grouped_transcript(&mut panel);
        panel.sync_list();
        assert_eq!(panel.list.item_count(), 6);

        // What an RpcStream update does: the overlay becomes a list entry.
        panel.live = Some(LiveTurn {
            items: vec![user(9, "live")],
            ..LiveTurn::default()
        });
        panel.sync_list();
        assert_eq!(panel.list.item_count(), 7);

        // The stream ended but the file has not landed it yet.
        panel.live.as_mut().expect("live").ended_at = Some(Instant::now());
        panel.sync_list();
        assert_eq!(panel.list.item_count(), 7);

        // The delta carrying the finished turn retires the overlay.
        panel.apply_delta(9, vec![user(9, "landed")], TranscriptMeta::default());
        assert!(panel.live.is_none());
        panel.sync_list();
        assert_eq!(panel.list.item_count(), panel.rows.len());
    }

    #[test]
    fn a_running_tool_outlives_the_linger() {
        let ended = Instant::now() - Duration::from_secs(10);
        let mut live = LiveTurn {
            ended_at: Some(ended),
            ..LiveTurn::default()
        };
        assert!(live_expired(&live, Instant::now()));
        live.tool = Some(("bash".to_string(), "out".to_string()));
        assert!(!live_expired(&live, Instant::now()));
    }

    #[test]
    fn the_rpc_session_speaks_the_extension_protocol() {
        use std::os::unix::net::UnixListener;

        let dir = std::env::temp_dir().join(format!("pilus-rpc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("test.sock");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind");

        // A fake extension: hello + a busy state, then it answers the
        // client's first write (the models request) and hangs up.
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut writer = stream.try_clone().expect("clone");
            writer
                .write_all(
                    b"{\"type\":\"hello\",\"protocol\":1,\"sessionId\":\"s\",\"sessionFile\":\"/tmp/s.jsonl\",\"pid\":1}\n",
                )
                .expect("hello");
            writer
                .write_all(b"{\"type\":\"state\",\"busy\":true}\n")
                .expect("state");
            let mut lines = BufReader::new(stream).lines();
            let request = lines.next().expect("line").expect("read");
            let value: Value = serde_json::from_str(&request).expect("json");
            assert_eq!(value["cmd"], "models");
            writer
                .write_all(
                    b"{\"type\":\"response\",\"id\":\"1\",\"ok\":true,\"data\":{\"models\":[{\"provider\":\"p\",\"id\":\"m\",\"name\":\"M\",\"contextWindow\":1,\"reasoning\":false}]}}\n",
                )
                .expect("reply");
        });

        let (update_tx, update_rx) = std::sync::mpsc::channel::<Update>();
        let (_command_tx, command_rx) =
            std::sync::mpsc::channel::<RpcCommand>();
        let commands = Mutex::new(command_rx);
        let stop = Arc::new(AtomicBool::new(false));
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let conn = connect_local(&path).expect("connect");
                rpc_session(conn, &update_tx, &commands, &stop);
            });
        });
        server.join().expect("server");

        let mut updates = Vec::new();
        while let Ok(update) = update_rx.try_recv() {
            updates.push(update);
        }
        assert_eq!(updates.len(), 4);
        assert!(matches!(updates[0], Update::RpcUp));
        assert!(matches!(updates[1], Update::RpcState(_)));
        match &updates[2] {
            Update::RpcResult {
                kind: RequestKind::Models,
                result: Ok(Some(_)),
            } => {}
            _ => panic!("expected a models result"),
        }
        assert!(matches!(updates[3], Update::RpcDown));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_reopen_retires_the_previous_sessions_senders() {
        let mut panel = SessionPanel::default();
        let stale_update = panel.tx.clone();
        let stale_send = panel.send_tx.clone();
        let stale_stop = panel.stop_tx.clone();

        // What `open_with` does before spawning the next session's threads.
        panel.rotate_channels();

        let delta = || {
            Update::Delta(Box::new(Delta {
                from: 0,
                items: vec![user(0, "stale")],
                meta: TranscriptMeta::default(),
            }))
        };
        // The old threads' sends die with their receivers: no crossover.
        assert!(stale_update.send(delta()).is_err());
        assert!(stale_send.send(Ok(())).is_err());
        assert!(stale_stop.send(Err("stale".to_string())).is_err());

        // The new session's own channel still lands.
        panel.tx.send(delta()).expect("fresh channel");
        assert!(matches!(panel.rx.try_recv(), Ok(Update::Delta(_))));
        assert!(panel.rx.try_recv().is_err());
    }
}
