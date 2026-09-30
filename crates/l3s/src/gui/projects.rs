//! The in-window Projects panel state.
//!
//! Projects are persistent records (name, machine, repository URL, optional
//! branch prefix, base directory) stored under `<root>/projects.json`.
//! This module holds the panel's list/form/worktree state; the window in
//! `mod.rs` owns rendering and key routing so the panel never becomes another
//! OS window. Nothing here touches GPUI or the daemon, so it is unit-testable
//! on its own.

use std::path::Path;

use l3s_client::{Project, load_projects, project_id_for_name as id_for, save_projects};

use super::text_input::TextField;

/// Which form field owns the keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectField {
    Name,
    Repository,
    BranchPrefix,
    Location,
}

impl ProjectField {
    /// The next field, wrapping.
    fn next(self) -> Self {
        match self {
            Self::Name => Self::Repository,
            Self::Repository => Self::BranchPrefix,
            Self::BranchPrefix => Self::Location,
            Self::Location => Self::Name,
        }
    }

    /// The previous field, wrapping.
    fn prev(self) -> Self {
        match self {
            Self::Name => Self::Location,
            Self::Repository => Self::Name,
            Self::BranchPrefix => Self::Repository,
            Self::Location => Self::BranchPrefix,
        }
    }
}

/// The panel's mode.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// The project list, with an inline new-worktree input per selection.
    List,
    /// The create/edit form.
    Form,
}

/// In-window Projects manager state.
pub struct ProjectsPanel {
    open: bool,
    projects: Vec<Project>,
    selected: usize,
    /// Whether the list cursor was placed by the keyboard and should paint.
    /// Mouse users get no selection affordance, like the machine rows.
    cursor_visible: bool,
    mode: Mode,
    /// The project being edited; `None` is a new project.
    editing: Option<String>,
    focus: Option<ProjectField>,
    name: TextField,
    repository: TextField,
    branch_prefix: TextField,
    location: TextField,
    /// Cursor into the machine choices (0 is Local, then saved machines).
    machine_cursor: usize,
    /// A delete awaiting its second confirmation.
    pending_delete: Option<String>,
    /// Inline new-worktree target project id, if any.
    worktree_target: Option<String>,
    worktree_name: TextField,
    status: String,
}

impl Default for ProjectsPanel {
    fn default() -> Self {
        Self {
            open: false,
            projects: Vec::new(),
            selected: 0,
            cursor_visible: false,
            mode: Mode::List,
            editing: None,
            focus: None,
            name: TextField::default(),
            repository: TextField::default(),
            branch_prefix: TextField::default(),
            location: TextField::default(),
            machine_cursor: 0,
            pending_delete: None,
            worktree_target: None,
            worktree_name: TextField::default(),
            status: String::new(),
        }
    }
}

impl ProjectsPanel {
    /// Whether the overlay should paint.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The loaded projects, in display order.
    pub fn projects(&self) -> &[Project] {
        &self.projects
    }

    /// The highlighted list row.
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Whether the keyboard cursor should paint its highlight.
    pub fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    /// Whether the form (rather than the list) is showing.
    pub fn in_form(&self) -> bool {
        self.mode == Mode::Form
    }

    /// The focused form field, if any.
    pub fn focus(&self) -> Option<ProjectField> {
        self.focus
    }

    /// Cursor into the machine choices.
    pub fn machine_cursor(&self) -> usize {
        self.machine_cursor
    }

    /// A delete awaiting confirmation, if any.
    pub fn pending_delete(&self) -> Option<&str> {
        self.pending_delete.as_deref()
    }

    /// Inline new-worktree target, if any.
    pub fn worktree_target(&self) -> Option<&str> {
        self.worktree_target.as_deref()
    }

    /// The inline worktree name buffer.
    pub fn worktree_name(&self) -> &TextField {
        &self.worktree_name
    }

    /// Replace the location field, moving the caret to the end.
    pub fn set_location(&mut self, dir: String) {
        self.location.set_value(dir);
        self.focus = Some(ProjectField::Location);
        self.status.clear();
    }

    /// Show a one-line status or error in the panel footer.
    pub fn set_status(&mut self, status: String) {
        self.status = status;
    }

    /// The last status or error line.
    pub fn status(&self) -> &str {
        &self.status
    }

    /// Show the panel and reload from `root`.
    pub fn open(&mut self, root: &Path) {
        self.open = true;
        self.mode = Mode::List;
        self.focus = None;
        self.pending_delete = None;
        self.worktree_target = None;
        self.worktree_name.clear();
        self.cursor_visible = false;
        self.status.clear();
        self.reload(root);
    }

    /// Hide the panel and forget transient state.
    pub fn close(&mut self) {
        self.open = false;
        self.mode = Mode::List;
        self.focus = None;
        self.editing = None;
        self.pending_delete = None;
        self.worktree_target = None;
    }

    /// Re-read the store from `root`.
    pub fn reload(&mut self, root: &Path) {
        self.projects = load_projects(root);
        self.selected = self.selected.min(self.projects.len().saturating_sub(1));
    }

