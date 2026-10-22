//! The Home page: what is going on right now, in one glance.
//!
//! Under a hero it lists the agents (working first, then the idle ones), the
//! last ten agent sessions, the open pull requests of the repositories the sidebar shows, and
//! the newest message in any sipil conversation and the open work on the sipil
//! daemons' boards. It owns no data of its own:
//! every block is another feature's, read through that feature's own gate.
//!
//! - Agents are the daemons' agents (any endpoint), ordered working, blocked,
//!   done, idle. A sipil daemon's agents are not listed: they have their own
//!   rows in the sidebar, and their talk and work show in the blocks below.
//! - Sessions are `pilus-index`'s newest rows, so they need the same
//!   `Settings::index_agents` permission as Search and show its consent hint
//!   until it is given. Nothing is read from a transcript here.
//! - Pull requests go through `pilus_client::GhAccount`'s cache, one read per
//!   repository on a worker thread, so a fresh list costs no request and a rate
//!   limit is waited out. With GitHub off nothing is asked at all.
//! - The newest sipil message is read from this device's own stores (the chat
//!   threads and the daemons' remembered conversations) and never dials anyone.
//! - Open work is each connected daemon's board as the sipil poller last read
//!   it (`Daemon::work`), what waits on someone first. Nothing is asked here.
//!
//! A row opens the thing it names (the working agent's pane, the resumed
//! session, the pull request, the conversation). An idle agent's row opens a
//! panel instead, with what it last said and a field to answer it or stop it
//! (`overview_peek`).

use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, Context, Div, FontWeight, ObjectFit, RenderImage, ScrollHandle, Stateful,
    StyledImage, div, img, linear_color_stop, linear_gradient, prelude::*, px,
};
use pilus_client::{ItemSummary, unix_seconds};
use pilus_index::{Filter, Index, Session, Transcript};

use super::github::age;
use super::github_page::{Subject, View};
use super::overview_peek::{AgentPanel, Lookup, Peeked};
use super::session_view::{Origin, SessionView, agent_glyph};
use super::sipil::Watch;
use super::sipil_social::{self, Latest};
use super::style::{self, ButtonKind, FillExt, Fonts, RadiusExt};
use super::tasks::{age_label, unix_now};
use super::theme::Theme;
use super::toast::ToastKind;
use super::{EndpointId, PilusView, activity, agent_icon, icon};

/// Sessions the page lists.
const SESSIONS: usize = 10;
/// Idle agents the page lists before "more in the sidebar"; working ones always show.
const AGENT_ROWS: usize = 8;
/// Repositories whose pull requests are read: the sidebar's, pins first.
const PULL_REPOS: usize = 5;
/// Pull requests the page lists, newest activity first.
const PULLS: usize = 10;
/// Issues the page lists, below the pull requests.
const ISSUES: usize = 5;
/// Items of open work the page lists before "more in the chat".
const WORK_ROWS: usize = 6;
/// How often an open page asks the account's cache about pull requests. The
/// cache owns freshness, so this only has to be shorter than its lifetime.
const PULLS_EVERY: Duration = Duration::from_secs(120);
/// How often an open page looks again for a newer sipil message.
const LOOK_EVERY: Duration = Duration::from_secs(2);
const HERO_HEIGHT: f32 = 232.0;
/// The widest the content runs, so a wide window does not stretch the rows.
const CONTENT_WIDTH: f32 = 1080.0;
/// The pull request column's width.
const PULLS_WIDTH: f32 = 380.0;
const ROW_HEIGHT: f32 = 46.0;
/// A message taller than this many lines or bytes is cut and faded out.
const MESSAGE_LINES: usize = 8;
const MESSAGE_LONG: usize = 700;
const MESSAGE_HEIGHT: f32 = 200.0;

/// Where a click on an agent goes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Target {
    /// A pane of a daemon's endpoint.
    Pane { endpoint: EndpointId, pane: String },
}

/// An agent the page lists.
#[derive(Clone)]
pub(super) struct Listed {
    /// `working`, `blocked`, `done` or `idle`.
    pub(super) status: String,
    pub(super) glyph: &'static [u8],
    pub(super) name: String,
    pub(super) detail: String,
    pub(super) target: Target,
    /// Where a terminal agent's own transcript is found, when it has one.
    pub(super) lookup: Option<Lookup>,
}

/// What a capture shows instead of the live feeds.
pub(super) struct Sample {
    agents: Vec<Listed>,
    /// Whether the capture shows GitHub's blocks and the sipil message.
    github: bool,
    sipil: bool,
    work: Vec<OpenWork>,
}

/// One item of open work on a daemon's board, with where it is kept.
#[derive(Clone)]
pub(super) struct OpenWork {
    /// The daemon's name.
    who: String,
    title: String,
    /// `todo`, `doing`, `waiting` or `blocked`.
    state: String,
    /// What it waits on, else its next step, else who is on it.
    detail: String,
    /// When it last changed, in Unix milliseconds.
    updated_at: u64,
    /// The daemon's primary agent, whose chat the row opens.
    watch: Watch,
}

/// One finished GitHub read: the pull requests and the issues of the repositories.
pub(super) struct Pulled {
    /// Each is an error only when every repository failed, which is when there
    /// is nothing to show; one that failed alone is left out of the merge.
    pulls: Result<Vec<ItemSummary>, String>,
    issues: Result<Vec<ItemSummary>, String>,
}

/// What a background thread reports back.
pub(super) enum Msg {
    /// The pull requests and issues were read.
    Github(Pulled),
    /// The screen of a pane agent was read, for the agent that was open.
    Peek(Target, Result<Peeked, String>),
    /// A reply or a stop reached the daemon, or did not: what to tell the person.
    Done(Result<String, String>),
    /// The conversation of the session at this transcript path was read.
    Transcript(String, Result<Transcript, String>),
}

