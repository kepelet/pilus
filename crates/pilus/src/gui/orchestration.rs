//! The native Orchestration page: the task queue as a free canvas, cards the
//! user places and arrows that say what waits on what.
//!
//! It is a second layout of the Tasks page, not a second copy of it: the
//! tasks, the machine and workspace scope, the selection, the composer, and
//! the detail pane are all `TasksPanel`'s, so switching between the list and
//! the canvas keeps the user's place. This module holds the canvas's pure
//! placement rules (default positions, links, arrow-key moves), which stay
//! unit-testable without GPUI, the layout file that remembers where the user
//! put things, and the rendering and drag handling that paint and edit it.
//!
//! The daemon owns the tasks and their dependencies; the canvas owns only
//! where cards sit, the arrows the daemon would not record, and the
//! workspace and agent a queued card will start with. Those live in
//! `orchestration.json` beside the settings, per machine.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

use gpui::{
    Anchor, AnyElement, Context, Div, DragMoveEvent, FontWeight, Hsla, MouseButton, Pixels, Point,
    Render, SharedString, Stateful, Window, anchored, deferred, div, prelude::*, px,
};
use pilus_client::{Agent, Task, TaskStatus};
use serde::{Deserialize, Serialize};

use super::style::{self, Connector, FillExt, Radius, RadiusExt, Side};
use super::task_compose::TasksMsg;
use super::tasks::{TaskLayout, age_label, status_glyph, unix_now};
use super::toast::ToastKind;
use super::{PICKER_MENU_MAX_H, PilusView, agent_icon, icon, picker_menu, picker_row};

/// A card's box on the canvas.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl Rect {
    fn center(self) -> (f32, f32) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }
}

/// A dependency drawn between two cards: `from` must merge before `to` can
/// run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Link {
    pub(super) from: String,
    pub(super) to: String,
    /// The dependency already merged: the line is history, not a hold.
    pub(super) settled: bool,
    /// Drawn on the canvas but not recorded by the daemon, so it is a plan
    /// rather than a hold.
    pub(super) planned: bool,
}

/// Every dependency whose both ends are among `tasks`, then the user's own
/// arrows the daemon does not already hold.
pub(super) fn links(tasks: &[&Task], planned: &[(String, String)]) -> Vec<Link> {
    let by_id: HashMap<&str, &Task> = tasks.iter().map(|task| (task.id.as_str(), *task)).collect();
    let mut seen = HashSet::new();
    let mut links = Vec::new();
    for task in tasks {
        for dep in &task.deps {
            let Some(from) = by_id.get(dep.as_str()) else {
                continue;
            };
            if seen.insert((dep.as_str(), task.id.as_str())) {
                links.push(Link {
                    from: dep.clone(),
                    to: task.id.clone(),
                    settled: from.status() == TaskStatus::Merged,
                    planned: false,
                });
            }
        }
    }
    for (from, to) in planned {
        if by_id.contains_key(from.as_str())
            && by_id.contains_key(to.as_str())
            && seen.insert((from.as_str(), to.as_str()))
        {
            links.push(Link {
                from: from.clone(),
                to: to.clone(),
                settled: false,
                planned: true,
            });
        }
    }
    links
}

/// Whether `goal` can be reached from `start` by following `edges` (each a
/// `(from, to)` arrow).
fn reaches(edges: &[(&str, &str)], start: &str, goal: &str) -> bool {
    let mut seen = HashSet::new();
    let mut stack = vec![start];
    while let Some(at) = stack.pop() {
        if at == goal {
            return true;
        }
        if !seen.insert(at) {
            continue;
        }
        stack.extend(
            edges
                .iter()
                .filter(|(from, _)| *from == at)
                .map(|(_, to)| *to),
        );
    }
    false
}

/// Whether an arrow `from → to` would close a loop among `links`: `to`
/// already leads back to `from`, or the arrow points at its own card.
fn closes_loop(links: &[Link], from: &str, to: &str) -> bool {
    let edges: Vec<(&str, &str)> = links
        .iter()
        .map(|link| (link.from.as_str(), link.to.as_str()))
        .collect();
    from == to || reaches(&edges, to, from)
}

/// Each task's longest dependency chain among `tasks`. A cycle, which the
/// daemon should refuse but a hand-edited queue could hold, ends the chain
/// instead of recursing forever.
fn depths<'a>(tasks: &[&'a Task]) -> HashMap<&'a str, usize> {
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
    let by_id: HashMap<&str, &Task> = tasks.iter().map(|task| (task.id.as_str(), *task)).collect();
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

/// The canvas's measurements, in pixels at the user's Interface size.
#[derive(Clone, Copy, Debug)]
struct Geometry {
    card_w: f32,
    card_h: f32,
    /// A folded card: its status line and title.
    folded_h: f32,
    /// Room between default columns for the lines to bend through.
    gap_x: f32,
    gap_y: f32,
    pad: f32,
    /// The dots' spacing, which a dragged card snaps to.
    grid: f32,
}

impl Geometry {
    fn new(scale: impl Fn(f32) -> f32) -> Self {
        Self {
            card_w: scale(248.0),
            card_h: scale(124.0),
            folded_h: scale(58.0),
            gap_x: scale(72.0),
            gap_y: scale(24.0),
            pad: 32.0,
            grid: 24.0,
        }
    }
}

/// Where cards with no saved spot go: a column per dependency depth, each
/// column ordered by id, so a chain reads left to right and its lines stay
/// short. Positions are in canvas pixels.
fn auto_positions(tasks: &[&Task], geometry: &Geometry) -> HashMap<String, (f32, f32)> {
    let depth = depths(tasks);
    let mut order: Vec<&Task> = tasks.to_vec();
    order.sort_by_key(|task| {
        (
            depth.get(task.id.as_str()).copied().unwrap_or(0),
            id_number(&task.id),
            task.id.clone(),
        )
    });
    let mut rows: HashMap<usize, usize> = HashMap::new();
    order
        .into_iter()
        .map(|task| {
            let column = depth.get(task.id.as_str()).copied().unwrap_or(0);
            let row = rows.entry(column).or_insert(0);
            let at = (
                geometry.pad + column as f32 * (geometry.card_w + geometry.gap_x),
                geometry.pad + *row as f32 * (geometry.card_h + geometry.gap_y),
            );
            *row += 1;
            (task.id.clone(), at)
        })
        .collect()
}

