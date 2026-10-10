//! The native Orchestration page: the task queue as a board with its
//! dependency lines, a bird's-eye view of what is waiting on what.
//!
//! It is a second layout of the Tasks page, not a second copy of it: the
//! tasks, the machine and workspace scope, the selection, the composer, and
//! the detail pane are all `TasksPanel`'s, so switching between the list and
//! the board keeps the user's place. This module holds the board's pure
//! placement rules (lanes, card slots, links, arrow-key moves), which stay
//! unit-testable without GPUI, and the rendering that paints them.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use gpui::{
    AnyElement, Context, Div, FontWeight, Hsla, SharedString, Stateful, div, prelude::*, px,
};
use pilus_client::{Agent, Task, TaskStatus};

use super::style::{self, Connector, RadiusExt, Side};
use super::tasks::{TaskLayout, TaskView, age_label, status_glyph, unix_now};
use super::{PilusView, agent_icon, icon};

/// A column of the board. Several statuses share one, so the board reads as
/// a handful of stages rather than the daemon's full state machine; each card
/// still carries its exact status glyph.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum Lane {
    Queued,
    Working,
    Review,
    Done,
    Failed,
}

impl Lane {
    /// Every lane, left to right.
    const ALL: [Lane; 5] = [
        Lane::Queued,
        Lane::Working,
        Lane::Review,
        Lane::Done,
        Lane::Failed,
    ];

    pub(super) fn of(status: TaskStatus) -> Self {
        match status {
            TaskStatus::Queued => Self::Queued,
            TaskStatus::Claimed | TaskStatus::Running => Self::Working,
            TaskStatus::Review | TaskStatus::Blocked => Self::Review,
            TaskStatus::Done | TaskStatus::Merging | TaskStatus::Merged => Self::Done,
            TaskStatus::Failed => Self::Failed,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Queued => "QUEUED",
            Self::Working => "WORKING",
            Self::Review => "REVIEW",
            Self::Done => "DONE",
            Self::Failed => "FAILED",
        }
    }

    /// The status whose glyph heads the lane.
    fn glyph(self) -> TaskStatus {
        match self {
            Self::Queued => TaskStatus::Queued,
            Self::Working => TaskStatus::Running,
            Self::Review => TaskStatus::Review,
            Self::Done => TaskStatus::Done,
            Self::Failed => TaskStatus::Failed,
        }
    }

    /// Whether the board draws this lane under `view` while it holds
    /// `count` cards. The working stages stay up when empty so the board
    /// keeps its shape; Failed appears only when something failed.
    fn shown(self, view: TaskView, count: usize) -> bool {
        match self {
            Self::Failed => count > 0 && view != TaskView::Active,
            Self::Done => view != TaskView::Active,
            Self::Queued | Self::Working | Self::Review => view != TaskView::Closed,
        }
    }
}

/// A dependency drawn between two cards: `from` must merge before `to` can
/// run. Slots are `(lane index, row)` on the board.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Link {
    pub(super) from: (usize, usize),
    pub(super) to: (usize, usize),
    pub(super) from_id: String,
    pub(super) to_id: String,
    /// The dependency already merged: the line is history, not a hold.
    pub(super) settled: bool,
}

/// The board's cards, placed in lanes.
#[derive(Debug)]
pub(super) struct Board<'a> {
    pub(super) lanes: Vec<(Lane, Vec<&'a Task>)>,
}

impl<'a> Board<'a> {
    /// Place the tasks `view` admits. Within a lane, a task sits below the
    /// tasks it depends on, so chains read top to bottom and their lines
    /// stay short.
    pub(super) fn build(tasks: &'a [Task], view: TaskView) -> Self {
        let depth = depths(tasks);
        let lanes = Lane::ALL
            .into_iter()
            .filter_map(|lane| {
                let mut cards: Vec<&Task> = tasks
                    .iter()
                    .filter(|task| Lane::of(task.status()) == lane && view.admits(task))
                    .collect();
                cards.sort_by_key(|task| {
                    (
                        depth.get(task.id.as_str()).copied().unwrap_or(0),
                        id_number(&task.id),
                        task.id.clone(),
                    )
                });
                lane.shown(view, cards.len()).then_some((lane, cards))
            })
            .collect();
        Self { lanes }
    }

    pub(super) fn slot(&self, id: &str) -> Option<(usize, usize)> {
        self.lanes
            .iter()
            .enumerate()
            .find_map(|(lane, (_, cards))| {
                cards
                    .iter()
                    .position(|task| task.id == id)
                    .map(|row| (lane, row))
            })
    }

