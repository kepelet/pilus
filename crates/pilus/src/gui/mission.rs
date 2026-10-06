//! The native Mission Control page: the daemon's agent dashboard, drawn by
//! pilus instead of the daemon's `mission` tab.
//!
//! The data is the daemon's own model, read with `mission.snapshot` on the
//! active endpoint while the page is open: live and resumable agent rows with
//! usage, fleet totals, and automation health. Usage counters are scanned on
//! demand, so the page asks for a `mission.refresh` when it opens, when the
//! scope changes, and when the user presses Refresh, then keeps re-reading the
//! cheap snapshot. Every row action is a UHP call made off the GUI thread
//! (answer, approve, interrupt, fork, close, resume, and the automation
//! run/pause controls); the next snapshot redraws the result, so the page
//! never edits a row locally. The one exception is dismissing a resumable
//! session, which the daemon keeps no API for: it is hidden on this client.
//!
//! The first half holds the page's state and the pure formatting and
//! ordering rules, unit-tested without GPUI; the second half renders.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui::{
    AnyElement, ClickEvent, Context, Div, Entity, FontWeight, Hsla, KeyDownEvent, Rgba,
    SharedString, div, prelude::*, px,
};
use pilus_client::{
    AutomationDetail, AutomationHealth, AutomationRow, MissionRow, MissionScope, MissionSnapshot,
};

use super::style::{self, ButtonKind, FillExt};
use super::text_input::TextInput;
use super::theme::Theme;
use super::{EndpointId, PilusView, agent_icon, icon, status_color, terminal_inset_border};

/// How often an open page re-reads the snapshot.
const REFRESH: Duration = Duration::from_secs(2);
/// The faster cadence while a usage scan is running, so its counters land
/// promptly.
const SCANNING_REFRESH: Duration = Duration::from_millis(600);
/// Context fill past which an agent is close to compacting; the daemon's
/// `mission::COMPACT_AT`.
const COMPACT_AT: f32 = 0.85;
/// Lines of a blocked agent's output read to find what it is waiting on.
const HINT_LINES: u16 = 8;

const DETAIL_WIDTH: f32 = 340.0;
const ROW_HEIGHT: f32 = 34.0;

/// The headline state the header shows: what needs you first.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Signal {
    /// An agent is blocked on the user.
    Attention,
    /// Agents are working and none is blocked.
    Active,
    Standby,
}

impl Signal {
    fn of(rows: &[&MissionRow]) -> Self {
        let live = |state: &str| {
            rows.iter()
                .any(|row| !row.resumable() && row.state == state)
        };
        if live("blocked") {
            Self::Attention
        } else if live("working") {
            Self::Active
        } else {
            Self::Standby
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Attention => "Attention",
            Self::Active => "Active",
            Self::Standby => "Standby",
        }
    }

    fn color(self, theme: Theme) -> Rgba {
        match self {
            Self::Attention => theme.alert,
            Self::Active => theme.accent,
            Self::Standby => theme.muted,
        }
    }
}

/// What the side pane is showing.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Selection {
    /// An agent row, by its [`row_keys`] key.
    Agent(String),
    /// An automation definition, by id.
    Automation(String),
}

/// A message from a background request.
pub(super) enum MissionMsg {
    Snapshot {
        generation: u64,
        result: Result<(MissionSnapshot, HashMap<String, String>), String>,
    },
    Budget(Option<f64>),
    Detail {
        id: String,
        result: Result<AutomationDetail, String>,
    },
    /// An action landed (or failed); re-read so the rows catch up.
    Done(String),
}

/// The page's own state; the rows live on the last snapshot.
pub(super) struct MissionPanel {
    open: bool,
    /// The endpoint the rows were read from; a switch drops them.
    endpoint: Option<EndpointId>,
    snapshot: Option<MissionSnapshot>,
    budget: Option<f64>,
    /// Bumped on every endpoint change or rescan, so a snapshot of the old one
    /// that lands late is dropped instead of flashing stale rows.
    generation: u64,
    in_flight: bool,
    fetched_at: Option<Instant>,
    /// Ask for a usage scan with the next read.
    rescan: bool,
    error: Option<String>,
    selection: Option<Selection>,
    /// Whether the side pane shows the selection (the daemon's `o`).
    detail_open: bool,
    answering: bool,
    confirm_close: Option<String>,
    /// What each blocked live pane is waiting on, by pane.
    hints: HashMap<String, String>,
    /// Resumable sessions hidden on this client.
    dismissed: HashSet<String>,
    automation: Option<(String, Option<Result<AutomationDetail, String>>)>,
    status: String,
    tx: Sender<MissionMsg>,
    rx: Receiver<MissionMsg>,
}

impl Default for MissionPanel {
    fn default() -> Self {
        let (tx, rx) = channel();
        Self {
            open: false,
            endpoint: None,
            snapshot: None,
            budget: None,
            generation: 0,
            in_flight: false,
            fetched_at: None,
            rescan: false,
            error: None,
            selection: None,
            detail_open: true,
            answering: false,
            confirm_close: None,
            hints: HashMap::new(),
            dismissed: HashSet::new(),
            automation: None,
            status: String::new(),
            tx,
            rx,
        }
    }
}

impl MissionPanel {
    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    /// Whether the answer field holds the keyboard.
    pub(super) fn answering(&self) -> bool {
        self.open && self.answering
    }

    /// Close the page, keeping the dismissed sessions.
    pub(super) fn close(&mut self) {
        self.open = false;
        self.answering = false;
        self.confirm_close = None;
        self.automation = None;
        self.selection = None;
        self.status.clear();
    }

    /// Follow a new endpoint, dropping rows that belonged to the old one.
    fn follow(&mut self, endpoint: &EndpointId) {
        if self.endpoint.as_ref() == Some(endpoint) {
            return;
        }
        self.endpoint = Some(endpoint.clone());
        self.generation += 1;
        self.snapshot = None;
        self.hints.clear();
        self.dismissed.clear();
        self.error = None;
        self.in_flight = false;
        self.fetched_at = None;
        self.rescan = true;
        self.selection = None;
        self.automation = None;
        self.answering = false;
        self.confirm_close = None;
    }

    /// The rows the page shows: the snapshot minus dismissed sessions, each
    /// with its stable key.
    fn visible_rows(&self) -> Vec<(String, &MissionRow)> {
        let Some(snapshot) = &self.snapshot else {
            return Vec::new();
        };
        row_keys(&snapshot.rows)
            .into_iter()
            .zip(&snapshot.rows)
            .filter(|(key, _)| !self.dismissed.contains(key))
            .collect()
    }

    fn selected_key(&self) -> Option<&str> {
        match &self.selection {
            Some(Selection::Agent(key)) => Some(key),
            _ => None,
        }
    }
}

/// A stable key per row: the pane for a live agent, else the agent, its
/// workspace, and its position among that workspace's sessions of the same
/// agent. Positions survive a refresh because the daemon lists sessions in
/// its own discovery order.
fn row_keys(rows: &[MissionRow]) -> Vec<String> {
    let mut seen: HashMap<(&str, &str), usize> = HashMap::new();
    rows.iter()
        .map(|row| match &row.pane {
            Some(pane) => format!("pane:{pane}"),
            None => {
                let ordinal = seen
                    .entry((row.agent.as_str(), row.workspace_id.as_str()))
                    .or_insert(0);
                let key = format!("resume:{}:{}:{ordinal}", row.agent, row.workspace_id);
                *ordinal += 1;
                key
            }
        })
        .collect()
}

/// The ordinal [`row_keys`] gave a resumable row.
fn resume_ordinal(key: &str) -> usize {
    key.rsplit(':')
        .next()
        .and_then(|ordinal| ordinal.parse().ok())
        .unwrap_or(0)
}

