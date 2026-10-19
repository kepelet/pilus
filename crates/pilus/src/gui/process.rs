//! The native Process page: what pilus itself is doing right now. CPU and
//! memory over the last two minutes, how busy each kind of thread is, how often
//! the window loop, the painter and the sipil poller wake, and the network calls
//! pilus made (sipil, SSH, GitHub, downloads) and how they ended.
//!
//! Nothing here is stored or sent. The page samples the operating system
//! (`pilus_client::process`) and the app's own counters (`perf`, `netlog`) once a
//! second while it is open, and not at all when it is not, so watching the cost
//! is the only cost of watching. The first half holds the pure rules (rates,
//! thread grouping, number labels), unit-tested without GPUI; the second half
//! holds the page's state and renders.

use std::collections::{HashMap, VecDeque};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use gpui::{Context, Div, FontWeight, KeyDownEvent, ScrollHandle, div, prelude::*, px};
use pilus_client::netlog::{self, Kind};
use pilus_client::process::{self, Thread, Usage};

use super::perf::{self, Count};
use super::style::{self, ButtonKind, RadiusExt};
use super::{PilusView, activity, icon};

/// Samples the charts keep: two minutes at one a second.
const HISTORY: usize = 120;
/// How often the page samples while it is in front.
const SAMPLE_EVERY: Duration = Duration::from_secs(1);
/// Thread groups listed.
const THREAD_ROWS: usize = 10;
/// Network calls listed.
const CALL_ROWS: usize = 24;
/// How far back the network block counts calls "lately".
const LATELY: Duration = Duration::from_secs(60);
/// Height of a chart, before the interface size scales it.
const CHART_HEIGHT: f32 = 56.0;

static LAUNCHED: OnceLock<Instant> = OnceLock::new();

/// Remember when pilus started, for the uptime. Call once at launch.
pub(super) fn mark_launch() {
    LAUNCHED.get_or_init(Instant::now);
}

// -- Pure rules ----------------------------------------------------------

/// One sample of the charts.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Point {
    /// Percent of one core.
    cpu: f32,
    resident: u64,
}

/// What the app counted, at one moment.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Counts {
    ticks: u64,
    repaints: u64,
    renders: u64,
    frames: u64,
    wakes: u64,
    /// Network calls by [`Kind`], in [`Kind::ALL`] order.
    calls: [u64; 4],
}

impl Counts {
    fn read() -> Self {
        let mut calls = [0; 4];
        for (slot, kind) in calls.iter_mut().zip(Kind::ALL) {
            *slot = netlog::total(kind);
        }
        Self {
            ticks: perf::total(Count::Tick),
            repaints: perf::total(Count::RepaintState) + perf::total(Count::RepaintMotion),
            renders: perf::total(Count::Render),
            frames: perf::total(Count::Frame),
            wakes: perf::total(Count::SipilWake),
            calls,
        }
    }
}

/// How often each thing happened per second over the last interval.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Rates {
    ticks: f32,
    repaints: f32,
    renders: f32,
    frames: f32,
    wakes: f32,
    calls: [f32; 4],
}

fn per_second(before: u64, after: u64, elapsed: Duration) -> f32 {
    if elapsed.is_zero() {
        return 0.0;
    }
    // Counts over a second or two are far below f32's exact range.
    #[allow(clippy::cast_precision_loss)]
    let events = after.saturating_sub(before) as f64;
    (events / elapsed.as_secs_f64()) as f32
}

fn rates(before: Counts, after: Counts, elapsed: Duration) -> Rates {
    let mut calls = [0.0; 4];
    for (at, rate) in calls.iter_mut().enumerate() {
        *rate = per_second(before.calls[at], after.calls[at], elapsed);
    }
    Rates {
        ticks: per_second(before.ticks, after.ticks, elapsed),
        repaints: per_second(before.repaints, after.repaints, elapsed),
        renders: per_second(before.renders, after.renders, elapsed),
        frames: per_second(before.frames, after.frames, elapsed),
        wakes: per_second(before.wakes, after.wakes, elapsed),
        calls,
    }
}

/// Threads of one name, taken together.
#[derive(Clone, Debug, PartialEq)]
struct Group {
    name: String,
    role: &'static str,
    threads: usize,
    running: usize,
    /// Percent of one core over the last interval.
    cpu: f32,
    /// CPU time since the threads started.
    total: Duration,
}