    /// Every dependency whose both ends are on the board.
    pub(super) fn links(&self) -> Vec<Link> {
        let mut links = Vec::new();
        for (lane, (_, cards)) in self.lanes.iter().enumerate() {
            for (row, task) in cards.iter().enumerate() {
                for dep in &task.deps {
                    let Some(from) = self.slot(dep) else {
                        continue;
                    };
                    let settled = self.lanes[from.0].1[from.1].status() == TaskStatus::Merged;
                    links.push(Link {
                        from,
                        to: (lane, row),
                        from_id: dep.clone(),
                        to_id: task.id.clone(),
                        settled,
                    });
                }
            }
        }
        links
    }

    /// The card an arrow key moves to from `current`: `dy` within the lane,
    /// `dx` across lanes (skipping empty ones) at the nearest row. With
    /// nothing selected, any key picks the first card.
    pub(super) fn step(&self, current: Option<&str>, dx: isize, dy: isize) -> Option<String> {
        let first = || {
            self.lanes
                .iter()
                .find_map(|(_, cards)| cards.first())
                .map(|task| task.id.clone())
        };
        let Some((lane, row)) = current.and_then(|id| self.slot(id)) else {
            return first();
        };
        if dx == 0 {
            let cards = &self.lanes[lane].1;
            let next = (row as isize + dy).clamp(0, cards.len() as isize - 1) as usize;
            return Some(cards[next].id.clone());
        }
        let mut at = lane as isize;
        loop {
            at += dx.signum();
            let Some((_, cards)) = usize::try_from(at).ok().and_then(|at| self.lanes.get(at))
            else {
                return Some(self.lanes[lane].1[row].id.clone());
            };
            if let Some(task) = cards.get(row.min(cards.len().saturating_sub(1))) {
                return Some(task.id.clone());
            }
        }
    }
}

/// Each task's longest dependency chain among `tasks`. A cycle, which the
/// daemon should refuse but a hand-edited queue could hold, ends the chain
/// instead of recursing forever.
fn depths(tasks: &[Task]) -> HashMap<&str, usize> {
    fn visit<'a>(
        id: &'a str,
        by_id: &HashMap<&'a str, &'a Task>,
        depth: &mut HashMap<&'a str, usize>,
        visiting: &mut HashSet<&'a str>,
    ) -> usize {
        if let Some(known) = depth.get(id) {
            return *known;
        }
        let Some(task) = by_id.get(id) else {
            return 0;
        };
        if !visiting.insert(id) {
            return 0;
        }
        let deepest = task
            .deps
            .iter()
            .filter_map(|dep| by_id.get_key_value(dep.as_str()).map(|(key, _)| *key))
            .map(|dep| visit(dep, by_id, depth, visiting) + 1)
            .max()
            .unwrap_or(0);
        visiting.remove(id);
        depth.insert(id, deepest);
        deepest
    }
    let by_id: HashMap<&str, &Task> = tasks.iter().map(|task| (task.id.as_str(), task)).collect();
    let mut depth = HashMap::new();
    let mut visiting = HashSet::new();
    for task in tasks {
        visit(task.id.as_str(), &by_id, &mut depth, &mut visiting);
    }
    depth
}

/// `t12` sorts after `t9`: the numeric part of a sequential id.
fn id_number(id: &str) -> u64 {
    id.trim_start_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .unwrap_or(u64::MAX)
}

/// The dependencies of `task` that have not merged yet, among `known`. An
/// id the machine no longer lists cannot be waited on, so it is left out.
pub(super) fn waiting_on<'a>(task: &'a Task, known: &[Task]) -> Vec<&'a str> {
    task.deps
        .iter()
        .filter(|dep| {
            known
                .iter()
                .any(|other| other.id == **dep && other.status() != TaskStatus::Merged)
        })
        .map(String::as_str)
        .collect()
}

/// The board's measurements, in pixels at the user's Interface size.
#[derive(Clone, Copy, Debug)]
struct Geometry {
    card_w: f32,
    card_h: f32,
    /// A folded card: its status line and title.
    folded_h: f32,
    row_gap: f32,
    /// Room between lanes for the lines to bend through.
    lane_gap: f32,
    lane_pad: f32,
    header_h: f32,
    /// Room between a lane's header and its first card.
    header_gap: f32,
    /// A folded lane: a strip holding its glyph, count, and name.
    strip_w: f32,
    pad: f32,
}

impl Geometry {
    fn new(scale: impl Fn(f32) -> f32) -> Self {
        Self {
            card_w: scale(248.0),
            card_h: scale(124.0),
            folded_h: scale(58.0),
            row_gap: scale(12.0),
            lane_gap: scale(48.0),
            lane_pad: scale(8.0),
            header_h: scale(36.0),
            header_gap: scale(10.0),
            strip_w: scale(40.0),
            pad: 16.0,
        }
    }

    fn lane_w(&self) -> f32 {
        self.card_w + 2.0 * self.lane_pad
    }

    fn cards_top(&self) -> f32 {
        self.pad + self.header_h + self.header_gap
    }
}

