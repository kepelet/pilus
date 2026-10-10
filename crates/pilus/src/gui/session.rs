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

use std::cell::{Cell, RefCell};
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
    Anchor, AnyElement, App, Context, Div, ElementId, Entity, FollowMode, Keystroke, ListAlignment,
    ListOffset, ListState, ScrollHandle, ScrollStrategy, Stateful, UniformListScrollHandle,
    anchored, canvas, deferred, div, list, point, prelude::*, px, relative, uniform_list,
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
use super::style::{self, FillExt, Fonts, RadiusExt};
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
/// Below this pane width the composer drops its side gutter, takes the full
/// width, and the jump buttons float over its top edge instead.
const COMPACT_BELOW: f32 = 560.0;
/// How long a fresh panel's model and thinking chips wait on the extension
/// socket's first report before settling for the file's or the catalog's
/// answer. The socket usually reports well inside this; a pi without the
/// extension just shows the fallback a beat later.
const CHIP_SETTLE: Duration = Duration::from_millis(1500);
/// Width of the model chip's skeleton: about a short model name.
const MODEL_SKELETON_W: f32 = 110.0;
/// Width of the thinking chip's skeleton: about one effort label.
const THINKING_SKELETON_W: f32 = 56.0;
/// The jump buttons' diameter, which their skeleton matches.
const JUMP_BUTTON: f32 = 30.0;
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
#[derive(Clone, Copy, PartialEq, Eq)]
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
pub(super) enum PickerKind {
    Model,
    Thinking,
}

/// Thinking levels pi accepts for `/thinking`, from its settings reference.
const THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// How long after a section opens its body still fades in. Longer than the
/// fade itself, so a frame painted a little late still catches it.
const DISCLOSE_WINDOW: Duration = Duration::from_millis(400);

/// Skeleton rows a picker paints while its catalog loads. Enough to fill
/// the menu the real rows will occupy, so the panel does not jump when they
/// land.
const PICKER_SKELETON_ROWS: usize = 5;

/// One composer picker row: what it shows, what choosing it sends, and
/// whether the live session is already on it.
struct PickerOption {
    /// The row's label: the catalog's display name, the model's slug when
    /// the catalog has no name for it, or the thinking level.
    label: String,
    /// What choosing the row sends: a `provider/id` slug, or a level.
    value: String,
    /// The provider a model belongs to, dimmed beside its label. Models
    /// repeat their display names across providers, so the slug is what
    /// tells two rows of a long catalog apart.
    hint: Option<String>,
    /// Whether the live agent is on this value.
    current: bool,
}

/// Every row a composer picker offers.
///
/// The catalog comes first, in its own order (pi's default provider and
/// model on top), then any model this session used that the catalog does
/// not know — a spelling the agent accepted once is still a valid choice.
fn picker_options(
    catalog: &[AgentModel],
    session_models: &[String],
    current: &str,
) -> Vec<PickerOption> {
    let mut options: Vec<PickerOption> = Vec::new();
    for model in catalog {
        let value = model_slug(model);
        if value.is_empty() || options.iter().any(|known| known.value == value) {
            continue;
        }
        let current = value == current;
        options.push(PickerOption {
            label: if model.name.is_empty() {
                value.clone()
            } else {
                model.name.clone()
            },
            value,
            hint: (!model.provider.is_empty()).then(|| model.provider.clone()),
            current,
        });
    }
    for value in session_models {
        if options.iter().any(|known| known.value == *value) {
            continue;
        }
        options.push(PickerOption {
            label: value.clone(),
            value: value.clone(),
            hint: None,
            current: *value == current,
        });
    }
    options
}

/// A model's `provider/id` slug, the spelling `/model` wants.
fn model_slug(model: &AgentModel) -> String {
    if model.provider.is_empty() {
        model.id.clone()
    } else {
        format!("{}/{}", model.provider, model.id)
    }
}

/// The rows a picker's filter keeps, as indices into `options`.
///
/// The query matches the label or the slug, so both `gpt` and
/// `openrouter/gpt` find the row.
fn picker_matches(options: &[PickerOption], filter: &str) -> Vec<usize> {
    let needle = filter.trim().to_lowercase();
    options
        .iter()
        .enumerate()
        .filter(|(_, option)| {
            needle.is_empty()
                || option.label.to_lowercase().contains(&needle)
                || option.value.to_lowercase().contains(&needle)
        })
        .map(|(index, _)| index)
        .collect()
}

/// Where a picker's highlight lands after moving `delta` rows, wrapping.
/// The cursor is an index into the matches, so a filter that drops the
/// highlighted row re-homes it on the first one left.
fn picker_cursor_after(cursor: usize, count: usize, delta: isize) -> usize {
    if count == 0 {
        return 0;
    }
    (cursor as isize + delta).rem_euclid(count as isize) as usize
}

/// How long after the last file growth the session still counts as active.
/// Past this, a trailing work item is a stale tail (interrupted session),
/// not live work. Generous: quiet stretches (a long generation writes
/// nothing until it completes) must not flip a live turn to idle.
const ACTIVE_WINDOW: Duration = Duration::from_secs(90);
/// Rows the prompts menu shows before it scrolls. It stacks its rows with
/// a gap, so fewer of them fit than the picker’s packed list holds.
const PROMPTS_MAX_ROWS: usize = 7;

/// Sessions the history menu lists. Newest first; older ones stay on disk
/// but are beyond the picker's useful reach.
const HISTORY_LIMIT: usize = 40;
/// Bytes of each remote session's first user line shipped back to title
/// it. Pi writes a system prompt tens of kilobytes long ahead of that
/// line, so the listing greps for the line itself rather than sampling the
/// file's head; the cap only bounds a long pasted prompt.
const HISTORY_SAMPLE_BYTES: usize = 4096;

/// One row in the session history menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct HistoryEntry {
    /// The session file, opened when the row is chosen.
    pub(super) path: PathBuf,
    /// The session id the file name carries.
    pub(super) id: String,
    /// The session's first prompt, empty when it has none yet.
    pub(super) title: String,
}

impl HistoryEntry {
    /// The row's label: the opener when there is one, else when the
    /// session started — an id is no name a person can read.
    fn label(&self) -> String {
        if !self.title.is_empty() {
            return self.title.clone();
        }
        match session_started(&self.path) {
            Some(started) => format!("Untitled · {started}"),
            None => "Untitled session".to_string(),
        }
    }
}

/// Characters the header keeps of the session's first prompt; the strip
/// ellipsizes whatever overflows the window width.
const TITLE_CHARS: usize = 80;

/// How many option rows the composer picker shows: the menu's shared
/// height budget, less its padding and, when the list is long enough to
/// need one, its filter field.
fn picker_visible_rows(filtered: bool) -> usize {
    let chrome = super::picker_menu_h(1, filtered) - super::PICKER_ROW_H;
    let budget = (super::PICKER_MENU_MAX_H - chrome).max(super::PICKER_ROW_H);
    (budget / super::PICKER_ROW_H) as usize
}

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
    /// The session generation the fold belongs to: the reader lives
    /// across a `/new` switch, so a delta produced before the switch must
    /// not land in the new transcript. Bumped on every open and follow.
    epoch: u64,
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
    New,
    SetModel,
    SetThinking,
    Models,
    State,
}

