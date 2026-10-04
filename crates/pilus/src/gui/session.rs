//! The native Pi Session panel.
//!
//! Pi renders its conversation through a terminal, but persists it as JSONL.
//! This panel paints that conversation natively inside the main window — user
//! turns on the right, assistant turns on the left, and thinking, tool calls,
//! tool results, and shell runs as collapsed cards — folding the session with
//! `pilus-session` and following it live. A floating composer sends prompts
//! back to the live agent through the endpoint's control surface.
//!
//! The session is located through the daemon's UHP `agent.sessions` (which
//! knows the exact conversation a live agent is in, on any endpoint), then its
//! bytes are read through a [`Source`]: a local file, or an SSH `tail` for a
//! machine. All reads, lookups, and prompt sends happen on background threads,
//! so the GUI thread only ever clones an already-folded [`Transcript`].

use std::cell::RefCell;
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use gpui::{
    Anchor, AnyElement, Context, Div, ElementId, Entity, ScrollHandle, Stateful, anchored,
    deferred, div, point, prelude::*, px, relative,
};
use pilus_client::{AgentModel, ControlClient, SshTarget};
use pilus_session::{
    Error as SessionError, Item, ItemKind, Source, Tail, ToolOutcome, Transcript, Usage,
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
/// Items rendered on first paint; the rest fills in behind the scenes,
/// so a huge session opens fast.
const TAIL_WINDOW: usize = 50;
/// How many earlier items each background fill step reveals.
const FILL_CHUNK: usize = 200;
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

/// What the reader thread sends back to the window.
enum Update {
    /// A freshly folded transcript.
    Transcript(Box<Transcript>),
    /// The pi model catalog for the endpoint, for the model picker.
    Catalog(Vec<AgentModel>),
    /// A read or lookup failure, shown in place of the transcript.
    Error(String),
}

/// The Pi Session panel's state, embedded in [`PilusView`].
pub struct SessionPanel {
    open: bool,
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
    scroll: ScrollHandle,
    /// How many tail items to render; older ones lazy-load on demand.
    visible: usize,
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
    /// The tail is pinned: follow it on growth. Set when the viewport sits
    /// at the bottom; cleared the moment it moves without growth, which can
    /// only be the user scrolling or jumping away.
    following: bool,
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
    /// Item count at the last frame, so the view follows the tail.
    last_seen: usize,
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
            title: String::new(),
            error: String::new(),
            endpoint: None,
            target: None,
            sending: false,
            status: String::new(),
            theme: Theme::default(),
            settings: Rc::new(RefCell::new(Settings::default())),
            transcript: Transcript::default(),
            scroll: ScrollHandle::new(),
            visible: TAIL_WINDOW,
            catalog: Vec::new(),
            picker: None,
            picker_dismissed: None,
            send_clears_input: true,
            pending_prompt: None,
            pending_from: 0,
            prompts_open: false,
            prompts_dismissed: false,
            following: true,
            awaiting_response: false,
            pending_steering: false,
            last_growth: None,
            expanded: HashSet::new(),
            groups: HashSet::new(),
            work_open: HashSet::new(),
            last_seen: 0,
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
    /// The control socket is used only to find the exact session the live
    /// agent is in; a failure falls back to the newest file for the cwd.
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
            title,
            target,
            cx,
        );
    }

    /// Open the panel for a session on `target`, read over SSH.
    pub fn open_remote(
        &mut self,
        target: SshTarget,
        cwd: String,
        title: String,
        pane: Option<String>,
        cx: &mut Context<PilusView>,
    ) {
        self.open_with(Endpoint::Remote { target, cwd }, None, title, pane, cx);
    }

    /// Start a reader thread for `endpoint` and reset the panel's state.
    fn open_with(
        &mut self,
        endpoint: Endpoint,
        explicit: Option<PathBuf>,
        title: String,
        target: Option<String>,
        cx: &mut Context<PilusView>,
    ) {
        self.close();
        self.open = true;
        self.title = title.clone();
        self.error.clear();
        self.status.clear();
        self.sending = false;
        self.transcript = Transcript::default();
        self.expanded.clear();
        self.groups.clear();
        self.work_open.clear();
        self.visible = TAIL_WINDOW;
        self.catalog.clear();
        self.picker = None;
        self.picker_dismissed = None;
        self.pending_prompt = None;
        self.pending_from = 0;
        self.prompts_open = false;
        self.prompts_dismissed = false;
        self.outbox.clear();
        self.following = true;
        self.awaiting_response = false;
        self.pending_steering = false;
        self.last_growth = None;
        self.last_seen = 0;
        self.target = target;
        self.endpoint = Some(endpoint.clone());
        self.stop = Arc::new(AtomicBool::new(false));
        let stop = self.stop.clone();
        let tx = self.tx.clone();
        let catalog_endpoint = endpoint.clone();
        std::thread::spawn(move || reader_loop(endpoint, explicit, tx, stop));
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

    /// Close the panel and stop its reader thread.
    pub fn close(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.open = false;
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
                Update::Transcript(transcript) => {
                    // Past the initial delivery, anything arriving is file
                    // growth: the session is alive right now.
                    if self.last_seen > 0 {
                        self.last_growth = Some(Instant::now());
                    }
                    self.transcript = *transcript;
                    self.error.clear();
                }
                Update::Catalog(catalog) => {
                    self.catalog = catalog;
                }
                Update::Error(error) => self.error = error,
            }
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

    /// Handle a keystroke. Returns whether it was consumed.
    pub fn key(
        &mut self,
        key: &str,
        input: &Entity<TextInput>,
        cx: &mut Context<PilusView>,
    ) -> bool {
        match key {
            "escape" => {
                // Open menus eat the first Escape in topmost order; the next
                // one closes the panel.
                if self.picker.take().is_some() {
                    self.picker_dismissed = None;
                } else if self.prompts_open {
                    self.prompts_open = false;
                    self.prompts_dismissed = false;
                } else {
                    self.close();
                }
                true
            }
            "enter" => {
                self.submit(input, cx);
                true
            }
            "end" => {
                self.scroll.scroll_to_bottom();
                true
            }
            _ => false,
        }
    }

    /// Send the composer's text to the live agent.
    fn submit(&mut self, input: &Entity<TextInput>, cx: &mut Context<PilusView>) {
        let text = input.read(cx).value().trim().to_string();
        self.send_text(text, true, cx);
    }

    /// Send a picker command (`/model`, `/thinking`) without touching the
    /// composer's draft.
    fn send_command(&mut self, command: String, cx: &mut Context<PilusView>) {
        self.picker = None;
        self.picker_dismissed = None;
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
            self.scroll.scroll_to_bottom();
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
        // Keep the toggled row in view, so expanding or collapsing it never
        // reads as the row vanishing. Indices are absolute while the scroll
        // container holds a tail window, so translate past it.
        let start = self.window_start();
        self.scroll.scroll_to_item(index.saturating_sub(start));
    }

    /// Reveal one more chunk of earlier items, holding the scroll position
    /// on the first item that was already visible.
    fn grow_earlier(&mut self) {
        let len = self.transcript.items.len();
        let old_start = len.saturating_sub(self.visible);
        if old_start == 0 {
            return;
        }
        self.visible += FILL_CHUNK;
        let start = len.saturating_sub(self.visible);
        self.scroll.scroll_to_top_of_item(old_start - start);
    }

    /// Absolute index of the first rendered item.
    fn window_start(&self) -> usize {
        self.transcript.items.len().saturating_sub(self.visible)
    }

    /// Scroll one absolute item into view, first widening the tail window
    /// until it reaches that item.
    ///
    /// A prompt older than the window has no child to scroll to, so jumping
    /// to it used to land on the window's first row. Growing the window is
    /// what the background fill would have done anyway; doing it here makes
    /// the jump land where the menu promised.
    fn jump_to(&mut self, index: usize) {
        let len = self.transcript.items.len();
        if index >= len {
            return;
        }
        self.visible = self.visible.max(len - index);
        self.scroll.scroll_to_item(index - self.window_start());
    }

    /// Toggle one tool-call group.
    pub(super) fn toggle_group(&mut self, index: usize) {
        if !self.groups.remove(&index) {
            self.groups.insert(index);
        }
    }

    /// Toggle one run of work between assistant replies.
    pub(super) fn toggle_work(&mut self, index: usize) {
        if !self.work_open.remove(&index) {
            self.work_open.insert(index);
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
        // The scroll container's direct children are the items, so
        // `scroll_to_bottom` can follow the tail. It is a flex sibling of the
        // composer, so the composer can never cover the last message.
        let mut list = div()
            .id("pilus-session-scroll")
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            // Every row takes the full width up to the reading measure, so
            // `items_center` centers the column instead of each row's text.
            .items_center()
            .gap_4()
            .px_6()
            .py_6()
            .overflow_y_scroll()
            .track_scroll(&self.scroll);
        // Consecutive tool calls collapse into one row, so a burst of 28 calls
        // does not flood the transcript. `flex_shrink_0` keeps every row at
        // its content height: without it a flex column whose content overflows
        // shrinks the rows until they vanish.
        // Only the tail window renders; the rest fills in behind the scenes
        // (see `render`). Expansion keys stay absolute transcript indices, so
        // growing the window never misaligns them.
        let full = &self.transcript.items;
        let window_start = full.len().saturating_sub(self.visible);
        let items = &full[window_start..];
        let mut index = 0;
        while index < items.len() {
            let absolute = window_start + index;
            let row = if is_work(&items[index].kind) {
                let run_start = index;
                while index < items.len() && is_work(&items[index].kind) {
                    index += 1;
                }
                let run = &items[run_start..index];
                // A run that is only thinking needs no `Working…` wrapper;
                // the thinking row stands on its own.
                if run
                    .iter()
                    .all(|item| matches!(item.kind, ItemKind::Thinking { .. }))
                {
                    let mut column = div().flex().flex_col().gap_4();
                    for (offset, item) in run.iter().enumerate() {
                        column = column.child(div().flex_shrink_0().child(self.item(
                            window_start + run_start + offset,
                            item,
                            fonts,
                            cx,
                        )));
                    }
                    column.into_any_element()
                } else {
                    self.work_group(
                        window_start + run_start,
                        run,
                        window_start + index == full.len(),
                        fonts,
                        cx,
                    )
                }
            } else {
                let row = self.item(absolute, &items[index], fonts, cx);
                index += 1;
                row
            };
            list = list.child(
                div()
                    .w_full()
                    .max_w(px(TRANSCRIPT_MAX_W))
                    .flex_shrink_0()
                    .child(row),
            );
        }
        // The optimistic prompt rides below the transcript until pi appends
        // the real user message.
        if let Some(prompt) = &self.pending_prompt {
            let bubble = self.user_bubble(prompt, 0, fonts);
            list = list.child(
                div()
                    .w_full()
                    .max_w(px(TRANSCRIPT_MAX_W))
                    .flex_shrink_0()
                    .child(bubble),
            );
        }
        list.into_any_element()
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

    /// Whether the scroll-to-bottom floater is needed: content overflows and
    /// the tail is not pinned.
    fn needs_jump_button(&self) -> bool {
        self.scroll.max_offset().y > px(0.0) && !self.following
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
                    this.session.submit(&input, cx);
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
                    this.session.following = true;
                    this.session.scroll.scroll_to_bottom();
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
                    // Leaving the tail: stop following so the jump sticks.
                    this.session.following = false;
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

    /// Whether pi is still working: a prompt is in flight, optimistically
    /// shown, still being steered, or the transcript ends mid-work with no
    /// reply yet.
    fn is_working(&self) -> bool {
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
        let thinking = match transcript.thinking_level.clone() {
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
    /// slug itself.
    fn model_display(&self) -> String {
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
        let current_model = self.transcript.model.clone().unwrap_or_default();
        let current_thinking = self.transcript.thinking_level.clone().unwrap_or_default();
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
        let count = self.transcript.items.len();
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
        // The first paint shows the tail window only; the pin follows live
        // turns, but reading history never yanks back down.
        let offset_y = self.scroll.offset().y;
        let max_y = self.scroll.max_offset().y;
        let at_top = offset_y >= -px(1.0);
        let at_bottom = offset_y <= px(4.0) - max_y;
        let grew = count != self.last_seen;
        if grew {
            let first = self.last_seen == 0;
            self.last_seen = count;
            if first {
                self.visible = count.min(TAIL_WINDOW);
                self.following = true;
            } else if self.following {
                self.visible = count;
            }
            if count > 0 && (first || self.following) {
                self.scroll.scroll_to_bottom();
            }
        }
        if at_bottom {
            self.following = true;
        } else if !grew {
            self.following = false;
        }
        // Background fill, a chunk per frame: while following, the window
        // grows and rides the tail; parked at the top it grows and holds
        // position, so scrolling up lazy-loads history. Mid-viewport stays
        // frozen so reading never shifts.
        if self.window_start() > 0 && (self.following || at_top) {
            if self.following {
                self.visible += FILL_CHUNK;
                if count > 0 {
                    self.scroll.scroll_to_bottom();
                }
            } else {
                self.grow_earlier();
            }
            cx.notify();
        }
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
fn reader_loop(
    endpoint: Endpoint,
    explicit: Option<PathBuf>,
    tx: Sender<Update>,
    stop: Arc<AtomicBool>,
) {
    let mut tail = match open_tail(&endpoint, explicit) {
        Ok(tail) => tail,
        Err(error) => {
            let _ = tx.send(Update::Error(error.to_string()));
            return;
        }
    };
    let interval = match endpoint {
        Endpoint::Local { .. } => LOCAL_POLL,
        Endpoint::Remote { .. } => REMOTE_POLL,
    };
    // Send the initial fold immediately so the panel is not blank for a tick.
    if tx
        .send(Update::Transcript(Box::new(tail.transcript().clone())))
        .is_err()
    {
        return;
    }
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(interval);
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match tail.poll() {
            Ok(true) => {
                if tx
                    .send(Update::Transcript(Box::new(tail.transcript().clone())))
                    .is_err()
                {
                    break;
                }
            }
            Ok(false) => {}
            Err(error) => {
                if tx.send(Update::Error(error.to_string())).is_err() {
                    break;
                }
            }
        }
    }
}

/// Resolve and open the session's byte source.
fn open_tail(endpoint: &Endpoint, explicit: Option<PathBuf>) -> Result<Tail, SessionError> {
    match endpoint {
        Endpoint::Local { api_socket, cwd } => {
            let path = match explicit {
                Some(path) => Some(path),
                None => resolve_local(api_socket, cwd),
            };
            match path {
                Some(path) => Tail::open(path),
                None => Err(SessionError::Source(format!(
                    "no pi session found for `{cwd}`"
                ))),
            }
        }
        Endpoint::Remote { target, cwd } => {
            let path = resolve_remote(target, cwd)?;
            Tail::with_source(Box::new(SshSource {
                target: target.clone(),
                path,
            }))
        }
    }
}

/// Find the local session file, preferring the id the daemon reports.
fn resolve_local(api_socket: &std::path::Path, cwd: &str) -> Option<PathBuf> {
    let root = pilus_session::sessions_root();
    if !cwd.is_empty()
        && let Ok(mut client) = ControlClient::connect(api_socket)
        && let Ok(sessions) = client.agent_sessions()
    {
        let id = sessions
            .iter()
            .filter(|session| session.agent == "pi")
            .filter(|session| session.cwd == cwd || cwd.starts_with(&format!("{}/", session.cwd)))
            .max_by_key(|session| session.cwd.len())
            .map(|session| session.session_id.clone());
        if let Some(id) = id
            && let Some(path) = pilus_session::find_by_id(&root, cwd, &id)
        {
            return Some(path);
        }
    }
    if cwd.is_empty() {
        return None;
    }
    pilus_session::latest(&root, cwd)
}

/// Find the remote session file over SSH, preferring the daemon-reported id.
fn resolve_remote(target: &SshTarget, cwd: &str) -> Result<String, SessionError> {
    let slug = pilus_session::dir_name_for(cwd);
    let id = ControlClient::connect_ssh(target.clone())
        .ok()
        .and_then(|mut client| client.agent_sessions().ok())
        .and_then(|sessions| {
            sessions
                .into_iter()
                .filter(|session| session.agent == "pi")
                .find(|session| session.cwd == cwd)
                .map(|session| session.session_id)
        });
    let glob = match id {
        Some(id) => format!("*_{id}.jsonl"),
        None => "*.jsonl".to_string(),
    };
    let command = format!(
        "dir=\"${{PI_CODING_AGENT_SESSION_DIR:-$HOME/.pi/agent/sessions}}/{slug}\"; \
         ls -t \"$dir\"/{glob} 2>/dev/null | head -n1"
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
        return Err(SessionError::Source(format!(
            "no pi session found for `{cwd}` on {}",
            target.destination
        )));
    }
    Ok(path)
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

    #[test]
    fn earlier_messages_grow_the_window() {
        let mut panel = SessionPanel::default();
        for index in 0..120 {
            panel.transcript.items.push(Item {
                id: index.to_string(),
                timestamp: String::new(),
                kind: ItemKind::Note {
                    title: "n".to_string(),
                    text: String::new(),
                },
            });
        }
        // First paint shows the tail only.
        assert_eq!(panel.window_start(), 70);
        panel.grow_earlier();
        assert_eq!(panel.visible, TAIL_WINDOW + FILL_CHUNK);
        assert_eq!(panel.window_start(), 0);
        // Nothing left to reveal; a second press is a no-op.
        panel.grow_earlier();
        assert_eq!(panel.window_start(), 0);
    }

    #[test]
    fn jumping_to_an_older_prompt_widens_the_window() {
        let mut panel = SessionPanel::default();
        for index in 0..120 {
            panel.transcript.items.push(Item {
                id: index.to_string(),
                timestamp: String::new(),
                kind: ItemKind::User {
                    text: format!("prompt {index}"),
                    images: 0,
                },
            });
        }
        // The tail window holds no row for the fifth prompt, so a jump has to
        // reveal one before it can scroll.
        assert_eq!(panel.window_start(), 70);
        panel.jump_to(5);
        assert_eq!(panel.window_start(), 5);
        // A prompt already in the window leaves the window alone.
        panel.visible = TAIL_WINDOW;
        panel.jump_to(119);
        assert_eq!(panel.visible, TAIL_WINDOW);
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
}
