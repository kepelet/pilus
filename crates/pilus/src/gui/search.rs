//! The native Search page: every Claude and Pi session on this Mac and on the
//! saved remote machines, found by title, folder or branch and resumed in a
//! click.
//!
//! The rows come from `pilus-index`, a SQLite file under the pilus state
//! directory that a background thread keeps in line with the agents' own
//! transcripts. The page owns the one decision the index cannot make for
//! itself, which is whether it may read them at all: until `index_agents` is
//! set the page is a consent card and nothing touches the disk or an SSH
//! host. Withdrawing the permission deletes the file.
//!
//! The first half holds the page's state; the second half renders.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use gpui::{Context, Div, FontWeight, KeyDownEvent, ScrollHandle, div, prelude::*, px};
use pilus_client::{ControlClient, SshTarget};
use pilus_index::{Agent, Filter, Index, Report, Session, Source};

use super::mission::row_fills;
use super::style::{self, ButtonKind, FillExt, RadiusExt};
use super::tasks::{age_label, unix_now};
use super::text_input::TextField;
use super::toast::ToastKind;
use super::{EndpointId, PilusView, icon};

/// The index's file name under the pilus state directory.
const INDEX_FILE: &str = "index.db";
/// The machine key of this Mac, as `Settings::index_machines` spells it.
const LOCAL: &str = "local";
/// How long an open page waits before reading the transcripts again.
const RESYNC: Duration = Duration::from_secs(30);
/// The most rows one query draws.
const LIMIT: usize = 200;
const ROW_HEIGHT: f32 = 46.0;

/// A machine the index could read, whether or not the person allowed it.
#[derive(Clone)]
pub(super) struct Choice {
    /// `local`, or the saved profile's id.
    pub(super) key: String,
    pub(super) label: String,
    /// How to reach it; `None` is this Mac.
    target: Option<SshTarget>,
}

/// What a background thread reports back.
enum Msg {
    /// One machine was read.
    Synced {
        machine: String,
        result: Result<Report, String>,
    },
    /// Every machine has been tried.
    Finished,
    /// A resume reached the daemon, or did not.
    Resumed {
        endpoint: EndpointId,
        title: String,
        result: Result<(), String>,
    },
}

/// The page's state.
pub(super) struct SearchPage {
    open: bool,
    /// The sources and permission panel, shown over the results.
    pub(super) sources: bool,
    /// Whether "Turn off" has been pressed once and awaits its second press.
    pub(super) confirm_off: bool,
    pub(super) field: TextField,
    /// The text the results were last drawn for.
    query: String,
    results: Vec<Session>,
    /// How many sessions the index holds, whatever the query.
    total: u64,
    selected: usize,
    filter: Filter,
    /// The window's own read connection, opened on first use.
    reader: Option<Index>,
    pub(super) syncing: bool,
    pub(super) synced_at: Option<Instant>,
    /// The last failure per machine key, cleared by its next good read.
    errors: BTreeMap<String, String>,
    resuming: bool,
    scroll: ScrollHandle,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

impl Default for SearchPage {
    fn default() -> Self {
        let (tx, rx) = channel();
        Self {
            open: false,
            sources: false,
            confirm_off: false,
            field: TextField::default(),
            query: String::new(),
            results: Vec::new(),
            total: 0,
            selected: 0,
            filter: Filter::default(),
            reader: None,
            syncing: false,
            synced_at: None,
            errors: BTreeMap::new(),
            resuming: false,
            scroll: ScrollHandle::new(),
            tx,
            rx,
        }
    }
}

impl SearchPage {
    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    pub(super) fn close(&mut self) {
        self.open = false;
        self.sources = false;
        self.confirm_off = false;
    }

    /// Whether the query field takes the keyboard: not while the permission
    /// panel is up, which has nothing to type into.
    pub(super) fn typing(&self, allowed: bool) -> bool {
        self.open && allowed && !self.sources
    }

    /// Move the highlight by `delta`, stopping at either end.
    fn step(&mut self, delta: isize) {
        let last = self.results.len().saturating_sub(1);
        self.selected = self.selected.saturating_add_signed(delta).min(last);
    }

    /// The query text the results were drawn for, so an edit can be told from
    /// the field's caret blinks.
    pub(super) fn query(&self) -> &str {
        &self.query
    }