/// Each card's top edge in a lane: the cards stack from `top`, `gap` apart.
fn stack(top: f32, gap: f32, heights: &[f32]) -> Vec<f32> {
    heights
        .iter()
        .scan(top, |next, height| {
            let at = *next;
            *next += height + gap;
            Some(at)
        })
        .collect()
}

/// Every card's box on the board: lanes side by side at their own widths,
/// cards stacked at their own heights, so a folded lane or card pulls the
/// rest in.
struct Layout {
    geometry: Geometry,
    /// Per lane: left edge, width, and how far it has folded, from 0 (open)
    /// to 1 (a strip).
    columns: Vec<(f32, f32, f32)>,
    /// Per lane, each card's `(top, height)`.
    lanes: Vec<Vec<(f32, f32)>>,
}

impl Layout {
    fn new(
        geometry: Geometry,
        board: &Board<'_>,
        lane_fold: impl Fn(Lane) -> f32,
        height: impl Fn(&Task) -> f32,
    ) -> Self {
        let folds: Vec<f32> = board
            .lanes
            .iter()
            .map(|(lane, _)| lane_fold(*lane))
            .collect();
        let widths: Vec<f32> = folds
            .iter()
            .map(|fold| geometry.lane_w() + (geometry.strip_w - geometry.lane_w()) * fold)
            .collect();
        // Neighbouring folded strips stack flush; the gap exists only for the
        // lines between open lanes, so it closes as both sides fold.
        let mut next = geometry.pad;
        let lefts: Vec<f32> = widths
            .iter()
            .enumerate()
            .map(|(i, width)| {
                let flush = if i == 0 { 0.0 } else { folds[i - 1] * folds[i] };
                next += if i == 0 {
                    0.0
                } else {
                    geometry.lane_gap * (1.0 - flush)
                };
                let at = next;
                next += width;
                at
            })
            .collect();
        let columns = lefts
            .into_iter()
            .zip(widths)
            .zip(folds)
            .map(|((left, width), fold)| (left, width, fold))
            .collect();
        let lanes = board
            .lanes
            .iter()
            .map(|(_, cards)| {
                let heights: Vec<f32> = cards.iter().map(|task| height(task)).collect();
                let tops = stack(geometry.cards_top(), geometry.row_gap, &heights);
                tops.into_iter().zip(heights).collect()
            })
            .collect();
        Self {
            geometry,
            columns,
            lanes,
        }
    }

    /// A lane's `(left, width, fold)`.
    fn column(&self, lane: usize) -> (f32, f32, f32) {
        self.columns[lane]
    }

    /// A card's `(x, y, height)`.
    fn card(&self, (lane, row): (usize, usize)) -> (f32, f32, f32) {
        let (top, height) = self.lanes[lane][row];
        let (left, ..) = self.columns[lane];
        (left + self.geometry.lane_pad, top, height)
    }

    /// The board's extent: every lane, as tall as the tallest open stack.
    fn size(&self) -> (f32, f32) {
        let g = &self.geometry;
        let width = self
            .columns
            .last()
            .map_or(g.pad, |(left, width, _)| left + width)
            + g.pad;
        let bottom = self
            .lanes
            .iter()
            .zip(&self.columns)
            .filter(|(_, (.., fold))| *fold < 1.0)
            .filter_map(|(cards, _)| cards.last())
            .map(|(top, height)| top + height)
            .fold(g.cards_top() + g.card_h, f32::max);
        (width, bottom + g.lane_pad + g.pad)
    }

    /// Whether `link` has nothing to join: both ends sit in one folded lane.
    fn hides(&self, link: &Link) -> bool {
        link.from.0 == link.to.0 && self.columns[link.from.0].2 >= 1.0
    }

    /// Where `link`'s line leaves and enters. A link to a lane on the right
    /// runs right edge to left edge, one to the left mirrors that, and one
    /// within a lane bows out of both right edges. A card in a folding lane
    /// hands its end to the strip, sliding there as the lane narrows.
    fn connector(&self, link: &Link, color: Hsla) -> Connector {
        let (from_side, to_side) = match link.from.0.cmp(&link.to.0) {
            std::cmp::Ordering::Less => (Side::Right, Side::Left),
            std::cmp::Ordering::Greater => (Side::Left, Side::Right),
            std::cmp::Ordering::Equal => (Side::Right, Side::Right),
        };
        let g = &self.geometry;
        let port = |slot: (usize, usize), side: Side| {
            let (x, y, height) = self.card(slot);
            let (left, width, fold) = self.column(slot.0);
            let (card_x, strip_x) = match side {
                Side::Left => (x, left),
                Side::Right => (x + g.card_w, left + width),
            };
            let card_y = y + height / 2.0;
            let strip_y = g.cards_top() + g.folded_h / 2.0;
            (
                card_x + (strip_x - card_x) * fold,
                card_y + (strip_y - card_y) * fold,
            )
        };
        Connector {
            from: port(link.from, from_side),
            from_side,
            to: port(link.to, to_side),
            to_side,
            color,
            dashed: link.settled,
        }
    }
}

