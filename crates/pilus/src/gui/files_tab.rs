//! The native Files tab: the active workspace's file tree, as the Luvus TUI
//! shows it.
//!
//! The daemon owns the tree (`files.tree` over UHP) and shares it with the TUI,
//! and answers for whichever node it has active, so the tab follows the
//! sidebar's selection rather than carrying a root of its own. The daemon can
//! open a folder (`files.reveal`) but not close one, so folding is this
//! window's own state: a set of folded paths that hides rows without touching
//! the daemon. Clicking a file hands it to the daemon (`files.open`), which
//! shows it in the terminal card; this tab never reads file contents. The rows
//! carry no Git state, so each read also asks for the Diff's change list and
//! marks changed files and the folders above them; a click on a changed file
//! opens it in the Diff instead.
//!
//! Every call blocks on the daemon, so each runs on a background thread and
//! reports through a channel, as the Git tab does. Answers carry a sequence
//! number so a slow poll cannot overwrite the tree a later action returned.

use std::collections::HashSet;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, ClickEvent, Context, Div, ListAlignment, ListState, SharedString, div, list,
    prelude::*, px, relative,
};
use pilus_client::{
    ControlClient, Error, FileChanges, FileRow, FileTree, OpenTarget, visible_rows,
};

use super::diff::status_glyph;
use super::right_sidebar::SidebarTab;
use super::style::{self, TreeEntry, TreeMark};
use super::{EndpointId, PilusView, icon};

/// How often the open tab asks the daemon for a fresh tree, so a change made
/// in the TUI shows up here.
const POLL: Duration = Duration::from_millis(2000);
/// How often it asks while a folder is still opening: the daemon answers an
/// expand before it has read the folder, so its children land a beat later.
const LOADING_POLL: Duration = Duration::from_millis(120);
/// How long a folder may show placeholders before it counts as empty.
const LOADING_LIMIT: Duration = Duration::from_secs(3);

/// Placeholder rows shown under a folder while the daemon opens it.
const SKELETON_ROWS: usize = 3;
/// Each placeholder's share of the row, so the stack reads as names.
const SKELETON_WIDTHS: [f32; SKELETON_ROWS] = [0.55, 0.4, 0.65];

/// One read of the daemon: the tree, and what Git says changed in it.
pub(super) struct Read {
    tree: FileTree,
    changes: FileChanges,
}

/// A folder the daemon is opening.
struct Loading {
    path: String,
    /// The request that opens it: only its answer, or a later one, can show
    /// the folder's children.
    seq: u32,
    /// Past this the folder is taken to be empty.
    until: Instant,
}

/// What a background call reports back to the tab.
pub(super) enum FilesMsg {
    Tree {
        endpoint: EndpointId,
        seq: u32,
        result: Result<Read, String>,
    },
    Opened {
        endpoint: EndpointId,
        result: Result<(), String>,
    },
}

/// What a request does to the daemon before it reads the tree back.
enum Prelude {
    /// Nothing: a plain poll.
    Poll,
    /// Open a closed folder.
    Expand(String),
    /// Re-read the tree from disk.
    Refresh,
}

/// What a click on a folder does.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FolderClick {
    /// Hide its rows in this window.
    Fold,
    /// Show the rows folded earlier.
    Unfold,
    /// The daemon has it closed: ask it to open.
    Expand,
}

/// Decide a folder click from the daemon's state and this window's.
///
/// A folder the daemon has closed always expands, even when it is in `folded`:
/// the TUI closed it since, and the stale fold must not keep it shut.
fn folder_click(row: &FileRow, folded: &HashSet<String>) -> FolderClick {
    match (row.expanded, folded.contains(&row.path)) {
        (false, _) => FolderClick::Expand,
        (true, true) => FolderClick::Unfold,
        (true, false) => FolderClick::Fold,
    }
}