    pub(super) fn set_query(&mut self, query: String) {
        self.query = query;
        self.selected = 0;
    }
}

impl PilusView {
    /// The index file this window reads and writes.
    pub(super) fn search_index_path(&self) -> PathBuf {
        self.pilus_root.join(INDEX_FILE)
    }

    /// Every machine the index could read: this Mac, then each saved profile
    /// that has a verified remote binary to reach it with.
    pub(super) fn search_choices(&self) -> Vec<Choice> {
        let mut choices = vec![Choice {
            key: LOCAL.to_string(),
            label: "This Mac".to_string(),
            target: None,
        }];
        choices.extend(self.machines.iter().filter_map(|machine| {
            Some(Choice {
                key: machine.id.clone(),
                label: machine.display().to_string(),
                target: Some(machine.ssh_target()?),
            })
        }));
        choices
    }

    /// The choices the person has allowed.
    pub(super) fn search_allowed(&self) -> Vec<Choice> {
        self.search_choices()
            .into_iter()
            .filter(|choice| self.settings.index_machines.contains(&choice.key))
            .collect()
    }

    /// Open the page. With permission it also starts reading.
    pub(super) fn open_search(&mut self, cx: &mut Context<Self>) {
        self.leave_settings_page();
        self.search.open = true;
        if self.settings.index_agents {
            self.search_sync();
            self.search_requery();
        } else {
            self.search_prepare();
        }
        cx.notify();
    }

    /// Offer this Mac ticked on the permission card. Nothing is read until
    /// the person presses Allow, but the choice is saved so a tick sticks.
    pub(super) fn search_prepare(&mut self) {
        if !self.settings.index_agents && self.settings.index_machines.is_empty() {
            self.settings.index_machines.push(LOCAL.to_string());
            self.save_settings();
        }
    }

    /// Give permission to read the ticked machines, and start.
    fn search_allow(&mut self, cx: &mut Context<Self>) {
        if self.settings.index_machines.is_empty() {
            return;
        }
        self.settings.index_agents = true;
        self.save_settings();
        self.search.sources = false;
        self.search_sync();
        self.search_requery();
        cx.notify();
    }

    /// Withdraw permission and delete everything indexed.
    fn search_turn_off(&mut self, cx: &mut Context<Self>) {
        self.settings.index_agents = false;
        self.save_settings();
        self.search.reader = None;
        pilus_index::wipe(&self.search_index_path());
        let page = &mut self.search;
        page.results.clear();
        page.total = 0;
        page.errors.clear();
        page.synced_at = None;
        page.sources = false;
        page.confirm_off = false;
        self.toast(ToastKind::Info, "Search index deleted");
        cx.notify();
    }

    /// Tick a machine on or off. Taking one off forgets what it contributed.
    fn search_toggle_source(&mut self, key: &str, cx: &mut Context<Self>) {
        let machines = &mut self.settings.index_machines;
        let allowed = if let Some(at) = machines.iter().position(|machine| machine == key) {
            machines.remove(at);
            false
        } else {
            machines.push(key.to_string());
            true
        };
        // Saved even before permission: the window re-reads the shared copy
        // each tick, which would otherwise undo the tick. `index_agents` alone
        // gates any reading.
        self.save_settings();
        if !self.settings.index_agents {
            cx.notify();
            return;
        }
        if allowed {
            self.search_sync();
        } else {
            if let Ok(mut index) = Index::open(&self.search_index_path()) {
                let _ = index.forget(key);
            }
            self.search.errors.remove(key);
            if self.search.filter.machine.as_deref() == Some(key) {
                self.search.filter.machine = None;
            }
        }
        self.search_requery();
        cx.notify();
    }

    /// Read the allowed machines' transcripts into the index, off the GUI
    /// thread and one machine after another, so a slow host never holds up
    /// this Mac's rows.
    pub(super) fn search_sync(&mut self) {
        if !self.settings.index_agents || self.search.syncing {
            return;
        }
        let choices = self.search_allowed();
        if choices.is_empty() {
            return;
        }
        self.search.syncing = true;
        let tx = self.search.tx.clone();
        let path = self.search_index_path();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        std::thread::spawn(move || {
            let index = Index::open(&path).map_err(|error| error.to_string());
            let mut index = match index {
                Ok(index) => index,
                Err(error) => {
                    for choice in &choices {
                        tx.send(Msg::Synced {
                            machine: choice.key.clone(),
                            result: Err(error.clone()),
                        })
                        .ok();
                    }
                    tx.send(Msg::Finished).ok();
                    return;
                }
            };
            for choice in choices {
                let source = match (&choice.target, &home) {
                    (Some(target), _) => Some(Source::Remote(target)),
                    (None, Some(home)) => Some(Source::Local(home.clone())),
                    (None, None) => None,
                };
                let result = match source {
                    Some(source) => pilus_index::refresh(&mut index, &choice.key, &source)
                        .map_err(|error| error.to_string()),
                    None => Err("no home directory".to_string()),
                };
                tx.send(Msg::Synced {
                    machine: choice.key,
                    result,
                })
                .ok();
            }
            tx.send(Msg::Finished).ok();
        });
    }

