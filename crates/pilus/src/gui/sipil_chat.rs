//! The native chat page for a sipil conversation, drawn in the terminal's card:
//! the transcript, and a composer to say something back.
//!
//! The page owns no conversation data. [`sipil`] holds the transcript of
//! whichever conversation is [`Watch`]ed and refreshes it while the page is
//! open; this paints a snapshot of it and hands intent back (send, stop, close).
//! The log is a virtualized list: only the lines in view are built and drawn, so
//! a long conversation costs what a short one does, and a line arriving
//! re-measures only the lines it touched. The list follows the end until the
//! reader scrolls up, and follows again when they come back.
//! A message is cleared from the composer only once sipil has taken it, so one
//! that was refused (no API key yet, say) stays there to fix and retry.
//!
//! Agents and sessions open here from the sidebar. A call an agent made is a
//! line that folds open to show what it was given and what came back. Any line
//! of a conversation can be replied to, which quotes it above what is sent.

use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AnyElement, Context, Div, Entity, Focusable, FollowMode, FontWeight, KeyDownEvent,
    ListAlignment, ListState, MouseButton, ScrollHandle, Stateful, div, img, list, prelude::*, px,
};
use pilus_client::sipil::{
    Activity, ItemRole, Quote, Routine, RunningTool, ToolCall, TranscriptItem, WorkBoard,
};

use super::markdown::{self, Document};
use super::md_select::Leaves;
use super::sipil::{self, Reach, SipilState, Watch};
use super::sipil_social as social;
use super::style::{self, ButtonKind, RadiusExt};
use super::text_input::TextInput;
use super::theme::Theme;
use super::{PilusView, icon, row_tooltip};

/// Pixels of lines the list builds beyond each edge of the view, so a scroll
/// does not show a line before it is drawn.
const OVERDRAW: f32 = 1200.0;
/// How near the top, in rows, the first visible line must be for the older
/// lines to be fetched, so they are in before the reader runs out.
const OLDER_PREFETCH_ROWS: usize = 3;
/// Width of the panel beside the log, for a thread.
const SIDE_PANEL: f32 = 340.0;
/// Lines of a tool's result shown when its call is folded open; the rest is cut.
const TOOL_LINES: usize = 40;

/// The page's own state: whether it is open, and what it last scrolled for.
pub(super) struct SipilChat {
    pub(super) open: bool,
    input: Entity<TextInput>,
    /// The field a reply is typed in, at the foot of the thread panel.
    thread_input: Entity<TextInput>,
    /// The keyboard is the thread's field, not the log's.
    in_thread: bool,
    /// The last message was sent from the thread's field, so that is the one to clear.
    sent_in_thread: bool,
    /// The field a contact's new name is typed in, in their panel.
    rename: Entity<TextInput>,
    /// A contact is being renamed, so the keyboard is the rename field's.
    pub(super) renaming: bool,
    /// The log: one list item per line that has its own row.
    list: ListState,
    /// The lines the list was last told about, and what the page needs to draw
    /// them, so a change re-measures only the lines it touched.
    built: Built,
    /// The snapshot the list was last brought up to date with.
    synced: Option<Arc<SipilState>>,
    /// The send count the composer was last cleared for.
    sent: u64,
    /// Parsed messages by their text, so a line is parsed once, not at every
    /// repaint. Emptied when it grows past a page of conversation.
    docs: RefCell<HashMap<String, Rc<Document>>>,
    /// The calls the person folded open, by call id. Folded shut is the rest.
    open_calls: HashSet<String>,
    /// The line the next message answers.
    reply: Option<Quote>,
    /// The daemon's description of itself is open beside the log.
    pub(super) about_open: bool,
    /// The daemon's board of open work and its routines are open beside the
    /// log.
    pub(super) work_open: bool,
    /// The agents whose instructions are folded open in it, by profile id.
    open_agents: HashSet<String>,
    /// The channel member whose removal is waiting for a second click.
    confirm_remove: Option<String>,
    /// The daemon whose agent is about to be invited into the open channel,
    /// waiting to be confirmed.
    confirm_agent: Option<String>,
    /// The line whose thread is open beside the log, by item id. Replies to it
    /// are drawn there and not in the log.
    thread: Option<u64>,
    thread_scroll: ScrollHandle,
    /// Lines in the open thread when the panel last scrolled, so it follows a
    /// reply arriving.
    thread_len: usize,
    /// The settings the log is set in, handed to its lines.
    log_settings: RefCell<Option<Rc<RefCell<crate::settings::Settings>>>>,
}

/// How tall the answer being written may grow before its start is cut.
const LIVE_HEIGHT: f32 = 160.0;
/// The most of an answer being written that is laid out: the end of it, which is
/// where the writing is.
const LIVE_CHARS: usize = 1200;

/// The end of an answer being written, with a mark where the start was cut.
fn live_tail(live: &str) -> String {
    let count = live.chars().count();
    if count <= LIVE_CHARS {
        return live.to_string();
    }
    let tail: String = live.chars().skip(count - LIVE_CHARS).collect();
    format!("…{tail}")
}

/// A line of the transcript that has a row, and a digest of what it shows.
struct Row {
    /// Its place in the transcript.
    at: usize,
    sig: u64,
}

/// What the list was last built from.
#[derive(Default)]
struct Built {
    watch: Watch,
    rows: Vec<Row>,
    /// The call a result answers, by call id, as a place in the transcript.
    results: HashMap<String, usize>,
    /// What each line that has a thread holds: the places of its replies.
    threads: HashMap<usize, Vec<usize>>,
    /// The line whose thread each reply belongs to.
    root_of: HashMap<usize, usize>,
    me: String,
    speaker: String,
    /// Columns of the nick column: the longest nick in view.
    nick_columns: usize,
    /// The conversation is working, so a call without a result is running.
    working: bool,
    /// The id of the first line that has a row, to tell when older lines were
    /// loaded in front of it.
    first_id: Option<u64>,
}

impl SipilChat {
    pub(super) fn new(
        font: gpui::Font,
        font_size: f32,
        theme: Theme,
        cx: &mut Context<PilusView>,
    ) -> Self {
        let input = cx.new(|cx| {
            TextInput::new(
                String::new(),
                "Message…",
                font.clone(),
                px(font_size),
                true,
                theme,
                cx,
            )
            .submit_on_enter()
        });
        let thread_input = cx.new(|cx| {
            TextInput::new(
                String::new(),
                "Reply…",
                font.clone(),
                px(font_size),
                true,
                theme,
                cx,
            )
            .submit_on_enter()
        });
        let rename = cx.new(|cx| {
            TextInput::new(String::new(), "Name", font, px(font_size), false, theme, cx)
                .submit_on_enter()
        });
        let list = ListState::new(0, ListAlignment::Top, px(OVERDRAW));
        list.set_follow_mode(FollowMode::Tail);
        // Older lines come as the reader nears the top; `load_older` ignores the
        // call when there are none or they are already on their way.
        list.set_scroll_handler(|event, _window, _cx| {
            if event.visible_range.start < OLDER_PREFETCH_ROWS {
                sipil::load_older();
            }
        });
        Self {
            open: false,
            input,
            thread_input,
            in_thread: false,
            sent_in_thread: false,
            rename,
            renaming: false,
            list,
            built: Built::default(),
            synced: None,
            sent: 0,
            docs: RefCell::default(),
            open_calls: HashSet::new(),
            reply: None,
            about_open: false,
            work_open: false,
            open_agents: HashSet::new(),
            confirm_remove: None,
            confirm_agent: None,
            thread: None,
            thread_scroll: ScrollHandle::new(),
            thread_len: 0,
            log_settings: RefCell::default(),
        }
    }

    /// Bring the list up to date with `state`. A conversation just opened, or
    /// whose nick column changed, starts over at its end; otherwise only the
    /// lines from the first one that changed are re-measured, which is the
    /// tail when a line arrives.
    fn sync(&mut self, state: &Arc<SipilState>) {
        if self
            .synced
            .as_ref()
            .is_some_and(|synced| Arc::ptr_eq(synced, state))
        {
            return;
        }
        self.synced = Some(Arc::clone(state));
        let subject = Subject::of(state);
        let me = nick_of(state);
        let working = subject.activity == Activity::Working;
        let items = &state.transcript;
        let known = known_calls(items);
        let results: HashMap<String, usize> = items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.role == ItemRole::Tool)
            .filter_map(|(at, item)| Some((item.call_id.clone()?, at)))
            .collect();
        let (threads, root_of) = group_threads(items, matches!(state.watch, Watch::On(..)));
        let rows: Vec<Row> = items
            .iter()
            .enumerate()
            // A reply is drawn in its thread, not in the log.
            .filter(|(at, _)| !root_of.contains_key(at))
            .filter(|(_, item)| {
                // A result is drawn under its call, so a result whose call is in
                // view has no row.
                !(item.role == ItemRole::Tool
                    && item.call_id.as_deref().is_some_and(|id| known.contains(id)))
            })
            .map(|(at, item)| Row {
                at,
                sig: signature(
                    item,
                    items,
                    &results,
                    working,
                    threads.get(&at).map_or(0, Vec::len),
                    &state.running,
                ),
            })
            .collect();
        let nick_columns = items
            .iter()
            .map(|item| line_nick(item, &me, &subject.speaker).chars().count())
            .max()
            .unwrap_or(0)
            .clamp(4, 24);

        let restart = self.built.watch != state.watch
            || self.built.me != me
            || self.built.speaker != subject.speaker
            || self.built.nick_columns != nick_columns;
        if restart {
            self.list.reset(rows.len());
            self.list.set_follow_mode(FollowMode::Tail);
            self.list.scroll_to_end();
        } else {
            // Older lines loaded in front: rows are inserted before the first one,
            // which moves the list's scroll anchor with them, so the person stays on
            // the line they were reading.
            let lead = match (self.built.first_id, self.built.rows.first()) {
                (Some(first), Some(old_first)) => rows
                    .iter()
                    .position(|row| items.get(row.at).is_some_and(|item| item.id == first))
                    .filter(|&lead| rows[lead].at >= old_first.at),
                _ => None,
            }
            .unwrap_or(0);
            let shift = if lead > 0 {
                self.list.splice(0..0, lead);
                rows[lead].at - self.built.rows[0].at
            } else {
                0
            };
            let kept = self
                .built
                .rows
                .iter()
                .zip(&rows[lead..])
                .take_while(|(old, new)| old.at + shift == new.at && old.sig == new.sig)
                .count();
            let old = self.built.rows.len();
            if kept < old || lead + kept < rows.len() {
                self.list
                    .splice(lead + kept..lead + old, rows.len() - lead - kept);
            }
        }
        let first_id = rows
            .first()
            .and_then(|row| items.get(row.at))
            .map(|item| item.id);
        self.built = Built {
            watch: state.watch.clone(),
            rows,
            results,
            threads,
            root_of,
            me,
            speaker: subject.speaker,
            nick_columns,
            working,
            first_id,
        };
    }

    /// Scroll the open thread to its end when it has grown, or was just opened.
    fn follow_thread(&mut self, state: &SipilState) {
        let open = self
            .thread
            .and_then(|id| state.transcript.iter().position(|item| item.id == id));
        let len = open.map_or(0, |at| self.built.threads.get(&at).map_or(0, Vec::len));
        if len != self.thread_len {
            self.thread_len = len;
            self.thread_scroll.scroll_to_bottom();
        }
    }

    /// Open the thread of the line `id`, for the captures.
    #[cfg(feature = "visual-test")]
    pub(super) fn open_thread_for_shot(&mut self, id: u64) {
        self.thread = Some(id);
        self.thread_len = usize::MAX;
    }

    /// Open the daemon's page with the instructions of `agents` folded open,
    /// for the captures.
    #[cfg(feature = "visual-test")]
    pub(super) fn open_about_for_shot(&mut self, agents: &[&str]) {
        self.about_open = true;
        self.open_agents = agents.iter().map(ToString::to_string).collect();
    }

    /// Close the thread, for the captures.
    #[cfg(feature = "visual-test")]
    pub(super) fn close_thread_for_shot(&mut self) {
        self.thread = None;
    }

    /// The field that has the keyboard: the composer, the thread's reply field,
    /// or the contact's name while it is being changed.
    pub(super) fn active_input(&self) -> &Entity<TextInput> {
        if self.renaming {
            &self.rename
        } else if self.thread.is_some() && self.in_thread {
            &self.thread_input
        } else {
            &self.input
        }
    }

    /// Fold `calls` open and start a reply to `who`'s `text` (none when `who` is
    /// empty), for the captures.
    #[cfg(feature = "visual-test")]
    pub(super) fn fold_open_for_shot(&mut self, calls: &[&str], who: &str, text: &str) {
        self.open_calls = calls.iter().map(ToString::to_string).collect();
        self.reply = (!who.is_empty()).then(|| Quote::of(who, text));
    }
}