/// The tab's own state.
pub(super) struct FilesPanel {
    endpoint: Option<EndpointId>,
    tree: Option<FileTree>,
    /// Folders the user closed here. Only folders the daemon has open stay in
    /// it, so a folder the TUI closes and reopens comes back open.
    folded: HashSet<String>,
    /// Indices into the tree's rows that survive `folded`.
    visible: Vec<usize>,
    /// The folder whose chevron was just turned, so only it animates.
    toggled: Option<String>,
    /// The folder being opened: placeholder rows sit under it until its
    /// children arrive.
    loading: Option<Loading>,
    changes: FileChanges,
    /// Where the loading folder sits in `visible`, when it shows.
    loading_at: Option<usize>,
    list: ListState,
    error: Option<String>,
    /// Requests sent and not yet answered; a poll waits until none are.
    in_flight: u32,
    /// The sequence of the newest request, and of the newest answer taken.
    sent: u32,
    applied: u32,
    last_poll: Option<Instant>,
    tx: Sender<FilesMsg>,
    rx: Receiver<FilesMsg>,
}

impl FilesPanel {
    pub(super) fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            endpoint: None,
            tree: None,
            folded: HashSet::new(),
            visible: Vec::new(),
            toggled: None,
            loading: None,
            changes: FileChanges::default(),
            loading_at: None,
            list: ListState::new(0, ListAlignment::Top, px(600.0)),
            error: None,
            in_flight: 0,
            sent: 0,
            applied: 0,
            last_poll: None,
            tx,
            rx,
        }
    }

    /// Ask for a fresh read the next time the tab polls, such as when it
    /// comes back on screen.
    pub(super) fn mark_stale(&mut self) {
        self.last_poll = None;
    }

    /// Drop everything read from the daemon: another machine answers for
    /// other trees.
    fn reset(&mut self) {
        self.tree = None;
        self.folded.clear();
        self.visible.clear();
        self.loading = None;
        self.loading_at = None;
        self.changes = FileChanges::default();
        self.list.reset(0);
        self.toggled = None;
        self.error = None;
        self.last_poll = None;
    }

    /// Fold a finished read in; true when what is drawn changed.
    fn apply(&mut self, seq: u32, result: Result<Read, String>) -> bool {
        // An answer older than one already taken describes a tree the user
        // has since changed.
        if seq < self.applied {
            return false;
        }
        self.applied = seq;
        match result {
            Ok(Read { tree, changes }) => {
                let mut changed = self.error.take().is_some();
                if self.tree.as_ref().map(|old| &old.root) != Some(&tree.root) {
                    // Another node: its folders are not the ones folded here.
                    self.folded.clear();
                }
                let open: HashSet<&str> = tree
                    .rows
                    .iter()
                    .filter(|row| row.dir && row.expanded)
                    .map(|row| row.path.as_str())
                    .collect();
                let before = self.folded.len();
                self.folded.retain(|path| open.contains(path.as_str()));
                changed |= self.folded.len() != before;
                if self.tree.as_ref() != Some(&tree) {
                    self.tree = Some(tree);
                    changed = true;
                }
                if self.changes != changes {
                    self.changes = changes;
                    changed = true;
                }
                changed |= self.settle_loading(seq);
                changed |= self.relayout();
                changed
            }
            Err(error) => {
                let mut changed = self.error.as_ref() != Some(&error);
                self.error = Some(error);
                // The folder will not open: drop its placeholders.
                if self
                    .loading
                    .as_ref()
                    .is_some_and(|loading| seq >= loading.seq)
                {
                    self.loading = None;
                    changed = true;
                }
                changed |= self.relayout();
                changed
            }
        }
    }

    /// End the placeholders once the folder shows children, or shows none
    /// for good: the daemon closed it again, or the wait ran out. True when
    /// they ended.
    fn settle_loading(&mut self, seq: u32) -> bool {
        let (Some(loading), Some(tree)) = (&self.loading, &self.tree) else {
            return false;
        };
        if seq < loading.seq {
            return false;
        }
        let waiting = tree
            .rows
            .iter()
            .position(|row| row.path == loading.path)
            .is_some_and(|at| {
                let folder = &tree.rows[at];
                let has_children = tree
                    .rows
                    .get(at + 1)
                    .is_some_and(|next| next.depth > folder.depth);
                folder.expanded && !has_children && Instant::now() < loading.until
            });
        if waiting {
            return false;
        }
        self.loading = None;
        true
    }

    /// Recompute the rows that show, keeping the list's scroll position by
    /// row. True when the set changed.
    fn relayout(&mut self) -> bool {
        let Some(tree) = &self.tree else {
            return false;
        };
        let visible = visible_rows(&tree.rows, &self.folded);
        let loading_at = self.loading.as_ref().and_then(|loading| {
            visible
                .iter()
                .position(|&index| tree.rows[index].path == loading.path)
        });
        if visible == self.visible && loading_at == self.loading_at {
            return false;
        }
        let before = self.slot_count();
        self.visible = visible;
        self.loading_at = loading_at;
        self.list.splice(0..before, self.slot_count());
        true
    }

    /// Rows in the list: the visible tree rows plus any placeholders.
    fn slot_count(&self) -> usize {
        self.visible.len() + self.loading_at.map_or(0, |_| SKELETON_ROWS)
    }

    /// What list row `ix` shows.
    fn slot(&self, ix: usize) -> Option<Slot<'_>> {
        let tree = self.tree.as_ref()?;
        let row_at = |at: usize| tree.rows.get(*self.visible.get(at)?);
        let Some(folder) = self.loading_at else {
            return row_at(ix).map(Slot::Row);
        };
        if ix <= folder {
            row_at(ix).map(Slot::Row)
        } else if ix <= folder + SKELETON_ROWS {
            let depth = row_at(folder)?.depth + 1;
            Some(Slot::Skeleton {
                depth,
                nth: ix - folder - 1,
            })
        } else {
            row_at(ix - SKELETON_ROWS).map(Slot::Row)
        }
    }
}