/// What a thread name means, for the ones pilus starts and the system's own.
fn role(name: &str) -> &'static str {
    match name {
        "main" => "Windows, input and painting",
        "worker" => "Runtime and renderer workers (unnamed)",
        "pilus-sipil" => "Sipil poller: daemons, contacts, presence",
        "pilus-sipil-net" => "Sipil network: iroh",
        "pilus-reader" => "Reads the daemon's frames",
        "pilus-writer" => "Writes to the daemon",
        "pilus-coalesce" => "Joins bursts of daemon frames",
        "pilus-reconcile" => "Polls agents, tabs and the status bar",
        "pilus-commands" => "Runs tab and pane commands",
        "pilus-events" => "Listens to the daemon's events",
        "pilus-perf" => "The PILUS_PERF counters",
        "pilus-remote-access" => "Watches the remote-access service",
        "pilus-ssh-stderr" => "Reads an SSH command's errors",
        "pilus-control-timeout" => "Times out a control request",
        "CVDisplayLink" => "Display refresh",
        "com.apple.NSEventThread" => "macOS input events",
        _ => "",
    }
}

/// Group `threads` by name and work out each group's CPU over `elapsed`, given
/// each thread's CPU time at the previous sample. The oldest unnamed thread is
/// the main one (thread ids only grow); the rest are workers. Busiest first.
fn group_threads(
    previous: &HashMap<u64, Duration>,
    threads: &[Thread],
    elapsed: Duration,
) -> Vec<Group> {
    let main = threads.iter().map(|thread| thread.id).min();
    let mut groups: Vec<Group> = Vec::new();
    for thread in threads {
        let name = if !thread.name.is_empty() {
            thread.name.clone()
        } else if Some(thread.id) == main {
            "main".to_string()
        } else {
            "worker".to_string()
        };
        let used = previous.get(&thread.id).copied().unwrap_or(thread.cpu);
        let cpu = process::percent(used, thread.cpu, elapsed);
        let at = match groups.iter().position(|group| group.name == name) {
            Some(at) => at,
            None => {
                groups.push(Group {
                    role: role(&name),
                    name,
                    threads: 0,
                    running: 0,
                    cpu: 0.0,
                    total: Duration::ZERO,
                });
                groups.len() - 1
            }
        };
        let group = &mut groups[at];
        group.threads += 1;
        group.running += usize::from(thread.running);
        group.cpu += cpu;
        group.total += thread.cpu;
    }
    groups.sort_by(|a, b| {
        b.cpu
            .total_cmp(&a.cpu)
            .then(b.total.cmp(&a.total))
            .then_with(|| a.name.cmp(&b.name))
    });
    groups
}

/// Fold neighbouring calls that are the same thing (same kind, host, call and
/// outcome) into one, newest first, with how many there were. A feed that polls a
/// host every few seconds is one line, not a page.
fn collapse(entries: Vec<netlog::Entry>) -> Vec<(netlog::Entry, usize)> {
    let mut rows: Vec<(netlog::Entry, usize)> = Vec::new();
    for entry in entries {
        match rows.last_mut() {
            Some((last, count))
                if last.kind == entry.kind
                    && last.target == entry.target
                    && last.what == entry.what
                    && last.ok == entry.ok
                    && last.took.is_some() == entry.took.is_some() =>
            {
                *count += 1;
            }
            _ => rows.push((entry, 1)),
        }
    }
    rows
}

/// Bytes as the Activity Monitor would say them.
fn memory(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    // Byte counts are far below f64's exact range, and this is for display.
    #[allow(clippy::cast_precision_loss)]
    let mb = bytes as f64 / MB;
    if mb >= 1024.0 {
        format!("{:.2} GB", mb / 1024.0)
    } else if mb >= 100.0 {
        format!("{mb:.0} MB")
    } else {
        format!("{mb:.1} MB")
    }
}

/// A duration as `1h 02m`, `4m 07s` or `12s`.
fn uptime(span: Duration) -> String {
    let secs = span.as_secs();
    match (secs / 3600, secs / 60 % 60, secs % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m {s:02}s"),
        (h, m, _) => format!("{h}h {m:02}m"),
    }
}

