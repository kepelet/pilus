//! The native Insights page: how much each agent and model was used, over a
//! window you pick, and a year of activity as a heatmap.
//!
//! The figures come from the same `pilus-index` file Search reads, from the
//! hourly token buckets it keeps for every transcript. The page owns no data
//! of its own and asks for no permission of its own: it is gated by the same
//! `Settings::index_agents`, shows the same consent card until it is given,
//! and is refreshed by the same background read (`search_sync`). Token counts
//! are what each turn's usage block reported, so they are estimates of use, not
//! a bill.
//!
//! The first half holds the pure rules (days in the person's time zone, heat
//! levels, number and date labels), unit-tested without GPUI; the second half
//! holds the page's state and renders.

use std::collections::BTreeMap;
use std::process::Command;

use gpui::{Context, Div, KeyDownEvent, ScrollHandle, div, prelude::*, px};
use pilus_index::{Cost, Hourly, Index, Overview, Ranked, Scope, Tokens};

use super::style::{self, ButtonKind, RadiusExt};
use super::tasks::unix_now;
use super::{PilusView, icon};

/// Weeks of history the heatmap shows.
const HEAT_WEEKS: i64 = 52;
/// Rows in each ranking.
const RANK_ROWS: usize = 6;
/// Months of cost the page lists, newest first.
const COST_MONTHS: usize = 6;
const HEAT_SIDE: f32 = 11.0;
const HEAT_GAP: f32 = 3.0;

// -- Pure rules ----------------------------------------------------------

/// The window the headline figures and rankings cover.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Range {
    Week,
    Month,
    Quarter,
    All,
}

impl Range {
    const ALL: [Self; 4] = [Self::Week, Self::Month, Self::Quarter, Self::All];

    fn label(self) -> &'static str {
        match self {
            Self::Week => "7 days",
            Self::Month => "30 days",
            Self::Quarter => "90 days",
            Self::All => "All time",
        }
    }

    /// How many days back the window reaches, or `None` for all of them.
    const fn days(self) -> Option<i64> {
        match self {
            Self::Week => Some(7),
            Self::Month => Some(30),
            Self::Quarter => Some(90),
            Self::All => None,
        }
    }
}

/// One local day's use.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Day {
    turns: u64,
    tokens: u64,
}

/// The day number (days since 1970-01-01) of unix `hour` for a viewer
/// `offset` seconds east of UTC.
fn local_day(hour: i64, offset: i64) -> i64 {
    (hour * 3600 + offset).div_euclid(86_400)
}

/// 0 for Sunday through 6 for Saturday. Day 0, 1970-01-01, was a Thursday.
fn weekday(day: i64) -> usize {
    usize::try_from((day + 4).rem_euclid(7)).unwrap_or(0)
}

/// The calendar date of a day number, by the civil-calendar algorithm, so the
/// page needs no date crate for a label.
fn civil(day: i64) -> (i64, usize, i64) {
    let shifted = day + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let date = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, usize::try_from(month - 1).unwrap_or(0), date)
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// `Oct 9`, or `Oct 9, 2025` for another year than `today`'s.
fn date_label(day: i64, today: i64) -> String {
    let (year, month, date) = civil(day);
    if year == civil(today).0 {
        format!("{} {date}", MONTHS[month])
    } else {
        format!("{} {date}, {year}", MONTHS[month])
    }
}

/// The day number of a calendar date (`month` counted from 0), the inverse of
/// [`civil`].
fn days_from_civil(year: i64, month: usize, date: i64) -> i64 {
    let month = i64::try_from(month).unwrap_or(0) + 1;
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let shifted = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted + 2) / 5 + date - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The `count` calendar months ending with `today`'s, oldest first, as
/// `(year, month)`.
fn last_months(today: i64, count: usize) -> Vec<(i64, usize)> {
    let (year, month, _) = civil(today);
    let index = year * 12 + i64::try_from(month).unwrap_or(0);
    (0..count)
        .rev()
        .map(|back| {
            let at = index - i64::try_from(back).unwrap_or(0);
            (
                at.div_euclid(12),
                usize::try_from(at.rem_euclid(12)).unwrap_or(0),
            )
        })
        .collect()
}

/// Group hourly cost into local calendar months.
fn month_costs(rows: &[Hourly], offset: i64) -> BTreeMap<(i64, usize), Cost> {
    let mut months: BTreeMap<(i64, usize), Cost> = BTreeMap::new();
    for row in rows {
        let (year, month, _) = civil(local_day(row.hour, offset));
        let cost = months.entry((year, month)).or_default();
        cost.micros += row.cost.micros;
        cost.unpriced += row.cost.unpriced;
    }
    months
}