    /// Run the query against the index.
    pub(super) fn search_requery(&mut self) {
        if !self.settings.index_agents {
            return;
        }
        if self.search.reader.is_none() {
            match Index::open(&self.search_index_path()) {
                Ok(index) => self.search.reader = Some(index),
                Err(error) => {
                    self.search
                        .errors
                        .insert("index".to_string(), error.to_string());
                    return;
                }
            }
        }
        let page = &mut self.search;
        let Some(reader) = page.reader.as_ref() else {
            return;
        };
        match reader.search(&page.query, &page.filter, LIMIT) {
            Ok(found) => {
                page.errors.remove("index");
                page.results = found;
                page.total = reader.count(None).unwrap_or(0);
                page.selected = page.selected.min(page.results.len().saturating_sub(1));
            }
            Err(error) => {
                page.errors.insert("index".to_string(), error.to_string());
            }
        }
    }

    /// Drain the background threads and schedule the next read. Returns
    /// whether anything on screen changed.
    pub(super) fn search_tick(&mut self, cx: &mut Context<Self>) -> bool {
        let mut changed = false;
        while let Ok(message) = self.search.rx.try_recv() {
            changed = true;
            match message {
                Msg::Synced { machine, result } => match result {
                    Ok(report) => {
                        self.search.errors.remove(&machine);
                        if report.indexed + report.removed > 0 {
                            self.search_requery();
                            self.stats_reload();
                        }
                    }
                    Err(error) => {
                        self.search.errors.insert(machine, error);
                    }
                },
                Msg::Finished => {
                    self.search.syncing = false;
                    self.search.synced_at = Some(Instant::now());
                    self.search_requery();
                    self.stats_reload();
                }
                Msg::Resumed {
                    endpoint,
                    title,
                    result,
                } => {
                    self.search.resuming = false;
                    match result {
                        Ok(()) => {
                            self.toast(ToastKind::Info, format!("Resumed {title}"));
                            self.search.close();
                            self.activate_endpoint(endpoint, None, None, cx);
                        }
                        Err(error) => {
                            self.toast(ToastKind::Error, format!("Couldn't resume: {error}"));
                        }
                    }
                }
            }
        }
        let due = self
            .search
            .synced_at
            .is_none_or(|at| at.elapsed() >= RESYNC);
        let watched = self.search.open || self.stats.is_open();
        if watched && self.settings.index_agents && !self.search.syncing && due {
            self.search_sync();
        }
        changed
    }

    /// Reopen `session` in a pane: its folder as a workspace on its machine
    /// (or the one already open), then the agent's own resume.
    fn search_resume(&mut self, session: &Session, cx: &mut Context<Self>) {
        if self.search.resuming {
            return;
        }
        let endpoint = self.endpoint_by_key(&session.machine);
        let Some(mut client) = self.control_client_for_endpoint(&endpoint) else {
            self.toast(ToastKind::Error, "Couldn't reach that machine's daemon");
            return;
        };
        self.search.resuming = true;
        let tx = self.search.tx.clone();
        let cwd = session.cwd.clone();
        let id = session.session_id.clone();
        let title = session.title.clone();
        let agent = session.agent;
        std::thread::spawn(move || {
            let result = resume(&mut client, agent, &cwd, &id).map_err(|error| error.to_string());
            tx.send(Msg::Resumed {
                endpoint,
                title,
                result,
            })
            .ok();
        });
        cx.notify();
    }