/// What the page needs to know about the conversation it shows.
struct Subject {
    title: String,
    /// Who the other side's lines are from, when a line does not say.
    speaker: String,
    activity: Activity,
    /// The page is a chat with people or their agent, not a conversation on
    /// this pilus's own daemon: there is no model to show.
    peer: bool,
}

impl Subject {
    fn of(state: &SipilState) -> Self {
        match &state.watch {
            Watch::Peer(id) => {
                let name = state.watched_person().map_or_else(
                    || id.chars().take(8).collect(),
                    |person| person.contact.name.clone(),
                );
                return Self {
                    title: name.clone(),
                    speaker: name,
                    activity: Activity::Idle,
                    peer: true,
                };
            }
            Watch::Channel(_) => {
                let channel = state.watched_channel();
                let name = channel.map_or("channel", |channel| channel.name.as_str());
                return Self {
                    title: format!("#{name}"),
                    speaker: String::new(),
                    activity: Activity::Idle,
                    peer: true,
                };
            }
            Watch::Off | Watch::On(..) => {}
        }
        let daemon = state.watched_daemon();
        let daemon_name = daemon.map_or_else(|| "sipil".to_string(), |daemon| daemon.name());
        let id = match &state.watch {
            Watch::On(_, id) => Some(id.as_str()),
            _ => None,
        };
        // What the daemon says this conversation is, which the lists may not.
        let info = state
            .watched
            .as_ref()
            .filter(|info| Some(info.id.as_str()) == id);
        if let Some(agent) = daemon.and_then(|daemon| {
            daemon
                .agents
                .iter()
                .find(|agent| Some(agent.id.as_str()) == id)
        }) {
            return Self {
                title: agent.name.clone(),
                speaker: agent.name.clone(),
                activity: agent.status,
                peer: false,
            };
        }
        if let Some(session) = daemon.and_then(|daemon| {
            daemon
                .sessions
                .iter()
                .find(|session| Some(session.id.as_str()) == id)
        }) {
            return Self {
                title: session.title.clone(),
                speaker: daemon_name,
                activity: session.status,
                peer: false,
            };
        }
        if let Some(info) = info {
            return Self {
                title: info.name.clone(),
                speaker: daemon_name,
                activity: info.status,
                peer: false,
            };
        }
        Self {
            title: "New session".to_string(),
            speaker: daemon_name,
            activity: Activity::Idle,
            peer: false,
        }
    }
}

impl PilusView {
    /// Open a conversation in the chat page, in place of whatever page is open.
    pub(super) fn open_sipil_chat(&mut self, watch: Watch, cx: &mut Context<Self>) {
        let same = self.sipil.watch == watch && self.sipil_chat.open;
        // Closing the others also closes this page, so it comes first.
        self.leave_settings_page();
        self.sipil_chat.open = true;
        if !same {
            self.sipil_chat
                .input
                .update(cx, |input, cx| input.clear(cx));
            // A folded-open call and a half-made reply belong to the
            // conversation they were made in.
            self.sipil_chat.open_calls.clear();
            self.sipil_chat.reply = None;
            self.sipil_chat.about_open = false;
            self.sipil_chat.work_open = false;
            self.sipil_chat.open_agents.clear();
            self.sipil_chat.confirm_remove = None;
            self.sipil_chat.confirm_agent = None;
            self.sipil_chat.renaming = false;
            self.sipil_chat.thread = None;
            self.sipil_chat.in_thread = false;
            self.sipil_chat
                .thread_input
                .update(cx, |input, cx| input.clear(cx));
            self.sipil_chat.thread_len = 0;
            // The list starts over at the end of the conversation it opens on.
            self.sipil_chat.synced = None;
            self.sipil_chat.built.watch = Watch::Off;
            sipil::watch(watch);
        }
        cx.notify();
    }

    /// Start a reply to the line `id`, quoting `quote`: the reply goes in that
    /// line's thread, opened beside the log, and not in the log.
    fn begin_reply(&mut self, id: u64, quote: Quote, cx: &mut Context<Self>) {
        let items = &self.sipil.transcript;
        let built = &self.sipil_chat.built;
        let root = items
            .iter()
            .position(|item| item.id == id)
            .map_or(id, |at| {
                built.root_of.get(&at).map_or(id, |&root| items[root].id)
            });
        self.sipil_chat.thread = Some(root);
        self.sipil_chat.in_thread = true;
        self.sipil_chat.reply = Some(quote);
        self.sipil_chat.thread_len = usize::MAX;
        cx.notify();
    }

    /// Open the thread of the line `id` without a reply begun.
    fn open_thread(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(quote) = self.thread_quote_of(id) else {
            return;
        };
        self.begin_reply(id, quote, cx);
    }

    /// Close the thread panel and drop the reply it was making.
    fn close_thread(&mut self, cx: &mut Context<Self>) {
        self.sipil_chat.thread = None;
        self.sipil_chat.in_thread = false;
        self.sipil_chat.reply = None;
        cx.notify();
    }

    /// What a reply to the line `id` quotes.
    fn thread_quote_of(&self, id: u64) -> Option<Quote> {
        let item = self.sipil.transcript.iter().find(|item| item.id == id)?;
        let built = &self.sipil_chat.built;
        let body = reply_of(item, matches!(self.sipil.watch, Watch::On(..)))
            .map_or_else(|| item.text.clone(), |(_, body)| body);
        Some(Quote::of(
            line_nick(item, &built.me, &built.speaker).trim_end_matches(':'),
            &body,
        ))
    }

    /// What a reply to the open thread's line quotes, when one is open.
    fn thread_quote(&self) -> Option<Quote> {
        self.thread_quote_of(self.sipil_chat.thread?)
    }

    /// Fold a call open or shut.
    fn toggle_sipil_call(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.sipil_chat.open_calls.remove(id) {
            self.sipil_chat.open_calls.insert(id.to_string());
        }
        // Opened or shut, the line is a different height.
        let at = self
            .sipil
            .transcript
            .iter()
            .position(|item| item.calls.iter().any(|call| call.id == id));
        if let Some(row) = at.and_then(|at| {
            self.sipil_chat
                .built
                .rows
                .iter()
                .position(|row| row.at == at)
        }) {
            self.sipil_chat.list.remeasure_items(row..row + 1);
        }
        cx.notify();
    }

    /// Close the page and stop reading its conversation.
    pub(super) fn close_sipil_chat(&mut self) {
        if self.sipil_chat.open {
            self.sipil_chat.open = false;
            sipil::watch(Watch::Off);
        }
    }

    /// Per-tick upkeep: leave when sipil goes away, clear a sent message, and
    /// follow the conversation. Returns whether a repaint is needed.
    pub(super) fn sipil_chat_tick(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.sipil_chat.open {
            return false;
        }
        // A chat with people never depended on a daemon; a daemon's page is
        // closed once the daemon is gone.
        // A channel this pilus was taken out of, or left, is gone: its page goes
        // with it, and nothing can be said in it.
        let channel_gone = matches!(&self.sipil.watch, Watch::Channel(id)
            if !self.sipil.me.channels.iter().any(|row| row.channel.id == *id));
        if self.sipil.watch == Watch::Off
            || channel_gone
            || (self.sipil.watch.daemon().is_some() && self.sipil.watched_daemon().is_none())
        {
            self.close_sipil_chat();
            return true;
        }
        if self.sipil.sent != self.sipil_chat.sent {
            self.sipil_chat.sent = self.sipil.sent;
            // In a thread, the next message is a reply to it too.
            self.sipil_chat.reply = self.thread_quote();
            let sent = if self.sipil_chat.sent_in_thread {
                &self.sipil_chat.thread_input
            } else {
                &self.sipil_chat.input
            };
            sent.update(cx, |input, cx| input.clear(cx));
        }
        self.sipil_chat.sync(&self.sipil);
        self.sipil_chat.follow_thread(&self.sipil);
        false
    }

