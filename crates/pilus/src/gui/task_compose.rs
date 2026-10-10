//! The Tasks page's pickers: which machine and workspace the list shows, and
//! the composer's machine, workspace, dependencies, and paths.
//!
//! A task is queued on one daemon, in one of its workspaces, after some of
//! that daemon's tasks, and may touch some of that workspace's folders. So the
//! pickers depend on each other: choosing a machine resets the workspace, and
//! choosing a workspace resets the paths. The list's own scope works the same
//! way, and a machine other than the one the window shows is read through a
//! small cache that refreshes while the page is open. This module holds that
//! state and its pure rules (so they test without GPUI or a daemon), the
//! background loads that fill the lists, and the dropdown rendering. The page
//! itself lives in `tasks.rs`.

use std::time::{Duration, Instant};

use gpui::{
    Anchor, AnyElement, Context, KeyDownEvent, anchored, deferred, div, point, prelude::*, px,
};
use pilus_client::Task;

use super::picker::{self, DirSource};
use super::style::{self, FillExt, RadiusExt};
use super::{
    EndpointId, PICKER_CHIP_H, PICKER_MENU_MAX_H, PilusView, icon, picker_face, picker_menu,
};

/// Most rows a dropdown builds; the rest are reached by typing a filter.
const MAX_ROWS: usize = 100;

/// The composer's pickers, in Tab order.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum TaskPicker {
    Machine,
    Workspace,
    Deps,
    Paths,
    /// The list's machine.
    ScopeMachine,
    /// The list's workspace filter.
    ScopeWorkspace,
}

impl TaskPicker {
    /// The composer's pickers, in Tab order.
    pub(super) const ALL: [TaskPicker; 4] = [
        TaskPicker::Machine,
        TaskPicker::Workspace,
        TaskPicker::Deps,
        TaskPicker::Paths,
    ];

    /// The list header's pickers.
    const SCOPE: [TaskPicker; 2] = [TaskPicker::ScopeMachine, TaskPicker::ScopeWorkspace];

    fn label(self) -> &'static str {
        match self {
            Self::Machine | Self::ScopeMachine => "Machine",
            Self::Workspace | Self::ScopeWorkspace => "Workspace",
            Self::Deps => "Runs after",
            Self::Paths => "Paths it may touch",
        }
    }

    /// Whether choosing a row toggles it and leaves the list open.
    fn multiple(self) -> bool {
        matches!(self, Self::Deps | Self::Paths)
    }

    fn glyph(self) -> &'static [u8] {
        match self {
            Self::Machine | Self::ScopeMachine => include_bytes!("../../assets/icons/server.svg"),
            Self::Workspace | Self::ScopeWorkspace => {
                include_bytes!("../../assets/icons/folder.svg")
            }
            Self::Deps => include_bytes!("../../assets/icons/git-branch.svg"),
            Self::Paths => include_bytes!("../../assets/icons/folder-git.svg"),
        }
    }
}

/// One option in a dropdown.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PickRow {
    /// What choosing the row stores: an endpoint key, a workspace id, a task
    /// id, or a glob.
    pub(super) key: String,
    pub(super) label: String,
    /// A dim trailing note: a working directory, a task's status.
    pub(super) hint: Option<String>,
    /// Whether the row is currently picked.
    pub(super) chosen: bool,
}

/// The rows whose label or key contains `filter`, case-insensitively.
fn filter_rows(rows: Vec<PickRow>, filter: &str) -> Vec<PickRow> {
    let needle = filter.trim().to_lowercase();
    if needle.is_empty() {
        return rows;
    }
    rows.into_iter()
        .filter(|row| {
            row.label.to_lowercase().contains(&needle) || row.key.to_lowercase().contains(&needle)
        })
        .collect()
}

/// Add `key` to `list`, or remove it when already there.
fn toggle(list: &mut Vec<String>, key: &str) {
    match list.iter().position(|item| item == key) {
        Some(at) => {
            list.remove(at);
        }
        None => list.push(key.to_string()),
    }
}