/// The page's state.
pub(super) struct OverviewPage {
    open: bool,
    scroll: ScrollHandle,
    sessions: Vec<Session>,
    pulls: Vec<ItemSummary>,
    /// A read has landed, so an empty list means none are open.
    pulls_loaded: bool,
    pulls_error: Option<String>,
    issues: Vec<ItemSummary>,
    issues_error: Option<String>,
    pulls_busy: bool,
    pulls_asked: Option<Instant>,
    latest: Option<Latest>,
    looked: Instant,
    sample: Option<Sample>,
    /// The agent opened under its row, with what it last said.
    pub(super) agent: AgentPanel,
    /// A recent session opened on its own page, over the rest of the page.
    pub(super) session: SessionView,
    pub(super) tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

impl Default for OverviewPage {
    fn default() -> Self {
        let (tx, rx) = channel();
        Self {
            open: false,
            scroll: ScrollHandle::new(),
            sessions: Vec::new(),
            pulls: Vec::new(),
            pulls_loaded: false,
            pulls_error: None,
            issues: Vec::new(),
            issues_error: None,
            pulls_busy: false,
            pulls_asked: None,
            latest: None,
            looked: Instant::now(),
            sample: None,
            agent: AgentPanel::default(),
            session: SessionView::default(),
            tx,
            rx,
        }
    }
}

impl OverviewPage {
    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    pub(super) fn close(&mut self) {
        self.open = false;
        self.sample = None;
        self.agent = AgentPanel::default();
        self.session.close();
    }

    /// The reply field under an open agent has the keyboard.
    pub(super) fn reply_focused(&self) -> bool {
        self.open && self.agent.open.is_some() && self.agent.focused
    }