/// The key `delta` rows from `current`, clamped. With nothing selected,
/// moving down picks the first row and moving up the last.
fn step_key(keys: &[String], current: Option<&str>, delta: isize) -> Option<String> {
    if keys.is_empty() {
        return None;
    }
    let at = current.and_then(|key| keys.iter().position(|row| row == key));
    let next = match at {
        Some(at) => (at as isize + delta).clamp(0, keys.len() as isize - 1) as usize,
        None if delta < 0 => keys.len() - 1,
        None => 0,
    };
    Some(keys[next].clone())
}

/// A compact token count: `945`, `12.3k`, `1.2M`.
fn fmt_tokens(count: u64) -> String {
    if count >= 1_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else if count >= 1_000 {
        format!("{:.1}k", count as f64 / 1_000.0)
    } else {
        count.to_string()
    }
}

/// A compact USD amount: `$3.20`, `$1.2k`.
fn fmt_cost(cost: f64) -> String {
    if cost >= 1000.0 {
        format!("${:.1}k", cost / 1000.0)
    } else {
        format!("${cost:.2}")
    }
}

/// A short model tag (`opus`, `sonnet`, `gpt-5`, …), else the id cut short.
fn short_model(model: &str) -> String {
    let lower = model.to_ascii_lowercase();
    for known in ["opus", "sonnet", "haiku", "gpt-5", "gpt-4o", "o3", "o1"] {
        if lower.contains(known) {
            return known.to_string();
        }
    }
    model.chars().take(10).collect()
}

/// Spend summed per model, largest first: the cost-by-model chart.
fn cost_by_model(rows: &[&MissionRow]) -> Vec<(String, f64)> {
    let mut totals: Vec<(String, f64)> = Vec::new();
    for usage in rows.iter().filter_map(|row| row.usage.as_ref()) {
        let Some(cost) = usage.cost_usd else {
            continue;
        };
        let model = short_model(&usage.model);
        match totals.iter_mut().find(|(name, _)| *name == model) {
            Some(entry) => entry.1 += cost,
            None => totals.push((model, cost)),
        }
    }
    totals.sort_by(|a, b| b.1.total_cmp(&a.1));
    totals
}

/// The line a blocked agent is waiting on: the last line of its output that
/// carries words, with the box-drawing frame some TUIs draw trimmed off.
fn blocked_line(output: &str) -> Option<String> {
    let frame = |c: char| {
        c.is_whitespace() || ('\u{2500}'..='\u{259F}').contains(&c) || matches!(c, '|' | '>')
    };
    output
        .lines()
        .rev()
        .map(|line| line.trim_matches(frame))
        .find(|line| line.chars().any(char::is_alphanumeric))
        .map(|line| line.chars().take(160).collect())
}

/// A deadline relative to `now`: `in 4m`, `in 3h`, `in 2d`, or `due`.
fn until_label(now: u64, at: u64) -> String {
    let secs = at.saturating_sub(now);
    match secs {
        0 => "due".to_string(),
        1..=59 => "in <1m".to_string(),
        60..=3599 => format!("in {}m", secs / 60),
        3600..=86_399 => format!("in {}h", secs / 3600),
        _ => format!("in {}d", secs / 86_400),
    }
}

/// A Unix time as `Oct 06 14:00 UTC`.
fn utc_label(at: u64) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let days = (at / 86_400) as i64;
    let (_, month, day) = civil_from_days(days);
    let secs = at % 86_400;
    format!(
        "{} {day:02} {:02}:{:02} UTC",
        MONTHS[(month - 1) as usize],
        secs / 3600,
        secs % 3600 / 60
    )
}

/// The civil date for days since the Unix epoch (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// The tint for an automation state.
fn automation_color(state: &str, theme: Theme) -> Rgba {
    match state {
        "running" => theme.accent,
        "restoring" => theme.branch,
        "review" | "failed" | "unavailable" | "needs_rebind" => theme.alert,
        "scheduled" | "completed" => theme.ok,
        _ => theme.muted,
    }
}

/// An automation state as a label.
fn automation_label(state: &str) -> String {
    match state {
        "needs_rebind" => "Needs rebind".to_string(),
        other => super::status_label(other),
    }
}

/// The automation strip's headline: what the scheduler needs from you.
fn automation_signal(health: AutomationHealth, theme: Theme) -> (&'static str, Rgba) {
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

/// A row action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RowOp {
    /// Approve a blocked agent's prompt (`y`, Enter).
    Approve,
    /// Interrupt the agent (Escape).
    Stop,
    Fork,
    Close,
}

impl RowOp {
    fn label(self) -> &'static str {
        match self {
            Self::Approve => "Approve",
            Self::Stop => "Stop",
            Self::Fork => "Fork",
            Self::Close => "Close",
        }
    }
}

impl PilusView {
    /// Open the page on the active endpoint, across all its workspaces.
    pub(super) fn open_mission(&mut self, cx: &mut Context<Self>) {
        self.leave_settings_page();
        self.picker.close();
        let active = self.active.clone();
        let panel = &mut self.mission;
        panel.follow(&active);
        panel.open = true;
        panel.in_flight = false;
        panel.fetched_at = None;
        panel.rescan = true;
        self.mission_fetch_budget();
        cx.notify();
    }

    /// Shot-harness entry point: open on a canned snapshot, no daemon.
    #[cfg(feature = "visual-test")]
    pub(super) fn mission_show(
        &mut self,
        snapshot: MissionSnapshot,
        hints: HashMap<String, String>,
        cx: &mut Context<Self>,
    ) {
        self.leave_settings_page();
        let keys = row_keys(&snapshot.rows);
        let active = self.active.clone();
        let panel = &mut self.mission;
        panel.endpoint = Some(active);
        panel.open = true;
        panel.selection = keys.first().cloned().map(Selection::Agent);
        panel.snapshot = Some(snapshot);
        panel.hints = hints;
        panel.budget = Some(50.0);
        // Hold the canned rows: no daemon will answer a read.
        panel.in_flight = true;
        panel.rescan = false;
        cx.notify();
    }

    /// Shot-harness entry point: open the reply field on the selected row.
    #[cfg(feature = "visual-test")]
    pub(super) fn mission_show_answer(&mut self, cx: &mut Context<Self>) {
        self.mission_open_answer(cx);
    }

    /// Shot-harness entry point: show an automation's detail, canned.
    #[cfg(feature = "visual-test")]
    pub(super) fn mission_show_automation(
        &mut self,
        detail: AutomationDetail,
        cx: &mut Context<Self>,
    ) {
        let panel = &mut self.mission;
        panel.answering = false;
        panel.selection = Some(Selection::Automation(detail.id.clone()));
        panel.automation = Some((detail.id.clone(), Some(Ok(detail))));
        cx.notify();
    }

    fn mission_fetch_budget(&self) {
        let tx = self.mission.tx.clone();
        let client = self.control_client_for_endpoint(&self.active);
        std::thread::spawn(move || {
            let budget = client.and_then(|mut client| client.mission_budget().ok().flatten());
            tx.send(MissionMsg::Budget(budget)).ok();
        });
    }

    /// Drain background answers and schedule the next read. Returns whether
    /// anything changed on screen.
    pub(super) fn mission_tick(&mut self) -> bool {
        let mut changed = false;
        while let Ok(message) = self.mission.rx.try_recv() {
            changed = true;
            self.mission_receive(message);
        }
        if !self.mission.open {
            return changed;
        }
        // A machine switch lands after the click: the rows follow `active`.
        if self.mission.endpoint.as_ref() != Some(&self.active) {
            let active = self.active.clone();
            self.mission.follow(&active);
            self.mission_fetch_budget();
            changed = true;
        }
        let panel = &self.mission;
        let cadence = if panel.snapshot.as_ref().is_some_and(|s| s.refreshing) {
            SCANNING_REFRESH
        } else {
            REFRESH
        };
        let due = panel.fetched_at.is_none_or(|at| at.elapsed() >= cadence);
        if due && !panel.in_flight {
            self.mission_fetch();
        }
        changed
    }