/// A workspace's top-level folders, once listed.
#[derive(Clone, Debug, Default)]
pub(super) struct Folders {
    /// The machine and root they were listed for; a mismatch means stale.
    key: Option<(EndpointId, String)>,
    /// Globs such as `src/**`.
    entries: Vec<String>,
    loading: bool,
    error: Option<String>,
}

/// What the list shows: one machine, optionally one of its workspaces.
#[derive(Clone, Debug, Default)]
pub(super) struct Scope {
    /// `None` follows the machine the window is showing.
    pub(super) machine: Option<EndpointId>,
    /// `None` shows every workspace on the machine.
    pub(super) workspace: Option<String>,
}

/// One machine's tasks as last fetched, for a machine the window is not
/// showing: its feed is not ours, so the page asks for them itself.
#[derive(Clone, Debug, Default)]
pub(super) struct Fetched {
    tasks: Vec<Task>,
    /// When the last answer arrived; `None` until the first.
    at: Option<Instant>,
    loading: bool,
    /// Whether any answer has arrived, so a refresh does not look like a
    /// first load.
    ever: bool,
}

/// How stale another machine's task list may get while it is on screen.
const REFRESH: Duration = Duration::from_secs(4);
/// How stale it may get when it only feeds the composer's dependencies.
const DEP_REFRESH: Duration = Duration::from_secs(60);

/// What a background load reports back to the window.
pub(super) enum TasksMsg {
    /// A line for the page footer.
    Status(String),
    Folders {
        key: (EndpointId, String),
        result: Result<Vec<String>, String>,
    },
    /// Another machine's tasks.
    Tasks {
        endpoint: EndpointId,
        result: Result<Vec<Task>, String>,
    },
}

/// The composer's picker state.
#[derive(Debug, Default)]
pub(super) struct ComposeState {
    /// `None` follows the machine the window is showing.
    pub(super) machine: Option<EndpointId>,
    /// `None` follows that machine's active workspace.
    pub(super) workspace: Option<String>,
    pub(super) deps: Vec<String>,
    pub(super) paths: Vec<String>,
    /// Globs typed into the paths filter, offered beside the folders.
    custom_paths: Vec<String>,
    dropdown: Option<TaskPicker>,
    /// The picker an outside press just closed, so its own click reads as a
    /// toggle-off rather than a reopen.
    dismissed: Option<TaskPicker>,
    filter: String,
    cursor: usize,
    folders: Folders,
}

impl ComposeState {
    /// A fresh composer aimed at `machine` and `workspace`.
    pub(super) fn at(machine: EndpointId, workspace: Option<String>) -> Self {
        Self {
            machine: Some(machine),
            workspace,
            ..Self::default()
        }
    }

    pub(super) fn dropdown(&self) -> Option<TaskPicker> {
        self.dropdown
    }

    fn open_dropdown(&mut self, picker: TaskPicker) {
        self.dropdown = Some(picker);
        self.dismissed = None;
        self.filter.clear();
        self.cursor = 0;
    }

    pub(super) fn close_dropdown(&mut self) {
        self.dropdown = None;
        self.dismissed = None;
        self.filter.clear();
        self.cursor = 0;
    }

    fn dismiss_dropdown(&mut self) {
        self.dismissed = self.dropdown.take();
        self.filter.clear();
        self.cursor = 0;
    }

    fn filter_insert(&mut self, text: &str) {
        self.filter
            .extend(text.chars().filter(|ch| !ch.is_control()));
        self.cursor = 0;
    }

    fn filter_backspace(&mut self) {
        self.filter.pop();
        self.cursor = 0;
    }

    fn move_cursor(&mut self, delta: isize, len: usize) {
        if len > 0 {
            self.cursor = (self.cursor as isize + delta).clamp(0, len as isize - 1) as usize;
        }
    }

    /// Pick a machine. Everything below it belonged to the last one.
    fn set_machine(&mut self, machine: EndpointId) {
        if self.machine.as_ref() == Some(&machine) {
            return;
        }
        self.machine = Some(machine);
        self.workspace = None;
        self.deps.clear();
        self.clear_paths();
    }

    /// Pick a workspace. Its folders are not the last one's.
    fn set_workspace(&mut self, id: String) {
        if self.workspace.as_ref() == Some(&id) {
            return;
        }
        self.workspace = Some(id);
        self.clear_paths();
    }