/// One row of the list.
enum Slot<'a> {
    Row(&'a FileRow),
    /// A placeholder for a child still being read.
    Skeleton {
        depth: u16,
        nth: usize,
    },
}

impl PilusView {
    /// Poll the daemon and fold finished calls into the tab. Returns whether
    /// anything changed.
    pub(super) fn files_tick(&mut self) -> bool {
        let mut changed = false;
        while let Ok(message) = self.files_panel.rx.try_recv() {
            let current = self.files_panel.endpoint.clone();
            match message {
                FilesMsg::Tree {
                    endpoint,
                    seq,
                    result,
                } => {
                    self.files_panel.in_flight = self.files_panel.in_flight.saturating_sub(1);
                    if current.is_some_and(|current| current != endpoint) {
                        continue;
                    }
                    changed |= self.files_panel.apply(seq, result);
                }
                FilesMsg::Opened { endpoint, result } => {
                    if current.is_some_and(|current| current != endpoint) {
                        continue;
                    }
                    if let Err(error) = result {
                        self.files_panel.error = Some(error);
                        changed = true;
                    }
                }
            }
        }
        if !self.files_visible() {
            return changed;
        }
        if self.files_panel.endpoint.as_ref() != Some(&self.active) {
            self.files_panel.reset();
            self.files_panel.endpoint = Some(self.active.clone());
            changed = true;
        }
        let panel = &self.files_panel;
        let every = if panel.loading.is_some() {
            LOADING_POLL
        } else {
            POLL
        };
        if panel.in_flight > 0 || panel.last_poll.is_some_and(|at| at.elapsed() < every) {
            return changed;
        }
        self.files_request(Prelude::Poll);
        changed
    }

    /// Run `prelude` on the daemon, then read the tree back.
    fn files_request(&mut self, prelude: Prelude) {
        let panel = &mut self.files_panel;
        panel.in_flight += 1;
        panel.sent += 1;
        panel.last_poll = Some(Instant::now());
        let seq = panel.sent;
        let tx = panel.tx.clone();
        let endpoint = self.active.clone();
        let client = self.control_client_for_endpoint(&endpoint);
        std::thread::spawn(move || {
            let result = match client {
                None => Err("Could not reach the daemon".to_string()),
                Some(mut client) => {
                    read_tree(&mut client, prelude).map_err(|error: Error| error.to_string())
                }
            };
            tx.send(FilesMsg::Tree {
                endpoint,
                seq,
                result,
            })
            .ok();
        });
    }

    fn files_refresh_now(&mut self, cx: &mut Context<Self>) {
        self.files_request(Prelude::Refresh);
        cx.notify();
    }

