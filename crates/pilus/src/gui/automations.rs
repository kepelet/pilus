//! The native Automations page: the daemon's scheduled definitions as their
//! own list, rather than a strip borrowed from another page.
//!
//! The rows and the scheduler's health come from `mission.snapshot` on the
//! active endpoint, the same model the daemon's dashboard draws; the page
//! never asks for a usage scan, so the read stays cheap and is re-read on a
//! short cadence while the page is open. Selecting a row loads its full
//! definition, next deadlines, and run history (`automation.get`,
//! `automation.preview`, `automation.history`). Run now, Pause, and Enable
//! are UHP calls made off the GUI thread; the next read redraws the result,
//! so the page never edits a row locally.
//!
//! The first half holds the page's state and the pure ordering rules,
//! unit-tested without GPUI; the second half renders.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, ClickEvent, Context, Div, FontWeight, KeyDownEvent, Rgba, SharedString, div,
    prelude::*, px,
};
use pilus_client::{AutomationDetail, AutomationHealth, AutomationRow, MissionScope};

use super::mission::{row_fills, step_key, unix_now, until_label, utc_label};
use super::style::{self, ButtonKind, FillExt, RadiusExt};
use super::theme::Theme;
use super::{EndpointId, PilusView};

/// How often an open page re-reads the rows.
const REFRESH: Duration = Duration::from_secs(3);

const DETAIL_WIDTH: f32 = 380.0;
const ROW_HEIGHT: f32 = 34.0;
const STATE_WIDTH: f32 = 104.0;
const AGENT_WIDTH: f32 = 96.0;
const NEXT_WIDTH: f32 = 72.0;
/// Run now and Pause/Enable, plus the cell padding.
const ACTIONS_WIDTH: f32 = 136.0;

/// The tint for an automation state.
pub(super) fn automation_color(state: &str, theme: Theme) -> Rgba {
    match state {
        "running" => theme.accent,
        "restoring" => theme.branch,
        "review" | "failed" | "unavailable" | "needs_rebind" => theme.alert,
        "scheduled" | "completed" => theme.ok,
        _ => theme.muted,
    }
}

/// An automation state as a label.
pub(super) fn automation_label(state: &str) -> String {
    match state {
        "needs_rebind" => "Needs rebind".to_string(),
        other => super::status_label(other),
    }
}

/// The scheduler's headline: what it needs from you first.
pub(super) fn automation_signal(health: AutomationHealth, theme: Theme) -> (&'static str, Rgba) {
    if health.review > 0 || health.failed > 0 {
        ("Attention", theme.alert)
    } else if health.running > 0 {
        ("Running", theme.accent)
    } else if health.scheduled > 0 {
        ("Scheduled", theme.ok)
    } else {
        ("Idle", theme.muted)
    }
}

/// Where a state sorts: what needs the user, then what is running, then
/// what is armed, then what will not fire on its own.
fn attention_rank(state: &str) -> u8 {
    match state {
        "review" | "failed" | "needs_rebind" | "unavailable" => 0,
        "running" | "restoring" => 1,
        "scheduled" => 2,
        _ => 3,
    }
}

/// The rows in display order: by attention, then the soonest deadline, then
/// name, so a list that only changes deadlines keeps its shape.
fn ordered(rows: &[AutomationRow]) -> Vec<&AutomationRow> {
    let mut rows: Vec<&AutomationRow> = rows.iter().collect();
    rows.sort_by(|a, b| {
        attention_rank(&a.state)
            .cmp(&attention_rank(&b.state))
            .then_with(|| {
                a.next_run_at
                    .unwrap_or(u64::MAX)
                    .cmp(&b.next_run_at.unwrap_or(u64::MAX))
            })
            .then_with(|| a.name.cmp(&b.name))
    });
    rows
}

/// A message from a background request.
pub(super) enum AutomationsMsg {
    Rows {
        generation: u64,
        result: Result<(AutomationHealth, Vec<AutomationRow>), String>,
    },
    Detail {
        id: String,
        result: Box<Result<AutomationDetail, String>>,
    },
    /// An action landed (or failed); re-read so the rows catch up.
    Done(String),
}

