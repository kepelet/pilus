//! The native Files tab: the active workspace's file tree, as the Luvus TUI
//! shows it.
//!
//! The daemon owns the tree (`files.tree` over UHP) and shares it with the TUI,
//! and answers for whichever node it has active, so the tab follows the
//! sidebar's selection rather than carrying a root of its own. The daemon can
//! open a folder (`files.reveal`) but not close one, so folding is this
//! window's own state: a set of folded paths that hides rows without touching
//! the daemon. The daemon keeps every folder open, so the folds are saved in
//! `pilus.db` (`Settings::files_folded`) and come back after a relaunch.
//! Clicking a file hands it to the daemon (`files.open`), which shows it in the
//! terminal card. The exception is a text file: the tab reads it
//! itself and opens it in place of the tree, in an editor (a markdown file is
//! drawn as a document and edited beside its preview, see `md_view`). An image
//! or an SVG is read as bytes and drawn as a picture instead (see
//! `image_view`), the SVG with a way back to its source. The rows
//! carry no Git state, so each read also asks for the Diff's change list and
//! marks changed files and the folders above them; a click on any other changed
//! file opens it in the Diff instead.
//!
//! The daemon never writes files, so the tab uploads itself, as it reads
//! documents itself: a local file is copied straight to disk, or piped over the
//! machine's SSH profile (see `pilus_client::upload_file`). The header's upload
//! button picks files, and files dropped on the tab, or on a folder row, go in
//! too. An upload never replaces a file.
//!
//! The same route serves the tree's right-click menu: create a file or folder,
//! rename, delete (see `pilus_client::create_entry`). Each asks its question in
//! the shared menu prompt, runs on a thread, and reports an [`Edit`] that the
//! tab settles: a renamed or deleted file closes or follows the document open
//! on it, and the tree is re-read from disk.
//!
//! Every call blocks on the daemon, so each runs on a background thread and
//! reports through a channel, as the Git tab does. Answers carry a sequence
//! number so a slow poll cannot overwrite the tree a later action returned.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use std::path::PathBuf;

use crossterm::event::{KeyModifiers, MouseButton as TerminalMouseButton, MouseEventKind};
use gpui::{
    AnyElement, ClickEvent, Context, Div, DragMoveEvent, ExternalPaths, FocusHandle, Focusable,
    KeyDownEvent, ListAlignment, ListState, MouseButton, MouseDownEvent, PathPromptOptions, Pixels,
    ScrollHandle, SharedString, Window, div, list, point, prelude::*, px, relative,
};
use pilus_client::{
    ControlClient, Error, FileChanges, FileRow, FileTree, OpenTarget, SshTarget, TextFile, Version,
    archive_name, create_entry, delete_entry, download_path, file_stamp, is_file, parent_of,
    read_text, rename_entry, upload_path, visible_rows, write_text,
};

use crate::settings::FilesFolded;

use super::diff::status_glyph;
use super::editor_ops::{self, Op};
use super::highlight;
use super::image_view::{self, Picture};
use super::md_view::{self, MdDoc, Page};
use super::right_sidebar::SidebarTab;
use super::selection::Cell;
use super::sidebar_menu::{Item, MenuTarget};
use super::style::{self, RadiusExt as _, TreeEntry, TreeMark};
use super::toast::ToastKind;
use super::{EndpointId, InputLayer, PilusView, PromptAction, icon};

/// How often the open tab asks the daemon for a fresh tree, so a change made
/// in the TUI shows up here.
const POLL: Duration = Duration::from_millis(2000);
/// How often it asks while a folder is still opening: the daemon answers an
/// expand before it has read the folder, so its children land a beat later.
const LOADING_POLL: Duration = Duration::from_millis(120);
/// How often an open file is checked for a change on disk. A remote check
/// is an SSH call that reads the file there, so it is no tighter than this.
const WATCH: Duration = Duration::from_secs(3);
/// How long a folder may show placeholders before it counts as empty.
const LOADING_LIMIT: Duration = Duration::from_secs(3);

/// How wide the tab must be to keep the tree beside an open document.
const TREE_BESIDE_MIN_PAGE: f32 = 640.0;
/// How wide the document column must be to hold the source beside its
/// preview, at the default interface size.
const SPLIT_MIN_DOC: f32 = 600.0;
/// The tree's width beside a document, at the default interface size.
const TREE_BESIDE_WIDTH: f32 = 240.0;
/// How wide an editor widens the column: past [`TREE_BESIDE_MIN_PAGE`], so
/// the tree stays beside the file, with room left for a line of code. The
/// window still keeps its terminal (see `right_sidebar::clamp_width`).
const EDIT_PAGE_WIDTH: f32 = 760.0;

/// Space above the editor's first line, which the caret's offset in the field
/// does not include.
pub(super) const EDITOR_PAD: f32 = 8.0;

/// Scroll `scroll` so the caret's row is in view, but only when the caret has
/// moved since `seen`: a wheel scroll is left alone. `row` is the caret row's
/// top and height within the field, as the field last painted.
pub(super) fn follow_caret(
    scroll: &ScrollHandle,
    row: Option<(Pixels, Pixels)>,
    caret: usize,
    seen: &mut Option<usize>,
) {
    if *seen == Some(caret) {
        return;
    }
    let Some((top, height)) = row else {
        return;
    };
    let view = scroll.bounds().size.height;
    if view <= px(0.0) {
        return;
    }
    let offset = scroll.offset();
    let top = top + px(EDITOR_PAD);
    let shown = -offset.y;
    if top < shown {
        scroll.set_offset(point(offset.x, -top));
    } else if top + height > shown + view {
        scroll.set_offset(point(offset.x, view - top - height));
    }
    *seen = Some(caret);
}

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
    Doc {
        endpoint: EndpointId,
        seq: u32,
        result: Result<TextFile, String>,
        /// The file's stamp, taken just before the read.
        stamp: Option<String>,
    },
    /// The open file's stamp now, for the watch that asked under `generation`.
    Stamp {
        endpoint: EndpointId,
        path: String,
        generation: u32,
        result: Result<String, String>,
    },
    /// A word a modified click landed on was checked as a file. The click
    /// went to neither the daemon nor the tab until now, so a miss replays it
    /// at `cell` with `modifiers`.
    PathProbe {
        endpoint: EndpointId,
        path: String,
        cell: Cell,
        modifiers: KeyModifiers,
        found: bool,
    },
    /// A picture was read and drawn, or why it could not be.
    Picture {
        endpoint: EndpointId,
        seq: u32,
        result: Result<Picture, String>,
    },
    Uploaded {
        endpoint: EndpointId,
        outcome: Upload,
    },
    /// A download finished: the path it was saved to, or why it was not.
    Downloaded { result: Result<PathBuf, String> },
    Edited {
        endpoint: EndpointId,
        result: Result<Edit, String>,
    },
    /// A save finished. `text` is what was written, since the buffer may have
    /// moved on while the write ran.
    Saved {
        endpoint: EndpointId,
        path: String,
        text: String,
        result: Result<(), String>,
        /// The file's stamp once written, so the watch does not take the
        /// save for someone else's change.
        stamp: Option<String>,
    },
    /// The index's copy of the edited file, which the editor's gutter marks
    /// compare against, or why there is none (most often: not tracked).
    Base {
        endpoint: EndpointId,
        path: String,
        result: Result<TextFile, String>,
    },
}

/// What one create, rename, or delete did to the tree.
#[derive(Debug, Eq, PartialEq)]
pub(super) enum Edit {
    /// A new entry, and the folder it went into.
    Created {
        path: String,
        dir: String,
    },
    Renamed {
        from: String,
        to: String,
    },
    Deleted {
        path: String,
    },
}

impl Edit {
    fn toast(&self) -> String {
        match self {
            Self::Created { path, .. } => format!("Created {}", base_name(path)),
            Self::Renamed { to, .. } => format!("Renamed to {}", base_name(to)),
            Self::Deleted { path } => format!("Deleted {}", base_name(path)),
        }
    }
}

fn base_name(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
}

/// Whether `path` is `dir` or sits anywhere below it.
fn within(path: &str, dir: &str) -> bool {
    path == dir
        || path
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Where `path` lands when `from` becomes `to`, or `None` when it is not
/// `from` or below it.
fn moved(path: &str, from: &str, to: &str) -> Option<String> {
    within(path, from).then(|| format!("{to}{}", &path[from.len()..]))
}

/// What a batch of uploads came to: the names stored, and why the rest were not.
#[derive(Debug, Default, Eq, PartialEq)]
pub(super) struct Upload {
    stored: Vec<String>,
    failed: Vec<String>,
    /// The folder they went into, by name, so a toast says where.
    into: String,
}

impl Upload {
    /// The toasts this batch earns: a success line for what went through and
    /// an error line for what did not.
    fn toasts(&self) -> Vec<(ToastKind, String)> {
        let mut toasts = Vec::new();
        let into = &self.into;
        match self.stored.as_slice() {
            [] => {}
            [name] => toasts.push((ToastKind::Success, format!("Uploaded {name} to {into}"))),
            many => toasts.push((
                ToastKind::Success,
                format!("Uploaded {} items to {into}", many.len()),
            )),
        }
        if !self.failed.is_empty() {
            toasts.push((ToastKind::Error, self.failed.join("\n")));
        }
        toasts
    }
}

/// A file open in the tab, in place of the tree.
struct FileDoc {
    path: String,
    /// Opened as text though its name says picture: an SVG's source.
    as_text: bool,
    /// The read this document waits on or came from: only its answer lands.
    seq: u32,
    state: DocState,
    /// Set once the user starts editing: the source is then in the editor's
    /// field, and the document is dirty when that differs from `base`.
    edit: Option<DocEdit>,
    /// The index's copy of the file, which the editor's gutter and the
    /// preview's blocks mark their changes against. `None` until read, or for
    /// a file Git does not track.
    git_base: Option<String>,
    /// The fingerprint of the file's Git change when `git_base` was last
    /// asked for, so it is read again only when Git's view moved. The outer
    /// `None` means it has not been asked for yet; the inner one, that Git
    /// listed no change.
    base_asked: Option<Option<String>>,
    /// How a reload finds the file again once the new read lands.
    resume: Option<Resume>,
    /// The file's stamp on disk as of the last read or save, which the watch
    /// compares against. `None` until known.
    stamp: Option<String>,
    watch: Watch,
    /// The file changed on disk under unsaved changes: its new stamp, until
    /// the user reloads or keeps theirs.
    changed_on_disk: Option<String>,
}

/// The on-disk watch of the open file.
#[derive(Default)]
struct Watch {
    /// Moves on every read and save, so a check sent before one is dropped.
    generation: u32,
    asked: Option<Instant>,
    in_flight: bool,
}

/// What a refused step does once the unsaved changes are saved or dropped.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum DocNext {
    Close,
    Reload,
    Open(String),
    /// Go from an SVG's source back to its picture.
    LeaveSource,
}

/// What a reload keeps of the view it replaces: the mode, where the caret
/// was and how far the editor was scrolled.
struct Resume {
    mode: EditMode,
    caret: usize,
    scroll: ScrollHandle,
}

/// How an edited document is shown.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EditMode {
    Source,
    /// Read-only: markdown rendered, any other file's source unchangeable.
    Preview,
    /// The source and the rendered text side by side, the preview following
    /// each keystroke.
    Split,
}

impl EditMode {
    /// Whether the editor's field is on screen.
    pub(super) fn edits(self) -> bool {
        !matches!(self, Self::Preview)
    }
}

/// A document opened for editing.
pub(super) struct DocEdit {
    /// The text on disk as of the last read or save.
    pub(super) base: String,
    /// What shows: the buffer is kept in every mode, so unsaved changes survive
    /// a look at the preview.
    pub(super) mode: EditMode,
    pub(super) saving: bool,
    pub(super) scroll: ScrollHandle,
    /// Where the caret was when the view last followed it, so the view only
    /// scrolls when the caret moves and a wheel scroll is left alone.
    pub(super) caret_seen: Option<usize>,
}

