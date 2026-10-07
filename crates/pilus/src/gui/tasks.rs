//! The native Tasks page: the daemon's orchestration queue as a list.
//!
//! Tasks arrive on the same feed as agents and tabs (`FeedUpdate::tasks`), so
//! this page owns no polling. This module holds the page's state and the pure
//! grouping and navigation rules, so they stay unit-testable without GPUI or
//! a daemon; the second half is the window's rendering, which paints that
//! state and sends every mutation to the daemon off the GUI thread. The page
//! never edits a task locally: the daemon answers with `task.*` events and
//! the next feed update redraws.

use std::collections::{HashMap, HashSet};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use gpui::{
    AnyElement, Context, Div, Entity, FontWeight, Hsla, KeyDownEvent, MouseButton, SharedString,
    Stateful, div, prelude::*, px,
};
use pilus_client::{NewTask, Task, TaskAction, TaskStatus};

use super::orchestration::{Board, Lane, agent_name, waiting_on};
use super::style::{self, ButtonKind, FillExt};
use super::task_compose::{ComposeState, Fetched, Scope, TaskPicker, TasksMsg};
use super::text_input::TextInput;
use super::theme::Theme;
use super::{EndpointId, PilusView, agent_icon, icon, terminal_inset_border};

/// Which slice of the queue the list shows.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum TaskView {
    #[default]
    All,
    /// Tasks that still need attention.
    Active,
    /// Done, merged, and failed.
    Closed,
}

impl TaskView {
    const ALL: [TaskView; 3] = [TaskView::All, TaskView::Active, TaskView::Closed];

    fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Active => "Active",
            Self::Closed => "Closed",
        }
    }

    pub(super) fn admits(self, task: &Task) -> bool {
        match self {
            Self::All => true,
            Self::Active => task.status().is_open(),
            Self::Closed => !task.status().is_open(),
        }
    }
}

/// How the page lays the queue out: the Tasks list or the Orchestration
/// board. Both read the same state, so switching keeps the selection.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum TaskLayout {
    #[default]
    List,
    Board,
}

/// The composer's fields, in Tab order.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum ComposeField {
    #[default]
    Title,
    Prompt,
    /// One of the pickers: no text to edit, so the window holds the keys.
    Pick(TaskPicker),
    Gate,
}

impl ComposeField {
    const ALL: [ComposeField; 7] = [
        ComposeField::Title,
        ComposeField::Prompt,
        ComposeField::Pick(TaskPicker::Machine),
        ComposeField::Pick(TaskPicker::Workspace),
        ComposeField::Pick(TaskPicker::Deps),
        ComposeField::Pick(TaskPicker::Paths),
        ComposeField::Gate,
    ];

    fn step(self, backwards: bool) -> Self {
        let at = Self::ALL
            .iter()
            .position(|field| *field == self)
            .unwrap_or(0);
        let len = Self::ALL.len();
        Self::ALL[(at + if backwards { len - 1 } else { 1 }) % len]
    }
}

/// What a detail-pane button asks the daemon to do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TaskOp {
    Start,
    Lifecycle(TaskAction),
}

impl TaskOp {
    fn label(self) -> &'static str {
        match self {
            Self::Start => "Start",
            Self::Lifecycle(TaskAction::Done) => "Run gate & finish",
            Self::Lifecycle(TaskAction::Merge) => "Merge",
            Self::Lifecycle(TaskAction::Retry) => "Retry",
            Self::Lifecycle(TaskAction::Release) => "Release",
            Self::Lifecycle(TaskAction::Delete) => "Delete",
        }
    }

    /// Past-tense status line once the daemon accepted it.
    fn done_text(self, id: &str) -> String {
        match self {
            Self::Start => format!("Started {id}"),
            Self::Lifecycle(TaskAction::Done) => format!("{id} finished"),
            Self::Lifecycle(TaskAction::Merge) => format!("Merged {id}"),
            Self::Lifecycle(TaskAction::Retry) => format!("Retrying {id}"),
            Self::Lifecycle(TaskAction::Release) => format!("Released {id}"),
            Self::Lifecycle(TaskAction::Delete) => format!("Deleted {id}"),
        }
    }
}

/// The operations a task in `status` offers, primary first.
///
/// Delete is not listed: every task has it, and it asks for confirmation.
fn ops_for(status: TaskStatus) -> &'static [TaskOp] {
    use TaskAction::{Done, Merge, Release, Retry};
    match status {
        TaskStatus::Queued => &[TaskOp::Start],
        TaskStatus::Claimed => &[TaskOp::Lifecycle(Release)],
        TaskStatus::Running => &[TaskOp::Lifecycle(Done), TaskOp::Lifecycle(Release)],
        TaskStatus::Review => &[TaskOp::Lifecycle(Done), TaskOp::Lifecycle(Retry)],
        TaskStatus::Blocked => &[TaskOp::Lifecycle(Retry), TaskOp::Lifecycle(Release)],
        TaskStatus::Done => &[TaskOp::Lifecycle(Merge)],
        TaskStatus::Failed => &[TaskOp::Lifecycle(Retry)],
        TaskStatus::Merging | TaskStatus::Merged => &[],
    }
}