    pub(super) fn sipil_chat_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        // While a contact is renamed the keys are the name's.
        if self.sipil_chat.renaming {
            match key {
                "enter" => {
                    if let Some(id) = self.sipil.watched_person().map(|p| p.contact.id.clone()) {
                        self.commit_contact_rename(&id, cx);
                    }
                }
                "escape" => {
                    self.sipil_chat.renaming = false;
                    cx.notify();
                }
                _ => {}
            }
            return;
        }
        match key {
            // Escape undoes the nearest thing: a reply being made, and last the page.
            "escape" if self.sipil_chat.thread.is_some() => self.close_thread(cx),
            "escape" if self.sipil_chat.reply.is_some() => {
                self.sipil_chat.reply = None;
                cx.notify();
            }
            "escape" => {
                self.close_sipil_chat();
                cx.notify();
            }
            // The composer declines a bare Enter, and Shift+Enter it takes
            // itself as a new line, so what reaches here is a send.
            "enter" => self.sipil_send(cx),
            _ => {}
        }
    }

    /// Send what is in the composer.
    fn sipil_send(&mut self, cx: &mut Context<Self>) {
        let in_thread = self.sipil_chat.thread.is_some() && self.sipil_chat.in_thread;
        let text = if in_thread {
            self.sipil_chat.thread_input.read(cx).value()
        } else {
            self.sipil_chat.input.read(cx).value()
        };
        if text.trim().is_empty() || self.sipil.sending {
            return;
        }
        self.sipil_chat.sent_in_thread = in_thread;
        // A line typed in the thread answers its line; one typed in the log is its own.
        let reply = self.sipil_chat.reply.clone().filter(|_| in_thread);
        sipil::send(text.trim().to_string(), reply);
        cx.notify();
    }

    /// The page, laid out like an IRC client: a topic bar, a log of
    /// `time nick: text` lines, and one input line. The log is a list that
    /// builds only the lines in view.
    pub(super) fn sipil_chat_page(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let mono = style::mono_family(&self.shared_settings);
        let state = &self.sipil;
        let subject = Subject::of(state);
        let thinking = state.thinking_here();
        // A daemon that is away is not writing, whatever it was doing when it
        // went.
        let away = matches!(state.watch, Watch::On(..))
            && state
                .watched_daemon()
                .is_some_and(|daemon| !daemon.connected());
        let working = !away && (subject.activity == Activity::Working || !thinking.is_empty());
        // The log is set in the terminal font, Markdown included: the renderer
        // takes its body family from the Interface font, so it is handed a copy
        // of the settings whose Interface font is the terminal's.
        let log_settings = {
            let mut copy = self.shared_settings.borrow().clone();
            copy.ui_font = Some(copy.terminal_font.clone());
            Rc::new(RefCell::new(copy))
        };

        let mut status = div().flex().flex_row().items_center().gap_2();
        if working && matches!(state.watch, Watch::On(..)) {
            status = status.child(
                style::small_button(
                    "pilus-sipil-chat-stop",
                    "Stop",
                    ButtonKind::Secondary,
                    theme,
                    fonts.label,
                )
                .on_click(cx.listener(|_, _, _, cx| {
                    sipil::abort();
                    cx.notify();
                })),
            );
        }
        if let (Watch::Peer(_), Some(person)) = (&state.watch, state.watched_person()) {
            let (text, color) = match person.reach {
                Reach::Online => ("online", theme.ok),
                Reach::Waiting => ("has not added you yet", theme.muted),
                Reach::Offline => ("offline", theme.muted),
                Reach::Unknown => ("", theme.muted),
            };
            status = status.child(
                div()
                    .text_size(px(fonts.secondary))
                    .text_color(color)
                    .child(text),
            );
        }
        // A daemon's conversation wears the daemon's logo, or the pilus mark.
        let logo = state
            .watched_daemon()
            .filter(|_| matches!(state.watch, Watch::On(..)))
            .map(|daemon| daemon_mark(daemon, 22.0, theme));
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_3()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .font_family(mono.clone())
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .flex()
                    .flex_row()
                    .gap_3()
                    .children(logo)
                    .child(
                        div()
                            .text_size(px(fonts.body))
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme.fg)
                            .child(subject.title.clone()),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .flex_shrink_0()
                    .gap_2()
                    .child(status)
                    .children(self.work_toggle(cx))
                    .children(self.about_toggle(cx)),
            );

        *self.sipil_chat.log_settings.borrow_mut() = Some(Rc::clone(&log_settings));
        let hint = self.sipil_chat.built.rows.is_empty().then(|| {
            // Lines that have arrived and are not in the list yet are a
            // tick from showing.
            if !state.transcript_loaded || !state.transcript.is_empty() {
                "-- Loading…"
            } else if matches!(state.watch, Watch::Channel(_)) {
                "-- Nothing here yet. Say something, or mention an agent with @name."
            } else if subject.peer {
                "-- No messages yet. Say hello."
            } else {
                "-- Say something to start."
            }
        });
        let log: AnyElement = match hint {
            Some(hint) => div()
                .flex_1()
                .min_h(px(0.0))
                .px_3()
                .py_2()
                .font_family(mono.clone())
                .text_size(px(fonts.secondary))
                .text_color(theme.muted)
                .child(hint)
                .into_any_element(),
            None => list(
                self.sipil_chat.list.clone(),
                cx.processor(|this: &mut PilusView, ix: usize, _window, cx| {
                    this.sipil_chat_row(ix, cx)
                }),
            )
            .flex_1()
            .min_h(px(0.0))
            .w_full()
            .into_any_element(),
        };

        let mut main = div().flex().flex_col().flex_1().min_w_0().h_full();
        // The daemon is the only place older lines are, so with it away there is
        // nothing to ask; scrolling to the top asks (see `SipilChat::new`).
        if state.loading_older && matches!(state.watch, Watch::On(..)) && !away {
            main = main.child(
                div()
                    .flex_shrink_0()
                    .px_3()
                    .py_1()
                    .font_family(mono.clone())
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child("-- Loading older messages…"),
            );
        }
        main = main.child(log);
        if away {
            let name = state
                .watched_daemon()
                .map_or_else(|| "The daemon".to_string(), |daemon| daemon.name());
            main = main.child(
                div()
                    .flex_shrink_0()
                    .px_3()
                    .py_1()
                    .font_family(mono.clone())
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(format!(
                        "-- {name} is away. This is the conversation as it was; what you write is sent when it is back."
                    )),
            );
        }
        // What was written while the daemon or the contact was away, until they
        // take it.
        if matches!(state.watch, Watch::On(..) | Watch::Peer(_)) {
            let me = nick_of(state);
            for queued in &state.queued {
                let request = queued.request.clone();
                main = main.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .flex_shrink_0()
                        .px_3()
                        .py_1()
                        .font_family(mono.clone())
                        .text_size(px(fonts.secondary))
                        .text_color(theme.muted)
                        .child(pilus_client::sipil::clock_label(queued.at))
                        .child(format!("{me}:"))
                        .child(div().flex_1().min_w_0().child(queued.text.clone()))
                        .child("waiting")
                        .child(
                            style::small_button(
                                format!("pilus-sipil-discard-{}", queued.request),
                                "Discard",
                                ButtonKind::Ghost,
                                theme,
                                fonts.label,
                            )
                            .on_click(cx.listener(
                                move |_, _, _, cx| {
                                    sipil::discard_queued(&request);
                                    cx.notify();
                                },
                            )),
                        ),
                );
            }
        }
        // Who is writing, said at the end of the messages.
        if working {
            let who = if thinking.is_empty() {
                subject.title.clone()
            } else {
                thinking.join(", ")
            };
            // The answer as far as it is written, for a daemon that streams.
            if !state.live.is_empty() && matches!(state.watch, Watch::On(..)) {
                main = main.child(
                    div()
                        .flex_shrink_0()
                        .px_3()
                        .py_1()
                        .max_h(px(LIVE_HEIGHT))
                        .overflow_hidden()
                        .font_family(mono.clone())
                        .text_size(px(fonts.secondary))
                        .text_color(theme.fg)
                        .child(live_tail(&state.live)),
                );
            }
            main = main.child(
                div()
                    .flex_shrink_0()
                    .px_3()
                    .py_1()
                    .font_family(mono.clone())
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(format!("{who} is typing…")),
            );
        }
        if let Some(error) = state.send_error.as_ref().or(state.chat_error.as_ref()) {
            main = main.child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .font_family(mono.clone())
                    .text_size(px(fonts.secondary))
                    .text_color(theme.alert)
                    .child("!!")
                    .child(div().flex_1().min_w_0().child(error.clone())),
            );
        }
        let main = main.child(self.irc_input(fonts, cx));
        let mut body = div().flex().flex_row().flex_1().min_h(px(0.0)).child(main);
        let thread = self
            .sipil_chat
            .thread
            .and_then(|id| self.sipil.transcript.iter().position(|item| item.id == id));
        if let Some(root) = thread {
            body = body.child(self.thread_panel(root, fonts, &log_settings, cx));
        } else if self.sipil_chat.work_open && self.watched_board().is_some() {
            body = body.child(self.work_panel(fonts, cx));
        } else if self.sipil_chat.about_open && self.sipil.watched_channel().is_some() {
            body = body.child(self.channel_panel(fonts, cx));
        } else if self.sipil_chat.about_open
            && matches!(self.sipil.watch, Watch::Peer(_))
            && self.sipil.watched_person().is_some()
        {
            body = body.child(self.contact_panel(fonts, cx));
        } else if self.sipil_chat.about_open && self.about_of_watched().is_some() {
            body = body.child(self.about_panel(fonts, cx));
        }
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(header)
            .child(body)
    }

    /// What the daemon of the open conversation says about itself, when this
    /// is a conversation with one that does.
    fn about_of_watched(&self) -> Option<&pilus_client::sipil::About> {
        if !matches!(self.sipil.watch, Watch::On(..)) {
            return None;
        }
        self.sipil.watched_daemon()?.about.as_ref()
    }

    /// The board and routines of the daemon of the open conversation, when it
    /// is a conversation with a daemon that keeps them.
    fn watched_board(&self) -> Option<(&WorkBoard, &[Routine])> {
        if !matches!(self.sipil.watch, Watch::On(..)) {
            return None;
        }
        let daemon = self.sipil.watched_daemon()?;
        if daemon.board_unsupported {
            return None;
        }
        Some((daemon.work.as_ref()?, daemon.routines.as_slice()))
    }

    /// The header's button for the daemon's board of open work: none for a
    /// person, a channel, or a daemon from before the board. It reads how many
    /// items wait on someone, since those are the ones to look at.
    fn work_toggle(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (board, routines) = self.watched_board()?;
        let theme = self.theme;
        let open = self.sipil_chat.work_open;
        let stuck = board.open.iter().filter(|item| item.stuck()).count();
        let tip = if open {
            "Hide open work".to_string()
        } else {
            work_summary(board, routines.len())
        };
        let button = style::icon_button(
            "pilus-sipil-work-toggle",
            include_bytes!("../../assets/icons/list-checks.svg"),
            if open {
                ButtonKind::Secondary
            } else {
                ButtonKind::Ghost
            },
            theme,
            18.0,
        )
        .tooltip(row_tooltip(tip, theme.panel_fg, Vec::new(), theme))
        .on_click(cx.listener(|this, _, _, cx| {
            this.sipil_chat.work_open = !this.sipil_chat.work_open;
            this.sipil_chat.about_open = false;
            // It shares the side with a thread.
            if this.sipil_chat.thread.take().is_some() {
                this.sipil_chat.reply = None;
            }
            cx.notify();
        }));
        Some(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .child(button)
                .when(stuck > 0, |row| {
                    row.child(
                        div()
                            .text_color(theme.alert)
                            .font_weight(FontWeight::BOLD)
                            .child(stuck.to_string()),
                    )
                })
                .into_any_element(),
        )
    }

    /// The daemon's board beside the log: what waits on someone first, then
    /// what is moving and what is queued; its routines with when each runs
    /// next; and what it closed lately. The agent keeps it; this only shows it.
    fn work_panel(&self, fonts: style::Fonts, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let mono = style::mono_family(&self.shared_settings);
        let Some((board, routines)) = self.watched_board() else {
            return div();
        };
        let now = now_ms();
        let heading = |text: String| {
            div()
                .pt_2()
                .text_size(px(fonts.label))
                .text_color(theme.muted)
                .child(text.to_uppercase())
        };
        let mut panel = div()
            .id("pilus-sipil-work-scroll")
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .gap_2()
            .px_3()
            .py_3()
            .overflow_y_scroll();

        panel = panel.child(heading(format!("Open · {}", board.open.len())));
        if board.open.is_empty() {
            panel = panel.child(div().text_color(theme.muted).child(
                "Nothing open. The agent adds an item when it takes on work that outlasts a turn.",
            ));
        }
        for item in &board.open {
            let color = match item.state.as_str() {
                "blocked" => theme.alert,
                // The theme's own yellow: waiting is not an error, but it is not fine either.
                "waiting" => theme.ansi[3],
                "doing" => theme.accent,
                _ => theme.muted,
            };
            let mut card = div()
                .flex()
                .flex_col()
                .gap_1()
                .p_2()
                .border_1()
                .border_color(if item.stuck() { color } else { theme.border })
                .radius_md()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_size(px(fonts.label))
                                .text_color(color)
                                .child(item.state.to_uppercase()),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .font_weight(FontWeight::BOLD)
                                .text_color(theme.fg)
                                .child(item.title.clone()),
                        ),
                );
            for (label, value) in [
                ("Waiting on", &item.waiting_on),
                ("Next", &item.next),
                ("On it", &item.owner),
            ] {
                if !value.is_empty() {
                    card = card.child(
                        div()
                            .text_color(theme.muted)
                            .child(format!("{label}: {value}")),
                    );
                }
            }
            if item.updated_at > 0 {
                card = card.child(
                    div()
                        .text_size(px(fonts.label))
                        .text_color(theme.muted)
                        .child(format!(
                            "#{} · changed {}",
                            item.id,
                            ago(item.updated_at, now)
                        )),
                );
            }
            panel = panel.child(card);
        }

        panel = panel.child(heading(format!("Routines · {}", routines.len())));
        if routines.is_empty() {
            panel = panel.child(
                div()
                    .text_color(theme.muted)
                    .child("No routines. Ask the agent to do something on a schedule."),
            );
        }
        for routine in routines {
            let next = if routine.paused {
                "paused".to_string()
            } else {
                routine.next_run_at.map_or_else(
                    || "not again".to_string(),
                    |at| format!("next {}", until(at, now)),
                )
            };
            let last = match (routine.last_run_at, routine.last_outcome.as_deref()) {
                (Some(at), Some(outcome)) => format!("last {} ({outcome})", ago(at, now)),
                _ => "never run".to_string(),
            };
            panel = panel.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_2()
                    .border_1()
                    .border_color(if routine.last_outcome.as_deref() == Some("failed") {
                        theme.alert
                    } else {
                        theme.border
                    })
                    .radius_md()
                    .child(
                        div()
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme.fg)
                            .child(routine.name.clone()),
                    )
                    .child(div().text_color(theme.muted).child(format!(
                        "{} {} · {next} · {last}",
                        routine.schedule, routine.timezone
                    ))),
            );
        }

        if !board.closed.is_empty() {
            panel = panel.child(heading("Recently closed".to_string()));
            for closed in board.closed.iter().take(CLOSED_SHOWN) {
                panel = panel.child(
                    div()
                        .flex()
                        .flex_col()
                        .child(div().text_color(theme.fg).child(closed.title.clone()))
                        .child(div().text_color(theme.muted).child(format!(
                            "{} · {}",
                            closed.outcome,
                            ago(closed.closed_at, now)
                        ))),
                );
            }
        }

        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w(px(SIDE_PANEL))
            .h_full()
            .border_l_1()
            .border_color(theme.border)
            .font_family(mono)
            .text_size(px(fonts.secondary))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .text_size(px(fonts.label))
                    .text_color(theme.muted)
                    .child("WORK")
                    .child(
                        style::icon_button(
                            "pilus-sipil-work-close",
                            include_bytes!("../../assets/icons/x.svg"),
                            ButtonKind::Ghost,
                            theme,
                            18.0,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.sipil_chat.work_open = false;
                            cx.notify();
                        })),
                    ),
            )
            .child(panel)
    }

    /// The header's button for the daemon's own page: none for a conversation
    /// with a person or a daemon that does not describe itself.
    fn about_toggle(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let channel = self.sipil.watched_channel().is_some();
        let contact =
            self.sipil.watched_person().is_some() && matches!(self.sipil.watch, Watch::Peer(_));
        if !channel && !contact {
            self.about_of_watched()?;
        }
        let theme = self.theme;
        let open = self.sipil_chat.about_open;
        Some(
            style::icon_button(
                "pilus-sipil-about-toggle",
                include_bytes!("../../assets/icons/info.svg"),
                if open {
                    ButtonKind::Secondary
                } else {
                    ButtonKind::Ghost
                },
                theme,
                18.0,
            )
            .tooltip(row_tooltip(
                match (open, channel, contact) {
                    (true, true, _) => "Hide channel info".to_string(),
                    (false, true, _) => "Channel info".to_string(),
                    (true, _, true) => "Hide contact info".to_string(),
                    (false, _, true) => "Contact info".to_string(),
                    (true, ..) => "Hide about".to_string(),
                    (false, ..) => "About this sipil".to_string(),
                },
                theme.panel_fg,
                Vec::new(),
                theme,
            ))
            .on_click(cx.listener(|this, _, _, cx| {
                this.sipil_chat.about_open = !this.sipil_chat.about_open;
                this.sipil_chat.work_open = false;
                // It shares the side with a thread.
                if this.sipil_chat.thread.take().is_some() {
                    this.sipil_chat.reply = None;
                }
                cx.notify();
            }))
            .into_any_element(),
        )
    }

    /// A contact, beside their chat: who they are, their device ID, whether
    /// they are there, the channels they are in with you and the agents shared
    /// either way. Rename and remove are here; sharing one of your own agents,
    /// which hands them a shell, stays in the Sipil tab.
    fn contact_panel(&self, fonts: style::Fonts, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let mono = style::mono_family(&self.shared_settings);
        let state = &self.sipil;
        let Some(person) = state.watched_person() else {
            return div();
        };
        let contact = &person.contact;
        let heading = |text: String| {
            div()
                .pt_2()
                .text_size(px(fonts.label))
                .text_color(theme.muted)
                .child(text.to_uppercase())
        };
        let (reach, color) = match person.reach {
            Reach::Online => ("online", theme.ok),
            Reach::Waiting => ("has not added you yet", theme.muted),
            Reach::Offline => ("offline", theme.muted),
            Reach::Unknown => ("", theme.muted),
        };
        let mut name_row = div().flex().flex_row().items_center().gap_2();
        if self.sipil_chat.renaming {
            let input = self.sipil_chat.rename.clone();
            input.update(cx, |input, _| {
                input.set_style(
                    theme,
                    gpui::font(style::mono_family(&self.shared_settings)),
                    px(fonts.body),
                );
            });
            let id = contact.id.clone();
            name_row = name_row
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .px_2()
                        .py_1()
                        .border_1()
                        .border_color(theme.border)
                        .radius_md()
                        .child(input),
                )
                .child(
                    style::small_button(
                        "pilus-sipil-contact-rename-save",
                        "Save",
                        ButtonKind::Primary,
                        theme,
                        fonts.label,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.commit_contact_rename(&id, cx);
                    })),
                );
        } else {
            let (id, name) = (contact.id.clone(), contact.name.clone());
            name_row = name_row
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(fonts.body))
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme.fg)
                        .child(contact.name.clone()),
                )
                .child(
                    style::icon_button(
                        "pilus-sipil-contact-rename",
                        include_bytes!("../../assets/icons/pencil.svg"),
                        ButtonKind::Ghost,
                        theme,
                        18.0,
                    )
                    .tooltip(row_tooltip(
                        "Rename".to_string(),
                        theme.panel_fg,
                        Vec::new(),
                        theme,
                    ))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.start_contact_rename(&id, &name, window, cx);
                    })),
                );
        }
        let mut panel = div()
            .id("pilus-sipil-contact-scroll")
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .gap_2()
            .px_3()
            .py_3()
            .overflow_y_scroll()
            .child(name_row)
            .children((!reach.is_empty()).then(|| div().text_color(color).child(reach)))
            .child(heading("Device".to_string()))
            .child(
                div()
                    .text_size(px(fonts.label))
                    .text_color(theme.fg)
                    .child(contact.id.clone()),
            );

        let channels: Vec<_> = state
            .me
            .channels
            .iter()
            .filter(|row| row.channel.members.contains(&contact.id))
            .collect();
        panel = panel.child(heading(format!("Channels · {}", channels.len())));
        if channels.is_empty() {
            panel = panel.child(
                div()
                    .text_color(theme.muted)
                    .child("Not in a channel with you."),
            );
        }
        for row in channels {
            let id = row.channel.id.clone();
            panel = panel.child(
                div()
                    .id(format!("pilus-sipil-contact-channel-{id}"))
                    .cursor_pointer()
                    .text_color(theme.accent)
                    .hover(|style| style.text_color(theme.fg))
                    .child(format!("#{}", row.channel.name))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_sipil_chat(Watch::Channel(id.clone()), cx);
                    })),
            );
        }

        let confirming = self.sipil_chat.confirm_remove.as_deref() == Some(contact.id.as_str());
        let who = contact.id.clone();
        panel = panel.child(
            div().pt_3().child(
                style::small_button(
                    "pilus-sipil-contact-remove",
                    if confirming {
                        "Confirm: remove contact"
                    } else {
                        "Remove contact"
                    },
                    ButtonKind::Danger,
                    theme,
                    fonts.label,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if this.sipil_chat.confirm_remove.as_deref() == Some(who.as_str()) {
                        this.sipil_chat.confirm_remove = None;
                        social::remove_contact(&who);
                    } else {
                        this.sipil_chat.confirm_remove = Some(who.clone());
                    }
                    cx.notify();
                })),
            ),
        );
        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w(px(SIDE_PANEL))
            .h_full()
            .border_l_1()
            .border_color(theme.border)
            .font_family(mono)
            .text_size(px(fonts.secondary))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .text_size(px(fonts.label))
                    .text_color(theme.muted)
                    .child("CONTACT")
                    .child(
                        style::icon_button(
                            "pilus-sipil-contact-close",
                            include_bytes!("../../assets/icons/x.svg"),
                            ButtonKind::Ghost,
                            theme,
                            18.0,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.sipil_chat.about_open = false;
                            this.sipil_chat.confirm_remove = None;
                            this.sipil_chat.renaming = false;
                            cx.notify();
                        })),
                    ),
            )
            .child(panel)
    }

    /// Start renaming the contact `id`: the name becomes a field, filled with
    /// the old one, and takes the keyboard.
    fn start_contact_rename(
        &mut self,
        id: &str,
        name: &str,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        let _ = id;
        self.sipil_chat.renaming = true;
        let input = self.sipil_chat.rename.clone();
        input.update(cx, |input, cx| input.set_value(name.to_string(), cx));
        window.focus(&input.focus_handle(cx), cx);
        cx.notify();
    }

    /// Keep the name typed for the contact `id`, if there is one.
    fn commit_contact_rename(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.sipil_chat.renaming {
            return;
        }
        let name = self.sipil_chat.rename.read(cx).value();
        self.sipil_chat.renaming = false;
        if !name.trim().is_empty() {
            social::rename_contact(id, name.trim());
        }
        cx.notify();
    }

    /// The channel, beside its log: its name and id, who is in it with their
    /// device IDs, and the contacts who could be. A member can be removed and a
    /// contact invited from here; every member hears of it.
    fn channel_panel(&self, fonts: style::Fonts, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let mono = style::mono_family(&self.shared_settings);
        let state = &self.sipil;
        let Some(channel) = state.watched_channel() else {
            return div();
        };
        let heading = |text: String| {
            div()
                .pt_2()
                .text_size(px(fonts.label))
                .text_color(theme.muted)
                .child(text.to_uppercase())
        };
        let id_line = |id: &str| {
            div()
                .text_size(px(fonts.label))
                .text_color(theme.muted)
                .child(id.to_string())
        };
        let mut panel = div()
            .id("pilus-sipil-channel-scroll")
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .gap_2()
            .px_3()
            .py_3()
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_size(px(fonts.body))
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme.fg)
                            .child(format!("#{}", channel.name)),
                    )
                    .child(id_line(&format!("channel {}", channel.id))),
            );

        // Only the owner changes who is in the channel.
        let mine = state.me.device_id.as_deref() == Some(channel.owner.as_str());
        // You first, then the others in the order they joined.
        let me = nick_of(state);
        panel = panel
            .child(heading(format!("Members · {}", channel.members.len() + 1)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(
                                div()
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(theme.fg)
                                    .child(me),
                            )
                            .child(div().text_color(theme.muted).child(if mine {
                                "you · owner"
                            } else {
                                "you"
                            })),
                    )
                    .children(state.me.device_id.as_deref().map(id_line)),
            );
        for member in &channel.members {
            let person = state
                .me
                .people
                .iter()
                .find(|person| person.contact.id == *member);
            let name = person.map_or_else(
                || member.chars().take(8).collect::<String>(),
                |person| person.contact.name.clone(),
            );
            let notes: Vec<(&str, gpui::Rgba)> = [
                (*member == channel.owner).then_some(("owner", theme.accent)),
                match person {
                    None => Some(("not a contact: listed but silent", theme.muted)),
                    Some(person) if person.reach == Reach::Online => Some(("online", theme.ok)),
                    Some(_) => None,
                },
            ]
            .into_iter()
            .flatten()
            .collect();
            let confirming = self.sipil_chat.confirm_remove.as_deref() == Some(member.as_str());
            let (channel_id, who) = (channel.id.clone(), member.clone());
            let remove = style::small_button(
                format!("pilus-sipil-channel-remove-{member}"),
                if confirming { "Confirm" } else { "Remove" },
                if confirming {
                    ButtonKind::Danger
                } else {
                    ButtonKind::Secondary
                },
                theme,
                fonts.label,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.sipil_chat.confirm_remove.as_deref() == Some(who.as_str()) {
                    this.sipil_chat.confirm_remove = None;
                    social::remove_from_channel(&channel_id, &who);
                } else {
                    this.sipil_chat.confirm_remove = Some(who.clone());
                }
                cx.notify();
            }));
            panel =
                panel.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_start()
                        .justify_between()
                        .gap_2()
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .gap_2()
                                        .child(
                                            div()
                                                .font_weight(FontWeight::BOLD)
                                                .text_color(nick_color(theme, &name))
                                                .child(name),
                                        )
                                        .children(notes.into_iter().map(|(text, color)| {
                                            div().text_color(color).child(text)
                                        })),
                                )
                                .child(id_line(member)),
                        )
                        .children(mine.then(|| div().flex_shrink_0().child(remove))),
                );
        }

        // The agents of this pilus's own daemons, invited one by one. Naming an
        // agent that is not here does nothing; inviting hands every member the
        // means to run it, so it asks once and removing is at once.
        let invited = state
            .me
            .channels
            .iter()
            .find(|row| row.channel.id == channel.id)
            .map(|row| row.agents.clone())
            .unwrap_or_default();
        let mine_here = state.mentionable();
        panel = panel.child(heading(format!("Your agents · {}", invited.len())));
        if mine_here.is_empty() {
            panel = panel.child(
                div()
                    .text_color(theme.muted)
                    .child("No sipil agent is connected."),
            );
        }
        for (daemon_id, agent) in mine_here {
            let here = invited.contains(&daemon_id);
            let confirming = self.sipil_chat.confirm_agent.as_deref() == Some(daemon_id.as_str());
            let (channel_id, daemon) = (channel.id.clone(), daemon_id.clone());
            let mut row = div().flex().flex_col().gap_2().child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(theme.fg)
                            .child(agent.clone()),
                    )
                    .child(
                        style::small_button(
                            format!("pilus-sipil-channel-agent-{daemon_id}"),
                            if here { "Remove" } else { "Invite" },
                            ButtonKind::Secondary,
                            theme,
                            fonts.label,
                        )
                        .on_click({
                            let (channel_id, daemon) = (channel_id.clone(), daemon.clone());
                            cx.listener(move |this, _, _, cx| {
                                if here {
                                    social::set_channel_agent(&channel_id, &daemon, false);
                                } else {
                                    this.sipil_chat.confirm_agent = Some(daemon.clone());
                                }
                                cx.notify();
                            })
                        }),
                    ),
            );
            if confirming && !here {
                row = row.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .p_2()
                        .radius_md()
                        .border_1()
                        .border_color(theme.alert)
                        .child(div().text_color(theme.fg).child(format!(
                            "Anyone in #{} can then ask {agent} something by naming it with \
                             @{agent}. It reads files and runs commands on the machine it runs \
                             on and uses its API keys, so invite it only into a channel of people \
                             you trust. You can remove it at any time.",
                            channel.name
                        )))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .gap_2()
                                .child(
                                    style::small_button(
                                        format!("pilus-sipil-channel-agent-yes-{daemon_id}"),
                                        "Invite",
                                        ButtonKind::Danger,
                                        theme,
                                        fonts.label,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            social::set_channel_agent(&channel_id, &daemon, true);
                                            this.sipil_chat.confirm_agent = None;
                                            cx.notify();
                                        },
                                    )),
                                )
                                .child(
                                    style::small_button(
                                        format!("pilus-sipil-channel-agent-no-{daemon_id}"),
                                        "Cancel",
                                        ButtonKind::Ghost,
                                        theme,
                                        fonts.label,
                                    )
                                    .on_click(cx.listener(
                                        |this, _, _, cx| {
                                            this.sipil_chat.confirm_agent = None;
                                            cx.notify();
                                        },
                                    )),
                                ),
                        ),
                );
            }
            panel = panel.child(row);
        }

        let candidates: Vec<_> = state
            .me
            .people
            .iter()
            .filter(|person| !channel.members.contains(&person.contact.id))
            .collect();
        if !mine {
            let owner = state
                .me
                .people
                .iter()
                .find(|person| person.contact.id == channel.owner)
                .map_or_else(
                    || channel.owner.chars().take(8).collect::<String>(),
                    |person| person.contact.name.clone(),
                );
            panel = panel.child(div().pt_2().text_color(theme.muted).child(format!(
                "Only {owner}, who owns this channel, can add or remove members."
            )));
        }
        let candidates = if mine { candidates } else { Vec::new() };
        if mine {
            panel = panel.child(heading("Invite a contact".to_string()));
        }
        if mine && candidates.is_empty() {
            panel = panel.child(
                div()
                    .text_color(theme.muted)
                    .child("Everyone you have added is in this channel."),
            );
        }
        for person in candidates {
            let (channel_id, who) = (channel.id.clone(), person.contact.id.clone());
            panel = panel.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(theme.fg)
                            .child(person.contact.name.clone()),
                    )
                    .child(
                        style::small_button(
                            format!("pilus-sipil-channel-invite-{}", person.contact.id),
                            "Invite",
                            ButtonKind::Secondary,
                            theme,
                            fonts.label,
                        )
                        .on_click(cx.listener(move |_, _, _, cx| {
                            social::invite_to_channel(&channel_id, &who);
                            cx.notify();
                        })),
                    ),
            );
        }
        let leave_id = channel.id.clone();
        panel = panel.child(
            div().pt_3().child(
                style::small_button(
                    "pilus-sipil-channel-leave",
                    "Leave channel",
                    ButtonKind::Danger,
                    theme,
                    fonts.label,
                )
                .on_click(cx.listener(move |_, _, _, cx| {
                    social::leave_channel(&leave_id);
                    cx.notify();
                })),
            ),
        );
        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w(px(SIDE_PANEL))
            .h_full()
            .border_l_1()
            .border_color(theme.border)
            .font_family(mono)
            .text_size(px(fonts.secondary))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .text_size(px(fonts.label))
                    .text_color(theme.muted)
                    .child("CHANNEL")
                    .child(
                        style::icon_button(
                            "pilus-sipil-channel-close",
                            include_bytes!("../../assets/icons/x.svg"),
                            ButtonKind::Ghost,
                            theme,
                            18.0,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.sipil_chat.about_open = false;
                            this.sipil_chat.confirm_remove = None;
                            this.sipil_chat.confirm_agent = None;
                            cx.notify();
                        })),
                    ),
            )
            .child(panel)
    }

    /// The daemon, as it describes itself: who it is, where it runs, which model
    /// it is on, and each agent with what it is told and given. The agents it
    /// hands work to are listed with the same detail as the primary one.
    fn about_panel(&self, fonts: style::Fonts, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let mono = style::mono_family(&self.shared_settings);
        let (Some(about), Some(daemon)) = (self.about_of_watched(), self.sipil.watched_daemon())
        else {
            return div();
        };
        let heading = |text: &str| {
            div()
                .pt_2()
                .text_size(px(fonts.label))
                .text_color(theme.muted)
                .child(text.to_uppercase())
        };
        let field = |label: &str, value: String| {
            div()
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_size(px(fonts.label))
                        .text_color(theme.muted)
                        .child(label.to_uppercase()),
                )
                .child(div().text_color(theme.fg).child(value))
        };
        let name = if about.name.is_empty() {
            daemon.name()
        } else {
            about.name.clone()
        };
        let mut panel = div()
            .id("pilus-sipil-about-scroll")
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .gap_2()
            .px_3()
            .py_3()
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .child(daemon_mark(daemon, 48.0, theme))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .min_w_0()
                            .child(
                                div()
                                    .text_size(px(fonts.body))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(theme.fg)
                                    .child(name),
                            )
                            .child(
                                div()
                                    .text_color(theme.muted)
                                    .child(format!("sipil {}", about.version)),
                            ),
                    ),
            );
        let host = [about.hostname.as_str(), about.platform.as_str()]
            .iter()
            .filter(|part| !part.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" · ");
        for (label, value) in [
            ("Host", host),
            ("Workspace", about.workspace.clone()),
            ("Data", about.home.clone()),
            ("Device", about.device_id.clone()),
        ] {
            if !value.is_empty() {
                panel = panel.child(field(label, value));
            }
        }
        panel = panel.child(field(
            "Model",
            about.model.as_ref().map_or_else(
                || "none: no provider key is set".to_string(),
                |model| format!("{} ({})", model.name, model.provider_name),
            ),
        ));

        panel = panel.child(heading("Agents"));
        for agent in &about.agents {
            let open = self.sipil_chat.open_agents.contains(&agent.id);
            let toggle = {
                let id = agent.id.clone();
                cx.listener(move |this, _, _, cx| {
                    if !this.sipil_chat.open_agents.remove(&id) {
                        this.sipil_chat.open_agents.insert(id.clone());
                    }
                    cx.notify();
                })
            };
            let mut card = div()
                .flex()
                .flex_col()
                .gap_1()
                .p_2()
                .border_1()
                .border_color(theme.border)
                .radius_md()
                .child(
                    div().flex().flex_row().items_center().gap_2().child(
                        div()
                            .font_weight(FontWeight::BOLD)
                            .text_color(nick_color(theme, &agent.name))
                            .child(agent.name.clone()),
                    ),
                );
            if !agent.summary.is_empty() {
                card = card.child(div().text_color(theme.muted).child(agent.summary.clone()));
            }
            if !agent.tools.is_empty() {
                card = card.child(
                    div().flex().flex_row().flex_wrap().gap_1().children(
                        agent
                            .tools
                            .iter()
                            .map(|tool| style::chip(theme, tool.clone(), fonts.label)),
                    ),
                );
            }
            if !agent.instructions.is_empty() {
                card = card.child(
                    div()
                        .id(format!("pilus-sipil-agent-instructions-{}", agent.id))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .cursor_pointer()
                        .text_color(theme.accent)
                        .hover(|style| style.text_color(theme.fg))
                        .on_click(toggle)
                        .child(icon(
                            if open {
                                include_bytes!("../../assets/icons/chevron-down.svg")
                            } else {
                                include_bytes!("../../assets/icons/chevron-right.svg")
                            },
                            12.0,
                            theme.muted,
                        ))
                        .child("Instructions"),
                );
                if open {
                    card = card.child(
                        div()
                            .font_family(mono.clone())
                            .text_size(px(fonts.label))
                            .text_color(theme.muted)
                            .child(clip_lines(&agent.instructions, TOOL_LINES)),
                    );
                }
            }
            panel = panel.child(card);
        }

        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w(px(SIDE_PANEL))
            .h_full()
            .border_l_1()
            .border_color(theme.border)
            .font_family(mono)
            .text_size(px(fonts.secondary))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .text_size(px(fonts.label))
                    .text_color(theme.muted)
                    .child("ABOUT")
                    .child(
                        style::icon_button(
                            "pilus-sipil-about-close",
                            include_bytes!("../../assets/icons/x.svg"),
                            ButtonKind::Ghost,
                            theme,
                            18.0,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.sipil_chat.about_open = false;
                            this.sipil_chat.confirm_agent = None;
                            cx.notify();
                        })),
                    ),
            )
            .child(panel)
    }

    /// A thread, beside the log: the line, and the replies to it, which are
    /// drawn here and not in the log. A message sent while it is open is one
    /// more reply.
    fn thread_panel(
        &self,
        root: usize,
        fonts: style::Fonts,
        settings: &Rc<RefCell<crate::settings::Settings>>,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = self.theme;
        let mono = style::mono_family(&self.shared_settings);
        let built = &self.sipil_chat.built;
        let items = &self.sipil.transcript;
        let daemon = matches!(self.sipil.watch, Watch::On(..));
        let known = known_calls(items);
        let widths = self.line_widths(fonts);
        let members = built.threads.get(&root).map_or(&[][..], Vec::as_slice);
        let mut lines = div()
            .id("pilus-sipil-thread-scroll")
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .px_2()
            .py_2()
            .gap(px(2.0))
            .overflow_y_scroll()
            .track_scroll(&self.sipil_chat.thread_scroll);
        for &at in std::iter::once(&root).chain(members) {
            let Some(item) = items.get(at) else {
                continue;
            };
            // A result is drawn under its call.
            if item.role == ItemRole::Tool
                && item.call_id.as_deref().is_some_and(|id| known.contains(id))
            {
                continue;
            }
            // Under the line it answers, a reply need not quote it again.
            let shown = match reply_of(item, daemon) {
                Some((_, body)) if at != root => TranscriptItem {
                    reply: None,
                    text: body,
                    ..item.clone()
                },
                _ => item.clone(),
            };
            lines = lines.child(
                div()
                    .font_family(mono.clone())
                    .text_size(px(fonts.secondary))
                    .child(self.irc_line(
                        &shown,
                        (&built.me, &built.speaker),
                        widths,
                        fonts,
                        settings,
                        built.working,
                        true,
                        cx,
                    )),
            );
        }
        let count = members
            .iter()
            .filter_map(|&at| items.get(at))
            .filter(|reply| reply.role != ItemRole::Tool && !reply.text.is_empty())
            .count();
        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w(px(SIDE_PANEL))
            .h_full()
            .border_l_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .font_family(mono)
                    .text_size(px(fonts.label))
                    .text_color(theme.muted)
                    .child(format!(
                        "THREAD · {count} {}",
                        if count == 1 { "REPLY" } else { "REPLIES" }
                    ))
                    .child(
                        style::icon_button(
                            "pilus-sipil-thread-close",
                            include_bytes!("../../assets/icons/x.svg"),
                            ButtonKind::Ghost,
                            theme,
                            18.0,
                        )
                        .tooltip(row_tooltip(
                            "Close the thread".to_string(),
                            theme.panel_fg,
                            Vec::new(),
                            theme,
                        ))
                        .on_click(cx.listener(|this, _, _, cx| this.close_thread(cx))),
                    ),
            )
            .child(lines)
            .child(self.thread_composer(fonts, cx))
    }

    /// The field a reply is typed in, under the thread it answers.
    fn thread_composer(&self, fonts: style::Fonts, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let input = self.sipil_chat.thread_input.clone();
        input.update(cx, |input, _| {
            input.set_style(
                theme,
                gpui::font(style::mono_family(&self.shared_settings)),
                px(fonts.body),
            );
        });
        div()
            .flex_shrink_0()
            .px_3()
            .py_2()
            .border_t_1()
            .border_color(theme.border)
            .font_family(style::mono_family(&self.shared_settings))
            .text_size(px(fonts.body))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.sipil_chat.in_thread = true;
                    cx.notify();
                }),
            )
            .child(input)
    }

    /// One row of the log, set in the terminal's font at the width the
    /// conversation's longest nick sets.
    fn sipil_chat_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let built = &self.sipil_chat.built;
        let Some((row, item)) = built
            .rows
            .get(ix)
            .and_then(|row| Some((row, self.sipil.transcript.get(row.at)?)))
        else {
            return div().into_any_element();
        };
        let Some(settings) = self.sipil_chat.log_settings.borrow().clone() else {
            return div().into_any_element();
        };
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let mono = style::mono_family(&self.shared_settings);
        let (time_width, nick_width) = self.line_widths(fonts);
        let last = ix + 1 == built.rows.len();
        let theme = self.theme;
        let replies = built.threads.get(&row.at).map_or(0, |members| {
            members
                .iter()
                .filter_map(|&at| self.sipil.transcript.get(at))
                .filter(|reply| reply.role != ItemRole::Tool && !reply.text.is_empty())
                .count()
        });
        let chip = (replies > 0).then(|| {
            let id = item.id;
            let open = self.sipil_chat.thread == Some(id);
            div()
                .id(format!("pilus-sipil-thread-{id}"))
                .ml(px(time_width + nick_width + 20.0))
                .cursor_pointer()
                .text_color(theme.accent)
                .when(open, |chip| chip.font_weight(FontWeight::BOLD))
                .hover(|style| style.text_color(theme.fg))
                .child(format!(
                    "{replies} {}",
                    if replies == 1 { "reply" } else { "replies" }
                ))
                .on_click(cx.listener(move |this, _, _, cx| this.open_thread(id, cx)))
        });
        div()
            .w_full()
            .px_2()
            .flex()
            .flex_col()
            .pt(px(if ix == 0 { 8.0 } else { 1.0 }))
            .pb(px(if last { 8.0 } else { 1.0 }))
            .font_family(mono)
            .text_size(px(fonts.secondary))
            .child(self.irc_line(
                item,
                (&built.me, &built.speaker),
                (time_width, nick_width),
                fonts,
                &settings,
                built.working,
                false,
                cx,
            ))
            .children(chip)
            .into_any_element()
    }

    /// The widths of the time and nick columns of a line, in pixels.
    fn line_widths(&self, fonts: style::Fonts) -> (f32, f32) {
        // Nicks right-align in a column as wide as the longest in view, so the
        // text starts at one edge the way an IRC client sets it.
        let char_width = fonts.secondary * 0.62;
        (
            char_width * 5.0,
            char_width * (self.sipil_chat.built.nick_columns as f32 + 1.0),
        )
    }

    /// One line of the log: `HH:MM nick: text`, with tool calls as folding `*`
    /// actions and a failed turn as `!!`. The text is Markdown. A tool's result
    /// is drawn under its call, so a result whose call is in view has no line.
    #[allow(clippy::too_many_arguments)]
    fn irc_line(
        &self,
        item: &TranscriptItem,
        names: (&str, &str),
        widths: (f32, f32),
        fonts: style::Fonts,
        settings: &Rc<RefCell<crate::settings::Settings>>,
        working: bool,
        stacked: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let (me, speaker) = names;
        let (time_width, nick_width) = widths;
        let theme = self.theme;
        let time = item
            .at
            .map(pilus_client::sipil::clock_label)
            .unwrap_or_default();
        let group = format!("pilus-sipil-line-{}", item.id);
        let item_id = item.id;
        // What a reply to this line would quote, when it is something said.
        let said = matches!(
            item.role,
            ItemRole::User | ItemRole::Peer | ItemRole::Assistant
        ) && item.error.is_none()
            && !item.text.trim().is_empty();
        let quote = said.then(|| {
            Quote::of(
                line_nick(item, me, speaker).trim_end_matches(':'),
                &item.text,
            )
        });
        let reply = quote.map(|quote| {
            style::icon_button(
                format!("pilus-sipil-reply-{}", item.id),
                include_bytes!("../../assets/icons/undo-2.svg"),
                ButtonKind::Ghost,
                theme,
                18.0,
            )
            .self_end()
            .opacity(0.0)
            .group_hover(group.clone(), |style| style.opacity(1.0))
            .tooltip(row_tooltip(
                "Reply".to_string(),
                theme.panel_fg,
                Vec::new(),
                theme,
            ))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.begin_reply(item_id, quote.clone(), cx);
            }))
        });
        let row = move |nick: String, nick_color: gpui::Rgba, text: AnyElement| {
            // In a narrow panel the sender goes over the message, which then has
            // the whole width.
            if stacked {
                return div()
                    .radius_md()
                    .id(group.clone())
                    .group(group.clone())
                    .flex()
                    .flex_col()
                    .px_1()
                    .hover(|style| style.bg(theme.panel_dim))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_baseline()
                            .gap_2()
                            .child(
                                div()
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(nick_color)
                                    .child(nick),
                            )
                            .child(div().text_color(theme.muted).child(time.clone())),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_start()
                            .gap_2()
                            .child(div().flex_1().min_w_0().child(text))
                            .children(reply),
                    );
            }
            div()
                .radius_md()
                .id(group.clone())
                .group(group.clone())
                .flex()
                .flex_row()
                .items_start()
                .gap_2()
                .px_1()
                .hover(|style| style.bg(theme.panel_dim))
                .child(
                    div()
                        .w(px(time_width))
                        .flex_shrink_0()
                        .text_color(theme.muted)
                        .child(time.clone()),
                )
                .child(
                    div()
                        .w(px(nick_width))
                        .flex_shrink_0()
                        .flex()
                        .justify_end()
                        .font_weight(FontWeight::BOLD)
                        .text_color(nick_color)
                        .child(nick),
                )
                .child(div().flex_1().min_w_0().child(text))
                .children(reply)
        };
        let plain = |text: &str, color: gpui::Rgba| {
            div()
                .text_color(color)
                .child(text.to_string())
                .into_any_element()
        };
        match item.role {
            ItemRole::User | ItemRole::Peer => {
                let nick = line_nick(item, me, speaker);
                let color = match (&item.speaker, item.role) {
                    (None, ItemRole::User) => theme.fg,
                    (Some(name), _) => nick_color(theme, name),
                    (None, _) => nick_color(theme, speaker),
                };
                let mut block = div().flex().flex_col();
                if let Some(quoted) = &item.reply {
                    block = block.child(self.quote_strip(quoted, fonts));
                }
                row(
                    nick,
                    color,
                    block
                        .child(self.chat_text(&item.text, theme.fg, fonts, settings))
                        .into_any_element(),
                )
            }
            ItemRole::Assistant => {
                if item.error.is_some() {
                    return row(
                        "!!".to_string(),
                        theme.alert,
                        plain(&item.text, theme.alert),
                    );
                }
                let name = item.speaker.as_deref().unwrap_or(speaker);
                let mut block = div().flex().flex_col().gap(px(2.0));
                if item.calls.is_empty() && !item.tools.is_empty() {
                    // A daemon that does not send the calls themselves.
                    block = block.child(
                        div()
                            .text_color(theme.muted)
                            .child(format!("used {}", item.tools.join(", "))),
                    );
                }
                if !item.text.is_empty() {
                    block = block.child(self.chat_text(&item.text, theme.fg, fonts, settings));
                }
                for call in &item.calls {
                    let result = self
                        .sipil_chat
                        .built
                        .results
                        .get(&call.id)
                        .and_then(|&at| self.sipil.transcript.get(at));
                    block = block.child(self.call_block(
                        call,
                        (result, working && result.is_none()),
                        output_of(&self.sipil.running, &call.id),
                        fonts,
                        cx,
                    ));
                }
                let (nick, color) = if item.text.is_empty() {
                    ("*".to_string(), theme.muted)
                } else {
                    (format!("{name}:"), nick_color(theme, name))
                };
                row(nick, color, block.into_any_element())
            }
            ItemRole::Tool => row(
                "-".to_string(),
                theme.muted,
                plain(&clip_lines(&item.text, TOOL_LINES), theme.muted),
            ),
        }
    }

    /// A call, folded to one line. Opened, it shows what it was given and what
    /// came back.
    fn call_block(
        &self,
        call: &ToolCall,
        who: (Option<&TranscriptItem>, bool),
        printed: Option<&str>,
        fonts: style::Fonts,
        cx: &mut Context<Self>,
    ) -> Div {
        let (result, running) = who;
        let theme = self.theme;
        let open = self.sipil_chat.open_calls.contains(&call.id);
        let failed = result.is_some_and(|result| result.is_error);
        let toggle = {
            let id = call.id.clone();
            cx.listener(move |this, _, _, cx| this.toggle_sipil_call(&id, cx))
        };

        let chevron = icon(
            if open {
                include_bytes!("../../assets/icons/chevron-down.svg")
            } else {
                include_bytes!("../../assets/icons/chevron-right.svg")
            },
            12.0,
            theme.muted,
        );
        let mut head = div()
            .id(format!("pilus-sipil-call-{}", call.id))
            .flex()
            .flex_row()
            .items_start()
            .gap_1()
            .cursor_pointer()
            .on_click(toggle)
            .child(div().pt(px(2.0)).child(chevron));
        head = head.child(
            div()
                .min_w_0()
                .flex()
                .flex_row()
                .gap_2()
                .text_color(theme.muted)
                .child(div().flex_shrink_0().child(call.name.clone()))
                .child(div().min_w_0().truncate().child(call.summary.clone())),
        );
        if failed {
            head = head.child(div().text_color(theme.alert).child("failed"));
        } else if running {
            // The last thing it printed, so a long command shows it is alive.
            let latest = printed.and_then(last_line);
            head = head.child(
                div()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .text_color(theme.muted)
                    .child(div().flex_shrink_0().child("…"))
                    .children(
                        latest.map(|line| div().min_w_0().truncate().child(line.to_string())),
                    ),
            );
        }
        let mut block = div().flex().flex_col().child(head);
        if !open {
            return block;
        }

        let section = |label: &str, body: AnyElement| {
            div()
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_size(px(fonts.label))
                        .text_color(theme.muted)
                        .child(label.to_uppercase()),
                )
                .child(body)
        };
        let mut body = div()
            .flex()
            .flex_col()
            .gap_1()
            .ml(px(5.0))
            .pl_3()
            .border_l_1()
            .border_color(theme.border);
        if !call.args.is_empty() {
            body = body.child(section(
                "arguments",
                div()
                    .text_color(theme.muted)
                    .child(clip_lines(&call.args, TOOL_LINES))
                    .into_any_element(),
            ));
        }
        match result {
            Some(result) => {
                body = body.child(section(
                    if failed { "error" } else { "result" },
                    div()
                        .text_color(if failed { theme.alert } else { theme.muted })
                        .child(clip_lines(&result.text, TOOL_LINES))
                        .into_any_element(),
                ));
            }
            None if running => {
                body = body.child(match printed.filter(|text| !text.trim().is_empty()) {
                    Some(text) => section(
                        "output",
                        div()
                            .text_color(theme.muted)
                            .child(tail_lines(text, TOOL_LINES))
                            .into_any_element(),
                    ),
                    None => div().text_color(theme.muted).child("running…"),
                });
            }
            None => {}
        }
        block = block.child(body);
        block
    }

    /// The line a reply answers, above what was said: who said it and the start of it.
    fn quote_strip(&self, quote: &Quote, fonts: style::Fonts) -> Div {
        let theme = self.theme;
        div()
            .flex()
            .flex_row()
            .gap_2()
            .min_w_0()
            .pl_2()
            .border_l_2()
            .border_color(theme.border)
            .text_size(px(fonts.label))
            .text_color(theme.muted)
            .child(
                div()
                    .flex_shrink_0()
                    .font_weight(FontWeight::BOLD)
                    .text_color(nick_color(theme, &quote.who))
                    .child(format!("{}:", quote.who)),
            )
            .child(div().min_w_0().truncate().child(quote.text.clone()))
    }

    /// A message's text. Plain talk is set plain; anything that reads as
    /// Markdown (emphasis, code, lists, headings, links, quotes, tables, more
    /// than one paragraph) is parsed and drawn as a document, once per text.
    pub(super) fn chat_text(
        &self,
        text: &str,
        color: gpui::Rgba,
        fonts: style::Fonts,
        settings: &Rc<RefCell<crate::settings::Settings>>,
    ) -> AnyElement {
        if !looks_like_markdown(text) {
            return div()
                .text_color(color)
                .child(text.to_string())
                .into_any_element();
        }
        let document = {
            let mut docs = self.sipil_chat.docs.borrow_mut();
            // A long conversation does not grow the cache without bound.
            if docs.len() > 400 {
                docs.clear();
            }
            Rc::clone(
                docs.entry(text.to_string())
                    .or_insert_with(|| Rc::new(Document::parse(text, false))),
            )
        };
        let unit = fonts.secondary / 13.0;
        let blocks = document.blocks();
        let mut column = div().flex().flex_col().w_full();
        for (index, block) in blocks.iter().enumerate() {
            let (before, after) = block.spacing();
            let last = index + 1 == blocks.len();
            let leaves = Leaves::inert(index);
            column = column.child(
                div()
                    .w_full()
                    .pt(px(before * unit * 0.5))
                    .pb(px(if last { 0.0 } else { after * unit * 0.5 }))
                    .child(markdown::render_block(
                        block,
                        self.theme,
                        // Chat sits a size under body text, like the rest of the log.
                        style::Fonts {
                            body: fonts.secondary,
                            ..fonts
                        },
                        settings,
                        &leaves,
                    )),
            );
        }
        column.into_any_element()
    }

    /// The input line, under the quote of the line it answers when there is one.
    fn irc_input(&self, fonts: style::Fonts, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let mono = style::mono_family(&self.shared_settings);
        let input = self.sipil_chat.input.clone();
        input.update(cx, |input, _| {
            input.set_style(
                theme,
                gpui::font(style::mono_family(&self.shared_settings)),
                px(fonts.body),
            );
        });
        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.sipil_chat.in_thread = false;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .px_3()
                    .py_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .font_family(mono)
                    .text_size(px(fonts.body))
                    .child(input),
            )
    }
}