enum DocState {
    Loading,
    Ready(MdDoc),
    Picture(Picture),
    Failed(String),
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

/// Drop the folds the daemon has made moot; true when any went.
///
/// A fold goes when the daemon lists its folder closed, or when the folder's
/// parent is listed without it (it was deleted or renamed). A folder the tree
/// does not mention at all stays folded: a `files.refresh` forgets every
/// listing and the next `files.tree` comes back empty or half read, and
/// dropping the folds then would show the whole tree open on the next poll.
fn prune_folds(tree: &FileTree, folded: &mut HashSet<String>) -> bool {
    if folded.is_empty() {
        return false;
    }
    let rows = &tree.rows;
    // Each listed folder: whether the daemon has it open, and whether its
    // listing has arrived (an open folder whose children are being read shows
    // none yet).
    let mut listed: HashMap<&str, (bool, bool)> = HashMap::new();
    for (at, row) in rows.iter().enumerate().filter(|(_, row)| row.dir) {
        let read = row.expanded && rows.get(at + 1).is_some_and(|next| next.depth > row.depth);
        listed.insert(row.path.as_str(), (row.expanded, read));
    }
    let root = tree.root.trim_end_matches('/');
    let before = folded.len();
    folded.retain(|path| match listed.get(path.as_str()) {
        Some(&(open, _)) => open,
        None => match parent_of(path) {
            Some(parent) if parent.trim_end_matches('/') == root => rows.is_empty(),
            Some(parent) => !listed.get(parent).is_some_and(|&(_, read)| read),
            None => true,
        },
    });
    folded.len() != before
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
    /// Folders the user closed here. A folder the daemon closes drops out
    /// (see [`prune_folds`]), so one the TUI closes and reopens comes back open.
    folded: HashSet<String>,
    /// The folds of trees not on screen, by machine key and root. The daemon
    /// keeps every folder open for the TUI, so a fold forgotten on a switch
    /// away would come back open when the user switches back.
    shelved: HashMap<(String, String), HashSet<String>>,
    /// The folds moved since they were last saved.
    folds_changed: bool,
    /// Indices into the tree's rows that survive `folded`.
    visible: Vec<usize>,
    /// The folder whose chevron was just turned, so only it animates.
    toggled: Option<String>,
    /// The folder being opened: placeholder rows sit under it until its
    /// children arrive.
    loading: Option<Loading>,
    changes: FileChanges,
    /// The file shown instead of the tree, if one is open.
    doc: Option<FileDoc>,
    /// Document reads sent so far.
    doc_reads: u32,
    /// The tree is drawn beside the open document, as of the last frame.
    tree_beside: bool,
    /// The document column is wide enough to hold the source beside its
    /// preview, as of the last frame.
    split_ok: bool,
    /// The page is wide enough for the tree beside the open document, as of
    /// the last frame, so the header offers to hide or show it.
    tree_fits: bool,
    /// Opening an editor widened the right column, which closing the
    /// document lets back.
    auto_widened: bool,
    /// The open document's page, which holds the keyboard while it is shown
    /// read-only so its shortcuts reach it. Made with the page's first frame.
    doc_focus: Option<FocusHandle>,
    /// What to do once the save in flight lands, from the unsaved prompt.
    after_save: Option<DocNext>,
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
    /// Where files being dragged over the tab would land, and when the
    /// pointer last said so.
    drop_hint: Option<DropHint>,
    tx: Sender<FilesMsg>,
    rx: Receiver<FilesMsg>,
}

/// The folder an external drag is over: `None` for the tree's root.
struct DropHint {
    dir: Option<String>,
    at: Instant,
}

/// How long a pointer report keeps the drop hint up. Only a guard against a
/// drag some other part of the window owns; an external drag reports every
/// few milliseconds while it is over the tab.
const DROP_HINT_LIFE: Duration = Duration::from_millis(600);

impl FilesPanel {
    pub(super) fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            endpoint: None,
            tree: None,
            folded: HashSet::new(),
            shelved: HashMap::new(),
            folds_changed: false,
            visible: Vec::new(),
            toggled: None,
            loading: None,
            changes: FileChanges::default(),
            doc: None,
            doc_reads: 0,
            tree_beside: false,
            split_ok: false,
            tree_fits: false,
            auto_widened: false,
            doc_focus: None,
            after_save: None,
            loading_at: None,
            list: ListState::new(0, ListAlignment::Top, px(600.0)),
            error: None,
            in_flight: 0,
            sent: 0,
            applied: 0,
            last_poll: None,
            drop_hint: None,
            tx,
            rx,
        }
    }

    /// Note that an external drag is over `dir` (the root when `None`).
    /// True when that moves the hint, so the tab needs a repaint.
    fn hint_drop(&mut self, dir: Option<String>) -> bool {
        let moved = self.drop_hint.as_ref().is_none_or(|hint| hint.dir != dir);
        self.drop_hint = Some(DropHint {
            dir,
            at: Instant::now(),
        });
        moved
    }

    /// The folder the drag in flight is over, while one is: `Some(None)` is
    /// the root.
    fn dropping_into(&self, dragging: bool) -> Option<Option<&str>> {
        let hint = self.drop_hint.as_ref()?;
        (dragging && hint.at.elapsed() < DROP_HINT_LIFE).then_some(hint.dir.as_deref())
    }

    /// Ask for a fresh read the next time the tab polls, such as when it
    /// comes back on screen.
    pub(super) fn mark_stale(&mut self) {
        self.last_poll = None;
    }

    /// The open document's path, if the tab is showing one.
    /// The folder the tree's top-level rows sit in, once the daemon has sent it.
    pub(super) fn tree_root(&self) -> Option<String> {
        self.tree.as_ref().map(|tree| tree.root.clone())
    }

    /// How the open document is shown, once it is being edited.
    pub(super) fn edit_mode(&self) -> Option<EditMode> {
        self.doc.as_ref()?.edit.as_ref().map(|edit| edit.mode)
    }

    pub(super) fn doc_path(&self) -> Option<&str> {
        self.doc.as_ref().map(|doc| doc.path.as_str())
    }

    /// The open document page's focus, once it has been drawn.
    pub(super) fn doc_focus(&self) -> Option<FocusHandle> {
        self.doc_focus.clone()
    }

    /// Whether the open file shows its source read-only: a file with no
    /// rendered form, out of the editor.
    fn source_read_only(&self) -> bool {
        self.doc.as_ref().is_some_and(|doc| {
            !md_view::is_markdown(&doc.path)
                && matches!(doc.state, DocState::Ready(_))
                && doc
                    .edit
                    .as_ref()
                    .is_some_and(|edit| edit.mode == EditMode::Preview)
        })
    }

    /// Whether the open document is a file the editor is the only view of.
    fn doc_is_source(&self) -> bool {
        self.doc.as_ref().is_some_and(|doc| {
            doc.edit.is_none()
                && !md_view::is_markdown(&doc.path)
                && matches!(doc.state, DocState::Ready(_))
        })
    }

    /// Drop the open document, which shows the tree again.
    pub(super) fn close_doc(&mut self) {
        self.doc = None;
    }

    /// The open document, once its text has been read.
    pub(super) fn doc_view(&self) -> Option<&MdDoc> {
        match &self.doc.as_ref()?.state {
            DocState::Ready(doc) => Some(doc),
            _ => None,
        }
    }

    pub(super) fn doc_view_mut(&mut self) -> Option<&mut MdDoc> {
        match &mut self.doc.as_mut()?.state {
            DocState::Ready(doc) => Some(doc),
            _ => None,
        }
    }

    /// Fold a finished document read in. An answer for a read the user has
    /// since replaced is dropped.
    fn apply_doc(&mut self, seq: u32, result: Result<TextFile, String>) -> bool {
        let Some(doc) = self.doc.as_mut().filter(|doc| doc.seq == seq) else {
            return false;
        };
        doc.state = match result {
            Ok(file) => DocState::Ready(MdDoc::new(&file, &doc.path)),
            Err(error) => DocState::Failed(error),
        };
        true
    }

    /// Fold a finished picture read in, as [`Self::apply_doc`] does a text one.
    fn apply_picture(&mut self, seq: u32, result: Result<Picture, String>) -> bool {
        let Some(doc) = self.doc.as_mut().filter(|doc| doc.seq == seq) else {
            return false;
        };
        doc.state = match result {
            Ok(picture) => DocState::Picture(picture),
            Err(error) => DocState::Failed(error),
        };
        true
    }

    /// Drop everything read from the daemon: another machine answers for
    /// other trees.
    fn reset(&mut self) {
        self.shelve_folds();
        self.doc = None;
        self.tree = None;
        self.visible.clear();
        self.loading = None;
        self.loading_at = None;
        self.changes = FileChanges::default();
        self.list.reset(0);
        self.toggled = None;
        self.error = None;
        self.last_poll = None;
    }

    /// Set the folds of the tree on screen aside, to return to it later.
    fn shelve_folds(&mut self) {
        let Some(tree) = &self.tree else {
            return;
        };
        let key = (self.endpoint_key(), tree.root.clone());
        let folds = std::mem::take(&mut self.folded);
        if !folds.is_empty() {
            self.shelved.insert(key, folds);
        }
    }

    /// The machine the tab reads from, as the saved folds name it.
    fn endpoint_key(&self) -> String {
        self.endpoint
            .as_ref()
            .map(EndpointId::key)
            .unwrap_or_default()
    }

    /// Take the folds saved by an earlier launch. They wait on the shelf until
    /// their tree is read.
    pub(super) fn restore_folds(&mut self, saved: &FilesFolded) {
        for (endpoint, roots) in saved {
            for (root, paths) in roots {
                if !paths.is_empty() {
                    let folds = paths.iter().cloned().collect();
                    self.shelved.insert((endpoint.clone(), root.clone()), folds);
                }
            }
        }
    }

    /// Every fold, on screen and shelved, to save. Sorted so an unchanged set
    /// saves the same.
    fn saved_folds(&self) -> FilesFolded {
        let current = self
            .tree
            .as_ref()
            .map(|tree| ((self.endpoint_key(), tree.root.clone()), &self.folded));
        let mut saved = FilesFolded::new();
        for ((endpoint, root), folds) in self
            .shelved
            .iter()
            .map(|(key, folds)| (key.clone(), folds))
            .chain(current)
        {
            if folds.is_empty() {
                continue;
            }
            let mut paths: Vec<String> = folds.iter().cloned().collect();
            paths.sort();
            saved.entry(endpoint).or_default().insert(root, paths);
        }
        saved
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
                    // Another node: its folders are not the ones folded here,
                    // and its tree does not hold the open document.
                    self.shelve_folds();
                    let key = (self.endpoint_key(), tree.root.clone());
                    self.folded = self.shelved.remove(&key).unwrap_or_default();
                    if self.tree.is_some() {
                        self.doc = None;
                    }
                }
                if prune_folds(&tree, &mut self.folded) {
                    self.folds_changed = true;
                    changed = true;
                }
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
    /// Let the right column back to its own width once the document that
    /// widened it is gone.
    fn files_release_width(&mut self, cx: &mut Context<Self>) -> bool {
        let panel = &mut self.files_panel;
        if !panel.auto_widened || panel.doc.is_some() {
            return false;
        }
        panel.auto_widened = false;
        self.right.widen(None);
        if cx.reduce_motion() {
            self.right.settle();
        }
        true
    }

    /// Poll the daemon and fold finished calls into the tab. Returns whether
    /// anything changed.
    pub(super) fn files_tick(&mut self, cx: &mut Context<Self>) -> bool {
        let mut changed = self.files_release_width(cx);
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
                FilesMsg::PathProbe {
                    endpoint,
                    path,
                    cell,
                    modifiers,
                    found,
                } => {
                    if found && endpoint == self.active {
                        self.files_open_path(&path, cx);
                        changed = true;
                    } else if endpoint == self.active {
                        // Not a file here: the daemon gets the click it would
                        // have had, and may know the path better than the tab.
                        for kind in [
                            MouseEventKind::Down(TerminalMouseButton::Left),
                            MouseEventKind::Up(TerminalMouseButton::Left),
                        ] {
                            self.forward_mouse_with(kind, cell, modifiers);
                        }
                    }
                }
                FilesMsg::Doc {
                    endpoint,
                    seq,
                    result,
                    stamp,
                } => {
                    if current.is_some_and(|current| current != endpoint) {
                        continue;
                    }
                    let applied = self.files_panel.apply_doc(seq, result);
                    changed |= applied;
                    // A stale answer leaves the document as it was.
                    if applied {
                        if let Some(doc) = self.files_panel.doc.as_mut() {
                            doc.stamp = stamp;
                            doc.watch.generation += 1;
                            doc.changed_on_disk = None;
                        }
                        self.files_doc_landed(cx);
                    }
                }
                FilesMsg::Stamp {
                    endpoint,
                    path,
                    generation,
                    result,
                } => {
                    let Some(doc) = self.files_panel.doc.as_mut().filter(|doc| doc.path == path)
                    else {
                        continue;
                    };
                    doc.watch.in_flight = false;
                    if current.is_some_and(|current| current != endpoint)
                        || doc.watch.generation != generation
                    {
                        continue;
                    }
                    let stamp = match result {
                        Ok(stamp) => stamp,
                        // Gone or unreachable: a save will say so.
                        Err(error) => {
                            tracing::debug!(%path, %error, "cannot check the open file");
                            continue;
                        }
                    };
                    let known = doc.stamp.as_ref() == Some(&stamp)
                        || doc.changed_on_disk.as_ref() == Some(&stamp);
                    if !known {
                        self.files_disk_changed(stamp, cx);
                        changed = true;
                    }
                }
                FilesMsg::Picture {
                    endpoint,
                    seq,
                    result,
                } => {
                    if current.is_some_and(|current| current != endpoint) {
                        continue;
                    }
                    changed |= self.files_panel.apply_picture(seq, result);
                }
                FilesMsg::Saved {
                    endpoint,
                    path,
                    text,
                    result,
                    stamp,
                } => {
                    changed = true;
                    if current.is_some_and(|current| current != endpoint) {
                        continue;
                    }
                    self.files_saved(&path, text, result, stamp, cx);
                }
                FilesMsg::Base {
                    endpoint,
                    path,
                    result,
                } => {
                    if current.is_some_and(|current| current != endpoint) {
                        continue;
                    }
                    let Some(doc) = self.files_panel.doc.as_mut().filter(|doc| doc.path == path)
                    else {
                        continue;
                    };
                    let base = match result {
                        // Half a base would mark the missing half removed.
                        Ok(file) if !file.truncated => Some(file.text),
                        Ok(_) => None,
                        Err(error) => {
                            tracing::debug!(%path, %error, "no index copy for the editor's marks");
                            None
                        }
                    };
                    if let DocState::Ready(md) = &mut doc.state {
                        md.forget_marks();
                    }
                    // The editor's field serves whichever file is edited, so
                    // it takes the copy only while this one is.
                    if doc.edit.is_some() {
                        let base = base.clone();
                        self.md_editor_input
                            .update(cx, |input, cx| input.set_diff_base(base, cx));
                    }
                    doc.git_base = base;
                    // The header's counts come from the same marks.
                    changed = true;
                }
                FilesMsg::Uploaded { endpoint, outcome } => {
                    for (kind, text) in outcome.toasts() {
                        self.toast(kind, text);
                    }
                    changed = true;
                    if current.is_some_and(|current| current != endpoint) {
                        continue;
                    }
                    // Show what landed without waiting for the next poll.
                    self.files_request(Prelude::Refresh);
                }
                FilesMsg::Downloaded { result } => {
                    changed = true;
                    match result {
                        Ok(saved) => {
                            let name = saved
                                .file_name()
                                .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                            self.toast(ToastKind::Success, format!("Downloaded {name}"));
                        }
                        Err(error) => self.toast(ToastKind::Error, error),
                    }
                }
                FilesMsg::Edited { endpoint, result } => {
                    changed = true;
                    match result {
                        Err(error) => self.toast(ToastKind::Error, error),
                        Ok(edit) => {
                            self.toast(ToastKind::Success, edit.toast());
                            if current.is_none_or(|current| current == endpoint) {
                                self.files_settle(&edit);
                            }
                        }
                    }
                }
            }
        }
        self.files_save_folds();
        // A new edit, or a commit, stage or write that moved the file's Git
        // change, wants the gutter's base read (again). Cheap when it does not.
        self.files_read_base();
        if !self.files_visible() {
            return changed;
        }
        changed |= self.files_follow_endpoint();
        self.files_watch_doc();
        let panel = &self.files_panel;
        let every = if panel.loading.is_some() {
            LOADING_POLL
        } else {
            super::activity::scaled(POLL)
        };
        if panel.in_flight > 0 || panel.last_poll.is_some_and(|at| at.elapsed() < every) {
            return changed;
        }
        self.files_request(Prelude::Poll);
        changed
    }

    /// Point the tab at the active machine, dropping what another machine's
    /// tree left. Returns whether it did.
    ///
    /// Opening a document starts by calling this: a tab that was never on
    /// screen has no endpoint, so its first visible tick would reset it and
    /// drop the document just requested.
    fn files_follow_endpoint(&mut self) -> bool {
        if self.files_panel.endpoint.as_ref() == Some(&self.active) {
            return false;
        }
        self.files_panel.reset();
        self.files_panel.endpoint = Some(self.active.clone());
        true
    }

    /// Bring the tab in line with an edit that went through: move or close the
    /// document open on the entry, then read the tree back from disk.
    fn files_settle(&mut self, edit: &Edit) {
        let panel = &mut self.files_panel;
        let prelude = match edit {
            Edit::Created { dir, .. } => {
                // A new entry in a folder the daemon has closed only shows once
                // the folder opens.
                let closed = panel.tree.as_ref().is_some_and(|tree| {
                    tree.rows
                        .iter()
                        .any(|row| row.dir && !row.expanded && row.path == *dir)
                });
                if closed {
                    panel.folds_changed |= panel.folded.remove(dir);
                    Prelude::Expand(dir.clone())
                } else {
                    Prelude::Refresh
                }
            }
            Edit::Renamed { from, to } => {
                let reopen = panel.doc_path().and_then(|open| moved(open, from, to));
                if let Some(path) = reopen {
                    self.files_read_doc(path);
                }
                Prelude::Refresh
            }
            Edit::Deleted { path } => {
                if panel.doc_path().is_some_and(|open| within(open, path)) {
                    panel.close_doc();
                }
                Prelude::Refresh
            }
        };
        self.files_save_folds();
        self.files_request(prelude);
    }

    /// Write the folds to `pilus.db` when they moved, so a relaunch shows the
    /// tree as it was left.
    fn files_save_folds(&mut self) {
        if !std::mem::take(&mut self.files_panel.folds_changed) {
            return;
        }
        let saved = self.files_panel.saved_folds();
        if self.settings.files_folded != saved {
            self.settings.files_folded = saved.clone();
            self.settings.save(&self.pilus_root);
            // A window opened later in this process, such as from the menu
            // bar after this one closed, starts from the shared copy.
            self.shared_settings.borrow_mut().files_folded = saved;
        }
    }

    /// Put the tree beside an open document away, or bring it back, and
    /// remember that for the next launch when `save`.
    pub(super) fn files_set_tree_hidden(&mut self, hidden: bool, save: bool) {
        self.settings.files_tree_hidden = hidden;
        // `sync_settings` copies the shared copy over this one each tick.
        self.shared_settings.borrow_mut().files_tree_hidden = hidden;
        if save {
            self.settings.save(&self.pilus_root);
        }
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

    /// Show the markdown file at `path` in place of the tree, reading it off
    /// the GUI thread.
    pub(super) fn files_open_doc(&mut self, path: &str, cx: &mut Context<Self>) {
        if self.files_doc_unsaved(DocNext::Open(path.to_string()), cx) {
            return;
        }
        self.files_follow_endpoint();
        let path = path.to_string();
        // A path clicked in the terminal arrives with the column away, and a
        // document read into a hidden column is a click that seems to do
        // nothing, so bring the column back first.
        if !self.right.visible {
            self.toggle_right_sidebar(cx);
        }
        self.right_navigate(cx, |this, _| {
            this.right.select_tab(SidebarTab::Files);
            this.files_read_doc(path);
        });
    }

    /// Check on a worker whether the word a click landed on names a file under
    /// the tree's root, then open it here or hand the click back to the daemon.
    ///
    /// The root is the tab's own when it has one. With the sidebar closed the
    /// tab is not polling, so the worker asks the daemon for the active node's
    /// tree instead. Returns false, leaving the click to the caller, when there
    /// is no way to reach the machine. The GUI thread never waits on the
    /// `stat`, which is an SSH call for a remote machine.
    pub(super) fn files_probe_path(&self, word: &str, cell: Cell, modifiers: KeyModifiers) -> bool {
        let endpoint = self.active.clone();
        let Ok(target) = self.read_target(&endpoint) else {
            return false;
        };
        let shown_root = self.files_panel.tree_root();
        let client = if shown_root.is_none() {
            let Some(client) = self.control_client_for_endpoint(&endpoint) else {
                return false;
            };
            Some(client)
        } else {
            None
        };
        let word = word.to_string();
        let tx = self.files_panel.tx.clone();
        std::thread::spawn(move || {
            let root = shown_root.or_else(|| {
                client?
                    .files_tree()
                    .ok()
                    .map(|tree| tree.root)
                    .filter(|root| !root.is_empty())
            });
            let path = match root {
                _ if word.starts_with('/') => word.clone(),
                Some(root) => format!(
                    "{}/{}",
                    root.trim_end_matches('/'),
                    word.trim_start_matches("./")
                ),
                None => String::new(),
            };
            let found = !path.is_empty() && is_file(target.as_ref(), &path);
            tx.send(FilesMsg::PathProbe {
                endpoint,
                path,
                cell,
                modifiers,
                found,
            })
            .ok();
        });
        true
    }

    /// Open the file at `path` as a click on its row would, for a caller that
    /// holds the path but not the row.
    pub(super) fn files_open_path(&mut self, path: &str, cx: &mut Context<Self>) {
        let row = FileRow {
            depth: 0,
            dir: false,
            expanded: false,
            name: base_name(path).to_string(),
            path: path.to_string(),
        };
        self.files_click_file(&row, OpenTarget::Preview, cx);
    }

    /// Start (or restart) the read of the document at `path`.
    pub(super) fn files_read_doc(&mut self, path: String) {
        self.files_read_doc_as(path, false);
    }

    /// Start the read of `path`: a picture when its name says so, unless
    /// `as_text` asks for the file's text, as an SVG's source.
    fn files_read_doc_as(&mut self, path: String, as_text: bool) {
        self.quick_open.note_opened(&path);
        let endpoint = self.active.clone();
        let target = match self.read_target(&endpoint) {
            Ok(target) => target,
            Err(error) => {
                self.files_panel.doc_reads += 1;
                self.files_panel.doc = Some(FileDoc {
                    path,
                    as_text,
                    seq: self.files_panel.doc_reads,
                    state: DocState::Failed(error),
                    edit: None,
                    git_base: None,
                    base_asked: None,
                    resume: None,
                    stamp: None,
                    watch: Watch::default(),
                    changed_on_disk: None,
                });
                return;
            }
        };
        let panel = &mut self.files_panel;
        panel.doc_reads += 1;
        let seq = panel.doc_reads;
        let picture = if as_text {
            None
        } else {
            image_view::kind(&path)
        };
        // A reload keeps what was on screen until the new read lands, as long
        // as it is the same kind of view.
        let keep = panel.doc.take().filter(|doc| doc.path == path);
        // Git's copy stays too, so the marks hold until it is read again.
        let (kept, git_base) = keep.map_or((None, None), |doc| (Some(doc.state), doc.git_base));
        panel.doc = Some(FileDoc {
            path: path.clone(),
            as_text,
            seq,
            state: match kept {
                Some(DocState::Ready(doc)) if picture.is_none() => DocState::Ready(doc),
                Some(DocState::Picture(shown)) if picture.is_some() => DocState::Picture(shown),
                _ => DocState::Loading,
            },
            edit: None,
            git_base,
            base_asked: None,
            resume: None,
            stamp: None,
            watch: Watch::default(),
            changed_on_disk: None,
        });
        let tx = panel.tx.clone();
        std::thread::spawn(move || {
            if let Some(kind) = picture {
                let result = image_view::load(target.as_ref(), &path, kind);
                tx.send(FilesMsg::Picture {
                    endpoint,
                    seq,
                    result,
                })
                .ok();
                return;
            }
            // Stamped first: a change during the read then shows as a change.
            let stamp = file_stamp(target.as_ref(), &path).ok();
            let result = read_text(target.as_ref(), Version::Disk(&path));
            tx.send(FilesMsg::Doc {
                endpoint,
                seq,
                result,
                stamp,
            })
            .ok();
        });
    }

    /// Whether the open document has changes that are not on disk.
    fn files_doc_dirty(&self, cx: &Context<Self>) -> bool {
        self.files_panel
            .doc
            .as_ref()
            .and_then(|doc| doc.edit.as_ref())
            .is_some_and(|edit| !self.md_editor_input.read(cx).holds(&edit.base))
    }

    /// Hold `next` back and ask Save, Don't Save or Cancel when the open
    /// document has unsaved changes: closing, reloading or replacing it would
    /// throw them away silently. True when it was held back.
    fn files_doc_unsaved(&mut self, next: DocNext, cx: &mut Context<Self>) -> bool {
        if !self.files_doc_dirty(cx) {
            return false;
        }
        let Some(path) = self.files_panel.doc_path().map(str::to_string) else {
            return false;
        };
        self.open_menu_prompt(
            PromptAction::UnsavedDoc { path, next },
            "Save Changes?",
            "",
            "",
            cx,
        );
        true
    }

    /// Whether the markdown editor's field holds the keyboard.
    ///
    /// Only while the column does: a press on the terminal clears
    /// `right.focused`, and without that check `sync_focus` would hand the
    /// keyboard back to the editor on the next frame, so the terminal could
    /// never be typed in while a file was open.
    pub(super) fn md_editing(&self) -> bool {
        self.files_doc_keys()
            && self
                .files_panel
                .doc
                .as_ref()
                .and_then(|doc| doc.edit.as_ref())
                .is_some_and(|edit| edit.mode.edits())
    }

    /// Whether the file open in the Files tab holds the keyboard, edited or
    /// read-only, so its shortcuts (`FilesDoc`) reach it. The same rule as
    /// [`Self::md_editing`]: only while the column does.
    pub(super) fn files_doc_keys(&self) -> bool {
        self.files_visible()
            && self.right.focused
            && self.input_layer() == InputLayer::Terminal
            && self.files_panel.doc.is_some()
    }

    /// Whether the open file's read-only source is on screen, in the
    /// editor's field.
    pub(super) fn files_source_shown(&self) -> bool {
        self.files_panel.source_read_only()
    }

    /// Keys the open file leaves to the window: the find bar's, then
    /// Escape, which stops editing.
    pub(super) fn md_edit_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.find_key(event, window, cx) {
            return;
        }
        if event.keystroke.key != "escape" {
            return;
        }
        // Escape steps back: out of the editor first, then from a read-only
        // file to the terminal, which takes the keyboard again.
        if self.md_editing() {
            self.files_preview_doc(cx);
        } else {
            self.right.focused = false;
            cx.notify();
        }
    }

    /// Ask for a line to go to in the open file, with the window's prompt.
    pub(super) fn files_ask_line(&mut self, cx: &mut Context<Self>) {
        let editable = self.files_panel.doc_view().is_some_and(MdDoc::editable);
        if !editable {
            return;
        }
        self.open_menu_prompt(
            PromptAction::GoToLine,
            "Go to Line",
            "A line, or line:column",
            "",
            cx,
        );
    }

    /// Put the editor's caret on `answer`'s line (`12`, or `12:5`). A
    /// rendered markdown file switches to its source to show the caret.
    pub(super) fn files_go_to_line(&mut self, answer: &str, cx: &mut Context<Self>) {
        let mut parts = answer.split(':').map(str::trim);
        let line = parts.next().and_then(|line| line.parse::<usize>().ok());
        let column = parts
            .next()
            .and_then(|column| column.parse::<usize>().ok())
            .unwrap_or(1);
        let Some(line) = line.filter(|&line| line > 0) else {
            self.toast(ToastKind::Error, "Type a line number, such as 12 or 12:5");
            return;
        };
        if self
            .files_panel
            .edit_mode()
            .is_none_or(|mode| !mode.edits())
            && md_view::is_markdown(self.files_panel.doc_path().unwrap_or_default())
        {
            self.files_begin_edit(cx);
        }
        // The keyboard comes back to the file the prompt was asked from.
        self.right.focused = true;
        self.md_editor_input.update(cx, |input, cx| {
            input.go_to_line(line, column.saturating_sub(1), cx);
        });
        cx.notify();
    }

    /// Cmd+W, and the header's close button: a file being edited goes
    /// read-only, and a read-only one closes.
    pub(super) fn files_step_out(&mut self, cx: &mut Context<Self>) {
        let editing = self.files_panel.edit_mode().is_some_and(EditMode::edits);
        if editing {
            self.files_preview_doc(cx);
        } else {
            self.files_close_doc(cx);
        }
    }

    /// A read landed for the open document: a file with no rendered form
    /// opens in the editor, and a reload finds the view it left.
    fn files_doc_landed(&mut self, cx: &mut Context<Self>) {
        let resume = self
            .files_panel
            .doc
            .as_mut()
            .and_then(|doc| doc.resume.take());
        if resume.is_none() && !self.files_panel.doc_is_source() {
            return;
        }
        self.files_begin_edit(cx);
        let Some(resume) = resume else {
            return;
        };
        let Some(edit) = self
            .files_panel
            .doc
            .as_mut()
            .and_then(|doc| doc.edit.as_mut())
        else {
            return;
        };
        edit.mode = resume.mode;
        edit.scroll = resume.scroll;
        // The caret goes back without the view chasing it: the scroll is
        // already where it was.
        let caret = self.md_editor_input.update(cx, |input, cx| {
            let caret =
                input.with_text(|text| text.floor_char_boundary(resume.caret.min(text.len())));
            input.select_range(caret..caret, cx);
            caret
        });
        edit.caret_seen = Some(caret);
        if resume.mode == EditMode::Preview {
            self.files_sync_preview(cx);
        }
    }

    /// Show the open document's source in the editor.
    fn files_edit_doc(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.files_begin_edit(cx);
        window.focus(&self.md_editor_input.focus_handle(cx), cx);
    }

    /// Switch the open document to its source, once it is editable.
    pub(super) fn files_begin_edit(&mut self, cx: &mut Context<Self>) {
        let Some(doc) = self.files_panel.doc.as_mut() else {
            return;
        };
        let DocState::Ready(md) = &doc.state else {
            return;
        };
        if !md.editable() {
            return;
        }
        let mut started = false;
        match doc.edit.as_mut() {
            Some(edit) if edit.mode == EditMode::Preview => edit.mode = EditMode::Source,
            Some(_) => {}
            None => {
                started = true;
                let text = md.source().to_string();
                let path = doc.path.clone();
                let git_base = doc.git_base.clone();
                doc.edit = Some(DocEdit {
                    base: text.clone(),
                    mode: EditMode::Source,
                    saving: false,
                    scroll: ScrollHandle::new(),
                    caret_seen: Some(0),
                });
                // One field serves every document, so the grammar and the Git
                // marks follow the file it was opened for, and a search of the
                // last one is over. A markdown file was most likely previewed
                // first, so its Git copy is already here.
                self.file_find.reset();
                self.md_editor_input.update(cx, |input, cx| {
                    input.set_marks(Vec::new(), cx);
                    input.set_diff_base(git_base, cx);
                    input.set_syntax(Some(path), cx);
                    input.set_value_at_start(text, cx);
                });
            }
        }
        // An editor wants room: the column widens until the tree fits beside
        // the file, short of the whole window, and settles back when the file
        // closes.
        if started && !self.right.expanded() {
            self.files_panel.auto_widened = true;
            self.right.widen(Some(EDIT_PAGE_WIDTH));
            if cx.reduce_motion() {
                self.right.settle();
            }
        }
        cx.notify();
    }

    /// Show the open document rendered, from the editor's text.
    pub(super) fn files_preview_doc(&mut self, cx: &mut Context<Self>) {
        let Some(doc) = self.files_panel.doc.as_mut() else {
            return;
        };
        let Some(edit) = doc.edit.as_mut() else {
            return;
        };
        edit.mode = EditMode::Preview;
        // The bar searches the editor, which is no longer being edited.
        if self.file_find.is_open() {
            self.file_find.reset();
            self.md_editor_input
                .update(cx, |input, cx| input.set_marks(Vec::new(), cx));
        }
        self.files_sync_preview(cx);
        cx.notify();
    }

    /// Bring the rendered document up to the editor's text.
    fn files_sync_preview(&mut self, cx: &mut Context<Self>) {
        let input = self.md_editor_input.read(cx);
        let Some(DocState::Ready(md)) = self.files_panel.doc.as_mut().map(|doc| &mut doc.state)
        else {
            return;
        };
        if !input.holds(md.source()) {
            md.replace_source(input.value());
        }
        self.files_mark_preview();
    }

    /// Bring the preview's Git marks up to date with its text, before its
    /// rows are drawn: a row drawn before would show none for a frame.
    fn files_mark_preview(&mut self) {
        if let Some(FileDoc {
            state: DocState::Ready(md),
            git_base,
            ..
        }) = self.files_panel.doc.as_mut()
        {
            md.mark_against(git_base.as_deref());
        }
    }

    /// Mark the open document as changed on disk, without a disk.
    #[cfg(feature = "visual-test")]
    pub(super) fn files_mark_changed_on_disk(&mut self) {
        if let Some(doc) = self.files_panel.doc.as_mut() {
            doc.changed_on_disk = Some("sample".to_string());
        }
    }

    /// Put the open document in side-by-side mode, without the window.
    #[cfg(feature = "visual-test")]
    pub(super) fn files_set_split(&mut self) {
        if let Some(edit) = self
            .files_panel
            .doc
            .as_mut()
            .and_then(|doc| doc.edit.as_mut())
        {
            edit.mode = EditMode::Split;
        }
    }

    /// Give the open edit `base` as its Git copy, without reading Git, so the
    /// gutter's marks show in a capture.
    #[cfg(feature = "visual-test")]
    pub(super) fn files_set_sample_base(&mut self, base: &str, cx: &mut Context<Self>) {
        let panel = &mut self.files_panel;
        let Some(doc) = panel.doc.as_mut() else {
            return;
        };
        let fingerprint = panel
            .changes
            .file(&doc.path)
            .map(|change| change.fingerprint.clone());
        doc.base_asked = Some(fingerprint);
        doc.git_base = Some(base.to_string());
        if let DocState::Ready(md) = &mut doc.state {
            md.forget_marks();
        }
        if doc.edit.is_some() {
            self.md_editor_input.update(cx, |input, cx| {
                input.set_diff_base(Some(base.to_string()), cx)
            });
        }
    }

    /// Show the source and the rendered text side by side, or go back to the
    /// source alone. A column too narrow for both is widened.
    fn files_split_doc(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.files_begin_edit(cx);
        let Some(edit) = self
            .files_panel
            .doc
            .as_mut()
            .and_then(|doc| doc.edit.as_mut())
        else {
            return;
        };
        edit.mode = if edit.mode == EditMode::Split {
            EditMode::Source
        } else {
            EditMode::Split
        };
        if edit.mode == EditMode::Split && !self.files_panel.split_ok && !self.right.expanded() {
            self.right.toggle_expanded();
            if cx.reduce_motion() {
                self.right.settle();
            }
        }
        window.focus(&self.md_editor_input.focus_handle(cx), cx);
        cx.notify();
    }

    /// Put the editor back to the text on disk.
    fn files_revert_doc(&mut self, cx: &mut Context<Self>) {
        let Some(base) = self
            .files_panel
            .doc
            .as_ref()
            .and_then(|doc| doc.edit.as_ref())
            .map(|edit| edit.base.clone())
        else {
            return;
        };
        self.md_editor_input
            .update(cx, |input, cx| input.set_value_at_start(base, cx));
        if let Some(edit) = self
            .files_panel
            .doc
            .as_mut()
            .and_then(|doc| doc.edit.as_mut())
        {
            edit.caret_seen = Some(0);
        }
        cx.notify();
    }

    /// Write the editor's text to the open document, off the GUI thread.
    pub(super) fn files_save_doc(&mut self, cx: &mut Context<Self>) {
        let Some(doc) = self.files_panel.doc.as_ref() else {
            return;
        };
        let Some(edit) = doc.edit.as_ref() else {
            return;
        };
        if edit.saving || !self.files_doc_dirty(cx) {
            return;
        }
        let endpoint = self.active.clone();
        let target = match self.read_target(&endpoint) {
            Ok(target) => target,
            Err(error) => {
                self.toast(ToastKind::Error, error);
                return;
            }
        };
        let path = doc.path.clone();
        let text = self.md_editor_input.read(cx).value();
        if let Some(doc) = self.files_panel.doc.as_mut() {
            // A check already on its way would see the save as a change.
            doc.watch.generation += 1;
            if let Some(edit) = doc.edit.as_mut() {
                edit.saving = true;
            }
        }
        let tx = self.files_panel.tx.clone();
        std::thread::spawn(move || {
            let result = write_text(target.as_ref(), &path, &text);
            let stamp = result
                .is_ok()
                .then(|| file_stamp(target.as_ref(), &path).ok())
                .flatten();
            tx.send(FilesMsg::Saved {
                endpoint,
                path,
                text,
                result,
                stamp,
            })
            .ok();
        });
        cx.notify();
    }

    /// Read the index's copy of the open file on a worker, for the editor's
    /// gutter and the preview's blocks, unless Git's change for the file is
    /// the one it was last read for. `git show` runs where the file lives,
    /// over SSH for a remote machine.
    fn files_read_base(&mut self) {
        let panel = &mut self.files_panel;
        // Only text has lines to mark: not a picture, nor a read in flight.
        let Some(doc) = panel
            .doc
            .as_mut()
            .filter(|doc| matches!(doc.state, DocState::Ready(_)))
        else {
            return;
        };
        let fingerprint = panel
            .changes
            .file(&doc.path)
            .map(|change| change.fingerprint.as_str());
        if doc.base_asked.as_ref().map(Option::as_deref) == Some(fingerprint) {
            return;
        }
        doc.base_asked = Some(fingerprint.map(str::to_string));
        let path = doc.path.clone();
        let endpoint = self.active.clone();
        let target = match self.read_target(&endpoint) {
            Ok(target) => target,
            Err(error) => {
                tracing::debug!(%path, %error, "cannot read the editor's index copy");
                return;
            }
        };
        let tx = self.files_panel.tx.clone();
        std::thread::spawn(move || {
            let (dir, name) = super::diff::split_path(&path);
            let dir = if dir.is_empty() { "/" } else { dir };
            // `./name` is read relative to the folder, so the repository's
            // root is never needed.
            let relative = format!("./{name}");
            let result = read_text(
                target.as_ref(),
                Version::Index {
                    repo: dir,
                    path: &relative,
                },
            );
            tx.send(FilesMsg::Base {
                endpoint,
                path,
                result,
            })
            .ok();
        });
    }

    /// Settle a finished save: what was written is now the text on disk.
    fn files_saved(
        &mut self,
        path: &str,
        text: String,
        result: Result<(), String>,
        stamp: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let name = super::diff::split_path(path).1.to_string();
        if let Some(doc) = self.files_panel.doc.as_mut().filter(|doc| doc.path == path) {
            if result.is_ok() {
                doc.stamp = stamp;
                doc.watch.generation += 1;
                doc.changed_on_disk = None;
            }
            if let Some(edit) = doc.edit.as_mut() {
                edit.saving = false;
                if result.is_ok() {
                    edit.base = text;
                }
            }
        }
        let next = self.files_panel.after_save.take();
        match result {
            Ok(()) => {
                self.toast(ToastKind::Success, format!("Saved {name}"));
                // The file may now differ from Git's copy.
                self.files_panel.mark_stale();
                // A step the unsaved prompt held back goes ahead, unless the
                // buffer moved on while the write ran.
                if let Some(next) = next
                    && !self.files_doc_dirty(cx)
                {
                    self.files_doc_next(next, cx);
                }
            }
            Err(error) => self.toast(ToastKind::Error, format!("Could not save {name}: {error}")),
        }
    }

    /// Ask on a worker, every [`WATCH`], whether the open file changed on
    /// disk since it was read or saved.
    fn files_watch_doc(&mut self) {
        let endpoint = self.active.clone();
        let every = super::activity::scaled(WATCH);
        let due = self.files_panel.doc.as_ref().is_some_and(|doc| {
            matches!(doc.state, DocState::Ready(_))
                && doc.stamp.is_some()
                && !doc.watch.in_flight
                && !doc.edit.as_ref().is_some_and(|edit| edit.saving)
                && doc.watch.asked.is_none_or(|at| at.elapsed() >= every)
        });
        if !due {
            return;
        }
        let Ok(target) = self.read_target(&endpoint) else {
            return;
        };
        let Some(doc) = self.files_panel.doc.as_mut() else {
            return;
        };
        doc.watch.asked = Some(Instant::now());
        doc.watch.in_flight = true;
        let (path, generation) = (doc.path.clone(), doc.watch.generation);
        let tx = self.files_panel.tx.clone();
        std::thread::spawn(move || {
            let result = file_stamp(target.as_ref(), &path);
            tx.send(FilesMsg::Stamp {
                endpoint,
                path,
                generation,
                result,
            })
            .ok();
        });
    }

    /// The open file changed on disk: read it again when nothing here is
    /// unsaved, and otherwise ask, so neither side's work is lost unseen.
    fn files_disk_changed(&mut self, stamp: String, cx: &mut Context<Self>) {
        if self.files_doc_dirty(cx) {
            if let Some(doc) = self.files_panel.doc.as_mut() {
                doc.changed_on_disk = Some(stamp);
            }
            cx.notify();
            return;
        }
        let name = self
            .files_panel
            .doc_path()
            .map(|path| super::diff::split_path(path).1.to_string())
            .unwrap_or_default();
        self.files_reload_doc(cx);
        self.toast(
            ToastKind::Info,
            format!("Reloaded {name}: it changed on disk"),
        );
    }

    /// Keep the edits over a file that changed on disk; the next save
    /// writes them over it.
    fn files_keep_mine(&mut self, cx: &mut Context<Self>) {
        if let Some(doc) = self.files_panel.doc.as_mut()
            && let Some(stamp) = doc.changed_on_disk.take()
        {
            doc.stamp = Some(stamp);
        }
        cx.notify();
    }

    /// Run a step the unsaved prompt held back.
    fn files_doc_next(&mut self, next: DocNext, cx: &mut Context<Self>) {
        match next {
            DocNext::Close => self.files_close_doc(cx),
            DocNext::Reload => self.files_reload_doc(cx),
            DocNext::Open(path) => self.files_open_doc(&path, cx),
            DocNext::LeaveSource => self.files_toggle_svg_source(cx),
        }
    }

    /// The unsaved prompt's Save: write the file, then take the step.
    pub(super) fn files_save_then(&mut self, next: DocNext, cx: &mut Context<Self>) {
        self.files_panel.after_save = Some(next);
        self.files_save_doc(cx);
        let saving = self
            .files_panel
            .doc
            .as_ref()
            .and_then(|doc| doc.edit.as_ref())
            .is_some_and(|edit| edit.saving);
        if !saving {
            self.files_panel.after_save = None;
        }
    }

    /// The unsaved prompt's Don't Save: drop the edits, then take the step.
    pub(super) fn files_discard_then(&mut self, next: DocNext, cx: &mut Context<Self>) {
        self.files_revert_doc(cx);
        self.files_doc_next(next, cx);
    }

    /// Put the tree back where the open document was.
    pub(super) fn files_close_doc(&mut self, cx: &mut Context<Self>) {
        if self.files_doc_unsaved(DocNext::Close, cx) {
            return;
        }
        self.right_navigate(cx, |this, _| this.files_panel.close_doc());
    }

    pub(super) fn files_reload_doc(&mut self, cx: &mut Context<Self>) {
        if self.files_doc_unsaved(DocNext::Reload, cx) {
            return;
        }
        let Some(doc) = self.files_panel.doc.as_ref() else {
            return;
        };
        let (path, as_text) = (doc.path.clone(), doc.as_text);
        let resume = doc.edit.as_ref().map(|edit| Resume {
            mode: edit.mode,
            caret: self.md_editor_input.read(cx).caret_byte(),
            scroll: edit.scroll.clone(),
        });
        self.files_read_doc_as(path, as_text);
        if let Some(doc) = self.files_panel.doc.as_mut() {
            doc.resume = resume;
        }
        cx.notify();
    }

    /// Switch an SVG between its picture and its source.
    fn files_toggle_svg_source(&mut self, cx: &mut Context<Self>) {
        if self.files_doc_unsaved(DocNext::LeaveSource, cx) {
            return;
        }
        if let Some(doc) = self.files_panel.doc.as_ref() {
            let (path, as_text) = (doc.path.clone(), !doc.as_text);
            self.files_read_doc_as(path, as_text);
            cx.notify();
        }
    }

    /// Ask for files with the system picker and upload them to `into`, or the
    /// tree's root.
    fn files_pick_upload(&mut self, into: Option<String>, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Upload".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = picked.await {
                this.update(cx, |this, cx| this.files_upload(paths, into, cx))
                    .ok();
            }
        })
        .detach();
    }

    /// Copy `paths` into `dir`, or the tree's root, off the GUI thread.
    fn files_upload(&mut self, paths: Vec<PathBuf>, dir: Option<String>, cx: &mut Context<Self>) {
        let Some(dir) = dir.or_else(|| self.files_panel.tree.as_ref().map(|t| t.root.clone()))
        else {
            self.toast(ToastKind::Error, "The file tree has not loaded yet");
            cx.notify();
            return;
        };
        let endpoint = self.active.clone();
        let target = match self.read_target(&endpoint) {
            Ok(target) => target,
            Err(error) => {
                self.toast(ToastKind::Error, error);
                cx.notify();
                return;
            }
        };
        // A folder over SSH takes a moment; say the drop was heard, and where
        // it landed, since a drop onto a row is easy to mis-aim.
        let into = base_name(&dir).to_string();
        self.toast(
            ToastKind::Info,
            match paths.as_slice() {
                [] => "Nothing to upload".to_string(),
                [one] => format!(
                    "Uploading {} to {into}…",
                    one.file_name()
                        .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
                ),
                many => format!("Uploading {} items to {into}…", many.len()),
            },
        );
        let tx = self.files_panel.tx.clone();
        std::thread::spawn(move || {
            let outcome = upload_all(target.as_ref(), &paths, &dir);
            tx.send(FilesMsg::Uploaded { endpoint, outcome }).ok();
        });
    }

    /// Open the tree's context menu on the entry at `path`, or on the tree's
    /// own folder when `root`.
    pub(super) fn files_open_menu(
        &mut self,
        path: String,
        dir: bool,
        root: bool,
        cx: &mut Context<Self>,
    ) {
        self.open_context_menu(
            MenuTarget::File {
                endpoint: self.active.key(),
                path,
                dir,
                root,
            },
            cx,
        );
    }

    /// Open the prompt a Files-menu row asks its question in.
    pub(super) fn files_menu_item(
        &mut self,
        target: &MenuTarget,
        item: Item,
        cx: &mut Context<Self>,
    ) {
        let MenuTarget::File {
            endpoint,
            path,
            dir,
            ..
        } = target
        else {
            return;
        };
        let endpoint = self.endpoint_by_key(endpoint);
        match item {
            Item::NewFile | Item::NewFolder => {
                let folder = item == Item::NewFolder;
                // A folder's new entry goes inside it; a file's goes beside it.
                let into = if *dir {
                    path.clone()
                } else {
                    parent_of(path).unwrap_or(path).to_string()
                };
                self.open_menu_prompt(
                    PromptAction::CreateEntry {
                        endpoint,
                        dir: into,
                        folder,
                    },
                    if folder { "New folder" } else { "New file" },
                    "Name",
                    "",
                    cx,
                );
            }
            Item::Rename => self.open_menu_prompt(
                PromptAction::RenameEntry {
                    endpoint,
                    path: path.clone(),
                },
                "Rename",
                "Name",
                base_name(path),
                cx,
            ),
            Item::Delete => self.open_menu_prompt(
                PromptAction::DeleteEntry {
                    endpoint,
                    path: path.clone(),
                    folder: *dir,
                },
                if *dir { "Delete folder" } else { "Delete file" },
                "",
                "",
                cx,
            ),
            Item::Upload => {
                // Like a drop: a folder takes the files, a file's neighbours do.
                let into = if *dir {
                    path.clone()
                } else {
                    parent_of(path).unwrap_or(path).to_string()
                };
                self.files_pick_upload(Some(into), cx);
            }
            Item::Download | Item::DownloadArchive => {
                self.files_pick_download(endpoint, path.clone(), *dir, cx);
            }
            _ => {}
        }
    }

    /// Ask where to save `path` and fetch it there: the file itself, or a
    /// `.tar.gz` for a folder. The copy runs off the GUI thread.
    fn files_pick_download(
        &mut self,
        endpoint: EndpointId,
        path: String,
        dir: bool,
        cx: &mut Context<Self>,
    ) {
        let target = match self.read_target(&endpoint) {
            Ok(target) => target,
            Err(error) => {
                self.toast(ToastKind::Error, error);
                cx.notify();
                return;
            }
        };
        let name = base_name(&path).to_string();
        let suggested = if dir { archive_name(&name) } else { name };
        let start = std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join("Downloads"))
            .filter(|downloads| downloads.is_dir())
            .unwrap_or_else(std::env::temp_dir);
        let chosen = cx.prompt_for_new_path(&start, Some(&suggested));
        let tx = self.files_panel.tx.clone();
        cx.spawn(async move |_, _| {
            let Ok(Ok(Some(destination))) = chosen.await else {
                return;
            };
            std::thread::spawn(move || {
                let result =
                    download_path(target.as_ref(), &path, dir, &destination).map(|()| destination);
                tx.send(FilesMsg::Downloaded { result }).ok();
            });
        })
        .detach();
    }

    pub(super) fn files_create(
        &mut self,
        endpoint: &EndpointId,
        dir: String,
        name: String,
        folder: bool,
        cx: &mut Context<Self>,
    ) {
        self.files_edit(endpoint, cx, move |target| {
            create_entry(target, &dir, &name, folder).map(|path| Edit::Created { path, dir })
        });
    }

    pub(super) fn files_rename(
        &mut self,
        endpoint: &EndpointId,
        from: String,
        name: String,
        cx: &mut Context<Self>,
    ) {
        self.files_edit(endpoint, cx, move |target| {
            let to = rename_entry(target, &from, &name)?;
            Ok(Edit::Renamed { from, to })
        });
    }

    pub(super) fn files_delete(
        &mut self,
        endpoint: &EndpointId,
        path: String,
        cx: &mut Context<Self>,
    ) {
        self.files_edit(endpoint, cx, move |target| {
            delete_entry(target, &path).map(|()| Edit::Deleted { path })
        });
    }

    /// Run `job` against the endpoint's machine off the GUI thread and report
    /// what it did.
    fn files_edit(
        &mut self,
        endpoint: &EndpointId,
        cx: &mut Context<Self>,
        job: impl FnOnce(Option<&SshTarget>) -> Result<Edit, String> + Send + 'static,
    ) {
        let target = match self.read_target(endpoint) {
            Ok(target) => target,
            Err(error) => {
                self.toast(ToastKind::Error, error);
                cx.notify();
                return;
            }
        };
        let tx = self.files_panel.tx.clone();
        let endpoint = endpoint.clone();
        std::thread::spawn(move || {
            let result = job(target.as_ref());
            tx.send(FilesMsg::Edited { endpoint, result }).ok();
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
                panel.folds_changed = true;
                panel.relayout();
                self.files_save_folds();
            }
            FolderClick::Unfold => {
                panel.folded.remove(&row.path);
                panel.folds_changed = true;
                panel.relayout();
                self.files_save_folds();
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
                panel.folds_changed |= panel.folded.remove(&row.path);
                self.files_save_folds();
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

    /// A click on a file row. A text file opens here, in the editor, even when
    /// Git reports it changed, since the Files tab is where it is edited; a
    /// markdown file opens as a document first, and an image or SVG as a
    /// picture. A double click, and any other file Git reports changed, goes to
    /// a pane and the Diff as before; the rest are a preview.
    fn files_click_file(&mut self, row: &FileRow, target: OpenTarget, cx: &mut Context<Self>) {
        if md_view::is_markdown(&row.path)
            || (target == OpenTarget::Preview
                && (image_view::kind(&row.path).is_some() || highlight::is_text(&row.path)))
        {
            self.files_open_doc(&row.path, cx);
            return;
        }
        if let Some(change) = self.files_panel.changes.file(&row.path).cloned() {
            self.right_navigate(cx, |this, cx| {
                this.right.select_tab(SidebarTab::Diff);
                this.diff_reveal(change.layer, &change.path, cx);
            });
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

    /// The tab: the tree, or the open document, with files and folders dropped
    /// on it uploaded to the root. Wide enough, the tree stays beside the
    /// document so another file is one click away.
    pub(super) fn files_page(&mut self, page_w: f32, cx: &mut Context<Self>) -> Div {
        let fits = self.files_panel.doc.is_some() && page_w >= TREE_BESIDE_MIN_PAGE;
        let beside = fits && !self.settings.files_tree_hidden;
        self.files_panel.tree_fits = fits;
        self.files_panel.tree_beside = beside;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let tree_w = if beside {
            TREE_BESIDE_WIDTH * fonts.body / 13.0
        } else {
            0.0
        };
        self.files_panel.split_ok = page_w - tree_w >= SPLIT_MIN_DOC * fonts.body / 13.0;
        if self.files_panel.doc.is_none() {
            return self.files_tree_zone(cx);
        }
        if !beside {
            return self.files_doc_zone(cx);
        }
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let tree_w = TREE_BESIDE_WIDTH * fonts.body / 13.0;
        div()
            .flex()
            .flex_row()
            .size_full()
            .min_h(px(0.0))
            .child(
                div()
                    .w(px(tree_w))
                    .flex_shrink_0()
                    .h_full()
                    .min_h(px(0.0))
                    .border_r_1()
                    .border_color(theme.border)
                    .child(self.files_tree_zone(cx)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .h_full()
                    .child(self.files_doc_zone(cx)),
            )
    }

    /// A drop on a preview still has a place to go: the root.
    fn files_doc_zone(&mut self, cx: &mut Context<Self>) -> Div {
        let hint = self.theme.panel_dim;
        let on_drop = cx.listener(|this, paths: &ExternalPaths, _, cx| {
            this.files_panel.drop_hint = None;
            this.files_upload(paths.paths().to_vec(), None, cx);
            cx.notify();
        });
        let on_move = cx.listener(|this, event: &DragMoveEvent<ExternalPaths>, _, cx| {
            if event.bounds.contains(&event.event.position) && this.files_panel.hint_drop(None) {
                cx.notify();
            }
        });
        let clear = self.theme.panel.opacity(0.0);
        div().size_full().min_h(px(0.0)).child(
            div()
                .id("pilus-files-doc-drop")
                .size_full()
                .min_h(px(0.0))
                // The tree's drop ring is a hairline it always reserves; the
                // same clear one here keeps the two headers' rules level.
                .border_1()
                .border_color(clear)
                .drag_over::<ExternalPaths>(move |style, _, _, _| style.bg(hint))
                .on_drag_move(on_move)
                .on_drop(on_drop)
                .child(self.files_doc_page(cx)),
        )
    }

    /// The tree with its header, taking files dropped on it.
    fn files_tree_zone(&mut self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let on_drop = cx.listener(|this, paths: &ExternalPaths, _, cx| {
            this.files_panel.drop_hint = None;
            this.files_upload(paths.paths().to_vec(), None, cx);
            cx.notify();
        });
        // Rows report after the panel (they are in front of it), so a row's
        // folder replaces the root within the same move.
        let on_move = cx.listener(|this, event: &DragMoveEvent<ExternalPaths>, _, cx| {
            if event.bounds.contains(&event.event.position) && this.files_panel.hint_drop(None) {
                cx.notify();
            }
        });
        let edge = theme.border_focus;
        let tree = self.files_tree_page(cx);
        div().size_full().min_h(px(0.0)).child(
            div()
                .id("pilus-files-drop")
                .size_full()
                .min_h(px(0.0))
                .border_1()
                .border_color(theme.panel.opacity(0.0))
                .drag_over::<ExternalPaths>(move |style, _, _, _| style.border_color(edge))
                .on_drag_move(on_move)
                .on_drop(on_drop)
                // Rows stop the press, so this is the blank area and the header.
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, _: &MouseDownEvent, _, cx| {
                        if let Some(root) = this.files_panel.tree.as_ref().map(|t| t.root.clone()) {
                            this.files_open_menu(root, true, true, cx);
                        }
                    }),
                )
                .child(tree),
        )
    }

    /// While files are dragged over the tab, the line saying where they would
    /// land: the folder under the pointer, or the tree's root.
    fn files_drop_label(&self, cx: &Context<Self>) -> Option<String> {
        let into = self.files_panel.dropping_into(cx.has_active_drag())?;
        let name = match into {
            Some(dir) => base_name(dir),
            None => self
                .files_panel
                .tree
                .as_ref()
                .map_or("the root", |tree| base_name(&tree.root)),
        };
        Some(format!("Drop to upload into {name}"))
    }

    /// The host and root above the file tree.
    fn files_tree_page(&mut self, cx: &mut Context<Self>) -> Div {
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

    /// The open document: its name over the rendered text.
    fn files_doc_page(&mut self, cx: &mut Context<Self>) -> Div {
        self.find_refresh(cx);
        self.files_mark_preview();
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let Some(doc) = self.files_panel.doc.as_ref() else {
            return div();
        };
        let (dir, name) = super::diff::split_path(&doc.path);
        let dir = self.shorten_cwd(&self.active.key(), dir);
        let editable = matches!(&doc.state, DocState::Ready(md) if md.editable());
        // Only markdown has a rendered form to switch to.
        let previewable = md_view::is_markdown(&doc.path);
        let mode = doc.edit.as_ref().map(|edit| edit.mode);
        let editing = mode.is_some_and(EditMode::edits);
        // Too narrow a column shows the source alone until it is widened.
        let split = mode == Some(EditMode::Split) && self.files_panel.split_ok;
        let dirty = self.files_doc_dirty(cx);
        // Lines added and removed since the index, as the marks show them: the
        // editor's while the file is edited, so the counts follow each
        // keystroke, and the preview's before that.
        let counts = match &doc.state {
            _ if doc.edit.is_some() => self.md_editor_input.update(cx, |input, _| {
                input
                    .line_changes()
                    .map(|changes| (changes.insertions, changes.deletions))
            }),
            // A preview never edited has only the document's own marks.
            DocState::Ready(md) => md.change_counts(),
            _ => None,
        };
        let counts =
            counts
                .filter(|&(added, removed)| added + removed > 0)
                .map(|(added, removed)| {
                    let count = |text: String, color| {
                        div()
                            .flex_shrink_0()
                            .text_size(px(fonts.label))
                            .text_color(color)
                            .child(text)
                    };
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .flex_shrink_0()
                        .children((added > 0).then(|| count(format!("+{added}"), theme.ok)))
                        .children((removed > 0).then(|| count(format!("−{removed}"), theme.alert)))
                });
        let Some(doc) = self.files_panel.doc.as_ref() else {
            return div();
        };
        let mut edit_buttons: Vec<AnyElement> = Vec::new();
        // An SVG is a picture that is also text: its own pencil swaps one for
        // the other.
        let svg = image_view::kind(&doc.path) == Some(image_view::Kind::Svg);
        if svg && (doc.as_text || matches!(doc.state, DocState::Picture(_))) {
            let source = doc.as_text;
            edit_buttons.push(
                self.diff_icon_button(
                    "pilus-files-doc-svg-source",
                    include_bytes!("../../assets/icons/pencil.svg"),
                    if source {
                        "Show the image"
                    } else {
                        "Show the source"
                    }
                    .to_string(),
                    source,
                    false,
                )
                .on_click(cx.listener(|this, _, _, cx| this.files_toggle_svg_source(cx)))
                .into_any_element(),
            );
        }
        // The pencil goes in and out of the editor. Out of it, markdown is
        // rendered and any other file's source is read-only. An SVG's source
        // being edited already has its own pencil lit.
        let svg_editing = svg && doc.as_text && editing;
        if editable && (previewable || mode.is_some()) && !svg_editing {
            edit_buttons.push(
                self.diff_icon_button(
                    "pilus-files-doc-edit",
                    include_bytes!("../../assets/icons/pencil.svg"),
                    match (editing, previewable) {
                        (true, true) => "Preview",
                        (true, false) => "Read-only",
                        (false, _) => "Edit",
                    }
                    .to_string(),
                    editing,
                    false,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    if editing {
                        this.files_preview_doc(cx);
                    } else {
                        this.files_edit_doc(window, cx);
                    }
                }))
                .into_any_element(),
            );
        }
        if editable && previewable {
            edit_buttons.push(
                self.diff_icon_button(
                    "pilus-files-doc-split",
                    include_bytes!("../../assets/icons/columns-2.svg"),
                    if mode == Some(EditMode::Split) {
                        "Source only"
                    } else {
                        "Side by side"
                    }
                    .to_string(),
                    mode == Some(EditMode::Split),
                    false,
                )
                .on_click(cx.listener(|this, _, window, cx| this.files_split_doc(window, cx)))
                .into_any_element(),
            );
        }
        if dirty {
            edit_buttons.push(
                self.diff_icon_button(
                    "pilus-files-doc-revert",
                    include_bytes!("../../assets/icons/undo-2.svg"),
                    "Revert changes".to_string(),
                    false,
                    false,
                )
                .on_click(cx.listener(|this, _, _, cx| this.files_revert_doc(cx)))
                .into_any_element(),
            );
            edit_buttons.push(
                self.diff_icon_button(
                    "pilus-files-doc-save",
                    include_bytes!("../../assets/icons/check.svg"),
                    "Save (⌘S)".to_string(),
                    true,
                    false,
                )
                .on_click(cx.listener(|this, _, _, cx| this.files_save_doc(cx)))
                .into_any_element(),
            );
        }
        // With room for the tree beside the file, the header can put it away
        // and bring it back.
        let tree_toggle = self.files_panel.tree_fits.then(|| {
            let shown = !self.settings.files_tree_hidden;
            self.diff_icon_button(
                "pilus-files-doc-tree",
                include_bytes!("../../assets/icons/panel-left.svg"),
                if shown {
                    "Hide the file tree"
                } else {
                    "Show the file tree"
                }
                .to_string(),
                shown,
                false,
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.files_set_tree_hidden(!this.settings.files_tree_hidden, true);
                cx.notify();
            }))
        });
        // Close steps out one level: an edited file goes read-only first.
        let close_tip = if editing {
            "Stop editing (⌘W)"
        } else if self.files_panel.tree_fits {
            "Close (⌘W)"
        } else {
            "Back to the file tree (⌘W)"
        };
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .flex_shrink_0()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .children(tree_toggle)
            .child(icon(
                include_bytes!("../../assets/icons/file.svg"),
                fonts.body,
                theme.muted,
            ))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .font_family(SharedString::from(style::mono_family(
                        &self.shared_settings,
                    )))
                    .text_size(px(fonts.secondary))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .truncate()
                            .text_color(theme.muted)
                            .child(format!("{dir}/")),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(theme.panel_fg)
                            .child(name.to_string()),
                    )
                    .children(dirty.then(|| {
                        div()
                            .flex_shrink_0()
                            .pl_1()
                            .text_color(theme.muted)
                            .child("•")
                    })),
            )
            .children(counts)
            .children(edit_buttons)
            .child(
                self.diff_icon_button(
                    "pilus-files-doc-reload",
                    include_bytes!("../../assets/icons/refresh-cw.svg"),
                    "Reload (⌘R)".to_string(),
                    false,
                    false,
                )
                .on_click(cx.listener(|this, _, _, cx| this.files_reload_doc(cx))),
            )
            .child(
                self.diff_icon_button(
                    "pilus-files-doc-close",
                    include_bytes!("../../assets/icons/x.svg"),
                    close_tip.to_string(),
                    false,
                    false,
                )
                .on_click(cx.listener(|this, _, _, cx| this.files_step_out(cx))),
            );
        let truncated = self
            .files_panel
            .doc_view()
            .is_some_and(|doc| doc.truncated)
            .then(|| {
                div()
                    .px_3()
                    .py_1()
                    .flex_shrink_0()
                    .text_size(px(fonts.label))
                    .text_color(theme.alert)
                    .child("TRUNCATED: only the start of this file is shown")
            });
        let body = if split && editable {
            self.files_split_body(cx)
        } else if (editing || self.files_panel.source_read_only()) && editable {
            self.files_editor_body(cx)
        } else {
            match &self.files_panel.doc.as_ref().map(|doc| &doc.state) {
                Some(DocState::Loading) | None => self.diff_message("Loading…").into_any_element(),
                Some(DocState::Picture(picture)) => image_view::view(picture, &theme, &fonts),
                Some(DocState::Failed(error)) => self
                    .diff_message(format!("Could not read this file: {error}"))
                    .into_any_element(),
                // Nothing to render: the editor is about to take it, or the
                // file is too long for it.
                Some(DocState::Ready(md)) if !previewable => {
                    let text = if md.editable() {
                        "Loading…"
                    } else {
                        "This file is too large to edit here"
                    };
                    self.diff_message(text).into_any_element()
                }
                Some(DocState::Ready(_)) => self.md_list(Page::Files, cx),
            }
        };
        let find_bar = self.find_bar(cx);
        let disk_bar = self
            .files_panel
            .doc
            .as_ref()
            .is_some_and(|doc| doc.changed_on_disk.is_some())
            .then(|| self.files_disk_bar(cx));
        // The page takes the file's shortcuts (`FilesDoc`) while it, or the
        // editor inside it, has the keyboard.
        let focus = self
            .files_panel
            .doc_focus
            .get_or_insert_with(|| cx.focus_handle())
            .clone();
        div()
            .key_context("FilesDoc")
            .track_focus(&focus)
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .min_h(px(0.0))
            .child(header)
            .children(truncated)
            .children(disk_bar)
            .children(find_bar)
            .child(div().flex_1().min_h(px(0.0)).child(body))
    }

    /// The line under the header when the file changed on disk under unsaved
    /// changes: take the disk's copy, or keep these edits.
    fn files_disk_bar(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .flex_shrink_0()
            .px_3()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.alert)
                    .child("Changed on disk since you opened it"),
            )
            .child(
                style::small_button(
                    "pilus-files-disk-reload",
                    "Reload",
                    style::ButtonKind::Danger,
                    theme,
                    fonts.secondary,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.files_discard_then(DocNext::Reload, cx);
                })),
            )
            .child(
                style::small_button(
                    "pilus-files-disk-keep",
                    "Keep Mine",
                    style::ButtonKind::Secondary,
                    theme,
                    fonts.secondary,
                )
                .on_click(cx.listener(|this, _, _, cx| this.files_keep_mine(cx))),
            )
    }

    /// The editor beside the rendered document, which follows its text.
    fn files_split_body(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        self.files_sync_preview(cx);
        let editor = self.files_editor_body(cx);
        let preview = self.md_list(Page::Files, cx);
        div()
            .flex()
            .flex_row()
            .size_full()
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .h_full()
                    .border_r_1()
                    .border_color(theme.border)
                    .child(editor),
            )
            .child(div().flex_1().min_w(px(0.0)).h_full().child(preview))
            .into_any_element()
    }

    /// The open document's source in the editor field, scrolled to follow the
    /// caret. The field is as tall as its text, so the scrolling is the
    /// wrapper's. Out of the editor, the same field shows the source
    /// read-only, and the line chords (`MdEditor`) stand down.
    fn files_editor_body(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let mono = gpui::font(style::mono_family(&self.shared_settings));
        let read_only = !self.files_panel.edit_mode().is_some_and(EditMode::edits);
        self.md_editor_input.update(cx, |input, cx| {
            input.set_style(theme, mono, px(fonts.body));
            input.set_line_numbers(true, cx);
            input.set_current_line(true, cx);
            input.set_code_editor(true);
            input.set_read_only(read_only, cx);
        });
        let (caret, row) = {
            let input = self.md_editor_input.read(cx);
            (input.caret_byte(), input.caret_row())
        };
        let Some(edit) = self
            .files_panel
            .doc
            .as_mut()
            .and_then(|doc| doc.edit.as_mut())
        else {
            return div().into_any_element();
        };
        let scroll = edit.scroll.clone();
        follow_caret(&scroll, row, caret, &mut edit.caret_seen);
        // Page Up and Page Down move a view's height, as last laid out.
        let page = scroll.bounds().size.height;
        self.md_editor_input
            .update(cx, |input, _| input.set_page_height(page));
        div()
            .id("pilus-files-editor")
            .when(!read_only, |editor| editor.key_context("MdEditor"))
            .on_action(cx.listener(|this, _: &editor_ops::Indent, _, cx| {
                this.files_editor_op(Op::Indent, cx);
            }))
            .on_action(cx.listener(|this, _: &editor_ops::Outdent, _, cx| {
                this.files_editor_op(Op::Outdent, cx);
            }))
            .on_action(cx.listener(|this, _: &editor_ops::ToggleComment, _, cx| {
                this.files_editor_op(Op::ToggleComment, cx);
            }))
            .on_action(cx.listener(|this, _: &editor_ops::SelectLine, _, cx| {
                this.files_editor_op(Op::SelectLine, cx);
            }))
            .on_action(cx.listener(|this, _: &editor_ops::DeleteLine, _, cx| {
                this.files_editor_op(Op::DeleteLine, cx);
            }))
            .on_action(cx.listener(|this, _: &editor_ops::MoveLineUp, _, cx| {
                this.files_editor_op(Op::MoveUp, cx);
            }))
            .on_action(cx.listener(|this, _: &editor_ops::MoveLineDown, _, cx| {
                this.files_editor_op(Op::MoveDown, cx);
            }))
            .on_action(cx.listener(|this, _: &editor_ops::CopyLineUp, _, cx| {
                this.files_editor_op(Op::CopyUp, cx);
            }))
            .on_action(cx.listener(|this, _: &editor_ops::CopyLineDown, _, cx| {
                this.files_editor_op(Op::CopyDown, cx);
            }))
            .on_action(cx.listener(|this, _: &editor_ops::NewLineBelow, _, cx| {
                this.files_editor_op(Op::NewLineBelow, cx);
            }))
            .on_action(cx.listener(|this, _: &editor_ops::NewLineAbove, _, cx| {
                this.files_editor_op(Op::NewLineAbove, cx);
            }))
            .size_full()
            .min_w(px(0.0))
            .overflow_y_scroll()
            .track_scroll(&scroll)
            .child(
                div()
                    .w_full()
                    .px_3()
                    .py(px(EDITOR_PAD))
                    .child(self.md_editor_input.clone()),
            )
            .into_any_element()
    }

    /// Run a line chord on the editor's field.
    fn files_editor_op(&mut self, op: Op, cx: &mut Context<Self>) {
        self.md_editor_input
            .update(cx, |input, cx| input.run_op(op, cx));
        cx.notify();
    }

    /// Two rows: the machine with the tab's controls, then the root.
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
        let dropping = self.files_drop_label(cx);
        let refresh = self
            .diff_icon_button(
                "pilus-files-refresh",
                include_bytes!("../../assets/icons/refresh-cw.svg"),
                "Refresh".to_string(),
                false,
                false,
            )
            .on_click(cx.listener(|this, _, _, cx| this.files_refresh_now(cx)));
        let new = self
            .diff_icon_button(
                "pilus-files-new",
                include_bytes!("../../assets/icons/plus.svg"),
                "New file or folder".to_string(),
                false,
                false,
            )
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                if let Some(root) = this.files_panel.tree.as_ref().map(|t| t.root.clone()) {
                    this.files_open_menu(root, true, true, cx);
                }
            }));
        let upload = self
            .diff_icon_button(
                "pilus-files-upload",
                include_bytes!("../../assets/icons/upload.svg"),
                "Upload files".to_string(),
                false,
                false,
            )
            .on_click(cx.listener(|this, _, _, cx| this.files_pick_upload(None, cx)));
        let host = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(icon(
                include_bytes!("../../assets/icons/server.svg"),
                fonts.body,
                theme.muted,
            ))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(fonts.secondary))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(theme.panel_fg)
                    .child(self.endpoint_label(&self.active)),
            )
            .child(new)
            .child(upload)
            .child(refresh);
        let directory = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(icon(
                if dropping.is_some() {
                    include_bytes!("../../assets/icons/upload.svg")
                } else {
                    include_bytes!("../../assets/icons/folder.svg")
                },
                fonts.body,
                if dropping.is_some() {
                    theme.panel_fg
                } else {
                    theme.muted
                },
            ))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(fonts.secondary))
                    .text_color(if dropping.is_some() {
                        theme.panel_fg
                    } else {
                        theme.muted
                    })
                    .child(dropping.unwrap_or(root)),
            );
        // The hairline runs under the machine's row, not the root's, so beside
        // an open document it meets the document header's own: both rows are
        // the same height.
        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .child(host.px_3().py_2().border_b_1().border_color(theme.border))
            .child(directory.px_3().pt_2())
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
            selected: !row.dir && self.files_panel.doc_path() == Some(row.path.as_str()),
        };
        let row = row.clone();
        let path = row.path.clone();
        let menu_entry = (row.path.clone(), row.dir);
        let tree = style::tree_row(
            SharedString::from(format!("pilus-files-row-{}", row.path)),
            entry,
            self.theme,
            &self.shared_settings,
        )
        .on_drag_move(cx.listener({
            let (dir, path) = (row.dir, row.path.clone());
            move |this, event: &DragMoveEvent<ExternalPaths>, _, cx| {
                if event.bounds.contains(&event.event.position)
                    && this.files_panel.hint_drop(dir.then(|| path.clone()))
                {
                    cx.notify();
                }
            }
        }))
        .on_drop(cx.listener({
            let (dir, path) = (row.dir, row.path.clone());
            move |this, paths: &ExternalPaths, _, cx| {
                // A file row has no inside: the drop falls through to the tab,
                // which uploads to the root.
                let into = dir.then(|| path.clone());
                this.files_panel.drop_hint = None;
                this.files_upload(paths.paths().to_vec(), into, cx);
                cx.notify();
            }
        }))
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                cx.stop_propagation();
                let (path, dir) = menu_entry.clone();
                this.files_open_menu(path, dir, false, cx);
            }),
        )
        .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
            // A right click opens the menu, not the entry.
            if !event.standard_click() {
                return;
            }
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
        }));
        // The folder the drag would drop into wears a ring. It is a layer over
        // the row, so the row's own geometry does not move while it shows.
        let targeted = self
            .files_panel
            .dropping_into(cx.has_active_drag())
            .flatten()
            .is_some_and(|dir| dir == path);
        let tree = tree.relative().children(targeted.then(|| {
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left_0()
                .right_0()
                .radius_md()
                .border_1()
                .border_color(self.theme.border_focus)
        }));
        // The list lays an item out at its content width; the column gives
        // the row's margin-based indent a width to stretch into.
        div()
            .w_full()
            .flex()
            .flex_col()
            .child(tree)
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