    fn mission_receive(&mut self, message: MissionMsg) {
        let panel = &mut self.mission;
        match message {
            MissionMsg::Snapshot { generation, result } => {
                if generation != panel.generation {
                    return;
                }
                panel.in_flight = false;
                panel.fetched_at = Some(Instant::now());
                match result {
                    Ok((snapshot, hints)) => {
                        panel.error = None;
                        panel.hints = hints;
                        panel.snapshot = Some(snapshot);
                    }
                    Err(error) => panel.error = Some(error),
                }
                // A selected row that left (closed, resumed into a pane)
                // releases the side pane rather than pointing at nothing.
                if let Some(key) = panel.selected_key().map(str::to_string)
                    && !panel.visible_rows().iter().any(|(row, _)| *row == key)
                {
                    panel.selection = None;
                    panel.answering = false;
                }
            }
            MissionMsg::Budget(budget) => panel.budget = budget,
            MissionMsg::Detail { id, result } => {
                if let Some((open, slot)) = &mut panel.automation
                    && *open == id
                {
                    *slot = Some(result);
                }
            }
            MissionMsg::Done(text) => {
                panel.status = text;
                panel.fetched_at = None;
                // A run or pause changes the definition the pane shows.
                if let Some((id, _)) = &panel.automation {
                    let id = id.clone();
                    self.mission_load_automation(id);
                }
            }
        }
    }

    /// Read the snapshot (after a usage scan when one is due) and what each
    /// blocked agent is waiting on.
    fn mission_fetch(&mut self) {
        let Some(client) = self.control_client_for_endpoint(&self.active) else {
            self.mission.error = Some("No control connection to this machine".to_string());
            self.mission.fetched_at = Some(Instant::now());
            return;
        };
        let panel = &mut self.mission;
        panel.in_flight = true;
        let rescan = std::mem::take(&mut panel.rescan);
        let generation = panel.generation;
        let tx = panel.tx.clone();
        std::thread::spawn(move || {
            let mut client = client;
            let result = (|| {
                if rescan {
                    client.mission_refresh(MissionScope::All, None)?;
                }
                let snapshot = client.mission_snapshot(MissionScope::All, None)?;
                // The snapshot leaves terminal content out by design; the
                // waiting line comes from the blocked agent's own output.
                let mut hints = HashMap::new();
                for row in snapshot.rows.iter().filter(|row| row.state == "blocked") {
                    if let Some(pane) = &row.pane
                        && let Ok(text) = client.read_agent(pane, HINT_LINES)
                        && let Some(line) = blocked_line(&text)
                    {
                        hints.insert(pane.clone(), line);
                    }
                }
                Ok((snapshot, hints))
            })()
            .map_err(|error: pilus_client::Error| error.to_string());
            tx.send(MissionMsg::Snapshot { generation, result }).ok();
        });
    }

    fn mission_rescan(&mut self, cx: &mut Context<Self>) {
        self.mission.rescan = true;
        self.mission.fetched_at = None;
        self.mission.in_flight = false;
        self.mission.generation += 1;
        if let Some(snapshot) = &mut self.mission.snapshot {
            snapshot.refreshing = true;
        }
        cx.notify();
    }

    fn mission_select(&mut self, selection: Option<Selection>, cx: &mut Context<Self>) {
        let panel = &mut self.mission;
        if panel.selection != selection {
            panel.answering = false;
            panel.confirm_close = None;
        }
        panel.automation = None;
        if let Some(Selection::Automation(id)) = &selection {
            panel.automation = Some((id.clone(), None));
            let id = id.clone();
            panel.selection = selection;
            panel.detail_open = true;
            self.mission_load_automation(id);
        } else {
            panel.selection = selection;
        }
        cx.notify();
    }

    fn mission_load_automation(&self, id: String) {
        let tx = self.mission.tx.clone();
        let client = self.control_client_for_endpoint(&self.active);
        std::thread::spawn(move || {
            let result = match client {
                Some(mut client) => client
                    .automation_detail(&id)
                    .map_err(|error| error.to_string()),
                None => Err("Could not reach the daemon".to_string()),
            };
            tx.send(MissionMsg::Detail { id, result }).ok();
        });
    }

    /// The selected agent row and its key.
    fn mission_selected_row(&self) -> Option<(String, MissionRow)> {
        let key = self.mission.selected_key()?;
        self.mission
            .visible_rows()
            .into_iter()
            .find(|(row, _)| row == key)
            .map(|(key, row)| (key, row.clone()))
    }

    /// Run a daemon call off the GUI thread and report it in the footer.
    fn mission_spawn(
        &mut self,
        pending: String,
        call: impl FnOnce(&mut pilus_client::ControlClient) -> pilus_client::Result<String>
        + Send
        + 'static,
        cx: &mut Context<Self>,
    ) {
        let tx = self.mission.tx.clone();
        let client = self.control_client_for_endpoint(&self.active);
        self.mission.status = pending;
        std::thread::spawn(move || {
            let text = match client {
                Some(mut client) => call(&mut client).unwrap_or_else(|error| error.to_string()),
                None => "Could not reach the daemon".to_string(),
            };
            tx.send(MissionMsg::Done(text)).ok();
        });
        cx.notify();
    }

    /// Jump to a live agent's pane, or reopen a resumable session.
    fn mission_go(&mut self, cx: &mut Context<Self>) {
        let Some((key, row)) = self.mission_selected_row() else {
            return;
        };
        match &row.pane {
            Some(pane) => {
                let pane = pane.clone();
                let workspace = (!row.workspace_id.is_empty()).then(|| row.workspace_id.clone());
                self.mission.close();
                self.activate_endpoint(self.active.clone(), workspace, Some(pane), cx);
                cx.notify();
            }
            None => self.mission_resume(&key, &row, cx),
        }
    }

    /// Resume a session row. The snapshot omits native session ids, so the
    /// row is matched back to `agent.sessions` by agent, workspace folder,
    /// and its position among that folder's sessions.
    fn mission_resume(&mut self, key: &str, row: &MissionRow, cx: &mut Context<Self>) {
        let cwd = self
            .surface
            .workspaces()
            .iter()
            .find(|workspace| workspace.id == row.workspace_id)
            .map(|workspace| workspace.cwd.clone());
        let Some(cwd) = cwd else {
            self.mission.status = "That session's workspace is not open".to_string();
            cx.notify();
            return;
        };
        let live: HashSet<String> = self
            .agents
            .iter()
            .filter_map(|agent| agent.session.clone())
            .collect();
        let agent = row.agent.clone();
        let ordinal = resume_ordinal(key);
        self.mission_spawn(
            format!("Resuming {agent}…"),
            move |client| {
                let trim = |path: &str| path.trim_end_matches('/').to_string();
                let session = client
                    .agent_sessions()?
                    .into_iter()
                    .filter(|session| {
                        session.agent == agent
                            && trim(&session.cwd) == trim(&cwd)
                            && !live.contains(&session.session_id)
                    })
                    .nth(ordinal);
                match session {
                    Some(session) => {
                        client.resume_agent(&session.session_id)?;
                        Ok(format!("Resumed {agent}"))
                    }
                    None => Ok(format!("Could not find that {agent} session")),
                }
            },
            cx,
        );
    }

    fn mission_row_op(&mut self, op: RowOp, cx: &mut Context<Self>) {
        let Some((key, row)) = self.mission_selected_row() else {
            return;
        };
        let Some(pane) = row.pane.clone() else {
            // A resumable session has no pane; Close hides it here.
            if op == RowOp::Close {
                self.mission.dismissed.insert(key);
                self.mission.selection = None;
                self.mission.status = format!("Dismissed {} session", row.agent);
                cx.notify();
            }
            return;
        };
        if op == RowOp::Close && self.mission.confirm_close.as_deref() != Some(key.as_str()) {
            self.mission.confirm_close = Some(key);
            cx.notify();
            return;
        }
        self.mission.confirm_close = None;
        let agent = row.agent.clone();
        self.mission_spawn(
            format!("{} {agent}…", op.label()),
            move |client| {
                match op {
                    RowOp::Approve => client.agent_keys(&pane, &["y", "enter"])?,
                    RowOp::Stop => client.interrupt_agent(&pane)?,
                    RowOp::Fork => client.fork_agent(&pane)?,
                    RowOp::Close => client.close_pane(&pane)?,
                }
                Ok(match op {
                    RowOp::Approve => format!("Approved {agent}"),
                    RowOp::Stop => format!("Interrupted {agent}"),
                    RowOp::Fork => format!("Forked {agent}"),
                    RowOp::Close => format!("Closed {agent}"),
                })
            },
            cx,
        );
    }

