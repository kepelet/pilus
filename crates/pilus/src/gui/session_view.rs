//! A recent session opened on its own page: the conversation drawn by pilus,
//! with Continue and Fork.
//!
//! Home's Recent Sessions and Search's results open a row here instead of
//! resuming it. The page is a layer of the page it came from
//! (`OverviewPage::session`, tagged with its [`Origin`]): it replaces that page's
//! content while it is open and Back, or Escape, returns to the list.
//!
//! The conversation is read on demand from the end of the session's transcript
//! (`pilus_index::read_transcript_local`) on a worker thread, shown and kept
//! nowhere. It is a virtualized list (only the rows in view are built), and
//! scrolling near the top reads a window twice as large and puts the older
//! entries in front, without moving the row being read, until the file's start
//! or `MAX_WINDOW`. It needs the permission Search has (`Settings::index_agents`, with
//! the session's machine ticked). A session on a saved machine is read the same
//! way over SSH (`read_transcript_remote`: the end of the file, cut out by
//! `tail` on the host), so only that window crosses the wire.
//!
//! - Continue is [`Reopen::Continue`]: the session's folder as a workspace on its
//!   machine and the agent's own resume.
//! - Fork is [`Reopen::Fork`]: the same, but the agent starts a new session from
//!   this one's history, so the original is left as it was.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;

use gpui::{AnyElement, Context, FollowMode, ListAlignment, ListState, div, list, prelude::*, px};
use pilus_client::SshTarget;
use pilus_index::{
    Agent, Entry, MAX_WINDOW, Session, Transcript, WINDOW, read_transcript_local,
    read_transcript_remote,
};

use super::overview::Msg;
use super::search::Reopen;
use super::style::{self, ButtonKind, FillExt, Fonts, RadiusExt};
use super::tasks::{age_label, unix_now};
use super::{PilusView, icon};

/// Lines of a tool's result drawn before it is cut.
const OUTPUT_LINES: usize = 40;
/// The widest the page runs.
const WIDTH: f32 = 860.0;
/// How far past the visible rows the list lays out, so a scroll does not wait
/// for a measure.
const OVERDRAW: f32 = 1200.0;
/// Earlier entries are asked for once fewer than this many rows are above the
/// ones in view.
const EARLIER_PREFETCH_ROWS: usize = 20;
/// The machine key of this Mac in the index.
const LOCAL: &str = "local";

/// What the page knows of the conversation.
#[derive(Debug, Default, Eq, PartialEq)]
enum Load {
    #[default]
    Reading,
    Ready,
    /// Why it cannot be shown.
    Unavailable(String),
}

/// The page the session was opened from, which Back returns to.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum Origin {
    #[default]
    Home,
    Search,
}

/// The session on the page.
pub(super) struct SessionView {
    open: Option<Session>,
    origin: Origin,
    load: Load,
    /// Where its transcript is read: `None` is this Mac.
    target: Option<SshTarget>,
    /// The conversation so far, one list row each.
    entries: Vec<Entry>,
    /// The file goes on before what has been read.
    earlier: bool,
    /// How much of the end of the file `entries` came from.
    window: u64,
    /// A larger window is being read.
    fetching: bool,
    /// The list was scrolled near its top. Set by the list's scroll handler,
    /// which has no view to call, and taken by [`PilusView::session_view_tick`].
    wants_earlier: Arc<AtomicBool>,
    /// Only the rows in view are built, so a long conversation costs what a
    /// short one does.
    list: ListState,
    /// Tool calls opened, by index into `entries`.
    expanded: HashSet<usize>,
}

impl Default for SessionView {
    fn default() -> Self {
        let wants_earlier = Arc::new(AtomicBool::new(false));
        let list = ListState::new(0, ListAlignment::Top, px(OVERDRAW));
        list.set_follow_mode(FollowMode::Tail);
        let flag = Arc::clone(&wants_earlier);
        list.set_scroll_handler(move |event, _window, _cx| {
            if event.visible_range.start < EARLIER_PREFETCH_ROWS {
                flag.store(true, Ordering::Relaxed);
            }
        });
        Self {
            open: None,
            origin: Origin::Home,
            load: Load::Reading,
            target: None,
            entries: Vec::new(),
            earlier: false,
            window: WINDOW,
            fetching: false,
            wants_earlier,
            list,
            expanded: HashSet::new(),
        }
    }
}