/// `$12.34` under a hundred dollars, `$1,234` above, and `<$0.01` for a cost
/// too small to show.
fn money(micros: u64) -> String {
    let cents = (micros + 5_000) / 10_000;
    if cents == 0 && micros > 0 {
        return "<$0.01".to_string();
    }
    if cents < 10_000 {
        return format!("${}.{:02}", cents / 100, cents % 100);
    }
    // From a hundred dollars up the cents are noise: round to the dollar.
    let digits = ((cents + 50) / 100).to_string();
    let mut grouped = String::new();
    for (at, digit) in digits.chars().enumerate() {
        if at > 0 && (digits.len() - at).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    format!("${grouped}")
}

/// Group hourly use into local days.
fn day_totals(rows: &[Hourly], offset: i64) -> BTreeMap<i64, Day> {
    let mut days: BTreeMap<i64, Day> = BTreeMap::new();
    for row in rows {
        let day = days.entry(local_day(row.hour, offset)).or_default();
        day.turns += row.turns;
        day.tokens += row.tokens;
    }
    days
}

/// How many days in a row, ending today (or yesterday, while today is young),
/// had any use.
fn streak(days: &BTreeMap<i64, Day>, today: i64) -> u64 {
    let active = |day: i64| days.get(&day).is_some_and(|day| day.turns > 0);
    let mut cursor = if active(today) { today } else { today - 1 };
    let mut run = 0;
    while active(cursor) {
        run += 1;
        cursor -= 1;
    }
    run
}

/// The turn counts that split the active days into four equal groups, so the
/// shades spread over however busy the person is rather than over a fixed
/// scale that one huge day would flatten.
fn heat_cuts(days: &BTreeMap<i64, Day>) -> [u64; 3] {
    let mut turns: Vec<u64> = days
        .values()
        .map(|day| day.turns)
        .filter(|turns| *turns > 0)
        .collect();
    turns.sort_unstable();
    let at = |quarter: usize| {
        turns
            .get((turns.len() * quarter / 4).min(turns.len().saturating_sub(1)))
            .copied()
            .unwrap_or(0)
    };
    [at(1), at(2), at(3)]
}

/// The shade for a day with `turns`, 0 for none and 1 to 4 above it.
fn heat_level(turns: u64, cuts: [u64; 3]) -> usize {
    match turns {
        0 => 0,
        t if t <= cuts[0] => 1,
        t if t <= cuts[1] => 2,
        t if t <= cuts[2] => 3,
        _ => 4,
    }
}

/// `1.2K`, `3.4M`, `5.6B`: three significant figures at most.
fn compact(count: u64) -> String {
    let scaled = |divisor: f64, unit: &str| {
        // Counts are far below f64's exact range, and this is for display.
        #[allow(clippy::cast_precision_loss)]
        let value = count as f64 / divisor;
        if value >= 100.0 {
            format!("{value:.0}{unit}")
        } else if value >= 10.0 {
            format!("{value:.1}{unit}")
        } else {
            format!("{value:.2}{unit}")
        }
    };
    match count {
        0..=999 => count.to_string(),
        1_000..=999_999 => scaled(1e3, "K"),
        1_000_000..=999_999_999 => scaled(1e6, "M"),
        _ => scaled(1e9, "B"),
    }
}

/// Seconds east of UTC from `date +%z` output (`+0700`, `-0530`).
fn parse_offset(text: &str) -> Option<i64> {
    let text = text.trim();
    let sign = match text.chars().next()? {
        '+' => 1,
        '-' => -1,
        _ => return None,
    };
    let digits = text.get(1..5)?;
    let hours: i64 = digits.get(..2)?.parse().ok()?;
    let minutes: i64 = digits.get(2..)?.parse().ok()?;
    Some(sign * (hours * 3600 + minutes * 60))
}

/// This Mac's offset from UTC now, or 0 when `date` will not say.
fn utc_offset() -> i64 {
    Command::new("date")
        .arg("+%z")
        .output()
        .ok()
        .and_then(|out| parse_offset(&String::from_utf8_lossy(&out.stdout)))
        .unwrap_or(0)
}

// -- State ---------------------------------------------------------------

/// What the page shows, read from the index.
struct Data {
    overview: Overview,
    agents: Vec<Ranked>,
    models: Vec<Ranked>,
    projects: Vec<Ranked>,
    /// What each machine used, when the window spans more than one.
    machines: Vec<Ranked>,
    /// A year of local days, which the heatmap and the streak read.
    days: BTreeMap<i64, Day>,
    /// The cost of everything indexed, whatever the window.
    cost_all: Cost,
    /// The last [`COST_MONTHS`] months' cost, oldest first.
    months: Vec<(i64, usize, Cost)>,
    /// Active days inside the chosen window.
    active_days: u64,
    today: i64,
}

/// The page's state.
pub(super) struct StatsPage {
    open: bool,
    range: Range,
    machine: Option<String>,
    data: Option<Data>,
    reader: Option<Index>,
    /// Seconds east of UTC, read when the page opens.
    offset: i64,
    error: Option<String>,
    /// The page's scroll position, so a capture can scroll it.
    scroll: ScrollHandle,
}

impl Default for StatsPage {
    fn default() -> Self {
        Self {
            open: false,
            range: Range::Month,
            machine: None,
            data: None,
            reader: None,
            offset: 0,
            error: None,
            scroll: ScrollHandle::new(),
        }
    }
}

impl StatsPage {
    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    pub(super) fn close(&mut self) {
        self.open = false;
    }
}

impl PilusView {
    /// Open the page. With permission it also starts reading.
    pub(super) fn open_stats(&mut self, cx: &mut Context<Self>) {
        self.leave_settings_page();
        self.stats.open = true;
        self.stats.offset = utc_offset();
        self.search_prepare();
        if self.settings.index_agents {
            self.search_sync();
            self.stats_reload();
        }
        cx.notify();
    }

    fn close_stats(&mut self, cx: &mut Context<Self>) {
        self.stats.close();
        self.search.close();
        cx.notify();
    }

    /// Read the figures again, after a sync or a change of window.
    pub(super) fn stats_reload(&mut self) {
        if !self.settings.index_agents || !self.stats.open {
            return;
        }
        if self.stats.reader.is_none() {
            match Index::open(&self.search_index_path()) {
                Ok(index) => self.stats.reader = Some(index),
                Err(error) => {
                    self.stats.error = Some(error.to_string());
                    return;
                }
            }
        }
        let Some(reader) = self.stats.reader.as_ref() else {
            return;
        };
        let now = i64::try_from(unix_now()).unwrap_or(0);
        let page = &self.stats;
        match build(reader, page.range, page.machine.clone(), now, page.offset) {
            Ok(data) => {
                self.stats.data = Some(data);
                self.stats.error = None;
            }
            Err(error) => self.stats.error = Some(error.to_string()),
        }
    }

    fn stats_set_range(&mut self, range: Range, cx: &mut Context<Self>) {
        self.stats.range = range;
        self.stats_reload();
        cx.notify();
    }

    fn stats_set_machine(&mut self, machine: Option<String>, cx: &mut Context<Self>) {
        self.stats.machine = machine;
        self.stats_reload();
        cx.notify();
    }

    /// The page's keys.
    pub(super) fn stats_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        if event.keystroke.modifiers.platform || event.keystroke.modifiers.control {
            return;
        }
        let reading = self.settings.index_agents && !self.search.sources;
        match key {
            "escape" | "q" => {
                if self.search.sources && self.settings.index_agents {
                    self.search.sources = false;
                    self.search.confirm_off = false;
                    cx.notify();
                } else {
                    self.close_stats(cx);
                }
            }
            "r" if reading => {
                self.search_sync();
                cx.notify();
            }
            "1" | "2" | "3" | "4" if reading => {
                let at = usize::from(key.as_bytes()[0] - b'1');
                self.stats_set_range(Range::ALL[at], cx);
            }
            _ => {}
        }
    }
}