impl PilusView {
    /// Open the board from the sidebar.
    pub(super) fn open_orchestration(&mut self, cx: &mut Context<Self>) {
        self.leave_settings_page();
        self.picker.close();
        self.tasks_panel.open_as(TaskLayout::Board);
        cx.notify();
    }

    /// The agent working `task`, if its pane still runs one.
    pub(super) fn task_agent(&self, task: &Task) -> Option<&Agent> {
        let pane = task.assignee?.to_string();
        self.endpoint_agents(&self.scope_machine())
            .iter()
            .find(|agent| agent.pane == pane)
    }

    /// `task`'s workspace by name, falling back to its root's folder.
    pub(super) fn task_workspace_name(&self, task: &Task) -> Option<String> {
        let id = task.workspace_id.as_deref();
        self.endpoint_workspaces(&self.scope_machine())
            .iter()
            .find(|workspace| Some(workspace.id.as_str()) == id)
            .map(|workspace| workspace.name.clone())
            .or_else(|| {
                let root = task.project_root.as_deref()?.trim_end_matches('/');
                root.rsplit('/').next().map(str::to_string)
            })
            .filter(|name| !name.is_empty())
    }

    /// Arrow keys and hjkl move the selection across the board.
    pub(super) fn board_key(&mut self, key: &str, tasks: &[Task]) -> bool {
        let (dx, dy) = match key {
            "left" | "h" => (-1, 0),
            "right" | "l" => (1, 0),
            "up" | "k" => (0, -1),
            "down" | "j" => (0, 1),
            "space" => {
                if let Some(id) = self.tasks_panel.selected.clone() {
                    self.toggle_fold(id);
                }
                return true;
            }
            _ => return false,
        };
        let mut board = Board::build(tasks, self.tasks_panel.view);
        // A folded lane shows no cards, so arrows pass over it.
        for (lane, cards) in &mut board.lanes {
            if self.tasks_panel.folded_lanes.contains(lane) {
                cards.clear();
            }
        }
        let next = board.step(self.tasks_panel.selected.as_deref(), dx, dy);
        self.tasks_panel.confirm_delete = None;
        self.tasks_panel.selected = next;
        true
    }

    /// Fold `id` down to its title, or open it back up.
    fn toggle_fold(&mut self, id: String) {
        let panel = &mut self.tasks_panel;
        if !panel.folded.remove(&id) {
            panel.folded.insert(id.clone());
        }
        panel.fold_turned.insert(id, Instant::now());
    }

    /// Fold `lane` down to a strip, or open it back up.
    fn toggle_lane(&mut self, lane: Lane) {
        let panel = &mut self.tasks_panel;
        if !panel.folded_lanes.remove(&lane) {
            panel.folded_lanes.insert(lane);
        }
        panel.lane_turned.insert(lane, Instant::now());
    }

    /// How far `lane` has folded this frame, and whether it is still moving.
    fn lane_fold(&self, lane: Lane, cx: &Context<Self>) -> (f32, bool) {
        let folded = self.tasks_panel.folded_lanes.contains(&lane);
        let (from, to) = if folded { (0.0, 1.0) } else { (1.0, 0.0) };
        match self
            .tasks_panel
            .lane_turned
            .get(&lane)
            .and_then(|turned| style::eased(*turned, style::FOLD, cx))
        {
            Some(step) => (from + (to - from) * step, true),
            None => (to, false),
        }
    }

    /// A card's height this frame, easing toward folded or open, and
    /// whether it is still moving.
    fn card_height(&self, id: &str, geometry: &Geometry, cx: &Context<Self>) -> (f32, bool) {
        let folded = self.tasks_panel.folded.contains(id);
        let (from, to) = if folded {
            (geometry.card_h, geometry.folded_h)
        } else {
            (geometry.folded_h, geometry.card_h)
        };
        match self
            .tasks_panel
            .fold_turned
            .get(id)
            .and_then(|turned| style::eased(*turned, style::FOLD, cx))
        {
            Some(step) => (from + (to - from) * step, true),
            None => (to, false),
        }
    }

    /// Tasks still waiting on another, for the header's count.
    pub(super) fn waiting_count(&self, tasks: &[Task]) -> usize {
        let known = self.machine_tasks(&self.scope_machine());
        tasks
            .iter()
            .filter(|task| task.status().is_open() && !waiting_on(task, &known).is_empty())
            .count()
    }

