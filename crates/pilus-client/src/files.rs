//! The workspace file tree from the daemon's UHP `files.*` methods.
//!
//! The daemon keeps one tree per active node and shares it with the Luvus TUI:
//! `files.tree` answers with the rows currently visible, already flattened with
//! a depth, and the only way to change what is open is `files.reveal`, which
//! expands the *ancestors* of a path and never the path itself. There is no
//! collapse call, so folding a folder is the window's own view state (see
//! [`visible_rows`]) and never reaches the daemon.
//!
//! The tree rows carry no Git state, so [`FileChanges`] lays the Diff's
//! `diff.list` over them: which files changed, and which folders hold one.
//!
//! Parsing is lenient, as in [`crate::git_status`]: a row without a path is
//! dropped, and a field an older daemon omits reads as empty.

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use crate::control::ControlClient;
use crate::diff::{DiffLayer, DiffSnapshot};
use crate::error::{Error, Result};

/// A child name no real file carries, revealed to open its parent.
///
/// `files.reveal` opens the ancestors of whatever path it is given without
/// checking that the path exists, so this opens a closed folder before its
/// contents are known.
const EXPAND_PROBE: &str = ".pilus-expand";

/// One visible row of the tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileRow {
    /// Nesting below the root; top-level rows are 0.
    pub depth: u16,
    pub dir: bool,
    /// Whether the daemon has the folder open. Always false for a file.
    pub expanded: bool,
    pub name: String,
    /// Absolute path on the daemon's machine.
    pub path: String,
}

impl FileRow {
    fn from_json(value: &Value) -> Option<Self> {
        let path = value.get("path")?.as_str()?.to_string();
        let name = value
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .or_else(|| path.rsplit('/').next().map(str::to_string))?;
        Some(Self {
            depth: value
                .get("depth")
                .and_then(Value::as_u64)
                .map_or(0, |depth| depth.min(u64::from(u16::MAX)) as u16),
            dir: value.get("dir").and_then(Value::as_bool).unwrap_or(false),
            expanded: value
                .get("expanded")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            name,
            path,
        })
    }
}

/// The active node's tree.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FileTree {
    /// The directory the top-level rows sit in.
    pub root: String,
    pub rows: Vec<FileRow>,
}

/// One changed file, as the Diff lists it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileChange {
    pub layer: DiffLayer,
    /// Git's one-letter status (`M`, `A`, `D`, `R`, `?`, `U`).
    pub status: String,
    /// The path the Diff names the file by, relative to the repository.
    pub path: String,
}

/// What Git reports changed, keyed by absolute path so tree rows look it up
/// directly.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FileChanges {
    files: HashMap<String, FileChange>,
    /// Every folder below the repository root that holds a changed file.
    dirs: HashSet<String>,
}

impl FileChanges {
    /// A file in two layers (staged, then edited again) keeps the first the
    /// daemon lists, which is the one the Diff opens first.
    pub fn from_snapshot(snapshot: &DiffSnapshot) -> Self {
        let repo = snapshot.repo.trim_end_matches('/');
        let mut changes = Self::default();
        for file in &snapshot.files {
            let absolute = format!("{repo}/{}", file.path);
            let mut parent = absolute.as_str();
            while let Some((dir, _)) = parent.rsplit_once('/') {
                if dir.len() <= repo.len() {
                    break;
                }
                changes.dirs.insert(dir.to_string());
                parent = dir;
            }
            changes.files.entry(absolute).or_insert_with(|| FileChange {
                layer: file.layer,
                status: file.status.clone(),
                path: file.path.clone(),
            });
        }
        changes
    }

    pub fn file(&self, absolute: &str) -> Option<&FileChange> {
        self.files.get(absolute)
    }

    /// Whether a changed file sits anywhere below the folder at `absolute`.
    pub fn dir_changed(&self, absolute: &str) -> bool {
        self.dirs.contains(absolute)
    }
}

/// Where `files.open` puts a file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenTarget {
    /// A lightweight view that adds no pane or tab.
    Preview,
    Pane,
    Tab,
}

impl OpenTarget {
    fn wire(self) -> &'static str {
        match self {
            Self::Preview => "preview",
            Self::Pane => "pane",
            Self::Tab => "tab",
        }
    }
}

/// The rows to draw once the folders in `folded` are closed in the view.
///
/// A row is hidden when any ancestor folder is folded. Rows arrive in tree
/// order with their depth, so one pass tracks the shallowest folded ancestor
/// still in effect. Returns indices into `rows`.
pub fn visible_rows(rows: &[FileRow], folded: &HashSet<String>) -> Vec<usize> {
    let mut visible = Vec::with_capacity(rows.len());
    let mut hidden_below: Option<u16> = None;
    for (index, row) in rows.iter().enumerate() {
        if let Some(depth) = hidden_below {
            if row.depth > depth {
                continue;
            }
            hidden_below = None;
        }
        visible.push(index);
        if row.dir && folded.contains(&row.path) {
            hidden_below = Some(row.depth);
        }
    }
    visible
}

impl ControlClient {
    /// The visible rows of the active node's file tree.
    pub fn files_tree(&mut self) -> Result<FileTree> {
        let result = self.request("files.tree", json!({}))?;
        let Some(rows) = result.get("rows").and_then(Value::as_array) else {
            return Err(Error::Control("files.tree: unrecognized reply".to_string()));
        };
        Ok(FileTree {
            root: result
                .get("root")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            rows: rows.iter().filter_map(FileRow::from_json).collect(),
        })
    }