    fn clear_paths(&mut self) {
        self.paths.clear();
        self.custom_paths.clear();
    }

    /// Pick or unpick a glob; a typed one joins the list on first use.
    fn toggle_path(&mut self, glob: &str) {
        let known = self.folders.entries.iter().any(|entry| entry == glob);
        if !known && !self.custom_paths.iter().any(|entry| entry == glob) {
            self.custom_paths.push(glob.to_string());
        }
        toggle(&mut self.paths, glob);
    }
}

/// `3 tasks`, `t1`, `t1, t2`, or `None` for the dependency chip.
fn deps_summary(deps: &[String]) -> String {
    match deps {
        [] => "None".to_string(),
        [one] => one.clone(),
        [a, b] => format!("{a}, {b}"),
        _ => format!("{} tasks", deps.len()),
    }
}

/// `Whole workspace`, `src/**`, or `src/** +2` for the paths chip.
fn paths_summary(paths: &[String]) -> String {
    match paths {
        [] => "Whole workspace".to_string(),
        [one] => one.clone(),
        [first, rest @ ..] => format!("{first} +{}", rest.len()),
    }
}

impl PilusView {
    /// The machine the composer queues the task on.
    pub(super) fn compose_machine(&self) -> EndpointId {
        self.tasks_panel
            .compose
            .machine
            .clone()
            .unwrap_or_else(|| self.active.clone())
    }

    /// The workspace the composer queues the task in: the pick if it still
    /// exists on the machine, else the machine's active one, else its first.
    pub(super) fn compose_workspace(&self) -> Option<pilus_protocol::ShellWorkspace> {
        let workspaces = self.endpoint_workspaces(&self.compose_machine());
        let picked = self.tasks_panel.compose.workspace.as_ref();
        picked
            .and_then(|id| workspaces.iter().find(|workspace| &workspace.id == id))
            .or_else(|| workspaces.iter().find(|workspace| workspace.active))
            .or_else(|| workspaces.first())
            .cloned()
    }

    /// The machine the list shows.
    pub(super) fn scope_machine(&self) -> EndpointId {
        self.tasks_panel
            .scope
            .machine
            .clone()
            .filter(|machine| self.connected_endpoints().contains(machine))
            .unwrap_or_else(|| self.active.clone())
    }

    /// The workspace the list is narrowed to, if it still exists on the
    /// machine.
    pub(super) fn scope_workspace_id(&self) -> Option<String> {
        let picked = self.tasks_panel.scope.workspace.as_ref()?;
        self.endpoint_workspaces(&self.scope_machine())
            .iter()
            .any(|workspace| &workspace.id == picked)
            .then(|| picked.clone())
    }

    /// `endpoint`'s tasks: the window's own feed for the machine it shows,
    /// else the last fetch.
    pub(super) fn machine_tasks(&self, endpoint: &EndpointId) -> Vec<Task> {
        if *endpoint == self.active {
            return self.tasks.items.clone();
        }
        self.tasks_panel
            .remote
            .get(endpoint)
            .map(|fetched| fetched.tasks.clone())
            .unwrap_or_default()
    }

    /// The tasks the list shows: the scope's machine, narrowed to its
    /// workspace.
    pub(super) fn scope_tasks(&self) -> Vec<Task> {
        let mut tasks = self.machine_tasks(&self.scope_machine());
        if let Some(workspace) = self.scope_workspace_id() {
            tasks.retain(|task| task.workspace_id.as_deref() == Some(workspace.as_str()));
        }
        tasks
    }

    /// Whether the scope's machine has not answered yet.
    pub(super) fn scope_loading(&self) -> bool {
        let machine = self.scope_machine();
        machine != self.active
            && self
                .tasks_panel
                .remote
                .get(&machine)
                .is_none_or(|fetched| !fetched.ever)
    }