/// Read every figure the page shows for `range` and `machine`.
fn build(
    index: &Index,
    range: Range,
    machine: Option<String>,
    now: i64,
    offset: i64,
) -> pilus_index::Result<Data> {
    let today = local_day(now.div_euclid(3600), offset);
    // The first unix hour of local day `day`.
    let day_start = |day: i64| (day * 86_400 - offset).div_euclid(3600);
    let window = Scope {
        machine: machine.clone(),
        since_hour: range.days().map(|days| day_start(today - days + 1)),
    };
    let year_start = day_start(today - HEAT_WEEKS * 7);
    let listed = last_months(today, COST_MONTHS);
    let (first_year, first_month) = listed.first().copied().unwrap_or((1970, 0));
    let month_start = day_start(days_from_civil(first_year, first_month, 1));
    // One read covers the heatmap, the months and the window: the longest
    // reach of the three, unless the window is all time.
    let reach = Scope {
        machine: machine.clone(),
        since_hour: window
            .since_hour
            .map(|since| since.min(year_start).min(month_start)),
    };
    let rows = index.hourly(&reach)?;
    let days = day_totals(&rows, offset);
    let active_days = days
        .range(range.days().map_or(i64::MIN, |days| today - days + 1)..)
        .filter(|(_, day)| day.turns > 0)
        .count() as u64;
    let monthly = month_costs(&rows, offset);
    let months = listed
        .into_iter()
        .map(|(year, month)| {
            (
                year,
                month,
                monthly.get(&(year, month)).copied().unwrap_or_default(),
            )
        })
        .collect();
    let all_time = Scope {
        machine: reach.machine.clone(),
        since_hour: None,
    };
    Ok(Data {
        cost_all: index.overview(&all_time)?.cost,
        months,
        overview: index.overview(&window)?,
        agents: index.top_agents(&window)?,
        models: index.top_models(&window, RANK_ROWS)?,
        projects: index.top_projects(&window, RANK_ROWS)?,
        machines: index.top_machines(&window)?,
        days,
        active_days,
        today,
    })
}

// -- Rendering -----------------------------------------------------------