    /// The board, drawn where the Tasks page draws its list.
    pub(super) fn task_board(&self, board: &Board<'_>, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = self.theme;
        let geometry = Geometry::new(|design| style::scaled(&self.shared_settings, design));
        let moving = std::cell::Cell::new(false);
        let layout = Layout::new(
            geometry,
            board,
            |lane| {
                let (fold, tweening) = self.lane_fold(lane, cx);
                moving.set(moving.get() || tweening);
                fold
            },
            |task| {
                let (height, tweening) = self.card_height(&task.id, &geometry, cx);
                moving.set(moving.get() || tweening);
                height
            },
        );
        let (width, height) = layout.size();
        // Dependencies can live outside the scoped workspace or the view, so
        // waiting is judged against the whole machine.
        let known = self.machine_tasks(&self.scope_machine());
        let selected = self.tasks_panel.selected.as_deref();

        let mut canvas = div().relative().w(px(width)).h(px(height)).min_h_full();
        for (index, (lane, cards)) in board.lanes.iter().enumerate() {
            canvas = canvas.child(self.board_lane(index, *lane, cards.len(), &layout, cx));
        }

        let links: Vec<Link> = board
            .links()
            .into_iter()
            .filter(|link| !layout.hides(link))
            .collect();
        let muted = Hsla::from(theme.muted);
        let lines = links
            .iter()
            .map(|link| {
                let strength = if link.settled { 0.35 } else { 0.75 };
                layout.connector(link, muted.opacity(strength))
            })
            .collect();
        canvas = canvas.child(style::connectors(lines, 1.5));
        // The selected card's links light up over the rest, fading in under
        // the selection's id so each pick replays it.
        if let Some(id) = selected {
            let lit: Vec<Connector> = links
                .iter()
                .filter(|link| link.from_id == id || link.to_id == id)
                .map(|link| layout.connector(link, theme.accent.into()))
                .collect();
            if !lit.is_empty() {
                canvas = canvas.child(style::enter(
                    SharedString::from(format!("pilus-board-lit-{id}")),
                    style::connectors(lit, 2.0),
                ));
            }
        }

        // Cards ride in a clip the lane's size, above the lines: a folding
        // lane narrows over its cards rather than squeezing them.
        for (lane, (_, cards)) in board.lanes.iter().enumerate() {
            let (left, lane_w, fold) = layout.column(lane);
            if fold >= 1.0 {
                continue;
            }
            let mut clip = div()
                .absolute()
                .left(px(left))
                .top_0()
                .w(px(lane_w))
                .h_full()
                .overflow_hidden();
            for (row, task) in cards.iter().enumerate() {
                let (x, y, height) = layout.card((lane, row));
                let waiting = waiting_on(task, &known);
                clip = clip.child(
                    self.board_card(task, &waiting, &geometry, height, cx)
                        .absolute()
                        .left(px(x - left))
                        .top(px(y)),
                );
            }
            canvas = canvas.child(clip);
        }
        if moving.get() {
            canvas = canvas.child(style::next_frame());
        }

        div()
            .id("pilus-board-scroll")
            .flex_1()
            .min_h(px(0.0))
            .min_w(px(0.0))
            .overflow_scroll()
            .child(canvas)
    }