/// Every card's box: its saved or default position at `height(task)`.
fn rects(
    tasks: &[&Task],
    positions: &HashMap<String, (f32, f32)>,
    geometry: &Geometry,
    mut height: impl FnMut(&Task) -> f32,
) -> Vec<(String, Rect)> {
    tasks
        .iter()
        .map(|task| {
            let (x, y) = positions
                .get(&task.id)
                .copied()
                .unwrap_or((geometry.pad, geometry.pad));
            (
                task.id.clone(),
                Rect {
                    x,
                    y,
                    w: geometry.card_w,
                    h: height(task),
                },
            )
        })
        .collect()
}

/// The canvas's extent: every card, and a margin past the farthest.
fn extent(rects: &[(String, Rect)], geometry: &Geometry) -> (f32, f32) {
    rects.iter().fold(
        (
            geometry.card_w + 2.0 * geometry.pad,
            geometry.card_h + 2.0 * geometry.pad,
        ),
        |(width, height), (_, rect)| {
            (
                width.max(rect.x + rect.w + geometry.pad),
                height.max(rect.y + rect.h + geometry.pad),
            )
        },
    )
}

/// The card an arrow key moves to from `current`: the nearest one that lies
/// in that direction, counting a sideways offset double so the pick stays
/// roughly in line. With nothing selected, any key picks the top-left card.
fn step(cards: &[(String, Rect)], current: Option<&str>, dx: f32, dy: f32) -> Option<String> {
    let Some(from) = current
        .and_then(|id| cards.iter().find(|(card, _)| card == id))
        .map(|(_, rect)| rect.center())
    else {
        return cards
            .iter()
            .min_by(|(_, a), (_, b)| a.y.total_cmp(&b.y).then(a.x.total_cmp(&b.x)))
            .map(|(id, _)| id.clone());
    };
    cards
        .iter()
        .filter(|(id, _)| Some(id.as_str()) != current)
        .filter_map(|(id, rect)| {
            let (x, y) = rect.center();
            let (along, across) = if dx != 0.0 {
                ((x - from.0) * dx, (y - from.1).abs())
            } else {
                ((y - from.1) * dy, (x - from.0).abs())
            };
            (along > 1.0).then_some((id, along + 2.0 * across))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(id, _)| id.clone())
        .or_else(|| current.map(str::to_string))
}

/// The line between two cards: out of the side that faces the other card,
/// into the side that faces this one. Cards that overlap in x bow out of
/// both right edges.
fn connector(from: Rect, to: Rect, color: Hsla, dashed: bool) -> Connector {
    let (from_side, to_side) = if to.x >= from.x + from.w {
        (Side::Right, Side::Left)
    } else if to.x + to.w <= from.x {
        (Side::Left, Side::Right)
    } else {
        (Side::Right, Side::Right)
    };
    let port = |rect: Rect, side: Side| match side {
        Side::Left => (rect.x, rect.y + rect.h / 2.0),
        Side::Right => (rect.x + rect.w, rect.y + rect.h / 2.0),
    };
    Connector {
        from: port(from, from_side),
        from_side,
        to: port(to, to_side),
        to_side,
        color,
        dashed,
    }
}

/// What a queued card will start with, when the user chose.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct Pick {
    /// The workspace to start in, by id.
    #[serde(default)]
    pub(super) workspace: Option<String>,
    /// The agent kind to start with.
    #[serde(default)]
    pub(super) agent: Option<String>,
}

/// One machine's canvas: its task ids are the daemon's, so they key it.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct MachineCanvas {
    /// Each pinned card's top-left corner.
    #[serde(default)]
    positions: BTreeMap<String, (f32, f32)>,
    /// Arrows drawn here, `(from, to)`: `to` waits on `from`.
    #[serde(default)]
    links: Vec<(String, String)>,
    #[serde(default)]
    picks: BTreeMap<String, Pick>,
}

/// Everything the canvas remembers, by machine.
#[derive(Debug, Default, Deserialize, Serialize)]
struct CanvasFile {
    #[serde(default)]
    machines: BTreeMap<String, MachineCanvas>,
}

const FILE: &str = "orchestration.json";

impl CanvasFile {
    /// Read `<root>/orchestration.json`, falling back to an empty canvas.
    fn load(root: &Path) -> Self {
        std::fs::read_to_string(root.join(FILE))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Persist, best-effort like the settings: a canvas that cannot be saved
    /// still works for the session.
    fn save(&self, root: &Path) {
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = std::fs::create_dir_all(root);
            let _ = std::fs::write(root.join(FILE), text);
        }
    }
}

/// Which of a card's two pickers is open.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PickKind {
    Workspace,
    Agent,
}

/// A card being dragged: the pointer where it last moved.
#[derive(Debug)]
struct Grab {
    id: String,
    last: Option<Point<Pixels>>,
}

/// An arrow being drawn: from `from`'s port to the pointer.
#[derive(Debug)]
struct Wire {
    from: String,
    /// Out of the card's right port (it is waited on) or into its left one.
    outgoing: bool,
    to: (f32, f32),
}

/// The canvas's state: what it remembers, and the gesture in flight.
#[derive(Debug, Default)]
pub(super) struct Canvas {
    file: Option<CanvasFile>,
    drag: Option<Grab>,
    wire: Option<Wire>,
    /// Something changed that the file does not hold yet.
    dirty: bool,
    /// Where each card sat at the last render, for drags to start from.
    shown: HashMap<String, (f32, f32)>,
    /// The picker open on a card.
    menu: Option<(String, PickKind)>,
}