impl PilusView {
    /// The page, drawn in place of the terminal card.
    pub(super) fn stats_page(&mut self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let allowed = self.settings.index_agents;
        let reading = allowed && !self.search.sources;

        let mut actions = div().flex().flex_row().items_center().gap_2();
        if reading {
            let mut range = style::segmented(theme);
            for choice in Range::ALL {
                range = range.child(
                    style::segment(
                        format!("pilus-stats-range-{}", choice.label()),
                        choice.label(),
                        self.stats.range == choice,
                        true,
                        theme,
                        fonts.label,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.stats_set_range(choice, cx))),
                );
            }
            actions = actions.child(range);
            actions = actions
                .child(
                    style::small_button(
                        "pilus-stats-refresh",
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
                        "pilus-stats-sources",
                        "Sources",
                        ButtonKind::Secondary,
                        theme,
                        fonts.label,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.search.sources = true;
                        cx.notify();
                    })),
                );
        }
        actions = actions.child(
            style::icon_button(
                "pilus-stats-close",
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
            .on_click(cx.listener(|this, _, _, cx| this.close_stats(cx))),
        );

        let bar = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_end()
            .flex_shrink_0()
            .flex_wrap()
            .gap_3()
            .px_4()
            .py_3()
            .child(actions);

        let body = if !reading {
            self.search_consent(cx).into_any_element()
        } else {
            self.stats_body(cx)
        };
        div()
            .flex()
            .flex_col()
            .size_full()
            .min_h(px(0.0))
            .child(bar)
            .child(body)
    }

    fn stats_body(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let Some(data) = self
            .stats
            .data
            .as_ref()
            .filter(|data| data.overview.sessions > 0)
        else {
            let (title, detail) = if self.search.syncing || self.stats.data.is_none() {
                (
                    "Reading your sessions…",
                    "The first read of a large history can take a little while.",
                )
            } else {
                (
                    "No usage in this window",
                    "Pick a longer window, or use an agent and refresh.",
                )
            };
            return div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child(style::empty_state(
                    theme,
                    fonts,
                    icon(
                        include_bytes!("../../assets/icons/activity.svg"),
                        24.0,
                        theme.muted,
                    ),
                    title,
                    detail,
                ))
                .into_any_element();
        };

        let tokens = data.overview.tokens;
        let cost = data.overview.cost;
        let today = data.today;
        // Flush tiles with no rules: the page's cards carry the borders, and a
        // frame around the headline figures would crowd them.
        let tiles = div()
            .flex()
            .flex_row()
            .px_4()
            .child(
                style::stat_tile(
                    theme,
                    fonts,
                    "TOKENS",
                    compact(tokens.total()),
                    theme.accent,
                    format!("{} cached", compact(tokens.cache_read + tokens.cache_write)),
                )
                .border_r_0(),
            )
            .child(
                style::stat_tile(
                    theme,
                    fonts,
                    "EST. COST",
                    money(cost.micros),
                    theme.panel_fg,
                    format!("{} all time", money(data.cost_all.micros)),
                )
                .border_r_0(),
            )
            .child(
                style::stat_tile(
                    theme,
                    fonts,
                    "OUTPUT",
                    compact(tokens.output),
                    theme.panel_fg,
                    format!("{} in", compact(tokens.input)),
                )
                .border_r_0(),
            )
            .child(
                style::stat_tile(
                    theme,
                    fonts,
                    "SESSIONS",
                    data.overview.sessions.to_string(),
                    theme.panel_fg,
                    format!("{} turns", compact(tokens.turns)),
                )
                .border_r_0(),
            )
            .child(
                style::stat_tile(
                    theme,
                    fonts,
                    "ACTIVE DAYS",
                    data.active_days.to_string(),
                    theme.panel_fg,
                    format!("{} day streak", streak(&data.days, today)),
                )
                .border_r_0(),
            );

        let allowed = self.search_allowed();
        let mut machines = div().flex().flex_row().items_center();
        if allowed.len() > 1 {
            let mut choices = style::segmented(theme).child(
                style::segment(
                    "pilus-stats-machine-all",
                    "All machines",
                    self.stats.machine.is_none(),
                    true,
                    theme,
                    fonts.label,
                )
                .on_click(cx.listener(|this, _, _, cx| this.stats_set_machine(None, cx))),
            );
            for choice in &allowed {
                let key = choice.key.clone();
                choices = choices.child(
                    style::segment(
                        format!("pilus-stats-machine-{}", choice.key),
                        choice.label.clone(),
                        self.stats.machine.as_deref() == Some(choice.key.as_str()),
                        true,
                        theme,
                        fonts.label,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.stats_set_machine(Some(key.clone()), cx);
                    })),
                );
            }
            machines = machines.child(choices);
        }

        // Only a page that spans machines needs to say which one a row is on.
        let spans_machines = self.stats.machine.is_none() && data.machines.len() > 1;
        let machine_label = |key: &str| {
            allowed.iter().find(|choice| choice.key == key).map_or_else(
                || {
                    if key == "local" {
                        "This Mac".to_string()
                    } else {
                        key.to_string()
                    }
                },
                |choice| choice.label.clone(),
            )
        };

        let agents = self.ranking(
            "pilus-stats-agent",
            data.agents.iter().map(|row| {
                let name = pilus_index::Agent::from_kind(&row.name)
                    .map_or_else(|| row.name.clone(), |agent| agent.label().to_string());
                (name, String::new(), row)
            }),
        );
        let models = self.ranking(
            "pilus-stats-model",
            data.models
                .iter()
                .map(|row| (row.name.clone(), String::new(), row)),
        );
        let projects = self.ranking(
            "pilus-stats-project",
            data.projects.iter().map(|row| {
                let name = if row.name.is_empty() {
                    "Unknown folder".to_string()
                } else {
                    self.shorten_cwd(&row.machine, &row.name)
                };
                let place = if spans_machines {
                    format!("{} · ", machine_label(&row.machine))
                } else {
                    String::new()
                };
                (name, place, row)
            }),
        );
        let by_machine = spans_machines.then(|| {
            self.ranking(
                "pilus-stats-by-machine",
                data.machines
                    .iter()
                    .map(|row| (machine_label(&row.name), String::new(), row)),
            )
        });

        let breakdown = self.breakdown(tokens);
        let costs = self.cost_block(data);
        let heat = self.heatmap(data, cx);

        // The cost by machine sits under the token breakdown. The blocks keep
        // their own height here, or the column would stretch them to match
        // the taller cost card.
        let mut tokens_column = div()
            .flex()
            .flex_col()
            .gap_4()
            .flex_1()
            .min_w(px(style::scaled(&self.shared_settings, 320.0)))
            .child(self.stats_block("TOKENS", breakdown).flex_none().w_full());
        if let Some(by_machine) = by_machine {
            tokens_column = tokens_column.child(
                self.stats_block("EST. COST BY MACHINE", by_machine)
                    .flex_none()
                    .w_full(),
            );
        }

        let column = div()
            .flex()
            .flex_col()
            .gap_4()
            .w_full()
            .max_w(px(style::scaled(&self.shared_settings, 1040.0)))
            .pb_6()
            .child(tiles)
            .child(machines.px_4())
            .child(div().px_4().child(self.stats_block("ACTIVITY", heat)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap_4()
                    .px_4()
                    .child(self.stats_block("TOP AGENTS", agents))
                    .child(self.stats_block("TOP MODELS", models)),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap_4()
                    .px_4()
                    .child(self.stats_block("EST. COST", costs))
                    .child(tokens_column),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .px_4()
                    .child(self.stats_block("TOP PROJECTS", projects)),
            );

        // A column, so the page is as tall as its content and scrolls; in a
        // row its child would stretch to the viewport and clip instead.
        div()
            .id("pilus-stats-scroll")
            .flex()
            .flex_col()
            .items_center()
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .restrict_scroll_to_axis()
            .track_scroll(&self.stats.scroll)
            .child(column)
            .into_any_element()
    }

    /// A titled block of the page, in the section frame the other pages use.
    pub(super) fn stats_block(&self, title: &'static str, body: impl IntoElement) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        div()
            .flex()
            .flex_col()
            .gap_3()
            .flex_1()
            .min_w(px(style::scaled(&self.shared_settings, 320.0)))
            .p_3()
            .radius_md()
            .bg(theme.panel_dim)
            .border_1()
            .border_color(theme.border)
            .child(style::section_label(theme, title, fonts.label))
            .child(body)
    }

    /// Rows of a ranking, each a meter against the leader.
    fn ranking<'a>(
        &self,
        id: &'static str,
        rows: impl Iterator<Item = (String, String, &'a Ranked)>,
    ) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let rows: Vec<(String, String, &Ranked)> = rows.collect();
        let lead = rows
            .first()
            .map_or(1, |(_, _, row)| row.tokens.total().max(1));
        let mut list = div().flex().flex_col().gap_3();
        if rows.is_empty() {
            list = list.child(
                div()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child("Nothing in this window"),
            );
        }
        for (at, (name, place, row)) in rows.into_iter().enumerate() {
            let total = row.tokens.total();
            // Counts are far below f32's exact range, and this is a bar width.
            #[allow(clippy::cast_precision_loss)]
            let fraction = total as f32 / lead as f32;
            // A row with no recorded cost and no known price says nothing about
            // cost rather than "$0.00".
            let spent = if row.cost.micros == 0 && row.cost.unpriced > 0 {
                String::new()
            } else {
                format!("{} · ", money(row.cost.micros))
            };
            list = list.child(style::rank_row(
                format!("{id}-{at}"),
                theme,
                fonts,
                name,
                format!(
                    "{place}{spent}{} tokens · {} sessions",
                    compact(total),
                    row.sessions
                ),
                fraction,
            ));
        }
        list
    }

    /// What the window and everything so far cost, and each recent month's.
    fn cost_block(&self, data: &Data) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let line = |label: &str, value: String| {
            div()
                .flex()
                .flex_row()
                .justify_between()
                .gap_3()
                .text_size(px(fonts.body))
                .child(div().text_color(theme.panel_fg).child(label.to_string()))
                .child(div().text_color(theme.muted).child(value))
        };
        let lead = data
            .months
            .iter()
            .map(|(_, _, cost)| cost.micros)
            .max()
            .unwrap_or(0)
            .max(1);
        let mut list = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(line(
                self.stats.range.label(),
                money(data.overview.cost.micros),
            ))
            .child(line("All time", money(data.cost_all.micros)));
        for (at, (year, month, cost)) in data.months.iter().rev().enumerate() {
            // Micro-dollars are far below f32's exact range; this is a bar width.
            #[allow(clippy::cast_precision_loss)]
            let fraction = cost.micros as f32 / lead as f32;
            list = list.child(style::rank_row(
                format!("pilus-stats-month-{at}"),
                theme,
                fonts,
                format!("{} {year}", MONTHS[*month]),
                money(cost.micros),
                fraction,
            ));
        }
        let mut notes = String::from(
            "Pi's costs are the figure it recorded for each turn. Claude's are estimated \
             from list prices, with cache writes at the 5-minute rate.",
        );
        if data.cost_all.unpriced > 0 {
            notes.push_str(&format!(
                " {} tokens from models with no known price are not counted.",
                compact(data.cost_all.unpriced)
            ));
        }
        list.child(
            div()
                .text_size(px(fonts.label))
                .text_color(theme.muted)
                .child(notes),
        )
    }

    /// Each kind of token with its count and share.
    fn breakdown(&self, tokens: Tokens) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let total = tokens.total().max(1);
        let mut list = div().flex().flex_col().gap_2();
        for (label, count) in [
            ("Cache read", tokens.cache_read),
            ("Input", tokens.input),
            ("Cache write", tokens.cache_write),
            ("Output", tokens.output),
        ] {
            // A share is for display, and counts are far below f64's exact range.
            #[allow(clippy::cast_precision_loss)]
            let share = count as f64 * 100.0 / total as f64;
            list = list.child(
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .gap_3()
                    .text_size(px(fonts.body))
                    .child(div().text_color(theme.panel_fg).child(label))
                    .child(
                        div()
                            .text_color(theme.muted)
                            .child(format!("{} · {share:.1}%", compact(count))),
                    ),
            );
        }
        list
    }

    /// A year of days, a column per week, shaded by how many turns the models
    /// answered that day.
    fn heatmap(&self, data: &Data, cx: &mut Context<Self>) -> Div {
        let _ = cx;
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let side = style::scaled(&self.shared_settings, HEAT_SIDE);
        let gap = style::scaled(&self.shared_settings, HEAT_GAP);
        let cuts = heat_cuts(&data.days);
        let today = data.today;
        let first = today - weekday(today) as i64 - (HEAT_WEEKS - 1) * 7;

        let mut months = div()
            .flex()
            .flex_row()
            .gap(px(gap))
            .h(px(fonts.label * 1.4));
        let mut grid = div().flex().flex_row().gap(px(gap));
        let mut last_month = usize::MAX;
        let mut last_label = -3;
        for week in 0..HEAT_WEEKS {
            let start = first + week * 7;
            let month = civil(start).1;
            // A month's name goes over its first column, unless the last name
            // is still too close for both to fit.
            let opens_month = week > 0 || civil(start).2 <= 7;
            let label = (month != last_month && opens_month && week - last_label >= 3)
                .then(|| MONTHS[month]);
            if label.is_some() {
                last_label = week;
            }
            last_month = month;
            months = months.child(
                div()
                    .w(px(side))
                    .flex_shrink_0()
                    .whitespace_nowrap()
                    .text_size(px(fonts.label))
                    .text_color(theme.muted)
                    .children(label),
            );
            let mut column = div().flex().flex_col().gap(px(gap));
            for row in 0..7 {
                let day = start + row;
                if day > today {
                    break;
                }
                let use_ = data.days.get(&day).copied().unwrap_or_default();
                let lines = if use_.turns == 0 {
                    vec![("No activity".to_string(), theme.muted)]
                } else {
                    vec![
                        (format!("{} turns", use_.turns), theme.muted),
                        (format!("{} tokens", compact(use_.tokens)), theme.muted),
                    ]
                };
                column = column.child(
                    style::heat_cell(
                        format!("pilus-stats-day-{day}"),
                        heat_level(use_.turns, cuts),
                        side,
                        theme,
                    )
                    .tooltip(super::row_tooltip(
                        date_label(day, today),
                        theme.panel_fg,
                        lines,
                        theme,
                    )),
                );
            }
            grid = grid.child(column);
        }

        let mut weekdays = div()
            .flex()
            .flex_col()
            .gap(px(gap))
            .pt(px(fonts.label * 1.4 + gap));
        for (row, name) in ["", "Mon", "", "Wed", "", "Fri", ""]
            .into_iter()
            .enumerate()
        {
            weekdays = weekdays.child(
                div()
                    .h(px(side))
                    .flex()
                    .items_center()
                    .text_size(px(fonts.label))
                    .text_color(theme.muted)
                    .id(format!("pilus-stats-weekday-{row}"))
                    .child(name),
            );
        }

        let mut legend = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(gap))
            .text_size(px(fonts.label))
            .text_color(theme.muted)
            .child("Less");
        for level in 0..style::HEAT_LEVELS {
            legend = legend.child(style::heat_cell(
                format!("pilus-stats-legend-{level}"),
                level,
                side,
                theme,
            ));
        }
        legend = legend.child("More");

        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .id("pilus-stats-heat-scroll")
                    .flex()
                    .flex_row()
                    .gap_2()
                    .overflow_x_scroll()
                    // Without this a vertical wheel turn scrolls the strip and
                    // a sideways swipe scrolls the page, as well as each other.
                    .restrict_scroll_to_axis()
                    .child(weekdays)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(gap))
                            .child(months)
                            .child(grid),
                    ),
            )
            .child(legend)
    }
}