/// How long a call took, in the unit that keeps three figures.
fn took(span: Option<Duration>) -> String {
    let Some(span) = span else {
        return "—".to_string();
    };
    if span < Duration::from_millis(1) {
        "<1 ms".to_string()
    } else if span < Duration::from_secs(1) {
        format!("{} ms", span.as_millis())
    } else {
        format!("{:.1} s", span.as_secs_f64())
    }
}

/// Time since `at`, as a call's age.
fn age(span: Duration) -> String {
    let secs = span.as_secs();
    if secs < 1 {
        "now".to_string()
    } else if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
    }
}

/// A rate with the precision that matters at its size.
fn rate(value: f32) -> String {
    if value >= 100.0 {
        format!("{value:.0}/s")
    } else if value >= 10.0 {
        format!("{value:.1}/s")
    } else {
        format!("{value:.2}/s")
    }
}

// -- The page ------------------------------------------------------------

/// What the previous sample saw, to difference the next against.
struct Last {
    at: Instant,
    cpu: Duration,
    threads: HashMap<u64, Duration>,
    counts: Counts,
}

/// The page's state.
pub(super) struct ProcessPage {
    open: bool,
    scroll: ScrollHandle,
    history: VecDeque<Point>,
    last: Option<Last>,
    usage: Option<Usage>,
    groups: Vec<Group>,
    rates: Rates,
    peak_cpu: f32,
    peak_resident: u64,
}

impl Default for ProcessPage {
    fn default() -> Self {
        Self {
            open: false,
            scroll: ScrollHandle::new(),
            history: VecDeque::new(),
            last: None,
            usage: None,
            groups: Vec::new(),
            rates: Rates::default(),
            peak_cpu: 0.0,
            peak_resident: 0,
        }
    }
}

impl ProcessPage {
    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    pub(super) fn close(&mut self) {
        self.open = false;
    }

    /// Fold one reading into the page. The first has nothing to difference
    /// against, so it only becomes the baseline.
    fn absorb(&mut self, at: Instant, usage: Usage, threads: &[Thread], counts: Counts) {
        if let Some(last) = &self.last {
            let elapsed = at.saturating_duration_since(last.at);
            let cpu = process::percent(last.cpu, usage.cpu, elapsed);
            self.history.push_back(Point {
                cpu,
                resident: usage.resident,
            });
            while self.history.len() > HISTORY {
                self.history.pop_front();
            }
            self.groups = group_threads(&last.threads, threads, elapsed);
            self.rates = rates(last.counts, counts, elapsed);
            self.peak_cpu = self.peak_cpu.max(cpu);
        }
        self.peak_resident = self.peak_resident.max(usage.resident);
        self.usage = Some(usage);
        self.last = Some(Last {
            at,
            cpu: usage.cpu,
            threads: threads.iter().map(|t| (t.id, t.cpu)).collect(),
            counts,
        });
    }

    /// Whether a sample is due at `now`.
    fn due(&self, now: Instant) -> bool {
        self.last.as_ref().is_none_or(|last| {
            now.saturating_duration_since(last.at) >= activity::scaled(SAMPLE_EVERY)
        })
    }
}

impl PilusView {
    /// Open the page and take its first reading.
    pub(super) fn open_process(&mut self, cx: &mut Context<Self>) {
        self.leave_settings_page();
        self.process.open = true;
        self.process_sample();
        cx.notify();
    }

    fn close_process(&mut self, cx: &mut Context<Self>) {
        self.process.close();
        cx.notify();
    }

    fn process_sample(&mut self) {
        if let Some(usage) = process::usage() {
            let threads = process::threads();
            self.process
                .absorb(Instant::now(), usage, &threads, Counts::read());
        }
    }

    /// Sample again if the page is open and a second has passed. True when the
    /// page changed.
    pub(super) fn process_tick(&mut self) -> bool {
        if !self.process.open || !self.process.due(Instant::now()) {
            return false;
        }
        self.process_sample();
        true
    }