    fn mission_open_answer(&mut self, cx: &mut Context<Self>) {
        if self
            .mission_selected_row()
            .is_some_and(|(_, row)| row.pane.is_some())
        {
            self.mission_answer.update(cx, |input, cx| input.clear(cx));
            self.mission.answering = true;
            self.mission.detail_open = true;
            cx.notify();
        }
    }

    fn mission_send_answer(&mut self, cx: &mut Context<Self>) {
        let text = self.mission_answer.read(cx).value().trim().to_string();
        let Some((_, row)) = self.mission_selected_row() else {
            return;
        };
        let Some(pane) = row.pane.clone() else {
            return;
        };
        self.mission.answering = false;
        if text.is_empty() {
            cx.notify();
            return;
        }
        let agent = row.agent.clone();
        self.mission_spawn(
            format!("Answering {agent}…"),
            move |client| {
                client.prompt_agent(&pane, &text)?;
                Ok(format!("Answered {agent}"))
            },
            cx,
        );
    }

    fn mission_automation_op(
        &mut self,
        id: String,
        run: bool,
        enable: bool,
        cx: &mut Context<Self>,
    ) {
        self.mission_spawn(
            if run {
                "Requesting a run…".to_string()
            } else if enable {
                "Enabling…".to_string()
            } else {
                "Pausing…".to_string()
            },
            move |client| {
                if run {
                    client.run_automation(&id)?;
                    Ok("Run requested".to_string())
                } else {
                    client.set_automation_enabled(&id, enable)?;
                    Ok(if enable { "Enabled" } else { "Paused" }.to_string())
                }
            },
            cx,
        );
    }