    /// Fetch `endpoint`'s tasks when they are older than `max_age`. The
    /// machine the window shows needs none: its feed already carries them.
    fn ensure_tasks(&mut self, endpoint: &EndpointId, max_age: Duration) {
        if *endpoint == self.active {
            return;
        }
        let fetched = self.tasks_panel.remote.entry(endpoint.clone()).or_default();
        if fetched.loading || fetched.at.is_some_and(|at| at.elapsed() < max_age) {
            return;
        }
        fetched.loading = true;
        let client = self.control_client_for_endpoint(endpoint);
        let endpoint = endpoint.clone();
        let tx = self.tasks_op_tx.clone();
        std::thread::spawn(move || {
            let result = match client {
                Some(mut client) => client
                    .task_summary()
                    .map(|summary| summary.items)
                    .map_err(|error| error.to_string()),
                None => Err("could not reach the machine".to_string()),
            };
            tx.send(TasksMsg::Tasks { endpoint, result }).ok();
        });
    }

    /// Keep the list's machine fresh; called every window tick while the
    /// page is open.
    pub(super) fn tasks_tick(&mut self) {
        let machine = self.scope_machine();
        self.ensure_tasks(&machine, REFRESH);
    }

    /// Point the list at `machine`, dropping what belonged to the last one.
    fn set_scope_machine(&mut self, machine: EndpointId) {
        if self.scope_machine() == machine {
            return;
        }
        self.tasks_panel.scope.machine = Some(machine);
        self.tasks_panel.scope.workspace = None;
        self.tasks_panel.selected = None;
        self.tasks_panel.confirm_delete = None;
    }

    /// The tasks a dependency can name on the composer's machine.
    fn compose_tasks(&self) -> Vec<Task> {
        self.machine_tasks(&self.compose_machine())
    }

    /// Where to list directories for `endpoint`.
    fn dir_source_for(&self, endpoint: &EndpointId) -> DirSource {
        match endpoint {
            EndpointId::Local => DirSource::Local,
            EndpointId::Machine(id) => self
                .machine_target_by_key(id)
                .map_or(DirSource::Local, DirSource::Remote),
        }
    }

    /// Start whatever loads the pickers now need: the chosen workspace's
    /// folders, and another machine's tasks. Called on every composer frame;
    /// a load already in flight or already answered costs a comparison.
    pub(super) fn compose_sync(&mut self) {
        let machine = self.compose_machine();
        let key = self
            .compose_workspace()
            .map(|workspace| (machine.clone(), workspace.cwd));
        if key != self.tasks_panel.compose.folders.key {
            self.tasks_panel.compose.folders = Folders {
                key: key.clone(),
                loading: key.is_some(),
                ..Folders::default()
            };
            if let Some((endpoint, root)) = key {
                let source = self.dir_source_for(&endpoint);
                let tx = self.tasks_op_tx.clone();
                std::thread::spawn(move || {
                    let result = picker::list(&source, &root).map(|listed| {
                        listed
                            .entries
                            .into_iter()
                            .filter(|entry| !entry.name.starts_with('.'))
                            .map(|entry| format!("{}/**", entry.name))
                            .collect()
                    });
                    tx.send(TasksMsg::Folders {
                        key: (endpoint, root),
                        result,
                    })
                    .ok();
                });
            }
        }
        self.ensure_tasks(&machine, DEP_REFRESH);
    }

    /// Fold one background answer into the page.
    pub(super) fn tasks_receive(&mut self, message: TasksMsg) {
        match message {
            TasksMsg::Status(text) => {
                self.tasks_panel.status = text;
                // The daemon changed a task: re-read a machine we poll.
                let machine = self.scope_machine();
                if let Some(fetched) = self.tasks_panel.remote.get_mut(&machine) {
                    fetched.at = None;
                }
            }
            TasksMsg::Folders { key, result } => {
                let folders = &mut self.tasks_panel.compose.folders;
                if folders.key.as_ref() == Some(&key) {
                    folders.loading = false;
                    match result {
                        Ok(entries) => folders.entries = entries,
                        Err(error) => folders.error = Some(error),
                    }
                }
            }
            TasksMsg::Tasks { endpoint, result } => {
                let fetched = self.tasks_panel.remote.entry(endpoint).or_default();
                fetched.loading = false;
                fetched.at = Some(Instant::now());
                fetched.ever = true;
                match result {
                    Ok(tasks) => fetched.tasks = tasks,
                    // Keep the last good list; the next tick tries again.
                    Err(error) => self.tasks_panel.status = format!("Tasks: {error}"),
                }
            }
        }
    }