/// The page's own state; the tasks themselves live on the window's feed.
#[derive(Debug, Default)]
pub(super) struct TasksPanel {
    open: bool,
    layout: TaskLayout,
    pub(super) view: TaskView,
    pub(super) selected: Option<String>,
    composing: bool,
    pub(super) compose_focus: ComposeField,
    pub(super) compose: ComposeState,
    collapsed: HashSet<TaskStatus>,
    /// Board cards folded down to their title.
    pub(super) folded: HashSet<String>,
    /// When each card last folded or unfolded, so its height eases.
    pub(super) fold_turned: HashMap<String, Instant>,
    /// Board lanes folded down to a strip.
    pub(super) folded_lanes: HashSet<Lane>,
    /// When each lane last folded or unfolded, so its width eases.
    pub(super) lane_turned: HashMap<Lane, Instant>,
    /// The group whose chevron was just toggled, so only it animates.
    toggled: Option<TaskStatus>,
    pub(super) confirm_delete: Option<String>,
    pub(super) status: String,
    /// Which machine and workspace the list shows.
    pub(super) scope: Scope,
    /// Other machines' tasks, as last fetched.
    pub(super) remote: HashMap<EndpointId, Fetched>,
}

impl TasksPanel {
    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    pub(super) fn open_as(&mut self, layout: TaskLayout) {
        self.open = true;
        self.layout = layout;
    }

    pub(super) fn layout(&self) -> TaskLayout {
        self.layout
    }

    pub(super) fn close_composer(&mut self) {
        self.composing = false;
    }

    pub(super) fn close(&mut self) {
        *self = Self {
            view: self.view,
            collapsed: std::mem::take(&mut self.collapsed),
            folded: std::mem::take(&mut self.folded),
            fold_turned: std::mem::take(&mut self.fold_turned),
            folded_lanes: std::mem::take(&mut self.folded_lanes),
            lane_turned: std::mem::take(&mut self.lane_turned),
            scope: std::mem::take(&mut self.scope),
            remote: std::mem::take(&mut self.remote),
            ..Self::default()
        };
    }

    pub(super) fn composing(&self) -> bool {
        self.composing
    }

    pub(super) fn compose_focus(&self) -> ComposeField {
        self.compose_focus
    }
}

/// The tasks `view` admits, grouped by status in board order.
fn grouped(tasks: &[Task], view: TaskView) -> Vec<(TaskStatus, Vec<&Task>)> {
    TaskStatus::ALL
        .into_iter()
        .filter_map(|status| {
            let rows: Vec<&Task> = tasks
                .iter()
                .filter(|task| task.status() == status && view.admits(task))
                .collect();
            (!rows.is_empty()).then_some((status, rows))
        })
        .collect()
}

/// The ids keyboard navigation walks, top to bottom: every row of every
/// expanded group.
fn nav_order<'a>(
    groups: &'a [(TaskStatus, Vec<&'a Task>)],
    collapsed: &HashSet<TaskStatus>,
) -> Vec<&'a str> {
    groups
        .iter()
        .filter(|(status, _)| !collapsed.contains(status))
        .flat_map(|(_, rows)| rows.iter().map(|task| task.id.as_str()))
        .collect()
}

/// The id `delta` rows from `current`, clamped to the list. With nothing
/// selected, moving down picks the first row and moving up the last.
fn step_selection(order: &[&str], current: Option<&str>, delta: isize) -> Option<String> {
    if order.is_empty() {
        return None;
    }
    let at = current.and_then(|id| order.iter().position(|row| *row == id));
    let next = match at {
        Some(at) => (at as isize + delta).clamp(0, order.len() as isize - 1) as usize,
        None if delta < 0 => order.len() - 1,
        None => 0,
    };
    Some(order[next].to_string())
}

/// A compact age such as `now`, `5m`, `3h`, `2d`, or `6w`.
pub(super) fn age_label(now: u64, then: u64) -> String {
    let secs = now.saturating_sub(then);
    match secs {
        0..=59 => "now".to_string(),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        86_400..=604_799 => format!("{}d", secs / 86_400),
        _ => format!("{}w", secs / 604_800),
    }
}

pub(super) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// The glyph and tint for a status.
pub(super) fn status_glyph(status: TaskStatus, size: f32, theme: Theme) -> impl IntoElement {
    let (bytes, color): (&'static [u8], _) = match status {
        TaskStatus::Queued => (
            include_bytes!("../../assets/icons/task-queued.svg"),
            theme.muted,
        ),
        TaskStatus::Claimed => (
            include_bytes!("../../assets/icons/task-claimed.svg"),
            theme.panel_fg,
        ),
        TaskStatus::Running => (
            include_bytes!("../../assets/icons/task-running.svg"),
            theme.accent,
        ),
        TaskStatus::Review => (
            include_bytes!("../../assets/icons/task-review.svg"),
            theme.branch,
        ),
        TaskStatus::Blocked => (
            include_bytes!("../../assets/icons/task-blocked.svg"),
            theme.alert,
        ),
        TaskStatus::Done => (include_bytes!("../../assets/icons/task-done.svg"), theme.ok),
        TaskStatus::Merging => (
            include_bytes!("../../assets/icons/task-merging.svg"),
            theme.branch,
        ),
        TaskStatus::Merged => (
            include_bytes!("../../assets/icons/task-done.svg"),
            theme.branch,
        ),
        TaskStatus::Failed => (
            include_bytes!("../../assets/icons/task-failed.svg"),
            theme.alert,
        ),
    };
    icon(bytes, size, color)
}