    /// A click on a folder row.
    fn files_click_folder(&mut self, row: &FileRow) {
        let panel = &mut self.files_panel;
        let action = folder_click(row, &panel.folded);
        panel.toggled = Some(row.path.clone());
        match action {
            FolderClick::Fold => {
                panel.folded.insert(row.path.clone());
                panel.relayout();
            }
            FolderClick::Unfold => {
                panel.folded.remove(&row.path);
                panel.relayout();
            }
            FolderClick::Expand => {
                // A second click while it opens must not ask again.
                if panel
                    .loading
                    .as_ref()
                    .is_some_and(|loading| loading.path == row.path)
                {
                    return;
                }
                panel.folded.remove(&row.path);
                self.files_request(Prelude::Expand(row.path.clone()));
                let panel = &mut self.files_panel;
                panel.loading = Some(Loading {
                    path: row.path.clone(),
                    seq: panel.sent,
                    until: Instant::now() + LOADING_LIMIT,
                });
                panel.relayout();
            }
        }
    }

    /// A click on a file row. A file Git reports changed opens in the Diff;
    /// any other is a preview, or a pane on a double click.
    fn files_click_file(&mut self, row: &FileRow, target: OpenTarget, cx: &mut Context<Self>) {
        if let Some(change) = self.files_panel.changes.file(&row.path).cloned() {
            self.select_right_tab(SidebarTab::Diff, cx);
            self.diff_reveal(change.layer, &change.path, cx);
            return;
        }
        let endpoint = self.active.clone();
        let tx = self.files_panel.tx.clone();
        let client = self.control_client_for_endpoint(&endpoint);
        let path = row.path.clone();
        std::thread::spawn(move || {
            let result = match client {
                None => Err("Could not reach the daemon".to_string()),
                Some(mut client) => client
                    .files_open(&path, target)
                    .map_err(|error| error.to_string()),
            };
            tx.send(FilesMsg::Opened { endpoint, result }).ok();
        });
    }

    /// The tab: the root and refresh control above the tree.
    pub(super) fn files_page(&mut self, cx: &mut Context<Self>) -> Div {
        let page = div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .min_h(px(0.0))
            .child(self.files_header(cx));
        let panel = &self.files_panel;
        let Some(tree) = &panel.tree else {
            return match &panel.error {
                Some(error) => page.child(self.diff_message(error.clone())),
                None => page.child(self.files_skeleton()),
            };
        };
        if tree.rows.is_empty() {
            return page.child(self.diff_message("This folder is empty"));
        }
        let footer = panel.error.as_ref().map(|error| {
            div()
                .px_3()
                .h(px(style::scaled(&self.shared_settings, 28.0)))
                .flex()
                .items_center()
                .flex_shrink_0()
                .border_t_1()
                .border_color(self.theme.border)
                .text_size(px(style::Fonts::resolve(&self.shared_settings).secondary))
                .text_color(self.theme.alert)
                .truncate()
                .child(error.clone())
        });
        page.child(
            div().flex_1().min_h(px(0.0)).px_2().pt_2().child(
                list(
                    panel.list.clone(),
                    cx.processor(|this: &mut PilusView, ix: usize, _window, cx| {
                        this.files_row(ix, cx)
                    }),
                )
                .size_full(),
            ),
        )
        .children(footer)
    }