    /// The open dropdown's rows after the filter, with a row offering to add
    /// the typed text as a glob when nothing in the paths list matches it.
    fn task_rows(&self, picker: TaskPicker) -> Vec<PickRow> {
        let compose = &self.tasks_panel.compose;
        let machine = self.compose_machine();
        let all: Vec<PickRow> = match picker {
            TaskPicker::Machine | TaskPicker::ScopeMachine => {
                let current = if picker == TaskPicker::Machine {
                    machine.clone()
                } else {
                    self.scope_machine()
                };
                self.connected_endpoints()
                    .into_iter()
                    .map(|id| PickRow {
                        key: id.key(),
                        label: self.endpoint_label(&id),
                        hint: None,
                        chosen: id == current,
                    })
                    .collect()
            }
            TaskPicker::ScopeWorkspace => {
                let current = self.scope_workspace_id();
                let mut rows = vec![PickRow {
                    key: String::new(),
                    label: "All workspaces".to_string(),
                    hint: None,
                    chosen: current.is_none(),
                }];
                rows.extend(self.endpoint_workspaces(&self.scope_machine()).iter().map(
                    |workspace| PickRow {
                        key: workspace.id.clone(),
                        label: workspace.name.clone(),
                        hint: Some(workspace.cwd.clone()),
                        chosen: current.as_deref() == Some(workspace.id.as_str()),
                    },
                ));
                rows
            }
            TaskPicker::Workspace => {
                let current = self.compose_workspace().map(|workspace| workspace.id);
                self.endpoint_workspaces(&machine)
                    .iter()
                    .map(|workspace| PickRow {
                        key: workspace.id.clone(),
                        label: workspace.name.clone(),
                        hint: Some(workspace.cwd.clone()),
                        chosen: current.as_deref() == Some(workspace.id.as_str()),
                    })
                    .collect()
            }
            TaskPicker::Deps => self
                .compose_tasks()
                .into_iter()
                .map(|task| PickRow {
                    chosen: compose.deps.contains(&task.id),
                    label: format!("{} · {}", task.id, task.title),
                    hint: Some(task.status().label().to_string()),
                    key: task.id,
                })
                .collect(),
            TaskPicker::Paths => compose
                .folders
                .entries
                .iter()
                .chain(
                    compose
                        .custom_paths
                        .iter()
                        .filter(|glob| !compose.folders.entries.contains(glob)),
                )
                .map(|glob| PickRow {
                    key: glob.clone(),
                    label: glob.clone(),
                    hint: None,
                    chosen: compose.paths.contains(glob),
                })
                .collect(),
        };
        let mut rows = filter_rows(all, &compose.filter);
        let typed = compose.filter.trim();
        if picker == TaskPicker::Paths
            && !typed.is_empty()
            && !rows.iter().any(|row| row.key == typed)
        {
            rows.push(PickRow {
                key: typed.to_string(),
                label: format!("Add “{typed}”"),
                hint: Some("custom glob".to_string()),
                chosen: false,
            });
        }
        rows
    }

    /// Apply a chosen row; single-pick lists close, multi-pick lists stay.
    fn choose_task_row(&mut self, picker: TaskPicker, row: &PickRow) {
        let endpoint = || {
            if row.key == "local" {
                EndpointId::Local
            } else {
                EndpointId::Machine(row.key.clone())
            }
        };
        match picker {
            TaskPicker::ScopeMachine => self.set_scope_machine(endpoint()),
            TaskPicker::ScopeWorkspace => {
                self.tasks_panel.scope.workspace = (!row.key.is_empty()).then(|| row.key.clone());
                self.tasks_panel.selected = None;
            }
            _ => {}
        }
        let compose = &mut self.tasks_panel.compose;
        match picker {
            TaskPicker::ScopeMachine | TaskPicker::ScopeWorkspace => {}
            TaskPicker::Machine => compose.set_machine(endpoint()),
            TaskPicker::Workspace => compose.set_workspace(row.key.clone()),
            TaskPicker::Deps => toggle(&mut compose.deps, &row.key),
            TaskPicker::Paths => {
                compose.toggle_path(&row.key);
                // A typed glob is now a row of its own; clear the filter so
                // it shows among the others.
                compose.filter.clear();
            }
        }
        if !picker.multiple() {
            compose.close_dropdown();
        }
    }