impl Canvas {
    fn ensure_loaded(&mut self, root: &Path) {
        if self.file.is_none() {
            self.file = Some(CanvasFile::load(root));
        }
    }

    fn machine(&mut self, key: &str) -> &mut MachineCanvas {
        self.file
            .get_or_insert_with(CanvasFile::default)
            .machines
            .entry(key.to_string())
            .or_default()
    }

    fn peek(&self, key: &str) -> Option<&MachineCanvas> {
        self.file.as_ref()?.machines.get(key)
    }

    /// What `id` will start with, on the machine `key`.
    pub(super) fn pick(&self, key: &str, id: &str) -> Option<&Pick> {
        self.peek(key)?.picks.get(id)
    }

    fn save_if_dirty(&mut self, root: &Path) {
        if self.dirty
            && let Some(file) = &self.file
        {
            file.save(root);
            self.dirty = false;
        }
    }
}

/// A card being moved, as GPUI's drag carries it.
#[derive(Clone)]
struct CardDrag {
    id: String,
}

/// An arrow being drawn from a port.
#[derive(Clone)]
struct LinkDrag {
    id: String,
    outgoing: bool,
}

/// What GPUI draws at the pointer for a drag. The card itself moves, so
/// there is nothing more to show.
struct DragGhost;

impl Render for DragGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