    /// The page's keys.
    pub(super) fn search_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        if event.keystroke.modifiers.platform {
            return;
        }
        match key {
            "escape" => {
                if self.search.sources && self.settings.index_agents {
                    self.search.sources = false;
                    self.search.confirm_off = false;
                } else {
                    self.search.close();
                }
            }
            "down" | "up" if self.settings.index_agents && !self.search.sources => {
                self.search.step(if key == "down" { 1 } else { -1 });
                self.search.scroll.scroll_to_item(self.search.selected);
            }
            "enter" if self.settings.index_agents && !self.search.sources => {
                if let Some(session) = self.search.results.get(self.search.selected).cloned() {
                    self.search_resume(&session, cx);
                }
            }
            _ => return,
        }
        cx.notify();
    }
}

/// Open `cwd` as a workspace when the session knows its folder, then resume.
///
/// `agent.resume` only knows the sessions the daemon recorded itself (through
/// its resume hook), which is a handful of the transcripts the index reads. For
/// any other, launch the agent with its own resume flag in the same folder.
fn resume(
    client: &mut ControlClient,
    agent: Agent,
    cwd: &str,
    id: &str,
) -> pilus_client::Result<()> {
    if !cwd.is_empty() {
        client.open_workspace(cwd)?;
    }
    match client.resume_agent(id) {
        Err(error) if error.to_string().contains("no resumable session") => {
            let Some(flag) = agent.resume_flag() else {
                return Err(error);
            };
            // A fresh workspace opens with one shell pane; reuse it rather than
            // splitting a second, empty terminal beside the agent.
            let pane = client.idle_pane().ok().flatten();
            client.start_agent(
                &launch_name(agent, id),
                agent.kind(),
                pane.as_deref(),
                &[flag.to_string(), id.to_string()],
            )?;
            if let Some(pane) = pane.as_deref() {
                client.focus_pane(pane).ok();
            }
            Ok(())
        }
        other => other,
    }
}

/// The pane alias for a resumed session. The daemon accepts only
/// `[a-z][a-z0-9_-]{0,31}`, and a title is free text, so the alias is the agent
/// and the start of the id: valid, and one per session.
fn launch_name(agent: Agent, id: &str) -> String {
    let short: String = id
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    format!("{}-{short}", agent.kind())
}

// -- Rendering -----------------------------------------------------------