    /// Open or close `picker` from its face, and give it keyboard focus.
    pub(super) fn toggle_task_dropdown(&mut self, picker: TaskPicker, cx: &mut Context<Self>) {
        if TaskPicker::ALL.contains(&picker) {
            self.tasks_panel.compose_focus = super::tasks::ComposeField::Pick(picker);
        }
        let compose = &mut self.tasks_panel.compose;
        if compose.dismissed.take() == Some(picker) {
            // The outside press already closed it; stay closed.
        } else if compose.dropdown == Some(picker) {
            compose.close_dropdown();
        } else {
            compose.open_dropdown(picker);
        }
        cx.notify();
    }

    /// Open `picker`'s list from the keyboard.
    pub(super) fn open_task_dropdown(&mut self, picker: TaskPicker) {
        self.tasks_panel.compose.open_dropdown(picker);
    }

    /// Keys while a dropdown is open: arrows move, Enter picks, typing
    /// filters, Escape closes just the list.
    pub(super) fn task_dropdown_key(
        &mut self,
        picker: TaskPicker,
        event: &KeyDownEvent,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        let rows = self.task_rows(picker);
        match key {
            "escape" | "tab" => self.tasks_panel.compose.close_dropdown(),
            "up" => self.tasks_panel.compose.move_cursor(-1, rows.len()),
            "down" => self.tasks_panel.compose.move_cursor(1, rows.len()),
            "enter" => {
                let at = self.tasks_panel.compose.cursor;
                if let Some(row) = rows.get(at) {
                    self.choose_task_row(picker, row);
                }
            }
            "backspace" | "delete" | "del" => self.tasks_panel.compose.filter_backspace(),
            _ => {
                let mods = event.keystroke.modifiers;
                if !(mods.platform || mods.control) {
                    let typed = event
                        .keystroke
                        .key_char
                        .as_deref()
                        .filter(|text| !text.is_empty())
                        .map(str::to_string)
                        .or_else(|| (key.chars().count() == 1).then(|| key.to_string()));
                    if let Some(text) = typed {
                        self.tasks_panel.compose.filter_insert(&text);
                    }
                }
            }
        }
        cx.notify();
    }

    /// The face's value text.
    fn task_picker_value(&self, picker: TaskPicker) -> String {
        let compose = &self.tasks_panel.compose;
        match picker {
            TaskPicker::Machine => self.endpoint_label(&self.compose_machine()),
            TaskPicker::ScopeMachine => self.endpoint_label(&self.scope_machine()),
            TaskPicker::ScopeWorkspace => self.scope_workspace_id().map_or_else(
                || "All workspaces".to_string(),
                |id| {
                    self.endpoint_workspaces(&self.scope_machine())
                        .iter()
                        .find(|workspace| workspace.id == id)
                        .map_or(id, |workspace| workspace.name.clone())
                },
            ),
            TaskPicker::Workspace => self
                .compose_workspace()
                .map_or_else(|| "No workspace".to_string(), |workspace| workspace.name),
            TaskPicker::Deps => deps_summary(&compose.deps),
            TaskPicker::Paths => paths_summary(&compose.paths),
        }
    }

    /// One picker's face, with its list floated beneath it while open.
    fn task_picker_face(
        &self,
        picker: TaskPicker,
        max_width: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.theme;
        let open = self.tasks_panel.compose.dropdown == Some(picker);
        let focused = self.tasks_panel.compose_focus == super::tasks::ComposeField::Pick(picker)
            && self.tasks_panel.composing();
        let mut face = picker_face(
            format!("pilus-task-picker-{:?}", picker),
            icon(picker.glyph(), 14.0, theme.muted).into_any_element(),
            self.task_picker_value(picker),
            max_width,
            open || focused,
            theme.panel,
            theme,
            self.footer_font(),
            cx.listener(move |this, _, _, cx| this.toggle_task_dropdown(picker, cx)),
        );
        if open {
            face = face.relative().child(
                div()
                    .absolute()
                    .top(px(0.0))
                    .left(px(0.0))
                    .child(self.task_dropdown(picker, cx)),
            );
        }
        face.into_any_element()
    }