    /// One row: the root's name and the refresh control.
    fn files_header(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        // The whole directory, `~`-shortened against this endpoint's home: a
        // bare folder name cannot tell two checkouts apart.
        let root = self
            .files_panel
            .tree
            .as_ref()
            .map(|tree| self.shorten_cwd(&self.active.key(), &tree.root))
            .unwrap_or_default();
        let refresh = self
            .diff_icon_button(
                "pilus-files-refresh",
                include_bytes!("../../assets/icons/refresh-cw.svg"),
                "Refresh".to_string(),
                false,
                false,
            )
            .on_click(cx.listener(|this, _, _, cx| this.files_refresh_now(cx)));
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .flex_shrink_0()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .child(icon(
                include_bytes!("../../assets/icons/folder.svg"),
                fonts.body,
                theme.muted,
            ))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(root),
            )
            .child(refresh)
    }

    /// One virtualized row of the tree.
    fn files_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let row = match self.files_panel.slot(ix) {
            Some(Slot::Row(row)) => row,
            Some(Slot::Skeleton { depth, nth }) => return self.files_skeleton_row(depth, nth),
            None => return div().into_any_element(),
        };
        let folded = self.files_panel.folded.contains(&row.path);
        let loading = self
            .files_panel
            .loading
            .as_ref()
            .is_some_and(|loading| loading.path == row.path);
        let entry = TreeEntry {
            depth: row.depth,
            name: row.name.clone().into(),
            folder: row.dir.then_some(loading || (row.expanded && !folded)),
            animate: self.files_panel.toggled.as_deref() == Some(row.path.as_str()),
            mark: self.files_mark(row),
        };
        let row = row.clone();
        style::tree_row(
            SharedString::from(format!("pilus-files-row-{}", row.path)),
            entry,
            self.theme,
            &self.shared_settings,
        )
        .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
            if row.dir {
                // The second press of a double click would undo the first.
                if event.click_count() < 2 {
                    this.files_click_folder(&row);
                }
            } else {
                let target = if event.click_count() >= 2 {
                    OpenTarget::Pane
                } else {
                    OpenTarget::Preview
                };
                this.files_click_file(&row, target, cx);
            }
            cx.notify();
        }))
        .into_any_element()
    }

    /// What Git says about a row: a changed file's status letter, or a dot on
    /// a folder with a changed file below it.
    fn files_mark(&self, row: &FileRow) -> Option<TreeMark> {
        let changes = &self.files_panel.changes;
        if row.dir {
            return changes
                .dir_changed(&row.path)
                .then_some(TreeMark::Dot(self.theme.branch));
        }
        let change = changes.file(&row.path)?;
        let (letter, color) = status_glyph(self.theme, change.layer, &change.status);
        Some(TreeMark::Status(letter.to_string().into(), color))
    }

    /// A placeholder under a folder the daemon is still opening, indented
    /// like the names it stands for.
    fn files_skeleton_row(&self, depth: u16, nth: usize) -> AnyElement {
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let lead = style::tree_name_x(depth);
        div()
            .flex()
            .items_center()
            .w_full()
            .h(px(style::scaled(&self.shared_settings, 24.0)))
            .pl(px(lead))
            .pr_2()
            .child(style::skeleton(
                format!("pilus-files-loading-{nth}"),
                self.theme,
                relative(SKELETON_WIDTHS[nth % SKELETON_ROWS]),
                fonts.body,
            ))
            .into_any_element()
    }

    /// Placeholder rows until the first read lands.
    fn files_skeleton(&self) -> Div {
        div()
            .flex()
            .flex_col()
            .p_2()
            .children(self.git_skeleton("pilus-files-skeleton"))
    }
}

/// Run `prelude`, then read the tree and what changed in it.
fn read_tree(client: &mut ControlClient, prelude: Prelude) -> Result<Read, Error> {
    match prelude {
        Prelude::Poll => {}
        Prelude::Expand(dir) => client.files_expand(&dir)?,
        Prelude::Refresh => client.files_refresh()?,
    }
    let tree = client.files_tree()?;
    let changes = client.files_changes();
    Ok(Read { tree, changes })
}

/// Shot-harness entry points: a canned tree, since the harness runs without a
/// daemon.
/// A canned change list for the sample tree: an edit, an untracked file, and
/// a staged addition, spread so folders carry the dot.
#[cfg(feature = "visual-test")]
fn sample_changes() -> FileChanges {
    use pilus_client::{DiffFile, DiffLayer, DiffSnapshot};

    let change = |layer, status: &str, path: &str| DiffFile {
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
    };
    FileChanges::from_snapshot(&DiffSnapshot {
        repo: "/work/pilus".to_string(),
        branch: "main".to_string(),
        files: vec![
            change(DiffLayer::Worktree, "M", "crates/pilus-client/src/lib.rs"),
            change(
                DiffLayer::Untracked,
                "U",
                "crates/pilus-client/src/files.rs",
            ),
            change(DiffLayer::Staged, "A", "docs/files.md"),
            change(DiffLayer::Worktree, "M", "README.md"),
        ],
        omitted: 0,
        refreshing: false,
        fingerprint: String::new(),
    })
}