/// Upload each of `paths` into `dir`, collecting what went through and what
/// did not rather than stopping at the first failure.
fn upload_all(target: Option<&SshTarget>, paths: &[PathBuf], dir: &str) -> Upload {
    let mut outcome = Upload {
        into: base_name(dir).to_string(),
        ..Upload::default()
    };
    for path in paths {
        match upload_path(target, path, dir) {
            Ok(name) => outcome.stored.push(name),
            Err(error) => outcome.failed.push(error),
        }
    }
    outcome
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
    /// Open `text` as the document at `path`, as a finished read would.
    pub(super) fn files_show_doc(&mut self, path: &str, text: &str, cx: &mut Context<Self>) {
        self.files_show_sample(cx);
        let panel = &mut self.files_panel;
        panel.doc_reads += 1;
        panel.doc = Some(FileDoc {
            path: path.to_string(),
            as_text: false,
            seq: panel.doc_reads,
            state: DocState::Ready(MdDoc::new(
                &TextFile {
                    text: text.to_string(),
                    truncated: false,
                },
                path,
            )),
            edit: None,
            git_base: None,
            base_asked: None,
            resume: None,
            stamp: None,
            watch: Watch::default(),
            changed_on_disk: None,
        });
        cx.notify();
    }

    /// Open `bytes` as the picture at `path`, as a finished read would.
    pub(super) fn files_show_picture(
        &mut self,
        path: &str,
        bytes: Vec<u8>,
        cx: &mut Context<Self>,
    ) {
        self.files_show_sample(cx);
        let state = match image_view::kind(path).map(|kind| image_view::decode(kind, bytes)) {
            Some(Ok(picture)) => DocState::Picture(picture),
            Some(Err(error)) => DocState::Failed(error),
            None => DocState::Failed("not a picture".to_string()),
        };
        let panel = &mut self.files_panel;
        panel.doc_reads += 1;
        panel.doc = Some(FileDoc {
            path: path.to_string(),
            as_text: false,
            seq: panel.doc_reads,
            state,
            edit: None,
            git_base: None,
            base_asked: None,
            resume: None,
            stamp: None,
            watch: Watch::default(),
            changed_on_disk: None,
        });
        cx.notify();
    }

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

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn a_rename_carries_the_paths_below_it() {
        assert_eq!(
            moved("/w/docs/a.md", "/w/docs", "/w/guide"),
            Some("/w/guide/a.md".to_string())
        );
        assert_eq!(
            moved("/w/a.md", "/w/a.md", "/w/b.md"),
            Some("/w/b.md".to_string())
        );
        // A sibling that shares the prefix is not inside the folder.
        assert_eq!(moved("/w/docs2/a.md", "/w/docs", "/w/guide"), None);
        assert!(within("/w/docs/a.md", "/w/docs"));
        assert!(!within("/w/docs2", "/w/docs"));
    }

    #[test]
    fn an_edit_says_what_it_did_by_name() {
        let created = Edit::Created {
            path: "/w/src/new.rs".into(),
            dir: "/w/src".into(),
        };
        assert_eq!(created.toast(), "Created new.rs");
        let renamed = Edit::Renamed {
            from: "/w/a".into(),
            to: "/w/b".into(),
        };
        assert_eq!(renamed.toast(), "Renamed to b");
        assert_eq!(
            Edit::Deleted {
                path: "/w/b/".into()
            }
            .toast(),
            "Deleted b"
        );
    }

    #[test]
    fn an_upload_reports_what_went_through_and_what_did_not() {
        let one = Upload {
            stored: names(&["a.txt"]),
            failed: Vec::new(),
            into: "src".to_string(),
        };
        assert_eq!(
            one.toasts(),
            vec![(ToastKind::Success, "Uploaded a.txt to src".to_string())]
        );
        let mixed = Upload {
            stored: names(&["a.txt", "b.txt"]),
            failed: names(&["c.txt already exists", "d.txt: denied"]),
            into: "src".to_string(),
        };
        assert_eq!(
            mixed.toasts(),
            vec![
                (ToastKind::Success, "Uploaded 2 items to src".to_string()),
                (
                    ToastKind::Error,
                    "c.txt already exists\nd.txt: denied".to_string()
                ),
            ]
        );
        assert!(Upload::default().toasts().is_empty());
    }

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
    fn a_refresh_that_reads_back_an_empty_tree_keeps_the_folds() {
        let mut panel = FilesPanel::new();
        let rows = || {
            vec![
                row("/w/a", 0, true, true),
                row("/w/a/b", 1, true, true),
                row("/w/a/b/x", 2, false, false),
                row("/w/c", 0, true, true),
                row("/w/c/y", 1, false, false),
            ]
        };
        panel.apply(1, Ok(tree("/w", rows())));
        panel.folded = folded(&["/w/a/b", "/w/c"]);
        // `files.refresh` forgot every listing: nothing is read yet.
        panel.apply(2, Ok(tree("/w", Vec::new())));
        // Then the root is back but `a` is still being read.
        let half = vec![row("/w/a", 0, true, true), row("/w/c", 0, true, true)];
        panel.apply(3, Ok(tree("/w", half)));
        assert_eq!(panel.folded, folded(&["/w/a/b", "/w/c"]));
        assert!(!panel.folds_changed);
        panel.apply(4, Ok(tree("/w", rows())));
        assert_eq!(panel.visible, [0, 1, 3]);
    }

    #[test]
    fn a_fold_goes_with_its_deleted_folder() {
        let mut panel = FilesPanel::new();
        let rows = vec![
            row("/w/a", 0, true, true),
            row("/w/a/b", 1, true, true),
            row("/w/a/b/x", 2, false, false),
            row("/w/c", 0, true, true),
            row("/w/c/y", 1, false, false),
        ];
        panel.apply(1, Ok(tree("/w", rows)));
        panel.folded = folded(&["/w/a/b", "/w/c"]);
        let after = vec![row("/w/a", 0, true, true), row("/w/a/z", 1, false, false)];
        assert!(panel.apply(2, Ok(tree("/w", after))));
        assert!(panel.folded.is_empty());
        assert!(panel.folds_changed);
    }

    #[test]
    fn saved_folds_come_back_in_a_new_window() {
        let mut panel = FilesPanel::new();
        panel.endpoint = Some(EndpointId::Machine("box".to_string()));
        let rows = || vec![row("/w/a", 0, true, true), row("/w/a/x", 1, false, false)];
        panel.apply(1, Ok(tree("/w", rows())));
        panel.folded.insert("/w/a".to_string());
        panel.apply(2, Ok(tree("/v", rows())));
        panel.folded.insert("/w/a".to_string());
        let saved = panel.saved_folds();
        assert_eq!(
            saved,
            FilesFolded::from([(
                "box".to_string(),
                [
                    ("/v".to_string(), vec!["/w/a".to_string()]),
                    ("/w".to_string(), vec!["/w/a".to_string()]),
                ]
                .into_iter()
                .collect(),
            )])
        );

        let mut next = FilesPanel::new();
        next.restore_folds(&saved);
        next.endpoint = Some(EndpointId::Machine("box".to_string()));
        next.apply(1, Ok(tree("/w", rows())));
        assert_eq!(next.visible, [0]);
        // Another machine with the same folder has folds of its own.
        let mut other = FilesPanel::new();
        other.restore_folds(&saved);
        other.endpoint = Some(EndpointId::Local);
        other.apply(1, Ok(tree("/w", rows())));
        assert_eq!(other.visible, [0, 1]);
    }

    #[test]
    fn another_root_shows_its_own_folds_and_gives_them_back() {
        let mut panel = FilesPanel::new();
        let rows = || vec![row("/w/a", 0, true, true), row("/w/a/x", 1, false, false)];
        panel.apply(1, Ok(tree("/w", rows())));
        panel.folded.insert("/w/a".to_string());
        panel.apply(2, Ok(tree("/v", rows())));
        assert!(panel.folded.is_empty());
        panel.apply(3, Ok(tree("/w", rows())));
        assert_eq!(panel.folded, folded(&["/w/a"]));
        assert_eq!(panel.visible, [0]);
    }

    #[test]
    fn another_machine_gives_the_folds_back_after_a_reset() {
        let mut panel = FilesPanel::new();
        let rows = vec![row("/w/a", 0, true, true), row("/w/a/x", 1, false, false)];
        panel.apply(1, Ok(tree("/w", rows.clone())));
        panel.folded.insert("/w/a".to_string());
        panel.reset();
        assert!(panel.folded.is_empty());
        panel.apply(2, Ok(tree("/w", rows)));
        assert_eq!(panel.folded, folded(&["/w/a"]));
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