/// A daemon's mark at `size` pixels: its own logo, or the pilus mark in the
/// colour of the icons around it, with nothing behind either.
fn daemon_mark(daemon: &sipil::Daemon, size: f32, theme: Theme) -> AnyElement {
    match daemon.own_logo() {
        Some(logo) => img(logo)
            .w(px(size))
            .h(px(size))
            .flex_shrink_0()
            .into_any_element(),
        None => div()
            .flex_shrink_0()
            .child(icon(
                include_bytes!("../../assets/icons/pilus-mark.svg"),
                size,
                theme.muted,
            ))
            .into_any_element(),
    }
}

/// The ids of the calls in view. A result whose call is among them is drawn
/// under it and has no row of its own.
/// How many recently closed items the work panel lists.
const CLOSED_SHOWN: usize = 5;

/// The time now, in Unix milliseconds, as the daemon writes times.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// How long ago `at` was, shortly: "just now", "5m ago", "3h ago", "2d ago".
fn ago(at: u64, now: u64) -> String {
    let minutes = now.saturating_sub(at) / 60_000;
    match minutes {
        0 => "just now".to_string(),
        1..60 => format!("{minutes}m ago"),
        60..1440 => format!("{}h ago", minutes / 60),
        _ => format!("{}d ago", minutes / 1440),
    }
}