#[cfg(feature = "visual-test")]
impl PilusView {
    pub(super) fn files_show_sample(&mut self, cx: &mut Context<Self>) {
        use super::right_sidebar::SidebarTab;

        let row = |depth: u16, name: &str, dir: bool, expanded: bool, parent: &str| FileRow {
            depth,
            dir,
            expanded,
            name: name.to_string(),
            path: format!("/work/pilus{parent}/{name}"),
        };
        let rows = vec![
            row(0, "crates", true, true, ""),
            row(1, "pilus", true, false, "/crates"),
            row(1, "pilus-client", true, true, "/crates"),
            row(2, "src", true, true, "/crates/pilus-client"),
            row(3, "control.rs", false, false, "/crates/pilus-client/src"),
            row(3, "files.rs", false, false, "/crates/pilus-client/src"),
            row(3, "lib.rs", false, false, "/crates/pilus-client/src"),
            row(2, "tests", true, false, "/crates/pilus-client"),
            row(2, "Cargo.toml", false, false, "/crates/pilus-client"),
            row(1, "pilus-protocol", true, false, "/crates"),
            row(0, "docs", true, false, ""),
            row(0, "site", true, false, ""),
            row(0, "AGENTS.md", false, false, ""),
            row(0, "Cargo.toml", false, false, ""),
            row(0, "README.md", false, false, ""),
        ];
        self.right.visible = true;
        self.right.select_tab(SidebarTab::Files);
        let panel = &mut self.files_panel;
        panel.endpoint = Some(self.active.clone());
        // A read in flight keeps the tick from polling a daemon that is not there.
        panel.in_flight = 1;
        panel.error = None;
        panel.folded.clear();
        panel.toggled = None;
        panel.tree = Some(FileTree {
            root: "/work/pilus".to_string(),
            rows,
        });
        panel.changes = sample_changes();
        panel.relayout();
        cx.notify();
    }

    /// Show the sample folder at `path` as still opening.
    pub(super) fn files_loading_sample(&mut self, path: &str, cx: &mut Context<Self>) {
        self.files_panel.loading = Some(Loading {
            path: path.to_string(),
            seq: u32::MAX,
            until: Instant::now() + Duration::from_secs(3600),
        });
        self.files_panel.toggled = Some(path.to_string());
        self.files_panel.relayout();
        cx.notify();
    }