    /// Hand the keyboard back to the composer.
    pub(super) fn blur_reply(&mut self) {
        self.agent.focused = false;
    }
}

/// The hero photograph, decoded once. GPUI's image buffers are BGRA, so the
/// channels are swapped after decoding.
fn hero() -> Option<Arc<RenderImage>> {
    static HERO: OnceLock<Option<Arc<RenderImage>>> = OnceLock::new();
    HERO.get_or_init(|| {
        let decoded = image::load_from_memory_with_format(
            include_bytes!("../../assets/hero-factory.png"),
            image::ImageFormat::Png,
        )
        .ok()?;
        let mut buffer = decoded.into_rgba8();
        for pixel in buffer.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        Some(Arc::new(RenderImage::new([image::Frame::new(buffer)])))
    })
    .clone()
}

/// `3 agents working`, `1 open pull request`.
fn counted(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// The items of every repository, the most recently active first, at most
/// `limit`. A timestamp that does not parse sorts last.
fn newest(mut items: Vec<ItemSummary>, limit: usize) -> Vec<ItemSummary> {
    items.sort_by_key(|item| std::cmp::Reverse(unix_seconds(&item.updated_at).unwrap_or(0)));
    items.truncate(limit);
    items
}

/// Read one list of each repository and merge them. It is an error only when
/// every repository failed.
fn merged(
    names: &[String],
    mut read: impl FnMut(&str) -> Result<Vec<ItemSummary>, String>,
) -> Result<Vec<ItemSummary>, String> {
    let mut items = Vec::new();
    let mut failures = Vec::new();
    for name in names {
        match read(name) {
            Ok(found) => items.extend(found),
            Err(error) => failures.push(error),
        }
    }
    match failures.into_iter().next() {
        Some(error) if items.is_empty() => Err(error),
        _ => Ok(items),
    }
}

impl PilusView {
    /// Open the page and start what it shows.
    pub(super) fn open_overview(&mut self, cx: &mut Context<Self>) {
        self.leave_settings_page();
        // The composer on the page is the Cmd+Shift+N one, so it starts too.
        self.home_begin();
        self.overview.open = true;
        self.overview.pulls_asked = None;
        self.overview.latest = sipil_social::latest(&self.sipil);
        self.overview.looked = Instant::now();
        if self.settings.index_agents {
            self.search_sync();
            self.overview_reload_sessions();
        }
        self.overview_load_pulls(false);
        cx.notify();
    }

    fn close_overview(&mut self, cx: &mut Context<Self>) {
        self.close_home_page();
        cx.notify();
    }

    /// Read the newest sessions from the index. It opens the file afresh each
    /// time: a reader kept across "Turn off", which deletes the file, would
    /// keep showing what it held.
    pub(super) fn overview_reload_sessions(&mut self) {
        if !self.settings.index_agents || !self.overview.open || self.overview.sample.is_some() {
            return;
        }
        let read = Index::open(&self.search_index_path())
            .and_then(|index| index.search("", &Filter::default(), SESSIONS));
        match read {
            Ok(found) => self.overview.sessions = found,
            Err(error) => tracing::warn!("home: could not read the session index: {error}"),
        }
    }

    /// Whether the page draws GitHub's blocks: the feature is on and `gh` is
    /// installed, as the sidebar decides it, or a capture is showing samples.
    fn overview_github(&self) -> bool {
        match &self.overview.sample {
            Some(sample) => sample.github,
            None => self.settings.github_shows_repositories() && !self.github.missing(),
        }
    }

    /// Ask for the open pull requests and issues of the repositories the
    /// sidebar shows.
    /// `force` is a person asking, which skips the cache's lifetime but not
    /// its rate-limit back-off.
    fn overview_load_pulls(&mut self, force: bool) {
        if self.overview.pulls_busy
            || self.overview.sample.is_some()
            || !self.settings.github_shows_repositories()
            || !self.github.ready()
        {
            return;
        }
        let names: Vec<String> = self
            .github
            .sidebar_repos(&self.settings.github_pinned, PULL_REPOS)
            .into_iter()
            .map(|repo| repo.name.clone())
            .collect();
        // The repository list has not landed; the next tick asks again.
        if names.is_empty() {
            return;
        }
        self.overview.pulls_busy = true;
        self.overview.pulls_asked = Some(Instant::now());
        let account = self.github.account();
        let tx = self.overview.tx.clone();
        std::thread::spawn(move || {
            let pulls = merged(&names, |name| {
                account.repo_pulls(name, force).map(|listing| listing.items)
            });
            let issues = merged(&names, |name| {
                account
                    .repo_issues(name, force)
                    .map(|listing| listing.items)
            });
            tx.send(Msg::Github(Pulled { pulls, issues })).ok();
        });
    }

    /// Fold finished reads in, ask again when due, and look for a newer
    /// message. Returns whether anything on screen changed.
    pub(super) fn overview_tick(&mut self) -> bool {
        let mut changed = false;
        while let Ok(msg) = self.overview.rx.try_recv() {
            let pulled = match msg {
                Msg::Github(pulled) => pulled,
                Msg::Peek(target, result) => {
                    self.overview_take_peek(target, result);
                    changed = true;
                    continue;
                }
                Msg::Transcript(path, result) => {
                    self.session_view_take(&path, result);
                    changed = true;
                    continue;
                }
                Msg::Done(result) => {
                    match result {
                        Ok(message) => self.toast(ToastKind::Info, message),
                        Err(error) => self.toast(ToastKind::Error, error),
                    }
                    changed = true;
                    continue;
                }
            };
            self.overview.pulls_busy = false;
            self.overview.pulls_loaded = true;
            // A failed read keeps the rows it had.
            match pulled.pulls {
                Ok(items) => {
                    self.overview.pulls = newest(items, PULLS);
                    self.overview.pulls_error = None;
                }
                Err(error) => self.overview.pulls_error = Some(error),
            }
            match pulled.issues {
                Ok(items) => {
                    self.overview.issues = newest(items, ISSUES);
                    self.overview.issues_error = None;
                }
                Err(error) => self.overview.issues_error = Some(error),
            }
            changed = true;
        }
        // The session page can be over Search as well as Home.
        changed |= self.session_view_tick();
        if !self.overview.open || self.overview.sample.is_some() {
            return changed;
        }
        let due = self
            .overview
            .pulls_asked
            .is_none_or(|at| at.elapsed() >= activity::scaled(PULLS_EVERY));
        if due {
            self.overview_load_pulls(false);
        }
        changed |= self.overview_agent_tick();
        if self.overview.looked.elapsed() >= activity::scaled(LOOK_EVERY) {
            self.overview.looked = Instant::now();
            let latest = sipil_social::latest(&self.sipil);
            if latest != self.overview.latest {
                self.overview.latest = latest;
                changed = true;
            }
        }
        changed
    }

    /// Every daemon's agent, the working ones first, then blocked, done and idle.
    pub(super) fn overview_agents(&self) -> Vec<Listed> {
        if let Some(sample) = &self.overview.sample {
            return sample.agents.clone();
        }
        let mut agents = Vec::new();
        for id in self.connected_endpoints() {
            for agent in self.endpoint_agents(&id) {
                // Where it works: the workspace, the folder in it, the branch.
                let mut detail = Vec::new();
                if !agent.workspace_name.is_empty() {
                    detail.push(agent.workspace_name.clone());
                }
                if !agent.cwd.is_empty() {
                    detail.push(self.shorten_cwd(&id.key(), &agent.cwd));
                }
                if let Some(branch) = &agent.branch {
                    detail.push(branch.clone());
                }
                if matches!(id, EndpointId::Machine(_)) {
                    detail.push(self.endpoint_label(&id));
                }
                agents.push(Listed {
                    status: agent.status.clone(),
                    glyph: agent_icon(&agent.agent),
                    name: agent.label().to_string(),
                    detail: detail.join(" · "),
                    target: Target::Pane {
                        endpoint: id.clone(),
                        pane: agent.pane.clone(),
                    },
                    lookup: Some(Lookup {
                        machine: id.key(),
                        kind: agent.agent.clone(),
                        cwd: self.expand_cwd(&id.key(), &agent.cwd),
                        session: agent.session.clone(),
                    }),
                });
            }
        }
        agents.sort_by_key(|agent| status_rank(&agent.status));
        agents
    }

    /// The open work of every daemon that has a board, what waits on someone
    /// first. `None` when no daemon has one, so the page has no block for it.
    fn overview_work(&self) -> Option<Vec<OpenWork>> {
        if let Some(sample) = &self.overview.sample {
            return (!sample.work.is_empty()).then(|| sample.work.clone());
        }
        let mut boards = false;
        let mut work = Vec::new();
        for daemon in &self.sipil.daemons {
            let (Some(board), Some(agent)) = (daemon.work.as_ref(), daemon.agents.first()) else {
                continue;
            };
            boards = true;
            for item in &board.open {
                let detail = [&item.waiting_on, &item.next, &item.owner]
                    .into_iter()
                    .find(|text| !text.is_empty())
                    .cloned()
                    .unwrap_or_default();
                work.push(OpenWork {
                    who: daemon.name(),
                    title: item.title.clone(),
                    state: item.state.clone(),
                    detail,
                    updated_at: item.updated_at,
                    watch: Watch::On(daemon.id.clone(), agent.id.clone()),
                });
            }
        }
        // Each board already keeps what waits first; across boards, keep that.
        work.sort_by_key(|item| !matches!(item.state.as_str(), "waiting" | "blocked"));
        boards.then_some(work)
    }
}

/// Where a status sits in the list: what needs looking at first.
fn status_rank(status: &str) -> u8 {
    match status {
        "working" => 0,
        "blocked" => 1,
        "done" => 2,
        "idle" => 3,
        _ => 4,
    }
}

// -- Rendering -----------------------------------------------------------

/// A titled block: the section frame the other pages use, with a count at the
/// right of its label.
fn card(theme: Theme, fonts: Fonts, label: &'static str, count: Option<usize>, body: Div) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .p_3()
        .radius_md()
        .bg(theme.panel_dim)
        .border_1()
        .border_color(theme.border)
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .px_1()
                .child(style::section_label(theme, label, fonts.label))
                .children(count.filter(|count| *count > 0).map(|count| {
                    div()
                        .text_size(px(fonts.label))
                        .text_color(theme.muted)
                        .child(count.to_string())
                })),
        )
        .child(body)
}

/// What a block says when it has nothing to list.
fn quiet(theme: Theme, fonts: Fonts, title: &str, detail: &str) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .px_1()
        .py_2()
        .child(
            div()
                .text_size(px(fonts.body))
                .text_color(theme.panel_fg)
                .child(title.to_string()),
        )
        .child(
            div()
                .text_size(px(fonts.secondary))
                .text_color(theme.muted)
                .child(detail.to_string()),
        )
}

impl PilusView {
    /// The page, drawn in place of the terminal card.
    pub(super) fn overview_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if self.overview.session.is_open_from(Origin::Home) {
            return self.session_page(cx);
        }
        let agents = self.overview_agents();
        let (busy, resting): (Vec<&Listed>, Vec<&Listed>) =
            agents.iter().partition(|agent| agent.status == "working");
        let working = busy.len();