/// The composer's text fields. One entity per field keeps each caret and
/// selection independent while Tab moves between them.
pub(super) struct TasksInputs {
    title: Entity<TextInput>,
    prompt: Entity<TextInput>,
    gate: Entity<TextInput>,
}

impl TasksInputs {
    pub(super) fn new(
        font: gpui::Font,
        size: f32,
        theme: Theme,
        cx: &mut Context<PilusView>,
    ) -> Self {
        let mut field = |placeholder: &'static str, multiline: bool| {
            let font = font.clone();
            cx.new(move |cx| {
                let input = TextInput::new(
                    String::new(),
                    placeholder,
                    font,
                    px(size),
                    multiline,
                    theme,
                    cx,
                );
                if multiline {
                    input
                } else {
                    input.submit_on_enter()
                }
            })
        };
        Self {
            title: field("Short summary", false),
            prompt: field("What should the agent do?", true),
            gate: field("Command that must pass", false),
        }
    }

    /// The text field behind `field`; the pickers have none.
    fn entity(&self, field: ComposeField) -> Option<&Entity<TextInput>> {
        match field {
            ComposeField::Title => Some(&self.title),
            ComposeField::Prompt => Some(&self.prompt),
            ComposeField::Gate => Some(&self.gate),
            ComposeField::Pick(_) => None,
        }
    }

    pub(super) fn focus_handle(
        &self,
        field: ComposeField,
        cx: &gpui::App,
    ) -> Option<gpui::FocusHandle> {
        self.entity(field)
            .map(|entity| gpui::Focusable::focus_handle(entity, cx))
    }
}

const DETAIL_WIDTH: f32 = 400.0;
/// The composer pane is wider: its pickers sit two to a row.
const COMPOSE_WIDTH: f32 = 460.0;
const ROW_HEIGHT: f32 = 36.0;
const GROUP_HEIGHT: f32 = 32.0;

impl PilusView {
    /// Open the page from the sidebar.
    pub(super) fn open_tasks(&mut self, cx: &mut Context<Self>) {
        self.leave_settings_page();
        self.picker.close();
        self.tasks_panel.open_as(TaskLayout::List);
        cx.notify();
    }

    /// Shot-harness entry points.
    #[cfg(feature = "visual-test")]
    pub(super) fn tasks_panel_select(&mut self, id: &str, cx: &mut Context<Self>) {
        self.select_task(Some(id.to_string()), cx);
    }

    #[cfg(feature = "visual-test")]
    pub(super) fn tasks_panel_pick(&mut self, picker: TaskPicker, cx: &mut Context<Self>) {
        self.toggle_task_dropdown(picker, cx);
    }

    #[cfg(feature = "visual-test")]
    pub(super) fn tasks_panel_compose(&mut self, cx: &mut Context<Self>) {
        self.open_task_composer(cx);
    }

    fn select_task(&mut self, id: Option<String>, cx: &mut Context<Self>) {
        self.tasks_panel.confirm_delete = None;
        self.tasks_panel.selected = id;
        cx.notify();
    }

    fn open_task_composer(&mut self, cx: &mut Context<Self>) {
        for entity in ComposeField::ALL
            .into_iter()
            .filter_map(|field| self.tasks_inputs.entity(field))
        {
            entity.update(cx, |input, cx| input.clear(cx));
        }
        self.tasks_panel.composing = true;
        self.tasks_panel.compose_focus = ComposeField::Title;
        // Start where the list is looking: the same machine and workspace.
        self.tasks_panel.compose =
            ComposeState::at(self.scope_machine(), self.scope_workspace_id());
        self.compose_sync();
        cx.notify();
    }

    /// The id of the workspace the window is showing: the fallback home for a
    /// task that predates `project`.
    pub(super) fn active_workspace_id(&self) -> Option<String> {
        self.surface
            .workspaces()
            .iter()
            .find(|workspace| workspace.active)
            .map(|workspace| workspace.id.clone())
    }

    /// Queue the composer's task on the picked machine and workspace. The
    /// feed delivers it back as a row.
    fn submit_task(&mut self, cx: &mut Context<Self>) {
        let read = |field: ComposeField, this: &Self| {
            this.tasks_inputs
                .entity(field)
                .map(|entity| entity.read(cx).value())
                .unwrap_or_default()
        };
        let title = read(ComposeField::Title, self).trim().to_string();
        if title.is_empty() {
            self.tasks_panel.status = "A task needs a title".to_string();
            cx.notify();
            return;
        }
        let compose = &self.tasks_panel.compose;
        let task = NewTask {
            title,
            prompt: read(ComposeField::Prompt, self).trim().to_string(),
            paths: compose.paths.clone(),
            deps: compose.deps.clone(),
            gate: read(ComposeField::Gate, self).trim().to_string(),
        };
        let Some(workspace) = self.compose_workspace() else {
            self.tasks_panel.status = "Open a workspace to queue a task".to_string();
            cx.notify();
            return;
        };
        let machine = self.compose_machine();
        self.tasks_panel.composing = false;
        self.tasks_panel.status = "Adding task…".to_string();
        let tx = self.tasks_op_tx.clone();
        let client = self.control_client_for_endpoint(&machine);
        std::thread::spawn(move || {
            let message = match client.map(|mut client| client.add_task(&task, &workspace.id)) {
                Some(Ok(Some(task))) => format!("Added {}", task.id),
                Some(Ok(None)) => "Added task".to_string(),
                Some(Err(error)) => format!("Could not add task: {error}"),
                None => "Could not reach the daemon".to_string(),
            };
            tx.send(TasksMsg::Status(message)).ok();
        });
        cx.notify();
    }