/// How far off `at` is, shortly: "in 5m", "in 3h", "in 2d"; "now" once due.
fn until(at: u64, now: u64) -> String {
    let minutes = at.saturating_sub(now).div_ceil(60_000);
    match minutes {
        0 => "now".to_string(),
        1..60 => format!("in {minutes}m"),
        60..1440 => format!("in {}h", minutes / 60),
        _ => format!("in {}d", minutes / 1440),
    }
}

/// The work button's tooltip: how much is open, and how much of it waits on
/// someone.
fn work_summary(board: &WorkBoard, routines: usize) -> String {
    let stuck = board.open.iter().filter(|item| item.stuck()).count();
    let open = match (board.open.len(), stuck) {
        (0, _) => "No open work".to_string(),
        (n, 0) => format!("Open work: {n}"),
        (n, stuck) => format!("Open work: {n}, {stuck} waiting on someone"),
    };
    match routines {
        0 => open,
        1 => format!("{open} · 1 routine"),
        n => format!("{open} · {n} routines"),
    }
}

fn known_calls(items: &[TranscriptItem]) -> HashSet<&str> {
    items
        .iter()
        .flat_map(|item| item.calls.iter())
        .map(|call| call.id.as_str())
        .collect()
}

/// A digest of what a row shows, so the list re-measures the lines whose
/// content changed and keeps the measure of the rest: the line itself, and
/// for each call it made, the result drawn under it.
fn signature(
    item: &TranscriptItem,
    items: &[TranscriptItem],
    results: &HashMap<String, usize>,
    working: bool,
    replies: usize,
    running: &[RunningTool],
) -> u64 {
    let mut hasher = DefaultHasher::new();
    replies.hash(&mut hasher);
    item.id.hash(&mut hasher);
    item.text.hash(&mut hasher);
    item.error.hash(&mut hasher);
    item.speaker.hash(&mut hasher);
    item.reply.is_some().hash(&mut hasher);
    item.tools.hash(&mut hasher);
    for call in &item.calls {
        call.id.hash(&mut hasher);
        call.summary.hash(&mut hasher);
        match results.get(&call.id).and_then(|&at| items.get(at)) {
            Some(result) => {
                result.is_error.hash(&mut hasher);
                result.text.hash(&mut hasher);
            }
            // A call still running shows a mark while the conversation works,
            // and the end of what it has printed.
            None => {
                working.hash(&mut hasher);
                output_of(running, &call.id).hash(&mut hasher);
            }
        }
    }
    hasher.finish()
}