        let hero = self.overview_hero(working, cx);
        let left = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.0))
            .gap_4()
            .child(self.overview_agents_card(
                "WORKING NOW",
                &busy,
                usize::MAX,
                Some((
                    "Nothing is running",
                    "Agents that are working show up here while they do.",
                )),
                false,
                cx,
            ))
            .children((!resting.is_empty()).then(|| {
                self.overview_agents_card("IDLE AGENTS", &resting, AGENT_ROWS, None, true, cx)
            }))
            .child(self.overview_sessions_card(cx));
        let mut columns = div().flex().flex_row().items_stretch().gap_4().child(left);
        // With GitHub off, or `gh` missing, there is no column at all. Pull
        // requests run the height of the two blocks beside them; issues sit
        // under them once the account is there to read.
        if self.overview_github() {
            let mut right = div()
                .flex()
                .flex_col()
                .gap_4()
                .w(px(style::scaled(&self.shared_settings, PULLS_WIDTH)))
                .flex_shrink_0()
                .child(self.overview_pulls_card(cx).flex_1());
            if self.overview.sample.is_some() || self.github.ready() {
                right = right.child(self.overview_issues_card(cx));
            }
            columns = columns.child(right);
        }
        // Without sipil set up and switched on there is nothing to hear from;
        // what it has to say sits right under the composer.
        let sipil = match &self.overview.sample {
            Some(sample) => sample.sipil,
            None => self.sipil_listed(),
        };
        let heard = sipil.then(|| {
            div()
                .flex()
                .flex_col()
                .gap_4()
                .child(self.overview_latest_card(cx))
                .children(
                    self.overview_work()
                        .map(|work| self.overview_work_card(&work, cx)),
                )
        });
        let content = div()
            .flex()
            .flex_col()
            .gap_4()
            .w_full()
            .max_w(px(CONTENT_WIDTH))
            .px_4()
            .py_4()
            .child(self.overview_composer(cx))
            .children(heard)
            .child(columns);

        div()
            .id("pilus-overview-scroll")
            .flex()
            .flex_col()
            .size_full()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .restrict_scroll_to_axis()
            .track_scroll(&self.overview.scroll)
            .child(hero)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .w_full()
                    .child(content),
            )
            .into_any_element()
    }

    /// The launch composer, the same one Cmd+Shift+N opens: the prompt and its
    /// pickers share `PilusView::home`, so a launch from here is a launch from there.
    fn overview_composer(&self, cx: &mut Context<Self>) -> Div {
        let status = self.home.status().to_string();
        let mut block = div()
            .flex()
            .flex_col()
            .gap_2()
            .w_full()
            .child(self.home_composer(cx));
        if !status.is_empty() {
            block = block.child(
                div()
                    .px_2()
                    .text_size(px(self.footer_font()))
                    .text_color(self.theme.muted)
                    .child(status),
            );
        }
        block
    }

    /// The photograph, faded into the page at its foot, with the greeting and a
    /// line of what is going on over the faded part so the text sits on the
    /// page's own fill in any theme.
    fn overview_hero(&self, working: usize, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = Fonts::resolve(&self.shared_settings);
        let nickname = self.sipil.me.nickname.trim();
        let greeting = if nickname.is_empty() {
            "Welcome back".to_string()
        } else {
            format!("Welcome back, {nickname}")
        };
        let mut facts = Vec::new();
        if working > 0 {
            facts.push(counted(working, "agent working", "agents working"));
        }
        if self.overview_github() && !self.overview.pulls.is_empty() {
            facts.push(counted(
                self.overview.pulls.len(),
                "open pull request",
                "open pull requests",
            ));
        }
        let summary = if facts.is_empty() {
            "All quiet. Open a workspace to start something.".to_string()
        } else {
            facts.join(" · ")
        };

        let actions = div()
            .absolute()
            .top_3()
            .right_3()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .child(
                style::icon_button(
                    "pilus-overview-close",
                    include_bytes!("../../assets/icons/x.svg"),
                    ButtonKind::Ghost,
                    theme,
                    style::scaled(&self.shared_settings, style::CONTROL_HEIGHT_SMALL),
                )
                .tooltip(super::row_tooltip(
                    "Close".to_string(),
                    theme.panel_fg,
                    Vec::new(),
                    theme,
                ))
                .on_click(cx.listener(|this, _, _, cx| this.close_overview(cx))),
            );

        div()
            .relative()
            .w_full()
            .h(px(HERO_HEIGHT))
            .flex_shrink_0()
            .overflow_hidden()
            .children(hero().map(|image| {
                img(image)
                    .absolute()
                    .inset_0()
                    .size_full()
                    .object_fit(ObjectFit::Cover)
            }))
            .child(div().absolute().inset_0().bg(linear_gradient(
                180.0,
                linear_color_stop(theme.bg.opacity(0.0), 0.2),
                linear_color_stop(theme.bg, 1.0),
            )))
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .bottom_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .w_full()
                            .max_w(px(CONTENT_WIDTH))
                            .px_4()
                            .pb_3()
                            .child(
                                div()
                                    .text_size(px(fonts.headline))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(theme.fg)
                                    .child(greeting),
                            )
                            .child(
                                div()
                                    .text_size(px(fonts.body))
                                    .text_color(theme.muted)
                                    .child(summary),
                            ),
                    ),
            )
            .child(actions)
    }

    /// One row of a block: a lead glyph, a title over a line of detail, and
    /// whatever trails. The caller adds the click.
    fn overview_row(
        &self,
        id: String,
        lead: impl IntoElement,
        title: String,
        detail: String,
        trail: Option<AnyElement>,
    ) -> Stateful<Div> {
        let theme = self.theme;
        let fonts = Fonts::resolve(&self.shared_settings);
        div()
            .id(id)
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .min_h(px(style::scaled(&self.shared_settings, ROW_HEIGHT)))
            .flex_shrink_0()
            .px_2()
            .radius_md()
            .hover_fill(theme.panel, style::RADIUS_MD)
            .cursor_pointer()
            .child(
                div()
                    .w(px(16.0))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(lead),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.0))
                    .py_1()
                    .child(
                        div()
                            .truncate()
                            .text_size(px(fonts.body))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.panel_fg)
                            .child(title),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(fonts.label))
                            .text_color(theme.muted)
                            .child(detail),
                    ),
            )
            .children(trail)
    }

    /// The small muted text at a row's right edge.
    fn overview_trail(&self, text: String) -> AnyElement {
        div()
            .flex_shrink_0()
            .text_size(px(Fonts::resolve(&self.shared_settings).label))
            .text_color(self.theme.muted)
            .child(text)
            .into_any_element()
    }

    /// One group of agents as a card. `limit` caps the rows; the rest are
    /// counted in a line under them.
    fn overview_agents_card(
        &self,
        label: &'static str,
        agents: &[&Listed],
        limit: usize,
        empty: Option<(&str, &str)>,
        expandable: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = self.theme;
        let fonts = Fonts::resolve(&self.shared_settings);
        let mut body = div().flex().flex_col().gap_1();
        if let Some((title, detail)) = empty.filter(|_| agents.is_empty()) {
            body = body.child(quiet(theme, fonts, title, detail));
        }
        let shown = agents.len().min(limit);
        for (index, agent) in agents.iter().take(shown).enumerate() {
            let target = agent.target.clone();
            let tint = super::status_color(&agent.status, theme);
            let lead = if agent.status == "working" {
                style::spinner(format!("pilus-overview-spinner-{index}"), 12.0, tint)
                    .into_any_element()
            } else {
                div()
                    .w(px(8.0))
                    .h(px(8.0))
                    .radius_full()
                    .bg(tint)
                    .into_any_element()
            };
            // Working says so with its spinner; the others spell it out.
            let detail = if agent.status == "working" {
                agent.detail.clone()
            } else if agent.detail.is_empty() {
                super::status_label(&agent.status)
            } else {
                format!("{} · {}", super::status_label(&agent.status), agent.detail)
            };
            let listed = (*agent).clone();
            let opened = expandable && self.overview.agent.open.as_ref() == Some(&target);
            body = body.child(
                self.overview_row(
                    format!("pilus-overview-{label}-{index}"),
                    lead,
                    agent.name.clone(),
                    detail,
                    Some(icon(agent.glyph, 12.0, theme.panel_fg).into_any_element()),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if expandable {
                        // An idle agent opens its panel; a second click folds it.
                        this.overview_toggle_agent(listed.clone(), cx);
                    } else {
                        this.overview_open_agent(&target, cx);
                    }
                })),
            );
            if opened {
                body = body.child(self.overview_agent_panel(agent, cx));
            }
        }
        if shown < agents.len() {
            body = body.child(
                div()
                    .px_2()
                    .pt_1()
                    .text_size(px(fonts.label))
                    .text_color(theme.muted)
                    .child(format!("{} more in the sidebar", agents.len() - shown)),
            );
        }
        card(theme, fonts, label, None, body)
    }

    fn overview_sessions_card(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = Fonts::resolve(&self.shared_settings);
        let mut body = div().flex().flex_col().gap_1();
        if !self.settings.index_agents {
            body = body.child(
                quiet(
                    theme,
                    fonts,
                    "Sessions aren't indexed",
                    "Search reads the titles of your Claude and Pi sessions once you allow it.",
                )
                .child(
                    div().pt_1().child(
                        style::small_button(
                            "pilus-overview-allow",
                            "Open Search",
                            ButtonKind::Secondary,
                            theme,
                            fonts.label,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.open_search(cx))),
                    ),
                ),
            );
        } else if self.overview.sessions.is_empty() {
            let (title, detail) = if self.search.syncing {
                (
                    "Reading your sessions…",
                    "The first read of a large history can take a little while.",
                )
            } else {
                (
                    "No sessions yet",
                    "Sessions of Claude and Pi on this Mac show up here.",
                )
            };
            body = body.child(quiet(theme, fonts, title, detail));
        }
        let now = unix_now();
        let shown = if self.settings.index_agents {
            self.overview.sessions.as_slice()
        } else {
            &[]
        };
        for (index, session) in shown.iter().enumerate() {
            let glyph = agent_glyph(session.agent);
            let mut detail = Vec::new();
            if !session.cwd.is_empty() {
                detail.push(self.shorten_cwd(&session.machine, &session.cwd));
            }
            if !session.branch.is_empty() {
                detail.push(session.branch.clone());
            }
            let when = age_label(now, u64::try_from(session.updated).unwrap_or(0));
            let picked = session.clone();
            body = body.child(
                self.overview_row(
                    format!("pilus-overview-session-{index}"),
                    icon(glyph, fonts.body, theme.muted),
                    session.title.clone(),
                    detail.join(" · "),
                    Some(self.overview_trail(when)),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.open_session_view(picked.clone(), Origin::Home, cx);
                })),
            );
        }
        card(theme, fonts, "RECENT SESSIONS", None, body)
    }

    fn overview_pulls_card(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = Fonts::resolve(&self.shared_settings);
        let sample = self.overview.sample.is_some();
        let mut body = div().flex().flex_col().gap_1();
        let note = if sample {
            None
        } else if !self.github.ready() {
            Some(match self.github.unreachable_note() {
                Some(note) => ("GitHub isn't connected", note),
                None => (
                    "Checking GitHub…",
                    "Pull requests appear once the GitHub CLI has answered.".to_string(),
                ),
            })
        } else if let Some(error) = self.overview.pulls_error.as_ref() {
            Some(("Couldn't read pull requests", error.clone()))
        } else if !self.overview.pulls_loaded {
            Some((
                "Reading pull requests…",
                "From the repositories in the sidebar.".to_string(),
            ))
        } else if self.overview.pulls.is_empty() {
            Some((
                "No open pull requests",
                "Nothing is waiting in the repositories in the sidebar.".to_string(),
            ))
        } else {
            None
        };
        if let Some((title, detail)) = note {
            body = body.child(quiet(theme, fonts, title, &detail));
        }
        for (index, item) in self.overview.pulls.iter().enumerate() {
            let subject = Subject::Pull {
                repo: item.repo.clone(),
                number: item.number,
            };
            let glyph: &'static [u8] = if item.draft {
                include_bytes!("../../assets/icons/git-pull-request-draft.svg")
            } else {
                include_bytes!("../../assets/icons/git-pull-request.svg")
            };
            let tint = if item.draft { theme.muted } else { theme.ok };
            let detail = format!("{} · #{} · {}", item.repo, item.number, item.author);
            body = body.child(
                self.overview_row(
                    format!("pilus-overview-pull-{index}"),
                    icon(glyph, fonts.body, tint),
                    item.title.clone(),
                    detail,
                    age(&item.updated_at).map(|when| self.overview_trail(when)),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.github_open(View::Item(subject.clone()), true, cx);
                })),
            );
        }
        card(
            theme,
            fonts,
            "PULL REQUESTS",
            Some(self.overview.pulls.len()),
            body,
        )
    }

    fn overview_issues_card(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = Fonts::resolve(&self.shared_settings);
        let mut body = div().flex().flex_col().gap_1();
        let note = if self.overview.sample.is_some() {
            None
        } else if let Some(error) = self.overview.issues_error.as_ref() {
            Some(("Couldn't read issues", error.clone()))
        } else if !self.overview.pulls_loaded {
            Some((
                "Reading issues…",
                "From the repositories in the sidebar.".to_string(),
            ))
        } else if self.overview.issues.is_empty() {
            Some((
                "No open issues",
                "Nothing is open in the repositories in the sidebar.".to_string(),
            ))
        } else {
            None
        };
        if let Some((title, detail)) = note {
            body = body.child(quiet(theme, fonts, title, &detail));
        }
        for (index, item) in self.overview.issues.iter().enumerate() {
            let subject = Subject::Issue {
                repo: item.repo.clone(),
                number: item.number,
            };
            let mut detail = format!("{} · #{} · {}", item.repo, item.number, item.author);
            if item.comments > 0 {
                detail.push_str(&format!(
                    " · {} {}",
                    item.comments,
                    if item.comments == 1 {
                        "comment"
                    } else {
                        "comments"
                    }
                ));
            }
            body = body.child(
                self.overview_row(
                    format!("pilus-overview-issue-{index}"),
                    icon(
                        include_bytes!("../../assets/icons/circle-dot.svg"),
                        fonts.body,
                        theme.ok,
                    ),
                    item.title.clone(),
                    detail,
                    age(&item.updated_at).map(|when| self.overview_trail(when)),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.github_open(View::Item(subject.clone()), true, cx);
                })),
            );
        }
        card(
            theme,
            fonts,
            "ISSUES",
            Some(self.overview.issues.len()),
            body,
        )
    }

    /// A message's text as the chat page draws it (Markdown when it reads as
    /// Markdown, parsed once per text), cut at a height with a fade so a long
    /// answer does not push the rest of the page away.
    pub(super) fn overview_message(&self, text: &str, fonts: Fonts) -> Div {
        let theme = self.theme;
        let text = text.trim();
        let long = text.len() > MESSAGE_LONG || text.lines().count() > MESSAGE_LINES;
        let body = div().text_size(px(fonts.body)).child(self.chat_text(
            text,
            theme.panel_fg,
            fonts,
            &self.shared_settings,
        ));
        if !long {
            return div().child(body);
        }
        div()
            .relative()
            .max_h(px(style::scaled(&self.shared_settings, MESSAGE_HEIGHT)))
            .overflow_hidden()
            .child(body)
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .bottom_0()
                    .h(px(36.0))
                    .bg(linear_gradient(
                        180.0,
                        linear_color_stop(theme.panel_dim.opacity(0.0), 0.0),
                        linear_color_stop(theme.panel_dim, 1.0),
                    )),
            )
    }

    /// The open work of the daemons' boards, what waits on someone first. A row
    /// opens the daemon's primary agent, whose chat has the full board.
    fn overview_work_card(&self, work: &[OpenWork], cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = Fonts::resolve(&self.shared_settings);
        let mut body = div().flex().flex_col().gap_1();
        if work.is_empty() {
            body = body.child(quiet(
                theme,
                fonts,
                "Nothing open",
                "Your agents add an item when they take on work that outlasts a turn.",
            ));
        }
        let shown = work.len().min(WORK_ROWS);
        for (index, item) in work.iter().take(shown).enumerate() {
            let tint = match item.state.as_str() {
                "blocked" => theme.alert,
                // The theme's own yellow: waiting is not an error, but it is not fine either.
                "waiting" => theme.ansi[3],
                "doing" => theme.accent,
                _ => theme.muted,
            };
            let mut detail = vec![item.who.clone(), item.state.clone()];
            if !item.detail.is_empty() {
                detail.push(item.detail.clone());
            }
            let when = (item.updated_at > 0)
                .then(|| self.overview_trail(age_label(unix_now(), item.updated_at / 1000)));
            let watch = item.watch.clone();
            body = body.child(
                self.overview_row(
                    format!("pilus-overview-work-{index}"),
                    div().w(px(8.0)).h(px(8.0)).radius_full().bg(tint),
                    item.title.clone(),
                    detail.join(" · "),
                    when,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.open_sipil_chat(watch.clone(), cx);
                })),
            );
        }
        if shown < work.len() {
            body = body.child(
                div()
                    .px_2()
                    .pt_1()
                    .text_size(px(fonts.label))
                    .text_color(theme.muted)
                    .child(format!("{} more on the boards", work.len() - shown)),
            );
        }
        card(theme, fonts, "OPEN WORK", None, body)
    }

    fn overview_latest_card(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = Fonts::resolve(&self.shared_settings);
        let body = match &self.overview.latest {
            None => div().child(quiet(
                theme,
                fonts,
                "No messages yet",
                "The newest line from your contacts, channels and agents lands here.",
            )),
            Some(latest) => {
                let glyph: &'static [u8] = match &latest.watch {
                    Watch::Channel(_) => include_bytes!("../../assets/icons/hash.svg"),
                    Watch::On(..) => include_bytes!("../../assets/icons/pilus-mark.svg"),
                    _ => include_bytes!("../../assets/icons/user.svg"),
                };
                let watch = latest.watch.clone();
                let mut said = Vec::new();
                if !latest.who.is_empty() {
                    said.push(latest.who.clone());
                }
                if latest.at > 0 {
                    said.push(age_label(unix_now(), latest.at));
                }
                if latest.unread > 0 {
                    said.push(format!("{} unread", latest.unread));
                }
                div().child(
                    div()
                        .id("pilus-overview-latest")
                        .flex()
                        .flex_row()
                        .items_start()
                        .gap_3()
                        .px_2()
                        .py_2()
                        .radius_md()
                        .hover_fill(theme.panel, style::RADIUS_MD)
                        .cursor_pointer()
                        .child(div().w(px(16.0)).flex_shrink_0().pt(px(2.0)).child(icon(
                            glyph,
                            fonts.body,
                            theme.muted,
                        )))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w(px(0.0))
                                .gap_1()
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .items_baseline()
                                        .gap_2()
                                        .child(
                                            div()
                                                .text_size(px(fonts.body))
                                                .font_weight(FontWeight::BOLD)
                                                .text_color(theme.panel_fg)
                                                .child(latest.place.clone()),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(fonts.label))
                                                .text_color(theme.muted)
                                                .child(said.join(" · ")),
                                        ),
                                )
                                .child(self.overview_message(&latest.text, fonts)),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_sipil_chat(watch.clone(), cx);
                        })),
                )
            }
        };
        card(theme, fonts, "LATEST FROM SIPIL", None, body)
    }
}