/// The page's own state; the rows live on the last read.
pub(super) struct AutomationsPanel {
    open: bool,
    /// The endpoint the rows were read from; a switch drops them.
    endpoint: Option<EndpointId>,
    health: AutomationHealth,
    rows: Option<Vec<AutomationRow>>,
    /// Bumped on every endpoint change, so a read of the old one that lands
    /// late is dropped instead of flashing stale rows.
    generation: u64,
    in_flight: bool,
    fetched_at: Option<Instant>,
    error: Option<String>,
    /// The selected definition, by id; the side pane shows it.
    selected: Option<String>,
    /// The definition the side pane last showed, kept while it slides shut.
    shown: Option<String>,
    /// The side pane's spring, so it slides rather than pops.
    slide: style::PaneSlide,
    detail: Option<(String, Option<Result<AutomationDetail, String>>)>,
    /// Whether the machine picker's list is open.
    machine_menu: bool,
    status: String,
    tx: Sender<AutomationsMsg>,
    rx: Receiver<AutomationsMsg>,
}

impl Default for AutomationsPanel {
    fn default() -> Self {
        let (tx, rx) = channel();
        Self {
            open: false,
            endpoint: None,
            health: AutomationHealth::default(),
            rows: None,
            generation: 0,
            in_flight: false,
            fetched_at: None,
            error: None,
            selected: None,
            shown: None,
            slide: style::PaneSlide::default(),
            detail: None,
            machine_menu: false,
            status: String::new(),
            tx,
            rx,
        }
    }
}

impl AutomationsPanel {
    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    pub(super) fn close(&mut self) {
        self.open = false;
        self.selected = None;
        self.detail = None;
        self.shown = None;
        self.slide = style::PaneSlide::default();
        self.machine_menu = false;
        self.status.clear();
    }

    /// Follow a new endpoint, dropping rows that belonged to the old one.
    fn follow(&mut self, endpoint: &EndpointId) {
        if self.endpoint.as_ref() == Some(endpoint) {
            return;
        }
        self.endpoint = Some(endpoint.clone());
        self.generation += 1;
        self.health = AutomationHealth::default();
        self.rows = None;
        self.error = None;
        self.in_flight = false;
        self.fetched_at = None;
        self.selected = None;
        self.detail = None;
    }