    /// One lane's backdrop and header. Lanes are chrome, so they take the
    /// terminal card's hairline rather than the content border. Open, the
    /// header band folds it; folded, the whole strip opens it.
    fn board_lane(
        &self,
        index: usize,
        lane: Lane,
        count: usize,
        layout: &Layout,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let geometry = &layout.geometry;
        let (left, width, _) = layout.column(index);
        let folded = self.tasks_panel.folded_lanes.contains(&lane);
        let turning = self
            .tasks_panel
            .lane_turned
            .get(&lane)
            .is_some_and(|turned| turned.elapsed() < style::FOLD * 2);
        let chevron = style::chevron(
            format!("pilus-board-lane-chevron-{}", lane.label()),
            !folded,
            turning,
            fonts.secondary,
            theme.muted,
        );
        let count_label = div()
            .text_size(px(fonts.label))
            .text_color(theme.muted)
            .child(count.to_string());
        // Each state's content fades in under its own id, so the swap between
        // the header row and the strip never blinks.
        let content: AnyElement = if folded {
            let mut letters = div().flex().flex_col().items_center();
            for letter in lane.label().chars() {
                letters =
                    letters.child(style::section_label(theme, letter.to_string(), fonts.label));
            }
            style::enter(
                SharedString::from(format!("pilus-board-strip-{}", lane.label())),
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_2()
                    .w(px(geometry.strip_w))
                    .h_full()
                    .pt(px((geometry.header_h - fonts.secondary) / 2.0))
                    .pb(px(geometry.header_h / 2.0))
                    .child(letters)
                    .child(div().flex_1())
                    .child(count_label),
            )
            .into_any_element()
        } else {
            let mut open = div().flex().flex_col().w(px(geometry.lane_w())).child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .h(px(geometry.header_h))
                    .px(px(geometry.lane_pad + 4.0))
                    .child(chevron)
                    .child(status_glyph(lane.glyph(), fonts.secondary, theme))
                    .child(style::section_label(theme, lane.label(), fonts.label))
                    .child(count_label),
            );
            if count == 0 {
                open = open.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .mt(px(geometry.header_gap))
                        .h(px(geometry.card_h / 2.0))
                        .text_size(px(fonts.secondary))
                        .text_color(theme.muted)
                        .child("No tasks"),
                );
            }
            style::enter(
                SharedString::from(format!("pilus-board-lane-open-{}", lane.label())),
                open,
            )
            .into_any_element()
        };
        let backdrop = div()
            .id(SharedString::from(format!(
                "pilus-board-lane-{}",
                lane.label()
            )))
            .absolute()
            .left(px(left))
            .top(px(geometry.pad))
            .bottom(px(geometry.pad))
            .w(px(width))
            .overflow_hidden()
            .radius_md()
            .bg(Hsla::from(theme.panel_dim).opacity(0.45))
            .border_1()
            .border_color(theme.border)
            .child(content);
        if folded {
            return backdrop
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.toggle_lane(lane);
                    cx.notify();
                }));
        }
        // Open, only the header band folds the lane; the cards below it take
        // their own clicks.
        backdrop.child(
            div()
                .id(SharedString::from(format!(
                    "pilus-board-lane-head-{}",
                    lane.label()
                )))
                .absolute()
                .top_0()
                .left_0()
                .w_full()
                .h(px(geometry.header_h))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.toggle_lane(lane);
                    cx.notify();
                })),
        )
    }

    /// One task card: status and id, title, description, then who runs it,
    /// where, and what it is waiting on.
    fn board_card(
        &self,
        task: &Task,
        waiting: &[&str],
        geometry: &Geometry,
        height: f32,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let selected = self.tasks_panel.selected.as_deref() == Some(task.id.as_str());
        let folded = self.tasks_panel.folded.contains(&task.id);
        // The chevron turns only right after a toggle; a card scrolling into
        // view keeps it still.
        let turning = self
            .tasks_panel
            .fold_turned
            .get(&task.id)
            .is_some_and(|turned| turned.elapsed() < style::FOLD * 2);
        let fold_id = task.id.clone();
        let chevron = div()
            .id(SharedString::from(format!("pilus-board-fold-{}", task.id)))
            .flex_shrink_0()
            .cursor_pointer()
            .child(style::chevron(
                format!("pilus-board-chevron-{}", task.id),
                !folded,
                turning,
                fonts.secondary,
                theme.muted,
            ))
            .on_click(cx.listener(move |this, _, _, cx| {
                // Folding is not picking: the card's own click stays out.
                cx.stop_propagation();
                this.toggle_fold(fold_id.clone());
                cx.notify();
            }));
        let mono = SharedString::from(style::mono_family(&self.shared_settings));

        let corner: AnyElement = match task.context_percent {
            Some(percent) if task.status() == TaskStatus::Running => style::context_gauge(
                format!("pilus-board-gauge-{}", task.id),
                Some(f32::from(percent)),
                percent >= 85,
                theme,
                fonts.body,
            )
            .into_any_element(),
            _ => div()
                .text_size(px(fonts.label))
                .text_color(theme.muted)
                .child(age_label(unix_now(), task.updated))
                .into_any_element(),
        };
        let top = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(chevron)
            .child(status_glyph(task.status(), fonts.secondary, theme))
            .child(
                div()
                    .font_family(mono)
                    .text_size(px(fonts.label))
                    .text_color(theme.muted)
                    .child(task.id.clone()),
            )
            .child(
                div()
                    .text_size(px(fonts.label))
                    .text_color(theme.muted)
                    .child(task.status().label()),
            )
            .child(div().flex_1())
            .child(corner);

        let title = div()
            .flex_1()
            .min_w(px(0.0))
            .truncate()
            .text_size(px(fonts.body))
            .font_weight(if selected {
                FontWeight::BOLD
            } else {
                FontWeight::MEDIUM
            })
            .text_color(theme.panel_fg)
            .child(if task.title.is_empty() {
                "Untitled task".to_string()
            } else {
                task.title.clone()
            });
        // Folded, the card keeps the one thing a glance must not miss.
        let title = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(title)
            .children(
                (folded && !waiting.is_empty()).then(|| blocked_badge(waiting, theme, fonts.label)),
            );

        let description = div()
            .flex_1()
            .min_h(px(0.0))
            .overflow_hidden()
            .line_clamp(2)
            .text_size(px(fonts.secondary))
            .text_color(theme.muted)
            .child(
                task.prompt
                    .as_deref()
                    .map(|prompt| prompt.split_whitespace().collect::<Vec<_>>().join(" "))
                    .unwrap_or_else(|| "No description".to_string()),
            );

        let agent = self.task_agent(task);
        let (agent_mark, agent_label, agent_color) = match agent {
            Some(agent) => (agent_icon(&agent.agent), agent_name(agent), theme.panel_fg),
            None if task.assignee.is_some() => (
                agent_icon(""),
                format!("pane {}", task.assignee.unwrap_or_default()),
                theme.panel_fg,
            ),
            None => (agent_icon(""), "Unassigned".to_string(), theme.muted),
        };
        let mut footer = div().flex().flex_row().items_center().gap_2().child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .flex_1()
                .min_w(px(0.0))
                .child(icon(agent_mark, fonts.secondary, agent_color))
                .child(
                    div()
                        .truncate()
                        .text_size(px(fonts.secondary))
                        .text_color(agent_color)
                        .child(agent_label),
                ),
        );
        if let Some(workspace) = self.task_workspace_name(task) {
            footer = footer.child(style::chip(theme, workspace, fonts.label));
        }
        if !waiting.is_empty() {
            footer = footer.child(blocked_badge(waiting, theme, fonts.label));
        }

        let id = task.id.clone();
        style::board_card(format!("pilus-board-card-{}", task.id), selected, theme)
            .w(px(geometry.card_w))
            .h(px(height))
            // The content keeps its open height and the card clips it, so
            // folding draws a curtain over the lower lines instead of
            // squeezing them.
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .flex_shrink_0()
                    .w_full()
                    .h(px(geometry.card_h))
                    .px_3()
                    .py_2()
                    .child(top)
                    .child(title)
                    .child(description)
                    .child(footer),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                let next =
                    (this.tasks_panel.selected.as_deref() != Some(id.as_str())).then(|| id.clone());
                this.tasks_panel.close_composer();
                this.tasks_panel.confirm_delete = None;
                this.tasks_panel.selected = next;
                cx.notify();
            }))
    }
}