/// Shot-harness entry point: the page over canned figures. It reads no daemon,
/// no GitHub and no index.
#[cfg(feature = "visual-test")]
impl PilusView {
    /// Scroll the page `down` pixels from its top.
    pub(super) fn overview_scroll_to(&mut self, down: f32, cx: &mut Context<Self>) {
        self.overview
            .scroll
            .set_offset(gpui::point(px(0.0), px(-down)));
        cx.notify();
    }

    /// Scroll the page to its end, as a long page is read.
    pub(super) fn overview_scroll_to_end(&mut self, cx: &mut Context<Self>) {
        self.overview
            .scroll
            .set_offset(gpui::point(px(0.0), px(-100_000.0)));
        cx.notify();
    }

    /// A capture's page: fixed agents, sessions, pull requests and message, so
    /// a shot never reaches a daemon, GitHub or the index.
    pub(super) fn overview_show_sample(
        &mut self,
        github: bool,
        sipil: bool,
        cx: &mut Context<Self>,
    ) {
        use pilus_index::Agent;

        self.leave_settings_page();
        self.home_begin();
        let now = i64::try_from(unix_now()).unwrap_or(0);
        let session = |agent, id: &str, title: &str, branch: &str, ago: i64| Session {
            machine: "local".to_string(),
            agent,
            session_id: id.to_string(),
            cwd: "/Users/me/Developer/pilus".to_string(),
            title: title.to_string(),
            branch: branch.to_string(),
            started: now - ago - 600,
            updated: now - ago,
            path: id.to_string(),
            size: 0,
        };
        let pull = |number, title: &str, author: &str, draft, ago: i64| ItemSummary {
            repo: "kepelet/pilus".to_string(),
            number,
            title: title.to_string(),
            author: author.to_string(),
            draft,
            branch: String::new(),
            updated_at: rfc3339(now - ago),
            comments: 0,
            url: String::new(),
        };
        self.settings.index_agents = true;
        *self.shared_settings.borrow_mut() = self.settings.clone();
        let page = &mut self.overview;
        page.open = true;
        let item = |who: &str, title: &str, state: &str, detail: &str, ago: i64| OpenWork {
            who: who.to_string(),
            title: title.to_string(),
            state: state.to_string(),
            detail: detail.to_string(),
            updated_at: u64::try_from(now - ago).unwrap_or(0) * 1000,
            watch: Watch::Peer("sample".to_string()),
        };
        page.sample = Some(Sample {
            github,
            sipil,
            work: vec![
                item(
                    "Researcher",
                    "Sign the 0.10 release build",
                    "blocked",
                    "waiting on the Developer ID certificate",
                    5_400,
                ),
                item(
                    "CoS",
                    "Draft the alpha announcement",
                    "waiting",
                    "Rina's review",
                    900,
                ),
                item(
                    "Researcher",
                    "Compare relay costs",
                    "doing",
                    "read the iroh pricing page",
                    120,
                ),
            ],
            agents: vec![
                Listed {
                    status: "working".to_string(),
                    glyph: include_bytes!("../../assets/icons/agents/claude-code.svg"),
                    name: "Claude".to_string(),
                    detail: "pilus · ~/D/k/pilus · home-page".to_string(),
                    target: Target::Pane {
                        endpoint: EndpointId::Local,
                        pane: "%1".to_string(),
                    },
                    lookup: None,
                },
                Listed {
                    status: "working".to_string(),
                    glyph: include_bytes!("../../assets/icons/agents/pi.svg"),
                    name: "Pi".to_string(),
                    detail: "sipil · ~/D/k/sipil · routines".to_string(),
                    target: Target::Pane {
                        endpoint: EndpointId::Local,
                        pane: "%2".to_string(),
                    },
                    lookup: None,
                },
                Listed {
                    status: "working".to_string(),
                    glyph: include_bytes!("../../assets/icons/pilus-mark.svg"),
                    name: "Researcher".to_string(),
                    detail: "sipil · ~/.sipil/workspace".to_string(),
                    target: Target::Pane {
                        endpoint: EndpointId::Local,
                        pane: "%3".to_string(),
                    },
                    lookup: None,
                },
                Listed {
                    status: "blocked".to_string(),
                    glyph: include_bytes!("../../assets/icons/agents/claude-code.svg"),
                    name: "Release notes".to_string(),
                    detail: "pilus · ~/D/k/pilus · alpha-0.10-readiness".to_string(),
                    target: Target::Pane {
                        endpoint: EndpointId::Local,
                        pane: "%4".to_string(),
                    },
                    lookup: None,
                },
                Listed {
                    status: "idle".to_string(),
                    glyph: include_bytes!("../../assets/icons/agents/pi.svg"),
                    name: "π - sipil".to_string(),
                    detail: "sipil · ~/D/k/sipil · main".to_string(),
                    target: Target::Pane {
                        endpoint: EndpointId::Local,
                        pane: "%5".to_string(),
                    },
                    lookup: None,
                },
            ],
        });
        page.agent = AgentPanel::sample(
            Target::Pane {
                endpoint: EndpointId::Local,
                pane: "%5".to_string(),
            },
            "π - sipil",
            super::overview_peek::Peek::Said {
                text: "The hub now **refuses a second `hub.run`** while one is live, and the daemon reports it instead of exiting.\n\n- added `Hub::busy`, with a test that races two runs\n- the stray-exit path logs the pid it found\n\nAll 127 tests pass. Want me to open the PR?".to_string(),
                at: u64::try_from(now - 540).unwrap_or(0),
            },
        );
        page.sessions = vec![
            session(
                Agent::Claude,
                "a",
                "Home page: working agents and pull requests",
                "home-page",
                90,
            ),
            session(
                Agent::Pi,
                "b",
                "Index agent sessions for search",
                "search-index",
                2_400,
            ),
            session(
                Agent::Claude,
                "c",
                "Sipil sidebar ordering, chat snapshots",
                "main",
                7_200,
            ),
            session(
                Agent::Claude,
                "d",
                "Harden hub.run against stray exits",
                "main",
                26_000,
            ),
            session(
                Agent::Pi,
                "e",
                "Draft the 0.10 alpha notes",
                "alpha-0.10-readiness",
                90_000,
            ),
        ];
        page.pulls = vec![
            pull(
                214,
                "feat(gui): Home page with activity",
                "rizaldy",
                false,
                600,
            ),
            pull(
                212,
                "perf(sipil): fewer wake-ups while idle",
                "rina",
                false,
                7_000,
            ),
            pull(209, "docs: SSH vs UHP", "rizaldy", true, 40_000),
            pull(
                203,
                "fix(search): resume on a remote machine",
                "dimas",
                false,
                200_000,
            ),
        ];
        page.issues = vec![
            pull(
                217,
                "Sessions list shows the wrong branch after a resume",
                "dimas",
                false,
                3_600,
            ),
            pull(
                211,
                "Hero image is slow to decode on first open",
                "rina",
                false,
                90_000,
            ),
            pull(
                198,
                "Pin repositories from the Home page",
                "rizaldy",
                false,
                400_000,
            ),
        ];
        page.pulls_loaded = true;
        page.pulls_error = None;
        page.latest = Some(Latest {
            watch: Watch::Channel("0123456789abcdef".to_string()),
            place: "#project-x".to_string(),
            who: "Researcher".to_string(),
            text: "I read through the **three open threads**:\n\n- the release notes can go out once the signing step is green\n- the alerts channel is waiting on `Rina`\n- the rest is [tracked here](https://github.com/kepelet/pilus/issues/198)\n\nWant me to draft the announcement?".to_string(),
            at: u64::try_from(now - 420).unwrap_or(0),
            unread: 3,
        });
        cx.notify();
    }
}