    /// The highlighted project, if any.
    pub fn highlighted(&self) -> Option<&Project> {
        self.projects.get(self.selected)
    }

    /// Move the list highlight by `delta`.
    pub fn move_selection(&mut self, delta: isize) {
        if self.projects.is_empty() {
            self.selected = 0;
            return;
        }
        let last = self.projects.len() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
        self.cursor_visible = true;
    }

    /// Point the keyboard cursor at `index` without painting it.
    pub fn select(&mut self, index: usize) {
        if index < self.projects.len() {
            self.selected = index;
        }
        self.cursor_visible = false;
    }

    /// Begin creating a project.
    pub fn start_new(&mut self) {
        self.mode = Mode::Form;
        self.editing = None;
        self.focus = Some(ProjectField::Name);
        self.name.clear();
        self.repository.clear();
        self.branch_prefix.clear();
        self.location.clear();
        self.machine_cursor = 0;
        self.pending_delete = None;
        self.status.clear();
    }

    /// Begin editing the highlighted project.
    pub fn start_edit(&mut self, machine_ids: &[Option<String>]) {
        let Some(project) = self.highlighted().cloned() else {
            return;
        };
        self.mode = Mode::Form;
        self.editing = Some(project.id.clone());
        self.focus = Some(ProjectField::Name);
        self.name.set_value(project.name.clone());
        self.repository.set_value(project.repository.clone());
        self.branch_prefix.set_value(project.branch_prefix.clone());
        self.location.set_value(project.location.clone());
        self.machine_cursor = machine_ids
            .iter()
            .position(|id| *id == project.machine)
            .unwrap_or(0);
        self.pending_delete = None;
        self.status.clear();
    }

    /// Leave the form without saving.
    pub fn cancel_form(&mut self) {
        self.mode = Mode::List;
        self.focus = None;
        self.editing = None;
        self.cursor_visible = false;
        self.status.clear();
    }

    /// Cycle the form focus.
    pub fn cycle_focus(&mut self, backward: bool) {
        self.focus = Some(match self.focus {
            Some(field) if backward => field.prev(),
            Some(field) => field.next(),
            None => ProjectField::Name,
        });
    }

    /// Move the machine cursor by `delta`, clamped to `choices` rows.
    pub fn move_machine(&mut self, delta: isize, choices: usize) {
        if choices == 0 {
            self.machine_cursor = 0;
            return;
        }
        let last = choices as isize - 1;
        self.machine_cursor = (self.machine_cursor as isize + delta).clamp(0, last) as usize;
    }

    /// The form field buffer for `field`.
    pub fn field(&self, field: ProjectField) -> &TextField {
        match field {
            ProjectField::Name => &self.name,
            ProjectField::Repository => &self.repository,
            ProjectField::BranchPrefix => &self.branch_prefix,
            ProjectField::Location => &self.location,
        }
    }

    /// The form field buffer for `field`, mutably. Only tests need direct
    /// mutation; the window edits through an entity.
    #[cfg(test)]
    pub fn field_mut(&mut self, field: ProjectField) -> &mut TextField {
        match field {
            ProjectField::Name => &mut self.name,
            ProjectField::Repository => &mut self.repository,
            ProjectField::BranchPrefix => &mut self.branch_prefix,
            ProjectField::Location => &mut self.location,
        }
    }

    /// Validate and save the form to `root`.
    pub fn submit(&mut self, root: &Path, machine_ids: &[Option<String>]) -> Result<(), String> {
        let name = self.name.value().trim().to_string();
        let repository = self.repository.value().trim().to_string();
        let branch_prefix = self.branch_prefix.value().trim().to_string();
        let mut location = self.location.value().trim().to_string();
        if let Some(error) = l3s_client::validate_project(&name, &location) {
            self.status = error.clone();
            return Err(error);
        }
        // Local `~` shorthand expands at save time so sidebar matching (which
        // compares against the daemon's absolute cwd) works verbatim.
        let machine = machine_ids.get(self.machine_cursor).cloned().flatten();
        if machine.is_none() {
            location = l3s_client::expand_project_local(&location);
        }
        let id = match &self.editing {
            Some(id) => id.clone(),
            None => {
                let mut id = id_for(&name);
                if self.projects.iter().any(|project| project.id == id) {
                    let mut counter = 2u32;
                    while self.projects.iter().any(|project| project.id == id) {
                        id = format!("{}-{counter}", id_for(&name));
                        counter = counter.saturating_add(1);
                    }
                }
                id
            }
        };
        let project = Project {
            id: id.clone(),
            name,
            machine,
            repository,
            branch_prefix,
            location,
        };
        if self.editing.is_some() {
            if let Some(slot) = self.projects.iter_mut().find(|p| p.id == id) {
                *slot = project;
            } else {
                self.projects.push(project);
            }
        } else {
            self.projects.push(project);
        }
        self.projects
            .sort_by_key(|project| project.name.to_lowercase());
        save_projects(root, &self.projects).map_err(|error| {
            let message = error.to_string();
            self.status = message.clone();
            message
        })?;
        self.mode = Mode::List;
        self.focus = None;
        self.editing = None;
        self.status = "Saved.".to_string();
        self.selected = self.projects.iter().position(|p| p.id == id).unwrap_or(0);
        self.cursor_visible = false;
        Ok(())
    }