impl SessionView {
    pub(super) fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// Whether the page is showing a session opened from `origin`.
    pub(super) fn is_open_from(&self, origin: Origin) -> bool {
        self.open.is_some() && self.origin == origin
    }

    pub(super) fn close(&mut self) {
        *self = Self::default();
    }

    /// Show `transcript` from its end, as a conversation just opened.
    fn start(&mut self, transcript: Transcript) {
        self.load = Load::Ready;
        self.list.reset(transcript.entries.len());
        self.list.set_follow_mode(FollowMode::Tail);
        self.list.scroll_to_end();
        self.entries = transcript.entries;
        self.earlier = transcript.earlier;
        self.expanded.clear();
    }

    /// Take a larger read of the same file. Its entries end with the ones
    /// already here, so the new ones are put in front and the reader stays on
    /// the row they were on. A read that does not line up (the file was
    /// rewritten) starts over.
    fn extend(&mut self, transcript: Transcript, window: u64) {
        self.window = window;
        let old = &self.entries;
        let probe = old.len().min(8);
        let lead = if probe == 0 {
            Some(0)
        } else {
            transcript
                .entries
                .windows(probe)
                .position(|at| at == &old[..probe])
        };
        let Some(lead) = lead.filter(|lead| lead + old.len() <= transcript.entries.len()) else {
            return self.start(transcript);
        };
        if lead > 0 {
            self.list.splice(0..0, lead);
            self.expanded = self.expanded.iter().map(|index| index + lead).collect();
        }
        // What the session said meanwhile comes after.
        let end = lead + old.len();
        let added = transcript.entries.len().saturating_sub(end);
        if added > 0 {
            self.list.splice(end..end, added);
        }
        self.entries = transcript.entries;
        self.earlier = transcript.earlier;
    }

    /// A page already showing `transcript`, for a capture.
    #[cfg(feature = "visual-test")]
    pub(super) fn sample(
        session: Session,
        origin: Origin,
        transcript: Transcript,
        opened: &[usize],
    ) -> Self {
        let mut view = Self {
            open: Some(session),
            origin,
            ..Self::default()
        };
        view.start(transcript);
        view.expanded = opened.iter().copied().collect();
        view
    }
}

/// The agent's mark.
pub(super) fn agent_glyph(agent: Agent) -> &'static [u8] {
    match agent {
        Agent::Claude => include_bytes!("../../assets/icons/agents/claude-code.svg"),
        Agent::Pi => include_bytes!("../../assets/icons/agents/pi.svg"),
    }
}

/// Read the last `window` bytes of the conversation of the transcript at `path`
/// and report it to `tx`.
fn read_in_background(
    path: String,
    agent: Agent,
    target: Option<SshTarget>,
    window: u64,
    tx: Sender<Msg>,
) {
    std::thread::spawn(move || {
        let found = match &target {
            None => read_transcript_local(&path, agent, window),
            Some(target) => read_transcript_remote(target, &path, agent, window),
        }
        .map_err(|error| error.to_string());
        tx.send(Msg::Transcript(path, found)).ok();
    });
}

impl PilusView {
    /// Open `session` on its own page and start reading its conversation.
    pub(super) fn open_session_view(
        &mut self,
        session: Session,
        origin: Origin,
        cx: &mut Context<Self>,
    ) {
        // The permission is Search's: the index on, and this machine ticked.
        let allowed = self.settings.index_agents
            && self
                .settings
                .index_machines
                .iter()
                .any(|key| *key == session.machine);
        let source = self
            .search_choices()
            .into_iter()
            .find(|choice| choice.key == session.machine);
        let mut view = SessionView {
            origin,
            ..SessionView::default()
        };
        match (allowed, source) {
            (false, _) => {
                view.load =
                    Load::Unavailable("Reading sessions is off. Turn it on in Search.".to_string());
            }
            (true, None) => {
                view.load = Load::Unavailable(
                    "This machine is no longer saved, so its transcript can't be read. \
                     Continue or Fork still tries it."
                        .to_string(),
                );
            }
            (true, Some(choice)) => {
                read_in_background(
                    session.path.clone(),
                    session.agent,
                    choice.target.clone(),
                    view.window,
                    self.overview.tx.clone(),
                );
                view.target = choice.target;
            }
        }
        view.open = Some(session);
        self.overview.session = view;
        cx.notify();
    }