    /// Row ids in display order, for keyboard navigation.
    fn ids(&self) -> Vec<String> {
        self.rows
            .as_deref()
            .map(|rows| {
                ordered(rows)
                    .into_iter()
                    .map(|row| row.id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn row(&self, id: &str) -> Option<&AutomationRow> {
        self.rows.as_ref()?.iter().find(|row| row.id == id)
    }

    /// Whether `id` is armed: the loaded definition's word, else the row's.
    fn enabled(&self, id: &str) -> Option<bool> {
        match &self.detail {
            Some((open, Some(Ok(detail)))) if open == id => Some(detail.enabled),
            _ => self.row(id).map(|row| row.enabled),
        }
    }
}

impl PilusView {
    /// Shot-harness entry point: open on canned rows, no daemon.
    #[cfg(feature = "visual-test")]
    pub(super) fn automations_show(
        &mut self,
        health: AutomationHealth,
        rows: Vec<AutomationRow>,
        detail: Option<AutomationDetail>,
        cx: &mut Context<Self>,
    ) {
        self.leave_settings_page();
        let active = self.active.clone();
        let panel = &mut self.automations_page;
        panel.endpoint = Some(active);
        panel.open = true;
        panel.health = health;
        panel.rows = Some(rows);
        panel.selected = detail.as_ref().map(|detail| detail.id.clone());
        panel.detail = detail.map(|detail| (detail.id.clone(), Some(Ok(detail))));
        // Hold the canned rows: no daemon will answer a read.
        panel.in_flight = true;
        cx.notify();
    }

    /// Shot-harness entry point: open the machine picker's list.
    #[cfg(feature = "visual-test")]
    pub(super) fn automations_show_machines(&mut self, cx: &mut Context<Self>) {
        self.automations_page.machine_menu = true;
        cx.notify();
    }

    /// Drain background answers and schedule the next read. Returns whether
    /// anything changed on screen.
    pub(super) fn automations_tick(&mut self) -> bool {
        let mut changed = false;
        while let Ok(message) = self.automations_page.rx.try_recv() {
            changed = true;
            self.automations_receive(message);
        }
        if !self.automations_page.open {
            return changed;
        }
        // A machine switch lands after the click: the rows follow `active`.
        if self.automations_page.endpoint.as_ref() != Some(&self.active) {
            let active = self.active.clone();
            self.automations_page.follow(&active);
            changed = true;
        }
        let panel = &self.automations_page;
        let due = panel.fetched_at.is_none_or(|at| at.elapsed() >= REFRESH);
        if due && !panel.in_flight {
            self.automations_fetch();
        }
        changed
    }

    fn automations_receive(&mut self, message: AutomationsMsg) {
        let panel = &mut self.automations_page;
        match message {
            AutomationsMsg::Rows { generation, result } => {
                if generation != panel.generation {
                    return;
                }
                panel.in_flight = false;
                panel.fetched_at = Some(Instant::now());
                match result {
                    Ok((health, rows)) => {
                        panel.error = None;
                        panel.health = health;
                        panel.rows = Some(rows);
                    }
                    Err(error) => panel.error = Some(error),
                }
                // A deleted definition releases the side pane rather than
                // pointing at nothing.
                if let Some(id) = panel.selected.clone()
                    && panel.row(&id).is_none()
                    && panel.rows.is_some()
                {
                    panel.selected = None;
                    panel.detail = None;
                }
            }
            AutomationsMsg::Detail { id, result } => {
                if let Some((open, slot)) = &mut panel.detail
                    && *open == id
                {
                    *slot = Some(*result);
                }
            }
            AutomationsMsg::Done(text) => {
                panel.status = text;
                panel.fetched_at = None;
                // A run or pause changes the definition the pane shows.
                if let Some(id) = panel.selected.clone() {
                    self.automations_load_detail(id);
                }
            }
        }
    }

    fn automations_fetch(&mut self) {
        let Some(client) = self.control_client_for_endpoint(&self.active) else {
            let panel = &mut self.automations_page;
            panel.error = Some("No control connection to this machine".to_string());
            panel.fetched_at = Some(Instant::now());
            return;
        };
        let panel = &mut self.automations_page;
        panel.in_flight = true;
        let generation = panel.generation;
        let tx = panel.tx.clone();
        std::thread::spawn(move || {
            let mut client = client;
            let result = client
                .mission_snapshot(MissionScope::All, None)
                .map(|snapshot| (snapshot.automation, snapshot.automations))
                .map_err(|error| error.to_string());
            tx.send(AutomationsMsg::Rows { generation, result }).ok();
        });
    }

    fn automations_refresh(&mut self, cx: &mut Context<Self>) {
        let panel = &mut self.automations_page;
        panel.fetched_at = None;
        panel.status.clear();
        if let Some(id) = panel.selected.clone() {
            self.automations_load_detail(id);
        }
        cx.notify();
    }

    fn automations_select(&mut self, id: Option<String>, cx: &mut Context<Self>) {
        let panel = &mut self.automations_page;
        if panel.selected == id {
            return;
        }
        panel.selected = id.clone();
        panel.detail = id.clone().map(|id| (id, None));
        if let Some(id) = id {
            self.automations_load_detail(id);
        }
        cx.notify();
    }

    fn automations_load_detail(&self, id: String) {
        let tx = self.automations_page.tx.clone();
        let client = self.control_client_for_endpoint(&self.active);
        std::thread::spawn(move || {
            let result = match client {
                Some(mut client) => client
                    .automation_detail(&id)
                    .map_err(|error| error.to_string()),
                None => Err("Could not reach the daemon".to_string()),
            };
            tx.send(AutomationsMsg::Detail {
                id,
                result: Box::new(result),
            })
            .ok();
        });
    }

    /// Run a definition now (`run`), or arm or pause it.
    fn automations_op(&mut self, id: String, run: bool, enable: bool, cx: &mut Context<Self>) {
        let name = self
            .automations_page
            .row(&id)
            .map(|row| row.name.clone())
            .unwrap_or_else(|| id.clone());
        let tx = self.automations_page.tx.clone();
        let client = self.control_client_for_endpoint(&self.active);
        self.automations_page.status = if run {
            format!("Requesting a run of {name}…")
        } else if enable {
            format!("Enabling {name}…")
        } else {
            format!("Pausing {name}…")
        };
        std::thread::spawn(move || {
            let call = |client: &mut pilus_client::ControlClient| {
                if run {
                    client.run_automation(&id)?;
                    Ok(format!("Run requested for {name}"))
                } else {
                    client.set_automation_enabled(&id, enable)?;
                    Ok(format!(
                        "{} {name}",
                        if enable { "Enabled" } else { "Paused" }
                    ))
                }
            };
            let text = match client {
                Some(mut client) => {
                    call(&mut client).unwrap_or_else(|error: pilus_client::Error| error.to_string())
                }
                None => "Could not reach the daemon".to_string(),
            };
            tx.send(AutomationsMsg::Done(text)).ok();
        });
        cx.notify();
    }

    /// Flip the selected definition between armed and paused.
    fn automations_toggle_selected(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.automations_page.selected.clone() else {
            return;
        };
        if let Some(enabled) = self.automations_page.enabled(&id) {
            self.automations_op(id, false, !enabled, cx);
        }
    }

    /// The page's keys.
    pub(super) fn automations_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let mods = event.keystroke.modifiers;
        if mods.platform || mods.control {
            return;
        }
        let ids = self.automations_page.ids();
        let current = self.automations_page.selected.clone();
        let step = |delta| step_key(&ids, current.as_deref(), delta);
        match key {
            "down" | "j" => self.automations_select(step(1), cx),
            "up" | "k" => self.automations_select(step(-1), cx),
            "r" => self.automations_refresh(cx),
            "n" => {
                if let Some(id) = current {
                    self.automations_op(id, true, false, cx);
                }
            }
            "p" | "space" => self.automations_toggle_selected(cx),
            "q" => {
                self.automations_page.close();
                cx.notify();
            }
            "escape" => {
                let panel = &mut self.automations_page;
                if std::mem::take(&mut panel.machine_menu) {
                    cx.notify();
                    return;
                }
                if panel.selected.take().is_none() {
                    panel.close();
                }
                cx.notify();
            }
            _ => {}
        }
    }

    /// The page, drawn in place of the terminal card.
    pub(super) fn automations_page(&mut self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let panel = &self.automations_page;
        let loaded = panel.rows.is_some();

        let mut main = div()
            .id("pilus-automations-scroll")
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.0))
            .overflow_y_scroll();
        main = match (&panel.rows, &panel.error) {
            (Some(rows), _) if rows.is_empty() => main.child(
                self.automations_message(
                    "No automations yet",
                    "Scheduled agent runs you define on this machine show up here with \
                 their next deadline and run history."
                        .to_string(),
                ),
            ),
            (Some(_), _) => main
                .child(self.automations_metrics())
                .child(self.automations_list(cx)),
            (None, Some(error)) => {
                main.child(self.automations_message("Automations are unavailable", error.clone()))
            }
            (None, None) => main.child(self.automations_skeleton()),
        };

        let mut body = div().flex().flex_row().flex_1().min_h(px(0.0)).child(main);
        let open = loaded && self.automations_page.selected.is_some();
        if open {
            self.automations_page.shown = self.automations_page.selected.clone();
        }
        let slide = self.automations_page.slide.sync(open, cx);
        if slide.shown()
            && let Some(id) = self.automations_page.shown.clone()
        {
            let width = style::scaled(&self.shared_settings, DETAIL_WIDTH);
            body = body.child(slide.wrap(width, self.automations_side(&id, cx)));
        }
        let panel = &self.automations_page;

        let hints = "↑↓ navigate · n run now · p pause/enable · r refresh · esc back";
        let footer_text = if panel.status.is_empty() {
            hints.to_string()
        } else {
            panel.status.clone()
        };
        let (signal, color) = automation_signal(panel.health, theme);
        let status = div()
            .flex()
            .flex_row()
            .items_center()
            .flex_shrink_0()
            .gap_2()
            .pr_3()
            .mr_3()
            .border_r_1()
            .border_color(theme.border)
            .text_color(color)
            .child(div().w(px(8.0)).h(px(8.0)).radius_full().bg(color))
            .child(signal);
        let footer = div()
            .px_4()
            .h(px(style::scaled(&self.shared_settings, 28.0)))
            .flex()
            .items_center()
            .flex_shrink_0()
            .border_t_1()
            .border_color(theme.border)
            .text_size(px(fonts.secondary))
            .text_color(theme.muted)
            .child(status)
            .child(div().flex_1().min_w(px(0.0)).truncate().child(footer_text));

        div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .min_h(px(0.0))
            .child(self.automations_header(cx))
            .child(body)
            .child(footer)
    }

    fn automations_header(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let loading = self.automations_page.in_flight && self.automations_page.rows.is_none();
        let machines = self.machine_picker(
            "automations",
            self.automations_page.machine_menu,
            |this| &mut this.automations_page.machine_menu,
            cx,
        );
        let refresh = style::small_button(
            "pilus-automations-refresh",
            div()
                .flex()
                .items_center()
                .gap_2()
                .children(loading.then(|| {
                    style::spinner(
                        "pilus-automations-refresh-spinner",
                        fonts.secondary,
                        theme.muted,
                    )
                }))
                .child("Refresh"),
            ButtonKind::Secondary,
            theme,
            fonts.secondary,
        )
        .on_click(cx.listener(|this, _, _, cx| this.automations_refresh(cx)));
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .h(px(style::scaled(&self.shared_settings, 48.0)))
            .flex_shrink_0()
            .px_4()
            .border_b_1()
            .border_color(theme.border)
            .child(style::title(theme, "Automations", fonts.title))
            .child(div().flex_1())
            .children(machines)
            .child(refresh)
    }

    /// The scheduler's counts as headline tiles.
    fn automations_metrics(&self) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let health = self.automations_page.health;
        let now = unix_now();
        let (next, next_detail) = match health.next_run_at {
            Some(at) => (until_label(now, at), utc_label(at)),
            None => ("—".to_string(), "no upcoming run".to_string()),
        };
        let attention = health.review + health.failed;
        style::stat_strip(theme)
            .border_l_0()
            .child(style::stat_tile(
                theme,
                fonts,
                "ARMED",
                format!("{:02}", health.scheduled),
                if health.scheduled > 0 {
                    theme.ok
                } else {
                    theme.muted
                },
                format!("{} of {} enabled", health.enabled, health.definitions),
            ))
            .child(style::stat_tile(
                theme,
                fonts,
                "RUNNING",
                format!("{:02}", health.running),
                if health.running > 0 {
                    theme.accent
                } else {
                    theme.muted
                },
                "pending, starting, or live",
            ))
            .child(style::stat_tile(
                theme,
                fonts,
                "ATTENTION",
                format!("{attention:02}"),
                if attention > 0 { theme.alert } else { theme.ok },
                format!("{} review · {} failed", health.review, health.failed),
            ))
            .child(
                style::stat_tile(theme, fonts, "NEXT RUN", next, theme.panel_fg, next_detail)
                    .border_r_0(),
            )
    }