    /// The page's keys; the answer field takes typing while it is open.
    pub(super) fn mission_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let mods = event.keystroke.modifiers;
        if self.mission.answering {
            match key {
                "escape" => {
                    self.mission.answering = false;
                    cx.notify();
                }
                "enter" if !mods.shift => self.mission_send_answer(cx),
                _ => {}
            }
            return;
        }
        if mods.platform || mods.control {
            return;
        }
        let keys: Vec<String> = self
            .mission
            .visible_rows()
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        let current = self.mission.selected_key().map(str::to_string);
        let step = |delta| step_key(&keys, current.as_deref(), delta).map(Selection::Agent);
        match key {
            "down" | "j" => self.mission_select(step(1), cx),
            "up" | "k" => self.mission_select(step(-1), cx),
            "r" => self.mission_rescan(cx),
            "enter" => self.mission_go(cx),
            "a" => self.mission_open_answer(cx),
            "y" => self.mission_row_op(RowOp::Approve, cx),
            "i" => self.mission_row_op(RowOp::Stop, cx),
            "f" => self.mission_row_op(RowOp::Fork, cx),
            "x" => self.mission_row_op(RowOp::Close, cx),
            "o" => {
                self.mission.detail_open = !self.mission.detail_open;
                cx.notify();
            }
            "q" => {
                self.mission.close();
                cx.notify();
            }
            "escape" => {
                let panel = &mut self.mission;
                if panel.confirm_close.take().is_none() && panel.selection.take().is_none() {
                    panel.close();
                }
                panel.automation = None;
                cx.notify();
            }
            _ => {}
        }
    }

    /// The page, drawn in place of the terminal card.
    pub(super) fn mission_page(&mut self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let rows: Vec<(String, MissionRow)> = self
            .mission
            .visible_rows()
            .into_iter()
            .map(|(key, row)| (key, row.clone()))
            .collect();
        let refs: Vec<&MissionRow> = rows.iter().map(|(_, row)| row).collect();
        let loaded = self.mission.snapshot.is_some();

        let mut main = div()
            .id("pilus-mission-scroll")
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.0))
            .gap_4()
            .p_4()
            .overflow_y_scroll();
        if loaded {
            main = main
                .child(self.mission_metrics(&refs))
                .child(self.mission_automations(cx))
                .child(self.mission_roster(&rows, cx));
        } else if let Some(error) = self.mission.error.clone() {
            main = main.child(self.mission_message("Mission Control is unavailable", error));
        } else {
            main = main.child(self.mission_skeleton());
        }

        let mut body = div().flex().flex_row().flex_1().min_h(px(0.0)).child(main);
        if self.mission.detail_open && loaded {
            body = body.child(self.mission_side(&refs, cx));
        }

        let hints = "↑↓ navigate · r refresh · ⏎ go · a answer · y approve · \
                     i stop · f fork · x close · o details";
        let footer_text = if self.mission.status.is_empty() {
            hints.to_string()
        } else {
            self.mission.status.clone()
        };
        let footer = div()
            .px_4()
            .h(px(style::scaled(&self.shared_settings, 28.0)))
            .flex()
            .items_center()
            .flex_shrink_0()
            .border_t_1()
            .border_color(terminal_inset_border())
            .text_size(px(fonts.secondary))
            .text_color(theme.muted)
            .truncate()
            .child(footer_text);

        div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .min_h(px(0.0))
            .child(self.mission_header(&refs, cx))
            .child(body)
            .child(footer)
    }

    fn mission_header(&self, rows: &[&MissionRow], cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let signal = Signal::of(rows);
        let machines = self.mission_machines(cx);
        let refreshing = self
            .mission
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.refreshing);
        let refresh = style::small_button(
            "pilus-mission-refresh",
            div()
                .flex()
                .items_center()
                .gap_2()
                .children(refreshing.then(|| {
                    style::spinner(
                        "pilus-mission-refresh-spinner",
                        fonts.secondary,
                        theme.muted,
                    )
                }))
                .child(if refreshing { "Refreshing" } else { "Refresh" }),
            ButtonKind::Secondary,
            theme,
            fonts.secondary,
        )
        .on_click(cx.listener(|this, _, _, cx| this.mission_rescan(cx)));
        let badge = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .text_size(px(fonts.secondary))
            .text_color(signal.color(theme))
            .child(
                div()
                    .w(px(8.0))
                    .h(px(8.0))
                    .rounded_full()
                    .bg(signal.color(theme)),
            )
            .child(signal.label());
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .h(px(style::scaled(&self.shared_settings, 48.0)))
            .flex_shrink_0()
            .px_4()
            .border_b_1()
            .border_color(terminal_inset_border())
            .child(style::title(theme, "Mission Control", fonts.title))
            .child(badge)
            .child(div().flex_1())
            .children(machines)
            .child(refresh)
    }

    /// The machine picker: Local and every enabled saved machine, one click
    /// to switch. Absent when Local is the only machine.
    fn mission_machines(&self, cx: &mut Context<Self>) -> Option<Div> {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let mut endpoints = vec![EndpointId::Local];
        endpoints.extend(
            self.machines
                .iter()
                .filter(|machine| machine.enabled && machine.id != "local")
                .map(|machine| EndpointId::Machine(machine.id.clone())),
        );
        if endpoints.len() < 2 {
            return None;
        }
        let mut picker = div().flex().flex_row().items_center().gap_1();
        for endpoint in endpoints {
            let kind = if self.endpoint_selected(&endpoint) {
                ButtonKind::Secondary
            } else {
                ButtonKind::Ghost
            };
            let target = endpoint.clone();
            picker = picker.child(
                style::small_button(
                    format!("pilus-mission-machine-{}", endpoint.key()),
                    self.endpoint_label(&endpoint),
                    kind,
                    theme,
                    fonts.secondary,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.activate_endpoint(target.clone(), None, None, cx);
                })),
            );
        }
        Some(picker)
    }

    fn mission_metrics(&self, rows: &[&MissionRow]) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let live = |state: &str| {
            rows.iter()
                .filter(|row| !row.resumable() && row.state == state)
                .count()
        };
        let (working, blocked) = (live("working"), live("blocked"));
        let resumable = rows.iter().filter(|row| row.resumable()).count();
        let cost: f64 = rows
            .iter()
            .filter_map(|row| row.usage.as_ref()?.cost_usd)
            .sum();
        let over_budget = self.mission.budget.is_some_and(|budget| cost > budget);
        let burn = self
            .mission
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.burn_usd_per_hour)
            .filter(|rate| *rate >= 0.005)
            .map(|rate| format!("{}/hr", fmt_cost(rate)))
            .unwrap_or_else(|| "rate unavailable".to_string());
        let spend_detail = match self.mission.budget {
            Some(budget) => format!("{burn} · budget {}", fmt_cost(budget)),
            None => burn,
        };
        style::stat_strip(theme)
            .child(style::stat_tile(
                theme,
                fonts,
                "AGENTS",
                format!("{:02}", rows.len()),
                theme.accent,
                format!("{} live · {resumable} resumable", rows.len() - resumable),
            ))
            .child(style::stat_tile(
                theme,
                fonts,
                "WORKING",
                format!("{working:02}"),
                if working > 0 {
                    theme.accent
                } else {
                    theme.muted
                },
                "currently working",
            ))
            .child(style::stat_tile(
                theme,
                fonts,
                "BLOCKED",
                format!("{blocked:02}"),
                if blocked > 0 { theme.alert } else { theme.ok },
                if blocked > 0 {
                    "awaiting response"
                } else {
                    "no blocked agents"
                },
            ))
            .child(style::stat_tile(
                theme,
                fonts,
                "SPEND",
                fmt_cost(cost),
                if over_budget { theme.alert } else { theme.ok },
                spend_detail,
            ))
    }

    /// The automation strip: scheduler health and one row per definition.
    fn mission_automations(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let Some(snapshot) = &self.mission.snapshot else {
            return div();
        };
        let health = snapshot.automation;
        let (signal, color) = automation_signal(health, theme);
        let now = unix_now();
        let next = health
            .next_run_at
            .map(|at| format!("next {} · {}", until_label(now, at), utc_label(at)))
            .unwrap_or_else(|| "no upcoming run".to_string());
        let summary = format!(
            "{} armed · {} live · {} review · {} failed · {next}",
            health.scheduled, health.running, health.review, health.failed
        );
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .child(style::section_label(theme, "AUTOMATIONS", fonts.label))
            .child(
                div()
                    .text_size(px(fonts.label))
                    .font_weight(FontWeight::BOLD)
                    .text_color(color)
                    .child(signal.to_uppercase()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(summary),
            );
        let mut column = div().flex().flex_col().gap_2().child(header);
        if snapshot.automations.is_empty() {
            return column;
        }
        let mut list = div().flex().flex_col();
        for row in &snapshot.automations {
            list = list.child(self.mission_automation_row(row, now, cx));
        }
        column = column.child(
            div()
                .p_1()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .child(list),
        );
        column
    }

    fn mission_automation_row(
        &self,
        row: &AutomationRow,
        now: u64,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let selected = self.mission.selection == Some(Selection::Automation(row.id.clone()));
        let (rest, hover) = row_fills(selected, theme);
        let color = automation_color(&row.state, theme);
        let id = row.id.clone();
        let next = row
            .next_run_at
            .map(|at| until_label(now, at))
            .unwrap_or_else(|| "—".to_string());
        div()
            .id(format!("pilus-mission-automation-{}", row.id))
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .h(px(style::scaled(&self.shared_settings, 30.0)))
            .px_2()
            .rounded_md()
            .cursor_pointer()
            .state_fill(rest, hover, style::RADIUS_MD)
            .child(div().w(px(8.0)).h(px(8.0)).rounded_full().bg(color))
            .child(
                div()
                    .w(px(style::scaled(&self.shared_settings, 96.0)))
                    .flex_shrink_0()
                    .text_size(px(fonts.secondary))
                    .text_color(color)
                    .child(automation_label(&row.state)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(fonts.body))
                    .text_color(theme.panel_fg)
                    .font_weight(if selected {
                        FontWeight::BOLD
                    } else {
                        FontWeight::NORMAL
                    })
                    .child(row.name.clone()),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(row.agent_id.clone()),
            )
            .child(
                div()
                    .w(px(style::scaled(&self.shared_settings, 64.0)))
                    .flex()
                    .justify_end()
                    .flex_shrink_0()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(next),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                let pick = Selection::Automation(id.clone());
                let next = (this.mission.selection.as_ref() != Some(&pick)).then_some(pick);
                this.mission_select(next, cx);
            }))
    }

    /// The agent table: one row per live agent, then resumable sessions.
    fn mission_roster(&self, rows: &[(String, MissionRow)], cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let label = |text: &'static str, width: Option<f32>, right: bool, last: bool| {
            let cell = self
                .mission_cell(width, last)
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
            .child(label("#", Some(36.0), false, false))
            .child(label("AGENT", None, false, false))
            .child(label("STATE", Some(76.0), false, false))
            .child(label("LOCATION", Some(120.0), false, false))
            .child(label("MODEL", Some(68.0), false, false))
            .child(label("TOKENS", Some(68.0), true, false))
            .child(label("CONTEXT", Some(112.0), false, false))
            .child(label("COST", Some(76.0), true, true));
        let mut column = div().flex().flex_col().gap_2().child(style::section_label(
            theme,
            "AGENT SESSIONS",
            fonts.label,
        ));
        if rows.is_empty() {
            return column.child(self.mission_empty());
        }
        let mut list = div()
            .flex()
            .flex_col()
            .border_1()
            .border_color(theme.border)
            .child(header);
        for (index, (key, row)) in rows.iter().enumerate() {
            list = list.child(self.mission_row(index + 1, key, row, index + 1 < rows.len(), cx));
        }
        column = column.child(list);
        column
    }

    /// One table cell: a fixed width, or the flexible column when `width` is
    /// `None`. Every cell but the last closes with a column rule, so the
    /// header and the rows share one grid.
    fn mission_cell(&self, width: Option<f32>, last: bool) -> Div {
        let cell = div().flex().items_center().h_full().px_2().min_w(px(0.0));
        let cell = match width {
            Some(width) => cell.w(px(style::scaled(&self.shared_settings, width))),
            None => cell.flex_1(),
        };
        let cell = cell.flex_shrink_0();
        if last {
            cell
        } else {
            cell.border_r_1().border_color(self.theme.border)
        }
    }

    fn mission_row(
        &self,
        number: usize,
        key: &str,
        row: &MissionRow,
        rule_below: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let scaled = |width: f32| px(style::scaled(&self.shared_settings, width));
        let selected = self.mission.selected_key() == Some(key);
        let (rest, hover) = row_fills(selected, theme);
        let state_color = if row.resumable() {
            theme.muted
        } else {
            status_color(&row.state, theme)
        };
        let state = if row.resumable() {
            "Resume".to_string()
        } else {
            super::status_label(&row.state)
        };
        let usage = row.usage.as_ref();
        let mono = SharedString::from(style::mono_family(&self.shared_settings));
        let dash = || "—".to_string();
        let model = usage
            .map(|usage| short_model(&usage.model))
            .filter(|model| !model.is_empty())
            .unwrap_or_else(dash);
        let tokens = usage
            .map(|usage| fmt_tokens(usage.total_tokens()))
            .unwrap_or_else(dash);
        let cost = usage
            .and_then(|usage| usage.cost_usd)
            .map(fmt_cost)
            .unwrap_or_else(dash);
        let context = usage.and_then(|usage| usage.context);
        let warns = context.is_some_and(|context| context >= COMPACT_AT);
        let glyph = if row.state == "working" && !row.resumable() {
            style::spinner(format!("pilus-mission-spin-{key}"), 10.0, state_color)
                .into_any_element()
        } else if row.resumable() {
            div()
                .w(px(8.0))
                .h(px(8.0))
                .rounded_full()
                .border_1()
                .border_color(state_color)
                .into_any_element()
        } else {
            div()
                .w(px(8.0))
                .h(px(8.0))
                .rounded_full()
                .bg(state_color)
                .into_any_element()
        };
        let key_owned = key.to_string();
        let square = gpui::AbsoluteLength::Pixels(px(0.0));
        let row_el = div()
            .id(format!("pilus-mission-row-{key}"))
            .flex()
            .flex_row()
            .items_stretch()
            .h(px(style::scaled(&self.shared_settings, ROW_HEIGHT)))
            .cursor_pointer()
            .state_fill(rest, hover, square)
            .child(
                self.mission_cell(Some(36.0), false)
                    .font_family(mono.clone())
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(format!("{number:02}")),
            )
            .child(
                self.mission_cell(None, false)
                    .gap_2()
                    .child(
                        div()
                            .w(px(10.0))
                            .flex()
                            .justify_center()
                            .flex_shrink_0()
                            .child(glyph),
                    )
                    .child(icon(agent_icon(&row.agent), 12.0, theme.panel_fg))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(fonts.body))
                            .font_weight(if selected {
                                FontWeight::BOLD
                            } else {
                                FontWeight::MEDIUM
                            })
                            .text_color(theme.panel_fg)
                            .child(row.agent.clone()),
                    ),
            )
            .child(
                self.mission_cell(Some(76.0), false)
                    .text_size(px(fonts.secondary))
                    .text_color(state_color)
                    .child(state),
            )
            .child(
                self.mission_cell(Some(120.0), false)
                    .truncate()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(row.location.clone()),
            )
            .child(
                self.mission_cell(Some(68.0), false)
                    .truncate()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.branch)
                    .child(model),
            )
            .child(
                self.mission_cell(Some(68.0), false)
                    .justify_end()
                    .font_family(mono.clone())
                    .text_size(px(fonts.secondary))
                    .text_color(theme.panel_fg)
                    .child(tokens),
            )
            .child(
                self.mission_cell(Some(112.0), false)
                    .gap_2()
                    .child(div().flex_1().child(style::meter(
                        format!("pilus-mission-ctx-{key}"),
                        context,
                        if warns { theme.alert } else { theme.accent },
                        theme,
                        style::scaled(&self.shared_settings, 5.0),
                    )))
                    .child(
                        div()
                            .w(scaled(30.0))
                            .flex()
                            .justify_end()
                            .font_family(mono.clone())
                            .text_size(px(fonts.label))
                            .text_color(if warns { theme.alert } else { theme.muted })
                            .child(
                                context
                                    .map(|context| format!("{}%", (context * 100.0).round()))
                                    .unwrap_or_else(dash),
                            ),
                    ),
            )
            .child(
                self.mission_cell(Some(76.0), true)
                    .justify_end()
                    .font_family(mono)
                    .text_size(px(fonts.secondary))
                    .text_color(theme.ok)
                    .child(cost),
            );
        let row_el = if rule_below {
            row_el.border_b_1().border_color(theme.border)
        } else {
            row_el
        };
        row_el
            .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                let pick = Selection::Agent(key_owned.clone());
                if event.click_count() >= 2 {
                    this.mission.selection = Some(pick);
                    this.mission_go(cx);
                    return;
                }
                this.mission_select(Some(pick), cx);
            }))
    }

    /// The side pane: the selected agent or automation, then the fleet's
    /// status counts and spend by model.
    fn mission_side(&self, rows: &[&MissionRow], cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let (title, content, actions): (SharedString, AnyElement, Option<Div>) =
            match &self.mission.selection {
                Some(Selection::Automation(id)) => (
                    "Automation".into(),
                    self.mission_automation_detail(id).into_any_element(),
                    self.mission_automation_actions(id, cx),
                ),
                _ => match self.mission_selected_row() {
                    Some((_, row)) => (
                        "Selected agent".into(),
                        self.mission_agent_detail(&row, cx).into_any_element(),
                        Some(self.mission_agent_actions(&row, cx)),
                    ),
                    None => (
                        "Selected agent".into(),
                        div()
                            .text_size(px(fonts.secondary))
                            .text_color(theme.muted)
                            .child("Select an agent to see its usage and act on it.")
                            .into_any_element(),
                        None,
                    ),
                },
            };
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .h(px(style::scaled(&self.shared_settings, 40.0)))
            .flex_shrink_0()
            .px_4()
            .border_b_1()
            .border_color(terminal_inset_border())
            .child(style::section_label(
                theme,
                title.to_uppercase(),
                fonts.label,
            ))
            .child(
                style::small_button(
                    "pilus-mission-side-close",
                    "Hide",
                    ButtonKind::Ghost,
                    theme,
                    fonts.secondary,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.mission.detail_open = false;
                    cx.notify();
                })),
            );
        let sections = div()
            .flex()
            .flex_col()
            .gap_5()
            .p_4()
            .child(content)
            .child(self.mission_status_counts(rows))
            .child(self.mission_cost_chart(rows));
        style::enter(
            "pilus-mission-side",
            div()
                .flex()
                .flex_col()
                .w(px(style::scaled(&self.shared_settings, DETAIL_WIDTH)))
                .flex_shrink_0()
                .min_h(px(0.0))
                .border_l_1()
                .border_color(terminal_inset_border())
                .child(header)
                .child(
                    div()
                        .id("pilus-mission-side-scroll")
                        .flex_1()
                        .min_h(px(0.0))
                        .overflow_y_scroll()
                        .child(sections),
                )
                .children(actions),
        )
        .into_any_element()
    }

    /// One labelled line of a property table.
    fn mission_property(&self, label: &'static str, value: impl IntoElement) -> Div {
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

    fn mission_agent_detail(&self, row: &MissionRow, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let color = if row.resumable() {
            theme.muted
        } else {
            status_color(&row.state, theme)
        };
        let state = if row.resumable() {
            "Resumable".to_string()
        } else {
            super::status_label(&row.state)
        };
        let title = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(icon(agent_icon(&row.agent), fonts.title, theme.fg))
            .child(style::title(theme, row.agent.clone(), fonts.title))
            .child(
                div()
                    .text_size(px(fonts.secondary))
                    .text_color(color)
                    .child(state),
            );
        let mut properties = div().flex().flex_col();
        if !row.workspace_name.is_empty() {
            properties =
                properties.child(self.mission_property("Workspace", row.workspace_name.clone()));
        }
        properties = properties.child(self.mission_property("Where", row.location.clone()));
        match &row.usage {
            Some(usage) => {
                if !usage.model.is_empty() {
                    properties = properties.child(self.mission_property(
                        "Model",
                        div().text_color(theme.branch).child(usage.model.clone()),
                    ));
                }
                properties = properties.child(self.mission_property(
                    "Tokens",
                    format!(
                        "{} in · {} out · {} cache",
                        fmt_tokens(usage.tokens_in),
                        fmt_tokens(usage.tokens_out),
                        fmt_tokens(usage.cache_tokens)
                    ),
                ));
                if let Some(context) = usage.context {
                    let warns = context >= COMPACT_AT;
                    let headroom = ((COMPACT_AT - context) * 100.0).max(0.0).round();
                    properties = properties.child(
                        self.mission_property(
                            "Context",
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(style::meter(
                                    "pilus-mission-detail-ctx",
                                    Some(context),
                                    if warns { theme.alert } else { theme.accent },
                                    theme,
                                    style::scaled(&self.shared_settings, 6.0),
                                ))
                                .child(
                                    div()
                                        .text_size(px(fonts.secondary))
                                        .text_color(if warns { theme.alert } else { theme.muted })
                                        .child(format!(
                                            "{}% used · {headroom}% until compact",
                                            (context * 100.0).round()
                                        )),
                                ),
                        ),
                    );
                }
                if let Some(cost) = usage.cost_usd {
                    properties = properties.child(
                        self.mission_property(
                            "Cost",
                            div()
                                .text_color(theme.ok)
                                .child(format!("${cost:.2} (estimate)")),
                        ),
                    );
                }
            }
            None => {
                properties = properties.child(self.mission_property(
                    "Usage",
                    div().text_color(theme.muted).child("No usage data yet"),
                ));
            }
        }
        let mut column = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(title)
            .child(properties);
        let hint = row
            .pane
            .as_ref()
            .and_then(|pane| self.mission.hints.get(pane));
        if row.state == "blocked"
            && let Some(hint) = hint
        {
            column = column.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_3()
                    .rounded_md()
                    .bg(theme.panel_dim)
                    .border_1()
                    .border_color(theme.border)
                    .child(style::section_label(theme, "WAITING ON", fonts.label))
                    .child(
                        div()
                            .text_size(px(fonts.secondary))
                            .text_color(theme.alert)
                            .child(hint.clone()),
                    ),
            );
        }
        if self.mission.answering {
            self.mission_answer.update(cx, |input, _| {
                input.set_style(
                    theme,
                    style::chrome_font(&self.shared_settings),
                    px(fonts.body),
                );
            });
            column = column.child(style::enter(
                "pilus-mission-answer",
                style::field(
                    "pilus-mission-answer-field",
                    "Answer",
                    true,
                    theme,
                    style::scaled(&self.shared_settings, style::CONTROL_HEIGHT),
                    fonts.label,
                    self.mission_answer.clone(),
                ),
            ));
        }
        column
    }

    fn mission_agent_actions(&self, row: &MissionRow, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let button = |id: &'static str, label: &'static str, kind: ButtonKind| {
            style::small_button(id, label, kind, theme, fonts.secondary)
        };
        let mut actions = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .p_3()
            .border_t_1()
            .border_color(terminal_inset_border());
        if row.resumable() {
            return actions
                .child(
                    button("pilus-mission-resume", "Resume", ButtonKind::Primary)
                        .on_click(cx.listener(|this, _, _, cx| this.mission_go(cx))),
                )
                .child(div().flex_1())
                .child(
                    button("pilus-mission-dismiss", "Dismiss", ButtonKind::Ghost).on_click(
                        cx.listener(|this, _, _, cx| this.mission_row_op(RowOp::Close, cx)),
                    ),
                );
        }
        if self.mission.answering {
            return actions
                .child(div().flex_1())
                .child(
                    button("pilus-mission-answer-cancel", "Cancel", ButtonKind::Ghost).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.mission.answering = false;
                            cx.notify();
                        }),
                    ),
                )
                .child(
                    button("pilus-mission-answer-send", "Send", ButtonKind::Primary)
                        .on_click(cx.listener(|this, _, _, cx| this.mission_send_answer(cx))),
                );
        }
        let blocked = row.state == "blocked";
        actions = actions.child(
            button("pilus-mission-go", "Go", ButtonKind::Primary)
                .on_click(cx.listener(|this, _, _, cx| this.mission_go(cx))),
        );
        if blocked {
            actions = actions.child(
                button("pilus-mission-approve", "Approve", ButtonKind::Secondary).on_click(
                    cx.listener(|this, _, _, cx| this.mission_row_op(RowOp::Approve, cx)),
                ),
            );
        }
        actions = actions.child(
            button("pilus-mission-answer", "Answer", ButtonKind::Secondary)
                .on_click(cx.listener(|this, _, _, cx| this.mission_open_answer(cx))),
        );
        if row.state == "working" {
            actions = actions.child(
                button("pilus-mission-stop", "Stop", ButtonKind::Secondary)
                    .on_click(cx.listener(|this, _, _, cx| this.mission_row_op(RowOp::Stop, cx))),
            );
        }
        actions = actions.child(
            button("pilus-mission-fork", "Fork", ButtonKind::Ghost)
                .on_click(cx.listener(|this, _, _, cx| this.mission_row_op(RowOp::Fork, cx))),
        );
        let confirming = self
            .mission
            .selected_key()
            .is_some_and(|key| self.mission.confirm_close.as_deref() == Some(key));
        actions.child(div().flex_1()).child(
            button(
                "pilus-mission-close-pane",
                if confirming { "Confirm close" } else { "Close" },
                if confirming {
                    ButtonKind::Danger
                } else {
                    ButtonKind::Ghost
                },
            )
            .on_click(cx.listener(|this, _, _, cx| this.mission_row_op(RowOp::Close, cx))),
        )
    }

    fn mission_automation_detail(&self, id: &str) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let detail = match &self.mission.automation {
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
                        format!("pilus-mission-automation-skeleton-{index}"),
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
            .child(self.mission_property("State", if detail.enabled { "Armed" } else { "Paused" }))
            .child(self.mission_property("Schedule", detail.schedule.clone()))
            .child(self.mission_property("Policy", detail.policy.clone()))
            .child(self.mission_property("Target", detail.target.clone()))
            .child(self.mission_property("Agent", detail.agent_id.clone()));
        if !detail.mode.is_empty() {
            properties = properties.child(self.mission_property("Run in", detail.mode.clone()));
        }
        if !detail.access.is_empty() {
            properties = properties.child(self.mission_property(
                "Access",
                super::status_label(&detail.access.replace('_', " ")),
            ));
        }
        if !detail.paths.is_empty() {
            let mut paths = div().flex().flex_row().flex_wrap().gap_1();
            for path in &detail.paths {
                paths = paths.child(style::chip(theme, path.clone(), fonts.label));
            }
            properties = properties.child(self.mission_property("Paths", paths));
        }
        let mono = SharedString::from(style::mono_family(&self.shared_settings));
        let block = |text: String| {
            div()
                .p_3()
                .rounded_md()
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
        let task = format!(
            "{}{}",
            detail.title,
            if detail.prompt.is_empty() {
                String::new()
            } else {
                format!("\n\n{}", detail.prompt)
            }
        );
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

    fn mission_automation_actions(&self, id: &str, cx: &mut Context<Self>) -> Option<Div> {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let enabled = match &self.mission.automation {
            Some((open, Some(Ok(detail)))) if open == id => detail.enabled,
            _ => return None,
        };
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
                .border_color(terminal_inset_border())
                .child(
                    style::small_button(
                        "pilus-mission-automation-run",
                        "Run now",
                        ButtonKind::Primary,
                        theme,
                        fonts.secondary,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.mission_automation_op(run_id.clone(), true, false, cx);
                    })),
                )
                .child(
                    style::small_button(
                        "pilus-mission-automation-toggle",
                        if enabled { "Pause" } else { "Enable" },
                        ButtonKind::Secondary,
                        theme,
                        fonts.secondary,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.mission_automation_op(toggle_id.clone(), false, !enabled, cx);
                    })),
                )
                .child(div().flex_1())
                .child(
                    style::small_button(
                        "pilus-mission-automation-close",
                        "Close",
                        ButtonKind::Ghost,
                        theme,
                        fonts.secondary,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.mission_select(None, cx))),
                ),
        )
    }

    /// Live agents per state: the daemon's AGENT STATUS matrix.
    fn mission_status_counts(&self, rows: &[&MissionRow]) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let mut grid = div().flex().flex_row().flex_wrap().gap_2();
        for state in ["working", "blocked", "done", "idle"] {
            let count = rows
                .iter()
                .filter(|row| !row.resumable() && row.state == state)
                .count();
            let color = status_color(state, theme);
            grid = grid.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .h(px(style::scaled(&self.shared_settings, 24.0)))
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .text_size(px(fonts.secondary))
                    .child(div().w(px(7.0)).h(px(7.0)).rounded_full().bg(color))
                    .child(
                        div()
                            .text_color(theme.panel_fg)
                            .child(super::status_label(state)),
                    )
                    .child(div().text_color(theme.muted).child(format!("{count:02}"))),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(style::section_label(theme, "AGENT STATUS", fonts.label))
            .child(grid)
    }

    fn mission_cost_chart(&self, rows: &[&MissionRow]) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let totals = cost_by_model(rows);
        let mut column = div().flex().flex_col().gap_2().child(style::section_label(
            theme,
            "COST BY MODEL",
            fonts.label,
        ));
        let Some(max) = totals.first().map(|(_, cost)| cost.max(1e-9)) else {
            return column.child(
                div()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child("Cost data unavailable"),
            );
        };
        for (model, cost) in totals {
            column = column.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .text_size(px(fonts.secondary))
                    .child(
                        div()
                            .w(px(style::scaled(&self.shared_settings, 52.0)))
                            .flex_shrink_0()
                            .truncate()
                            .text_color(theme.branch)
                            .child(model.clone()),
                    )
                    .child(div().flex_1().child(style::meter(
                        format!("pilus-mission-cost-{model}"),
                        Some((cost / max) as f32),
                        theme.accent,
                        theme,
                        style::scaled(&self.shared_settings, 8.0),
                    )))
                    .child(
                        div()
                            .w(px(style::scaled(&self.shared_settings, 52.0)))
                            .flex()
                            .justify_end()
                            .flex_shrink_0()
                            .text_color(theme.ok)
                            .child(fmt_cost(cost)),
                    ),
            );
        }
        column
    }

    fn mission_empty(&self) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        div()
            .flex()
            .flex_col()
            .items_center()
            .gap_3()
            .py_8()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .child(icon(
                include_bytes!("../../assets/icons/gauge.svg"),
                fonts.headline,
                theme.muted,
            ))
            .child(style::title(theme, "No agents running", fonts.title))
            .child(
                div()
                    .max_w(px(360.0))
                    .text_center()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(
                        "Agents you start show up here with their context, tokens, and cost. \
                         Sessions you can resume are listed too.",
                    ),
            )
    }

    fn mission_message(&self, title: &'static str, detail: String) -> Div {
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

    fn mission_skeleton(&self) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let mut tiles = div().flex().flex_row().gap_3();
        for index in 0..4 {
            tiles = tiles.child(
                div()
                    .flex_1()
                    .h(px(style::scaled(&self.shared_settings, 76.0)))
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .p_3()
                    .child(style::skeleton(
                        format!("pilus-mission-tile-skeleton-{index}"),
                        theme,
                        gpui::relative(0.5),
                        fonts.label,
                    )),
            );
        }
        let mut column = div().flex().flex_col().gap_4().child(tiles);
        for (index, width) in [0.7, 0.55, 0.62].into_iter().enumerate() {
            column = column.child(
                div()
                    .flex()
                    .items_center()
                    .h(px(style::scaled(&self.shared_settings, ROW_HEIGHT)))
                    .px_2()
                    .child(style::skeleton(
                        format!("pilus-mission-row-skeleton-{index}"),
                        theme,
                        gpui::relative(width),
                        fonts.body,
                    )),
            );
        }
        column
    }
}