    /// The list header's machine and workspace pickers.
    pub(super) fn task_scope_pickers(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut row = div().flex().flex_row().items_center().gap_1();
        for picker in TaskPicker::SCOPE {
            row = row.child(self.task_picker_face(picker, 150.0, cx));
        }
        row.into_any_element()
    }

    /// The composer's four pickers, each under its label.
    pub(super) fn task_pickers(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let mut grid = div().flex().flex_row().flex_wrap().gap_x_4().gap_y_2();
        for picker in TaskPicker::ALL {
            grid = grid.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(fonts.label))
                            .text_color(theme.muted)
                            .child(picker.label()),
                    )
                    .child(self.task_picker_face(picker, 160.0, cx)),
            );
        }
        grid.into_any_element()
    }

    /// The open list, floated under its face.
    fn task_dropdown(&self, picker: TaskPicker, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let rows = self.task_rows(picker);
        let cursor = self.tasks_panel.compose.cursor;
        let filter = self.tasks_panel.compose.filter.clone();
        let total = rows.len();
        let mut list = picker_menu("pilus-task-dropdown", theme)
            .max_h(px(PICKER_MENU_MAX_H))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.tasks_panel.compose.dismiss_dropdown();
                cx.notify();
            }));
        if !filter.is_empty() {
            list = list.child(
                div()
                    .px_3()
                    .pt_1()
                    .text_size(px(self.section_font()))
                    .text_color(theme.muted)
                    .child(format!("Filter: {filter}")),
            );
        }
        if rows.is_empty() {
            list = list.child(
                div()
                    .px_3()
                    .py_3()
                    .text_size(px(self.chrome_font()))
                    .text_color(theme.muted)
                    .child(self.task_empty_note(picker)),
            );
        }
        for (index, row) in rows.into_iter().take(MAX_ROWS).enumerate() {
            let active = index == cursor;
            let picked = row.clone();
            list = list.child(
                div()
                    .id(format!("pilus-task-option-{index}"))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .h(px(30.0))
                    .px_3()
                    .radius_md()
                    .text_size(px(self.chrome_font()))
                    .text_color(theme.panel_fg)
                    .cursor_pointer()
                    .state_fill(
                        if active {
                            theme.panel
                        } else {
                            theme.panel.opacity(0.0)
                        },
                        theme.panel,
                        style::RADIUS_MD,
                    )
                    .child(icon(
                        include_bytes!("../../assets/icons/check.svg"),
                        13.0,
                        if row.chosen {
                            theme.accent
                        } else {
                            theme.accent.opacity(0.0)
                        },
                    ))
                    .child(div().flex_1().min_w(px(0.0)).truncate().child(row.label))
                    .children(row.hint.map(|hint| {
                        div()
                            .max_w(px(110.0))
                            .truncate()
                            .text_size(px(self.section_font()))
                            .text_color(theme.muted)
                            .child(hint)
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.choose_task_row(picker, &picked);
                        cx.notify();
                    })),
            );
        }
        if total > MAX_ROWS {
            list = list.child(
                div()
                    .px_3()
                    .py_1()
                    .text_size(px(self.section_font()))
                    .text_color(theme.muted)
                    .child(format!("…{} more · type to filter", total - MAX_ROWS)),
            );
        }
        deferred(
            anchored()
                .anchor(Anchor::TopLeft)
                .offset(point(px(0.0), px(PICKER_CHIP_H + 4.0)))
                .snap_to_window_with_margin(px(8.0))
                .child(style::enter("pilus-task-dropdown-enter", list)),
        )
        .with_priority(2)
        .into_any_element()
    }

    /// What an empty list says, by what it was waiting on.
    fn task_empty_note(&self, picker: TaskPicker) -> &'static str {
        let compose = &self.tasks_panel.compose;
        match picker {
            TaskPicker::Machine | TaskPicker::ScopeMachine => "No machines",
            TaskPicker::Workspace | TaskPicker::ScopeWorkspace => "No workspaces on this machine",
            TaskPicker::Deps if self.compose_machine() != self.active && self.scope_loading() => {
                "Loading tasks…"
            }
            TaskPicker::Deps => "No tasks to depend on",
            TaskPicker::Paths if compose.folders.loading => "Loading folders…",
            TaskPicker::Paths if compose.folders.error.is_some() => {
                "Could not list folders · type a glob"
            }
            TaskPicker::Paths => "No folders · type a glob",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ComposeState, EndpointId, PickRow, deps_summary, filter_rows, paths_summary, toggle,
    };

    fn row(key: &str, label: &str) -> PickRow {
        PickRow {
            key: key.to_string(),
            label: label.to_string(),
            hint: None,
            chosen: false,
        }
    }

    #[test]
    fn filter_matches_label_or_key_ignoring_case() {
        let rows = vec![row("t1", "t1 · Auth refactor"), row("t2", "t2 · Docs")];
        assert_eq!(filter_rows(rows.clone(), "AUTH").len(), 1);
        assert_eq!(filter_rows(rows.clone(), "t2")[0].key, "t2");
        assert_eq!(filter_rows(rows.clone(), "  ").len(), 2);
        assert!(filter_rows(rows, "nope").is_empty());
    }

    #[test]
    fn toggle_adds_then_removes() {
        let mut list = Vec::new();
        toggle(&mut list, "t1");
        toggle(&mut list, "t2");
        toggle(&mut list, "t1");
        assert_eq!(list, ["t2"]);
    }

    #[test]
    fn a_new_machine_resets_everything_beneath_it() {
        let mut state = ComposeState::default();
        state.set_machine(EndpointId::Machine("m1".into()));
        state.set_workspace("w1".into());
        state.deps.push("t1".into());
        state.toggle_path("src/**");
        state.set_machine(EndpointId::Machine("m2".into()));
        assert_eq!(state.workspace, None);
        assert!(state.deps.is_empty());
        assert!(state.paths.is_empty());
    }

    #[test]
    fn repicking_the_same_machine_or_workspace_keeps_the_rest() {
        let mut state = ComposeState::default();
        state.set_machine(EndpointId::Local);
        state.set_workspace("w1".into());
        state.toggle_path("src/**");
        state.set_machine(EndpointId::Local);
        state.set_workspace("w1".into());
        assert_eq!(state.paths, ["src/**"]);
        state.set_workspace("w2".into());
        assert!(state.paths.is_empty());
    }

    #[test]
    fn a_typed_glob_joins_the_list_and_stays_pickable() {
        let mut state = ComposeState::default();
        state.toggle_path("docs/**/*.md");
        assert_eq!(state.paths, ["docs/**/*.md"]);
        assert_eq!(state.custom_paths, ["docs/**/*.md"]);
        state.toggle_path("docs/**/*.md");
        assert!(state.paths.is_empty());
        assert_eq!(state.custom_paths.len(), 1);
    }

    #[test]
    fn dismissing_remembers_which_list_it_closed() {
        let mut state = ComposeState::default();
        state.open_dropdown(super::TaskPicker::Deps);
        state.filter_insert("t1");
        state.dismiss_dropdown();
        assert_eq!(state.dropdown(), None);
        assert_eq!(state.dismissed, Some(super::TaskPicker::Deps));
        assert!(state.filter.is_empty());
    }

    #[test]
    fn the_cursor_clamps_to_the_rows() {
        let mut state = ComposeState::default();
        state.move_cursor(5, 3);
        assert_eq!(state.cursor, 2);
        state.move_cursor(-9, 3);
        assert_eq!(state.cursor, 0);
        state.move_cursor(1, 0);
        assert_eq!(state.cursor, 0);
    }

    #[test]
    fn summaries_read_at_a_glance() {
        assert_eq!(deps_summary(&[]), "None");
        assert_eq!(deps_summary(&["t1".into(), "t2".into()]), "t1, t2");
        assert_eq!(
            deps_summary(&["t1".into(), "t2".into(), "t3".into()]),
            "3 tasks"
        );
        assert_eq!(paths_summary(&[]), "Whole workspace");
        assert_eq!(paths_summary(&["a/**".into(), "b/**".into()]), "a/** +1");
    }
}