/// What `item` answers and what it says, when it is a reply. A conversation
/// with a daemon has no field for a quote, so there it is the Markdown block
/// [`Quote::prefix`] put in front of the message.
fn reply_of(item: &TranscriptItem, daemon: bool) -> Option<(Quote, String)> {
    if let Some(quote) = &item.reply {
        return Some((quote.clone(), item.text.clone()));
    }
    if !daemon || item.role != ItemRole::User {
        return None;
    }
    let (head, body) = item.text.strip_prefix("> ")?.split_once("\n\n")?;
    let (who, text) = head.split_once(": ")?;
    (!text.is_empty() && !head.contains('\n')).then(|| {
        (
            Quote {
                who: who.to_string(),
                text: text.to_string(),
            },
            body.to_string(),
        )
    })
}

/// The threads of a conversation: a reply is found under the line it quotes,
/// the nearest one before it that says what the quote does, and a reply to a
/// reply joins the thread it is in. With a daemon, which has one conversation,
/// the lines the agent answers with follow their reply into its thread.
/// Returns what each line holds, and the line each reply belongs to, by place.
fn group_threads(
    items: &[TranscriptItem],
    daemon: bool,
) -> (HashMap<usize, Vec<usize>>, HashMap<usize, usize>) {
    let said = |item: &TranscriptItem| {
        matches!(
            item.role,
            ItemRole::User | ItemRole::Peer | ItemRole::Assistant
        ) && item.error.is_none()
            && !item.text.trim().is_empty()
    };
    let body = |at: usize| {
        reply_of(&items[at], daemon).map_or_else(|| items[at].text.clone(), |(_, body)| body)
    };
    let mut threads: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut root_of: HashMap<usize, usize> = HashMap::new();
    let mut current: Option<usize> = None;
    for (at, item) in items.iter().enumerate() {
        let reply = matches!(item.role, ItemRole::User | ItemRole::Peer)
            .then(|| reply_of(item, daemon))
            .flatten();
        let root = if let Some((quote, _)) = reply {
            let found = (0..at)
                .rev()
                .find(|&earlier| {
                    said(&items[earlier]) && Quote::of("", &body(earlier)).text == quote.text
                })
                .map(|earlier| root_of.get(&earlier).copied().unwrap_or(earlier));
            current = if daemon { found } else { None };
            found
        } else if daemon && item.role == ItemRole::User {
            current = None;
            None
        } else if daemon {
            current
        } else {
            None
        };
        if let Some(root) = root {
            threads.entry(root).or_default().push(at);
            root_of.insert(at, root);
        }
    }
    (threads, root_of)
}

