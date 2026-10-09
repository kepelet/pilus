//! Diff review data from the daemon's UHP `diff.*` methods.
//!
//! The daemon owns the Git scan, the bounded patch parser, and the review
//! notes; pilus only draws them. `diff.list` answers for the daemon's active
//! workspace, so the page follows whichever workspace the sidebar focuses.
//! Everything here is read-only against the repository: notes live in the
//! daemon's own store and never touch the checkout. Parsing is lenient, as in
//! [`crate::mission`]: a field an older daemon omits reads as unknown rather
//! than failing the whole snapshot.

use serde_json::{Value, json};

use crate::control::ControlClient;
use crate::error::{Error, Result};

/// Which Git layer a changed file belongs to. A file changed in both the
/// index and the working tree appears once per layer.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DiffLayer {
    Staged,
    Worktree,
    Untracked,
    Conflict,
}

impl DiffLayer {
    /// Sidebar order: conflicts first, since they block everything else.
    pub const ALL: [DiffLayer; 4] = [
        DiffLayer::Conflict,
        DiffLayer::Staged,
        DiffLayer::Worktree,
        DiffLayer::Untracked,
    ];

    pub fn wire(self) -> &'static str {
        match self {
            Self::Staged => "staged",
            Self::Worktree => "worktree",
            Self::Untracked => "untracked",
            Self::Conflict => "conflict",
        }
    }

    pub fn from_wire(text: &str) -> Option<Self> {
        Some(match text {
            "staged" => Self::Staged,
            "worktree" => Self::Worktree,
            "untracked" => Self::Untracked,
            "conflict" => Self::Conflict,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Staged => "Staged",
            Self::Worktree => "Changes",
            Self::Untracked => "Untracked",
            Self::Conflict => "Conflicts",
        }
    }
}

/// One changed file in one layer, as `diff.list` reports it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffFile {
    pub path: String,
    /// The pre-rename path; equal to `path` when the file was not renamed.
    pub old_path: String,
    pub layer: DiffLayer,
    /// Git's one-letter status (`M`, `A`, `D`, `R`, `?`, `U`).
    pub status: String,
    /// `None` until the file's diff has loaded.
    pub additions: Option<u32>,
    pub deletions: Option<u32>,
    pub binary: bool,
    /// Unresolved notes on the file.
    pub notes: u32,
    /// Whether the daemon records this version as viewed.
    pub viewed: bool,
    /// The file changed after it was last marked viewed.
    pub modified_since_review: bool,
    /// Identifies this version of the file's diff.
    pub fingerprint: String,
}

impl DiffFile {
    fn from_json(value: &Value) -> Option<Self> {
        let path = value.get("path")?.as_str()?.to_string();
        let count = |field: &str| {
            value
                .get(field)
                .and_then(Value::as_u64)
                .map(|n| n.min(u64::from(u32::MAX)) as u32)
        };
        let flag = |field: &str| value.get(field).and_then(Value::as_bool).unwrap_or(false);
        Some(Self {
            old_path: value
                .get("old_path")
                .and_then(Value::as_str)
                .unwrap_or(&path)
                .to_string(),
            layer: DiffLayer::from_wire(value.get("layer")?.as_str()?)?,
            status: text(value, "status"),
            additions: count("additions"),
            deletions: count("deletions"),
            binary: flag("binary"),
            notes: count("notes").unwrap_or(0),
            viewed: flag("viewed"),
            modified_since_review: flag("modified_since_review"),
            fingerprint: text(value, "fingerprint"),
            path,
        })
    }

    /// The identity a list row and an open selection share: a path can sit in
    /// several layers.
    pub fn key(&self) -> (DiffLayer, &str) {
        (self.layer, &self.path)
    }

    pub fn renamed(&self) -> bool {
        self.old_path != self.path
    }
}

/// The latest scan of the active workspace's repository.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DiffSnapshot {
    pub repo: String,
    pub branch: String,
    pub files: Vec<DiffFile>,
    /// Files the daemon dropped to stay inside its bounds.
    pub omitted: u32,
    /// A background refresh is still running, so a newer list is coming.
    pub refreshing: bool,
    /// Changes whenever the list does; polling compares it to skip redraws.
    pub fingerprint: String,
}

/// What one changed line is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiffLineKind {
    Context,
    Addition,
    Deletion,
}

/// One source line of a hunk, with the numbers notes anchor to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffHunk {
    pub header: String,
    pub lines: Vec<DiffLine>,
}

/// One file's bounded diff, with its hunks' lines loaded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileDiff {
    pub file: DiffFile,
    pub hunks: Vec<DiffHunk>,
    /// The patch hit a size bound and lines were dropped.
    pub truncated: bool,
    pub omitted_lines: u32,
}

impl FileDiff {
    fn from_json(value: &Value) -> Option<Self> {
        let file = DiffFile::from_json(value.get("file")?)?;
        let hunks = value
            .get("hunks")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .map(|hunk| DiffHunk {
                header: text(hunk, "header"),
                lines: hunk
                    .get("lines")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                    .iter()
                    .filter_map(line_from_json)
                    .collect(),
            })
            .collect();
        Some(Self {
            file,
            hunks,
            truncated: value
                .get("truncated")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            omitted_lines: value
                .get("omitted_lines")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32,
        })
    }
}