impl PilusView {
    /// Open the canvas from the sidebar.
    pub(super) fn open_orchestration(&mut self, cx: &mut Context<Self>) {
        self.leave_settings_page();
        self.picker.close();
        self.tasks_panel.open_as(TaskLayout::Board);
        self.tasks_panel.canvas.ensure_loaded(&self.pilus_root);
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

    fn canvas_geometry(&self) -> Geometry {
        Geometry::new(|design| style::scaled(&self.shared_settings, design))
    }

    /// Sync the canvas with the queue before it paints: drop what refers to
    /// tasks the machine no longer lists or the daemon now records itself,
    /// work out where each card sits, and write the file once a gesture
    /// has ended.
    pub(super) fn prepare_canvas(&mut self, tasks: &[Task], cx: &Context<Self>) {
        let machine = self.scope_machine();
        let key = machine.key();
        let everything = self.machine_tasks(&machine);
        let geometry = self.canvas_geometry();
        let refs: Vec<&Task> = tasks.iter().collect();
        let auto = auto_positions(&refs, &geometry);
        let dragging = cx.has_active_drag();
        let canvas = &mut self.tasks_panel.canvas;
        canvas.ensure_loaded(&self.pilus_root);
        if !dragging {
            canvas.drag = None;
            canvas.wire = None;
        }
        // An empty machine may just not have answered yet, so it prunes nothing.
        if !everything.is_empty() {
            let known: HashSet<&str> = everything.iter().map(|task| task.id.as_str()).collect();
            let queued: HashSet<&str> = everything
                .iter()
                .filter(|task| task.status() == TaskStatus::Queued)
                .map(|task| task.id.as_str())
                .collect();
            let held: HashSet<(&str, &str)> = everything
                .iter()
                .flat_map(|task| task.deps.iter().map(|dep| (dep.as_str(), task.id.as_str())))
                .collect();
            let state = canvas.machine(&key);
            let before = (state.positions.len(), state.links.len(), state.picks.len());
            state.positions.retain(|id, _| known.contains(id.as_str()));
            state.picks.retain(|id, _| queued.contains(id.as_str()));
            state.links.retain(|(from, to)| {
                known.contains(from.as_str())
                    && known.contains(to.as_str())
                    && !held.contains(&(from.as_str(), to.as_str()))
            });
            let after = (state.positions.len(), state.links.len(), state.picks.len());
            canvas.dirty |= before != after;
        }
        let pinned = canvas.peek(&key).map(|state| state.positions.clone());
        let pinned = pinned.unwrap_or_default();
        canvas.shown = refs
            .iter()
            .filter_map(|task| {
                let at = pinned
                    .get(&task.id)
                    .or_else(|| auto.get(&task.id))
                    .copied()?;
                Some((task.id.clone(), at))
            })
            .collect();
        if !dragging {
            canvas.save_if_dirty(&self.pilus_root);
        }
    }

    /// Arrow keys and hjkl move the selection across the canvas.
    pub(super) fn board_key(&mut self, key: &str, tasks: &[Task]) -> bool {
        let (dx, dy) = match key {
            "left" | "h" => (-1.0, 0.0),
            "right" | "l" => (1.0, 0.0),
            "up" | "k" => (0.0, -1.0),
            "down" | "j" => (0.0, 1.0),
            "space" => {
                if let Some(id) = self.tasks_panel.selected.clone() {
                    self.toggle_fold(id);
                }
                return true;
            }
            _ => return false,
        };
        let geometry = self.canvas_geometry();
        let refs: Vec<&Task> = tasks.iter().collect();
        let cards = rects(&refs, &self.tasks_panel.canvas.shown, &geometry, |task| {
            if self.tasks_panel.folded.contains(&task.id) {
                geometry.folded_h
            } else {
                geometry.card_h
            }
        });
        let next = step(&cards, self.tasks_panel.selected.as_deref(), dx, dy);
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

    /// A card is being dragged: carry it by how far the pointer moved since
    /// the last event, and pick it. The first event only anchors the pointer,
    /// so the card does not jump by the distance GPUI waited before starting.
    fn canvas_move(&mut self, id: &str, position: Point<Pixels>, cx: &mut Context<Self>) {
        let key = self.scope_machine().key();
        let grid = self.canvas_geometry().grid;
        let panel = &mut self.tasks_panel;
        if panel.canvas.drag.as_ref().is_none_or(|grab| grab.id != id) {
            panel.canvas.drag = Some(Grab {
                id: id.to_string(),
                last: None,
            });
            panel.selected = Some(id.to_string());
            panel.confirm_delete = None;
            panel.close_composer();
        }
        let canvas = &mut panel.canvas;
        let Some(grab) = canvas.drag.as_mut() else {
            return;
        };
        let Some(last) = grab.last.replace(position) else {
            cx.notify();
            return;
        };
        let moved = position - last;
        let Some(&(x, y)) = canvas.shown.get(id) else {
            return;
        };
        // Held back from the edge, so a card is never lost off the canvas.
        let at = (
            (x + moved.x.as_f32()).max(grid),
            (y + moved.y.as_f32()).max(grid),
        );
        canvas.machine(&key).positions.insert(id.to_string(), at);
        canvas.shown.insert(id.to_string(), at);
        canvas.dirty = true;
        cx.notify();
    }

    /// An arrow is being drawn: follow the pointer, in canvas coordinates.
    fn canvas_wire(&mut self, drag: &LinkDrag, to: (f32, f32), cx: &mut Context<Self>) {
        self.tasks_panel.canvas.wire = Some(Wire {
            from: drag.id.clone(),
            outgoing: drag.outgoing,
            to,
        });
        cx.notify();
    }

    /// The pointer came up: the gesture is over, so keep what it changed.
    fn canvas_release(&mut self, cx: &mut Context<Self>) {
        let canvas = &mut self.tasks_panel.canvas;
        canvas.drag = None;
        canvas.wire = None;
        canvas.save_if_dirty(&self.pilus_root);
        cx.notify();
    }

    /// Ask the daemon to record `deps` for `id`. Older daemons refuse, which
    /// leaves the canvas's own arrow standing.
    fn send_deps(&mut self, id: String, deps: Vec<String>, cx: &mut Context<Self>) {
        let tx = self.tasks_op_tx.clone();
        let client = self.control_client_for_endpoint(&self.scope_machine());
        std::thread::spawn(move || {
            let message = match client.map(|mut client| client.set_task_deps(&id, &deps)) {
                Some(Ok(())) if deps.is_empty() => Ok(format!("{id} waits on nothing")),
                Some(Ok(())) => Ok(format!("{id} waits on {}", deps.join(", "))),
                Some(Err(error)) => Err(format!(
                    "The daemon did not take the change to {id}, so it stays on this canvas: {error}"
                )),
                None => Err("Could not reach the daemon; the arrow stays on this canvas".into()),
            };
            tx.send(TasksMsg::Status(message)).ok();
        });
        cx.notify();
    }

    /// An arrow was dropped on `target`: draw it, and tell the daemon.
    fn draw_link(&mut self, drag: &LinkDrag, target: &str, cx: &mut Context<Self>) {
        let (from, to) = if drag.outgoing {
            (drag.id.clone(), target.to_string())
        } else {
            (target.to_string(), drag.id.clone())
        };
        let machine = self.scope_machine();
        let key = machine.key();
        let everything = self.machine_tasks(&machine);
        let refs: Vec<&Task> = everything.iter().collect();
        let planned = self
            .tasks_panel
            .canvas
            .peek(&key)
            .map(|state| state.links.clone())
            .unwrap_or_default();
        let current = links(&refs, &planned);
        if from == to {
            return;
        }
        if current
            .iter()
            .any(|link| link.from == from && link.to == to)
        {
            self.toast(ToastKind::Info, format!("{to} already waits on {from}"));
            cx.notify();
            return;
        }
        if closes_loop(&current, &from, &to) {
            self.toast(ToastKind::Info, format!("{from} already waits on {to}"));
            cx.notify();
            return;
        }
        let canvas = &mut self.tasks_panel.canvas;
        canvas.machine(&key).links.push((from.clone(), to.clone()));
        canvas.dirty = true;
        canvas.save_if_dirty(&self.pilus_root);
        let mut deps: Vec<String> = everything
            .iter()
            .find(|task| task.id == to)
            .map(|task| task.deps.clone())
            .unwrap_or_default();
        deps.push(from);
        self.send_deps(to, deps, cx);
    }

    /// Take an arrow off the canvas, and off the task if the daemon holds it.
    fn remove_link(&mut self, from: &str, to: &str, cx: &mut Context<Self>) {
        let machine = self.scope_machine();
        let key = machine.key();
        let canvas = &mut self.tasks_panel.canvas;
        let state = canvas.machine(&key);
        state.links.retain(|(a, b)| !(a == from && b == to));
        canvas.dirty = true;
        canvas.save_if_dirty(&self.pilus_root);
        let held = self
            .machine_tasks(&machine)
            .into_iter()
            .find(|task| task.id == to)
            .filter(|task| task.deps.iter().any(|dep| dep == from));
        if let Some(task) = held {
            let deps = task.deps.into_iter().filter(|dep| dep != from).collect();
            self.send_deps(to.to_string(), deps, cx);
        }
        cx.notify();
    }

    /// Open `kind`'s list on a card, or close it if it is the open one.
    fn toggle_pick_menu(&mut self, id: &str, kind: PickKind, cx: &mut Context<Self>) {
        let canvas = &mut self.tasks_panel.canvas;
        if canvas
            .menu
            .as_ref()
            .is_some_and(|(open, k)| open == id && *k == kind)
        {
            canvas.menu = None;
        } else {
            canvas.menu = Some((id.to_string(), kind));
            if kind == PickKind::Agent {
                let key = self.scope_machine().key();
                self.ensure_agent_configs_for(&key);
            }
        }
        cx.notify();
    }

    /// Choose what `id` starts with. `None` goes back to the default.
    fn choose_pick(
        &mut self,
        id: &str,
        kind: PickKind,
        value: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let key = self.scope_machine().key();
        let canvas = &mut self.tasks_panel.canvas;
        canvas.menu = None;
        let pick = canvas
            .machine(&key)
            .picks
            .entry(id.to_string())
            .or_default();
        match kind {
            PickKind::Workspace => pick.workspace = value,
            PickKind::Agent => pick.agent = value,
        }
        if *pick == Pick::default() {
            canvas.machine(&key).picks.remove(id);
        }
        canvas.dirty = true;
        canvas.save_if_dirty(&self.pilus_root);
        cx.notify();
    }

    /// The canvas, drawn where the Tasks page draws its list.
    pub(super) fn task_canvas(&self, tasks: &[Task], cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = self.theme;
        let geometry = self.canvas_geometry();
        let machine = self.scope_machine();
        let key = machine.key();
        let refs: Vec<&Task> = tasks.iter().collect();
        let mut moving = false;
        let cards = rects(&refs, &self.tasks_panel.canvas.shown, &geometry, |task| {
            let (height, tweening) = self.card_height(&task.id, &geometry, cx);
            moving |= tweening;
            height
        });
        let (width, height) = extent(&cards, &geometry);
        let rect_of: HashMap<&str, Rect> = cards
            .iter()
            .map(|(id, rect)| (id.as_str(), *rect))
            .collect();
        // Dependencies can live outside the scoped workspace, so waiting is
        // judged against the whole machine.
        let known = self.machine_tasks(&machine);
        let selected = self.tasks_panel.selected.as_deref();
        let canvas_state = &self.tasks_panel.canvas;
        let planned = canvas_state
            .peek(&key)
            .map(|state| state.links.clone())
            .unwrap_or_default();

        let mut surface = div()
            .id("pilus-canvas")
            .relative()
            .w(px(width))
            .h(px(height))
            .min_w_full()
            .min_h_full()
            .child(style::dot_grid(
                Hsla::from(theme.border).opacity(0.7),
                geometry.grid,
            ));

        let all_links = links(&refs, &planned);
        let muted = Hsla::from(theme.muted);
        let branch = Hsla::from(theme.branch);
        let line = |link: &Link, color: Hsla| {
            let (from, to) = (
                rect_of.get(link.from.as_str())?,
                rect_of.get(link.to.as_str())?,
            );
            Some(connector(*from, *to, color, link.settled))
        };
        let lines = all_links
            .iter()
            .filter_map(|link| {
                let strength = if link.settled { 0.35 } else { 0.75 };
                line(
                    link,
                    if link.planned { branch } else { muted }.opacity(strength),
                )
            })
            .collect();
        surface = surface.child(style::connectors(lines, 1.5));

        // The selected card's links light up over the rest, fading in under
        // the selection's id so each pick replays it, each with a handle to
        // cut it.
        if let Some(id) = selected {
            let touching: Vec<&Link> = all_links
                .iter()
                .filter(|link| link.from == id || link.to == id)
                .collect();
            let lit: Vec<Connector> = touching
                .iter()
                .filter_map(|link| line(link, theme.accent.into()))
                .collect();
            if !lit.is_empty() {
                let handles: Vec<AnyElement> = touching
                    .iter()
                    .filter_map(|link| {
                        let at = line(link, muted)?.midpoint(2.0);
                        Some(self.link_handle(link, at, cx))
                    })
                    .collect();
                surface = surface.child(style::enter(
                    SharedString::from(format!("pilus-canvas-lit-{id}")),
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .child(style::connectors(lit, 2.0))
                        .children(handles),
                ));
            }
        }

        // The arrow being drawn follows the pointer.
        if let Some(wire) = &canvas_state.wire
            && let Some(card) = rect_of.get(wire.from.as_str())
        {
            let tip = Rect {
                x: wire.to.0,
                y: wire.to.1,
                w: 0.0,
                h: 0.0,
            };
            let line = if wire.outgoing {
                connector(*card, tip, theme.accent.into(), true)
            } else {
                connector(tip, *card, theme.accent.into(), true)
            };
            surface = surface.child(style::connectors(vec![line], 1.5));
        }

        // Cards sit above the lines. The picked and dragged one goes last,
        // so it overlaps the rest rather than hiding behind them.
        let mut order: Vec<&Task> = refs.clone();
        order.sort_by_key(|task| Some(task.id.as_str()) == selected);
        for task in order {
            let Some(rect) = rect_of.get(task.id.as_str()) else {
                continue;
            };
            let waiting = waiting_on(task, &known);
            surface = surface.child(self.canvas_card(task, *rect, &waiting, &key, cx));
        }
        if moving {
            surface = surface.child(style::next_frame());
        }

        let surface = surface
            .on_drag_move::<CardDrag>(cx.listener(
                |this, event: &DragMoveEvent<CardDrag>, _, cx| {
                    let id = event.drag(cx).id.clone();
                    this.canvas_move(&id, event.event.position, cx);
                },
            ))
            .on_drag_move::<LinkDrag>(cx.listener(
                |this, event: &DragMoveEvent<LinkDrag>, _, cx| {
                    let drag = event.drag(cx).clone();
                    let at = event.event.position - event.bounds.origin;
                    this.canvas_wire(&drag, (at.x.as_f32(), at.y.as_f32()), cx);
                },
            ))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.canvas_release(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.canvas_release(cx)),
            );

        div()
            .id("pilus-board-scroll")
            .flex_1()
            .min_h(px(0.0))
            .min_w(px(0.0))
            .overflow_scroll()
            .child(surface)
    }

    /// The round handle on a lit arrow that cuts it.
    fn link_handle(&self, link: &Link, at: (f32, f32), cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let size = 18.0;
        let (from, to) = (link.from.clone(), link.to.clone());
        div()
            .id(SharedString::from(format!(
                "pilus-canvas-cut-{}-{}",
                link.from, link.to
            )))
            .absolute()
            .left(px(at.0 - size / 2.0))
            .top(px(at.1 - size / 2.0))
            .w(px(size))
            .h(px(size))
            .flex()
            .items_center()
            .justify_center()
            .radius_full()
            .bg(theme.bg)
            .border_1()
            .border_color(theme.border_focus)
            .cursor_pointer()
            .child(icon(
                include_bytes!("../../assets/icons/x.svg"),
                size * 0.6,
                theme.muted,
            ))
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.remove_link(&from, &to, cx);
            }))
            .into_any_element()
    }

    /// One port on a card's edge: drag from it to another card to draw an
    /// arrow. The right one makes this card the one waited on; the left one
    /// makes it the one waiting.
    fn canvas_port(&self, id: &str, outgoing: bool, rect: Rect) -> Div {
        let theme = self.theme;
        let hit = 22.0;
        let dot = 10.0;
        let drawing = self
            .tasks_panel
            .canvas
            .wire
            .as_ref()
            .is_some_and(|wire| wire.from == id && wire.outgoing == outgoing);
        let edge = if outgoing { rect.w } else { 0.0 };
        let drag = LinkDrag {
            id: id.to_string(),
            outgoing,
        };
        // The hover fill makes its element relative, so the port sits in a
        // wrapper that does the placing.
        div()
            .absolute()
            .left(px(edge - hit / 2.0))
            .top(px(rect.h / 2.0 - hit / 2.0))
            .w(px(hit))
            .h(px(hit))
            .child(
                div()
                    .id(SharedString::from(format!(
                        "pilus-canvas-port-{}-{id}",
                        if outgoing { "out" } else { "in" }
                    )))
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_crosshair()
                    .hover_fill(Hsla::from(theme.accent).opacity(0.25), Radius::Full)
                    .child(
                        div()
                            .w(px(dot))
                            .h(px(dot))
                            .radius_full()
                            .border_1()
                            .border_color(if drawing {
                                theme.accent
                            } else {
                                theme.border_focus
                            })
                            .bg(if drawing { theme.accent } else { theme.panel }),
                    )
                    .on_drag(drag, |_, _, _, cx| cx.new(|_| DragGhost)),
            )
    }

    /// One task card on the canvas: the card itself, its two ports, and the
    /// list of whichever picker is open on it.
    fn canvas_card(
        &self,
        task: &Task,
        rect: Rect,
        waiting: &[&str],
        key: &str,
        cx: &mut Context<Self>,
    ) -> Div {
        let id = task.id.clone();
        let mut wrapper = div()
            .absolute()
            .left(px(rect.x))
            .top(px(rect.y))
            .w(px(rect.w))
            .h(px(rect.h))
            .child(self.board_card(task, waiting, rect, key, cx))
            .child(self.canvas_port(&id, false, rect))
            .child(self.canvas_port(&id, true, rect));
        if let Some((open, kind)) = &self.tasks_panel.canvas.menu
            && *open == id
        {
            wrapper = wrapper.child(self.pick_menu(task, *kind, rect, key, cx));
        }
        wrapper
    }

    /// The list under a card's workspace or agent chip.
    fn pick_menu(
        &self,
        task: &Task,
        kind: PickKind,
        rect: Rect,
        key: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let picked = self
            .tasks_panel
            .canvas
            .pick(key, &task.id)
            .cloned()
            .unwrap_or_default();
        // Each row: its label, the value it sets, and whether it is the
        // current one.
        let rows: Vec<(String, Option<String>, bool)> = match kind {
            PickKind::Workspace => {
                let current = picked.workspace.clone().or(task.workspace_id.clone());
                self.endpoint_workspaces(&self.scope_machine())
                    .iter()
                    .map(|workspace| {
                        (
                            workspace.name.clone(),
                            Some(workspace.id.clone()),
                            current.as_deref() == Some(workspace.id.as_str()),
                        )
                    })
                    .collect()
            }
            PickKind::Agent => {
                let mut rows = vec![("Any agent".to_string(), None, picked.agent.is_none())];
                rows.extend(
                    self.agent_config_cache
                        .get(key)
                        .map(Vec::as_slice)
                        .unwrap_or_default()
                        .iter()
                        .map(|config| {
                            (
                                config.label.clone(),
                                Some(config.kind.clone()),
                                picked.agent.as_deref() == Some(config.kind.as_str()),
                            )
                        }),
                );
                rows
            }
        };
        let mut list = picker_menu(format!("pilus-canvas-menu-{}", task.id), theme)
            .max_h(px(PICKER_MENU_MAX_H))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.tasks_panel.canvas.menu = None;
                cx.notify();
            }));
        if rows.is_empty() {
            list = list.child(
                div()
                    .px_3()
                    .py_3()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(match kind {
                        PickKind::Workspace => "No workspaces",
                        PickKind::Agent => "Looking for agents…",
                    }),
            );
        }
        for (index, (label, value, active)) in rows.into_iter().enumerate() {
            let id = task.id.clone();
            list = list.child(picker_row(
                format!("pilus-canvas-option-{}-{index}", task.id),
                label,
                active,
                theme,
                fonts.body,
                cx.listener(move |this, _, _, cx| {
                    this.choose_pick(&id, kind, value.clone(), cx);
                }),
            ));
        }
        div()
            .absolute()
            .left(px(8.0))
            .top(px(rect.h - 2.0))
            .child(
                deferred(
                    anchored()
                        .anchor(Anchor::TopLeft)
                        .snap_to_window_with_margin(px(8.0))
                        .child(style::enter(
                            SharedString::from(format!("pilus-canvas-menu-enter-{}", task.id)),
                            list,
                        )),
                )
                .with_priority(2),
            )
            .into_any_element()
    }

    /// The agent and workspace a card shows: what a queued card will start
    /// with, or who runs it and where.
    fn card_footer(&self, task: &Task, waiting: &[&str], key: &str, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        // Only a card that has not started can still be pointed elsewhere.
        let pickable = task.status() == TaskStatus::Queued;
        let picked = self
            .tasks_panel
            .canvas
            .pick(key, &task.id)
            .cloned()
            .unwrap_or_default();

        let (agent_mark, agent_label, agent_color) = if pickable {
            let config = picked.agent.as_deref().map(|kind| {
                self.agent_config_cache
                    .get(key)
                    .and_then(|configs| configs.iter().find(|config| config.kind == kind))
                    .map_or_else(|| kind.to_string(), |config| config.label.clone())
            });
            match config {
                Some(label) => (
                    agent_icon(picked.agent.as_deref().unwrap_or_default()),
                    label,
                    theme.panel_fg,
                ),
                None => (agent_icon(""), "Any agent".to_string(), theme.muted),
            }
        } else {
            match self.task_agent(task) {
                Some(agent) => (agent_icon(&agent.agent), agent_name(agent), theme.panel_fg),
                None if task.assignee.is_some() => (
                    agent_icon(""),
                    format!("pane {}", task.assignee.unwrap_or_default()),
                    theme.panel_fg,
                ),
                None => (agent_icon(""), "Unassigned".to_string(), theme.muted),
            }
        };
        let mut agent = div()
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
            );
        let mut footer = div().flex().flex_row().items_center().gap_2();
        let workspace = match picked.workspace.as_deref() {
            Some(chosen) if pickable => self
                .endpoint_workspaces(&self.scope_machine())
                .iter()
                .find(|workspace| workspace.id == chosen)
                .map(|workspace| workspace.name.clone()),
            _ => None,
        }
        .or_else(|| self.task_workspace_name(task));
        let chevron = |color| {
            icon(
                include_bytes!("../../assets/icons/chevron-down.svg"),
                fonts.label,
                color,
            )
        };
        if pickable {
            let id = task.id.clone();
            agent = agent.child(chevron(theme.muted));
            footer = footer.child(
                div()
                    .id(SharedString::from(format!(
                        "pilus-canvas-agent-{}",
                        task.id
                    )))
                    .flex()
                    .flex_1()
                    .min_w(px(0.0))
                    .cursor_pointer()
                    .child(agent)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.toggle_pick_menu(&id, PickKind::Agent, cx);
                    })),
            );
        } else {
            footer = footer.child(agent);
        }
        if let Some(workspace) = workspace {
            let chip = style::chip(
                theme,
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .child(workspace)
                    .children(pickable.then(|| chevron(theme.muted))),
                fonts.label,
            );
            if pickable {
                let id = task.id.clone();
                footer = footer.child(
                    div()
                        .id(SharedString::from(format!(
                            "pilus-canvas-workspace-{}",
                            task.id
                        )))
                        .cursor_pointer()
                        .child(chip)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.toggle_pick_menu(&id, PickKind::Workspace, cx);
                        })),
                );
            } else {
                footer = footer.child(chip);
            }
        }
        if !waiting.is_empty() {
            footer = footer.child(blocked_badge(waiting, theme, fonts.label));
        }
        footer
    }

    /// One task card: status and id, title, description, then who runs it,
    /// where, and what it is waiting on. Dragging it moves it.
    fn board_card(
        &self,
        task: &Task,
        waiting: &[&str],
        rect: Rect,
        key: &str,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let geometry = self.canvas_geometry();
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

        let footer = self.card_footer(task, waiting, key, cx);

        let id = task.id.clone();
        let me = task.id.clone();
        let target = task.id.clone();
        let accent = theme.accent;
        style::board_card(format!("pilus-board-card-{}", task.id), selected, theme)
            .w_full()
            .h(px(rect.h))
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
            // An arrow dragged over a card lights its frame.
            .drag_over::<LinkDrag>(move |style, drag, _, _| {
                if drag.id == me {
                    style
                } else {
                    style.border_color(accent)
                }
            })
            .on_drop(cx.listener(move |this, drag: &LinkDrag, _, cx| {
                this.draw_link(drag, &target, cx);
            }))
            .on_drag(CardDrag { id: id.clone() }, |_, _, _, cx| {
                cx.new(|_| DragGhost)
            })
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
    use std::collections::HashMap;

    use pilus_client::{Task, TaskStatus};

    use super::{
        CanvasFile, Geometry, Link, MachineCanvas, Pick, Rect, auto_positions, closes_loop,
        connector, depths, extent, links, rects, step, waiting_on,
    };

    fn task(id: &str, status: TaskStatus, deps: &[&str]) -> Task {
        Task {
            id: id.to_string(),
            status: Some(status),
            deps: deps.iter().map(|dep| dep.to_string()).collect(),
            ..Task::default()
        }
    }

    fn refs(tasks: &[Task]) -> Vec<&Task> {
        tasks.iter().collect()
    }

    fn card(id: &str, x: f32, y: f32) -> (String, Rect) {
        (
            id.to_string(),
            Rect {
                x,
                y,
                w: 100.0,
                h: 60.0,
            },
        )
    }

    #[test]
    fn a_chain_reads_left_to_right_one_column_per_depth() {
        let tasks = [
            task("t9", TaskStatus::Queued, &["t2"]),
            task("t2", TaskStatus::Queued, &["t1"]),
            task("t10", TaskStatus::Queued, &[]),
            task("t1", TaskStatus::Running, &[]),
        ];
        let g = Geometry::new(|design| design);
        let at = auto_positions(&refs(&tasks), &g);
        let column = |id: &str| (at[id].0 - g.pad) / (g.card_w + g.gap_x);
        assert_eq!(column("t1"), 0.0);
        assert_eq!(column("t10"), 0.0);
        assert_eq!(column("t2"), 1.0);
        assert_eq!(column("t9"), 2.0);
        // Within a column, ids sort numerically: t1 above t10.
        assert!(at["t1"].1 < at["t10"].1);
        assert_eq!(depths(&refs(&tasks))["t9"], 2);
    }

    #[test]
    fn links_join_cards_and_mark_merged_ones_settled() {
        let tasks = [
            task("t1", TaskStatus::Merged, &[]),
            task("t2", TaskStatus::Running, &["t1"]),
            task("t3", TaskStatus::Queued, &["t2", "t7"]),
        ];
        let all = links(&refs(&tasks), &[]);
        assert_eq!(all.len(), 2, "t7 is not on the canvas");
        let settled = all.iter().find(|link| link.from == "t1").expect("t1");
        assert!(settled.settled && !settled.planned);
        assert!(!all.iter().any(|link| link.from == "t2" && link.settled));
    }

    #[test]
    fn a_drawn_arrow_the_daemon_holds_is_not_drawn_twice() {
        let tasks = [
            task("t1", TaskStatus::Running, &[]),
            task("t2", TaskStatus::Queued, &["t1"]),
            task("t3", TaskStatus::Queued, &[]),
        ];
        let planned = [
            ("t1".to_string(), "t2".to_string()),
            ("t2".to_string(), "t3".to_string()),
            ("t2".to_string(), "t9".to_string()),
        ];
        let all = links(&refs(&tasks), &planned);
        assert_eq!(all.len(), 2);
        assert!(
            all.iter()
                .any(|link| link.from == "t2" && link.to == "t3" && link.planned)
        );
        assert!(
            all.iter()
                .any(|link| link.from == "t1" && link.to == "t2" && !link.planned)
        );
    }

    #[test]
    fn an_arrow_that_would_close_a_loop_is_refused() {
        let link = |from: &str, to: &str| Link {
            from: from.to_string(),
            to: to.to_string(),
            settled: false,
            planned: false,
        };
        let chain = [link("t1", "t2"), link("t2", "t3")];
        assert!(closes_loop(&chain, "t3", "t1"));
        assert!(closes_loop(&chain, "t2", "t1"));
        assert!(closes_loop(&chain, "t2", "t2"));
        assert!(!closes_loop(&chain, "t1", "t3"));
        assert!(!closes_loop(&chain, "t4", "t1"));
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
    fn arrows_pick_the_nearest_card_that_way() {
        let cards = [
            card("a", 0.0, 0.0),
            card("b", 200.0, 0.0),
            card("c", 200.0, 200.0),
            card("d", 400.0, 10.0),
        ];
        assert_eq!(step(&cards, None, 1.0, 0.0).as_deref(), Some("a"));
        assert_eq!(step(&cards, Some("a"), 1.0, 0.0).as_deref(), Some("b"));
        assert_eq!(step(&cards, Some("b"), 1.0, 0.0).as_deref(), Some("d"));
        assert_eq!(step(&cards, Some("b"), 0.0, 1.0).as_deref(), Some("c"));
        // Nothing lies up or left of a: the selection stays.
        assert_eq!(step(&cards, Some("a"), -1.0, 0.0).as_deref(), Some("a"));
        assert_eq!(step(&cards, Some("a"), 0.0, -1.0).as_deref(), Some("a"));
        assert_eq!(step(&cards, Some("c"), 0.0, -1.0).as_deref(), Some("b"));
        assert!(step(&[], None, 1.0, 0.0).is_none());
    }

    #[test]
    fn a_line_leaves_the_side_facing_the_other_card() {
        let left = Rect {
            x: 0.0,
            y: 0.0,
            w: 100.0,
            h: 60.0,
        };
        let right = Rect {
            x: 300.0,
            y: 100.0,
            w: 100.0,
            h: 60.0,
        };
        let line = connector(left, right, gpui::Hsla::default(), false);
        assert_eq!(line.from, (100.0, 30.0));
        assert_eq!(line.to, (300.0, 130.0));
        let back = connector(right, left, gpui::Hsla::default(), false);
        assert_eq!(back.from, (300.0, 130.0));
        assert_eq!(back.to, (100.0, 30.0));
        // Stacked cards bow out of both right edges.
        let below = Rect { y: 200.0, ..left };
        let bow = connector(left, below, gpui::Hsla::default(), false);
        assert_eq!((bow.from.0, bow.to.0), (100.0, 100.0));
    }

    #[test]
    fn folded_cards_take_less_of_the_canvas() {
        let tasks = [task("t1", TaskStatus::Queued, &[])];
        let g = Geometry::new(|design| design);
        let positions = HashMap::from([("t1".to_string(), (500.0, 400.0))]);
        let open = extent(&rects(&refs(&tasks), &positions, &g, |_| g.card_h), &g);
        let folded = extent(&rects(&refs(&tasks), &positions, &g, |_| g.folded_h), &g);
        assert_eq!(open.0, 500.0 + g.card_w + g.pad);
        assert!(folded.1 < open.1);
        // An empty canvas still has room for a card.
        assert_eq!(
            extent(&[], &g),
            (g.card_w + 2.0 * g.pad, g.card_h + 2.0 * g.pad)
        );
    }

    #[test]
    fn the_layout_file_round_trips_and_forgives_missing_fields() {
        let mut file = CanvasFile::default();
        let machine = MachineCanvas {
            positions: [("t1".to_string(), (10.0, 20.0))].into(),
            links: vec![("t1".to_string(), "t2".to_string())],
            picks: [(
                "t2".to_string(),
                Pick {
                    workspace: Some("w1".to_string()),
                    agent: None,
                },
            )]
            .into(),
        };
        file.machines.insert("local".to_string(), machine);
        let text = serde_json::to_string(&file).expect("serialize");
        let back: CanvasFile = serde_json::from_str(&text).expect("parse");
        let local = &back.machines["local"];
        assert_eq!(local.positions["t1"], (10.0, 20.0));
        assert_eq!(local.links, [("t1".to_string(), "t2".to_string())]);
        assert_eq!(local.picks["t2"].workspace.as_deref(), Some("w1"));
        // A file from before a field existed still loads.
        let old: CanvasFile =
            serde_json::from_str(r#"{"machines":{"m":{"positions":{}}}}"#).expect("old file");
        assert!(old.machines["m"].links.is_empty());
        assert!(serde_json::from_str::<CanvasFile>("{}").is_ok());
    }

    #[test]
    fn a_cycle_ends_the_chain() {
        let tasks = [
            task("t1", TaskStatus::Queued, &["t2"]),
            task("t2", TaskStatus::Queued, &["t1"]),
        ];
        let depth = depths(&refs(&tasks));
        assert!(depth["t1"] <= 2 && depth["t2"] <= 2);
    }
}
