//! The daemon's orchestration tasks, as typed values.
//!
//! `task.list` and the `task.*` mutations answer with the schema's `task`
//! object. This module folds that JSON into [`Task`] so the window never
//! reads a wire field by name; unknown statuses degrade to
//! [`TaskStatus::Queued`] rather than dropping the row, because a daemon newer
//! than this client may add states.

use serde_json::Value;

/// Where a task sits in the orchestration lifecycle.
///
/// Variant order is board order: work flows top to bottom, with `Failed`
/// last.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum TaskStatus {
    Running,
    Claimed,
    Review,
    Blocked,
    Queued,
    Done,
    Merging,
    Merged,
    Failed,
}

impl TaskStatus {
    /// Every status, in board order.
    pub const ALL: [TaskStatus; 9] = [
        TaskStatus::Running,
        TaskStatus::Claimed,
        TaskStatus::Review,
        TaskStatus::Blocked,
        TaskStatus::Queued,
        TaskStatus::Done,
        TaskStatus::Merging,
        TaskStatus::Merged,
        TaskStatus::Failed,
    ];

    /// Parse the wire spelling.
    pub fn from_wire(text: &str) -> Self {
        match text {
            "running" => Self::Running,
            "claimed" => Self::Claimed,
            "review" => Self::Review,
            "blocked" => Self::Blocked,
            "done" | "completed" | "complete" => Self::Done,
            "merging" => Self::Merging,
            "merged" => Self::Merged,
            "failed" => Self::Failed,
            _ => Self::Queued,
        }
    }

    /// The wire spelling, as `task.update` accepts it.
    pub fn wire(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Claimed => "claimed",
            Self::Review => "review",
            Self::Blocked => "blocked",
            Self::Queued => "queued",
            Self::Done => "done",
            Self::Merging => "merging",
            Self::Merged => "merged",
            Self::Failed => "failed",
        }
    }

    /// The display name.
    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "Running",
            Self::Claimed => "Claimed",
            Self::Review => "In review",
            Self::Blocked => "Blocked",
            Self::Queued => "Queued",
            Self::Done => "Done",
            Self::Merging => "Merging",
            Self::Merged => "Merged",
            Self::Failed => "Failed",
        }
    }

    /// Whether the task still needs attention.
    pub fn is_open(self) -> bool {
        !matches!(self, Self::Done | Self::Merged | Self::Failed)
    }
}

/// One orchestration task.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Task {
    /// Sequential id such as `t3`.
    pub id: String,
    pub title: String,
    pub status: Option<TaskStatus>,
    /// The pane working the task, once claimed.
    pub assignee: Option<u32>,
    /// Ids of tasks that must merge first.
    pub deps: Vec<String>,
    /// Globs the task leases while it runs.
    pub paths: Vec<String>,
    /// Command run when the task is marked done.
    pub gate: Option<String>,
    pub prompt: Option<String>,
    pub notes: Vec<String>,
    /// Captured gate and agent output, oldest first.
    pub outputs: Vec<String>,
    pub branch: Option<String>,
    pub worktree: Option<String>,
    /// `worktree` or `workspace`.
    pub mode: Option<String>,
    /// Which try this is, from 1.
    pub attempt: u32,
    /// Share of the agent's context window used, in whole percent.
    pub context_percent: Option<u8>,
    /// The workspace the task belongs to, from its `project`.
    pub workspace_id: Option<String>,
    /// That workspace's root directory.
    pub project_root: Option<String>,
    /// Unix seconds.
    pub created: u64,
    /// Unix seconds.
    pub updated: u64,
}

impl Task {
    /// The task's status, defaulting to queued.
    pub fn status(&self) -> TaskStatus {
        self.status.unwrap_or(TaskStatus::Queued)
    }