    /// What Git reports changed in the active workspace.
    ///
    /// A workspace that is not a repository, or a daemon without `diff.list`,
    /// reads as nothing changed: the tree is still worth showing.
    pub fn files_changes(&mut self) -> FileChanges {
        self.diff_list()
            .map(|snapshot| FileChanges::from_snapshot(&snapshot))
            .unwrap_or_default()
    }

    /// Open `path` in the daemon's view of `target`.
    pub fn files_open(&mut self, path: &str, target: OpenTarget) -> Result<()> {
        self.request(
            "files.open",
            json!({ "path": path, "target": target.wire() }),
        )?;
        Ok(())
    }

    /// Open the folder at `dir` in the shared tree.
    pub fn files_expand(&mut self, dir: &str) -> Result<()> {
        let probe = format!("{}/{EXPAND_PROBE}", dir.trim_end_matches('/'));
        self.request("files.reveal", json!({ "path": probe }))?;
        Ok(())
    }

    /// Have the daemon re-read the tree from disk.
    pub fn files_refresh(&mut self) -> Result<()> {
        self.request("files.refresh", json!({}))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(depth: u16, dir: bool, path: &str) -> FileRow {
        FileRow {
            depth,
            dir,
            expanded: dir,
            name: path.rsplit('/').next().unwrap_or(path).to_string(),
            path: path.to_string(),
        }
    }

    fn folded(paths: &[&str]) -> HashSet<String> {
        paths.iter().map(|path| path.to_string()).collect()
    }

    #[test]
    fn row_reads_the_daemons_shape() {
        let row = FileRow::from_json(&json!({
            "depth": 2, "dir": true, "expanded": true,
            "name": "src", "path": "/w/crates/src",
        }))
        .expect("row");
        assert_eq!((row.depth, row.dir, row.expanded), (2, true, true));
        assert_eq!(
            (row.name.as_str(), row.path.as_str()),
            ("src", "/w/crates/src")
        );
    }

    #[test]
    fn row_needs_a_path_and_tolerates_the_rest() {
        assert!(FileRow::from_json(&json!({ "name": "orphan" })).is_none());
        let row = FileRow::from_json(&json!({ "path": "/w/a/b.rs" })).expect("row");
        assert_eq!(row.name, "b.rs");
        assert_eq!((row.depth, row.dir, row.expanded), (0, false, false));
    }

    fn changed(layer: DiffLayer, status: &str, path: &str) -> crate::DiffFile {
        crate::DiffFile {
            path: path.to_string(),
            old_path: path.to_string(),
            layer,
            status: status.to_string(),
            additions: None,
            deletions: None,
            binary: false,
            notes: 0,
            viewed: false,
            modified_since_review: false,
            fingerprint: String::new(),
        }
    }

    #[test]
    fn changes_are_keyed_by_absolute_path_and_mark_their_folders() {
        let snapshot = DiffSnapshot {
            repo: "/w/repo/".to_string(),
            branch: "main".to_string(),
            files: vec![
                changed(DiffLayer::Worktree, "M", "crates/a/src/lib.rs"),
                changed(DiffLayer::Untracked, "U", "notes.md"),
                // Staged, then edited again: the first listing wins.
                changed(DiffLayer::Staged, "A", "crates/a/src/lib.rs"),
            ],
            omitted: 0,
            refreshing: false,
            fingerprint: String::new(),
        };
        let changes = FileChanges::from_snapshot(&snapshot);
        let lib = changes.file("/w/repo/crates/a/src/lib.rs").expect("lib");
        assert_eq!((lib.layer, lib.status.as_str()), (DiffLayer::Worktree, "M"));
        assert_eq!(lib.path, "crates/a/src/lib.rs");
        assert!(changes.file("/w/repo/notes.md").is_some());
        assert!(changes.file("/w/repo/crates").is_none());
        for dir in ["/w/repo/crates", "/w/repo/crates/a", "/w/repo/crates/a/src"] {
            assert!(changes.dir_changed(dir), "{dir}");
        }
        // The root itself is every change's ancestor, so it says nothing.
        assert!(!changes.dir_changed("/w/repo"));
        assert!(!changes.dir_changed("/w/repo/docs"));
    }

    #[test]
    fn nothing_folded_shows_every_row() {
        let rows = [
            row(0, true, "/w/a"),
            row(1, false, "/w/a/x"),
            row(0, false, "/w/b"),
        ];
        assert_eq!(visible_rows(&rows, &HashSet::new()), [0, 1, 2]);
    }

    #[test]
    fn folding_hides_descendants_and_not_siblings() {
        let rows = [
            row(0, true, "/w/a"),
            row(1, true, "/w/a/sub"),
            row(2, false, "/w/a/sub/deep"),
            row(1, false, "/w/a/x"),
            row(0, true, "/w/b"),
            row(1, false, "/w/b/y"),
        ];
        // The folded folder itself stays; everything under it goes.
        assert_eq!(visible_rows(&rows, &folded(&["/w/a"])), [0, 4, 5]);
        // A nested fold leaves its parent's other children.
        assert_eq!(visible_rows(&rows, &folded(&["/w/a/sub"])), [0, 1, 3, 4, 5]);
    }

    #[test]
    fn an_inner_fold_under_a_folded_ancestor_changes_nothing() {
        let rows = [
            row(0, true, "/w/a"),
            row(1, true, "/w/a/sub"),
            row(2, false, "/w/a/sub/deep"),
            row(0, false, "/w/z"),
        ];
        assert_eq!(visible_rows(&rows, &folded(&["/w/a", "/w/a/sub"])), [0, 3]);
    }

    #[test]
    fn a_folded_file_path_is_ignored() {
        let rows = [row(0, false, "/w/a"), row(0, false, "/w/b")];
        assert_eq!(visible_rows(&rows, &folded(&["/w/a"])), [0, 1]);
    }
}