    /// The definitions, most urgent first, as the same full-width table the
    /// Mission Control roster draws.
    fn automations_list(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let cell = |width: Option<f32>, last: bool| {
            style::table_cell(&self.shared_settings, theme, width, last)
        };
        let label = |text: &'static str, width: Option<f32>, right: bool, last: bool| {
            let cell = cell(width, last)
                .overflow_hidden()
                .text_size(px(fonts.label))
                .font_weight(FontWeight::BOLD)
                .text_color(theme.muted);
            if right {
                cell.justify_end().child(text)
            } else {
                cell.child(text)
            }
        };
        let header = div()
            .flex()
            .flex_row()
            .items_stretch()
            .h(px(style::scaled(&self.shared_settings, 26.0)))
            .bg(theme.panel_dim)
            .border_b_1()
            .border_color(theme.border)
            .child(label("STATE", Some(STATE_WIDTH), false, false))
            .child(label("NAME", None, false, false))
            .child(label("AGENT", Some(AGENT_WIDTH), false, false))
            .child(label("NEXT", Some(NEXT_WIDTH), true, false))
            .child(label("ACTIONS", Some(ACTIONS_WIDTH), false, true));
        let now = unix_now();
        let mut list = div()
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(theme.border)
            .child(header);
        if let Some(rows) = &self.automations_page.rows {
            for row in ordered(rows) {
                list = list.child(self.automations_row(row, now, cx));
            }
        }
        div()
            .flex()
            .flex_col()
            .child(
                style::section_label(theme, "AUTOMATIONS", fonts.label)
                    .px_4()
                    .pt_4()
                    .pb_2(),
            )
            .child(list)
    }

    fn automations_row(
        &self,
        row: &AutomationRow,
        now: u64,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let cell = |width: Option<f32>, last: bool| {
            style::table_cell(&self.shared_settings, theme, width, last)
        };
        let selected = self.automations_page.selected.as_deref() == Some(row.id.as_str());
        let (rest, hover) = row_fills(selected, theme);
        let color = automation_color(&row.state, theme);
        let next = row
            .next_run_at
            .map(|at| until_label(now, at))
            .unwrap_or_else(|| "—".to_string());
        let name = div()
            .flex()
            .flex_col()
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
                    .child(row.name.clone()),
            )
            .children(row.latest_error.clone().map(|error| {
                div()
                    .truncate()
                    .text_size(px(fonts.label))
                    .text_color(theme.alert)
                    .child(error)
            }));
        let run_id = row.id.clone();
        let toggle_id = row.id.clone();
        let enabled = row.enabled;
        let actions = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .child(
                style::small_button(
                    format!("pilus-automations-run-{}", row.id),
                    "Run",
                    ButtonKind::Ghost,
                    theme,
                    fonts.secondary,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.automations_op(run_id.clone(), true, false, cx);
                })),
            )
            .child(
                style::small_button(
                    format!("pilus-automations-toggle-{}", row.id),
                    if enabled { "Pause" } else { "Enable" },
                    ButtonKind::Ghost,
                    theme,
                    fonts.secondary,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.automations_op(toggle_id.clone(), false, !enabled, cx);
                })),
            );
        let id = row.id.clone();
        div()
            .id(format!("pilus-automations-row-{}", row.id))
            .flex()
            .flex_row()
            .items_stretch()
            .min_h(px(style::scaled(&self.shared_settings, ROW_HEIGHT)))
            .cursor_pointer()
            .state_fill(rest, hover, style::Radius::None)
            .child(
                cell(Some(STATE_WIDTH), false)
                    .gap_2()
                    .text_size(px(fonts.secondary))
                    .text_color(color)
                    .child(
                        div()
                            .w(px(8.0))
                            .h(px(8.0))
                            .flex_shrink_0()
                            .radius_full()
                            .bg(color),
                    )
                    .child(automation_label(&row.state)),
            )
            .child(cell(None, false).py_1().child(name))
            .child(
                cell(Some(AGENT_WIDTH), false)
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(div().min_w(px(0.0)).truncate().child(row.agent_id.clone())),
            )
            .child(
                cell(Some(NEXT_WIDTH), false)
                    .justify_end()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(next),
            )
            .child(cell(Some(ACTIONS_WIDTH), true).px_1().child(actions))
            .border_b_1()
            .border_color(theme.border)
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                let next = (this.automations_page.selected.as_deref() != Some(id.as_str()))
                    .then(|| id.clone());
                this.automations_select(next, cx);
            }))
    }

    /// The side pane: the selected definition and its actions.
    fn automations_side(&self, id: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .h(px(style::scaled(&self.shared_settings, 40.0)))
            .flex_shrink_0()
            .px_4()
            .border_b_1()
            .border_color(theme.border)
            .child(style::section_label(theme, "AUTOMATION", fonts.label))
            .child(
                style::small_button(
                    "pilus-automations-side-close",
                    "Close",
                    ButtonKind::Ghost,
                    theme,
                    fonts.secondary,
                )
                .on_click(cx.listener(|this, _, _, cx| this.automations_select(None, cx))),
            );
        style::enter(
            "pilus-automations-side",
            div()
                .flex()
                .flex_col()
                .w_full()
                .min_h(px(0.0))
                .border_l_1()
                .border_color(theme.border)
                .child(header)
                .child(
                    div()
                        .id("pilus-automations-side-scroll")
                        .flex_1()
                        .min_h(px(0.0))
                        .overflow_y_scroll()
                        .child(div().p_4().child(self.automations_detail(id))),
                )
                .children(self.automations_actions(id, cx)),
        )
        .into_any_element()
    }

    /// One labelled line of a property table.
    fn automations_property(&self, label: &'static str, value: impl IntoElement) -> Div {
        let fonts = style::Fonts::resolve(&self.shared_settings);
        div()
            .flex()
            .flex_row()
            .items_start()
            .gap_3()
            .min_h(px(style::scaled(&self.shared_settings, 22.0)))
            .child(
                div()
                    .w(px(style::scaled(&self.shared_settings, 72.0)))
                    .flex_shrink_0()
                    .text_size(px(fonts.secondary))
                    .text_color(self.theme.muted)
                    .child(label),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .text_size(px(fonts.body))
                    .text_color(self.theme.panel_fg)
                    .child(value),
            )
    }

    fn automations_detail(&self, id: &str) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let detail = match &self.automations_page.detail {
            Some((open, Some(Ok(detail)))) if open == id => detail,
            Some((open, Some(Err(error)))) if open == id => {
                return div()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.alert)
                    .child(error.clone());
            }
            _ => {
                let mut column = div().flex().flex_col().gap_2();
                for (index, width) in [0.6, 0.8, 0.5].into_iter().enumerate() {
                    column = column.child(style::skeleton(
                        format!("pilus-automations-detail-skeleton-{index}"),
                        theme,
                        gpui::relative(width),
                        fonts.body,
                    ));
                }
                return column;
            }
        };
        let now = unix_now();
        let mut properties = div()
            .flex()
            .flex_col()
            .child(
                self.automations_property("State", if detail.enabled { "Armed" } else { "Paused" }),
            )
            .child(self.automations_property("Schedule", detail.schedule.clone()))
            .child(self.automations_property("Policy", detail.policy.clone()))
            .child(self.automations_property("Target", detail.target.clone()))
            .child(self.automations_property("Agent", detail.agent_id.clone()));
        if !detail.mode.is_empty() {
            properties = properties.child(self.automations_property("Run in", detail.mode.clone()));
        }
        if !detail.access.is_empty() {
            properties = properties.child(self.automations_property(
                "Access",
                super::status_label(&detail.access.replace('_', " ")),
            ));
        }
        if !detail.paths.is_empty() {
            let mut paths = div().flex().flex_row().flex_wrap().gap_1();
            for path in &detail.paths {
                paths = paths.child(style::chip(theme, path.clone(), fonts.label));
            }
            properties = properties.child(self.automations_property("Paths", paths));
        }
        let mono = SharedString::from(style::mono_family(&self.shared_settings));
        let block = |text: String| {
            div()
                .p_3()
                .radius_md()
                .bg(theme.panel_dim)
                .border_1()
                .border_color(theme.border)
                .font_family(mono.clone())
                .text_size(px(fonts.secondary))
                .text_color(theme.panel_fg)
                .child(text)
        };
        let section = |label: &'static str, body: Div| {
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(style::section_label(theme, label, fonts.label))
                .child(body)
        };
        let mut column = div()
            .flex()
            .flex_col()
            .gap_4()
            .child(style::title(theme, detail.name.clone(), fonts.title))
            .child(properties);
        if !detail.occurrences.is_empty() {
            let mut list = div().flex().flex_col().gap_1();
            for at in &detail.occurrences {
                list = list.child(
                    div()
                        .flex()
                        .flex_row()
                        .justify_between()
                        .text_size(px(fonts.secondary))
                        .child(div().text_color(theme.panel_fg).child(utc_label(*at)))
                        .child(div().text_color(theme.muted).child(until_label(now, *at))),
                );
            }
            column = column.child(section("NEXT RUNS", list));
        }
        let task = if detail.prompt.is_empty() {
            detail.title.clone()
        } else {
            format!("{}\n\n{}", detail.title, detail.prompt)
        };
        column = column.child(section("TASK", block(task)));
        if let Some(gate) = &detail.gate {
            column = column.child(section("GATE", block(gate.clone())));
        }
        let mut history = div().flex().flex_col().gap_1();
        if detail.runs.is_empty() {
            history = history.child(
                div()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child("No runs yet"),
            );
        }
        for run in detail.runs.iter().take(8) {
            let color = match run.status.as_str() {
                "succeeded" | "delivered" => theme.ok,
                "failed" | "review" => theme.alert,
                "running" | "starting" | "pending" => theme.accent,
                _ => theme.muted,
            };
            let mut line = div().flex().flex_col().child(
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .text_size(px(fonts.secondary))
                    .child(
                        div()
                            .text_color(color)
                            .child(super::status_label(&run.status)),
                    )
                    .child(
                        div()
                            .text_color(theme.muted)
                            .child(utc_label(run.scheduled_at)),
                    ),
            );
            if let Some(error) = &run.error {
                line = line.child(
                    div()
                        .text_size(px(fonts.label))
                        .text_color(theme.alert)
                        .child(error.clone()),
                );
            }
            history = history.child(line);
        }
        column.child(section("RUN HISTORY", history))
    }

    fn automations_actions(&self, id: &str, cx: &mut Context<Self>) -> Option<Div> {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let enabled = self.automations_page.enabled(id)?;
        let run_id = id.to_string();
        let toggle_id = id.to_string();
        Some(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .p_3()
                .border_t_1()
                .border_color(theme.border)
                .child(
                    style::small_button(
                        "pilus-automations-detail-run",
                        "Run now",
                        ButtonKind::Primary,
                        theme,
                        fonts.secondary,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.automations_op(run_id.clone(), true, false, cx);
                    })),
                )
                .child(
                    style::small_button(
                        "pilus-automations-detail-toggle",
                        if enabled { "Pause" } else { "Enable" },
                        ButtonKind::Secondary,
                        theme,
                        fonts.secondary,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.automations_op(toggle_id.clone(), false, !enabled, cx);
                    })),
                ),
        )
    }

    fn automations_message(&self, title: &'static str, detail: String) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        div()
            .flex()
            .flex_col()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_2()
            .child(style::title(theme, title, fonts.title))
            .child(
                div()
                    .max_w(px(420.0))
                    .text_center()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(detail),
            )
    }

    fn automations_skeleton(&self) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let mut column = div().flex().flex_col().gap_1().p_4();
        for (index, width) in [0.7, 0.55, 0.62].into_iter().enumerate() {
            column = column.child(
                div()
                    .flex()
                    .items_center()
                    .h(px(style::scaled(&self.shared_settings, ROW_HEIGHT)))
                    .px_2()
                    .child(style::skeleton(
                        format!("pilus-automations-row-skeleton-{index}"),
                        theme,
                        gpui::relative(width),
                        fonts.body,
                    )),
            );
        }
        column
    }
}

#[cfg(test)]
mod tests {
    use pilus_client::AutomationRow;

    use super::ordered;

    fn row(id: &str, state: &str, next: Option<u64>) -> AutomationRow {
        AutomationRow {
            id: id.to_string(),
            name: id.to_string(),
            state: state.to_string(),
            next_run_at: next,
            ..AutomationRow::default()
        }
    }

    #[test]
    fn rows_sort_by_attention_then_deadline() {
        let rows = [
            row("paused", "paused", None),
            row("later", "scheduled", Some(500)),
            row("sooner", "scheduled", Some(100)),
            row("live", "running", None),
            row("broken", "failed", None),
        ];
        let ids: Vec<&str> = ordered(&rows).iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["broken", "live", "sooner", "later", "paused"]);
    }

    #[test]
    fn equal_rows_keep_a_stable_name_order() {
        let rows = [row("b", "paused", None), row("a", "completed", None)];
        let ids: Vec<&str> = ordered(&rows).iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
    }
}