    /// A conversation arrived: the first read, or a larger one asked for by
    /// scrolling up. One for a session that was closed or replaced meanwhile is
    /// dropped.
    pub(super) fn session_view_take(&mut self, path: &str, found: Result<Transcript, String>) {
        let view = &mut self.overview.session;
        if view
            .open
            .as_ref()
            .is_none_or(|session| session.path != path)
        {
            return;
        }
        match (found, view.load == Load::Reading, view.fetching) {
            (Ok(transcript), true, _) => view.start(transcript),
            (Ok(transcript), false, true) => {
                let window = (view.window * 2).min(MAX_WINDOW);
                view.extend(transcript, window);
            }
            (Err(error), true, _) => {
                view.load = Load::Unavailable(format!("Couldn't read the transcript: {error}"));
            }
            // Earlier history that cannot be read is left unread, and not asked
            // for again.
            (Err(error), false, _) => {
                tracing::warn!("session page: could not read earlier history: {error}");
                view.earlier = false;
            }
            (Ok(_), false, false) => {}
        }
        view.fetching = false;
    }

    /// Ask for earlier history when the list was scrolled near its top. Returns
    /// whether the page changed.
    pub(super) fn session_view_tick(&mut self) -> bool {
        let view = &mut self.overview.session;
        if !view.wants_earlier.swap(false, Ordering::Relaxed) {
            return false;
        }
        let Some(session) = view.open.as_ref() else {
            return false;
        };
        if view.load != Load::Ready || view.fetching || !view.earlier || view.window >= MAX_WINDOW {
            return false;
        }
        view.fetching = true;
        read_in_background(
            session.path.clone(),
            session.agent,
            view.target.clone(),
            (view.window * 2).min(MAX_WINDOW),
            self.overview.tx.clone(),
        );
        true
    }

    fn session_view_toggle(&mut self, index: usize, cx: &mut Context<Self>) {
        let view = &mut self.overview.session;
        if !view.expanded.remove(&index) {
            view.expanded.insert(index);
        }
        // The row is taller or shorter now.
        view.list.remeasure_items(index..index + 1);
        cx.notify();
    }

    /// The page, in place of the Home page's content.
    pub(super) fn session_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let fonts = Fonts::resolve(&self.shared_settings);
        let Some(session) = self.overview.session.open.clone() else {
            return div().into_any_element();
        };