/// The first `limit` lines of `text`, with a mark where the rest was cut.
/// What a running call has printed, by the call's id.
fn output_of<'a>(running: &'a [RunningTool], call: &str) -> Option<&'a str> {
    running
        .iter()
        .find(|tool| tool.call_id == call)
        .map(|tool| tool.output.as_str())
}

/// The last line with anything on it.
fn last_line(text: &str) -> Option<&str> {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
}

/// The last `limit` lines, with a mark where earlier ones were cut: what a
/// running call has printed is read from its end.
fn tail_lines(text: &str, limit: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= limit {
        return text.trim_end().to_string();
    }
    format!("…\n{}", lines[lines.len() - limit..].join("\n"))
}

fn clip_lines(text: &str, limit: usize) -> String {
    let mut lines = text.lines();
    let mut kept: Vec<&str> = lines.by_ref().take(limit).collect();
    if lines.next().is_some() {
        kept.push("…");
    }
    kept.join("\n")
}

/// Who a line is from, as the nick column shows it: `name:`, or a marker for a
/// tool call (`*`), a tool result (`-`) or a failed turn (`!!`).
fn line_nick(item: &TranscriptItem, me: &str, speaker: &str) -> String {
    match item.role {
        ItemRole::User => format!("{}:", item.speaker.as_deref().unwrap_or(me)),
        ItemRole::Peer => format!("{}:", item.speaker.as_deref().unwrap_or(speaker)),
        ItemRole::Assistant if item.error.is_some() => "!!".to_string(),
        ItemRole::Assistant if item.text.is_empty() => "*".to_string(),
        ItemRole::Assistant => format!("{}:", item.speaker.as_deref().unwrap_or(speaker)),
        ItemRole::Tool => "-".to_string(),
    }
}

/// What this device is called in the log.
fn nick_of(state: &SipilState) -> String {
    if state.me.nickname.is_empty() {
        "you".to_string()
    } else {
        state.me.nickname.clone()
    }
}

/// Whether `text` is worth parsing as Markdown: it has an emphasis, code, link,
/// heading, quote, table or list mark, or more than one line.
fn looks_like_markdown(text: &str) -> bool {
    if text.contains(['*', '_', '`', '[', '|', '~', '\n']) {
        return true;
    }
    text.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with('#')
            || line.starts_with('>')
            || line.starts_with("- ")
            || line.starts_with("+ ")
            || line.split_once(". ").is_some_and(|(number, _)| {
                !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
            })
    })
}