/// Shot-harness entry point: a session's page over a canned conversation. It
/// reads no transcript.
#[cfg(feature = "visual-test")]
impl PilusView {
    pub(super) fn session_show_sample(&mut self, origin: Origin, cx: &mut Context<Self>) {
        use pilus_index::{Agent, Entry};

        match origin {
            Origin::Home => self.overview_show_sample(false, false, cx),
            Origin::Search => self.search_show_sample(true, false, cx),
        }
        let now = i64::try_from(unix_now()).unwrap_or(0);
        let session = Session {
            machine: "local".to_string(),
            agent: Agent::Claude,
            session_id: "sample".to_string(),
            cwd: "/Users/me/Developer/pilus".to_string(),
            title: "Home sessions open on their own page".to_string(),
            branch: "alpha-0.10-readiness".to_string(),
            started: now - 7_800,
            updated: now - 3_600,
            path: "sample".to_string(),
            size: 0,
        };
        let say = |text: &str, ago: i64| Entry::Assistant {
            text: text.to_string(),
            at: now - ago,
        };
        let tool = |name: &str, summary: &str, output: &str, failed: bool| Entry::Tool {
            name: name.to_string(),
            summary: summary.to_string(),
            output: Some(output.to_string()),
            failed,
            at: 0,
        };
        let transcript = Transcript {
            earlier: true,
            entries: vec![
                Entry::User {
                    text: "Make a recent session open on its own page, with Continue and Fork."
                        .to_string(),
                    at: now - 7_700,
                },
                say(
                    "I'll start with how **Home** lists them and how *Search* resumes one.",
                    7_650,
                ),
                tool(
                    "Bash",
                    "grep -n resume crates/pilus/src/gui/search.rs",
                    "416:    pub(super) fn search_resume(&mut self, session: &Session, cx: &mut Context<Self>) {\n478:fn resume(",
                    false,
                ),
                tool(
                    "Bash",
                    "cargo test -p pilus-index transcript",
                    "error[E0425]: cannot find value `calls` in this scope",
                    true,
                ),
                // Long enough to be several rows, which must read as one card.
                say(
                    &"A paragraph that names `read_transcript_local`, `chunks` and `Place` keeps every word of it a separate element, which is what made a long reply slow to scroll.\n\n"
                        .repeat(10),
                    4_000,
                ),
                say(
                    "The page reads the end of the transcript, so a long session costs one seek:\n\n- **Continue** resumes it in its folder\n- **Fork** starts a new session from its history\n\n```rust\nlet transcript = read_transcript_local(&path, agent)?;\n```",
                    3_700,
                ),
            ],
        };
        self.overview.session = SessionView::sample(session, origin, transcript, &[2]);
        cx.notify();
    }
}