impl PilusView {
    /// The page, drawn in place of the terminal card.
    pub(super) fn search_page(&mut self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let allowed = self.settings.index_agents;

        let status = if !allowed {
            "Off".to_string()
        } else if self.search.syncing {
            format!("{} sessions · indexing…", self.search.total)
        } else {
            format!("{} sessions", self.search.total)
        };
        let mut actions = div().flex().flex_row().items_center().gap_2();
        if allowed {
            actions = actions
                .child(
                    style::small_button(
                        "pilus-search-refresh",
                        "Refresh",
                        ButtonKind::Secondary,
                        theme,
                        fonts.label,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.search_sync();
                        cx.notify();
                    })),
                )
                .child(
                    style::small_button(
                        "pilus-search-sources",
                        "Sources",
                        ButtonKind::Secondary,
                        theme,
                        fonts.label,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.search.sources = !this.search.sources;
                        this.search.confirm_off = false;
                        cx.notify();
                    })),
                );
        }
        let close = style::icon_button(
            "pilus-search-close",
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
        .on_click(cx.listener(|this, _, _, cx| {
            this.search.close();
            cx.notify();
        }));

        let bar = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_end()
            .flex_shrink_0()
            .px_4()
            .pt_3()
            .child(close);
        if !allowed || self.search.sources {
            return div()
                .flex()
                .flex_col()
                .size_full()
                .min_h(px(0.0))
                .child(bar)
                .child(self.search_consent(cx));
        }
        let trailing = div()
            .flex()
            .flex_row()
            .items_center()
            .flex_shrink_0()
            .gap_3()
            .child(
                div()
                    .text_size(px(fonts.label))
                    .text_color(theme.muted)
                    .child(status),
            )
            .child(actions);
        div()
            .flex()
            .flex_col()
            .size_full()
            .min_h(px(0.0))
            .child(bar)
            .child(self.search_results(trailing, cx))
    }

    /// The permission card, and the sources panel once it has been given.
    pub(super) fn search_consent(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let allowed = self.settings.index_agents;

        let line = |text: &str| {
            div()
                .text_size(px(fonts.secondary))
                .text_color(theme.muted)
                .child(text.to_string())
        };
        let mut sources = div().flex().flex_col().gap_2();
        for choice in self.search_choices() {
            let on = self.settings.index_machines.contains(&choice.key);
            let key = choice.key.clone();
            let mut row = div().flex().flex_row().items_center().gap_3().child(
                style::checkbox(
                    format!("pilus-search-source-{}", choice.key),
                    on,
                    choice.label.clone(),
                    theme,
                    fonts.body,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.search_toggle_source(&key, cx);
                })),
            );
            if let Some(error) = self.search.errors.get(&choice.key) {
                row = row.child(
                    div()
                        .truncate()
                        .text_size(px(fonts.label))
                        .text_color(theme.alert)
                        .child(error.clone()),
                );
            }
            sources = sources.child(row);
        }

        let mut buttons = div().flex().flex_row().items_center().gap_2();
        if allowed {
            let confirm = self.search.confirm_off;
            buttons = buttons
                .child(
                    style::button(
                        "pilus-search-done",
                        "Done",
                        ButtonKind::Secondary,
                        theme,
                        fonts.body,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.search.sources = false;
                        this.search.confirm_off = false;
                        cx.notify();
                    })),
                )
                .child(
                    style::button(
                        "pilus-search-off",
                        if confirm {
                            "Press again to delete the index"
                        } else {
                            "Turn off and delete index"
                        },
                        ButtonKind::Danger,
                        theme,
                        fonts.body,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        if this.search.confirm_off {
                            this.search_turn_off(cx);
                        } else {
                            this.search.confirm_off = true;
                            cx.notify();
                        }
                    })),
                );
        } else {
            buttons = buttons.child(
                style::button_enabled(
                    "pilus-search-allow",
                    "Allow indexing",
                    ButtonKind::Primary,
                    theme,
                    fonts.body,
                    !self.settings.index_machines.is_empty(),
                )
                .on_click(cx.listener(|this, _, _, cx| this.search_allow(cx))),
            );
        }

        let card = style::section_card(theme)
            .gap_3()
            .px_4()
            .child(
                div()
                    .text_size(px(fonts.title))
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme.fg)
                    .child(if allowed {
                        "Sources"
                    } else {
                        "Index your agent sessions"
                    }),
            )
            .child(line(
                "pilus can index the sessions your coding agents have saved, so you can \
                 search them, resume one, and see how much you use each agent and model.",
            ))
            .child(line(
                "It reads the transcript files in ~/.claude/projects and ~/.pi/agent/sessions \
                 on each machine ticked below.",
            ))
            .child(line(
                "It keeps each session's title, folder, branch and times, plus per-hour \
                 token counts and model names, in ~/.pilus/index.db on this Mac. Message \
                 text is not stored.",
            ))
            .child(line(
                "A remote machine is read over SSH only while it is ticked. Turning \
                 indexing off deletes the index.",
            ))
            .child(style::section_label(theme, "SOURCES", fonts.label))
            .child(sources)
            .child(buttons);
        div()
            .id("pilus-search-consent")
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .child(card)
    }

    /// The query, its filters and the matching sessions.
    fn search_results(&self, trailing: Div, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let height = style::scaled(&self.shared_settings, style::CONTROL_HEIGHT);

        self.search_input.update(cx, |input, _| {
            input.set_style(
                theme,
                style::chrome_font(&self.shared_settings),
                px(fonts.body),
            );
        });
        let field = style::search_field(
            "pilus-search-field",
            true,
            theme,
            height,
            fonts.secondary,
            self.search_input.clone(),
        )
        .flex_1()
        .min_w(px(0.0))
        .text_size(px(fonts.body));

        let allowed = self.search_allowed();
        let mut filters = div().flex().flex_row().flex_wrap().items_center().gap_2();
        if allowed.len() > 1 {
            let mut machines = style::segmented(theme).child(
                style::segment(
                    "pilus-search-machine-all",
                    "All machines",
                    self.search.filter.machine.is_none(),
                    true,
                    theme,
                    fonts.label,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.search.filter.machine = None;
                    this.search_requery();
                    cx.notify();
                })),
            );
            for choice in &allowed {
                let key = choice.key.clone();
                machines = machines.child(
                    style::segment(
                        format!("pilus-search-machine-{}", choice.key),
                        choice.label.clone(),
                        self.search.filter.machine.as_deref() == Some(choice.key.as_str()),
                        true,
                        theme,
                        fonts.label,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.search.filter.machine = Some(key.clone());
                        this.search_requery();
                        cx.notify();
                    })),
                );
            }
            filters = filters.child(machines);
        }
        let mut agents = style::segmented(theme).child(
            style::segment(
                "pilus-search-agent-all",
                "All agents",
                self.search.filter.agent.is_none(),
                true,
                theme,
                fonts.label,
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.search.filter.agent = None;
                this.search_requery();
                cx.notify();
            })),
        );
        for agent in Agent::ALL {
            agents = agents.child(
                style::segment(
                    format!("pilus-search-agent-{}", agent.kind()),
                    agent.label(),
                    self.search.filter.agent == Some(agent),
                    true,
                    theme,
                    fonts.label,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.search.filter.agent = Some(agent);
                    this.search_requery();
                    cx.notify();
                })),
            );
        }
        filters = filters.child(agents);
        let filters = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_3()
            .child(filters.flex_1().min_w(px(0.0)))
            .child(trailing);

        let mut problems = div().flex().flex_col().gap_1();
        for (key, error) in &self.search.errors {
            let who = self
                .search_choices()
                .into_iter()
                .find(|choice| &choice.key == key)
                .map_or_else(|| key.clone(), |choice| choice.label);
            problems = problems.child(
                div()
                    .truncate()
                    .text_size(px(fonts.label))
                    .text_color(theme.alert)
                    .child(format!("{who}: {error}")),
            );
        }

        let now = unix_now();
        let many = allowed.len() > 1;
        let mut list = div()
            .id("pilus-search-list")
            .flex()
            .flex_col()
            .gap_1()
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .track_scroll(&self.search.scroll);
        for (index, session) in self.search.results.iter().enumerate() {
            list = list.child(self.search_row(index, session, many, now, cx));
        }
        if self.search.results.is_empty() {
            let (title, detail) = if self.search.syncing && self.search.total == 0 {
                (
                    "Indexing your sessions…",
                    "The first read of a large history can take a few seconds.",
                )
            } else if self.search.query.is_empty() {
                (
                    "No sessions yet",
                    "Sessions from Claude and Pi show up here once they have a prompt.",
                )
            } else {
                (
                    "No matching sessions",
                    "Try fewer words, or clear the filters.",
                )
            };
            list = list.child(
                style::empty_state(
                    theme,
                    fonts,
                    icon(
                        include_bytes!("../../assets/icons/search.svg"),
                        24.0,
                        theme.muted,
                    ),
                    title,
                    detail,
                )
                .py_6(),
            );
        }

        let hint = div()
            .flex_shrink_0()
            .text_size(px(fonts.label))
            .text_color(theme.muted)
            .child("↑↓ to move · Enter to resume · Esc to close");

        div().flex().flex_1().min_h(px(0.0)).justify_center().child(
            div()
                .flex()
                .flex_col()
                .gap_3()
                .size_full()
                .max_w(px(style::scaled(&self.shared_settings, 880.0)))
                .px_4()
                .pb_3()
                .child(div().flex().flex_row().child(field))
                .child(filters)
                .child(problems)
                .child(list)
                .child(hint),
        )
    }

    fn search_row(
        &self,
        index: usize,
        session: &Session,
        many: bool,
        now: u64,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let selected = index == self.search.selected;
        let (rest, hover) = row_fills(selected, theme);
        let glyph: &'static [u8] = match session.agent {
            Agent::Claude => include_bytes!("../../assets/icons/agents/claude-code.svg"),
            Agent::Pi => include_bytes!("../../assets/icons/agents/pi.svg"),
        };
        let mut meta = Vec::new();
        if !session.cwd.is_empty() {
            meta.push(self.shorten_cwd(&session.machine, &session.cwd));
        }
        if !session.branch.is_empty() {
            meta.push(session.branch.clone());
        }
        if many {
            let label = self
                .search_choices()
                .into_iter()
                .find(|choice| choice.key == session.machine)
                .map_or_else(|| session.machine.clone(), |choice| choice.label);
            meta.push(label);
        }
        let age = age_label(now, u64::try_from(session.updated).unwrap_or(0));
        let picked = session.clone();
        div()
            .id(format!("pilus-search-row-{index}"))
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .h(px(style::scaled(&self.shared_settings, ROW_HEIGHT)))
            .flex_shrink_0()
            .px_3()
            .radius_md()
            .state_fill(rest, hover, style::RADIUS_MD)
            .cursor_pointer()
            .child(icon(glyph, fonts.body, theme.muted))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.0))
                    .child(
                        div()
                            .truncate()
                            .text_size(px(fonts.body))
                            .text_color(theme.panel_fg)
                            .font_weight(if selected {
                                FontWeight::BOLD
                            } else {
                                FontWeight::MEDIUM
                            })
                            .child(session.title.clone()),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(fonts.label))
                            .text_color(theme.muted)
                            .child(meta.join(" · ")),
                    ),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(fonts.label))
                    .text_color(theme.muted)
                    .child(age),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.search.selected = index;
                this.search_resume(&picked, cx);
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_name_is_a_valid_daemon_alias() {
        let name = launch_name(Agent::Claude, "34818B53-54dc-4921-b157-d43f1cf0ffc3");
        assert_eq!(name, "claude-34818b53");
        assert!(
            name.starts_with(|c: char| c.is_ascii_lowercase())
                && name.len() <= 32
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        );
        assert_eq!(launch_name(Agent::Claude, ""), "claude-");
    }

    fn session(id: &str) -> Session {
        Session {
            machine: LOCAL.to_string(),
            agent: Agent::Pi,
            session_id: id.to_string(),
            cwd: String::new(),
            title: id.to_string(),
            branch: String::new(),
            started: 0,
            updated: 0,
            path: id.to_string(),
            size: 0,
        }
    }

    #[test]
    fn the_highlight_stops_at_either_end() {
        let mut page = SearchPage {
            results: vec![session("a"), session("b"), session("c")],
            ..SearchPage::default()
        };
        page.step(-1);
        assert_eq!(page.selected, 0);
        page.step(1);
        page.step(1);
        page.step(1);
        assert_eq!(page.selected, 2);
    }

    #[test]
    fn stepping_an_empty_list_stays_put() {
        let mut page = SearchPage::default();
        page.step(1);
        assert_eq!(page.selected, 0);
    }

    #[test]
    fn the_field_takes_the_keyboard_only_with_permission_and_no_panel() {
        let mut page = SearchPage::default();
        assert!(!page.typing(true), "closed");
        page.open = true;
        assert!(page.typing(true));
        assert!(!page.typing(false), "no permission");
        page.sources = true;
        assert!(!page.typing(true), "the sources panel has no field");
    }
}