    fn run_task_op(&mut self, id: String, op: TaskOp, cx: &mut Context<Self>) {
        self.tasks_panel.confirm_delete = None;
        // A task starts in the workspace it was queued in, wherever the
        // window is looking now.
        let workspace_id = self
            .scope_tasks()
            .iter()
            .find(|task| task.id == id)
            .and_then(|task| task.workspace_id.clone())
            .or_else(|| self.active_workspace_id());
        if op == TaskOp::Start && workspace_id.is_none() {
            self.tasks_panel.status = "Open a workspace to start a task".to_string();
            cx.notify();
            return;
        }
        self.tasks_panel.status = format!("{} {id}…", op.label());
        let tx = self.tasks_op_tx.clone();
        let client = self.control_client_for_endpoint(&self.scope_machine());
        std::thread::spawn(move || {
            let result = client.map(|mut client| match op {
                TaskOp::Start => client.start_task(
                    &id,
                    None,
                    "worktree",
                    workspace_id.as_deref().unwrap_or_default(),
                ),
                TaskOp::Lifecycle(action) => client.task_action(action, &id),
            });
            let message = match result {
                Some(Ok(())) => op.done_text(&id),
                Some(Err(error)) => format!("{} failed: {error}", op.label()),
                None => "Could not reach the daemon".to_string(),
            };
            tx.send(TasksMsg::Status(message)).ok();
        });
        cx.notify();
    }