#[cfg(any(test, feature = "visual-test"))]
/// `then` (unix seconds) as the UTC timestamp GitHub writes, for the samples.
fn rfc3339(then: i64) -> String {
    let day = then.div_euclid(86_400);
    let secs = then.rem_euclid(86_400);
    let (year, month, date) = super::stats::civil(day);
    format!(
        "{year:04}-{:02}-{date:02}T{:02}:{:02}:{:02}Z",
        month + 1,
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pull(number: u64, updated_at: &str) -> ItemSummary {
        ItemSummary {
            repo: "o/r".to_string(),
            number,
            title: String::new(),
            author: String::new(),
            draft: false,
            branch: String::new(),
            updated_at: updated_at.to_string(),
            comments: 0,
            url: String::new(),
        }
    }

    #[test]
    fn the_most_recently_active_pull_request_leads() {
        let shown = newest(
            vec![
                pull(1, "2026-10-01T00:00:00Z"),
                pull(2, "2026-10-19T00:00:00Z"),
                pull(3, "2026-10-10T00:00:00Z"),
            ],
            PULLS,
        );
        let numbers: Vec<u64> = shown.iter().map(|item| item.number).collect();
        assert_eq!(numbers, [2, 3, 1]);
    }

    #[test]
    fn the_list_is_cut_at_its_limit_and_an_unreadable_date_sorts_last() {
        let mut items: Vec<ItemSummary> = (0..20)
            .map(|number| pull(number, "2026-10-05T00:00:00Z"))
            .collect();
        items.push(pull(99, "not a date"));
        let shown = newest(items, PULLS);
        assert_eq!(shown.len(), PULLS);
        assert!(shown.iter().all(|item| item.number != 99));
    }

    #[test]
    fn counts_agree_with_their_noun() {
        assert_eq!(
            counted(1, "agent working", "agents working"),
            "1 agent working"
        );
        assert_eq!(
            counted(3, "agent working", "agents working"),
            "3 agents working"
        );
    }

    #[test]
    fn a_sample_timestamp_reads_back() {
        let stamp = rfc3339(1_790_000_000);
        assert_eq!(unix_seconds(&stamp), Some(1_790_000_000));
    }
}