/// The agent as the card names it: the user's alias, else its kind.
/// `Agent::label` falls back to the pane's folder, which the card's
/// workspace chip already shows.
pub(super) fn agent_name(agent: &Agent) -> String {
    if agent.name.is_empty() {
        agent.agent.clone()
    } else {
        agent.name.clone()
    }
}

/// `t3` or `t3 +1`: what a card is waiting on, tinted as a hold.
fn blocked_badge(waiting: &[&str], theme: super::theme::Theme, size: f32) -> Div {
    let text = match waiting {
        [one] => (*one).to_string(),
        [first, rest @ ..] => format!("{first} +{}", rest.len()),
        [] => String::new(),
    };
    style::chip(
        theme,
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .child(icon(
                include_bytes!("../../assets/icons/task-blocked.svg"),
                size,
                theme.alert,
            ))
            .child(div().text_color(theme.alert).child(text)),
        size,
    )
}

#[cfg(test)]
mod tests {
    use pilus_client::{Task, TaskStatus};

    use super::{Board, Geometry, Lane, Layout, TaskView, depths, stack, waiting_on};

    fn task(id: &str, status: TaskStatus, deps: &[&str]) -> Task {
        Task {
            id: id.to_string(),
            status: Some(status),
            deps: deps.iter().map(|dep| dep.to_string()).collect(),
            ..Task::default()
        }
    }

    #[test]
    fn statuses_fold_into_stages() {
        assert_eq!(Lane::of(TaskStatus::Claimed), Lane::Working);
        assert_eq!(Lane::of(TaskStatus::Blocked), Lane::Review);
        assert_eq!(Lane::of(TaskStatus::Merging), Lane::Done);
    }

    #[test]
    fn empty_stages_stay_but_failed_only_when_used() {
        let tasks = [task("t1", TaskStatus::Queued, &[])];
        let board = Board::build(&tasks, TaskView::All);
        let lanes: Vec<_> = board.lanes.iter().map(|(lane, _)| *lane).collect();
        assert_eq!(
            lanes,
            [Lane::Queued, Lane::Working, Lane::Review, Lane::Done]
        );
        let active = Board::build(&tasks, TaskView::Active);
        assert_eq!(active.lanes.len(), 3);
    }

    #[test]
    fn a_task_sits_below_what_it_waits_on() {
        let tasks = [
            task("t9", TaskStatus::Queued, &["t2"]),
            task("t2", TaskStatus::Queued, &["t1"]),
            task("t10", TaskStatus::Queued, &[]),
            task("t1", TaskStatus::Running, &[]),
        ];
        let board = Board::build(&tasks, TaskView::All);
        let queued: Vec<_> = board.lanes[0]
            .1
            .iter()
            .map(|task| task.id.as_str())
            .collect();
        assert_eq!(queued, ["t10", "t2", "t9"]);
        assert_eq!(depths(&tasks)["t9"], 2);
    }