/// What the reader thread sends back to the window.
enum Update {
    Delta(Box<Delta>),
    /// The reader resolved the session and found no file yet: a fresh pane
    /// whose first prompt pi has not written. The header uses it to stop
    /// holding a skeleton for a session that simply has no title.
    Empty,
    /// The pi model catalog for the endpoint, for the model picker.
    Catalog(Vec<AgentModel>),
    /// The session history for the endpoint, newest first.
    History(Vec<HistoryEntry>),
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
    /// The extension socket says this pane moved to another session — the
    /// `/new`, `/resume`, or `/fork` that swapped the socket. The panel
    /// retargets its reader without waiting on the daemon's re-report.
    SessionFollow {
        session_id: String,
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

/// Occupancy as a percentage (0–100): pi's own figure, or computed from the
/// token count when pi has not supplied one. `None` when the window is
/// unknown, so a zero never divides into a full red gauge.
fn ctx_percent(usage: &ContextUsage) -> Option<f64> {
    if let Some(percent) = usage.percent {
        return Some(percent);
    }
    let tokens = usage.tokens?;
    (usage.context_window > 0).then(|| tokens as f64 / usage.context_window as f64 * 100.0)
}

/// Whether the context readout is worth painting: only an unknown window
/// hides it. A known window always shows, even at zero, so the gauge is a
/// stable fixture of the composer rather than something that disappears on
/// a fresh or just-compacted session.
fn ctx_visible(usage: &ContextUsage) -> bool {
    usage.context_window > 0
}

/// Whether the context gauge warns: at or above 80% full.
fn ctx_warns(usage: &ContextUsage) -> bool {
    ctx_percent(usage).is_some_and(|percent| percent >= 80.0)
}

/// The gauge's companion text: `421k/1M`, or `–/1M` while pi has not
/// re-counted after a compaction.
fn ctx_counts(usage: &ContextUsage) -> String {
    match usage.tokens {
        Some(tokens) => format!("{}/{}", fmt_count(tokens), fmt_count(usage.context_window)),
        None => format!("–/{}", fmt_count(usage.context_window)),
    }
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

/// Where a transcript glide is headed.
#[derive(Clone, Copy)]
enum ScrollGoal {
    /// The tail, re-pinned when the glide lands.
    Bottom,
    /// A transcript item's row, with the pixel offset it resolved to.
    Item { index: usize, to: f32 },
}

/// A transcript glide: the list eases from where it was to its goal, then
/// lands exactly with the list's own jump so estimated row heights cannot
/// leave it short.
struct ScrollTween {
    from: f32,
    goal: ScrollGoal,
    started: Instant,
    duration: Duration,
}

/// Shortest and longest a transcript glide runs; distance picks between.
const SCROLL_GLIDE_MIN: Duration = Duration::from_millis(260);
const SCROLL_GLIDE_MAX: Duration = Duration::from_millis(520);

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
    /// Hidden but still live: its pane left the screen, and the reader,
    /// socket, and transcript keep running so coming back shows the
    /// conversation as it is now instead of reloading it.
    suspended: bool,
    /// When this panel was last opened on a session: the model and
    /// thinking chips hold skeletons for the socket's first report rather
    /// than flashing a fallback that the report then replaces.
    opened_at: Option<Instant>,
    /// The panel stands in for a pi pane's terminal: Escape does not close
    /// it, so there is no way to strand the pane behind the card.
    pinned: bool,
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
    /// Whether the catalog read is still in flight. The pickers paint
    /// skeleton rows until it lands, so an empty menu never reads as "no
    /// models".
    catalog_loading: bool,
    /// Item indices the user has expanded from their collapsed default.
    expanded: HashSet<usize>,
    /// The section the user last toggled, and when: its chevron turns and an
    /// opened body fades in, once.
    disclosed: Option<(Toggle, Instant)>,
    /// Group indices (a run of tool calls, keyed by its first item) expanded.
    groups: HashSet<usize>,
    /// Work groups (a stretch of thinking and tool calls between assistant
    /// replies, keyed by its first item) expanded.
    work_open: HashSet<usize>,
    /// Active work runs (keyed like `work_open`) the user collapsed. A run
    /// that is still being worked on opens by default, so only the closing
    /// needs remembering; once the turn ends the run falls back to
    /// `work_open` and so to collapsed.
    work_shut: HashSet<usize>,
    /// Active thinking items the user collapsed, for the same reason.
    thinking_shut: HashSet<usize>,
    /// The first item index of the latest batch appended to the transcript
    /// and when it landed: items from there on fade in while it is fresh.
    arrived: Option<(usize, Instant)>,
    /// Which composer picker is open, if any.
    picker: Option<PickerKind>,
    /// The picker an outside press just dismissed, so the following click on
    /// its own chip stays closed instead of reopening it.
    picker_dismissed: Option<PickerKind>,
    /// The open picker's type-to-filter field. It is the focused element
    /// while the picker is open, so a query never lands in the prompt
    /// draft the user is about to send.
    picker_field: Option<Entity<TextInput>>,
    /// The open picker's highlighted row, as an index into its matches.
    picker_cursor: usize,
    /// The query the highlight was last reconciled against, so a frame
    /// that only repaints does not rebuild the option list.
    picker_query: String,
    /// Scroll position for the picker's virtualized option list, which is
    /// how a catalog of hundreds of models stays browsable.
    picker_scroll: UniformListScrollHandle,
    /// The composer row's width as last painted, which decides between the
    /// side gutter and the full-width layout.
    dock_width: Rc<Cell<f32>>,
    /// Scroll position of the model/effort/context row.
    meta_scroll: ScrollHandle,
    /// The glide a jump button started, until it lands.
    scroll_tween: Option<ScrollTween>,
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
    /// Whether the session history menu is open, and whether an outside
    /// press just dismissed it.
    history_open: bool,
    history_dismissed: bool,
    /// The endpoint's earlier sessions, newest first. Read once per panel
    /// open on a background thread; empty while the read is in flight.
    history: Vec<HistoryEntry>,
    /// Whether the history read is still in flight, so the menu shows
    /// skeleton rows instead of "no earlier sessions".
    history_loading: bool,
    /// A sent prompt is still waiting for pi's first responding content.
    /// The optimistic indicator persists through this, not just the send.
    awaiting_response: bool,
    /// The optimistic indicator reads `Steering…`: the prompt went out while
    /// pi was already working.
    pending_steering: bool,
    /// Transcript length an abort settled: older trailing work is an
    /// interrupted tail, not a running turn, so the socket-down fallback
    /// and the steering guess ignore it. Items past it count again.
    settled_through: usize,
    /// When the user last interrupted the turn. The interrupted turn keeps
    /// writing its remnant, so `is_working` suppresses the socket-down
    /// fallback until a new user turn starts, and the dock shows a
    /// `Stopped` receipt where `Working` was.
    stopped_at: Option<Instant>,
    /// Transcript length when the abort landed: a user item at or past it
    /// is the new turn that ends the abort's receipt. Kept apart from
    /// `settled_through`, which deltas may lower on a rewind.
    abort_at: usize,
    /// The model slug the user just picked, shown at once: the socket's
    /// report and the file's echo confirm it, but a fresh session may have
    /// neither yet (reconnecting socket, unwritten file) while pi already
    /// runs on the pick.
    picked_model: Option<String>,
    /// The thinking level the user just picked, same optimism as above.
    picked_thinking: Option<String>,
    /// Transcript length when the pick went out: file truth adopted after
    /// that point confirms (or corrects) the optimistic pick.
    picked_from: usize,
    /// When the session file last grew. Delivery implies growth (the reader
    /// only forwards new bytes), so recency here means live work — unlike a
    /// trailing work item, which may be a stale interrupted tail.
    last_growth: Option<Instant>,
    /// Item count at the last delivery, so growth marks the session active.
    last_seen: usize,
    /// Whether the reader has delivered its first word for this session — a
    /// fold, an error, or a confirmed empty file. Until then a remote header
    /// holds a skeleton; after it, an absent first prompt is simply a fresh
    /// session, not a load in flight.
    loaded: bool,
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
    /// Commands for the extension's socket; `Some` whenever a pane is
    /// known, even while the channel itself is reconnecting or no session
    /// id has been reported yet — the socket can be discovered by pane.
    rpc_tx: Option<Sender<RpcCommand>>,
    /// Whether the socket is live right now; state pushes only count then.
    rpc_up: bool,
    /// The session the panel is on: the id the daemon seeded it with, or
    /// the newer one the extension socket followed. `None` while the pane
    /// has not reported and no socket has been discovered.
    reported: Option<String>,
    /// The session generation: bumped on every open and follow so a delta
    /// produced before a `/new` switch cannot land in the new transcript.
    epoch: u64,
    /// Session follows pushed to the reader thread, tagged with the epoch
    /// the panel applied them under.
    switch_tx: Option<Sender<(u64, String)>>,
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
            suspended: false,
            opened_at: None,
            pinned: false,
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
            catalog_loading: false,
            disclosed: None,
            picker: None,
            picker_dismissed: None,
            picker_field: None,
            picker_cursor: 0,
            picker_query: String::new(),
            picker_scroll: UniformListScrollHandle::default(),
            dock_width: Rc::new(Cell::new(0.0)),
            meta_scroll: ScrollHandle::new(),
            scroll_tween: None,
            send_clears_input: true,
            pending_prompt: None,
            pending_from: 0,
            prompts_open: false,
            prompts_dismissed: false,
            history_open: false,
            history_dismissed: false,
            history: Vec::new(),
            history_loading: false,
            awaiting_response: false,
            pending_steering: false,
            settled_through: 0,
            stopped_at: None,
            abort_at: 0,
            picked_model: None,
            picked_thinking: None,
            picked_from: 0,
            last_growth: None,
            expanded: HashSet::new(),
            groups: HashSet::new(),
            work_open: HashSet::new(),
            work_shut: HashSet::new(),
            thinking_shut: HashSet::new(),
            arrived: None,
            last_seen: 0,
            loaded: false,
            changed_from: None,
            cache: SessionCache::default(),
            expand_cache: HashMap::new(),
            expand_key: None,
            rpc_tx: None,
            rpc_up: false,
            reported: None,
            epoch: 0,
            switch_tx: None,
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

    /// Hide the panel without stopping it: its pane left the screen, but
    /// the user is likely to come back. Floating menus close so the return
    /// lands on the conversation, not a stale picker.
    pub fn suspend(&mut self) {
        if !self.open {
            return;
        }
        self.open = false;
        self.suspended = true;
        self.picker = None;
        self.picker_dismissed = None;
        self.prompts_open = false;
        self.prompts_dismissed = false;
        self.history_open = false;
        self.history_dismissed = false;
    }

    /// Show a suspended panel again exactly as it stands.
    pub fn resume(&mut self) {
        if self.suspended {
            self.suspended = false;
            self.open = true;
        }
    }

    /// An empty panel that shares this one's wiring (the picker's filter
    /// field), to stand in while this one is parked.
    pub(super) fn sibling(&self) -> Self {
        Self {
            picker_field: self.picker_field.clone(),
            theme: self.theme,
            settings: self.settings.clone(),
            ..Self::default()
        }
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
        target: Option<String>,
        cx: &mut Context<PilusView>,
    ) {
        let path = PathBuf::from(&spec);
        // Mirror pi's `--session <path|id>`: an explicit file is used as-is,
        // a directory or a bare cwd resolves to its newest session, and a
        // separator-free spec that names no directory is tried as a full or
        // partial session id before the cwd fallback.
        let (explicit, cwd, resolve_latest) = if path.is_file() {
            (Some(path), String::new(), false)
        } else if path.is_dir() {
            (None, local_cwd(&spec), true)
        } else if let Some(found) = pilus_session::resolve(&spec) {
            (Some(found), String::new(), false)
        } else {
            // Nothing resolved: keep the spec as the cwd so the reader's
            // "no session" message can name what was asked for.
            (None, local_cwd(&spec), true)
        };
        self.open_with(
            Endpoint::Local { api_socket, cwd },
            ReadSpec {
                explicit,
                session_id: None,
                resolve_latest,
                display: spec,
            },
            target,
            cx,
        );
    }

    /// Open the panel for a local pane. `session_id` is the daemon's
    /// latest report — the extension socket may already know a newer one
    /// and will retarget the reader itself; `None` opens an empty panel
    /// that fills once either produces an id.
    pub fn open_local_session(
        &mut self,
        api_socket: PathBuf,
        cwd: String,
        session_id: Option<String>,
        pane: String,
        cx: &mut Context<PilusView>,
    ) {
        self.open_with(
            Endpoint::Local { api_socket, cwd },
            ReadSpec {
                explicit: None,
                session_id,
                resolve_latest: false,
                display: String::new(),
            },
            Some(pane),
            cx,
        );
    }

    /// Open the panel for a remote pane; `session_id` has the same
    /// seed-or-wait meaning as [`Self::open_local_session`].
    pub fn open_remote_session(
        &mut self,
        target: SshTarget,
        cwd: String,
        session_id: Option<String>,
        pane: String,
        cx: &mut Context<PilusView>,
    ) {
        self.open_with(
            Endpoint::Remote { target, cwd },
            ReadSpec {
                explicit: None,
                session_id,
                resolve_latest: false,
                display: String::new(),
            },
            Some(pane),
            cx,
        );
    }

    /// Park the outgoing session's expansion under its key so a switch
    /// back lands on the same open rows. `mem::take` clears the sets; the
    /// caller's generic clears then cover a first open, where nothing was
    /// parked.
    fn park_expansion(&mut self) {
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
    }

    /// Start a reader thread for `endpoint` and reset the panel's state.
    fn open_with(
        &mut self,
        endpoint: Endpoint,
        spec: ReadSpec,
        target: Option<String>,
        cx: &mut Context<PilusView>,
    ) {
        self.park_expansion();
        self.close();
        self.rotate_channels();
        self.open = true;
        self.opened_at = Some(Instant::now());
        self.error.clear();
        self.status.clear();
        self.sending = false;
        self.transcript = Transcript::default();
        self.expanded.clear();
        self.groups.clear();
        self.work_open.clear();
        self.work_shut.clear();
        self.thinking_shut.clear();
        // A session id identifies its expansion; a standalone file its path,
        // and a bare cwd spec (`--session-ui`) the cwd itself.
        let expand_key = spec
            .session_id
            .clone()
            .or_else(|| {
                spec.explicit
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
        self.catalog_loading = true;
        self.picker = None;
        self.picker_dismissed = None;
        self.picker_cursor = 0;
        self.picker_query.clear();
        self.pending_prompt = None;
        self.pending_from = 0;
        self.prompts_open = false;
        self.prompts_dismissed = false;
        self.history_open = false;
        self.history_dismissed = false;
        self.history.clear();
        self.history_loading = false;
        self.outbox.clear();
        self.awaiting_response = false;
        self.pending_steering = false;
        self.settled_through = 0;
        self.stopped_at = None;
        self.abort_at = 0;
        self.picked_model = None;
        self.picked_thinking = None;
        self.picked_from = 0;
        self.last_growth = None;
        self.last_seen = 0;
        self.loaded = false;
        self.target = target;
        self.endpoint = Some(endpoint.clone());
        self.reported = spec.session_id.clone();
        self.epoch += 1;
        self.stop = Arc::new(AtomicBool::new(false));
        let stop = self.stop.clone();
        let tx = self.tx.clone();
        let cache = self.cache.clone();
        let catalog_endpoint = endpoint.clone();
        let (switch_tx, switch_rx) = std::sync::mpsc::channel();
        self.switch_tx = Some(switch_tx);
        let tag = self.epoch;
        std::thread::spawn(move || reader_loop(endpoint, spec, tag, cache, tx, switch_rx, stop));
        // A pane always gets the RPC channel: with no reported id the loop
        // discovers the socket by its sidecar's `pane` key, and either way
        // it follows the pane across `/new` switches on its own.
        // `--session-ui` has no pane and stays file-only.
        if let Some(pane) = self.target.clone() {
            let (command_tx, command_rx) = std::sync::mpsc::channel();
            self.rpc_tx = Some(command_tx);
            let rpc_endpoint = catalog_endpoint.clone();
            let rpc_tx = self.tx.clone();
            let rpc_stop = self.stop.clone();
            let seed = self.reported.clone();
            std::thread::spawn(move || {
                rpc_loop(rpc_endpoint, pane, seed, rpc_tx, command_rx, rpc_stop);
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
            let models = configs
                .iter()
                .find(|config| config.kind == "pi")
                .map(|pi| pi.models.clone())
                .unwrap_or_default();
            let _ = catalog_tx.send(Update::Catalog(models));
        });
        cx.notify();
    }

    /// Pin or unpin the panel. Pinned, it replaces a pi pane's terminal:
    /// Escape is swallowed instead of closing it.
    pub fn set_pinned(&mut self, pinned: bool) {
        self.pinned = pinned;
    }

    /// The session the panel is on, while it is open: the daemon's report,
    /// or the newer one the extension socket followed ahead of it. A stale
    /// id can linger behind a `/new`; callers compare it against the
    /// daemon's word and trust this one while [`Self::rpc_live`] holds.
    pub fn live_session(&self) -> Option<String> {
        self.open.then(|| self.reported.clone()).flatten()
    }

    /// Whether the extension socket is connected right now; while it is,
    /// [`Self::live_session`] is the pane's true session even when the
    /// daemon has not caught up.
    pub fn rpc_live(&self) -> bool {
        self.open && self.rpc_up
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
        self.suspended = false;
        self.pinned = false;
        self.last_seen = 0;
        self.loaded = false;
        self.catalog_loading = false;
        self.history_open = false;
        self.history_dismissed = false;
        self.history_loading = false;
    }

    /// Take the session the extension socket says this pane moved to — a
    /// `/new`, `/resume`, or `/fork` inside pi. The socket is the authority:
    /// it is the same extension that reports to the daemon, just faster, so
    /// a follow retargets the reader without a panel reopen (which would
    /// blank the transcript mid-switch).
    fn follow_session(&mut self, session_id: String) {
        if !safe_session_id(&session_id) || self.reported.as_deref() == Some(&session_id) {
            return;
        }
        // The outgoing session's expansion parks under its own key; the new
        // id then claims the slot and restores whatever it had saved.
        self.park_expansion();
        self.reported = Some(session_id.clone());
        self.expand_key = Some(session_id.clone());
        if let Some(state) = self.expand_cache.get(&session_id).cloned() {
            self.expanded = state.expanded;
            self.groups = state.groups;
            self.work_open = state.work_open;
        } else {
            self.expanded.clear();
            self.groups.clear();
            self.work_open.clear();
        }
        self.work_shut.clear();
        self.thinking_shut.clear();
        self.transcript = Transcript::default();
        self.list = new_list_state();
        self.rows.clear();
        self.list_pending = false;
        self.list_live = false;
        self.rows_dirty = true;
        self.changed_from = None;
        self.live = None;
        // The outgoing session's model and thinking belong to it: a fresh
        // `state` push names the new session's own pair, and until it
        // lands the picker falls back to the catalog default pi will run.
        self.pi_state = None;
        self.pending_prompt = None;
        self.pending_from = 0;
        self.awaiting_response = false;
        self.pending_steering = false;
        self.settled_through = 0;
        self.stopped_at = None;
        self.abort_at = 0;
        self.picked_model = None;
        self.picked_thinking = None;
        self.picked_from = 0;
        self.outbox.clear();
        self.picker = None;
        self.prompts_open = false;
        self.last_growth = None;
        self.last_seen = 0;
        self.loaded = false;
        self.error.clear();
        self.status.clear();
        self.epoch += 1;
        if let Some(tx) = &self.switch_tx {
            let _ = tx.send((self.epoch, session_id));
        }
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
                    // A delta produced before a session follow belongs to
                    // the outgoing transcript; the new fold lands under
                    // the new epoch.
                    if delta.epoch == self.epoch {
                        self.apply_delta(delta.from, delta.items, delta.meta);
                    }
                }
                // The reader confirmed the session has no file yet, so there
                // is nothing to wait for: the header drops its skeleton.
                Update::Empty => self.loaded = true,
                Update::Catalog(catalog) => {
                    self.catalog = catalog;
                    self.catalog_loading = false;
                }
                Update::History(history) => {
                    self.history = history;
                    self.history_loading = false;
                }
                Update::Error(error) => {
                    self.loaded = true;
                    self.error = error;
                }
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
                    let stale_extension = matches!(
                        &result,
                        Err(error)
                            if kind == RequestKind::New && error.contains("unknown command")
                    );
                    if stale_extension {
                        // An extension too old to answer `new` gets the
                        // command typed into the TUI — the same fallback
                        // an offline socket takes.
                        self.send_text("/new".to_string(), false, cx);
                    } else {
                        self.rpc_result(kind, result);
                    }
                }
                Update::SessionFollow { session_id } => {
                    self.follow_session(session_id);
                }
            }
            changed = true;
        }
        // A finished live turn waits on the file; past the linger it is stale.
        if self
            .live
            .as_ref()
            .is_some_and(|live| live_expired(live, Instant::now()))
        {
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
            match result {
                // The terminal interrupt landed: same receipt as the
                // socket path, without waiting on the file to agree.
                Ok(()) => self.mark_stopped(),
                Err(error) => {
                    self.status = format!("couldn't stop: {error}");
                    self.stopped_at = None;
                }
            }
            changed = true;
        }
        // Typing in the filter field lands through the input method, so the
        // query is reconciled here rather than in a keystroke.
        self.sync_picker(cx);
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
        if self
            .live
            .as_ref()
            .is_some_and(|live| live.ended_at.is_some())
        {
            self.live = None;
        }
        self.transcript.set_meta(meta);
        self.last_seen = self.transcript.items.len();
        // File truth adopted after a pick confirms (or corrects) the
        // optimistic chips; a rewind re-opens settled history.
        if self.transcript.items.len() > self.picked_from {
            if self.transcript.model.is_some() {
                self.picked_model = None;
            }
            if self.transcript.thinking_level.is_some() {
                self.picked_thinking = None;
            }
        }
        self.settled_through = self.settled_through.min(from);
        // A user item at or past the abort is the new turn that ends the
        // abort's receipt; the interrupted turn's remnant never is.
        if self.abort_settling()
            && self
                .transcript
                .items
                .get(self.abort_at.min(self.transcript.items.len())..)
                .is_some_and(|tail| {
                    tail.iter()
                        .any(|item| matches!(item.kind, ItemKind::User { .. }))
                })
        {
            self.stopped_at = None;
        }
        self.loaded = true;
        self.rows_dirty = true;
        // The splice has to reach back to the earliest item the deltas
        // rewrote — an outcome can land on a call rows above the tail.
        self.changed_from = Some(self.changed_from.map_or(from, |earlier| earlier.min(from)));
        self.error.clear();
    }

    /// The reply to a socket command: a failed prompt drops the optimistic
    /// bubble, the models reply refills the picker, a state reply refreshes
    /// the pickers, and any refusal lands on the status line.
    fn rpc_result(&mut self, kind: RequestKind, result: Result<Option<Value>, String>) {
        match (kind, result) {
            (RequestKind::Prompt, Err(error)) => {
                self.status = format!("couldn't send: {error}");
                self.pending_prompt = None;
                self.awaiting_response = false;
                self.pending_steering = false;
            }
            (RequestKind::Abort, Err(error)) => {
                self.status = format!("couldn't stop: {error}");
            }
            (RequestKind::Abort, Ok(_)) => {
                // The optimistic `stop()` already recorded this; a repeat
                // keeps the receipt fresh while a late `busy` push still
                // corrects it.
                self.mark_stopped();
            }
            (RequestKind::Models, Ok(Some(data))) => {
                // The live reply lists the models pi can run but names no
                // default; carry the catalog's own default flags across so the
                // composer still falls back to them while the socket is up.
                let previous = std::mem::take(&mut self.catalog);
                self.catalog = models_from(&data)
                    .iter()
                    .map(|model| {
                        let known = previous
                            .iter()
                            .find(|old| old.provider == model.provider && old.id == model.id);
                        AgentModel {
                            provider: model.provider.clone(),
                            id: model.id.clone(),
                            name: model.name.clone(),
                            is_default: known.is_some_and(|old| old.is_default),
                            context_window: None,
                            default_effort: known.and_then(|old| old.default_effort.clone()),
                            efforts: known.and_then(|old| old.efforts.clone()),
                        }
                    })
                    .collect();
            }
            (RequestKind::State, Ok(Some(data))) => {
                // The explicit refresh a fresh socket asks for after its
                // hello: the pushed `state` event usually lands first and
                // this just confirms it.
                if let Ok(state) = serde_json::from_value::<PiState>(data) {
                    self.pi_state = Some(state);
                }
            }
            (RequestKind::SetModel, Err(error)) => {
                // The pick did not take: drop its optimism, keep the error.
                self.picked_model = None;
                self.status = error;
            }
            (RequestKind::SetThinking, Err(error)) => {
                self.picked_thinking = None;
                self.status = error;
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
        // An open picker owns the keyboard while it lasts: arrows walk its
        // rows, Enter takes the highlighted one. Everything else belongs to
        // the filter field, or to the draft when the list is short enough
        // to need no field.
        if self.picker.is_some() {
            match keystroke.key.as_str() {
                "escape" => self.close_picker(cx),
                "up" => self.move_picker(-1, cx),
                "down" => self.move_picker(1, cx),
                "enter" | "tab" => self.choose_picker_cursor(cx),
                _ => return false,
            }
            return true;
        }
        if self.pi_shortcut(keystroke, cx) {
            return true;
        }
        match keystroke.key.as_str() {
            "escape" => {
                // Open menus eat the first Escape in topmost order; the next
                // one closes the panel — unless it is pinned, where Escape
                // does nothing so the pane's terminal stays replaced.
                if self.prompts_open {
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
                self.glide_to_tail(!cx.reduce_motion());
                true
            }
            _ => false,
        }
    }

    /// pi's own TUI shortcuts, answered natively so the panel responds to
    /// the keys a pi user already reaches for. The bindings follow pi's
    /// keybinding reference; anything else falls through to the composer.
    fn pi_shortcut(&mut self, keystroke: &Keystroke, cx: &mut Context<PilusView>) -> bool {
        let mods = keystroke.modifiers;
        if mods.alt || mods.platform {
            return false;
        }
        match (keystroke.key.as_str(), mods.control, mods.shift) {
            ("tab", false, true) => self.cycle_thinking(cx),
            ("t", true, false) => self.toggle_all(|kind| matches!(kind, ItemKind::Thinking { .. })),
            ("o", true, false) => self.toggle_all(is_tool_output),
            ("l", true, false) => self.toggle_picker(PickerKind::Model, cx),
            ("p", true, shift) => self.cycle_model(if shift { -1 } else { 1 }, cx),
            _ => return false,
        }
        true
    }

    /// Step to the next thinking level the current model accepts, wrapping
    /// like pi's Shift+Tab. A model with a single level has nothing to cycle.
    fn cycle_thinking(&mut self, cx: &mut Context<PilusView>) {
        let options = self.thinking_options();
        if let Some(value) = cycle_value(&options, 1) {
            self.choose_picker(PickerKind::Thinking, value, cx);
        }
    }

    /// Step through the model picker's rows, like pi's Ctrl+P and
    /// Shift+Ctrl+P.
    fn cycle_model(&mut self, step: isize, cx: &mut Context<PilusView>) {
        if let Some(value) = cycle_value(&self.model_options(), step) {
            self.choose_picker(PickerKind::Model, value, cx);
        }
    }

    /// Expand every item `wanted` picks, or collapse them all when every one
    /// is already showing: pi's Ctrl+T for thinking and Ctrl+O for tool
    /// output. Expanding also opens the work runs and call groups holding
    /// the items, or they would open behind a collapsed header.
    fn toggle_all(&mut self, wanted: fn(&ItemKind) -> bool) {
        self.sync_list();
        let targets: Vec<usize> = self
            .transcript
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| wanted(&item.kind))
            .map(|(index, _)| index)
            .collect();
        if targets.is_empty() {
            return;
        }
        if targets.iter().all(|index| self.item_shown(*index)) {
            for index in &targets {
                self.expanded.remove(index);
                if self.thinking_is_active(*index) {
                    self.thinking_shut.insert(*index);
                }
            }
        } else {
            for index in targets {
                self.expanded.insert(index);
                self.thinking_shut.remove(&index);
                if let Some(run) = self.work_run_of(index) {
                    self.work_open.insert(run);
                    self.work_shut.remove(&run);
                }
                if let Some(group) = self.call_group_of(index) {
                    self.groups.insert(group);
                }
            }
        }
        self.list.remeasure();
    }

    /// Whether the work run starting at `start` is the one pi is on right
    /// now: it ends the transcript while a turn runs.
    fn work_is_active(&self, start: usize) -> bool {
        self.is_working()
            && self.rows.get(self.row_of(start)).is_some_and(|row| {
                row.kind == RowKind::Work
                    && row.start == start
                    && row.end == self.transcript.items.len()
            })
    }

    /// Whether a work run shows its body: open by default while active.
    fn work_is_open(&self, start: usize) -> bool {
        if self.work_is_active(start) {
            !self.work_shut.contains(&start)
        } else {
            self.work_open.contains(&start)
        }
    }

    /// Whether `index` is a thinking item of the turn in progress, whether
    /// still streaming in the live overlay or already in the transcript's
    /// trailing run.
    fn thinking_is_active(&self, index: usize) -> bool {
        if !self.is_working() {
            return false;
        }
        if index >= LIVE_BASE {
            return self.live.as_ref().is_some_and(|live| {
                matches!(
                    live.items.get(index - LIVE_BASE).map(|item| &item.kind),
                    Some(ItemKind::Thinking { .. })
                )
            });
        }
        let items = &self.transcript.items;
        matches!(
            items.get(index).map(|item| &item.kind),
            Some(ItemKind::Thinking { .. })
        ) && self
            .rows
            .get(self.row_of(index))
            .is_some_and(|row| row.kind != RowKind::Item && row.end == items.len())
    }

    /// Whether a thinking item shows its body: open by default while active.
    fn thinking_is_open(&self, index: usize) -> bool {
        if self.thinking_is_active(index) {
            !self.thinking_shut.contains(&index)
        } else {
            self.expanded.contains(&index)
        }
    }

    /// Whether an item is expanded and nothing collapsed hides it.
    fn item_shown(&self, index: usize) -> bool {
        self.thinking_is_open(index)
            && self
                .work_run_of(index)
                .is_none_or(|run| self.work_is_open(run))
            && self
                .call_group_of(index)
                .is_none_or(|group| self.groups.contains(&group))
    }

    /// The start of the collapsed work run holding `index`, if any.
    fn work_run_of(&self, index: usize) -> Option<usize> {
        let row = self.rows.get(self.row_of(index))?;
        (row.kind == RowKind::Work && (row.start..row.end).contains(&index)).then_some(row.start)
    }

    /// The start of the multi-call group holding a tool call, keyed the way
    /// [`Self::work_group`] keys it. A lone call has no group header.
    fn call_group_of(&self, index: usize) -> Option<usize> {
        let run = self.work_run_of(index)?;
        let items = &self.transcript.items;
        let is_call = |at: usize| matches!(items[at].kind, ItemKind::ToolCall { .. });
        if !is_call(index) {
            return None;
        }
        let row_end = self.rows.get(self.row_of(index))?.end;
        let mut start = index;
        while start > run && is_call(start - 1) {
            start -= 1;
        }
        let mut end = index + 1;
        while end < row_end && is_call(end) {
            end += 1;
        }
        (end - start > 1).then_some(start)
    }

    /// Send the composer's text to the live agent: over the extension
    /// socket when it is up, through the pane's terminal otherwise. Alt
    /// held on Enter queues a follow-up behind a busy turn.
    fn submit(&mut self, input: &Entity<TextInput>, cx: &mut Context<PilusView>, alt: bool) {
        let text = input.read(cx).value().trim().to_string();
        // `/new` is the one slash command with a native path: the socket
        // asks pi for a fresh session and the panel follows the new file
        // itself. With no socket it falls back to typing in the TUI.
        if text == "/new" && self.rpc_up {
            input.update(cx, |input, cx| input.clear(cx));
            self.status = "starting a new session…".to_string();
            self.send_rpc_command(RpcCommand::New);
            cx.notify();
            return;
        }
        match route(&text, self.rpc_up, self.is_working(), alt) {
            Route::Rpc(deliver_as) => self.send_rpc(text, deliver_as, input, cx),
            // Slash commands reach pi's editor through the terminal and
            // leave no user bubble for the transcript to echo, so the
            // composer shows nothing instead of a fake prompt. A typed
            // `/model` or `/thinking` still names the pickers at once.
            Route::Terminal if text.starts_with('/') => {
                input.update(cx, |input, cx| input.clear(cx));
                self.note_picked(&text);
                self.send_text(text, false, cx);
            }
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
        self.send_rpc_command(RpcCommand::Prompt { text, deliver_as });
        cx.notify();
    }

    /// Send a picker command (`/model`, `/thinking`) without touching the
    /// composer's draft. The socket takes the typed command; the terminal
    /// path only remains for when it is down. Either way the pick shows at
    /// once; the socket's report and the file's echo confirm it later.
    fn send_command(&mut self, command: String, cx: &mut Context<PilusView>) {
        self.close_picker(cx);
        self.note_picked(&command);
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

    /// Remember a `/model` or `/thinking` pick so the chips name it while
    /// the confirmation travels: on a fresh session the socket may still
    /// be reconnecting and the file unwritten, while pi already runs on it.
    /// File truth adopted after the pick clears it (see `apply_delta`); a
    /// refusal clears it in `rpc_result`.
    fn note_picked(&mut self, command: &str) {
        if let Some(argument) = command.strip_prefix("/model ") {
            let argument = argument.trim();
            if !argument.is_empty() {
                self.picked_model = Some(argument.to_string());
                self.picked_from = self.transcript.items.len();
            }
        } else if let Some(level) = command.strip_prefix("/thinking ") {
            let level = level.trim();
            if !level.is_empty() {
                self.picked_thinking = Some(level.to_string());
                self.picked_from = self.transcript.items.len();
            }
        }
    }
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
        // A fresh prompt is a new turn: the abort's receipt ends here.
        self.stopped_at = None;
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
    /// optimistic state clears at once and the dock shows a `Stopped`
    /// receipt where `Working` was; the transcript catches up.
    fn stop(&mut self, cx: &mut Context<PilusView>) {
        if self.rpc_up {
            self.send_rpc_command(RpcCommand::Abort);
            self.mark_stopped();
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
        self.mark_stopped();
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

    /// Record an interruption: the socket's `busy` clears so the send
    /// button flips back at once, trailing work counts as settled history
    /// rather than a running turn, and the dock shows a `Stopped` receipt
    /// until a new user turn starts. A late `busy` push still corrects this.
    fn mark_stopped(&mut self) {
        self.pending_prompt = None;
        self.awaiting_response = false;
        self.pending_steering = false;
        if let Some(state) = &mut self.pi_state {
            state.busy = false;
            state.pending = false;
        }
        self.live = None;
        self.settled_through = self.transcript.items.len();
        self.abort_at = self.transcript.items.len();
        self.stopped_at = Some(Instant::now());
    }

    /// Whether we interrupted a turn and no new user turn has started since.
    /// The aborted turn keeps writing its remnant, so the socket-down
    /// fallback must not read that work as a running turn.
    fn abort_settling(&self) -> bool {
        self.stopped_at.is_some()
    }

    /// Give the panel the field its open picker filters through. The view
    /// owns the entity, like the composer prompt; the panel only reads the
    /// query and clears it when a picker opens.
    pub(super) fn set_picker_field(&mut self, field: Entity<TextInput>) {
        self.picker_field = Some(field);
    }

    /// Whether a composer picker is open, so the view hands the keyboard to
    /// its filter field instead of the prompt.
    pub(super) fn picker_open(&self) -> bool {
        self.picker.is_some()
    }

    /// Whether the open picker draws its filter field: only a model list that
    /// outgrows the rows that fit is worth filtering.
    fn picker_filtered(&self) -> bool {
        self.picker == Some(PickerKind::Model)
            && self.picker_list_options().len() > picker_visible_rows(false)
    }

    /// The filter field, while the open picker has one. The view focuses
    /// this instead of the prompt, so a query never lands in the draft the
    /// user is about to send.
    pub(super) fn picker_filter_field(&self) -> Option<Entity<TextInput>> {
        if self.picker_filtered() {
            self.picker_field.clone()
        } else {
            None
        }
    }

    /// Toggle a composer picker from its chip.
    ///
    /// A press outside the picker already closed it in the capture phase; in
    /// that case this click is a toggle-off and must not reopen it. A click
    /// on the *other* chip still opens it directly.
    fn toggle_picker(&mut self, kind: PickerKind, cx: &mut Context<PilusView>) {
        if self.picker_dismissed.take() == Some(kind) || self.picker == Some(kind) {
            self.close_picker(cx);
            return;
        }
        self.picker = Some(kind);
        self.open_picker(cx);
    }

    /// Open a picker for the shot harness, which paints a long catalog
    /// without a live agent behind it.
    #[cfg(feature = "visual-test")]
    pub(super) fn open_picker_shot(&mut self, kind: PickerKind) {
        self.picker = Some(kind);
        self.picker_cursor = self.current_row("");
        self.picker_scroll
            .scroll_to_item(self.picker_cursor, ScrollStrategy::Center);
    }

    /// The catalog the model picker offers, `pub(super)` for the same
    /// reason as [`Self::open_picker_shot`].
    #[cfg(feature = "visual-test")]
    pub(super) fn set_catalog(&mut self, catalog: Vec<AgentModel>) {
        self.catalog = catalog;
        self.catalog_loading = false;
    }

    /// Hold the pickers on their loading skeletons for a shot, since the
    /// catalog read normally lands before the frame is captured.
    #[cfg(feature = "visual-test")]
    pub(super) fn set_catalog_loading(&mut self, loading: bool) {
        self.catalog_loading = loading;
    }

    /// Fill and open the history menu for a shot, since the harness has no
    /// endpoint behind it to list sessions from.
    #[cfg(feature = "visual-test")]
    pub(super) fn open_history_shot(&mut self, entries: Vec<HistoryEntry>) {
        self.history = entries;
        self.history_loading = false;
        self.history_open = true;
    }

    /// A freshly opened picker: an empty query, and the highlight on the
    /// value the session is already using — in a catalog of hundreds of
    /// models, row zero is rarely it.
    fn open_picker(&mut self, cx: &mut Context<PilusView>) {
        self.clear_picker_field(cx);
        self.picker_cursor = self.current_row("");
        self.picker_scroll
            .scroll_to_item(self.picker_cursor, ScrollStrategy::Center);
    }

    /// The row a picker opens its highlight on: the live value when the
    /// query keeps it, the first row left otherwise.
    fn current_row(&self, filter: &str) -> usize {
        let (options, matches) = self.picker_rows(filter);
        if filter.trim().is_empty()
            && let Some(row) = matches.iter().position(|index| options[*index].current)
        {
            return row;
        }
        0
    }

    /// Close the open picker, forgetting its query and highlight.
    fn close_picker(&mut self, cx: &mut Context<PilusView>) {
        self.picker = None;
        self.picker_dismissed = None;
        self.picker_cursor = 0;
        self.clear_picker_field(cx);
    }

    /// Empty the filter field, so a reopened picker never starts on the
    /// previous query.
    fn clear_picker_field(&mut self, cx: &mut Context<PilusView>) {
        self.picker_query.clear();
        if let Some(field) = &self.picker_field {
            field.update(cx, |field, cx| field.clear(cx));
        }
    }

    /// Keep the open picker's highlight honest as its query narrows the
    /// list: a query that drops the highlighted row re-homes it, and the
    /// list scrolls the highlight back into view. Only a changed query
    /// does any of this, so wheel-scrolling the list is never undone by a
    /// repaint.
    fn sync_picker(&mut self, cx: &mut Context<PilusView>) {
        if self.picker.is_none() {
            return;
        }
        let query = self.picker_filter(cx);
        if query == self.picker_query {
            return;
        }
        self.picker_query = query;
        let (_, matches) = self.picker_rows(&self.picker_query);
        // A query picks for the user: the first row it keeps. An empty one
        // leaves the highlight where it is, clamped into range.
        let cursor = if self.picker_query.trim().is_empty() {
            self.picker_cursor.min(matches.len().saturating_sub(1))
        } else {
            0
        };
        if cursor != self.picker_cursor {
            self.picker_cursor = cursor;
            self.picker_scroll
                .scroll_to_item(cursor, ScrollStrategy::Center);
        }
    }

    /// Close the open picker from an outside press, remembering which one so
    /// its own chip's click stays closed.
    fn dismiss_picker(&mut self, cx: &mut Context<PilusView>) {
        if self.picker.is_some() {
            self.picker_dismissed = self.picker;
        }
        self.close_picker(cx);
    }

    /// The open picker's query, read from its filter field.
    fn picker_filter(&self, cx: &App) -> String {
        self.picker_field
            .as_ref()
            .map(|field| field.read(cx).value())
            .unwrap_or_default()
    }

    /// The open picker's options and the rows `filter` keeps.
    fn picker_rows(&self, filter: &str) -> (Vec<PickerOption>, Vec<usize>) {
        let options = self.picker_list_options();
        let matches = picker_matches(&options, filter);
        (options, matches)
    }

    /// Every row the open picker offers, current value marked.
    fn picker_list_options(&self) -> Vec<PickerOption> {
        match self.picker {
            Some(PickerKind::Thinking) => self.thinking_options(),
            _ => self.model_options(),
        }
    }

    /// The model picker's rows, current model marked.
    fn model_options(&self) -> Vec<PickerOption> {
        picker_options(
            &self.catalog,
            &self.transcript.models,
            &self.current_model_slug(),
        )
    }

    /// The thinking levels the current model accepts, as the catalog spells
    /// them. `None` when the model is unknown, so the picker falls back to
    /// pi's full set instead of guessing wrong.
    fn current_model_efforts(&self) -> Option<Vec<String>> {
        let slug = self.current_model_slug();
        if slug.is_empty() {
            return None;
        }
        self.catalog
            .iter()
            .find(|model| model_slug(model) == slug)
            .and_then(|model| model.efforts.clone())
    }

    /// The thinking picker's rows: exactly the levels the current model
    /// accepts. A model with no reasoning gets only `off` (pi's own rule),
    /// and an unknown model gets the full set.
    fn thinking_options(&self) -> Vec<PickerOption> {
        let current = self.current_thinking().unwrap_or_default();
        let levels: Vec<String> = match self.current_model_efforts() {
            Some(efforts) if efforts.is_empty() => vec!["off".to_string()],
            Some(efforts) => efforts,
            None => THINKING_LEVELS
                .iter()
                .map(|level| (*level).to_string())
                .collect(),
        };
        levels
            .into_iter()
            .map(|level| {
                let value = pilus_client::thinking_level(&level);
                PickerOption {
                    label: pilus_client::effort_label(&level),
                    current: value == current,
                    value,
                    hint: None,
                }
            })
            .collect()
    }

    /// Move the open picker's highlight, keeping it in view.
    fn move_picker(&mut self, delta: isize, cx: &mut Context<PilusView>) {
        let (_, matches) = self.picker_rows(&self.picker_filter(cx));
        self.picker_cursor = picker_cursor_after(self.picker_cursor, matches.len(), delta);
        self.picker_scroll
            .scroll_to_item(self.picker_cursor, ScrollStrategy::Center);
    }

    /// Take the picker's highlighted row, as the live agent would.
    fn choose_picker_cursor(&mut self, cx: &mut Context<PilusView>) {
        let Some(kind) = self.picker else {
            return;
        };
        let (options, matches) = self.picker_rows(&self.picker_filter(cx));
        if let Some(value) = matches
            .get(self.picker_cursor)
            .map(|index| options[*index].value.clone())
        {
            self.choose_picker(kind, value, cx);
        }
    }

    /// Send a picker's choice to the live agent as its slash command.
    fn choose_picker(&mut self, kind: PickerKind, value: String, cx: &mut Context<PilusView>) {
        let command = match kind {
            PickerKind::Model => format!("/model {value}"),
            PickerKind::Thinking => format!("/thinking {value}"),
        };
        self.send_command(command, cx);
    }

    /// Toggle one card between collapsed and expanded.
    pub(super) fn toggle(&mut self, index: usize) {
        let set = if self.thinking_is_active(index) {
            &mut self.thinking_shut
        } else {
            &mut self.expanded
        };
        if !set.remove(&index) {
            set.insert(index);
        }
        self.note_disclosed(Toggle::Item(index));
        // Remeasure the row in place, so expanding or collapsing it never
        // reads as the row vanishing.
        let row = self.row_of(index);
        self.list.remeasure_items(row..row + 1);
    }

    /// Scroll one absolute item into view, leaving the tail pin behind.
    ///
    /// Glides there unless motion is reduced or the row is already close
    /// enough that easing would only read as lag.
    fn jump_to(&mut self, index: usize, animate: bool) {
        self.sync_list();
        if index >= self.transcript.items.len() {
            return;
        }
        let target = ListOffset {
            item_ix: self.row_of(index),
            offset_in_item: px(0.0),
        };
        if !animate {
            self.scroll_tween = None;
            self.list.scroll_to(target);
            return;
        }
        let from = self.settle_scroll_position();
        // Resolve the destination's pixel offset by landing on it and
        // coming straight back; nothing paints in between.
        let home = self.list.logical_scroll_top();
        self.list.scroll_to(target);
        let to = -self.list.scroll_px_offset_for_scrollbar().y.as_f32();
        self.list.scroll_to(home);
        self.start_glide(from, ScrollGoal::Item { index, to });
    }

    /// Re-pin the list to the tail: follow new content and anchor at the end.
    fn follow_tail(&mut self) {
        self.scroll_tween = None;
        self.list.set_follow_mode(FollowMode::Tail);
    }

    /// Glide to the tail, then re-pin it.
    fn glide_to_tail(&mut self, animate: bool) {
        if !animate || self.list.is_following_tail() {
            self.follow_tail();
            return;
        }
        let from = self.settle_scroll_position();
        self.start_glide(from, ScrollGoal::Bottom);
    }

    /// The list's scroll position in pixels, with the tail's "past the end"
    /// anchor swapped for the real bottom edge so a glide starts from
    /// what is on screen.
    fn settle_scroll_position(&mut self) -> f32 {
        let count = self.list.item_count();
        if self.list.is_following_tail() || self.list.logical_scroll_top().item_ix >= count {
            let viewport = self.list.viewport_bounds().size.height;
            self.list.scroll_by(-viewport);
        }
        -self.list.scroll_px_offset_for_scrollbar().y.as_f32()
    }

    fn start_glide(&mut self, from: f32, goal: ScrollGoal) {
        let to = match goal {
            ScrollGoal::Bottom => self.list.max_offset_for_scrollbar().y.as_f32(),
            ScrollGoal::Item { to, .. } => to,
        };
        let distance = (to - from).abs();
        let extra = SCROLL_GLIDE_MAX - SCROLL_GLIDE_MIN;
        let duration = SCROLL_GLIDE_MIN + extra.mul_f32((distance / 6000.0).clamp(0.0, 1.0));
        self.scroll_tween = Some(ScrollTween {
            from,
            goal,
            started: Instant::now(),
            duration,
        });
    }

    /// Advance the glide one frame; true while it still needs frames.
    pub(super) fn tick_scroll(&mut self) -> bool {
        let Some(tween) = &self.scroll_tween else {
            return false;
        };
        let step = (tween.started.elapsed().as_secs_f32() / tween.duration.as_secs_f32()).min(1.0);
        if step >= 1.0 {
            let goal = tween.goal;
            self.scroll_tween = None;
            match goal {
                ScrollGoal::Bottom => self.follow_tail(),
                ScrollGoal::Item { index, .. } => self.jump_to(index, false),
            }
            return true;
        }
        let to = match tween.goal {
            // The bottom moves while rows measure and output streams in.
            ScrollGoal::Bottom => self.list.max_offset_for_scrollbar().y.as_f32(),
            ScrollGoal::Item { to, .. } => to,
        };
        let eased = 1.0 - (1.0 - step).powi(5);
        let wanted = tween.from + (to - tween.from) * eased;
        let now = -self.list.scroll_px_offset_for_scrollbar().y.as_f32();
        self.list.scroll_by(px(wanted - now));
        true
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
            let old_count = self.rows.len() + self.list_pending as usize + self.list_live as usize;
            let new_count = new.len() + pending as usize + live as usize;
            let old_coverage = self.rows.last().map_or(0, |row| row.end);
            let new_coverage = new.last().map_or(0, |row| row.end);
            // Only items pi appends mid-session fade in. A first load, or
            // items the live overlay or the optimistic bubble already
            // showed, would flash a second time as they land in the file.
            if old_coverage > 0
                && new_coverage > old_coverage
                && !self.list_live
                && !self.list_pending
            {
                self.arrived = Some((old_coverage, Instant::now()));
            }
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
                self.list
                    .splice(self.rows.len()..self.rows.len() + old_tail, new_tail);
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
        self.note_disclosed(Toggle::Group(index));
        let row = self.row_of(index);
        self.list.remeasure_items(row..row + 1);
    }

    /// Toggle one run of work between assistant replies.
    pub(super) fn toggle_work(&mut self, index: usize) {
        let set = if self.work_is_active(index) {
            &mut self.work_shut
        } else {
            &mut self.work_open
        };
        if !set.remove(&index) {
            set.insert(index);
        }
        self.note_disclosed(Toggle::Work(index));
        let row = self.row_of(index);
        self.list.remeasure_items(row..row + 1);
    }

    /// Remember the section just toggled, so its chevron turns and, when it
    /// opened, its body fades in.
    fn note_disclosed(&mut self, toggle: Toggle) {
        self.disclosed = Some((toggle, Instant::now()));
    }

    /// Whether `toggle` is the section the user flipped a moment ago.
    fn recently_toggled(&self, toggle: Toggle) -> bool {
        self.disclosed
            .is_some_and(|(last, at)| last == toggle && at.elapsed() < DISCLOSE_WINDOW)
    }

    /// Fade an item in when it has just been appended to the transcript.
    ///
    /// Gated on a short window for the same reason as [`Self::disclose`]:
    /// the list drops element state for rows that scroll away, so an
    /// unconditional fade would replay on every scroll back.
    fn arrive(&self, index: usize, content: AnyElement) -> AnyElement {
        match self.arrived {
            Some((from, at)) if index >= from && at.elapsed() < DISCLOSE_WINDOW => style::enter(
                gpui::SharedString::from(format!("session-arrive-{index}")),
                div().w_full().child(content),
            )
            .into_any_element(),
            _ => content,
        }
    }

    /// A section body, faded in when the user has just opened it.
    ///
    /// Only a fresh disclosure animates: the list drops the element state of
    /// rows that scroll away, so a fade keyed on the body alone would replay
    /// whenever an open section scrolled back into view.
    fn disclose(&self, toggle: Toggle, body: Div) -> AnyElement {
        if self.recently_toggled(toggle) {
            style::enter(
                gpui::SharedString::from(format!("session-disclose-{}", toggle_key(toggle))),
                body,
            )
            .into_any_element()
        } else {
            body.into_any_element()
        }
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
            .radius_md()
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
            .radius_lg()
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
        let expanded = match &item.kind {
            ItemKind::Thinking { .. } => self.thinking_is_open(index),
            _ => self.expanded.contains(&index),
        };
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
            .child(style::chevron(
                format!("session-chevron-{}", toggle_key(toggle)),
                expanded,
                self.recently_toggled(toggle),
                fonts.label,
                theme.muted,
            ))
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
                    .child(clip_lines(&body, EXPANDED_LINES)),
                Section::Markdown(body_fonts) => div().pb_2().child(super::markdown::render(
                    &body,
                    Theme {
                        fg: theme.muted,
                        ..theme
                    },
                    body_fonts,
                    &self.settings,
                )),
            };
            column = column.child(self.disclose(Toggle::Item(index), block));
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
                self.disclose(
                    Toggle::Item(index),
                    div()
                        .pb_2()
                        .overflow_hidden()
                        .font_family(style::mono_family(&self.settings))
                        .text_size(px(fonts.secondary))
                        .text_color(theme.muted)
                        .whitespace_normal()
                        .child(clip_lines(&details, EXPANDED_LINES)),
                ),
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
            column = column.child(self.disclose(Toggle::Group(start), inner));
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
        let expanded = self.work_is_open(start);
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
                    let group =
                        self.tool_group(start + group_start, &run[group_start..index], fonts, cx);
                    inner = inner.child(self.arrive(start + group_start, group));
                } else {
                    let element = self.item(start + index, &run[index], fonts, cx);
                    inner = inner.child(self.arrive(start + index, element));
                    index += 1;
                }
            }
            column = column.child(self.disclose(Toggle::Work(start), inner));
        }
        column.into_any_element()
    }

    /// The header strip: the session's first prompt, once it has one, then
    /// the history and close buttons. Until a first line lands a remote pane
    /// shows a skeleton bar while it crosses the wire; a local pane, or a
    /// remote one whose reader has already confirmed an empty session, holds
    /// the space with nothing. Model, cost, and usage live in the composer.
    fn header(&self, fonts: Fonts, cx: &mut Context<PilusView>) -> Div {
        let theme = self.theme;
        let title = match self.first_prompt() {
            Some(prompt) => div()
                .min_w(px(0.0))
                .overflow_hidden()
                .text_size(px(fonts.body))
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(theme.fg)
                .whitespace_nowrap()
                .text_ellipsis()
                .child(prompt)
                .into_any_element(),
            // The first line of a remote session arrives a round trip after the
            // panel opens, so a skeleton holds its place until the reader
            // reports one (or confirms there is none).
            None if self.awaiting_title() => {
                style::skeleton("pilus-session-title-skeleton", theme, px(280.0), fonts.body)
                    .into_any_element()
            }
            // Nothing to wait for: a local pane reads at once, and a loaded
            // remote pane simply has no title yet.
            None => div().into_any_element(),
        };
        let mut row = div()
            .relative()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap_2()
            .h(px(40.0))
            .px_4()
            .child(div().flex_1().min_w(px(0.0)).child(title))
            .child(self.history_button(cx))
            .child(self.close_button(cx));
        // The menu floats below the header, right-aligned under its button,
        // so opening it never shifts the transcript.
        if self.history_open {
            row = row.child(
                div()
                    .absolute()
                    .top(relative(1.0))
                    .right(px(0.0))
                    .pt_2()
                    .child(self.history_menu(fonts, cx)),
            );
        }
        row
    }

    /// The header's history button: opens the session menu.
    fn history_button(&self, cx: &mut Context<PilusView>) -> Stateful<Div> {
        let theme = self.theme;
        header_icon_button(
            "session-history-button",
            include_bytes!("../../assets/icons/clock.svg"),
            theme,
        )
        .on_click(cx.listener(|this: &mut PilusView, _, _, cx| {
            this.session.toggle_history(cx);
        }))
    }

    /// The header's close button: dismisses the panel. The click is also a
    /// release, so the auto-pin does not reopen the same pane next tick.
    fn close_button(&self, cx: &mut Context<PilusView>) -> Stateful<Div> {
        let theme = self.theme;
        header_icon_button(
            "session-close-button",
            include_bytes!("../../assets/icons/x.svg"),
            theme,
        )
        .on_click(cx.listener(|this: &mut PilusView, _, _, cx| {
            this.release_pi_session(cx);
        }))
    }

    /// The session history menu: the endpoint's earlier sessions, newest
    /// first. Choosing one reads it in place, read-only.
    fn history_menu(&self, fonts: Fonts, cx: &mut Context<PilusView>) -> AnyElement {
        let theme = self.theme;
        let rows = self.history.len().clamp(1, picker_visible_rows(false));
        let list_h = super::picker_rows_h(rows).min(super::PICKER_MENU_MAX_H);
        let mut list = super::picker_menu("pilus-session-history", theme)
            .max_h(px(list_h))
            .on_mouse_down_out(cx.listener(|this: &mut PilusView, _, _, cx| {
                this.session.dismiss_history();
                cx.notify();
            }));
        if self.history_loading && self.history.is_empty() {
            for row in 0..PICKER_SKELETON_ROWS {
                list = list.child(
                    div()
                        .flex()
                        .items_center()
                        .h(px(super::PICKER_ROW_H))
                        .px_3()
                        .child(style::skeleton(
                            ("session-history-skeleton", row),
                            theme,
                            relative(0.72),
                            fonts.body,
                        )),
                );
            }
        } else if self.history.is_empty() {
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .h(px(super::PICKER_ROW_H))
                    .px_3()
                    .text_size(px(fonts.body))
                    .text_color(theme.muted)
                    .child("No earlier sessions"),
            );
        } else {
            for (index, entry) in self.history.iter().enumerate() {
                let path = entry.path.clone();
                let active = self.reported.as_deref() == Some(entry.id.as_str());
                list = list.child(super::picker_row(
                    ("session-history-option", index),
                    entry.label(),
                    active,
                    theme,
                    fonts.body,
                    cx.listener(move |this: &mut PilusView, _, _, cx| {
                        // Browsing an old session detaches from the live pane,
                        // so remember the release before the panel retargets.
                        this.release_pi_for_history();
                        this.session.open_history_entry(path.clone(), cx);
                        cx.notify();
                    }),
                ));
            }
        }
        style::enter("session-history-enter", list).into_any_element()
    }

    /// Whether the session's bytes live on another machine, where every read
    /// is a round trip behind the panel.
    fn is_remote(&self) -> bool {
        matches!(self.endpoint, Some(Endpoint::Remote { .. }))
    }

    /// Whether the header still waits on a first line: a remote pane whose
    /// reader has not reported a fold, an error, or an empty session yet.
    fn awaiting_title(&self) -> bool {
        self.is_remote() && !self.loaded
    }

    /// The session's first prompt as one line, or `None` while the
    /// transcript has no user message yet.
    fn first_prompt(&self) -> Option<String> {
        let text = self
            .transcript
            .items
            .iter()
            .find_map(|item| match &item.kind {
                ItemKind::User { text, .. } => Some(text.as_str()),
                _ => None,
            })?;
        let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
        Some(truncate(&one_line, TITLE_CHARS))
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
        if self.shows_empty_state() {
            return self.empty_state(fonts);
        }
        if self.transcript.items.is_empty() && self.pending_prompt.is_none() && self.live.is_none()
        {
            // A remote pane whose reader has not reported holds a blank
            // rather than flash the placeholder before its first fold lands.
            return div().flex_1().min_h(px(0.0)).into_any_element();
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

    /// Whether the body shows the fresh-session placeholder: the panel is
    /// open with nothing on its way in — no prompt sent, no live turn. A
    /// pane with no reported session is a fresh start, so the placeholder
    /// paints at once instead of waiting on a read; a known session waits
    /// for the reader to confirm it is empty, so an existing one never
    /// flashes the placeholder before its first fold lands.
    fn shows_empty_state(&self) -> bool {
        self.open
            && self.error.is_empty()
            && self.transcript.items.is_empty()
            && self.pending_prompt.is_none()
            && self.live.is_none()
            && (self.loaded || self.reported.is_none())
    }

    /// The directory the session runs in, `~`-shortened, for the empty
    /// state's subtitle.
    fn cwd_display(&self) -> Option<String> {
        let cwd = match self.endpoint.as_ref()? {
            Endpoint::Local { cwd, .. } | Endpoint::Remote { cwd, .. } => cwd,
        };
        (!cwd.is_empty()).then(|| super::shorten_home(cwd))
    }

    /// The fresh-session placeholder, centered in the transcript area:
    /// nothing has been asked yet, so the headline names the next step and
    /// the directory says where it will run.
    fn empty_state(&self, fonts: Fonts) -> AnyElement {
        let theme = self.theme;
        let mut column = div()
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .px_6()
            .text_center()
            .child(
                div()
                    .text_size(px(fonts.headline))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(theme.fg)
                    .child("What should Pi do?"),
            );
        if let Some(cwd) = self.cwd_display() {
            column = column.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(super::icon(
                        include_bytes!("../../assets/icons/folder.svg"),
                        14.0,
                        theme.muted,
                    ))
                    .child(div().max_w(px(420.0)).truncate().child(cwd)),
            );
        }
        column.into_any_element()
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
            RowKind::Item => {
                let element = self.item(row.start, &items[row.start], fonts, cx);
                self.arrive(row.start, element)
            }
            // A run that is only thinking needs no `Working…` wrapper; the
            // thinking rows stand on their own.
            RowKind::Thinking => {
                let mut column = div().flex().flex_col().gap_4();
                for (offset, item) in items[row.start..row.end].iter().enumerate() {
                    let element = self.item(row.start + offset, item, fonts, cx);
                    column = column.child(
                        div()
                            .flex_shrink_0()
                            .child(self.arrive(row.start + offset, element)),
                    );
                }
                column.into_any_element()
            }
            RowKind::Work => {
                let element = self.work_group(
                    row.start,
                    &items[row.start..row.end],
                    row.end == items.len(),
                    fonts,
                    cx,
                );
                self.arrive(row.start, element)
            }
        };
        div()
            .w_full()
            .flex()
            .flex_row()
            .justify_center()
            .px_6()
            .pb_4()
            .child(div().w_full().max_w(px(TRANSCRIPT_MAX_W)).child(content))
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
                .child(div().w_full().max_w(px(TRANSCRIPT_MAX_W)).child(content))
                .into_any_element()
        };
        let extra = ix.saturating_sub(self.rows.len());
        if extra == 0
            && let Some(prompt) = self.pending_prompt.clone()
        {
            return frame(
                style::enter(
                    "session-pending-prompt",
                    div().w_full().child(self.user_bubble(&prompt, 0, fonts)),
                )
                .into_any_element(),
            );
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
            let element = self.item(LIVE_BASE + offset, item, fonts, cx);
            column = column.child(div().flex_shrink_0().child(style::enter(
                gpui::SharedString::from(format!("session-live-{offset}")),
                div().w_full().child(element),
            )));
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
                    .radius_md()
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
        let has_text = !input.read(cx).value().trim().is_empty();
        let mut bar = div()
            .flex()
            .flex_col()
            .radius_lg()
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
            .child(self.meta_row(has_text, fonts, cx));
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
        let measured = self.dock_width.get();
        let compact = measured > 0.0 && measured < COMPACT_BELOW;
        let gutter = if compact { 0.0 } else { 2.0 * JUMP_GUTTER };
        let mut dock = div()
            .w_full()
            .max_w(px(COMPOSER_MAX_W + gutter))
            .flex()
            .flex_col();
        let band = |content: Div| {
            if compact {
                div().child(content)
            } else {
                dock_slot(content)
            }
        };
        if self.is_working() {
            dock = dock.child(band(self.working_row(fonts)));
        } else if self.show_stopped() {
            dock = dock.child(band(self.stopped_row(fonts)));
        }
        // The prompts jumper shows whenever there is a prompt to jump to;
        // the tail arrow only while unpinned.
        let user_turns = self
            .transcript
            .items
            .iter()
            .filter(|item| matches!(item.kind, ItemKind::User { .. }))
            .count()
            + usize::from(self.pending_prompt.is_some());
        let has_prompts = user_turns > 1;
        let show_cluster = has_prompts || self.prompts_open;
        let panel_row = if compact {
            // Full width, with the buttons riding the panel's top edge.
            let mut row = div().relative().child(bar);
            if show_cluster {
                row = row.child(
                    div()
                        .absolute()
                        .bottom(relative(1.0))
                        .right(px(8.0))
                        .pb_2()
                        .child(self.jump_cluster(true, fonts, cx)),
                );
            }
            row
        } else {
            // The cluster's column is always reserved, so the panel never
            // narrows when the first prompt lands; a loading transcript
            // holds a skeleton there until it knows whether there is one.
            let mut row = dock_slot(bar);
            row = if show_cluster {
                row.child(self.jump_cluster(false, fonts, cx))
            } else if !self.loaded {
                row.child(
                    div()
                        .w(px(JUMP_BUTTON))
                        .flex_shrink_0()
                        .child(style::skeleton(
                            "session-jump-skeleton",
                            theme,
                            px(JUMP_BUTTON),
                            JUMP_BUTTON,
                        )),
                )
            } else {
                row.child(div().w(px(JUMP_BUTTON)).flex_shrink_0())
            };
            row
        };
        // The dock centers in the pane, so the panel itself stays centered
        // while the buttons hang in the gutter beside it.
        dock = dock.child(panel_row);
        let width_probe = self.dock_width.clone();
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
                    .relative()
                    .w_full()
                    .flex()
                    .flex_row()
                    .justify_center()
                    .child(
                        // Measures the row, which is pane-wide in both
                        // layouts, so the layout choice cannot feed back.
                        canvas(
                            move |bounds, window, _cx| {
                                let width = bounds.size.width.as_f32();
                                if (width_probe.get() - width).abs() > 0.5 {
                                    width_probe.set(width);
                                    window.refresh();
                                }
                            },
                            |_, (), _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
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
    /// An idle button with an empty draft is disabled, so the arrow reads
    /// as unavailable until there is something to send.
    fn send_button(&self, enabled: bool, cx: &mut Context<PilusView>) -> Stateful<Div> {
        let theme = self.theme;
        let stopping = self.is_working();
        let active = stopping || enabled;
        // A filled square reads as "stop" at 15px where the stroked outline
        // reads as a rounded box, so the glyph carries weight the send arrow
        // does not need.
        let glyph: &'static [u8] = if stopping {
            include_bytes!("../../assets/icons/square-fill.svg")
        } else {
            include_bytes!("../../assets/icons/arrow-up.svg")
        };
        let mut button = div()
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
            .radius_full()
            .bg(theme.bg);
        // The fill goes in before the glyph so it paints beneath it.
        if active {
            button = button
                .cursor_pointer()
                .hover_fill(theme.border, style::RADIUS_FULL)
                .on_click(cx.listener(move |this: &mut PilusView, _, _, cx| {
                    if stopping {
                        this.session.stop(cx);
                    } else {
                        let input = this.session_prompt.clone();
                        this.session.submit(&input, cx, false);
                    }
                }));
        } else {
            button = button.opacity(0.4);
        }
        button.child(super::icon(glyph, 15.0, theme.panel_fg))
    }

    /// The jump buttons beside the composer: the prompts menu over a row
    /// with the prompts button and the scroll-to-bottom arrow.
    fn jump_cluster(&self, compact: bool, fonts: Fonts, cx: &mut Context<PilusView>) -> AnyElement {
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
                .radius_full()
                .cursor_pointer()
                .state_fill(theme.panel, theme.border, style::RADIUS_FULL)
                .child(super::icon(glyph, 15.0, theme.panel_fg))
        };
        // A narrow column beside the panel; the menu leaves it, so opening
        // one never widens the dock.
        let mut cluster = div().relative().flex_shrink_0().flex().items_center();
        cluster = if compact {
            cluster.flex_row()
        } else {
            cluster.w(px(30.0)).flex_col()
        };
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
        let mut row = div().flex().items_center().gap_2();
        row = if compact {
            row.flex_row()
        } else {
            row.flex_col()
        };
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
                    this.session.glide_to_tail(!cx.reduce_motion());
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
        let rows = prompts.len().clamp(1, PROMPTS_MAX_ROWS);
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
                    this.session.jump_to(index, !cx.reduce_motion());
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

    /// Toggle the history menu from its header button, loading the
    /// endpoint's sessions on first open. An outside press already closed
    /// it in the capture phase; then the click stays closed.
    fn toggle_history(&mut self, cx: &mut Context<PilusView>) {
        if self.history_dismissed {
            self.history_dismissed = false;
            self.history_open = false;
            return;
        }
        self.history_open = !self.history_open;
        if self.history_open {
            self.load_history();
        }
        cx.notify();
    }

    /// Close the history menu from an outside press.
    fn dismiss_history(&mut self) {
        self.history_dismissed = self.history_open;
        self.history_open = false;
    }

    /// Read the endpoint's earlier sessions on a background thread. The
    /// panel's own channel carries the result, so a listing left over from
    /// a previous open is dropped when `rotate_channels` replaced it.
    fn load_history(&mut self) {
        if self.history_loading {
            return;
        }
        let Some(endpoint) = self.endpoint.clone() else {
            return;
        };
        self.history_loading = true;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let entries = match &endpoint {
                Endpoint::Local { cwd, .. } => Self::local_history(cwd),
                Endpoint::Remote { target, cwd } => Self::remote_history(target, cwd),
            };
            let _ = tx.send(Update::History(entries));
        });
    }

    /// Switch the panel to a session chosen from the history menu.
    ///
    /// The chosen session is an older file, not the pane's live one, so the
    /// panel drops its pane target: the transcript is read-only, and no
    /// prompt or state push can land in it.
    pub(super) fn open_history_entry(&mut self, path: PathBuf, cx: &mut Context<PilusView>) {
        let Some(endpoint) = self.endpoint.clone() else {
            return;
        };
        self.open_with(
            endpoint,
            ReadSpec {
                explicit: Some(path),
                session_id: None,
                resolve_latest: false,
                display: String::new(),
            },
            None,
            cx,
        );
    }

    /// The endpoint's earlier sessions, newest first, from the local store.
    fn local_history(cwd: &str) -> Vec<HistoryEntry> {
        if cwd.is_empty() {
            return Vec::new();
        }
        pilus_session::listing(&pilus_session::sessions_root(), cwd, HISTORY_LIMIT)
            .into_iter()
            .map(|session| HistoryEntry {
                path: session.path,
                id: session.id,
                title: session.title,
            })
            .collect()
    }

    /// The remote mirror of [`Self::local_history`]: one SSH round trip
    /// that lists the cwd's sessions and samples each opener, since a read
    /// per session would be a round trip per row.
    fn remote_history(target: &SshTarget, cwd: &str) -> Vec<HistoryEntry> {
        if cwd.is_empty() {
            return Vec::new();
        }
        let dir = pilus_session::dir_name_for(cwd);
        let command = format!(
            "root=\"${{PI_CODING_AGENT_SESSION_DIR:-$HOME/.pi/agent/sessions}}\"; \
             for f in $(ls -t \"$root\"/{dir}/*.jsonl 2>/dev/null | head -n {HISTORY_LIMIT}); do \
             printf '\\n@@PILUS %s\\n' \"$f\"; grep -m1 '\"role\":\"user\"' \"$f\" | head -c {HISTORY_SAMPLE_BYTES}; printf '\\n'; done"
        );
        let Ok(output) = target.shell(&command).output() else {
            return Vec::new();
        };
        if !output.status.success() {
            return Vec::new();
        }
        let mut entries = parse_history_output(&String::from_utf8_lossy(&output.stdout));
        entries.truncate(HISTORY_LIMIT);
        entries
    }

    /// Whether the session file has grown recently: live work, not a stale
    /// interrupted tail.
    fn recently_active(&self) -> bool {
        self.last_growth
            .is_some_and(|at| at.elapsed() < ACTIVE_WINDOW)
    }

    /// Whether pi is still working: while the socket is up its `busy` is
    /// exact; the heuristics below are the fallback for when it is down.
    ///
    /// A trailing work item alone is not a turn: an interrupted session's
    /// file also ends in one, so the fallback only counts it while the file
    /// is actually growing. Otherwise opening a stopped session would paint
    /// `Working` and the stop square forever.
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
        // The interrupted turn keeps writing its remnant; that work is not
        // a running turn until a new user prompt starts one.
        if self.abort_settling() {
            return false;
        }
        self.work_in_flight() && self.recently_active()
    }

    /// Whether the transcript ends mid-work: pi's last word past the last
    /// abort was a tool call, its result, or its reasoning, so a turn is
    /// running. Work an abort settled is history, not a turn.
    fn work_in_flight(&self) -> bool {
        let settled = self.settled_through.min(self.transcript.items.len());
        self.transcript.items[settled..]
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

    /// Whether the dock shows the `Stopped` receipt: we interrupted and pi
    /// is not working again. It rides through the aborted turn's remnant
    /// and ends only when a new user turn starts (or `is_working` turns
    /// true for another reason), so the composer never flips back to
    /// `Working` on the interrupted tail.
    fn show_stopped(&self) -> bool {
        self.abort_settling() && !self.is_working()
    }

    /// The `(spinner) Working` line above the composer, or `Steering…` when
    /// the pending prompt went out while pi was already working.
    ///
    /// The spinner runs on its own clock rather than on transcript growth,
    /// so it keeps turning through a long silent think.
    fn working_row(&self, fonts: Fonts) -> Div {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_1()
            .pb_1()
            .text_size(px(fonts.label))
            .text_color(self.theme.muted)
            .child(style::spinner(
                "session-working-spinner",
                fonts.label,
                self.theme.muted,
            ))
            .child(if self.pending_steering {
                "Steering…"
            } else {
                "Working"
            })
    }

    /// The `Stopped` receipt above the composer: the band `Working` held,
    /// so the panel never shifts when a turn ends this way.
    fn stopped_row(&self, fonts: Fonts) -> Div {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_1()
            .pb_1()
            .text_size(px(fonts.label))
            .text_color(self.theme.muted)
            .child("■")
            .child("Stopped")
    }

    /// Model and thinking pickers, token stats, and the send arrow below the
    /// input. The right side holds only the send button, disabled while the
    /// draft is empty.
    fn meta_row(&self, has_text: bool, fonts: Fonts, cx: &mut Context<PilusView>) -> Div {
        let theme = self.theme;
        let transcript = &self.transcript;
        // The chips, gauge, and token counts scroll sideways when they
        // outgrow the panel; the send button stays put beside them.
        let mut row = div().flex().flex_none().flex_row().items_center().gap_1();
        let cpu: &'static [u8] = include_bytes!("../../assets/icons/cpu.svg");
        let gauge: &'static [u8] = include_bytes!("../../assets/icons/gauge.svg");
        // Until the session names its model, a skeleton holds the chip's
        // place: a placeholder that the first report renames would jump
        // the whole row sideways.
        row = row.child(if self.model_pending() {
            self.chip_skeleton("session-model-skeleton", cpu, MODEL_SKELETON_W, fonts)
                .into_any_element()
        } else {
            self.picker_chip(
                "session-model-picker",
                cpu,
                self.model_display(),
                PickerKind::Model,
                fonts,
                cx,
            )
            .into_any_element()
        });
        row = row.child(if self.thinking_pending() {
            self.chip_skeleton(
                "session-thinking-skeleton",
                gauge,
                THINKING_SKELETON_W,
                fonts,
            )
            .into_any_element()
        } else {
            let thinking = match self.current_thinking() {
                Some(level) => pilus_client::effort_label(&level),
                None => "Thinking".to_string(),
            };
            self.picker_chip(
                "session-thinking-picker",
                gauge,
                thinking,
                PickerKind::Thinking,
                fonts,
                cx,
            )
            .into_any_element()
        });
        if let Some(context) = self.context_usage().filter(ctx_visible) {
            let context = &context;
            let color = if ctx_warns(context) {
                theme.alert
            } else {
                theme.muted
            };
            row = row.child(
                div()
                    .pl_2()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(style::context_gauge(
                        "session-context-gauge",
                        ctx_percent(context).map(|percent| percent as f32),
                        ctx_warns(context),
                        theme,
                        style::scaled(&self.settings, style::GAUGE_SIZE),
                    ))
                    .child(
                        div()
                            .text_size(px(fonts.label))
                            .text_color(color)
                            .whitespace_nowrap()
                            .child(ctx_counts(context)),
                    ),
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
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_2()
            .pt_1()
            .pb_2()
            .child(
                div()
                    .id("session-meta-scroll")
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_x_scroll()
                    .track_scroll(&self.meta_scroll)
                    .child(row),
            )
            .child(self.send_button(has_text, cx))
    }

    /// The context occupancy to paint: the extension's own report when it
    /// is up, else one rebuilt from the transcript — the latest response's
    /// token total over the current model's catalog window — so a session
    /// whose socket is down, or whose machine never loaded the extension
    /// (a fresh SSH host), still shows its gauge.
    fn context_usage(&self) -> Option<ContextUsage> {
        if let Some(context) = self
            .pi_state
            .as_ref()
            .and_then(|state| state.context.clone())
        {
            return Some(context);
        }
        let slug = self.current_model_slug();
        let window = self
            .catalog
            .iter()
            .find(|model| model_slug(model) == slug)
            .or_else(|| self.catalog.iter().find(|model| model.id == slug))
            .and_then(|model| model.context_window)?;
        Some(ContextUsage {
            tokens: self.transcript.context_tokens,
            context_window: window,
            percent: None,
        })
    }

    /// Whether the panel is still waiting to learn what a chip should say:
    /// the reader's first fold, the catalog, or — briefly — the extension
    /// socket's first report, which outranks both.
    fn chips_loading(&self) -> bool {
        let socket_settling = self.rpc_tx.is_some()
            && !self.rpc_up
            && self.opened_at.is_some_and(|at| at.elapsed() < CHIP_SETTLE);
        !self.loaded || self.catalog_loading || socket_settling
    }

    /// Whether the model chip still waits on its first answer.
    fn model_pending(&self) -> bool {
        let known = self
            .pi_state
            .as_ref()
            .is_some_and(|state| state.model.is_some())
            || self.picked_model.is_some()
            || self.transcript.model.is_some();
        !known && self.chips_loading()
    }

    /// Whether the thinking chip still waits on its first answer.
    fn thinking_pending(&self) -> bool {
        let known = self
            .pi_state
            .as_ref()
            .is_some_and(|state| state.thinking.is_some())
            || self.picked_thinking.is_some()
            || self.transcript.thinking_level.is_some();
        !known && self.chips_loading()
    }

    /// A picker chip's loading face: its glyph and a pulsing bar where the
    /// value will land, at the chip's own height.
    fn chip_skeleton(
        &self,
        id: &'static str,
        glyph: &'static [u8],
        width: f32,
        fonts: Fonts,
    ) -> Div {
        let theme = self.theme;
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .h(px(super::PICKER_CHIP_H))
            .px_2()
            .child(super::icon(glyph, 14.0, theme.muted))
            .child(style::skeleton(id, theme, px(width), fonts.secondary))
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
        let current = self.current_model_slug();
        if current.is_empty() {
            return "model".to_string();
        }
        for model in &self.catalog {
            if model_slug(model) == current && !model.name.is_empty() {
                return model.name.clone();
            }
        }
        current
    }

    /// The `provider/id` slug the model picker marks current: the
    /// extension's report when it is up, the just-made pick while its
    /// confirmation is still travelling, the transcript fold while the
    /// socket is down, else the catalog's own default so a fresh pane still
    /// names the model pi will run.
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
        if let Some(model) = self.picked_model.clone().filter(|model| !model.is_empty()) {
            return model;
        }
        if let Some(model) = self
            .transcript
            .model
            .clone()
            .filter(|model| !model.is_empty())
        {
            return model;
        }
        self.catalog_default().map(model_slug).unwrap_or_default()
    }

    /// The thinking level the picker marks current: the extension's report
    /// when it is up, the just-made pick while its confirmation is still
    /// travelling, the transcript fold while the socket is down, else the
    /// catalog default so a fresh pane still names the level pi will run.
    fn current_thinking(&self) -> Option<String> {
        self.pi_state
            .as_ref()
            .and_then(|state| state.thinking.clone())
            .or_else(|| self.picked_thinking.clone())
            .or_else(|| self.transcript.thinking_level.clone())
            .or_else(|| {
                self.catalog_default()
                    .and_then(|model| model.default_effort.as_deref())
                    .map(pilus_client::thinking_level)
            })
    }

    /// The model the catalog marks default, else its first row. The remote
    /// catalog reads pi's own settings, so it names the model a fresh session
    /// opens on even before the extension socket reports one.
    fn catalog_default(&self) -> Option<&AgentModel> {
        self.catalog
            .iter()
            .find(|model| model.is_default)
            .or_else(|| self.catalog.first())
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
                this.session.toggle_picker(kind, cx);
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
    /// The list is virtualized: pi's catalog runs to hundreds of models,
    /// so painting every row would cost more than the transcript does. Rows
    /// are a fixed height and the menu an exact pixel height, so the
    /// anchored offset opens it upward without shifting the composer.
    fn picker_list(
        &self,
        kind: PickerKind,
        fonts: Fonts,
        cx: &mut Context<PilusView>,
    ) -> AnyElement {
        let theme = self.theme;
        if self.catalog_loading {
            return self.picker_skeleton(fonts);
        }
        let (options, matches) = self.picker_rows(&self.picker_filter(cx));
        let cursor = self.picker_cursor.min(matches.len().saturating_sub(1));
        let filtered = self.picker_filtered();
        // One row even when the filter keeps nothing, so the placeholder
        // still measures right.
        let rows = matches.len().clamp(1, picker_visible_rows(filtered));
        let list_h = super::picker_menu_h(rows, filtered);
        let mut list = super::picker_menu("pilus-session-picker", theme)
            .max_h(px(list_h))
            .on_mouse_down_out(cx.listener(|this: &mut PilusView, _, _, cx| {
                this.session.dismiss_picker(cx);
                cx.notify();
            }));
        if filtered {
            let count = format!("{} of {}", matches.len(), options.len());
            let Some(field) = self.picker_field.clone() else {
                return self.anchored_picker(list, list_h);
            };
            field.update(cx, |field, _| {
                field.set_style(
                    theme,
                    style::chrome_font(&self.settings),
                    px(fonts.secondary),
                );
            });
            list = list.child(super::picker_filter_field(theme, fonts.label, count, field));
        }
        if matches.is_empty() {
            let empty = if options.is_empty() {
                "No models available"
            } else {
                "No models match"
            };
            return self.anchored_picker(
                list.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .h(px(super::PICKER_ROW_H))
                        .px_3()
                        .text_size(px(fonts.body))
                        .text_color(theme.muted)
                        .child(empty),
                ),
                list_h,
            );
        }
        let view = cx.weak_entity();
        let body_h = super::picker_body_h(rows);
        list = list.child(
            uniform_list(
                "session-picker-list",
                matches.len(),
                move |range, _window, _cx| {
                    range
                        .map(|row| {
                            let option = &options[matches[row]];
                            let argument = option.value.clone();
                            let hint = option.hint.clone();
                            let label = option.label.clone();
                            let active = row == cursor;
                            let view = view.clone();
                            super::picker_option_row(
                                ("session-picker-option", row),
                                label,
                                hint,
                                active || option.current,
                                theme,
                                fonts.body,
                                // A choice goes to the live agent as a
                                // slash command and surfaces in the
                                // transcript when pi records the change.
                                move |_, _, cx| {
                                    // A dropped view means the window is
                                    // closing under the overlay.
                                    let _ = view.update(cx, |this: &mut PilusView, cx| {
                                        this.session.choose_picker(kind, argument.clone(), cx);
                                        cx.notify();
                                    });
                                },
                            )
                            .into_any_element()
                        })
                        .collect()
                },
            )
            .h(px(body_h))
            .w_full()
            .track_scroll(&self.picker_scroll),
        );
        self.anchored_picker(list, list_h)
    }

    /// The picker menu while the catalog loads: skeleton bars in place of
    /// the option rows, so the menu reserves the space the real rows will
    /// take and the panel never flashes an empty list.
    fn picker_skeleton(&self, fonts: Fonts) -> AnyElement {
        let theme = self.theme;
        let rows = PICKER_SKELETON_ROWS;
        let list_h = super::picker_menu_h(rows, false);
        let mut list = super::picker_menu("pilus-session-picker", theme).max_h(px(list_h));
        for row in 0..rows {
            list = list.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .w_full()
                    .h(px(super::PICKER_ROW_H))
                    .px_3()
                    .child(style::skeleton(
                        ("session-picker-skeleton", row),
                        theme,
                        relative(0.72),
                        fonts.body,
                    )),
            );
        }
        self.anchored_picker(list, list_h)
    }

    /// Float a picker menu above its chip: the anchored overlay offsets by
    /// the menu's exact height so opening it never moves the composer.
    fn anchored_picker(&self, menu: Stateful<Div>, height: f32) -> AnyElement {
        deferred(
            anchored()
                .anchor(Anchor::TopLeft)
                .offset(point(px(0.0), px(-(height + 6.0))))
                .snap_to_window_with_margin(px(8.0))
                .child(style::enter("session-picker-enter", menu)),
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
        let header = self.header(fonts, cx);
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

/// What the reader should tail: an explicit file (`--session-ui` only), a
/// reported session id, or nothing yet — a pane open then idles on the
/// switch channel until a follow lands instead of guessing a file.
struct ReadSpec {
    /// An explicit session file; bypasses discovery entirely.
    explicit: Option<PathBuf>,
    /// The daemon's seed for which session this pane is in; the socket
    /// may follow onto a newer one.
    session_id: Option<String>,
    /// `--session-ui` only: no id resolves the cwd's newest session file.
    /// Pane opens never guess.
    resolve_latest: bool,
    /// The argument `--session-ui` was given, named in a "no session"
    /// message. Empty for a pane open.
    display: String,
}

/// The background reader: resolve the session, then tail it until stopped
/// or retargeted.
///
/// A reported session id can precede its file — pi creates the file lazily,
/// at the first user message — so a resolution miss is retried on the poll
/// interval rather than raised as an error. Only real IO and SSH failures
/// surface.
///
/// `switch_rx` retargets the tail when the extension socket reports the
/// pane moved to another session ("/new", "/resume"): the old tail parks
/// under its cache key and the new id resolves through the same "not yet"
/// retry, since its file may not exist either.
/// A tail the reader just opened.
///
/// `hint` is the cached remote path the source was opened with: a guess
/// that skips the resolve round-trip. If the source behind it fails, the
/// reader drops the hint file and resolves for real rather than erroring
/// on a stale cache entry.
struct OpenedTail {
    tail: Tail,
    /// The parked-tail cache key, the source's label.
    key: String,
    hint: Option<PathBuf>,
}

fn reader_loop(
    endpoint: Endpoint,
    spec: ReadSpec,
    tag: u64,
    cache: SessionCache,
    tx: Sender<Update>,
    switch_rx: Receiver<(u64, String)>,
    stop: Arc<AtomicBool>,
) {
    let interval = match endpoint {
        Endpoint::Local { .. } => LOCAL_POLL,
        Endpoint::Remote { .. } => REMOTE_POLL,
    };
    let mut wanted = spec.session_id;
    let mut tag = tag;
    let mut opened: Option<OpenedTail> = None;
    // Whether this session's "no file yet" has already been reported, so a
    // fresh pane idles without re-sending the same empty note every poll.
    let mut announced_empty = false;
    // A `false` send means the window went away: exit and park. An error
    // exits directly with nothing parked — a tail that failed is not to
    // be trusted again.
    let mut park_and_exit = false;
    loop {
        // Take the newest retarget queued since the last pass; the tail it
        // replaces parks under its own key so a switch back catches up.
        while let Ok((next_tag, next_id)) = switch_rx.try_recv() {
            if wanted.as_deref() != Some(next_id.as_str()) {
                if let Some(opened) = opened.take() {
                    cache.put(&opened.key, opened.tail);
                }
                wanted = Some(next_id);
                tag = next_tag;
                announced_empty = false;
            }
        }
        if stop.load(Ordering::Relaxed) {
            park_and_exit = true;
        }
        if park_and_exit {
            break;
        }
        let mut stale_hint = false;
        match &mut opened {
            Some(opened) => {
                match opened.tail.poll() {
                    Ok(true) => {
                        // A metadata-only fold marks nothing, so `from`
                        // lands at the end and the delta carries `meta`
                        // alone.
                        let from = opened
                            .tail
                            .take_changed_from()
                            .unwrap_or_else(|| opened.tail.transcript().items.len());
                        if !send_delta(&tx, &opened.tail, from, tag) {
                            park_and_exit = true;
                        }
                    }
                    Ok(false) => {}
                    Err(error) => {
                        // A tail opened from a cached path failed on a
                        // stale guess, not a real error: forget the hint,
                        // drop the tail, and let the next pass resolve
                        // for real.
                        if let Some(hint) = opened.hint.take() {
                            let _ = std::fs::remove_file(hint);
                            stale_hint = true;
                        } else {
                            let _ = tx.send(Update::Error(error.to_string()));
                            return;
                        }
                    }
                }
                if !park_and_exit && sleep_sliced(interval, &stop) {
                    continue;
                }
            }
            None => {
                // A pane with no reported id has nothing to open yet; an
                // explicit file and a `--session-ui` cwd are targets even
                // without one.
                if wanted.is_none() && !spec.resolve_latest && spec.explicit.is_none() {
                    // No session yet and guessing is off the table: wait
                    // for a follow or the stop flag.
                    if sleep_sliced(interval, &stop) {
                        continue;
                    }
                    continue;
                }
                match open_tail(&endpoint, spec.explicit.clone(), wanted.as_deref(), &cache) {
                    Ok(Some(mut fresh)) => {
                        // Send the initial fold immediately so the panel is
                        // not blank for a tick.
                        if !send_delta(&tx, &fresh.tail, 0, tag) {
                            cache.put(&fresh.key, fresh.tail);
                            return;
                        }
                        // A reused tail's catch-up poll may have marked an
                        // index that the full snapshot already covers.
                        fresh.tail.take_changed_from();
                        opened = Some(fresh);
                    }
                    Ok(None) => {
                        // `--session-ui` names an existing session, so a miss
                        // is the argument the user passed, not a fresh pane
                        // waiting on its first prompt: report it instead of
                        // idling on an empty placeholder.
                        if spec.resolve_latest {
                            let _ = tx.send(Update::Error(format!(
                                "No Pi session found for `{}`",
                                spec.display
                            )));
                            return;
                        }
                        // The session is known but pi has not written its file
                        // yet: a fresh pane, not a load in flight. Tell the
                        // panel once so its header drops the skeleton.
                        if !announced_empty {
                            announced_empty = true;
                            if tx.send(Update::Empty).is_err() {
                                park_and_exit = true;
                            }
                        }
                        if sleep_sliced(interval, &stop) {
                            continue;
                        }
                    }
                    Err(error) => {
                        let _ = tx.send(Update::Error(error.to_string()));
                        return;
                    }
                }
            }
        }
        if stale_hint {
            opened = None;
            continue;
        }
    }
    // The panel moved on; park the tail so a switch back reads only the
    // bytes appended in between.
    if let Some(opened) = opened.take() {
        cache.put(&opened.key, opened.tail);
    }
}

/// Push one delta: every item at or after `from`, plus the fold's metadata.
/// Returns false when the window has gone away.
fn send_delta(tx: &Sender<Update>, tail: &Tail, from: usize, tag: u64) -> bool {
    let transcript = tail.transcript();
    let from = from.min(transcript.items.len());
    tx.send(Update::Delta(Box::new(Delta {
        epoch: tag,
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
        RpcCommand::New => RequestKind::New,
        RpcCommand::SetModel { .. } => RequestKind::SetModel,
        RpcCommand::SetThinking { .. } => RequestKind::SetThinking,
        RpcCommand::Models => RequestKind::Models,
        RpcCommand::State => RequestKind::State,
    }
}

/// Connect to the extension socket at `path` (a temp path in tests).
fn connect_path(path: &Path) -> Result<RpcConn, String> {
    let stream = UnixStream::connect(path).map_err(|error| error.to_string())?;
    let reader = stream.try_clone().map_err(|error| error.to_string())?;
    Ok(RpcConn {
        lines: BufReader::new(Box::new(reader)),
        writer: Box::new(stream),
        child: None,
    })
}

/// The sockets this pane might answer on: sidecars naming the pane first
/// — the newest wins a `/new` race, since a crashed pi can leave its
/// sidecar behind — then the last-known session's socket for extensions
/// old enough to write no `pane` key.
fn socket_candidates(agent_dir: &Path, pane: &str, current: &str) -> Vec<PathBuf> {
    let mut keyed: Vec<(Option<std::time::SystemTime>, PathBuf)> = Vec::new();
    if safe_session_id(pane)
        && let Ok(entries) = std::fs::read_dir(agent_dir.join("pilus"))
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Some(sidecar) = Sidecar::parse(&text) else {
                continue;
            };
            if sidecar.pane.as_deref() != Some(pane) {
                continue;
            }
            let modified = entry.metadata().and_then(|meta| meta.modified()).ok();
            keyed.push((modified, PathBuf::from(sidecar.socket)));
        }
    }
    keyed.sort_by_key(|candidate| std::cmp::Reverse(candidate.0));
    let mut paths: Vec<PathBuf> = keyed.into_iter().map(|(_, path)| path).collect();
    if !current.is_empty() {
        paths.push(socket_path(agent_dir, current));
    }
    paths
}

/// Connect to the pane's extension socket: every candidate gets a try
/// before the loop backs off, so one stale sidecar cannot hide a live one.
fn connect_local(agent_dir: &Path, pane: &str, current: &str) -> Result<RpcConn, String> {
    for path in socket_candidates(agent_dir, pane, current) {
        if let Ok(conn) = connect_path(&path) {
            return Ok(conn);
        }
    }
    Err("no extension socket".to_string())
}

/// Reach the extension socket on a machine: the sidecar naming this pane
/// wins, then the last-known session id's file — the same pane-keyed
/// discovery as the local loop, folded into one SSH round trip.
fn connect_remote(target: &SshTarget, pane: &str, current: &str) -> Result<RpcConn, String> {
    // Interpolation is only safe inside the daemon's id alphabet; a
    // stranger pane or id simply runs no keyed scan.
    let keyed = if safe_session_id(pane) {
        format!(
            "for f in $(ls -t \"$dir\"/*.json 2>/dev/null); do \
             grep -Fq '\"pane\":\"{pane}\"' \"$f\" && {{ cat \"$f\"; exit 0; }}; \
             done"
        )
    } else {
        String::new()
    };
    let fallback = if !current.is_empty() && safe_session_id(current) {
        format!("cat \"$dir/{current}.json\" 2>/dev/null")
    } else {
        String::new()
    };
    let output = target
        .shell(&format!(
            "dir=\"${{PI_CODING_AGENT_DIR:-$HOME/.pi/agent}}/pilus\"; \
             [ -d \"$dir\" ] || exit 1; {keyed} {fallback}"
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

/// The extension channel's owner: find this pane's socket, run one
/// session, and rediscover on failure. The daemon's report is only the
/// seed — the sidecar's `pane` key lets the loop follow the pane onto a
/// new session ("/new", "/resume") without waiting on a re-report, and a
/// missing socket just means pi has not loaded the extension yet — the
/// file tail keeps the panel honest meanwhile.
fn rpc_loop(
    endpoint: Endpoint,
    pane: String,
    session_id: Option<String>,
    update: Sender<Update>,
    commands: Receiver<RpcCommand>,
    stop: Arc<AtomicBool>,
) {
    // One receiver survives every reconnect; the Mutex only exists because
    // `Receiver` is not `Sync`, never because two threads compete for it.
    let commands = Mutex::new(commands);
    // The last session the socket hello'd; empty until one is known. The
    // un-keyed fallback path still needs somewhere plausible to point.
    let mut current = session_id.unwrap_or_default();
    while !stop.load(Ordering::Relaxed) {
        let conn = match &endpoint {
            Endpoint::Local { .. } => connect_local(&agent_dir(), &pane, &current),
            Endpoint::Remote { target, .. } => connect_remote(target, &pane, &current),
        };
        match conn {
            Ok(conn) => rpc_session(conn, &update, &commands, &stop, &mut current),
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
/// way pi sends them. `current` is the last session the hello named — the
/// loop mutates it so the next reconnect keeps pointing where pi is.
fn rpc_session(
    conn: RpcConn,
    update: &Sender<Update>,
    commands: &Mutex<Receiver<RpcCommand>>,
    stop: &AtomicBool,
    current: &mut String,
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
                Incoming::Event(RpcEvent::Hello { session_id, .. }) => {
                    up = true;
                    // The pane moved sessions while we were away; the
                    // socket is the authority, so the reader retargets
                    // without a panel reopen.
                    if *current != session_id {
                        *current = session_id.clone();
                        if update.send(Update::SessionFollow { session_id }).is_err() {
                            break 'session;
                        }
                    }
                    if update.send(Update::RpcUp).is_err() {
                        break 'session;
                    }
                    // Refill the picker's catalog from the live agent, and
                    // ask for its state outright: a fresh socket already
                    // pushes one, but the reply guarantees the pickers name
                    // the new session's own model and thinking even when a
                    // push is missed on a reconnect race.
                    next_id += 1;
                    let id = next_id.to_string();
                    if writeln!(writer, "{}", RpcCommand::Models.to_line(&id)).is_err() {
                        break 'session;
                    }
                    pending.insert(id, RequestKind::Models);
                    next_id += 1;
                    let id = next_id.to_string();
                    if writeln!(writer, "{}", RpcCommand::State.to_line(&id)).is_err() {
                        break 'session;
                    }
                    pending.insert(id, RequestKind::State);
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
) -> Result<Option<OpenedTail>, SessionError> {
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
                .map(|(tail, key)| {
                    Some(OpenedTail {
                        tail,
                        key,
                        hint: None,
                    })
                }),
                None => Ok(None),
            }
        }
        Endpoint::Remote { target, .. } => {
            let dir = remote_cache_dir(target);
            // An explicit path (a history choice) is already resolved, so
            // skip the glob; otherwise a session id also names the mirror,
            // so a re-open folds the cached copy before any SSH call — and
            // the cached path skips the resolve glob too.
            let (path, hint) = if let Some(path) = explicit {
                (path.to_string_lossy().into_owned(), None)
            } else {
                let hinted = session_id.and_then(|id| remote_path_hint(&dir, id));
                match hinted {
                    Some((path, hint)) => (path, Some(hint)),
                    None => {
                        let Some(path) = resolve_remote(target, session_id)? else {
                            return Ok(None);
                        };
                        let id = session_id
                            .map(str::to_string)
                            .or_else(|| session_id_from_path(&path));
                        if let Some(id) = id.as_deref() {
                            write_remote_path_hint(&dir, id, &path);
                        }
                        (path, None)
                    }
                }
            };
            // The mirror is keyed by the session id — resolved or the one
            // the file's own name carries — never by its path.
            let mirror = session_id
                .map(str::to_string)
                .or_else(|| session_id_from_path(&path))
                .and_then(|id| remote_mirror(&dir, &id));
            let tail_key = format!("{}:{path}", target.destination);
            open_or_reuse(cache, tail_key, || {
                let source: Box<dyn Source> = Box::new(SshSource {
                    target: target.clone(),
                    path: path.clone(),
                });
                match mirror {
                    Some(mirror) => Tail::with_source_and_mirror(source, mirror),
                    None => Tail::with_source(source),
                }
            })
            .map(|(tail, key)| Some(OpenedTail { tail, key, hint }))
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

/// The local mirror of a remote endpoint's sessions: one directory per
/// destination, so a re-opened session folds from disk before the first
/// SSH call instead of waiting on the whole file to cross the network.
fn remote_cache_dir(target: &SshTarget) -> PathBuf {
    let key: String = target
        .destination
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    pilus_client::pilus_config_dir()
        .join("session-cache")
        .join(key)
}

/// The mirror file for a session id, when the id is safe as a filename.
fn remote_mirror(dir: &Path, id: &str) -> Option<PathBuf> {
    safe_session_id(id).then(|| dir.join(format!("{id}.jsonl")))
}

/// The cached remote path for `id` and the file it came from, when one
/// was written by an earlier resolve. The file rides along so a stale
/// guess can be dropped by whoever trusted it.
fn remote_path_hint(dir: &Path, id: &str) -> Option<(String, PathBuf)> {
    let file = dir.join(format!("{}.path", sanitize_id(id)?));
    let path = std::fs::read_to_string(&file).ok()?.trim().to_string();
    (!path.is_empty()).then_some((path, file))
}

/// Record where `id` resolves on the remote, so the next open skips the
/// directory glob.
fn write_remote_path_hint(dir: &Path, id: &str, path: &str) {
    let Some(id) = sanitize_id(id) else {
        return;
    };
    let _ = std::fs::create_dir_all(dir)
        .and_then(|()| std::fs::write(dir.join(format!("{id}.path")), path));
}

/// `id` when it is safe as a cache filename — the same charset pi's own
/// session ids use.
fn sanitize_id(id: &str) -> Option<String> {
    safe_session_id(id).then(|| id.to_string())
}

/// The session id a pi session file's name carries: `<timestamp>_<id>`,
/// so the id is everything after the first `_`.
fn session_id_from_path(path: &str) -> Option<String> {
    let stem = Path::new(path).file_stem()?.to_str()?;
    sanitize_id(stem.split_once('_')?.1)
}

/// A compact header icon button: a bordered hover fill under a muted glyph.
fn header_icon_button(id: &'static str, glyph: &'static [u8], theme: Theme) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .w(px(28.0))
        .h(px(28.0))
        .flex_shrink_0()
        .radius_md()
        .cursor_pointer()
        .hover_fill(theme.border, style::RADIUS_MD)
        .child(super::icon(glyph, 15.0, theme.muted))
}

/// When a session started, from the timestamp pi puts at the front of its
/// file name (`2026-10-06T03-02-28-378Z_<id>.jsonl`), as `2026-10-06 03:02`.
fn session_started(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let (date, time) = name.split_once('T')?;
    let mut clock = time.split('-');
    let (hour, minute) = (clock.next()?, clock.next()?);
    let numeric =
        |part: &str, len: usize| part.len() == len && part.bytes().all(|b| b.is_ascii_digit());
    (date.len() == 10 && numeric(hour, 2) && numeric(minute, 2))
        .then(|| format!("{date} {hour}:{minute}"))
}

/// Fold the remote history sample into entries.
///
/// The command emits a `@@PILUS <path>` marker before each sampled file; a
/// file whose name carries no id is skipped rather than shown as a row with
/// no way to open it.
fn parse_history_output(output: &str) -> Vec<HistoryEntry> {
    fn finish(current: &mut Option<String>, body: &mut String, entries: &mut Vec<HistoryEntry>) {
        if let Some(path) = current.take()
            && let Some(id) = session_id_from_path(&path)
        {
            entries.push(HistoryEntry {
                path: PathBuf::from(path),
                id,
                title: pilus_session::first_prompt_in(body).unwrap_or_default(),
            });
        }
        body.clear();
    }
    let mut entries = Vec::new();
    let mut current: Option<String> = None;
    let mut body = String::new();
    for line in output.lines() {
        match line.strip_prefix("@@PILUS ") {
            Some(path) => {
                finish(&mut current, &mut body, &mut entries);
                current = Some(path.trim().to_string());
            }
            None if current.is_some() => {
                body.push_str(line);
                body.push('\n');
            }
            None => {}
        }
    }
    finish(&mut current, &mut body, &mut entries);
    entries
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

/// A stable name for a section toggle, for element ids.
fn toggle_key(toggle: Toggle) -> String {
    match toggle {
        Toggle::Item(index) => format!("item-{index}"),
        Toggle::Group(index) => format!("group-{index}"),
        Toggle::Work(index) => format!("work-{index}"),
    }
}

/// Whether an item is tool output: what pi's Ctrl+O folds.
fn is_tool_output(kind: &ItemKind) -> bool {
    matches!(
        kind,
        ItemKind::ToolCall { .. } | ItemKind::ToolResult { .. } | ItemKind::Bash { .. }
    )
}

/// The value `step` rows away from the current option, wrapping at either
/// end. With nothing marked current the cycle enters at the near end.
/// `None` when there is no other value to move to.
fn cycle_value(options: &[PickerOption], step: isize) -> Option<String> {
    let len = options.len() as isize;
    let current = options.iter().position(|option| option.current);
    if len == 0 || (len == 1 && current.is_some()) {
        return None;
    }
    let next = match current {
        Some(at) => (at as isize + step).rem_euclid(len),
        None if step < 0 => len - 1,
        None => 0,
    };
    options
        .get(next as usize)
        .map(|option| option.value.clone())
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
        headline: fonts.headline,
        title: fonts.title,
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

/// Resolve a cwd spec to the absolute path pi keys its session folder by.
/// Pi names the folder from the process cwd, so a relative `--session-ui .`
/// must be made absolute before it can match.
fn local_cwd(path: &str) -> String {
    std::fs::canonicalize(path)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| expand_local(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilus_session::PiModel;

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

    #[test]
    fn session_id_from_path_reads_the_name_suffix() {
        assert_eq!(
            session_id_from_path(
                "/home/u/.pi/agent/sessions/--repo--/2026-08-26T18-07-57-407Z_01a03f41-e1df-7a41-b4a8-81033186eeda.jsonl"
            ),
            Some("01a03f41-e1df-7a41-b4a8-81033186eeda".to_string())
        );
        // An id holding `_` keeps its whole suffix.
        assert_eq!(
            session_id_from_path("/tmp/2026_t_a-b-c.jsonl"),
            Some("t_a-b-c".to_string())
        );
        assert_eq!(session_id_from_path("/tmp/nounderscore.jsonl"), None);
        assert_eq!(session_id_from_path("/tmp/dir/"), None);
    }

    #[test]
    fn history_output_folds_markers_into_entries() {
        let output = "\n@@PILUS /s/--app--/t_one.jsonl\n\
            {\"type\":\"session\",\"id\":\"one\"}\n\
            {\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"fix   it\"}}\n\
            \n@@PILUS /s/--app--/t_two.jsonl\n\
            {\"type\":\"session\",\"id\":\"two\"}\n";
        let entries = parse_history_output(output);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "one");
        assert_eq!(entries[0].title, "fix it");
        assert_eq!(entries[1].id, "two");
        assert!(entries[1].title.is_empty());
        // A file whose name carries no id is skipped, not shown unopenable.
        assert!(parse_history_output("@@PILUS /s/nounderscore.jsonl\n{}").is_empty());
    }

    #[test]
    fn history_entry_label_falls_back_to_the_start_time() {
        let titled = HistoryEntry {
            path: PathBuf::from("/s/t_a.jsonl"),
            id: "a".into(),
            title: "Fix the parser".into(),
        };
        assert_eq!(titled.label(), "Fix the parser");
        let untitled = HistoryEntry {
            path: PathBuf::from("/s/2026-10-06T03-02-28-378Z_b.jsonl"),
            id: "b".into(),
            title: String::new(),
        };
        assert_eq!(untitled.label(), "Untitled · 2026-10-06 03:02");
        // A name with no timestamp still never shows the raw id.
        let bare = HistoryEntry {
            path: PathBuf::from("/s/t_c.jsonl"),
            id: "c".into(),
            title: String::new(),
        };
        assert_eq!(bare.label(), "Untitled session");
    }

    #[test]
    fn remote_path_hints_round_trip_and_reject_unsafe_ids() {
        let dir = std::env::temp_dir().join(format!("pilus-hints-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(remote_path_hint(&dir, "abc-123"), None);
        write_remote_path_hint(
            &dir,
            "abc-123",
            "/home/u/.pi/agent/sessions/--w--/t_abc-123.jsonl",
        );
        let Some((path, file)) = remote_path_hint(&dir, "abc-123") else {
            panic!("the written hint must read back");
        };
        assert_eq!(path, "/home/u/.pi/agent/sessions/--w--/t_abc-123.jsonl");
        assert!(file.exists());
        // An id that is not filename-safe never becomes a hint.
        write_remote_path_hint(&dir, "../escape", "/tmp/x.jsonl");
        assert_eq!(remote_path_hint(&dir, "../escape"), None);
        // The directory still holds only the one real hint.
        let files = std::fs::read_dir(&dir).expect("dir").count();
        assert_eq!(files, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn model(provider: &str, id: &str, name: &str) -> AgentModel {
        AgentModel {
            provider: provider.to_string(),
            id: id.to_string(),
            name: name.to_string(),
            ..AgentModel::default()
        }
    }

    #[test]
    fn picker_options_keep_every_catalog_model() {
        let catalog: Vec<AgentModel> = (0..50)
            .map(|ix| model("openrouter", &format!("m{ix}"), &format!("Model {ix}")))
            .collect();
        let options = picker_options(&catalog, &[], "openrouter/m7");
        // The picker truncates no longer: every catalog model is a row.
        assert_eq!(options.len(), 50);
        assert_eq!(options[7].value, "openrouter/m7");
        assert!(options[7].current);
        assert_eq!(options[7].hint.as_deref(), Some("openrouter"));
        assert_eq!(options[8].label, "Model 8");
    }

    #[test]
    fn picker_options_add_unknown_session_models_once() {
        let catalog = vec![model("pi", "known", "Known")];
        let session = vec!["pi/known".to_string(), "other/model".to_string()];
        let options = picker_options(&catalog, &session, "");
        // A model the catalog dropped stays selectable, and the one it
        // knows does not appear twice.
        assert_eq!(
            options.iter().map(|o| o.value.as_str()).collect::<Vec<_>>(),
            vec!["pi/known", "other/model"]
        );
        assert_eq!(options[1].hint, None);
    }

    #[test]
    fn the_thinking_picker_follows_the_models_efforts() {
        let mut panel = SessionPanel::default();
        let mut deepseek = model("hyper", "deepseek", "DeepSeek");
        deepseek.efforts = Some(vec!["Low".into(), "High".into(), "Extra High".into()]);
        let mut plain = model("pi", "plain", "Plain");
        plain.efforts = Some(Vec::new());
        panel.catalog = vec![deepseek, plain];
        panel.pi_state = Some(PiState {
            model: Some(PiModel {
                provider: "hyper".into(),
                id: "deepseek".into(),
                name: "DeepSeek".into(),
                ..PiModel::default()
            }),
            thinking: Some("high".into()),
            ..PiState::default()
        });
        let options = panel.thinking_options();
        assert_eq!(
            options.iter().map(|o| o.value.as_str()).collect::<Vec<_>>(),
            vec!["low", "high", "xhigh"]
        );
        assert_eq!(options[1].label, "High");
        assert!(options[1].current);

        // A non-reasoning model offers only `off`, as pi clamps it.
        panel.pi_state = Some(PiState {
            model: Some(PiModel {
                provider: "pi".into(),
                id: "plain".into(),
                name: "Plain".into(),
                ..PiModel::default()
            }),
            ..PiState::default()
        });
        assert_eq!(
            panel
                .thinking_options()
                .iter()
                .map(|o| o.value.as_str())
                .collect::<Vec<_>>(),
            vec!["off"]
        );

        // An unknown model falls back to pi's full set.
        panel.catalog.clear();
        panel.pi_state = Some(PiState {
            model: Some(PiModel {
                provider: "mystery".into(),
                id: "x".into(),
                name: "X".into(),
                ..PiModel::default()
            }),
            ..PiState::default()
        });
        assert_eq!(panel.thinking_options().len(), THINKING_LEVELS.len());
    }

    #[test]
    fn picker_filter_matches_label_and_slug() {
        let catalog = vec![
            model("openrouter", "gpt-5", "GPT 5"),
            model("pi", "muse", "Muse Spark"),
        ];
        let options = picker_options(&catalog, &[], "");
        assert_eq!(picker_matches(&options, ""), vec![0, 1]);
        assert_eq!(picker_matches(&options, "muse"), vec![1]);
        assert_eq!(picker_matches(&options, "openrouter/gpt"), vec![0]);
        assert!(picker_matches(&options, "nope").is_empty());
    }

    #[test]
    fn picker_cursor_wraps_in_both_directions() {
        assert_eq!(picker_cursor_after(0, 3, 1), 1);
        assert_eq!(picker_cursor_after(2, 3, 1), 0);
        assert_eq!(picker_cursor_after(0, 3, -1), 2);
        assert_eq!(picker_cursor_after(4, 0, 1), 0);
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
    fn ctrl_t_folds_every_thinking_block() {
        let mut panel = SessionPanel::default();
        grouped_transcript(&mut panel);
        let thinking = |kind: &ItemKind| matches!(kind, ItemKind::Thinking { .. });

        panel.toggle_all(thinking);
        assert_eq!(panel.expanded, HashSet::from([1, 2, 8]));

        // Every block already showing: the same key folds them all again.
        panel.toggle_all(thinking);
        assert!(panel.expanded.is_empty());
    }

    #[test]
    fn ctrl_o_opens_tool_output_behind_its_work_header() {
        let mut panel = SessionPanel::default();
        grouped_transcript(&mut panel);

        // Expanded items behind a collapsed work run are not showing, so the
        // key opens the run rather than folding what nobody can see.
        panel.expanded.extend([4, 5, 6]);
        panel.toggle_all(is_tool_output);
        assert!(panel.work_open.contains(&4));
        assert_eq!(panel.expanded, HashSet::from([4, 5, 6]));

        panel.toggle_all(is_tool_output);
        assert!(panel.expanded.is_empty());
    }

    #[test]
    fn cycling_wraps_and_enters_at_the_near_end() {
        let option = |value: &str, current: bool| PickerOption {
            label: value.to_string(),
            value: value.to_string(),
            hint: None,
            current,
        };
        let options = vec![
            option("low", false),
            option("high", true),
            option("xhigh", false),
        ];
        assert_eq!(cycle_value(&options, 1).as_deref(), Some("xhigh"));
        assert_eq!(cycle_value(&options, -1).as_deref(), Some("low"));

        let last = vec![option("low", false), option("high", true)];
        assert_eq!(cycle_value(&last, 1).as_deref(), Some("low"));

        let unmarked = vec![option("a", false), option("b", false)];
        assert_eq!(cycle_value(&unmarked, 1).as_deref(), Some("a"));
        assert_eq!(cycle_value(&unmarked, -1).as_deref(), Some("b"));

        // A non-reasoning model's lone `off` has nowhere to go.
        assert_eq!(cycle_value(&[option("off", true)], 1), None);
        assert_eq!(cycle_value(&[], 1), None);
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
        panel.jump_to(5, false);
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
    fn composer_falls_back_to_the_catalog_default() {
        let mut panel = SessionPanel::default();
        // A fresh pane with no extension socket and an empty transcript.
        assert_eq!(panel.current_model_slug(), "");
        assert_eq!(panel.current_thinking(), None);
        assert_eq!(panel.model_display(), "model");

        let mut preferred = model("opencode-go", "muse", "Muse Spark");
        preferred.is_default = true;
        preferred.default_effort = Some("Extra High".to_string());
        panel.catalog = vec![model("openrouter", "gpt", "GPT"), preferred];

        // The default names the chip and the picker's current row.
        assert_eq!(panel.current_model_slug(), "opencode-go/muse");
        assert_eq!(panel.model_display(), "Muse Spark");
        assert_eq!(panel.current_thinking().as_deref(), Some("xhigh"));
        panel.picker = Some(PickerKind::Thinking);
        let current = panel
            .picker_list_options()
            .into_iter()
            .find(|option| option.current)
            .map(|option| option.value);
        assert_eq!(current.as_deref(), Some("xhigh"));

        // The live report still wins over the catalog default.
        panel.pi_state = Some(PiState {
            model: Some(PiModel {
                provider: "openrouter".to_string(),
                id: "gpt".to_string(),
                name: "GPT".to_string(),
                ..PiModel::default()
            }),
            thinking: Some("low".to_string()),
            ..PiState::default()
        });
        assert_eq!(panel.current_model_slug(), "openrouter/gpt");
        assert_eq!(panel.model_display(), "GPT");
        assert_eq!(panel.current_thinking().as_deref(), Some("low"));
    }

    #[test]
    fn live_models_reply_keeps_the_catalog_default() {
        let mut panel = SessionPanel::default();
        let mut preferred = model("opencode-go", "muse", "Muse Spark");
        preferred.is_default = true;
        preferred.default_effort = Some("High".to_string());
        panel.catalog = vec![preferred];
        panel.rpc_result(
            RequestKind::Models,
            Ok(Some(serde_json::json!({
                "models": [
                    {"provider": "opencode-go", "id": "muse", "name": "Muse Spark"},
                    {"provider": "openrouter", "id": "gpt", "name": "GPT"}
                ]
            }))),
        );
        assert_eq!(panel.catalog.len(), 2);
        assert!(panel.catalog[0].is_default);
        assert_eq!(panel.catalog[0].default_effort.as_deref(), Some("High"));
        assert!(!panel.catalog[1].is_default);
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
    fn the_context_gauge_tracks_occupancy() {
        // pi reports `percent` on a 0–100 scale, not as a fraction.
        let usage = ContextUsage {
            tokens: Some(421_000),
            context_window: 1_000_000,
            percent: Some(42.1),
        };
        assert_eq!(ctx_percent(&usage), Some(42.1));
        assert!(ctx_visible(&usage));
        assert!(!ctx_warns(&usage));
        assert_eq!(ctx_counts(&usage), "421k/1M");
        assert!(ctx_warns(&ContextUsage {
            tokens: None,
            context_window: 1,
            percent: Some(82.0),
        }));
        // After a compaction pi re-counts: the readout still paints, with an
        // empty track and the counts, rather than vanishing.
        let uncounted = ContextUsage {
            tokens: None,
            context_window: 1_048_576,
            percent: None,
        };
        assert_eq!(ctx_percent(&uncounted), None);
        assert_eq!(ctx_counts(&uncounted), "–/1M");
        assert!(ctx_visible(&uncounted));

        // A fresh session counts zero, and the gauge still holds its place.
        assert!(ctx_visible(&ContextUsage {
            tokens: Some(0),
            context_window: 1_048_576,
            percent: Some(0.0),
        }));

        // Without pi's figure the gauge computes the same percentage from
        // the token count, and the warning follows it.
        let computed = ContextUsage {
            tokens: Some(900_000),
            context_window: 1_000_000,
            percent: None,
        };
        assert_eq!(ctx_percent(&computed), Some(90.0));
        assert!(ctx_warns(&computed));

        // An unknown window never divides into a full red gauge.
        assert_eq!(
            ctx_percent(&ContextUsage {
                tokens: Some(7_367),
                context_window: 0,
                percent: None,
            }),
            None
        );

        // The captured wire fixture: 7367 / 1048576 ≈ 0.7%.
        let fixture = ContextUsage {
            tokens: Some(7_367),
            context_window: 1_048_576,
            percent: Some(0.702_571_868_896_484_4),
        };
        assert!(!ctx_warns(&fixture));
        assert_eq!(ctx_counts(&fixture), "7.4k/1M");
    }

    #[test]
    fn the_gauge_falls_back_to_the_transcript_without_the_extension() {
        let mut panel = SessionPanel::default();
        panel.catalog.push(AgentModel {
            provider: "anthropic".to_string(),
            id: "sonnet".to_string(),
            name: "Sonnet".to_string(),
            context_window: Some(200_000),
            ..AgentModel::default()
        });
        panel.transcript.model = Some("anthropic/sonnet".to_string());
        panel.transcript.context_tokens = Some(50_000);
        let usage = panel.context_usage().expect("fallback usage");
        assert_eq!(ctx_percent(&usage), Some(25.0));
        assert_eq!(ctx_counts(&usage), "50k/200k");
        // After a compaction the counts are unknown but the window stays.
        panel.transcript.context_tokens = None;
        let usage = panel.context_usage().expect("fallback usage");
        assert_eq!(ctx_counts(&usage), "–/200k");
        // An uncatalogued model has no window, so nothing paints.
        panel.transcript.model = Some("other/unknown".to_string());
        assert!(panel.context_usage().is_none());
        // The extension's report outranks the fallback.
        panel.pi_state = Some(PiState {
            context: Some(ContextUsage {
                tokens: Some(1),
                context_window: 10,
                percent: Some(10.0),
            }),
            ..PiState::default()
        });
        assert_eq!(panel.context_usage().map(|c| c.context_window), Some(10));
    }

    #[test]
    fn the_header_keeps_the_first_prompt_short() {
        let mut panel = SessionPanel::default();
        panel.transcript.items.push(Item {
            id: "u".to_string(),
            timestamp: String::new(),
            kind: ItemKind::User {
                text: "word ".repeat(60),
                images: 0,
            },
        });
        let title = panel.first_prompt().expect("prompt");
        assert_eq!(title.chars().count(), TITLE_CHARS + 1);
        assert!(title.ends_with('…'));
    }

    #[test]
    fn only_a_remote_pane_shows_the_title_skeleton() {
        let mut panel = SessionPanel::default();
        assert!(!panel.is_remote());
        assert!(!panel.awaiting_title());
        panel.endpoint = Some(Endpoint::Local {
            api_socket: PathBuf::from("/tmp/pilus.sock"),
            cwd: "/w".to_string(),
        });
        assert!(!panel.awaiting_title());
        panel.endpoint = Some(Endpoint::Remote {
            target: SshTarget {
                destination: "box".to_string(),
                binary: "luvus".to_string(),
                session: None,
            },
            cwd: "/w".to_string(),
        });
        // A remote pane holds the skeleton only until its reader reports.
        assert!(panel.awaiting_title());
        panel.loaded = true;
        assert!(!panel.awaiting_title());
    }

    #[test]
    fn a_reader_fold_clears_the_remote_skeleton() {
        let mut panel = SessionPanel {
            endpoint: Some(Endpoint::Remote {
                target: SshTarget {
                    destination: "box".to_string(),
                    binary: "luvus".to_string(),
                    session: None,
                },
                cwd: "/w".to_string(),
            }),
            ..SessionPanel::default()
        };
        assert!(panel.awaiting_title());
        // An empty fold is still a report: the session is fresh, not loading.
        panel.apply_delta(0, Vec::new(), TranscriptMeta::default());
        assert!(!panel.awaiting_title());
        assert!(panel.first_prompt().is_none());
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
        // client's two hello refreshes (models, state) and hangs up.
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
            let models_id = value["id"].as_str().unwrap_or("1").to_string();
            let request = lines.next().expect("line").expect("read");
            let value: Value = serde_json::from_str(&request).expect("json");
            assert_eq!(value["cmd"], "state");
            let state_id = value["id"].as_str().unwrap_or("2").to_string();
            writer
                .write_all(
                    format!(
                        "{{\"type\":\"response\",\"id\":\"{models_id}\",\"ok\":true,\"data\":{{\"models\":[{{\"provider\":\"p\",\"id\":\"m\",\"name\":\"M\",\"contextWindow\":1,\"reasoning\":false}}]}}}}\n"
                    )
                    .as_bytes(),
                )
                .expect("reply");
            writer
                .write_all(
                    format!(
                        "{{\"type\":\"response\",\"id\":\"{state_id}\",\"ok\":true,\"data\":{{\"busy\":false}}}}\n"
                    )
                    .as_bytes(),
                )
                .expect("reply");
        });

        let (update_tx, update_rx) = std::sync::mpsc::channel::<Update>();
        let (_command_tx, command_rx) = std::sync::mpsc::channel::<RpcCommand>();
        let commands = Mutex::new(command_rx);
        let stop = Arc::new(AtomicBool::new(false));
        // No session is known yet, so the hello's id lands as a follow.
        let mut current = String::new();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let conn = connect_path(&path).expect("connect");
                rpc_session(conn, &update_tx, &commands, &stop, &mut current);
            });
        });
        server.join().expect("server");

        let mut updates = Vec::new();
        while let Ok(update) = update_rx.try_recv() {
            updates.push(update);
        }
        assert_eq!(updates.len(), 6);
        assert!(matches!(
            &updates[0],
            Update::SessionFollow { session_id } if session_id == "s"
        ));
        assert!(matches!(updates[1], Update::RpcUp));
        assert!(matches!(updates[2], Update::RpcState(_)));
        match &updates[3] {
            Update::RpcResult {
                kind: RequestKind::Models,
                result: Ok(Some(_)),
            } => {}
            _ => panic!("expected a models result"),
        }
        match &updates[4] {
            Update::RpcResult {
                kind: RequestKind::State,
                result: Ok(Some(_)),
            } => {}
            _ => panic!("expected a state result"),
        }
        assert!(matches!(updates[5], Update::RpcDown));
        assert_eq!(current, "s");

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
                epoch: 0,
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

    #[test]
    fn socket_candidates_prefers_the_panes_newest_sidecar() {
        let root = std::env::temp_dir().join(format!("pilus-candidates-{}", std::process::id()));
        let dir = root.join("pilus");
        std::fs::create_dir_all(&dir).expect("dir");
        let write = |name: &str, pane: Option<&str>, socket: &str| {
            let keyed = pane
                .map(|p| format!("\"pane\":\"{p}\","))
                .unwrap_or_default();
            let path = dir.join(format!("{name}.json"));
            std::fs::write(
                &path,
                format!(
                    "{{\"protocol\":1,\"pid\":1,\"socket\":\"{socket}\",\"execPath\":\"/n\",{keyed}\"sessionId\":\"{name}\"}}"
                ),
            )
            .expect("sidecar");
            path
        };
        write("other", Some("9"), "/tmp/other.sock");
        write("unkeyed", None, "/tmp/unkeyed.sock");
        let older = write("ours", Some("86"), "/tmp/ours.sock");
        let _newer = write("ours-new", Some("86"), "/tmp/ours-new.sock");
        // Force the mtime race a `/new` creates: the newest sidecar wins.
        let old = std::time::SystemTime::now() - Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&older)
            .expect("open older")
            .set_modified(old)
            .expect("mtime");

        let paths = socket_candidates(&root, "86", "stale-id");
        assert_eq!(paths[0], PathBuf::from("/tmp/ours-new.sock"));
        assert_eq!(paths[1], PathBuf::from("/tmp/ours.sock"));
        // The last-known session's socket is the un-keyed last resort.
        assert_eq!(
            paths.last(),
            Some(&PathBuf::from(root.join("pilus/stale-id.sock")))
        );
        // Another pane's socket and an un-keyed sidecar never leak in.
        assert!(!paths.iter().any(|p| p.ends_with("other.sock")));
        assert!(!paths.iter().any(|p| p.ends_with("unkeyed.sock")));

        // With nothing keyed to the pane, the seed id still resolves.
        let paths = socket_candidates(&root, "404", "seed");
        assert_eq!(paths, vec![root.join("pilus/seed.sock")]);
        // And an unsafe pane runs no scan at all.
        let paths = socket_candidates(&root, "../x", "seed");
        assert_eq!(paths, vec![root.join("pilus/seed.sock")]);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_session_follow_swaps_transcripts_and_tells_the_reader() {
        let mut panel = SessionPanel::default();
        panel.open = true;
        panel.reported = Some("s1".to_string());
        panel.expand_key = Some("s1".to_string());
        panel.expanded.insert(0);
        // The outgoing session's live state must not leak into the new one:
        // the pickers fall back to the catalog default until the fresh
        // `state` push lands.
        panel.pi_state = Some(PiState {
            busy: true,
            thinking: Some("high".to_string()),
            ..PiState::default()
        });
        panel.picked_model = Some("other/flex".to_string());
        panel.stopped_at = Some(Instant::now());
        panel.settled_through = 1;
        push(
            &mut panel,
            ItemKind::User {
                text: "old".to_string(),
                images: 0,
            },
        );
        let (switch_tx, switch_rx) = std::sync::mpsc::channel::<(u64, String)>();
        panel.switch_tx = Some(switch_tx);
        let epoch = panel.epoch;

        panel.follow_session("s2".to_string());

        assert_eq!(panel.reported.as_deref(), Some("s2"));
        assert_eq!(panel.epoch, epoch + 1);
        assert!(panel.transcript.items.is_empty());
        assert!(panel.expanded.is_empty());
        assert!(panel.pi_state.is_none());
        assert!(panel.picked_model.is_none());
        assert!(panel.stopped_at.is_none());
        assert_eq!(panel.settled_through, 0);
        assert_eq!(panel.expand_key.as_deref(), Some("s2"));
        // The outgoing expansion parked under its own id.
        assert!(
            panel
                .expand_cache
                .get("s1")
                .is_some_and(|state| state.expanded.contains(&0))
        );
        assert_eq!(switch_rx.try_recv(), Ok((panel.epoch, "s2".to_string())));

        // A repeat hello or a malformed id is ignored.
        panel.follow_session("s2".to_string());
        panel.follow_session("../evil".to_string());
        assert_eq!(panel.reported.as_deref(), Some("s2"));
        assert!(switch_rx.try_recv().is_err());
    }

    #[test]
    fn a_session_follow_fills_an_unreported_panel() {
        let mut panel = SessionPanel {
            open: true,
            ..SessionPanel::default()
        };
        let (switch_tx, switch_rx) = std::sync::mpsc::channel::<(u64, String)>();
        panel.switch_tx = Some(switch_tx);

        panel.follow_session("s9".to_string());

        assert_eq!(panel.reported.as_deref(), Some("s9"));
        assert!(matches!(switch_rx.try_recv(), Ok((_, id)) if id == "s9"));
    }

    #[test]
    fn a_state_reply_refreshes_the_pickers() {
        let mut panel = SessionPanel::default();
        panel.rpc_result(
            RequestKind::State,
            Ok(Some(serde_json::json!({
                "busy": false,
                "thinking": "low",
                "model": {
                    "provider": "p",
                    "id": "m",
                    "name": "M",
                },
            }))),
        );
        let state = panel.pi_state.as_ref().expect("state");
        assert!(!state.busy);
        assert_eq!(state.thinking.as_deref(), Some("low"));
        assert_eq!(panel.current_model_slug(), "p/m");
        assert_eq!(panel.current_thinking().as_deref(), Some("low"));
    }

    #[test]
    fn an_abort_reply_clears_busy_and_names_the_failure() {
        let mut panel = SessionPanel::default();
        panel.pi_state = Some(PiState {
            busy: true,
            ..PiState::default()
        });
        panel.rpc_result(RequestKind::Abort, Ok(None));
        assert!(!panel.pi_state.as_ref().expect("state").busy);
        panel.pi_state = Some(PiState {
            busy: true,
            ..PiState::default()
        });
        panel.rpc_result(RequestKind::Abort, Err("denied".to_string()));
        assert_eq!(panel.status, "couldn't stop: denied");
        // The button stays up when the abort failed.
        assert!(panel.pi_state.as_ref().expect("state").busy);
    }

    #[test]
    fn stop_settles_the_trailing_work_and_receipts_above_the_composer() {
        let mut panel = SessionPanel::default();
        // Socket down, turn writing as it goes: the file's recency is what
        // marks it live, not the trailing tool call alone.
        panel.last_growth = Some(Instant::now());
        push(
            &mut panel,
            ItemKind::ToolCall {
                call_id: "c1".to_string(),
                name: "read".to_string(),
                arguments: Value::Null,
                outcome: None,
            },
        );
        assert!(panel.is_working());

        panel.mark_stopped();
        assert!(!panel.is_working(), "the button flips back to send");
        assert!(panel.show_stopped(), "the receipt takes the Working band");

        // The aborted turn keeps writing its remnant — more work, and the
        // file grows. It must not flip the composer back to `Working`.
        panel.last_growth = Some(Instant::now());
        push(
            &mut panel,
            ItemKind::ToolResult {
                call_id: "c1".to_string(),
                name: "read".to_string(),
                text: "partial".to_string(),
                images: 0,
                is_error: false,
            },
        );
        assert!(!panel.is_working(), "the remnant is not a running turn");
        assert!(
            panel.show_stopped(),
            "the receipt rides through the remnant"
        );

        // A new user turn is what ends the receipt: a prompt typed into the
        // pi TUI lands as a user item past the abort.
        let next = panel.transcript.items.len();
        panel.apply_delta(next, vec![user(next, "next")], TranscriptMeta::default());
        assert!(!panel.show_stopped(), "the new turn ends the receipt");
        assert!(!panel.is_working(), "a bare user turn has no work yet");

        // Our own send re-lights the composer at once.
        panel.set_optimistic("mine");
        assert!(panel.is_working());
    }

    #[test]
    fn opening_a_stopped_session_shows_send_not_stop() {
        let mut panel = SessionPanel::default();
        // A prior run was interrupted: the file ends mid-tool and has not
        // grown since, and the socket has not reported yet. The stale tail
        // must not paint `Working` or the stop square.
        push(
            &mut panel,
            ItemKind::ToolCall {
                call_id: "c1".to_string(),
                name: "read".to_string(),
                arguments: Value::Null,
                outcome: None,
            },
        );
        assert!(panel.last_growth.is_none());
        assert!(!panel.is_working(), "the button is send, not stop");
        assert!(
            !panel.show_stopped(),
            "no receipt for a session we did not stop"
        );

        // Once the file grows, the same tail reads as a live turn again.
        panel.last_growth = Some(Instant::now());
        assert!(panel.is_working());
    }

    #[test]
    fn the_stopped_receipt_rides_the_remnant_until_a_new_turn() {
        let mut panel = SessionPanel::default();
        panel.mark_stopped();
        assert!(panel.show_stopped());
        // The aborted turn's remnant lands and the file grows: still a
        // receipt, never `Working` again on the interrupted tail.
        panel.last_growth = Some(Instant::now());
        push(
            &mut panel,
            ItemKind::Thinking {
                text: "remnant".to_string(),
            },
        );
        assert!(panel.show_stopped());
        assert!(!panel.is_working());
        // A new turn ends it.
        panel.set_optimistic("next");
        assert!(!panel.show_stopped());
    }

    #[test]
    fn a_pick_shows_at_once_and_file_truth_confirms_it() {
        let mut panel = SessionPanel::default();
        // A fresh pane: no socket report, empty transcript, catalog default.
        let mut preferred = model("opencode-go", "muse", "Muse Spark");
        preferred.is_default = true;
        panel.catalog = vec![preferred];
        assert_eq!(panel.current_model_slug(), "opencode-go/muse");

        // The user picks while the socket is still reconnecting: the chips
        // name the pick even though nothing confirms it yet.
        panel.note_picked("/model other/flex");
        panel.note_picked("/thinking low");
        assert_eq!(panel.current_model_slug(), "other/flex");
        assert_eq!(panel.current_thinking().as_deref(), Some("low"));

        // The file echoing the pick adopts file truth.
        let mut meta = TranscriptMeta::default();
        meta.model = Some("other/flex".to_string());
        meta.thinking_level = Some("low".to_string());
        panel.apply_delta(0, vec![user(0, "hi")], meta);
        assert!(panel.picked_model.is_none());
        assert!(panel.picked_thinking.is_none());
        assert_eq!(panel.current_model_slug(), "other/flex");
        assert_eq!(panel.current_thinking().as_deref(), Some("low"));
    }

    #[test]
    fn a_fresh_session_names_the_next_step() {
        let mut panel = SessionPanel::default();
        // Closed: the placeholder never paints.
        assert!(!panel.shows_empty_state());

        // A pane with no reported session is a fresh start: the placeholder
        // paints at once instead of waiting on a read.
        panel.open = true;
        assert!(panel.shows_empty_state());

        // A reported session waits for the reader to confirm it is empty, so
        // an existing pane never flashes the placeholder before its fold.
        panel.reported = Some("s1".to_string());
        assert!(!panel.shows_empty_state());
        panel.loaded = true;
        assert!(panel.shows_empty_state());

        // An existing session has items, so the placeholder gives way.
        push(
            &mut panel,
            ItemKind::User {
                text: "hi".to_string(),
                images: 0,
            },
        );
        assert!(!panel.shows_empty_state());

        // A fresh session with the first prompt already sent shows the
        // optimistic bubble instead of the placeholder.
        let mut fresh = SessionPanel {
            open: true,
            loaded: true,
            ..SessionPanel::default()
        };
        fresh.pending_prompt = Some("first".to_string());
        assert!(!fresh.shows_empty_state());

        // A live turn is content too, even before the file catches up.
        let mut streaming = SessionPanel {
            open: true,
            loaded: true,
            ..SessionPanel::default()
        };
        streaming.live = Some(LiveTurn::default());
        assert!(!streaming.shows_empty_state());
    }

    #[test]
    fn live_session_and_rpc_live_gate_on_open() {
        let mut panel = SessionPanel::default();
        assert_eq!(panel.live_session(), None);
        panel.open = true;
        panel.reported = Some("s1".to_string());
        assert_eq!(panel.live_session().as_deref(), Some("s1"));
        assert!(!panel.rpc_live());
        panel.rpc_up = true;
        assert!(panel.rpc_live());
        panel.close();
        assert_eq!(panel.live_session(), None);
        assert!(!panel.rpc_live());
    }
}