    /// Keys the focused composer field declines, and the list's shortcuts.
    pub(super) fn tasks_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let mods = event.keystroke.modifiers;
        // An open picker list owns the keyboard until it closes.
        if let Some(picker) = self.tasks_panel.compose.dropdown() {
            self.task_dropdown_key(picker, event, cx);
            return;
        }
        if self.tasks_panel.composing {
            let focus = self.tasks_panel.compose_focus;
            match key {
                "escape" => self.tasks_panel.composing = false,
                "tab" => self.tasks_panel.compose_focus = focus.step(mods.shift),
                "enter" if mods.platform => self.submit_task(cx),
                "enter" | "space" if matches!(focus, ComposeField::Pick(_)) => {
                    if let ComposeField::Pick(picker) = focus {
                        self.open_task_dropdown(picker);
                    }
                }
                "enter" if focus == ComposeField::Title => {
                    self.tasks_panel.compose_focus = ComposeField::Prompt;
                }
                "enter" if focus == ComposeField::Gate => self.submit_task(cx),
                _ => {}
            }
            cx.notify();
            return;
        }
        let tasks = self.scope_tasks();
        if self.tasks_panel.layout == TaskLayout::Board && self.board_key(key, &tasks) {
            cx.notify();
            return;
        }
        let groups = grouped(&tasks, self.tasks_panel.view);
        let order = nav_order(&groups, &self.tasks_panel.collapsed);
        let current = self.tasks_panel.selected.as_deref();
        match key {
            "escape" => {
                if self.tasks_panel.confirm_delete.take().is_some() {
                } else if self.tasks_panel.selected.take().is_none() {
                    self.tasks_panel.close();
                }
            }
            "down" | "j" => self.tasks_panel.selected = step_selection(&order, current, 1),
            "up" | "k" => self.tasks_panel.selected = step_selection(&order, current, -1),
            "c" | "n" if !mods.platform && !mods.control => self.open_task_composer(cx),
            _ => {}
        }
        cx.notify();
    }

    /// The page, drawn in place of the terminal card.
    pub(super) fn tasks_page(&mut self, cx: &mut Context<Self>) -> Div {
        if self.tasks_panel.composing {
            self.compose_sync();
        }
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let tasks = self.scope_tasks();
        let groups = grouped(&tasks, self.tasks_panel.view);
        let selected = self
            .tasks_panel
            .selected
            .as_ref()
            .and_then(|id| tasks.iter().find(|task| task.id == *id))
            .cloned();

        let list = if !self.agents_loaded || self.scope_loading() {
            self.tasks_skeleton().into_any_element()
        } else if tasks.is_empty() {
            self.tasks_empty(cx).into_any_element()
        } else if groups.is_empty() {
            self.tasks_message("Nothing in this view")
                .into_any_element()
        } else if self.tasks_panel.layout == TaskLayout::Board {
            self.task_board(&Board::build(&tasks, self.tasks_panel.view), cx)
                .into_any_element()
        } else {
            self.tasks_list(&groups, cx).into_any_element()
        };

        let mut body = div()
            .flex()
            .flex_row()
            .flex_1()
            .min_h(px(0.0))
            .child(div().flex().flex_col().flex_1().min_w(px(0.0)).child(list));
        // The composer takes the side pane the detail view would use.
        if self.tasks_panel.composing {
            body = body.child(self.task_compose_pane(cx));
        } else if let Some(task) = selected {
            body = body.child(self.task_detail(&task, cx));
        }

        let footer = (!self.tasks_panel.status.is_empty()).then(|| {
            div()
                .px_4()
                .h(px(style::scaled(&self.shared_settings, 28.0)))
                .flex()
                .items_center()
                .border_t_1()
                .border_color(terminal_inset_border())
                .text_size(px(fonts.secondary))
                .text_color(theme.muted)
                .child(self.tasks_panel.status.clone())
        });

        div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .min_h(px(0.0))
            .child(self.tasks_header(&tasks, cx))
            .child(body)
            .children(footer)
    }

    fn tasks_header(&self, tasks: &[Task], cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let open = tasks.iter().filter(|task| task.status().is_open()).count();
        let (title, summary) = match self.tasks_panel.layout {
            TaskLayout::List => ("Tasks", format!("{open} open")),
            TaskLayout::Board => match self.waiting_count(tasks) {
                0 => ("Orchestration", format!("{open} open")),
                waiting => ("Orchestration", format!("{open} open · {waiting} waiting")),
            },
        };
        let board = self.tasks_panel.layout == TaskLayout::Board;
        let mut views = div().flex().flex_row().items_center().gap_1();
        // The board draws every lane itself, so the list's filters do not apply.
        for view in TaskView::ALL.into_iter().filter(|_| !board) {
            let kind = if self.tasks_panel.view == view {
                ButtonKind::Secondary
            } else {
                ButtonKind::Ghost
            };
            views = views.child(
                style::small_button(
                    format!("pilus-tasks-view-{}", view.label()),
                    view.label(),
                    kind,
                    theme,
                    fonts.secondary,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.tasks_panel.view = view;
                    cx.notify();
                })),
            );
        }
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
            .child(style::title(theme, title, fonts.title))
            .child(
                div()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(summary),
            )
            .child(self.task_scope_pickers(cx))
            .child(views)
            .child(div().flex_1())
            .child(
                style::small_button(
                    "pilus-tasks-new",
                    "New task",
                    ButtonKind::Primary,
                    theme,
                    fonts.secondary,
                )
                .on_click(cx.listener(|this, _, _, cx| this.open_task_composer(cx))),
            )
    }

    fn tasks_list(
        &self,
        groups: &[(TaskStatus, Vec<&Task>)],
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let mut column = div().flex().flex_col().gap_1().p_2();
        for (status, rows) in groups {
            let collapsed = self.tasks_panel.collapsed.contains(status);
            column = column.child(self.task_group_header(*status, rows.len(), collapsed, cx));
            if !collapsed {
                for task in rows {
                    column = column.child(self.task_row(task, cx));
                }
            }
        }
        div()
            .id("pilus-tasks-scroll")
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .child(column)
    }

    fn task_group_header(
        &self,
        status: TaskStatus,
        count: usize,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let animate = self.tasks_panel.toggled == Some(status);
        div()
            .id(format!("pilus-tasks-group-{}", status.wire()))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .h(px(style::scaled(&self.shared_settings, GROUP_HEIGHT)))
            .px_2()
            .rounded_md()
            .cursor_pointer()
            .hover_fill(theme.panel_dim, style::RADIUS_MD)
            .child(style::chevron(
                format!("pilus-tasks-chevron-{}", status.wire()),
                !collapsed,
                animate,
                fonts.secondary,
                theme.muted,
            ))
            .child(status_glyph(status, fonts.body, theme))
            .child(
                div()
                    .text_size(px(fonts.body))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.panel_fg)
                    .child(status.label()),
            )
            .child(
                div()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(count.to_string()),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                let panel = &mut this.tasks_panel;
                if !panel.collapsed.remove(&status) {
                    panel.collapsed.insert(status);
                }
                panel.toggled = Some(status);
                cx.notify();
            }))
    }

    fn task_row(&self, task: &Task, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let selected = self.tasks_panel.selected.as_deref() == Some(task.id.as_str());
        let id = task.id.clone();
        let rest: Hsla = if selected {
            theme.panel.into()
        } else {
            gpui::transparent_black()
        };
        let hover: Hsla = if selected {
            theme.panel.into()
        } else {
            theme.panel_dim.into()
        };
        let mut meta = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .flex_shrink_0();
        if let Some(dep) = task.deps.first() {
            let more = task.deps.len() - 1;
            let text = if more == 0 {
                format!("after {dep}")
            } else {
                format!("after {dep} +{more}")
            };
            meta = meta.child(style::chip(theme, text, fonts.label));
        }
        if let Some(path) = task.paths.first() {
            let more = task.paths.len() - 1;
            let text = if more == 0 {
                path.clone()
            } else {
                format!("{path} +{more}")
            };
            meta = meta.child(style::chip(theme, text, fonts.label));
        }
        if task.status() == TaskStatus::Running
            && let Some(percent) = task.context_percent
        {
            meta = meta.child(style::context_gauge(
                format!("pilus-task-gauge-{}", task.id),
                Some(f32::from(percent)),
                percent >= 85,
                theme,
                fonts.body,
            ));
        }
        if let Some(pane) = task.assignee {
            meta = meta.child(
                div()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(format!("pane {pane}")),
            );
        }
        meta = meta.child(
            div()
                .w(px(style::scaled(&self.shared_settings, 28.0)))
                .flex()
                .justify_end()
                .text_size(px(fonts.secondary))
                .text_color(theme.muted)
                .child(age_label(unix_now(), task.updated)),
        );
        div()
            .id(format!("pilus-task-{}", task.id))
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .h(px(style::scaled(&self.shared_settings, ROW_HEIGHT)))
            .px_2()
            .rounded_md()
            .cursor_pointer()
            .state_fill(rest, hover, style::RADIUS_MD)
            .child(status_glyph(task.status(), fonts.body, theme))
            .child(
                div()
                    .w(px(style::scaled(&self.shared_settings, 36.0)))
                    .flex_shrink_0()
                    .font_family(SharedString::from(style::mono_family(
                        &self.shared_settings,
                    )))
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(task.id.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(fonts.body))
                    .font_weight(if selected {
                        FontWeight::BOLD
                    } else {
                        FontWeight::NORMAL
                    })
                    .text_color(theme.panel_fg)
                    .child(task.title.clone()),
            )
            .child(meta)
            .on_click(cx.listener(move |this, _, _, cx| {
                let next =
                    (this.tasks_panel.selected.as_deref() != Some(id.as_str())).then(|| id.clone());
                // Looking at a task leaves the draft behind.
                this.tasks_panel.composing = false;
                this.select_task(next, cx);
            }))
    }

    fn tasks_skeleton(&self) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let mut column = div().flex().flex_col().gap_1().p_2();
        for (index, width) in [0.46, 0.62, 0.38, 0.55, 0.42].into_iter().enumerate() {
            column = column.child(
                div()
                    .flex()
                    .items_center()
                    .h(px(style::scaled(&self.shared_settings, ROW_HEIGHT)))
                    .px_2()
                    .child(style::skeleton(
                        format!("pilus-tasks-skeleton-{index}"),
                        theme,
                        gpui::relative(width),
                        fonts.body,
                    )),
            );
        }
        column
    }

    fn tasks_message(&self, text: &'static str) -> Div {
        div()
            .flex()
            .flex_1()
            .items_center()
            .justify_center()
            .text_size(px(style::Fonts::resolve(&self.shared_settings).body))
            .text_color(self.theme.muted)
            .child(text)
    }

    fn tasks_empty(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        div()
            .flex()
            .flex_col()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_3()
            .px_6()
            .child(icon(
                include_bytes!("../../assets/icons/list-checks.svg"),
                fonts.headline,
                theme.muted,
            ))
            .child(style::title(theme, "No tasks yet", fonts.title))
            .child(
                div()
                    .max_w(px(360.0))
                    .text_center()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(
                        "Queue work for your agents. Each task runs in its own worktree \
                         and merges once its gate passes.",
                    ),
            )
            .child(
                style::button(
                    "pilus-tasks-empty-new",
                    "New task",
                    ButtonKind::Primary,
                    theme,
                    fonts.secondary,
                )
                .on_click(cx.listener(|this, _, _, cx| this.open_task_composer(cx))),
            )
    }

    /// One labelled line of the detail pane's property table.
    fn task_property(&self, label: &'static str, value: impl IntoElement) -> Div {
        let fonts = style::Fonts::resolve(&self.shared_settings);
        div()
            .flex()
            .flex_row()
            .items_start()
            .gap_3()
            .min_h(px(style::scaled(&self.shared_settings, 24.0)))
            .child(
                div()
                    .w(px(style::scaled(&self.shared_settings, 76.0)))
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

    fn mono_block(&self, text: String) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        div()
            .p_3()
            .rounded_md()
            .bg(theme.panel_dim)
            .border_1()
            .border_color(theme.border)
            .font_family(SharedString::from(style::mono_family(
                &self.shared_settings,
            )))
            .text_size(px(fonts.secondary))
            .text_color(theme.panel_fg)
            .child(text)
    }

    fn task_detail(&self, task: &Task, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let status = task.status();
        let mut properties = div().flex().flex_col().child(
            self.task_property(
                "Status",
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(status_glyph(status, fonts.body, theme))
                    .child(status.label()),
            ),
        );
        if let Some(workspace) = self.task_workspace_name(task) {
            properties = properties.child(self.task_property("Workspace", workspace));
        } else if let Some(root) = &task.project_root {
            properties = properties.child(self.task_property("Workspace", root.clone()));
        }
        if let Some(mode) = &task.mode {
            properties = properties.child(self.task_property("Mode", mode.clone()));
        }
        if let Some(pane) = task.assignee {
            let worker = match self.task_agent(task) {
                Some(agent) => div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(icon(agent_icon(&agent.agent), fonts.body, theme.panel_fg))
                    .child(format!("{} · pane {pane}", agent_name(agent))),
                None => div().child(format!("pane {pane}")),
            };
            properties = properties.child(self.task_property("Agent", worker));
        }
        if let Some(branch) = &task.branch {
            properties = properties.child(self.task_property("Branch", branch.clone()));
        }
        if task.attempt > 1 {
            properties = properties.child(self.task_property("Attempt", task.attempt.to_string()));
        }
        if !task.deps.is_empty() {
            properties = properties.child(self.task_property("After", task.deps.join(", ")));
        }
        let known = self.machine_tasks(&self.scope_machine());
        let waiting = waiting_on(task, &known);
        if !waiting.is_empty() {
            properties = properties.child(
                self.task_property(
                    "Blocked by",
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_color(theme.alert)
                        .child(icon(
                            include_bytes!("../../assets/icons/task-blocked.svg"),
                            fonts.body,
                            theme.alert,
                        ))
                        .child(waiting.join(", ")),
                ),
            );
        }
        if !task.paths.is_empty() {
            let mut paths = div().flex().flex_row().flex_wrap().gap_1();
            for path in &task.paths {
                paths = paths.child(style::chip(theme, path.clone(), fonts.label));
            }
            properties = properties.child(self.task_property("Paths", paths));
        }
        if let Some(percent) = task.context_percent {
            properties = properties.child(self.task_property("Context", format!("{percent}%")));
        }
        properties = properties.child(self.task_property(
            "Updated",
            format!("{} ago", age_label(unix_now(), task.updated)),
        ));

        let mut sections = div()
            .flex()
            .flex_col()
            .gap_4()
            .p_4()
            .child(style::title(theme, task.title.clone(), fonts.title))
            .child(properties);
        if let Some(prompt) = &task.prompt {
            sections = sections.child(self.task_section("PROMPT", self.mono_block(prompt.clone())));
        }
        if let Some(gate) = &task.gate {
            sections = sections.child(self.task_section("GATE", self.mono_block(gate.clone())));
        }
        if let Some(output) = task.outputs.last() {
            sections = sections.child(self.task_section("OUTPUT", self.mono_block(output.clone())));
        }
        if !task.notes.is_empty() {
            sections =
                sections.child(self.task_section("NOTES", self.mono_block(task.notes.join("\n"))));
        }

        let mut actions = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_2()
            .p_3()
            .border_t_1()
            .border_color(terminal_inset_border());
        for (index, op) in ops_for(status).iter().copied().enumerate() {
            let id = task.id.clone();
            let kind = if index == 0 {
                ButtonKind::Primary
            } else {
                ButtonKind::Secondary
            };
            actions = actions.child(
                style::small_button(
                    format!("pilus-task-op-{}", op.label()),
                    op.label(),
                    kind,
                    theme,
                    fonts.secondary,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.run_task_op(id.clone(), op, cx))),
            );
        }
        let confirming = self.tasks_panel.confirm_delete.as_deref() == Some(task.id.as_str());
        let id = task.id.clone();
        actions = actions.child(div().flex_1()).child(
            style::small_button(
                "pilus-task-op-delete",
                if confirming {
                    "Confirm delete"
                } else {
                    "Delete"
                },
                if confirming {
                    ButtonKind::Danger
                } else {
                    ButtonKind::Ghost
                },
                theme,
                fonts.secondary,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.tasks_panel.confirm_delete.as_deref() == Some(id.as_str()) {
                    this.run_task_op(id.clone(), TaskOp::Lifecycle(TaskAction::Delete), cx);
                    this.tasks_panel.selected = None;
                } else {
                    this.tasks_panel.confirm_delete = Some(id.clone());
                    cx.notify();
                }
            })),
        );

        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .h(px(style::scaled(&self.shared_settings, 48.0)))
            .flex_shrink_0()
            .px_4()
            .border_b_1()
            .border_color(terminal_inset_border())
            .child(
                div()
                    .font_family(SharedString::from(style::mono_family(
                        &self.shared_settings,
                    )))
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(task.id.clone()),
            )
            .child(
                style::small_button(
                    "pilus-task-detail-close",
                    "Close",
                    ButtonKind::Ghost,
                    theme,
                    fonts.secondary,
                )
                .on_click(cx.listener(|this, _, _, cx| this.select_task(None, cx))),
            );

        style::enter(
            format!("pilus-task-detail-{}", task.id),
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
                        .id("pilus-task-detail-scroll")
                        .flex_1()
                        .min_h(px(0.0))
                        .overflow_y_scroll()
                        .child(sections),
                )
                .child(actions),
        )
        .into_any_element()
    }

    fn task_section(&self, label: &'static str, body: impl IntoElement) -> Div {
        let fonts = style::Fonts::resolve(&self.shared_settings);
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(style::section_label(self.theme, label, fonts.label))
            .child(body)
    }

    /// The new-task form, in the side pane where a task's detail would be.
    fn task_compose_pane(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let focus = self.tasks_panel.compose_focus;
        let field_row =
            |field: ComposeField, label: &'static str, height: f32, cx: &mut Context<Self>| {
                let entity = self.tasks_inputs.entity(field)?.clone();
                entity.update(cx, |input, _| {
                    input.set_style(
                        theme,
                        style::chrome_font(&self.shared_settings),
                        px(fonts.body),
                    );
                });
                Some(
                    style::field(
                        format!("pilus-task-field-{label}"),
                        label,
                        focus == field,
                        theme,
                        style::scaled(&self.shared_settings, height),
                        fonts.label,
                        entity,
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.tasks_panel.compose_focus = field;
                            this.tasks_panel.compose.close_dropdown();
                            cx.notify();
                        }),
                    ),
                )
            };
        let title = field_row(ComposeField::Title, "Title", style::CONTROL_HEIGHT, cx);
        let prompt = field_row(ComposeField::Prompt, "Prompt", 96.0, cx);
        let gate = field_row(
            ComposeField::Gate,
            "Quality gate",
            style::CONTROL_HEIGHT,
            cx,
        );
        let form = div()
            .flex()
            .flex_col()
            .gap_4()
            .p_4()
            .children(title)
            .children(prompt)
            .child(self.task_pickers(cx))
            .children(gate);

        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .h(px(style::scaled(&self.shared_settings, 48.0)))
            .flex_shrink_0()
            .px_4()
            .border_b_1()
            .border_color(terminal_inset_border())
            .child(style::title(theme, "New task", fonts.title))
            .child(
                style::small_button(
                    "pilus-task-compose-close",
                    "Close",
                    ButtonKind::Ghost,
                    theme,
                    fonts.secondary,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.tasks_panel.composing = false;
                    cx.notify();
                })),
            );
        let actions = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_end()
            .gap_2()
            .p_3()
            .border_t_1()
            .border_color(terminal_inset_border())
            .child(
                style::small_button(
                    "pilus-task-compose-cancel",
                    "Cancel",
                    ButtonKind::Ghost,
                    theme,
                    fonts.secondary,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.tasks_panel.composing = false;
                    cx.notify();
                })),
            )
            .child(
                style::small_button(
                    "pilus-task-compose-create",
                    "Create task",
                    ButtonKind::Primary,
                    theme,
                    fonts.secondary,
                )
                .on_click(cx.listener(|this, _, _, cx| this.submit_task(cx))),
            );
        style::enter(
            "pilus-task-compose-pane",
            div()
                .flex()
                .flex_col()
                .w(px(style::scaled(&self.shared_settings, COMPOSE_WIDTH)))
                .flex_shrink_0()
                .min_h(px(0.0))
                .border_l_1()
                .border_color(terminal_inset_border())
                .child(header)
                .child(
                    div()
                        .id("pilus-task-compose-scroll")
                        .flex_1()
                        .min_h(px(0.0))
                        .overflow_y_scroll()
                        .child(form),
                )
                .child(actions),
        )
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use pilus_client::{Task, TaskStatus};

    use super::{ComposeField, TaskView, age_label, grouped, nav_order, ops_for, step_selection};

    fn task(id: &str, status: TaskStatus) -> Task {
        Task {
            id: id.to_string(),
            status: Some(status),
            ..Task::default()
        }
    }

    #[test]
    fn groups_follow_board_order_and_hide_empty_ones() {
        let tasks = [
            task("t1", TaskStatus::Done),
            task("t2", TaskStatus::Queued),
            task("t3", TaskStatus::Running),
            task("t4", TaskStatus::Queued),
        ];
        let groups = grouped(&tasks, TaskView::All);
        let statuses: Vec<_> = groups.iter().map(|(status, _)| *status).collect();
        assert_eq!(
            statuses,
            [TaskStatus::Running, TaskStatus::Queued, TaskStatus::Done]
        );
        assert_eq!(groups[1].1.len(), 2);
    }

    #[test]
    fn views_split_open_from_closed() {
        let tasks = [task("t1", TaskStatus::Done), task("t2", TaskStatus::Queued)];
        assert_eq!(grouped(&tasks, TaskView::Active).len(), 1);
        assert_eq!(grouped(&tasks, TaskView::Closed)[0].1[0].id, "t1");
    }

    #[test]
    fn navigation_skips_collapsed_groups_and_clamps() {
        let tasks = [
            task("t1", TaskStatus::Running),
            task("t2", TaskStatus::Queued),
            task("t3", TaskStatus::Queued),
        ];
        let groups = grouped(&tasks, TaskView::All);
        let mut collapsed = std::collections::HashSet::new();
        assert_eq!(nav_order(&groups, &collapsed), ["t1", "t2", "t3"]);
        collapsed.insert(TaskStatus::Running);
        let order = nav_order(&groups, &collapsed);
        assert_eq!(order, ["t2", "t3"]);
        assert_eq!(step_selection(&order, None, 1).as_deref(), Some("t2"));
        assert_eq!(step_selection(&order, None, -1).as_deref(), Some("t3"));
        assert_eq!(step_selection(&order, Some("t3"), 1).as_deref(), Some("t3"));
        assert_eq!(
            step_selection(&order, Some("t3"), -1).as_deref(),
            Some("t2")
        );
        assert_eq!(step_selection(&[], None, 1), None);
    }

    #[test]
    fn terminal_tasks_offer_no_forward_actions() {
        assert!(ops_for(TaskStatus::Merged).is_empty());
        assert!(ops_for(TaskStatus::Merging).is_empty());
        assert_eq!(ops_for(TaskStatus::Queued).len(), 1);
    }

    #[test]
    fn ages_pick_the_largest_unit() {
        assert_eq!(age_label(100, 100), "now");
        assert_eq!(age_label(400, 100), "5m");
        assert_eq!(age_label(10_900, 100), "3h");
        assert_eq!(age_label(172_900, 100), "2d");
        assert_eq!(age_label(3_000_000, 100), "4w");
        assert_eq!(age_label(5, 100), "now");
    }

    #[test]
    fn tab_cycles_the_composer_both_ways() {
        let mut field = ComposeField::Title;
        for _ in 0..ComposeField::ALL.len() {
            field = field.step(false);
        }
        assert_eq!(field, ComposeField::Title);
        assert_eq!(ComposeField::Title.step(true), ComposeField::Gate);
    }
}