    pub(super) fn process_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        if event.keystroke.modifiers.platform || event.keystroke.modifiers.control {
            return;
        }
        if matches!(key, "escape" | "q") {
            self.close_process(cx);
        }
    }

    pub(super) fn process_page(&mut self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let launched = *LAUNCHED.get_or_init(Instant::now);
        let subtitle = format!(
            "pid {} · up {} · sampled each second while this page is open",
            std::process::id(),
            uptime(launched.elapsed())
        );
        let bar = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .flex_shrink_0()
            .flex_wrap()
            .gap_3()
            .px_4()
            .py_3()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(fonts.title))
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme.fg)
                            .child("Process"),
                    )
                    .child(
                        div()
                            .text_size(px(fonts.secondary))
                            .text_color(theme.muted)
                            .child(subtitle),
                    ),
            )
            .child(
                style::small_button(
                    "pilus-process-close",
                    "Close",
                    ButtonKind::Ghost,
                    theme,
                    fonts.label,
                )
                .on_click(cx.listener(|this, _, _, cx| this.close_process(cx))),
            );
        let body = match self.process.usage {
            Some(_) => self.process_body(),
            None => div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child(style::empty_state(
                    theme,
                    fonts,
                    icon(
                        include_bytes!("../../assets/icons/cpu.svg"),
                        24.0,
                        theme.muted,
                    ),
                    "Not available here",
                    "This system does not report a process's CPU and memory to pilus.",
                ))
                .into_any_element(),
        };
        div()
            .flex()
            .flex_col()
            .size_full()
            .min_h(px(0.0))
            .child(bar)
            .child(body)
    }

    fn process_body(&self) -> gpui::AnyElement {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let page = &self.process;
        let usage = page.usage.unwrap_or_default();
        let now = page.history.back().copied().unwrap_or_default();
        let running: usize = page.groups.iter().map(|group| group.running).sum();
        let launched = *LAUNCHED.get_or_init(Instant::now);
        let calls: u64 = page
            .last
            .as_ref()
            .map_or(0, |last| last.counts.calls.iter().sum());
        let lately = netlog::recent(usize::MAX)
            .iter()
            .filter(|entry| entry.at.elapsed() <= LATELY)
            .count();

        let tiles = style::stat_strip(theme)
            .child(style::stat_tile(
                theme,
                fonts,
                "CPU",
                format!("{:.1}%", now.cpu),
                if now.cpu >= 50.0 {
                    theme.alert
                } else {
                    theme.accent
                },
                format!("peak {:.1}% · 100% is one core", page.peak_cpu),
            ))
            .child(style::stat_tile(
                theme,
                fonts,
                "MEMORY",
                memory(usage.resident),
                theme.panel_fg,
                format!("peak {}", memory(page.peak_resident)),
            ))
            .child(style::stat_tile(
                theme,
                fonts,
                "THREADS",
                usage.threads.to_string(),
                theme.panel_fg,
                format!("{running} running now"),
            ))
            .child(style::stat_tile(
                theme,
                fonts,
                "CPU TIME",
                uptime(usage.cpu),
                theme.panel_fg,
                format!("of {} up", uptime(launched.elapsed())),
            ))
            .child(
                style::stat_tile(
                    theme,
                    fonts,
                    "NETWORK CALLS",
                    calls.to_string(),
                    theme.panel_fg,
                    format!("{lately} in the last minute"),
                )
                .border_r_0(),
            );

        let cpu_peak = page
            .history
            .iter()
            .map(|point| point.cpu)
            .fold(10.0_f32, f32::max);
        let cpu_chart = self.process_chart(
            page.history.iter().map(|point| point.cpu / cpu_peak),
            theme.accent,
            format!("0 – {cpu_peak:.0}%"),
        );
        let low = page
            .history
            .iter()
            .map(|point| point.resident)
            .min()
            .unwrap_or(0);
        let high = page
            .history
            .iter()
            .map(|point| point.resident)
            .max()
            .unwrap_or(0);
        let span = high.saturating_sub(low).max(1);
        // Memory moves by a few megabytes against hundreds, so the chart is
        // drawn between the lowest and highest reading, not from zero.
        #[allow(clippy::cast_precision_loss)]
        let memory_chart = self.process_chart(
            page.history
                .iter()
                .map(|point| 0.08 + 0.92 * (point.resident - low) as f32 / span as f32),
            theme.ok,
            format!("{} – {}", memory(low), memory(high)),
        );

        let column = div()
            .flex()
            .flex_col()
            .gap_4()
            .w_full()
            .max_w(px(style::scaled(&self.shared_settings, 1040.0)))
            .pb_6()
            .child(tiles)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap_4()
                    .px_4()
                    .child(self.process_block("CPU · LAST 2 MINUTES", cpu_chart))
                    .child(self.process_block("MEMORY · LAST 2 MINUTES", memory_chart)),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap_4()
                    .px_4()
                    .child(self.process_block("THREADS", self.process_threads()))
                    .child(self.process_block("WAKE-UPS", self.process_activity())),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .px_4()
                    .child(self.process_block("NETWORK", self.process_network())),
            );

        div()
            .id("pilus-process-scroll")
            .flex()
            .flex_col()
            .items_center()
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .track_scroll(&self.process.scroll)
            .child(column)
            .into_any_element()
    }

    /// A titled block, in the frame the Stats page uses.
    fn process_block(&self, title: &'static str, body: impl IntoElement) -> Div {
        self.stats_block(title, body)
    }

    /// Bars for `levels` (each 0..=1), oldest on the left, with a caption for
    /// the range they span.
    fn process_chart(
        &self,
        levels: impl Iterator<Item = f32>,
        color: gpui::Rgba,
        caption: String,
    ) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let height = style::scaled(&self.shared_settings, CHART_HEIGHT);
        let levels: Vec<f32> = levels.collect();
        let missing = HISTORY.saturating_sub(levels.len());
        let mut bars = div()
            .flex()
            .flex_row()
            .items_end()
            .gap(px(1.0))
            .h(px(height))
            .w_full();
        // Every reading has a slot of the same width, so the chart spans the
        // block from the start; a page just opened has few readings, and the
        // slots for the rest are left empty, so it fills from the right, as a
        // live one does.
        for _ in 0..missing {
            bars = bars.child(div().flex_1().min_w(px(0.0)).h_full());
        }
        for level in levels {
            bars = bars.child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .h_full()
                    .flex()
                    .items_end()
                    .child(
                        div()
                            .w_full()
                            .h(px((height * level.clamp(0.0, 1.0)).max(1.0)))
                            .bg(color)
                            .opacity(0.85),
                    ),
            );
        }
        div().flex().flex_col().gap_2().child(bars).child(
            div()
                .text_size(px(fonts.secondary))
                .text_color(theme.muted)
                .child(caption),
        )
    }

    fn process_threads(&self) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let groups = &self.process.groups;
        let lead = groups.first().map_or(1.0, |group| group.cpu.max(1.0));
        let mut list = div().flex().flex_col().gap_3();
        for (at, group) in groups.iter().take(THREAD_ROWS).enumerate() {
            let many = if group.threads > 1 {
                format!(" ×{}", group.threads)
            } else {
                String::new()
            };
            let role = if group.role.is_empty() {
                String::new()
            } else {
                format!(" · {}", group.role)
            };
            list = list.child(style::rank_row(
                format!("pilus-process-thread-{at}"),
                theme,
                fonts,
                format!("{}{many}", group.name),
                format!("{:.1}% · {} total{role}", group.cpu, uptime(group.total)),
                (group.cpu / lead).clamp(0.0, 1.0),
            ));
        }
        if groups.len() > THREAD_ROWS {
            list = list.child(
                div()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(format!(
                        "and {} more kinds of thread",
                        groups.len() - THREAD_ROWS
                    )),
            );
        }
        list
    }

    fn process_activity(&self) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let rates = self.process.rates;
        let line = |label: &'static str, hint: &'static str, value: f32| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .justify_between()
                        .gap_3()
                        .text_size(px(fonts.body))
                        .child(div().text_color(theme.panel_fg).child(label))
                        .child(div().text_color(theme.panel_fg).child(rate(value))),
                )
                .child(
                    div()
                        .text_size(px(fonts.secondary))
                        .text_color(theme.muted)
                        .child(hint),
                )
        };
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(line(
                "Window loop",
                "Polls state; speeds up only while something animates",
                rates.ticks,
            ))
            .child(line(
                "Repaints",
                "Frames drawn because state or an animation changed",
                rates.repaints,
            ))
            .child(line(
                "Renders",
                "Window renders, however triggered",
                rates.renders,
            ))
            .child(line(
                "Daemon frames",
                "Screen updates from the Luvus daemon",
                rates.frames,
            ))
            .child(line(
                "Sipil poller",
                "Wakes for daemons, contacts and presence",
                rates.wakes,
            ))
    }

    fn process_network(&self) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let counts = self.process.last.as_ref().map(|last| last.counts.calls);
        let mut kinds = div().flex().flex_row().flex_wrap().gap_x_4().gap_y_1();
        for (at, kind) in Kind::ALL.into_iter().enumerate() {
            let total = counts.map_or(0, |calls| calls[at]);
            kinds = kinds.child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .text_size(px(fonts.body))
                    .child(div().text_color(theme.panel_fg).child(kind.label()))
                    .child(
                        div()
                            .text_color(theme.muted)
                            .child(format!("{total} · {}", rate(self.process.rates.calls[at]))),
                    ),
            );
        }

        let cell = |width: Option<f32>, last: bool| {
            style::table_cell(&self.shared_settings, theme, width, last)
        };
        let head = |text: &'static str, width: Option<f32>, last: bool| {
            cell(width, last)
                .overflow_hidden()
                .text_size(px(fonts.label))
                .font_weight(FontWeight::BOLD)
                .text_color(theme.muted)
                .child(text)
        };
        let row_height = style::scaled(&self.shared_settings, 26.0);
        let mut table = div()
            .flex()
            .flex_col()
            .border_1()
            .border_color(theme.border)
            .radius_md()
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .h(px(row_height))
                    .border_b_1()
                    .border_color(theme.border)
                    .child(head("WHEN", Some(80.0), false))
                    .child(head("KIND", Some(72.0), false))
                    .child(head("TO", Some(150.0), false))
                    .child(head("CALL", None, false))
                    .child(head("TOOK", Some(80.0), false))
                    .child(head("RESULT", Some(80.0), true)),
            );
        let entries: Vec<(netlog::Entry, usize)> = collapse(netlog::recent(CALL_ROWS * 8))
            .into_iter()
            .take(CALL_ROWS)
            .collect();
        if entries.is_empty() {
            table = table.child(
                div()
                    .p_3()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child("Nothing has gone out yet."),
            );
        }
        for (at, (entry, repeats)) in entries.iter().enumerate() {
            let result_color = if entry.ok { theme.ok } else { theme.alert };
            let text = |value: String, color| {
                div()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(px(fonts.body))
                    .text_color(color)
                    .child(value)
            };
            table = table.child(
                div()
                    .id(("pilus-process-call", at))
                    .flex()
                    .flex_row()
                    .h(px(row_height))
                    .when(at > 0, |row| row.border_t_1().border_color(theme.border))
                    .child(
                        cell(Some(80.0), false).child(text(age(entry.at.elapsed()), theme.muted)),
                    )
                    .child(
                        cell(Some(72.0), false)
                            .child(text(entry.kind.label().to_string(), theme.panel_fg)),
                    )
                    .child(
                        cell(Some(150.0), false).child(text(entry.target.clone(), theme.panel_fg)),
                    )
                    .child(cell(None, false).child(text(
                        if *repeats > 1 {
                            format!("{}  ×{repeats}", entry.what)
                        } else {
                            entry.what.clone()
                        },
                        theme.panel_fg,
                    )))
                    .child(cell(Some(80.0), false).child(text(took(entry.took), theme.muted)))
                    .child(cell(Some(80.0), true).child(text(
                        if entry.took.is_none() {
                            "started".to_string()
                        } else if entry.ok {
                            "ok".to_string()
                        } else {
                            "failed".to_string()
                        },
                        result_color,
                    ))),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(kinds)
            .child(table)
            .child(
                div()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(
                        "Calls out of this machine only: the daemon's own socket is local. \
                         SSH is noted when a command is started.",
                    ),
            )
    }
}