/// A nick's color: one of the theme's ANSI hues, chosen by the name, so a
/// speaker keeps the same color from line to line and from run to run.
fn nick_color(theme: Theme, name: &str) -> gpui::Rgba {
    // The normal and bright hues, without black and white, which read as
    // text or as nothing.
    const HUES: [usize; 12] = [1, 2, 3, 4, 5, 6, 9, 10, 11, 12, 13, 14];
    let sum = name.bytes().fold(7usize, |sum, byte| {
        sum.wrapping_mul(31).wrapping_add(usize::from(byte))
    });
    theme.ansi[HUES[sum % HUES.len()]]
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilus_client::sipil::{Agent, ConversationInfo, ConversationRole, Role, Session, ToolCall};

    use super::super::sipil::{ChannelRow, Daemon, Me, Phase};

    fn agent(id: &str, name: &str) -> Agent {
        Agent {
            id: id.to_string(),
            name: name.to_string(),
            role: Role::Primary,
            status: Activity::Idle,
            model: None,
            error: None,
        }
    }

    /// A state with one connected daemon `d`, and `watch` on it.
    fn on(daemon: Daemon, watch: Watch) -> SipilState {
        SipilState {
            daemons: vec![Daemon {
                id: "d".to_string(),
                phase: Phase::Connected,
                ..daemon
            }],
            watch,
            ..SipilState::default()
        }
    }

    #[test]
    fn times_on_the_board_read_shortly_both_ways() {
        let now = 10 * 86_400_000;
        assert_eq!(ago(now, now), "just now");
        assert_eq!(ago(now - 5 * 60_000, now), "5m ago");
        assert_eq!(ago(now - 3 * 3_600_000, now), "3h ago");
        assert_eq!(ago(now - 2 * 86_400_000, now), "2d ago");
        assert_eq!(until(now, now), "now");
        assert_eq!(until(now + 90_000, now), "in 2m");
        assert_eq!(until(now + 3 * 3_600_000, now), "in 3h");
        assert_eq!(until(now + 2 * 86_400_000, now), "in 2d");
    }

    #[test]
    fn the_work_button_says_what_waits_on_someone() {
        let item = |state: &str| pilus_client::sipil::WorkItem {
            state: state.to_string(),
            ..Default::default()
        };
        let board = |states: &[&str]| WorkBoard {
            open: states.iter().map(|s| item(s)).collect(),
            closed: Vec::new(),
        };
        assert_eq!(work_summary(&board(&[]), 0), "No open work");
        assert_eq!(
            work_summary(&board(&["doing", "todo"]), 1),
            "Open work: 2 · 1 routine"
        );
        assert_eq!(
            work_summary(&board(&["blocked", "waiting", "doing"]), 3),
            "Open work: 3, 2 waiting on someone · 3 routines"
        );
    }

    #[test]
    fn a_short_result_is_shown_whole_and_a_long_one_is_cut() {
        assert_eq!(clip_lines("a\nb", 8), "a\nb");
        assert_eq!(clip_lines("1\n2\n3\n4", 2), "1\n2\n…");
        assert_eq!(clip_lines("", 8), "");
    }

    #[test]
    fn an_agent_goes_by_its_own_name() {
        let daemon = Daemon {
            agents: vec![agent("1", "Researcher"), agent("2", "Chief")],
            ..Daemon::default()
        };
        let state = on(daemon, Watch::On("d".to_string(), "2".to_string()));
        let subject = Subject::of(&state);
        assert_eq!(subject.title, "Chief");
        assert_eq!(subject.speaker, "Chief");
    }

    fn info(id: &str) -> ConversationInfo {
        ConversationInfo {
            id: id.to_string(),
            name: format!("conversation {id}"),
            role: ConversationRole::Session,
            status: Activity::Working,
            model: None,
            profile: None,
            error: None,
        }
    }

    #[test]
    fn a_conversation_neither_list_carries_is_named_by_what_the_daemon_says_it_is() {
        let state = SipilState {
            watched: Some(info("12")),
            ..on(
                Daemon::default(),
                Watch::On("d".to_string(), "12".to_string()),
            )
        };
        let subject = Subject::of(&state);
        assert_eq!(subject.title, "conversation 12");
        assert_eq!(subject.activity, Activity::Working);

        // One for another conversation (the page has since moved on) is not used.
        let stale = SipilState {
            watch: Watch::On("d".to_string(), "13".to_string()),
            ..state
        };
        assert_eq!(Subject::of(&stale).title, "New session");
    }

    #[test]
    fn a_result_belongs_under_its_call() {
        let item = |id, role, call: Option<&str>, calls: &[&str]| TranscriptItem {
            id,
            role,
            text: String::new(),
            tools: Vec::new(),
            calls: calls
                .iter()
                .map(|id| ToolCall {
                    id: id.to_string(),
                    ..ToolCall::default()
                })
                .collect(),
            call_id: call.map(str::to_string),
            is_error: false,
            reply: None,
            error: None,
            at: None,
            speaker: None,
        };
        let items = vec![
            item(1, ItemRole::Assistant, None, &["a", "b"]),
            item(2, ItemRole::Tool, Some("a"), &[]),
            item(3, ItemRole::Tool, Some("cut-off"), &[]),
        ];
        let known = known_calls(&items);
        assert!(known.contains("a") && known.contains("b"));
        assert!(
            !known.contains("cut-off"),
            "a result whose call is out of view keeps its own line"
        );
    }

    fn line(id: u64, role: ItemRole, text: &str) -> TranscriptItem {
        TranscriptItem {
            id,
            role,
            text: text.to_string(),
            tools: Vec::new(),
            calls: Vec::new(),
            call_id: None,
            is_error: false,
            reply: None,
            error: None,
            at: None,
            speaker: None,
        }
    }

    fn asking(id: u64, call: &str) -> TranscriptItem {
        TranscriptItem {
            calls: vec![ToolCall {
                id: call.to_string(),
                name: "bash".to_string(),
                ..ToolCall::default()
            }],
            ..line(id, ItemRole::Assistant, "")
        }
    }

    fn answer(id: u64, call: &str, is_error: bool) -> TranscriptItem {
        TranscriptItem {
            call_id: Some(call.to_string()),
            is_error,
            ..line(id, ItemRole::Tool, "done")
        }
    }

    #[test]
    fn a_row_is_re_measured_only_when_what_it_shows_changed() {
        let items = vec![asking(1, "a"), answer(2, "a", false)];
        let none = HashMap::new();
        let results: HashMap<String, usize> = [("a".to_string(), 1)].into();
        let call = &items[0];
        let waiting = signature(call, &items, &none, false, 0, &[]);
        let answered = signature(call, &items, &results, false, 0, &[]);
        assert_ne!(
            waiting, answered,
            "a result arriving changes its call's row"
        );
        assert_eq!(answered, signature(call, &items, &results, true, 0, &[]));
        assert_ne!(
            waiting,
            signature(call, &items, &none, true, 0, &[]),
            "a call still running shows a mark while the conversation works"
        );
        let said = line(3, ItemRole::Assistant, "hello");
        assert_eq!(
            signature(&said, &items, &none, false, 0, &[]),
            signature(&said, &items, &none, true, 0, &[]),
            "a line with no call does not care"
        );
    }

    #[test]
    fn a_running_call_is_re_measured_as_it_prints_and_not_for_other_calls_output() {
        let items = vec![asking(1, "a")];
        let none = HashMap::new();
        let printing = |call: &str, text: &str| {
            vec![RunningTool {
                call_id: call.to_string(),
                name: "bash".to_string(),
                output: text.to_string(),
            }]
        };
        let sig = |running: &[RunningTool]| signature(&items[0], &items, &none, true, 0, running);
        assert_ne!(sig(&[]), sig(&printing("a", "one\n")));
        assert_ne!(
            sig(&printing("a", "one\n")),
            sig(&printing("a", "one\ntwo\n"))
        );
        assert_eq!(
            sig(&[]),
            sig(&printing("elsewhere", "noise")),
            "another call's output is no reason to redraw this row"
        );
        // Once the result is in, what it printed no longer matters to the row.
        let results: HashMap<String, usize> = [("a".to_string(), 1)].into();
        let with_result = vec![asking(1, "a"), answer(2, "a", false)];
        assert_eq!(
            signature(&with_result[0], &with_result, &results, true, 0, &[]),
            signature(
                &with_result[0],
                &with_result,
                &results,
                true,
                0,
                &printing("a", "x")
            )
        );
    }

    #[test]
    fn what_a_call_printed_is_read_from_its_end() {
        assert_eq!(last_line("one\ntwo\n\n  \n"), Some("two"));
        assert_eq!(last_line("\n \n"), None);
        assert_eq!(tail_lines("a\nb\n", 5), "a\nb");
        assert_eq!(tail_lines("1\n2\n3\n4\n", 2), "…\n3\n4");
        let running = [RunningTool {
            call_id: "a".to_string(),
            name: "bash".to_string(),
            output: "x".to_string(),
        }];
        assert_eq!(output_of(&running, "a"), Some("x"));
        assert_eq!(output_of(&running, "b"), None);
    }

    fn said_by(id: u64, role: ItemRole, text: &str, reply: Option<&str>) -> TranscriptItem {
        TranscriptItem {
            reply: reply.map(|text| Quote::of("them", text)),
            ..line(id, role, text)
        }
    }

    #[test]
    fn a_reply_with_a_quote_field_is_a_thread_of_the_line_it_quotes() {
        let items = vec![
            said_by(1, ItemRole::Peer, "lunch at noon?", None),
            said_by(2, ItemRole::Peer, "bring the notes", None),
            said_by(3, ItemRole::User, "yes", Some("lunch at noon?")),
            said_by(4, ItemRole::Peer, "great", Some("yes")),
            said_by(5, ItemRole::User, "unrelated", None),
            said_by(6, ItemRole::User, "gone", Some("a line out of view")),
        ];
        let (threads, root_of) = group_threads(&items, false);
        assert_eq!(
            threads.get(&0),
            Some(&vec![2, 3]),
            "a reply to a reply joins its thread"
        );
        assert_eq!(root_of.get(&2), Some(&0));
        assert_eq!(root_of.get(&3), Some(&0));
        assert!(!root_of.contains_key(&1) && !root_of.contains_key(&4));
        assert!(
            !root_of.contains_key(&5),
            "a reply to a line that is not in view stays in the log"
        );
    }

    #[test]
    fn with_a_daemon_a_reply_is_read_off_its_quote_and_the_answer_joins_the_thread() {
        let quoted = Quote::of("Researcher", "There are 43 files").prefix("and the biggest?");
        let items = vec![
            said_by(1, ItemRole::User, "how many files?", None),
            said_by(2, ItemRole::Assistant, "There are 43 files", None),
            said_by(3, ItemRole::User, &quoted, None),
            said_by(4, ItemRole::Assistant, "mod.rs", None),
            line(5, ItemRole::Tool, "ls"),
            said_by(6, ItemRole::User, "thanks", None),
            said_by(7, ItemRole::Assistant, "welcome", None),
        ];
        let (reply, body) = reply_of(&items[2], true).expect("a quoted message");
        assert_eq!(reply.text, "There are 43 files");
        assert_eq!(body, "and the biggest?");
        assert!(reply_of(&items[2], false).is_none());

        let (threads, root_of) = group_threads(&items, true);
        assert_eq!(threads.get(&1), Some(&vec![2, 3, 4]));
        assert!(
            !root_of.contains_key(&5) && !root_of.contains_key(&6),
            "the next plain message ends the thread"
        );
    }

    #[test]
    fn a_session_is_titled_by_what_was_said_and_an_unlisted_one_is_new() {
        let daemon = Daemon {
            label: "CoS".to_string(),
            sessions: vec![Session {
                id: "7".to_string(),
                title: "Port the page".to_string(),
                status: Activity::Working,
                model: None,
                error: None,
            }],
            ..Daemon::default()
        };
        let state = on(daemon, Watch::On("d".to_string(), "7".to_string()));
        let subject = Subject::of(&state);
        assert_eq!(subject.title, "Port the page");
        assert_eq!(subject.activity, Activity::Working);
        assert_eq!(
            subject.speaker, "CoS",
            "the session answers as its daemon's agent"
        );
        let unlisted = SipilState {
            watch: Watch::On("d".to_string(), "gone".to_string()),
            ..state
        };
        assert_eq!(Subject::of(&unlisted).title, "New session");
    }

    #[test]
    fn a_channel_is_titled_for_what_it_is() {
        let state = SipilState {
            me: Me {
                nickname: "alex".to_string(),
                channels: vec![ChannelRow {
                    channel: pilus_client::sipil::Channel {
                        id: "0123456789abcdef".to_string(),
                        name: "project-x".to_string(),
                        members: vec!["a".repeat(64), "b".repeat(64)],
                        owner: "a".repeat(64),
                        created: 0,
                    },
                    unread: 0,
                    agents: Vec::new(),
                }],
                ..Me::default()
            },
            watch: Watch::Channel("0123456789abcdef".to_string()),
            ..SipilState::default()
        };
        let channel = Subject::of(&state);
        assert_eq!(channel.title, "#project-x");
        assert!(channel.peer);
    }

    #[test]
    fn a_line_shows_its_speaker_with_a_colon_and_agents_by_their_own_name() {
        let item = |role, text: &str, speaker: Option<&str>| TranscriptItem {
            id: 1,
            role,
            text: text.to_string(),
            tools: Vec::new(),
            calls: Vec::new(),
            call_id: None,
            is_error: false,
            reply: None,
            error: None,
            at: None,
            speaker: speaker.map(str::to_string),
        };
        assert_eq!(
            line_nick(&item(ItemRole::User, "x", None), "alex", "rina"),
            "alex:"
        );
        assert_eq!(
            line_nick(&item(ItemRole::Peer, "x", None), "alex", "rina"),
            "rina:"
        );
        assert_eq!(
            line_nick(
                &item(ItemRole::User, "x", Some("Researcher")),
                "alex",
                "rina"
            ),
            "Researcher:"
        );
        assert_eq!(
            line_nick(&item(ItemRole::Peer, "x", Some("rani")), "alex", ""),
            "rani:"
        );
        assert_eq!(line_nick(&item(ItemRole::Tool, "x", None), "me", "s"), "-");
        let call = TranscriptItem {
            tools: vec!["bash".to_string()],
            ..item(ItemRole::Assistant, "", None)
        };
        assert_eq!(line_nick(&call, "me", "Researcher"), "*");
    }

    #[test]
    fn plain_talk_is_set_plain_and_markup_is_parsed() {
        for plain in [
            "hello there",
            "see you at 10:30, ok?",
            "rina: done (really)",
        ] {
            assert!(!looks_like_markdown(plain), "{plain}");
        }
        for marked in [
            "**bold**",
            "some `code`",
            "a [link](http://x)",
            "# Heading",
            "> quoted",
            "- one\n- two",
            "1. first",
            "two\nlines",
            "a | table",
        ] {
            assert!(looks_like_markdown(marked), "{marked}");
        }
    }

    #[test]
    fn the_answer_being_written_shows_its_end() {
        assert_eq!(live_tail("short"), "short");
        let long = "x".repeat(LIVE_CHARS) + "end";
        let shown = live_tail(&long);
        assert!(shown.starts_with('…') && shown.ends_with("xxend"));
        assert_eq!(shown.chars().count(), LIVE_CHARS + 1);
    }
}