fn line_from_json(value: &Value) -> Option<DiffLine> {
    let number = |field: &str| {
        value
            .get(field)
            .and_then(Value::as_u64)
            .map(|n| n.min(u64::from(u32::MAX)) as u32)
    };
    Some(DiffLine {
        kind: match value.get("kind")?.as_str()? {
            "context" => DiffLineKind::Context,
            "addition" => DiffLineKind::Addition,
            "deletion" => DiffLineKind::Deletion,
            _ => return None,
        },
        old_line: number("old_line"),
        new_line: number("new_line"),
        text: text(value, "text"),
    })
}

/// The side of the diff a note anchors to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NoteSide {
    Old,
    New,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum NoteKind {
    #[default]
    Question,
    Issue,
    Suggestion,
    Praise,
}

impl NoteKind {
    pub const ALL: [NoteKind; 4] = [
        NoteKind::Question,
        NoteKind::Issue,
        NoteKind::Suggestion,
        NoteKind::Praise,
    ];

    pub fn wire(self) -> &'static str {
        match self {
            Self::Question => "question",
            Self::Issue => "issue",
            Self::Suggestion => "suggestion",
            Self::Praise => "praise",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Question => "Question",
            Self::Issue => "Issue",
            Self::Suggestion => "Suggestion",
            Self::Praise => "Praise",
        }
    }

    fn from_wire(text: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|kind| kind.wire() == text)
            .unwrap_or_default()
    }
}

/// Whether a note still points at live code. `Outdated` and `Orphaned` notes
/// lost their anchor when the code moved or went away.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NoteState {
    Open,
    Resolved,
    Outdated,
    Orphaned,
}

impl NoteState {
    fn from_wire(text: &str) -> Self {
        match text {
            "resolved" => Self::Resolved,
            "outdated" => Self::Outdated,
            "orphaned" => Self::Orphaned,
            _ => Self::Open,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::Resolved => "Resolved",
            Self::Outdated => "Outdated",
            Self::Orphaned => "Orphaned",
        }
    }
}

/// A review note anchored to a source range of one file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffNote {
    pub id: String,
    pub path: String,
    pub layer: DiffLayer,
    pub side: NoteSide,
    pub start_line: u32,
    pub end_line: u32,
    pub kind: NoteKind,
    pub state: NoteState,
    pub body: String,
    /// How many times the note was handed to an agent.
    pub deliveries: usize,
}

impl DiffNote {
    fn from_json(value: &Value) -> Option<Self> {
        let line = |field: &str| value.get(field).and_then(Value::as_u64).map(|n| n as u32);
        let start_line = line("start_line")?;
        Some(Self {
            id: value.get("id")?.as_str()?.to_string(),
            path: value.get("path")?.as_str()?.to_string(),
            layer: DiffLayer::from_wire(value.get("layer")?.as_str()?)?,
            side: match value.get("side").and_then(Value::as_str) {
                Some("old") => NoteSide::Old,
                _ => NoteSide::New,
            },
            end_line: line("end_line").unwrap_or(start_line),
            start_line,
            kind: NoteKind::from_wire(
                value
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            ),
            state: NoteState::from_wire(
                value
                    .get("state")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            ),
            body: text(value, "body"),
            deliveries: value
                .get("deliveries")
                .and_then(Value::as_array)
                .map_or(0, Vec::len),
        })
    }

    /// Whether `line` on `side` falls inside the note's range.
    pub fn covers(&self, side: NoteSide, line: u32) -> bool {
        self.side == side && (self.start_line..=self.end_line).contains(&line)
    }
}

/// Where a note's range sits, for `diff.note.add`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NoteRange {
    pub side: NoteSide,
    pub start: u32,
    pub end: u32,
}

/// How an agent handoff chooses its notes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NoteSelection {
    Ids(Vec<String>),
    AllOpen,
}