/// A list row's resting and hover fills: the selected row holds the panel
/// pill, like the Tasks page.
fn row_fills(selected: bool, theme: Theme) -> (Hsla, Hsla) {
    if selected {
        (theme.panel.into(), theme.panel.into())
    } else {
        (gpui::transparent_black(), theme.panel_dim.into())
    }
}

/// The answer field's entity, built once with the window.
pub(super) fn answer_input(
    font: gpui::Font,
    size: f32,
    theme: Theme,
    cx: &mut Context<PilusView>,
) -> Entity<TextInput> {
    cx.new(|cx| {
        TextInput::new(
            String::new(),
            "Type a reply and press Enter",
            font,
            px(size),
            false,
            theme,
            cx,
        )
        .submit_on_enter()
    })
}

#[cfg(test)]
mod tests {
    use pilus_client::{MissionRow, MissionUsage};

    use super::{
        Signal, blocked_line, civil_from_days, cost_by_model, fmt_cost, fmt_tokens, resume_ordinal,
        row_keys, short_model, step_key, until_label, utc_label,
    };

    fn row(pane: Option<&str>, agent: &str, state: &str) -> MissionRow {
        MissionRow {
            pane: pane.map(str::to_string),
            agent: agent.to_string(),
            state: state.to_string(),
            workspace_id: "ws1".to_string(),
            ..MissionRow::default()
        }
    }