    /// Fold one wire `task` object. `None` without an id.
    pub fn from_json(value: &Value) -> Option<Self> {
        let id = value.get("id")?.as_str()?.to_string();
        let text = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        };
        let list = |key: &str| -> Vec<String> {
            value
                .get(key)
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        let project = |key: &str| {
            value
                .get("project")
                .and_then(|project| project.get(key))
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        };
        let number = |key: &str| value.get(key).and_then(Value::as_u64).unwrap_or(0);
        Some(Self {
            id,
            title: text("title").unwrap_or_default(),
            status: value
                .get("status")
                .and_then(Value::as_str)
                .map(TaskStatus::from_wire),
            assignee: value
                .get("assignee")
                .and_then(Value::as_u64)
                .and_then(|pane| u32::try_from(pane).ok()),
            deps: list("deps"),
            paths: list("paths"),
            gate: text("gate"),
            prompt: text("prompt"),
            notes: list("notes"),
            outputs: list("outputs"),
            branch: text("branch"),
            worktree: text("worktree"),
            mode: text("mode"),
            attempt: u32::try_from(number("attempt")).unwrap_or(1).max(1),
            context_percent: value
                .get("context")
                .and_then(Value::as_f64)
                .map(|share| (share.clamp(0.0, 1.0) * 100.0).round() as u8),
            workspace_id: project("workspace_id"),
            project_root: project("root"),
            created: number("created"),
            updated: number("updated"),
        })
    }

    /// Fold a `task.list` result.
    pub fn list_from_json(value: &Value) -> Vec<Self> {
        value
            .get("tasks")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(Self::from_json).collect())
            .unwrap_or_default()
    }

    /// Whether any of the task's text matches `needle`, case-insensitively.
    pub fn matches(&self, needle: &str) -> bool {
        let needle = needle.trim().to_lowercase();
        needle.is_empty()
            || self.id.to_lowercase().contains(&needle)
            || self.title.to_lowercase().contains(&needle)
            || self
                .paths
                .iter()
                .any(|path| path.to_lowercase().contains(&needle))
    }
}

/// Everything `task.add` takes.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NewTask {
    pub title: String,
    pub prompt: String,
    pub paths: Vec<String>,
    pub deps: Vec<String>,
    pub gate: String,
}

/// Split a comma- or whitespace-separated field into its entries.
pub fn split_list(text: &str) -> Vec<String> {
    text.split([',', '\n', ' '])
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Task, TaskStatus, split_list};

    #[test]
    fn folds_a_wire_task() {
        let task = Task::from_json(&json!({
            "id": "t2",
            "title": "Auth refactor",
            "status": "review",
            "assignee": 4,
            "deps": ["t1"],
            "paths": ["src/auth/**"],
            "gate": "cargo test auth",
            "outputs": ["1 failed"],
            "notes": [],
            "worktree": null,
            "branch": "task/t2",
            "context": 0.456,
            "attempt": 2,
            "previous_attempts": [],
            "created": 10,
            "updated": 20,
            "project": {"workspace_id": "w1", "root": "/repo"},
        }))
        .expect("task");
        assert_eq!(task.status(), TaskStatus::Review);
        assert_eq!(task.assignee, Some(4));
        assert_eq!(task.deps, ["t1"]);
        assert_eq!(task.gate.as_deref(), Some("cargo test auth"));
        assert_eq!(task.worktree, None);
        assert_eq!(task.context_percent, Some(46));
        assert_eq!(task.attempt, 2);
        assert_eq!(task.workspace_id.as_deref(), Some("w1"));
        assert_eq!(task.project_root.as_deref(), Some("/repo"));
    }

    #[test]
    fn unknown_status_stays_visible_as_queued() {
        let task =
            Task::from_json(&json!({"id": "t9", "title": "x", "status": "parked"})).expect("task");
        assert_eq!(task.status(), TaskStatus::Queued);
        assert!(task.status().is_open());
    }

    #[test]
    fn rows_without_an_id_are_dropped() {
        let list = Task::list_from_json(&json!({"tasks": [{"title": "orphan"}, {"id": "t1"}]}));
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn terminal_statuses_are_closed() {
        for status in TaskStatus::ALL {
            let closed = matches!(
                status,
                TaskStatus::Done | TaskStatus::Merged | TaskStatus::Failed
            );
            assert_eq!(status.is_open(), !closed);
            assert_eq!(TaskStatus::from_wire(status.wire()), status);
        }
    }

    #[test]
    fn lists_split_on_commas_and_spaces() {
        assert_eq!(
            split_list("src/a/**, src/b/** t1"),
            ["src/a/**", "src/b/**", "t1"]
        );
        assert!(split_list("  ").is_empty());
    }
}