    /// Request deletion of the highlighted project; the second call confirms.
    pub fn request_delete(&mut self, root: &Path) -> DeleteOutcome {
        let Some(project) = self.highlighted().cloned() else {
            return DeleteOutcome::Empty;
        };
        if self.pending_delete.as_deref() != Some(project.id.as_str()) {
            self.pending_delete = Some(project.id.clone());
            self.status = format!("Delete `{}`? Press again to confirm.", project.name);
            return DeleteOutcome::Confirming;
        }
        self.pending_delete = None;
        self.projects.retain(|p| p.id != project.id);
        match save_projects(root, &self.projects) {
            Ok(()) => {
                self.selected = self.selected.min(self.projects.len().saturating_sub(1));
                self.status = format!("Deleted `{}`.", project.name);
                DeleteOutcome::Deleted
            }
            Err(error) => {
                self.status = error.to_string();
                DeleteOutcome::Failed
            }
        }
    }

    /// Clear a pending delete (any other key cancels the two-step).
    pub fn cancel_delete(&mut self) {
        self.pending_delete = None;
    }

    /// Open the inline new-worktree input for the highlighted project.
    pub fn start_worktree(&mut self) {
        let Some(project) = self.highlighted().cloned() else {
            return;
        };
        self.worktree_target = Some(project.id.clone());
        self.worktree_name.clear();
        self.pending_delete = None;
        self.status.clear();
    }

    /// Close the inline new-worktree input.
    pub fn cancel_worktree(&mut self) {
        self.worktree_target = None;
        self.worktree_name.clear();
    }

    /// Take the pending worktree request: the target project plus branch.
    pub fn take_worktree(&mut self) -> Option<(Project, String)> {
        let id = self.worktree_target.clone()?;
        let project = self.projects.iter().find(|p| p.id == id)?.clone();
        let name = self.worktree_name.value().trim().to_string();
        if name.is_empty() {
            self.status = "A worktree name is required.".to_string();
            return None;
        }
        let branch = project.branch_for(&name);
        self.worktree_target = None;
        self.worktree_name.clear();
        Some((project, branch))
    }
}

/// The outcome of a delete request.
pub enum DeleteOutcome {
    /// Nothing to delete.
    Empty,
    /// First press: awaiting confirmation.
    Confirming,
    /// Deleted and saved.
    Deleted,
    /// Save failed; the panel's status carries the reason.
    Failed,
}

impl ProjectsPanel {
    /// Machine choice rows are built by the window; this documents the
    /// contract: row 0 is Local (`None`), then one row per saved machine.
    pub fn machine_ids(machines: &[l3s_client::MachineInfo]) -> Vec<Option<String>> {
        let mut ids = vec![None];
        ids.extend(machines.iter().map(|m| Some(m.id.clone())));
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_submit_round_trips() {
        let dir = std::env::temp_dir().join(format!("l3s-panel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut panel = ProjectsPanel::default();
        panel.open(&dir);
        panel.start_new();
        panel.field_mut(ProjectField::Name).set_value("flo".to_string());
        panel.field_mut(ProjectField::Location).set_value("/tmp/flo".to_string());
        panel.submit(&dir, &[None]).expect("submit");
        assert_eq!(panel.projects().len(), 1);
        assert_eq!(panel.projects()[0].id, "flo");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn submit_rejects_empty_name() {
        let dir = std::env::temp_dir().join(format!("l3s-panel-bad-{}", std::process::id()));
        let mut panel = ProjectsPanel::default();
        panel.open(&dir);
        panel.start_new();
        panel.field_mut(ProjectField::Location).set_value("/tmp/x".to_string());
        assert!(panel.submit(&dir, &[None]).is_err());
    }

    #[test]
    fn location_and_status_helpers() {
        let mut panel = ProjectsPanel::default();
        panel.set_location("/tmp/picked".to_string());
        assert_eq!(panel.field(ProjectField::Location).value(), "/tmp/picked");
        assert_eq!(panel.focus(), Some(ProjectField::Location));
        panel.set_status("hello".to_string());
        assert_eq!(panel.status(), "hello");
    }

    #[test]
    fn delete_needs_two_steps() {
        let dir = std::env::temp_dir().join(format!("l3s-panel-del-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut panel = ProjectsPanel::default();
        panel.open(&dir);
        panel.start_new();
        panel.field_mut(ProjectField::Name).set_value("flo".to_string());
        panel.field_mut(ProjectField::Location).set_value("/tmp/flo".to_string());
        panel.submit(&dir, &[None]).expect("submit");
        assert!(matches!(
            panel.request_delete(&dir),
            DeleteOutcome::Confirming
        ));
        assert!(matches!(panel.request_delete(&dir), DeleteOutcome::Deleted));
        assert!(panel.projects().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