    #[test]
    fn keys_are_stable_for_live_and_resumable_rows() {
        let rows = [
            row(Some("4"), "claude", "blocked"),
            row(None, "codex", "idle"),
            row(None, "codex", "idle"),
            row(None, "claude", "idle"),
        ];
        let keys = row_keys(&rows);
        assert_eq!(
            keys,
            [
                "pane:4",
                "resume:codex:ws1:0",
                "resume:codex:ws1:1",
                "resume:claude:ws1:0"
            ]
        );
        assert_eq!(resume_ordinal(&keys[2]), 1);
    }

    #[test]
    fn selection_steps_and_clamps() {
        let keys: Vec<String> = ["a", "b", "c"].map(str::to_string).to_vec();
        assert_eq!(step_key(&keys, None, 1).as_deref(), Some("a"));
        assert_eq!(step_key(&keys, None, -1).as_deref(), Some("c"));
        assert_eq!(step_key(&keys, Some("c"), 1).as_deref(), Some("c"));
        assert_eq!(step_key(&keys, Some("b"), -1).as_deref(), Some("a"));
        assert_eq!(step_key(&[], None, 1), None);
    }

    #[test]
    fn signal_puts_blocked_agents_first() {
        let blocked = row(Some("1"), "claude", "blocked");
        let working = row(Some("2"), "codex", "working");
        // A resumable row never raises the signal, whatever it reports.
        let resumable = row(None, "claude", "blocked");
        assert_eq!(Signal::of(&[&working, &blocked]), Signal::Attention);
        assert_eq!(Signal::of(&[&working]), Signal::Active);
        assert_eq!(Signal::of(&[&resumable]), Signal::Standby);
    }