        let back = style::small_button(
            "pilus-session-back",
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .child(icon(
                    include_bytes!("../../assets/icons/chevron-left.svg"),
                    fonts.label,
                    theme.muted,
                ))
                .child(match self.overview.session.origin {
                    Origin::Home => "Home",
                    Origin::Search => "Search",
                }),
            ButtonKind::Ghost,
            theme,
            fonts.label,
        )
        .on_click(cx.listener(|this, _, _, cx| {
            this.overview.session.close();
            cx.notify();
        }));
        let busy = self.search.resuming;
        let fork_target = session.clone();
        let continue_target = session.clone();
        let actions = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(
                style::button_enabled(
                    "pilus-session-fork",
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .child(icon(
                            include_bytes!("../../assets/icons/git-fork.svg"),
                            fonts.label,
                            theme.panel_fg,
                        ))
                        .child("Fork"),
                    ButtonKind::Secondary,
                    theme,
                    fonts.label,
                    !busy,
                )
                .tooltip(super::row_tooltip(
                    "Start a new session from this one's history".to_string(),
                    theme.panel_fg,
                    Vec::new(),
                    theme,
                ))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.search_reopen(&fork_target, Reopen::Fork, cx);
                })),
            )
            .child(
                style::button_enabled(
                    "pilus-session-continue",
                    "Continue",
                    ButtonKind::Primary,
                    theme,
                    fonts.label,
                    !busy,
                )
                .tooltip(super::row_tooltip(
                    "Carry on in this session".to_string(),
                    theme.panel_fg,
                    Vec::new(),
                    theme,
                ))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.search_reopen(&continue_target, Reopen::Continue, cx);
                })),
            );
        let bar = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .child(back)
            .child(actions);

        let mut facts: Vec<String> = vec![session.agent.label().to_string()];
        if session.machine != LOCAL {
            let name = self
                .search_choices()
                .into_iter()
                .find(|choice| choice.key == session.machine)
                .map_or_else(|| session.machine.clone(), |choice| choice.label);
            facts.push(name);
        }
        if !session.cwd.is_empty() {
            facts.push(self.shorten_cwd(&session.machine, &session.cwd));
        }
        if !session.branch.is_empty() {
            facts.push(session.branch.clone());
        }
        let now = unix_now();
        facts.push(format!(
            "updated {} ago",
            age_label(now, u64::try_from(session.updated).unwrap_or(0))
        ));
        let heading = div()
            .flex()
            .flex_col()
            .gap_1()
            .px_1()
            .min_w(px(0.0))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap_2()
                    .child(
                        // The mark sits on the first line of a title that wraps.
                        div().flex_shrink_0().pt(px(4.0)).child(icon(
                            agent_glyph(session.agent),
                            fonts.body,
                            theme.muted,
                        )),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .text_size(px(fonts.body + 3.0))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(theme.panel_fg)
                            .child(session.title.clone()),
                    ),
            )
            .child(
                div()
                    .min_w(px(0.0))
                    .font_family(style::mono_family(&self.shared_settings))
                    .text_size(px(fonts.label))
                    .text_color(theme.muted)
                    .child(facts.join(" · ")),
            );

        let column = |child: AnyElement| {
            div()
                .flex()
                .flex_row()
                .justify_center()
                .w_full()
                .flex_shrink_0()
                .child(
                    div()
                        .w_full()
                        .max_w(px(style::scaled(&self.shared_settings, WIDTH)))
                        .px_4()
                        .child(child),
                )
        };
        let top = column(
            div()
                .flex()
                .flex_col()
                .gap_4()
                .pt_4()
                .pb_3()
                .child(bar)
                .child(heading)
                .into_any_element(),
        );
        style::enter(
            "pilus-session-page",
            div()
                .flex()
                .flex_col()
                .size_full()
                .min_h(px(0.0))
                .child(top)
                .child(self.session_conversation(cx)),
        )
        .into_any_element()
    }

    /// The conversation: a list of the entries, or why there are none.
    fn session_conversation(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let fonts = Fonts::resolve(&self.shared_settings);
        let view = &self.overview.session;
        let note = |text: &str| {
            div()
                .flex_shrink_0()
                .w_full()
                .px_4()
                .py_2()
                .flex()
                .justify_center()
                .text_size(px(fonts.secondary))
                .text_color(theme.muted)
                .child(text.to_string())
                .into_any_element()
        };
        match &view.load {
            Load::Reading => return note("Reading the conversation…"),
            Load::Unavailable(why) => return note(why),
            Load::Ready if view.entries.is_empty() => {
                return note("Nothing has been said in this session yet.");
            }
            Load::Ready => {}
        }
        let mut body = div().flex().flex_col().flex_1().min_h(px(0.0)).w_full();
        if view.fetching {
            body = body.child(note("Loading earlier messages…"));
        } else if view.earlier && view.window >= MAX_WINDOW {
            body = body.child(note("Earlier messages are not shown."));
        }
        body.child(
            list(
                view.list.clone(),
                cx.processor(|this: &mut PilusView, ix: usize, _window, cx| {
                    this.session_row(ix, cx)
                }),
            )
            .flex_1()
            .min_h(px(0.0))
            .w_full(),
        )
        .into_any_element()
    }

    /// One row of the list: the entry, in the page's column.
    fn session_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let view = &self.overview.session;
        let (Some(entry), Some(session)) = (view.entries.get(ix), view.open.as_ref()) else {
            return div().into_any_element();
        };
        div()
            .flex()
            .flex_row()
            .justify_center()
            .w_full()
            .child(
                div()
                    .w_full()
                    .max_w(px(style::scaled(&self.shared_settings, WIDTH)))
                    .px_4()
                    .pb_3()
                    .child(self.session_entry(session.agent, ix, entry, cx)),
            )
            .into_any_element()
    }

    fn session_entry(
        &self,
        agent: Agent,
        index: usize,
        entry: &Entry,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.theme;
        let fonts = Fonts::resolve(&self.shared_settings);
        let said = |who: String, at: i64, text: &str, fill: gpui::Rgba| {
            let when = if at > 0 {
                format!(
                    " · {} ago",
                    age_label(unix_now(), u64::try_from(at).unwrap_or(0))
                )
            } else {
                String::new()
            };
            div()
                .flex()
                .flex_col()
                .gap_1()
                .p_3()
                .radius_md()
                .border_1()
                .border_color(theme.border)
                .bg(fill)
                .child(style::section_label(
                    theme,
                    format!("{who}{when}").to_uppercase(),
                    fonts.label,
                ))
                .child(div().text_size(px(fonts.body)).child(self.chat_text(
                    text,
                    theme.panel_fg,
                    fonts,
                    &self.shared_settings,
                )))
                .into_any_element()
        };
        match entry {
            Entry::User { text, at } => said("You".to_string(), *at, text, theme.panel),
            Entry::Assistant { text, at } => {
                said(agent.label().to_string(), *at, text, theme.panel_dim)
            }
            Entry::Tool {
                name,
                summary,
                output,
                failed,
                ..
            } => {
                let open = self.overview.session.expanded.contains(&index);
                let mono = style::mono_family(&self.shared_settings);
                let mut block = div().flex().flex_col().gap_1().child(
                    div()
                        .id(format!("pilus-session-tool-{index}"))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .h(px(style::scaled(
                            &self.shared_settings,
                            style::CONTROL_HEIGHT_SMALL,
                        )))
                        .radius_md()
                        .hover_fill(theme.panel, style::RADIUS_MD)
                        .cursor_pointer()
                        .child(style::chevron(
                            format!("pilus-session-chevron-{index}"),
                            open,
                            !cx.reduce_motion(),
                            fonts.label,
                            theme.muted,
                        ))
                        .child(
                            div()
                                .flex_shrink_0()
                                .font_family(mono.clone())
                                .text_size(px(fonts.label))
                                .text_color(if *failed { theme.alert } else { theme.panel_fg })
                                .child(name.clone()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .truncate()
                                .font_family(mono.clone())
                                .text_size(px(fonts.label))
                                .text_color(theme.muted)
                                .child(summary.clone()),
                        )
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.session_view_toggle(index, cx)),
                        ),
                );
                if open {
                    let text = output.as_deref().unwrap_or("No result recorded.");
                    let lines: Vec<&str> = text.lines().collect();
                    let mut result = div()
                        .flex()
                        .flex_col()
                        .mx_2()
                        .p_2()
                        .radius_md()
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.panel_dim)
                        .font_family(mono)
                        .text_size(px(fonts.label))
                        .text_color(if *failed { theme.alert } else { theme.panel_fg });
                    for line in lines.iter().take(OUTPUT_LINES) {
                        // A blank line keeps its height.
                        let line = if line.is_empty() { "\u{a0}" } else { line };
                        result = result.child(div().child((*line).to_string()));
                    }
                    if lines.len() > OUTPUT_LINES {
                        result = result.child(
                            div()
                                .pt_1()
                                .text_color(theme.muted)
                                .child(format!("… {} more lines", lines.len() - OUTPUT_LINES)),
                        );
                    }
                    block = block.child(result);
                }
                block.into_any_element()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn said(text: &str) -> Entry {
        Entry::User {
            text: text.to_string(),
            at: 0,
        }
    }

    fn view_of(texts: &[&str], earlier: bool) -> SessionView {
        let mut view = SessionView::default();
        view.start(Transcript {
            entries: texts.iter().map(|text| said(text)).collect(),
            earlier,
        });
        view
    }

    #[test]
    fn older_entries_go_in_front_and_open_tool_rows_move_with_them() {
        let mut view = view_of(&["c", "d"], true);
        view.expanded.insert(1);
        let read = Transcript {
            entries: ["a", "b", "c", "d", "e"].iter().map(|t| said(t)).collect(),
            earlier: false,
        };
        view.extend(read, WINDOW * 2);
        assert_eq!(view.entries.len(), 5);
        assert_eq!(view.list.item_count(), 5);
        assert_eq!(view.expanded, HashSet::from([3]));
        assert!(!view.earlier);
        assert_eq!(view.window, WINDOW * 2);
    }

    #[test]
    fn a_read_that_does_not_line_up_starts_over() {
        let mut view = view_of(&["c", "d"], true);
        view.expanded.insert(0);
        let rewritten = Transcript {
            entries: vec![said("x"), said("y"), said("z")],
            earlier: true,
        };
        view.extend(rewritten, WINDOW * 2);
        assert_eq!(view.entries.len(), 3);
        assert_eq!(view.list.item_count(), 3);
        assert!(view.expanded.is_empty());
    }
}