/// Shot-harness entry point: the page over canned figures. It reads no index.
#[cfg(feature = "visual-test")]
impl PilusView {
    /// Scroll the page to its end, as a long page is read.
    pub(super) fn stats_scroll_to_end(&mut self, cx: &mut Context<Self>) {
        self.stats
            .scroll
            .set_offset(gpui::point(px(0.0), px(-100_000.0)));
        cx.notify();
    }

    pub(super) fn stats_show_sample(&mut self, allowed: bool, cx: &mut Context<Self>) {
        self.leave_settings_page();
        self.settings.index_agents = allowed;
        self.settings.index_machines = vec!["local".to_string()];
        *self.shared_settings.borrow_mut() = self.settings.clone();
        let now = i64::try_from(unix_now()).unwrap_or(0);
        let today = local_day(now.div_euclid(3600), 0);
        let mut days = BTreeMap::new();
        // A believable year: weekdays busy, weekends light, a quiet fortnight,
        // and a streak that runs into today.
        for back in 0..HEAT_WEEKS * 7 {
            let day = today - back;
            let quiet = (40..54).contains(&back);
            let weekend = matches!(weekday(day), 0 | 6);
            let wave = ((day * 7919) % 23).unsigned_abs();
            let turns = match (quiet, weekend) {
                (true, _) => 0,
                (_, true) => wave / 3,
                _ => 10 + wave * 4,
            };
            if turns > 0 {
                days.insert(
                    day,
                    Day {
                        turns,
                        tokens: turns * 61_000,
                    },
                );
            }
        }
        let ranked = |name: &str, sessions, input, output, cache| Ranked {
            name: name.to_string(),
            machine: "local".to_string(),
            sessions,
            tokens: Tokens {
                input,
                output,
                cache_read: cache,
                cache_write: cache / 40,
                turns: sessions * 40,
            },
            // About a dollar for every four million tokens, for the look of it.
            cost: Cost {
                micros: (input + output + cache / 8) / 4,
                unpriced: 0,
            },
        };
        self.stats.data = Some(Data {
            overview: Overview {
                tokens: Tokens {
                    input: 109_087_084,
                    output: 22_180_240,
                    cache_read: 5_158_946_048,
                    cache_write: 27_308_753,
                    turns: 29_609,
                },
                cost: Cost {
                    micros: 1_287_400_000,
                    unpriced: 0,
                },
                sessions: 445,
            },
            cost_all: Cost {
                micros: 4_912_300_000,
                unpriced: 18_000_000,
            },
            months: last_months(today, COST_MONTHS)
                .into_iter()
                .zip([
                    310_000_000_u64,
                    482_000_000,
                    655_000_000,
                    590_000_000,
                    1_020_000_000,
                    1_287_000_000,
                ])
                .map(|((year, month), micros)| {
                    (
                        year,
                        month,
                        Cost {
                            micros,
                            unpriced: 0,
                        },
                    )
                })
                .collect(),
            agents: vec![
                ranked("pi", 231, 60_000_000, 12_000_000, 3_500_000_000),
                ranked("claude", 214, 49_000_000, 10_000_000, 1_600_000_000),
            ],
            models: vec![
                ranked(
                    "deepseek-v4.1-flash",
                    90,
                    40_000_000,
                    8_000_000,
                    2_200_000_000,
                ),
                ranked(
                    "claude-sonnet-5-5",
                    197,
                    30_000_000,
                    8_000_000,
                    1_450_000_000,
                ),
                ranked(
                    "muse-spark-1.3-contributor",
                    100,
                    20_000_000,
                    3_000_000,
                    1_100_000_000,
                ),
                ranked("claude-opus-5-5", 14, 5_000_000, 900_000, 170_000_000),
            ],
            projects: vec![
                ranked(
                    "/Users/me/Developer/pilus",
                    152,
                    30_000_000,
                    6_000_000,
                    770_000_000,
                ),
                ranked(
                    "/Users/me/.superset/projects/l3s",
                    74,
                    20_000_000,
                    4_000_000,
                    740_000_000,
                ),
                Ranked {
                    machine: "studio".to_string(),
                    ..ranked(
                        "/Users/me/Developer/sipil",
                        40,
                        10_000_000,
                        2_000_000,
                        300_000_000,
                    )
                },
            ],
            machines: vec![
                ranked("local", 330, 80_000_000, 16_000_000, 3_900_000_000),
                ranked("studio", 115, 29_000_000, 6_000_000, 1_250_000_000),
            ],
            active_days: u64::try_from(days.len()).unwrap_or(0),
            days,
            today,
        });
        self.stats.open = true;
        self.search.synced_at = Some(std::time::Instant::now());
        self.search.syncing = false;
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hours_become_local_days() {
        // 2026-10-09 22:00 UTC is already the 10th at UTC+7.
        let hour = 1_791_583_200 / 3600;
        let utc = local_day(hour, 0);
        assert_eq!(civil(utc), (2026, 9, 9));
        assert_eq!(civil(local_day(hour, 7 * 3600)), (2026, 9, 10));
        assert_eq!(local_day(0, -3600), -1);
    }

    #[test]
    fn weekdays_and_dates_agree_with_the_calendar() {
        assert_eq!(weekday(0), 4, "1970-01-01 was a Thursday");
        assert_eq!(civil(0), (1970, 0, 1));
        assert_eq!(civil(19_723), (2024, 0, 1));
        assert_eq!(civil(19_783), (2024, 2, 1));
        assert_eq!(weekday(19_723), 1, "2024-01-01 was a Monday");
    }

    #[test]
    fn dates_name_the_year_only_when_it_differs() {
        let today = 19_723 + 100;
        assert_eq!(date_label(19_723 + 99, today), "Apr 9");
        assert_eq!(date_label(19_723 - 1, today), "Dec 31, 2023");
    }

    fn rows(hours: &[(i64, u64)]) -> Vec<Hourly> {
        hours
            .iter()
            .map(|(hour, turns)| Hourly {
                hour: *hour,
                turns: *turns,
                tokens: turns * 10,
                cost: Cost::default(),
            })
            .collect()
    }

    #[test]
    fn hours_in_one_local_day_add_up() {
        let days = day_totals(&rows(&[(1, 2), (5, 3), (30, 4)]), 0);
        assert_eq!(days.len(), 2);
        assert_eq!(
            days[&0],
            Day {
                turns: 5,
                tokens: 50
            }
        );
        assert_eq!(days[&1].turns, 4);
    }

    #[test]
    fn a_streak_runs_back_from_today_or_yesterday() {
        let days = day_totals(
            &rows(&[(24 * 7, 1), (24 * 8, 1), (24 * 9, 1), (24 * 4, 1)]),
            0,
        );
        assert_eq!(streak(&days, 9), 3, "today counts");
        assert_eq!(streak(&days, 10), 3, "a young today does not break it");
        assert_eq!(streak(&days, 11), 0, "a missed day does");
        assert_eq!(streak(&BTreeMap::new(), 5), 0);
    }

    #[test]
    fn heat_levels_spread_over_the_busy_days() {
        let mut days = BTreeMap::new();
        for (day, turns) in [(0, 1), (1, 2), (2, 3), (3, 4), (4, 100), (5, 0)] {
            days.insert(day, Day { turns, tokens: 0 });
        }
        let cuts = heat_cuts(&days);
        assert_eq!(heat_level(0, cuts), 0);
        assert_eq!(heat_level(1, cuts), 1);
        assert_eq!(heat_level(100, cuts), 4);
        let levels: Vec<usize> = [1, 2, 3, 4, 100]
            .into_iter()
            .map(|turns| heat_level(turns, cuts))
            .collect();
        assert!(
            levels.windows(2).all(|pair| pair[0] <= pair[1]),
            "{levels:?}"
        );
        assert_eq!(heat_cuts(&BTreeMap::new()), [0, 0, 0]);
        assert_eq!(heat_level(7, [0, 0, 0]), 4);
    }

    #[test]
    fn counts_are_compact() {
        assert_eq!(compact(0), "0");
        assert_eq!(compact(999), "999");
        assert_eq!(compact(1_234), "1.23K");
        assert_eq!(compact(12_345), "12.3K");
        assert_eq!(compact(123_456), "123K");
        assert_eq!(compact(5_158_946_048), "5.16B");
        assert_eq!(compact(22_180_240), "22.2M");
    }

    #[test]
    fn the_offset_reads_date_output() {
        assert_eq!(parse_offset("+0700\n"), Some(25_200));
        assert_eq!(parse_offset("-0530"), Some(-19_800));
        assert_eq!(parse_offset("+0000"), Some(0));
        assert_eq!(parse_offset("nonsense"), None);
        assert_eq!(parse_offset(""), None);
    }

    #[test]
    fn the_window_digits_pick_the_ranges() {
        assert_eq!(Range::ALL[usize::from(b'3' - b'1')], Range::Quarter);
        assert_eq!(Range::All.days(), None);
        assert_eq!(Range::Week.days(), Some(7));
    }

    #[test]
    fn calendar_arithmetic_runs_both_ways() {
        assert_eq!(days_from_civil(1970, 0, 1), 0);
        assert_eq!(days_from_civil(2024, 0, 1), 19_723);
        assert_eq!(days_from_civil(2024, 2, 1), 19_783);
        for day in [-800, -1, 0, 59, 60, 365, 19_723, 20_380] {
            let (year, month, date) = civil(day);
            assert_eq!(days_from_civil(year, month, date), day);
        }
    }

    #[test]
    fn the_last_months_end_with_this_one() {
        // 2026-10-19 is day 20_380.
        let today = days_from_civil(2026, 9, 19);
        assert_eq!(last_months(today, 3), vec![(2026, 7), (2026, 8), (2026, 9)]);
        let january = days_from_civil(2027, 0, 5);
        assert_eq!(
            last_months(january, 3),
            vec![(2026, 10), (2026, 11), (2027, 0)]
        );
    }

    #[test]
    fn hourly_cost_groups_into_local_months() {
        let at = |day: i64, micros| Hourly {
            hour: day * 24,
            turns: 1,
            tokens: 1,
            cost: Cost {
                micros,
                unpriced: 0,
            },
        };
        let october = days_from_civil(2026, 9, 31);
        let november = days_from_civil(2026, 10, 1);
        let months = month_costs(&[at(october, 5), at(october - 1, 7), at(november, 11)], 0);
        assert_eq!(months[&(2026, 9)].micros, 12);
        assert_eq!(months[&(2026, 10)].micros, 11);
        // At UTC+8 the last hour of October already belongs to November.
        let shifted = month_costs(
            &[Hourly {
                hour: october * 24 + 20,
                ..at(0, 3)
            }],
            8 * 3600,
        );
        assert_eq!(shifted[&(2026, 10)].micros, 3);
    }

    #[test]
    fn money_reads_at_a_glance() {
        assert_eq!(money(0), "$0.00");
        assert_eq!(money(400), "<$0.01");
        assert_eq!(money(12_340_000), "$12.34");
        assert_eq!(money(99_994_000), "$99.99");
        assert_eq!(
            money(99_995_000),
            "$100",
            "rounds up into the whole-dollar form"
        );
        assert_eq!(money(1_234_560_000), "$1,235");
        assert_eq!(money(5_912_300_000), "$5,912");
        assert_eq!(money(1_000_000_000_000), "$1,000,000");
    }
}