    /// Fold the sample folder at `path`, as a click on its row does.
    pub(super) fn files_fold_sample(&mut self, path: &str, cx: &mut Context<Self>) {
        self.files_panel.folded.insert(path.to_string());
        self.files_panel.toggled = Some(path.to_string());
        self.files_panel.relayout();
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(path: &str, depth: u16, dir: bool, expanded: bool) -> FileRow {
        FileRow {
            depth,
            dir,
            expanded,
            name: path.rsplit('/').next().unwrap_or(path).to_string(),
            path: path.to_string(),
        }
    }

    fn tree(root: &str, rows: Vec<FileRow>) -> Read {
        Read {
            tree: FileTree {
                root: root.to_string(),
                rows,
            },
            changes: FileChanges::default(),
        }
    }

    /// `path` opening, asked for by request `seq`, with `wait` left.
    fn opening(path: &str, seq: u32, wait: Duration) -> Option<Loading> {
        Some(Loading {
            path: path.to_string(),
            seq,
            until: Instant::now() + wait,
        })
    }

    fn folded(paths: &[&str]) -> HashSet<String> {
        paths.iter().map(|path| path.to_string()).collect()
    }

    #[test]
    fn an_open_folder_folds_and_unfolds_here() {
        let open = row("/w/a", 0, true, true);
        assert_eq!(folder_click(&open, &folded(&[])), FolderClick::Fold);
        assert_eq!(folder_click(&open, &folded(&["/w/a"])), FolderClick::Unfold);
    }

    #[test]
    fn a_folder_the_daemon_closed_expands_even_when_folded() {
        let closed = row("/w/a", 0, true, false);
        assert_eq!(folder_click(&closed, &folded(&[])), FolderClick::Expand);
        assert_eq!(
            folder_click(&closed, &folded(&["/w/a"])),
            FolderClick::Expand
        );
    }

    #[test]
    fn folding_hides_the_rows_below_and_unfolding_returns_them() {
        let mut panel = FilesPanel::new();
        let rows = vec![
            row("/w/a", 0, true, true),
            row("/w/a/x", 1, false, false),
            row("/w/b", 0, false, false),
        ];
        assert!(panel.apply(1, Ok(tree("/w", rows))));
        assert_eq!(panel.visible, [0, 1, 2]);
        panel.folded.insert("/w/a".to_string());
        assert!(panel.relayout());
        assert_eq!(panel.visible, [0, 2]);
        panel.folded.clear();
        assert!(panel.relayout());
        assert_eq!(panel.visible, [0, 1, 2]);
    }

    #[test]
    fn placeholders_sit_under_the_opening_folder_until_its_answer() {
        let mut panel = FilesPanel::new();
        let closed = vec![
            row("/w/a", 0, true, false),
            row("/w/b", 0, true, false),
            row("/w/c", 0, false, false),
        ];
        panel.apply(1, Ok(tree("/w", closed)));
        panel.loading = opening("/w/a", 2, LOADING_LIMIT);
        assert!(panel.relayout());
        assert_eq!(panel.slot_count(), 3 + SKELETON_ROWS);
        let shown: Vec<String> = (0..panel.slot_count())
            .map(|ix| match panel.slot(ix) {
                Some(Slot::Row(row)) => row.name.clone(),
                Some(Slot::Skeleton { depth, nth }) => format!("~{depth}.{nth}"),
                None => "?".to_string(),
            })
            .collect();
        assert_eq!(shown, ["a", "~1.0", "~1.1", "~1.2", "b", "c"]);
        assert!(panel.slot(panel.slot_count()).is_none());

        // An answer to an earlier request leaves them; the one asked for ends them.
        panel.apply(1, Ok(tree("/w", vec![row("/w/a", 0, true, false)])));
        assert_eq!(panel.loading_at, Some(0));
        let opened = vec![row("/w/a", 0, true, true), row("/w/a/x", 1, false, false)];
        assert!(panel.apply(2, Ok(tree("/w", opened))));
        assert_eq!((panel.loading.is_none(), panel.slot_count()), (true, 2));
    }

    #[test]
    fn a_failed_open_drops_the_placeholders() {
        let mut panel = FilesPanel::new();
        panel.apply(1, Ok(tree("/w", vec![row("/w/a", 0, true, false)])));
        panel.loading = opening("/w/a", 2, LOADING_LIMIT);
        panel.relayout();
        assert!(panel.apply(2, Err("down".to_string())));
        assert_eq!(panel.slot_count(), 1);
    }

    #[test]
    fn a_fold_is_dropped_once_the_daemon_closes_the_folder() {
        let mut panel = FilesPanel::new();
        let open = vec![row("/w/a", 0, true, true), row("/w/a/x", 1, false, false)];
        panel.apply(1, Ok(tree("/w", open)));
        panel.folded.insert("/w/a".to_string());
        panel.relayout();
        // The TUI closes it, then opens it again: it must not return folded.
        panel.apply(2, Ok(tree("/w", vec![row("/w/a", 0, true, false)])));
        assert!(panel.folded.is_empty());
        let reopened = vec![row("/w/a", 0, true, true), row("/w/a/x", 1, false, false)];
        panel.apply(3, Ok(tree("/w", reopened)));
        assert_eq!(panel.visible, [0, 1]);
    }

    #[test]
    fn another_root_forgets_the_folds() {
        let mut panel = FilesPanel::new();
        panel.apply(1, Ok(tree("/w", vec![row("/w/a", 0, true, true)])));
        panel.folded.insert("/w/a".to_string());
        panel.apply(2, Ok(tree("/v", vec![row("/w/a", 0, true, true)])));
        assert!(panel.folded.is_empty());
    }

    #[test]
    fn an_older_answer_does_not_replace_a_newer_one() {
        let mut panel = FilesPanel::new();
        let open = tree(
            "/w",
            vec![row("/w/a", 0, true, true), row("/w/a/x", 1, false, false)],
        );
        let closed = tree("/w", vec![row("/w/a", 0, true, false)]);
        assert!(panel.apply(2, Ok(open)));
        // The poll sent before the expand answers late.
        assert!(!panel.apply(1, Ok(closed)));
        assert_eq!(panel.visible, [0, 1]);
    }

    #[test]
    fn a_failed_read_keeps_the_tree_and_reports_once() {
        let mut panel = FilesPanel::new();
        panel.apply(1, Ok(tree("/w", vec![row("/w/a", 0, false, false)])));
        assert!(panel.apply(2, Err("down".to_string())));
        assert!(!panel.apply(3, Err("down".to_string())));
        assert!(panel.tree.is_some());
        assert!(panel.apply(4, Ok(tree("/w", vec![row("/w/a", 0, false, false)]))));
        assert!(panel.error.is_none());
    }

    #[test]
    fn placeholders_outlast_an_answer_that_opens_the_folder_before_its_children() {
        let mut panel = FilesPanel::new();
        panel.apply(1, Ok(tree("/w", vec![row("/w/a", 0, true, false)])));
        panel.loading = opening("/w/a", 2, LOADING_LIMIT);
        panel.relayout();
        // The daemon answers the expand with the folder open and still empty.
        panel.apply(2, Ok(tree("/w", vec![row("/w/a", 0, true, true)])));
        assert!(panel.loading.is_some());
        assert_eq!(panel.slot_count(), 1 + SKELETON_ROWS);
        // A later read has the children.
        let read = vec![row("/w/a", 0, true, true), row("/w/a/x", 1, false, false)];
        assert!(panel.apply(3, Ok(tree("/w", read))));
        assert!(panel.loading.is_none());
        assert_eq!(panel.slot_count(), 2);
    }

    #[test]
    fn an_empty_folder_stops_waiting_once_its_time_is_up() {
        let mut panel = FilesPanel::new();
        panel.apply(1, Ok(tree("/w", vec![row("/w/a", 0, true, false)])));
        panel.loading = opening("/w/a", 2, Duration::ZERO);
        panel.relayout();
        assert!(panel.apply(2, Ok(tree("/w", vec![row("/w/a", 0, true, true)]))));
        assert!(panel.loading.is_none());
        assert_eq!(panel.slot_count(), 1);
    }

    #[test]
    fn a_folder_the_daemon_left_closed_stops_waiting() {
        let mut panel = FilesPanel::new();
        panel.apply(1, Ok(tree("/w", vec![row("/w/a", 0, true, false)])));
        panel.loading = opening("/w/a", 2, LOADING_LIMIT);
        panel.relayout();
        panel.apply(2, Ok(tree("/w", vec![row("/w/a", 0, true, false)])));
        assert!(panel.loading.is_none());
    }

    #[test]
    fn a_changed_read_marks_the_tree_without_moving_rows() {
        let mut panel = FilesPanel::new();
        let rows = vec![row("/w/a", 0, true, true), row("/w/a/x", 1, false, false)];
        panel.apply(1, Ok(tree("/w", rows.clone())));
        let mut read = tree("/w", rows);
        read.changes = sample_changes_for("/w");
        assert!(panel.apply(2, Ok(read)));
        assert!(panel.changes.file("/w/a/x").is_some());
        assert!(panel.changes.dir_changed("/w/a"));
        assert_eq!(panel.visible, [0, 1]);
    }

    fn sample_changes_for(repo: &str) -> FileChanges {
        use pilus_client::{DiffFile, DiffLayer, DiffSnapshot};
        FileChanges::from_snapshot(&DiffSnapshot {
            repo: repo.to_string(),
            branch: "main".to_string(),
            files: vec![DiffFile {
                path: "a/x".to_string(),
                old_path: "a/x".to_string(),
                layer: DiffLayer::Worktree,
                status: "M".to_string(),
                additions: None,
                deletions: None,
                binary: false,
                notes: 0,
                viewed: false,
                modified_since_review: false,
                fingerprint: String::new(),
            }],
            omitted: 0,
            refreshing: false,
            fingerprint: String::new(),
        })
    }
}