/// Shot-harness entry point: the page over canned figures. It reads no counters,
/// and the calls it lists are made up.
#[cfg(feature = "visual-test")]
impl PilusView {
    pub(super) fn process_scroll_to_end(&mut self, cx: &mut Context<Self>) {
        self.process
            .scroll
            .set_offset(gpui::point(px(0.0), px(-100_000.0)));
        cx.notify();
    }

    pub(super) fn process_show_sample(&mut self, cx: &mut Context<Self>) {
        self.leave_settings_page();
        let start = Instant::now();
        let mut page = ProcessPage {
            open: true,
            ..ProcessPage::default()
        };
        // A believable two minutes: a quiet idle with a burst of work in the
        // middle, and memory creeping up.
        for second in 0..HISTORY {
            let burst = (40..58).contains(&second);
            let wobble = (second * 7919 % 13) as f32 / 10.0;
            page.history.push_back(Point {
                cpu: if burst {
                    38.0 + wobble * 6.0
                } else {
                    0.6 + wobble * 0.5
                },
                resident: (212 << 20)
                    + (second as u64) * (96 << 10)
                    + if burst { 6 << 20 } else { 0 },
            });
        }
        page.peak_cpu = 47.2;
        page.peak_resident = 231 << 20;
        page.usage = Some(Usage {
            resident: 224 << 20,
            virtual_size: 412 << 30,
            cpu: Duration::from_secs(1843),
            threads: 31,
        });
        let group = |name: &str, threads, running, cpu, secs| Group {
            name: name.to_string(),
            role: role(name),
            threads,
            running,
            cpu,
            total: Duration::from_secs(secs),
        };
        page.groups = vec![
            group("main", 1, 1, 3.1, 912),
            group("worker", 8, 0, 1.4, 388),
            group("pilus-sipil-net", 2, 0, 0.6, 61),
            group("pilus-reader", 1, 0, 0.4, 44),
            group("CVDisplayLink", 1, 0, 0.3, 120),
            group("pilus-reconcile", 2, 0, 0.2, 18),
            group("pilus-sipil", 1, 0, 0.1, 9),
            group("pilus-writer", 1, 0, 0.0, 2),
        ];
        page.rates = Rates {
            ticks: 19.4,
            repaints: 0.8,
            renders: 2.1,
            frames: 6.5,
            wakes: 1.0,
            calls: [1.1, 0.4, 0.0, 0.0],
        };
        let mut counts = Counts::default();
        counts.calls = [1_284, 96, 14, 1];
        page.last = Some(Last {
            at: start,
            cpu: Duration::ZERO,
            threads: HashMap::new(),
            counts,
        });
        self.process = page;
        for (kind, target, what, ok) in [
            (Kind::Sipil, "6d0f41ce", "agents", true),
            (Kind::Sipil, "6d0f41ce", "sessions", true),
            (Kind::Ssh, "buildbox", "shell", true),
            (Kind::Sipil, "c9ab05a2", "health", false),
            (Kind::Github, "local", "api user", true),
            (Kind::Sipil, "6d0f41ce", "transcript", true),
            (Kind::Http, "luvus.dev", "update check", true),
        ] {
            netlog::record(kind, target, what, Instant::now(), ok);
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(id: u64, name: &str, cpu_ms: u64, running: bool) -> Thread {
        Thread {
            id,
            name: name.to_string(),
            cpu: Duration::from_millis(cpu_ms),
            running,
        }
    }

    fn usage(resident: u64, cpu_ms: u64) -> Usage {
        Usage {
            resident,
            virtual_size: 0,
            cpu: Duration::from_millis(cpu_ms),
            threads: 3,
        }
    }

    #[test]
    fn threads_of_one_name_are_one_group_and_the_busiest_group_leads() {
        let before: HashMap<u64, Duration> = [
            (1, Duration::from_millis(1000)),
            (2, Duration::from_millis(100)),
            (3, Duration::from_millis(100)),
            (4, Duration::from_millis(0)),
        ]
        .into();
        let now = [
            thread(1, "", 1050, false),
            thread(2, "pilus-sipil-net", 250, true),
            thread(3, "pilus-sipil-net", 300, false),
            thread(4, "", 20, false),
        ];
        let groups = group_threads(&before, &now, Duration::from_secs(1));
        let names: Vec<&str> = groups.iter().map(|group| group.name.as_str()).collect();
        assert_eq!(names, ["pilus-sipil-net", "main", "worker"]);
        let net = &groups[0];
        assert_eq!((net.threads, net.running), (2, 1));
        // 150 ms + 200 ms in one second is 35% of a core.
        assert!((net.cpu - 35.0).abs() < 0.01, "{}", net.cpu);
        assert_eq!(net.role, "Sipil network: iroh");
        assert!((groups[1].cpu - 5.0).abs() < 0.01);
        assert_eq!(groups[1].role, "Windows, input and painting");
    }

    #[test]
    fn a_thread_that_appeared_since_the_last_sample_starts_from_nothing() {
        let groups = group_threads(
            &HashMap::new(),
            &[thread(9, "pilus-new", 400, true)],
            Duration::from_secs(1),
        );
        assert_eq!(groups[0].cpu, 0.0);
        assert_eq!(groups[0].total, Duration::from_millis(400));
    }

    #[test]
    fn counts_become_rates_over_the_interval() {
        let before = Counts {
            ticks: 100,
            wakes: 10,
            calls: [4, 0, 0, 0],
            ..Counts::default()
        };
        let after = Counts {
            ticks: 140,
            wakes: 11,
            calls: [6, 0, 0, 0],
            ..Counts::default()
        };
        let got = rates(before, after, Duration::from_secs(2));
        assert_eq!((got.ticks, got.wakes, got.calls[0]), (20.0, 0.5, 1.0));
        assert_eq!(rates(before, after, Duration::ZERO), Rates::default());
        assert_eq!(
            rates(after, before, Duration::from_secs(1)),
            Rates::default()
        );
    }

    #[test]
    fn the_first_reading_is_a_baseline_and_the_second_gives_cpu_and_history() {
        let mut page = ProcessPage::default();
        let start = Instant::now();
        page.absorb(start, usage(100 << 20, 1000), &[], Counts::default());
        assert!(page.history.is_empty(), "nothing to difference yet");
        assert_eq!(page.peak_resident, 100 << 20);
        page.absorb(
            start + Duration::from_secs(1),
            usage(120 << 20, 1250),
            &[],
            Counts::default(),
        );
        let point = page.history.back().copied().unwrap_or_default();
        assert!((point.cpu - 25.0).abs() < 0.01, "{}", point.cpu);
        assert_eq!(point.resident, 120 << 20);
        assert_eq!(page.peak_resident, 120 << 20);
        assert!((page.peak_cpu - 25.0).abs() < 0.01);
    }

    #[test]
    fn the_history_keeps_two_minutes() {
        let mut page = ProcessPage::default();
        let start = Instant::now();
        for second in 0..(HISTORY as u64 + 30) {
            page.absorb(
                start + Duration::from_secs(second),
                usage(1 << 20, second * 10),
                &[],
                Counts::default(),
            );
        }
        assert_eq!(page.history.len(), HISTORY);
    }

    #[test]
    fn a_run_of_the_same_call_is_one_row_with_its_count() {
        let call = |kind, target: &str, what: &str, ok| netlog::Entry {
            at: Instant::now(),
            kind,
            target: target.to_string(),
            what: what.to_string(),
            took: None,
            ok,
        };
        let rows = collapse(vec![
            call(Kind::Ssh, "host", "uhp", true),
            call(Kind::Ssh, "host", "uhp", true),
            call(Kind::Ssh, "host", "uhp", true),
            call(Kind::Sipil, "ab12", "health", true),
            call(Kind::Sipil, "ab12", "health", false),
            call(Kind::Ssh, "host", "uhp", true),
        ]);
        let shape: Vec<(&str, usize)> = rows
            .iter()
            .map(|(entry, count)| (entry.what.as_str(), *count))
            .collect();
        assert_eq!(
            shape,
            [("uhp", 3), ("health", 1), ("health", 1), ("uhp", 1)]
        );
    }

    #[test]
    fn labels_read_at_a_glance() {
        assert_eq!(memory(12 << 20), "12.0 MB");
        assert_eq!(memory(250 << 20), "250 MB");
        assert_eq!(memory(3 << 30), "3.00 GB");
        assert_eq!(uptime(Duration::from_secs(12)), "12s");
        assert_eq!(uptime(Duration::from_secs(247)), "4m 07s");
        assert_eq!(uptime(Duration::from_secs(3720)), "1h 02m");
        assert_eq!(took(None), "—");
        assert_eq!(took(Some(Duration::from_micros(200))), "<1 ms");
        assert_eq!(took(Some(Duration::from_millis(42))), "42 ms");
        assert_eq!(took(Some(Duration::from_millis(2500))), "2.5 s");
        assert_eq!(age(Duration::from_millis(300)), "now");
        assert_eq!(age(Duration::from_secs(5)), "5s ago");
        assert_eq!(age(Duration::from_secs(125)), "2m ago");
        assert_eq!(rate(0.333), "0.33/s");
        assert_eq!(rate(19.4), "19.4/s");
        assert_eq!(rate(120.0), "120/s");
    }

    #[test]
    fn it_samples_once_a_second_in_front_and_less_behind() {
        let mut page = ProcessPage::default();
        let start = Instant::now();
        assert!(page.due(start), "never sampled");
        page.absorb(start, usage(1, 1), &[], Counts::default());
        assert!(!page.due(start + Duration::from_millis(400)));
        assert!(page.due(start + Duration::from_millis(1100)));
    }
}