    #[test]
    fn spend_groups_by_model_largest_first() {
        let mut opus = row(Some("1"), "claude", "idle");
        opus.usage = Some(MissionUsage {
            model: "claude-opus-4".into(),
            cost_usd: Some(2.0),
            ..MissionUsage::default()
        });
        let mut opus_too = opus.clone();
        opus_too.usage.as_mut().expect("usage").cost_usd = Some(3.0);
        let mut gpt = row(Some("2"), "codex", "idle");
        gpt.usage = Some(MissionUsage {
            model: "gpt-5-codex".into(),
            cost_usd: Some(4.0),
            ..MissionUsage::default()
        });
        let unknown = row(Some("3"), "pi", "idle");
        let totals = cost_by_model(&[&gpt, &opus, &opus_too, &unknown]);
        assert_eq!(
            totals,
            [("opus".to_string(), 5.0), ("gpt-5".to_string(), 4.0)]
        );
    }

    #[test]
    fn figures_format_compactly() {
        assert_eq!(fmt_tokens(945), "945");
        assert_eq!(fmt_tokens(12_300), "12.3k");
        assert_eq!(fmt_tokens(30_200_000), "30.2M");
        assert_eq!(fmt_cost(3.2), "$3.20");
        assert_eq!(fmt_cost(17_700.0), "$17.7k");
        assert_eq!(short_model("claude-sonnet-4-5"), "sonnet");
        assert_eq!(short_model("mystery-model-xl"), "mystery-mo");
    }

    #[test]
    fn blocked_line_skips_frames_and_blank_lines() {
        let output = "Edit src/main.rs?\n│ ❯ 1. Yes                │\n╰──────────╯\n\n";
        assert_eq!(blocked_line(output).as_deref(), Some("❯ 1. Yes"));
        assert_eq!(blocked_line("  \n───\n"), None);
    }

    #[test]
    fn deadlines_read_relative_and_in_utc() {
        assert_eq!(until_label(100, 100), "due");
        assert_eq!(until_label(100, 400), "in 5m");
        assert_eq!(until_label(0, 7200), "in 2h");
        assert_eq!(until_label(0, 172_800), "in 2d");
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2026-10-06 14:00:00 UTC.
        assert_eq!(utc_label(1_791_295_200), "Oct 06 14:00 UTC");
    }
}