    #[test]
    fn links_join_cards_and_mark_merged_ones_settled() {
        let tasks = [
            task("t1", TaskStatus::Merged, &[]),
            task("t2", TaskStatus::Running, &["t1"]),
            task("t3", TaskStatus::Queued, &["t2", "t7"]),
        ];
        let board = Board::build(&tasks, TaskView::All);
        let links = board.links();
        assert_eq!(links.len(), 2, "t7 is not on the board");
        let settled = links.iter().find(|link| link.from_id == "t1").expect("t1");
        assert!(settled.settled);
        assert_eq!(settled.to, (1, 0));
        assert!(
            !links
                .iter()
                .any(|link| link.from_id == "t2" && link.settled)
        );
    }

    #[test]
    fn only_unmerged_known_deps_hold_a_task() {
        let tasks = [
            task("t1", TaskStatus::Merged, &[]),
            task("t2", TaskStatus::Done, &[]),
            task("t3", TaskStatus::Queued, &["t1", "t2", "t8"]),
        ];
        assert_eq!(waiting_on(&tasks[2], &tasks), ["t2"]);
    }

    #[test]
    fn arrows_walk_lanes_and_skip_empty_ones() {
        let tasks = [
            task("t1", TaskStatus::Queued, &[]),
            task("t2", TaskStatus::Queued, &[]),
            task("t3", TaskStatus::Review, &[]),
        ];
        let board = Board::build(&tasks, TaskView::All);
        assert_eq!(board.step(None, 0, 1).as_deref(), Some("t1"));
        assert_eq!(board.step(Some("t1"), 0, 1).as_deref(), Some("t2"));
        assert_eq!(board.step(Some("t2"), 0, 1).as_deref(), Some("t2"));
        // Working is empty, so right from Queued lands in Review.
        assert_eq!(board.step(Some("t2"), 1, 0).as_deref(), Some("t3"));
        assert_eq!(board.step(Some("t3"), 1, 0).as_deref(), Some("t3"));
        assert_eq!(board.step(Some("t3"), -1, 0).as_deref(), Some("t1"));
    }

    #[test]
    fn folded_cards_pull_the_stack_up() {
        assert_eq!(stack(10.0, 4.0, &[20.0, 8.0, 20.0]), [10.0, 34.0, 46.0]);
        assert!(stack(0.0, 4.0, &[]).is_empty());
    }

    #[test]
    fn a_link_meets_each_card_at_its_own_middle() {
        let tasks = [
            task("t1", TaskStatus::Queued, &[]),
            task("t2", TaskStatus::Queued, &["t1"]),
        ];
        let board = Board::build(&tasks, TaskView::All);
        let geometry = Geometry::new(|design| design);
        let layout = Layout::new(
            geometry,
            &board,
            |_| 0.0,
            |task| {
                if task.id == "t1" { 40.0 } else { 100.0 }
            },
        );
        let (_, top, _) = layout.card((0, 1));
        assert_eq!(top, geometry.cards_top() + 40.0 + geometry.row_gap);
        let line = layout.connector(&board.links()[0], gpui::Hsla::default());
        assert_eq!(line.from.1, geometry.cards_top() + 20.0);
        assert_eq!(line.to.1, top + 50.0);
    }

    #[test]
    fn a_folded_lane_narrows_and_takes_its_cards_lines() {
        let tasks = [
            task("t1", TaskStatus::Running, &[]),
            task("t2", TaskStatus::Queued, &["t1"]),
            task("t3", TaskStatus::Queued, &["t2"]),
        ];
        let board = Board::build(&tasks, TaskView::All);
        let geometry = Geometry::new(|design| design);
        let fold = |lane| if lane == Lane::Queued { 1.0 } else { 0.0 };
        let layout = Layout::new(geometry, &board, fold, |_| geometry.card_h);
        let (_, width, _) = layout.column(0);
        assert_eq!(width, geometry.strip_w);
        let (working, ..) = layout.column(1);
        assert_eq!(working, geometry.pad + geometry.strip_w + geometry.lane_gap);
        let links = board.links();
        let into_strip = links.iter().find(|link| link.from_id == "t1").expect("t1");
        let line = layout.connector(into_strip, gpui::Hsla::default());
        assert_eq!(line.to.0, geometry.pad + geometry.strip_w);
        let inside = links.iter().find(|link| link.from_id == "t2").expect("t2");
        assert!(layout.hides(inside));
        assert!(!layout.hides(into_strip));
    }

    #[test]
    fn a_cycle_ends_the_chain() {
        let tasks = [
            task("t1", TaskStatus::Queued, &["t2"]),
            task("t2", TaskStatus::Queued, &["t1"]),
        ];
        let depth = depths(&tasks);
        assert!(depth["t1"] <= 2 && depth["t2"] <= 2);
    }
}