fn text(value: &Value, field: &str) -> String {
    value
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

impl ControlClient {
    /// Ask for a fresh Git scan and wait for it, so the next list is current.
    pub fn diff_refresh(&mut self) -> Result<()> {
        self.request("diff.refresh", json!({}))?;
        Ok(())
    }

    /// The latest snapshot of the active workspace's changes.
    pub fn diff_list(&mut self) -> Result<DiffSnapshot> {
        let result = self.request("diff.list", json!({}))?;
        Ok(DiffSnapshot {
            repo: text(&result, "repo"),
            branch: text(&result, "branch"),
            files: result
                .get("files")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .filter_map(DiffFile::from_json)
                .collect(),
            omitted: result.get("omitted").and_then(Value::as_u64).unwrap_or(0) as u32,
            refreshing: result
                .get("refreshing")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            fingerprint: text(&result, "fingerprint"),
        })
    }

    /// One file's bounded diff, with every line loaded.
    pub fn diff_get(&mut self, path: &str, layer: DiffLayer) -> Result<FileDiff> {
        let result = self.request(
            "diff.get",
            json!({ "path": path, "layer": layer.wire(), "include_patch": true }),
        )?;
        FileDiff::from_json(&result)
            .ok_or_else(|| Error::Control("diff.get: unrecognized reply".to_string()))
    }

    /// Notes for the whole review, or one file's.
    pub fn diff_notes(&mut self, file: Option<&str>) -> Result<Vec<DiffNote>> {
        let params = match file {
            Some(file) => json!({ "file": file }),
            None => json!({}),
        };
        let result = self.request("diff.note.list", params)?;
        Ok(notes_from(&result))
    }

    pub fn diff_note_add(
        &mut self,
        file: &str,
        layer: DiffLayer,
        range: NoteRange,
        kind: NoteKind,
        body: &str,
    ) -> Result<DiffNote> {
        let mut params = json!({
            "file": file,
            "layer": layer.wire(),
            "kind": kind.wire(),
            "body": body,
        });
        let (field, end) = (
            match range.side {
                NoteSide::Old => "old_line",
                NoteSide::New => "new_line",
            },
            range.end.max(range.start),
        );
        params[field] = json!(range.start);
        if end != range.start {
            params["end_line"] = json!(end);
        }
        let result = self.request("diff.note.add", params)?;
        result
            .get("note")
            .and_then(DiffNote::from_json)
            .ok_or_else(|| Error::Control("diff.note.add: unrecognized reply".to_string()))
    }

    pub fn diff_note_edit(&mut self, id: &str, body: &str) -> Result<()> {
        self.request("diff.note.edit", json!({ "id": id, "body": body }))?;
        Ok(())
    }

    /// Resolve an open note, or reopen a resolved one.
    pub fn diff_note_resolve(&mut self, id: &str, resolved: bool) -> Result<()> {
        let method = if resolved {
            "diff.note.resolve"
        } else {
            "diff.note.reopen"
        };
        self.request(method, json!({ "id": id }))?;
        Ok(())
    }

    /// Delete a note from the daemon's local store; the checkout is untouched.
    pub fn diff_note_remove(&mut self, id: &str) -> Result<()> {
        self.request("diff.note.remove", json!({ "id": id }))?;
        Ok(())
    }

    /// Hand notes to a live agent as one grouped message. Sending never
    /// resolves a note.
    pub fn diff_note_send(&mut self, to: &str, notes: &NoteSelection) -> Result<()> {
        let params = match notes {
            NoteSelection::Ids(ids) => json!({ "to": to, "ids": ids }),
            NoteSelection::AllOpen => json!({ "to": to, "all_open": true }),
        };
        self.request("diff.note.send", params)?;
        Ok(())
    }
}

fn notes_from(result: &Value) -> Vec<DiffNote> {
    result
        .get("notes")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(DiffNote::from_json)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_row_reads_the_daemons_shape() {
        let row = json!({
            "additions": 1, "binary": false, "deletions": null,
            "fingerprint": "abc", "layer": "worktree",
            "modified_since_review": true, "notes": 2,
            "old_path": "a.md", "path": "b.md", "status": "R", "viewed": false,
        });
        let file = DiffFile::from_json(&row).expect("row");
        assert_eq!(file.layer, DiffLayer::Worktree);
        assert_eq!((file.additions, file.deletions), (Some(1), None));
        assert!(file.renamed() && file.modified_since_review);
        assert_eq!(file.notes, 2);
    }

    #[test]
    fn unknown_layer_drops_the_row() {
        let row = json!({ "path": "a", "layer": "stash" });
        assert!(DiffFile::from_json(&row).is_none());
    }

    #[test]
    fn diff_lines_keep_both_numbers() {
        let reply = json!({
            "file": { "path": "a", "layer": "staged", "status": "M" },
            "truncated": true,
            "hunks": [{
                "header": "@@ -1,2 +1,2 @@",
                "lines": [
                    { "kind": "deletion", "old_line": 1, "new_line": null, "text": "x" },
                    { "kind": "addition", "old_line": null, "new_line": 1, "text": "y" },
                    { "kind": "mystery", "text": "skipped" },
                ],
            }],
        });
        let diff = FileDiff::from_json(&reply).expect("diff");
        assert!(diff.truncated);
        let lines = &diff.hunks[0].lines;
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].kind, DiffLineKind::Deletion);
        assert_eq!((lines[1].old_line, lines[1].new_line), (None, Some(1)));
    }

    #[test]
    fn note_parses_and_covers_its_range() {
        let note = DiffNote::from_json(&json!({
            "id": "n1", "path": "a", "layer": "worktree", "side": "old",
            "start_line": 4, "end_line": 6, "kind": "issue", "state": "outdated",
            "body": "why", "deliveries": [{}],
        }))
        .expect("note");
        assert_eq!(
            (note.kind, note.state),
            (NoteKind::Issue, NoteState::Outdated)
        );
        assert_eq!(note.deliveries, 1);
        assert!(note.covers(NoteSide::Old, 5));
        assert!(!note.covers(NoteSide::New, 5));
        assert!(!note.covers(NoteSide::Old, 7));
    }
}