#[cfg(feature = "visual-test")]
impl PilusView {
    /// Shot-harness entry point: the page over canned sessions. It never
    /// reads the transcripts, writes the index or saves the settings, so a
    /// capture touches nothing on the machine.
    pub(super) fn search_show_sample(
        &mut self,
        allowed: bool,
        panel: bool,
        cx: &mut Context<Self>,
    ) {
        self.leave_settings_page();
        self.settings.index_agents = allowed;
        self.settings.index_machines = vec![LOCAL.to_string()];
        *self.shared_settings.borrow_mut() = self.settings.clone();
        let now = i64::try_from(unix_now()).unwrap_or(0);
        let sample = |agent, id: &str, title: &str, cwd: &str, branch: &str, ago: i64| Session {
            machine: LOCAL.to_string(),
            agent,
            session_id: id.to_string(),
            cwd: cwd.to_string(),
            title: title.to_string(),
            branch: branch.to_string(),
            started: now - ago - 600,
            updated: now - ago,
            path: id.to_string(),
            size: 0,
        };
        let page = &mut self.search;
        page.open = true;
        page.sources = panel;
        page.results = vec![
            sample(
                Agent::Claude,
                "a",
                "Sipil sidebar ordering, chat snapshots",
                "/Users/me/Developer/pilus",
                "main",
                120,
            ),
            sample(
                Agent::Pi,
                "b",
                "Index agent sessions for search",
                "/Users/me/Developer/pilus",
                "search-index",
                3_000,
            ),
            sample(
                Agent::Claude,
                "c",
                "Custom relay URL on iOS",
                "/Users/me/.luvus/worktrees/pilus/lucky-kite",
                "lucky-kite",
                40_000,
            ),
            sample(
                Agent::Pi,
                "d",
                "Refactor the connector retry loop",
                "/Users/me/Developer/sipil",
                "main",
                200_000,
            ),
            sample(
                Agent::Claude,
                "e",
                "Release the app via the justfile",
                "/Users/me/Developer/pilus",
                "main",
                900_000,
            ),
        ];
        page.total = 492;
        page.selected = 0;
        page.synced_at = Some(Instant::now());
        page.syncing = false;
        cx.notify();
    }
}
